//! `--listen-unix` and `--trust-proxy`: a reverse proxy on this machine reaches
//! the server over a Unix socket, and its own access control decides who sees
//! a profile, so the server asks for no token.

use std::io::{BufRead as _, BufReader, Read as _, Write as _};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdout, Command, Output, Stdio};
use std::time::Duration;

use futures_util::{SinkExt as _, StreamExt as _};
use serde_json::{Value, json};
use tokio::net::UnixStream as AsyncUnixStream;
use tokio::time::timeout;
use tokio_tungstenite::client_async;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest as _;

use crate::profiles::record_calls;
use crate::support::{ScratchDir, flight_recordings};

const DEADLINE: Duration = Duration::from_secs(10);

/// The public URL the proxy serves the server at.
const PUBLIC_URL: &str = "http://viewer.test/r/abc/";

/// A running `microscope web --listen-unix`, killed when dropped.
struct Proxied {
    child: Child,
    stdout: BufReader<ChildStdout>,
    recording: Option<PathBuf>,
    socket: PathBuf,
}

impl Proxied {
    /// Starts the server on a socket in `scratch`, serving `profile`, and
    /// returns once its `ready` line is read.
    fn start(scratch: &ScratchDir, profile: &Path) -> (Self, String) {
        let socket = scratch.path().join("s.sock");
        let mut command = Command::new(env!("CARGO_BIN_EXE_microscope"));
        command
            .args(["web", "--listen-unix"])
            .arg(&socket)
            .args(["--public-url", PUBLIC_URL, "--trust-proxy"])
            .arg(profile)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());

        let recording = flight_recordings::watch_adapter(&mut command);
        let mut child = command.spawn().expect("start microscope web");
        let stdout = BufReader::new(child.stdout.take().expect("stdout"));

        // Owning the child from here on means a failed read still kills it.
        let mut proxied = Self {
            child,
            stdout,
            recording,
            socket,
        };

        let mut line = String::new();
        proxied
            .stdout
            .read_line(&mut line)
            .expect("read the ready line");
        let path = line
            .trim_end()
            .strip_prefix("ready ")
            .unwrap_or_else(|| panic!("the first stdout line is {line:?}, not ready"))
            .to_owned();

        (proxied, path)
    }

    /// Sends raw HTTP over the socket and returns the whole response.
    fn http(&self, request: &str) -> String {
        let mut stream = UnixStream::connect(&self.socket).expect("connect to the socket");
        stream
            .set_read_timeout(Some(DEADLINE))
            .expect("read timeout");
        stream.write_all(request.as_bytes()).expect("send");

        let mut response = Vec::new();
        let _ = stream.read_to_end(&mut response);
        String::from_utf8_lossy(&response).into_owned()
    }

    /// Opens the WebSocket at `path` over the socket with these headers,
    /// or returns the HTTP status that refused it.
    async fn connect(&self, path: &str, origin: &str, host: &str) -> Result<WebSocket, u16> {
        let mut request = format!("ws://{host}{path}")
            .into_client_request()
            .expect("a request");
        let headers = request.headers_mut();
        headers.insert("Host", host.parse().expect("a header"));
        headers.insert("Origin", origin.parse().expect("a header"));

        let stream = AsyncUnixStream::connect(&self.socket)
            .await
            .expect("connect to the socket");
        match timeout(DEADLINE, client_async(request, stream)).await {
            Ok(Ok((socket, _))) => Ok(socket),
            Ok(Err(tokio_tungstenite::tungstenite::Error::Http(response))) => {
                Err(response.status().as_u16())
            }
            Ok(Err(error)) => panic!("connect: {error}"),
            Err(elapsed) => panic!("connecting timed out: {elapsed}"),
        }
    }
}

impl Drop for Proxied {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        if let Some(recording) = &self.recording {
            flight_recordings::adapter_finished(recording);
        }
    }
}

type WebSocket = tokio_tungstenite::WebSocketStream<AsyncUnixStream>;

/// The next text message, skipping binary frames, failing at the deadline.
async fn next_text(socket: &mut WebSocket) -> Value {
    loop {
        let message = timeout(DEADLINE, socket.next())
            .await
            .expect("a message in time")
            .expect("a connection")
            .expect("a message");
        if let Message::Text(text) = message {
            return serde_json::from_str(&text).expect("JSON");
        }
    }
}

/// Runs `microscope web` with `arguments` to its end, without a socket.
fn refused(arguments: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_microscope"))
        .arg("web")
        .args(arguments)
        .stdin(Stdio::null())
        .output()
        .expect("run microscope web")
}

#[tokio::test]
async fn the_socket_serves_its_profile_to_a_proxy_with_no_token() {
    let scratch = ScratchDir::new("web-proxy");
    let profile = record_calls(&scratch);
    let (server, ready) = Proxied::start(&scratch, &profile);

    // The page's path is under the public URL's path, and names the profile.
    let id = ready
        .strip_prefix("/r/abc/profile/")
        .unwrap_or_else(|| panic!("{ready}"));
    assert_eq!(id.len(), 16, "{ready}");
    assert!(id.bytes().all(|byte| byte.is_ascii_hexdigit()), "{ready}");

    // A proxied request needs no cookie, and gets the check's 204.
    let check = |host: &str, origin: Option<&str>| {
        let origin = origin.map_or_else(String::new, |origin| format!("Origin: {origin}\r\n"));
        server.http(&format!(
            "POST /r/abc/api/check HTTP/1.1\r\nHost: {host}\r\n{origin}Content-Length: 0\r\nConnection: close\r\n\r\n"
        ))
    };

    let response = check("viewer.test", Some("http://viewer.test"));
    assert!(response.starts_with("HTTP/1.1 204"), "{response}");

    // Another name, another origin, or no browser at all is refused.
    for (host, origin) in [
        ("evil.test", Some("http://viewer.test")),
        ("viewer.test", Some("http://evil.test")),
        ("viewer.test", None),
    ] {
        let response = check(host, origin);
        assert!(
            response.starts_with("HTTP/1.1 403"),
            "{host} {origin:?}: {response}"
        );
    }

    // The WebSocket upgrades as a viewer and answers profile queries.
    let mut socket = server
        .connect("/r/abc/api/ws", "http://viewer.test", "viewer.test")
        .await
        .expect("the socket upgrades");
    let hello = next_text(&mut socket).await;
    assert_eq!(hello["type"], "hello", "{hello}");
    assert_eq!(hello["role"], "view", "{hello}");
    // The proxy shares the page with links of its own.
    assert_eq!(hello["share"], false, "{hello}");

    let request = json!({"id": 1, "method": "profile", "params": {"profile": id}});
    socket
        .send(Message::text(request.to_string()))
        .await
        .expect("send");
    let answer = loop {
        let message = next_text(&mut socket).await;
        if message["id"] == 1 {
            break message;
        }
    };
    assert_eq!(answer["type"], "result", "{answer}");
    assert_eq!(answer["result"]["samples"], 6, "{answer}");

    // A viewer may not control the program, so the profile is all it gets.
    socket
        .send(Message::text(
            json!({"id": 2, "method": "pause"}).to_string(),
        ))
        .await
        .expect("send");
    let refusal = loop {
        let message = next_text(&mut socket).await;
        if message["id"] == 2 {
            break message;
        }
    };
    assert_eq!(refusal["error"]["kind"], "forbidden", "{refusal}");

    // Nor may it make links of the server's own.
    socket
        .send(Message::text(
            json!({"id": 3, "method": "share", "params": {"role": "view", "to": "/"}}).to_string(),
        ))
        .await
        .expect("send");
    let refusal = loop {
        let message = next_text(&mut socket).await;
        if message["id"] == 3 {
            break message;
        }
    };
    assert_eq!(refusal["error"]["kind"], "forbidden", "{refusal}");

    // Another origin or host cannot open the socket.
    for (origin, host) in [
        ("http://evil.test", "viewer.test"),
        ("http://viewer.test", "evil.test"),
    ] {
        let refused = server.connect("/r/abc/api/ws", origin, host).await;
        assert_eq!(refused.err(), Some(403), "{origin} {host}");
    }
}

#[test]
fn an_interrupted_server_removes_the_socket_it_created() {
    let scratch = ScratchDir::new("web-proxy-exit");
    let profile = record_calls(&scratch);
    let (mut server, _) = Proxied::start(&scratch, &profile);
    let socket = server.socket.clone();
    assert!(socket.exists());

    // Ctrl-C, as a terminal sends it, is an orderly stop.
    let pid = nix::unistd::Pid::from_raw(i32::try_from(server.child.id()).expect("a pid"));
    nix::sys::signal::kill(pid, nix::sys::signal::Signal::SIGINT).expect("interrupt");
    let status = server.child.wait().expect("wait");
    assert!(status.success(), "{status}");
    assert!(!socket.exists());
}

#[test]
fn listen_unix_needs_the_public_url_it_serves_pages_at() {
    let scratch = ScratchDir::new("web-proxy-args");
    let socket = scratch.path().join("s.sock");
    let socket = socket.to_str().expect("a path");
    let output = refused(&["--listen-unix", socket]);
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("--public-url"),
        "{output:?}"
    );
    assert!(!scratch.path().join("s.sock").exists());
}

#[test]
fn trust_proxy_needs_the_public_url_it_serves_pages_at() {
    let output = refused(&["--trust-proxy"]);
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("--public-url"),
        "{output:?}"
    );
}

#[test]
fn listen_unix_conflicts_with_a_tcp_listener() {
    let scratch = ScratchDir::new("web-proxy-conflicts");
    let socket = scratch.path().join("s.sock");
    let socket = socket.to_str().expect("a path");
    for conflict in [["--port", "0"], ["--listen", "127.0.0.1:0"]] {
        let mut arguments = vec!["--listen-unix", socket, "--public-url", PUBLIC_URL];
        arguments.extend(conflict);
        let output = refused(&arguments);
        assert!(!output.status.success(), "{conflict:?}");
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("cannot be used with"),
            "{conflict:?}: {output:?}"
        );
    }
}

#[test]
fn an_existing_file_at_the_socket_path_is_never_replaced() {
    let scratch = ScratchDir::new("web-proxy-existing");
    let path = scratch.path().join("s.sock");
    std::fs::write(&path, "not a socket").expect("write");
    let output = refused(&[
        "--listen-unix",
        path.to_str().expect("a path"),
        "--public-url",
        PUBLIC_URL,
    ]);

    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("already exists"),
        "{output:?}"
    );
    assert_eq!(
        std::fs::read_to_string(&path).expect("read"),
        "not a socket"
    );
}

#[test]
fn trust_proxy_serves_only_profiles() {
    let scratch = ScratchDir::new("web-proxy-programs");
    let program = env!("CARGO_BIN_EXE_microscope");
    let socket = scratch.path().join("s.sock");
    let socket = socket.to_str().expect("a path");

    // A program to launch, a process to attach to, a core dump, or nothing.
    let cases: Vec<Vec<&str>> = vec![
        vec![program],
        vec!["--attach", "1"],
        vec!["--core", "/nonexistent/core"],
        vec![],
    ];

    for case in cases {
        let mut arguments = vec![
            "--listen-unix",
            socket,
            "--public-url",
            PUBLIC_URL,
            "--trust-proxy",
        ];
        arguments.extend(&case);
        let output = refused(&arguments);
        assert!(!output.status.success(), "{case:?}");
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("--trust-proxy serves only profiles"),
            "{case:?}: {output:?}"
        );
    }

    assert!(!scratch.path().join("s.sock").exists());
}
