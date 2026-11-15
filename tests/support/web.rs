use std::fmt::Write as _;
use std::io::{BufRead as _, BufReader, Read as _, Write as _};
use std::net::{SocketAddr, TcpStream};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures_util::{SinkExt as _, StreamExt as _};
use serde_json::{Value, json};
use tokio::net::TcpStream as AsyncTcpStream;
use tokio::time::timeout;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest as _;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

use crate::support::flight_recordings;

pub const DEADLINE: Duration = Duration::from_secs(10);

type Transcript = Arc<Mutex<Vec<String>>>;

pub struct Web {
    name: String,
    child: Option<Child>,
    pub address: SocketAddr,
    pub control_token: String,

    pub base: String,

    pub public: Option<(String, String)>,

    pub link_to: Option<String>,
    transcript: Transcript,
    recording: Option<PathBuf>,
    started: Instant,
}

impl Web {
    pub fn start(name: &str, arguments: &[&str]) -> Self {
        let mut command = Command::new(env!("CARGO_BIN_EXE_microscope"));
        command
            .args(["web", "--port", "0"])
            .env("USER", "tester")
            .args(arguments)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let recording = flight_recordings::watch_adapter(&mut command);
        let mut child = command.spawn().expect("start microscope web");
        let started = Instant::now();
        let transcript = Transcript::default();
        let stderr = child.stderr.take().expect("stderr");
        let errors = Arc::clone(&transcript);

        std::thread::spawn(move || {
            for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                errors
                    .lock()
                    .expect("transcript")
                    .push(format!("!! server stderr: {line}"));
            }
        });

        let mut stdout = BufReader::new(child.stdout.take().expect("stdout"));
        let mut link = None;
        let mut serving = None;
        let mut printed = String::new();
        while link.is_none() {
            printed.clear();
            assert!(
                stdout
                    .read_line(&mut printed)
                    .expect("read the server's output")
                    > 0,
                "microscope web exited before printing its link"
            );
            if let Some(address) = printed
                .trim()
                .strip_prefix("microscope web: serving http://")
            {
                serving = Some(address.to_owned());
            }

            link = printed.trim().strip_prefix("open ").map(str::to_owned);
        }

        std::thread::spawn(move || {
            let mut rest = Vec::new();
            let _ = stdout.read_to_end(&mut rest);
        });

        let link = link.expect("a link");
        let serving = serving.expect("the address it serves at");
        let (joining, token) = link.rsplit_once('#').expect("a join link");
        let (front, to) = joining.rsplit_once("join").expect("a join link");
        let link_to = to.strip_prefix("?to=").map(str::to_owned);

        let (public, base) = if front == format!("http://{serving}/") {
            (None, "/".to_owned())
        } else {
            let (scheme, rest) = front.split_once("://").expect("a URL");
            let (host, path) = rest.split_once('/').expect("a path");
            (
                Some((host.to_owned(), format!("{scheme}://{host}"))),
                format!("/{path}"),
            )
        };

        Self {
            name: name.to_owned(),
            child: Some(child),
            address: serving.parse().expect("a socket address"),
            control_token: token.to_owned(),
            base,
            public,
            link_to,
            transcript,
            recording,
            started,
        }
    }

    pub fn origin(&self) -> String {
        self.public.as_ref().map_or_else(
            || format!("http://{}", self.address),
            |(_, origin)| origin.clone(),
        )
    }

    pub fn host(&self) -> String {
        self.public
            .as_ref()
            .map_or_else(|| self.address.to_string(), |(host, _)| host.clone())
    }

    pub fn post_login(
        &self,
        token: &str,
        origin: Option<&str>,
        host: Option<&str>,
    ) -> (u16, Option<String>) {
        let mut request = format!(
            "POST {}api/login HTTP/1.1\r\nConnection: close\r\nContent-Length: {}\r\n",
            self.base,
            token.len()
        );
        if let Some(host) = host {
            let _ = write!(request, "Host: {host}\r\n");
        }

        if let Some(origin) = origin {
            let _ = write!(request, "Origin: {origin}\r\n");
        }

        request.push_str("\r\n");
        request.push_str(token);

        let response = self.http(&request);
        let status = response
            .split(' ')
            .nth(1)
            .and_then(|code| code.parse().ok())
            .expect("a status line");

        let cookie = response.lines().find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("set-cookie").then(|| {
                value
                    .trim()
                    .split(';')
                    .next()
                    .unwrap_or_default()
                    .to_owned()
            })
        });

        (status, cookie)
    }

    pub fn http_bytes(&self, request: &str) -> (String, Vec<u8>) {
        let mut stream = TcpStream::connect(self.address).expect("connect");
        stream
            .set_read_timeout(Some(DEADLINE))
            .expect("read timeout");

        stream.write_all(request.as_bytes()).expect("send");

        let mut response = Vec::new();
        let _ = stream.read_to_end(&mut response);

        let end = response
            .windows(4)
            .position(|window| window == b"\r\n\r\n")
            .expect("a response head");

        let body = response.split_off(end + 4);

        (String::from_utf8_lossy(&response).into_owned(), body)
    }

    pub fn http(&self, request: &str) -> String {
        let mut stream = TcpStream::connect(self.address).expect("connect");
        stream
            .set_read_timeout(Some(DEADLINE))
            .expect("read timeout");

        stream.write_all(request.as_bytes()).expect("send");
        let mut response = String::new();
        let _ = stream.read_to_string(&mut response);

        response
    }

    pub fn cookie(&self, token: &str) -> String {
        let host = self.host();
        let (status, cookie) = self.post_login(token, Some(&self.origin()), Some(&host));
        assert_eq!(status, 204, "login with {token}");
        cookie.expect("a cookie")
    }

    pub async fn control(&self, name: &str) -> Client {
        let cookie = self.cookie(&self.control_token.clone());

        self.connect(
            name,
            Some(&cookie),
            Some(&self.origin()),
            Some(&self.host()),
            None,
        )
        .await
        .expect("connect")
    }

    pub async fn joining(&self, name: &str, link: &str) -> Client {
        let token = link.rsplit_once('#').expect("a token in the link").1;
        let cookie = self.cookie(token);

        self.connect(name, Some(&cookie), Some(&self.origin()), None, None)
            .await
            .expect("connect")
    }

    pub async fn connect(
        &self,
        name: &str,
        cookie: Option<&str>,
        origin: Option<&str>,
        host: Option<&str>,
        protocols: Option<&str>,
    ) -> Result<Client, u16> {
        let mut request = format!("ws://{}{}api/ws", self.address, self.base)
            .into_client_request()
            .expect("a request");

        let headers = request.headers_mut();
        if let Some(cookie) = cookie {
            headers.insert("Cookie", cookie.parse().expect("header"));
        }

        if let Some(origin) = origin {
            headers.insert("Origin", origin.parse().expect("header"));
        }

        if let Some(host) = host {
            headers.insert("Host", host.parse().expect("header"));
        }

        if let Some(protocols) = protocols {
            headers.insert("Sec-WebSocket-Protocol", protocols.parse().expect("header"));
        }

        match timeout(DEADLINE, tokio_tungstenite::connect_async(request)).await {
            Ok(Ok((socket, _))) => Ok(Client {
                name: name.to_owned(),
                socket,
                next_id: 1,
                pending: Vec::new(),
                transcript: Arc::clone(&self.transcript),
                traffic: Vec::new(),
                started: self.started,
                state: None,
                binary: Vec::new(),
            }),
            Ok(Err(tokio_tungstenite::tungstenite::Error::Http(response))) => {
                Err(response.status().as_u16())
            }
            Ok(Err(error)) => panic!("connect: {error}"),
            Err(elapsed) => panic!("connecting timed out: {elapsed}"),
        }
    }

    pub fn interrupt(&mut self) -> std::process::ExitStatus {
        self.stop(nix::sys::signal::Signal::SIGINT)
    }

    pub fn terminate(&mut self) -> std::process::ExitStatus {
        self.stop(nix::sys::signal::Signal::SIGTERM)
    }

    fn stop(&mut self, signal: nix::sys::signal::Signal) -> std::process::ExitStatus {
        let mut child = self.child.take().expect("a running server");
        let pid = nix::unistd::Pid::from_raw(i32::try_from(child.id()).expect("a pid"));

        let _ = nix::sys::signal::kill(pid, signal);

        let deadline = Instant::now() + DEADLINE;
        loop {
            if let Some(status) = child.try_wait().expect("wait") {
                return status;
            }

            if Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                panic!("microscope web did not exit on {signal}");
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn print_transcript(&self) {
        eprintln!("--- {} transcript ---", self.name);
        for line in self.transcript.lock().expect("transcript").iter() {
            eprintln!("{line}");
        }
    }
}

impl Drop for Web {
    fn drop(&mut self) {
        let failed = std::thread::panicking();

        if self.child.is_some() {
            self.interrupt();
        }

        if failed {
            self.print_transcript();
        } else if let Some(recording) = &self.recording {
            flight_recordings::adapter_finished(recording);
        }
    }
}

pub struct Client {
    name: String,
    socket: WebSocketStream<MaybeTlsStream<AsyncTcpStream>>,
    next_id: u64,

    pending: Vec<Value>,
    transcript: Transcript,

    traffic: Vec<Value>,
    started: Instant,

    state: Option<Value>,

    binary: Vec<(u64, Vec<u8>)>,
}

impl Client {
    fn log(&self, direction: &str, text: &str) {
        let text = match text.char_indices().nth(400) {
            Some((end, _)) => format!("{}… ({} bytes)", &text[..end], text.len()),
            None => text.to_owned(),
        };

        self.transcript.lock().expect("transcript").push(format!(
            "[{:8.3}] {} {direction} {text}",
            self.started.elapsed().as_secs_f64(),
            self.name
        ));
    }

    pub async fn send_raw(&mut self, text: &str) {
        self.log(">>", text);

        if let Ok(value) = serde_json::from_str::<Value>(text) {
            self.traffic.push(json!({"to": "server", "message": value}));
        }

        self.socket.send(Message::text(text)).await.expect("send");
    }

    pub async fn send(&mut self, method: &str, params: Value) -> u64 {
        let id = self.next_id;
        self.next_id += 1;

        let mut message = json!({"id": id, "method": method});

        if !params.is_null() {
            message["params"] = params;
        }

        self.send_raw(&message.to_string()).await;
        id
    }

    pub async fn next(&mut self) -> Value {
        if !self.pending.is_empty() {
            return self.pending.remove(0);
        }

        self.receive().await
    }

    async fn receive(&mut self) -> Value {
        loop {
            let message = timeout(DEADLINE, self.socket.next())
                .await
                .unwrap_or_else(|_| panic!("{} waited too long for a message", self.name))
                .unwrap_or_else(|| panic!("{} was disconnected", self.name))
                .expect("a message");

            let text = match message {
                Message::Text(text) => text,
                Message::Binary(bytes) => {
                    let (id, bytes) = bytes.split_at(8);
                    let id = u64::from_le_bytes(id.try_into().expect("an id"));
                    self.log("<<", &format!("{} bytes for {id}", bytes.len()));
                    let hex = bytes.iter().fold(String::new(), |mut hex, byte| {
                        let _ = write!(hex, "{byte:02x}");
                        hex
                    });
                    self.traffic
                        .push(json!({"to": "page", "binary": {"id": id, "hex": hex}}));
                    self.binary.push((id, bytes.to_vec()));
                    continue;
                }
                _ => continue,
            };

            self.log("<<", &text);
            let value: Value = serde_json::from_str(&text).expect("JSON");
            self.traffic.push(json!({"to": "page", "message": value}));

            if value["type"] == "state" {
                self.state = Some(value.clone());
            }

            return value;
        }
    }

    pub async fn expect(&mut self, what: &str, matches: impl Fn(&Value) -> bool) -> Value {
        if let Some(index) = self.pending.iter().position(&matches) {
            return self.pending.remove(index);
        }

        let deadline = Instant::now() + DEADLINE;
        loop {
            assert!(Instant::now() < deadline, "{} never saw {what}", self.name);
            let message = self.receive().await;
            if matches(&message) {
                return message;
            }

            self.pending.push(message);
        }
    }

    pub async fn request(
        &mut self,
        method: &str,
        params: Value,
    ) -> Result<Value, (String, String)> {
        let id = self.send(method, params).await;
        let answer = self
            .expect(&format!("the answer to {method}"), |message| {
                message["id"] == id && matches!(message["type"].as_str(), Some("result" | "error"))
            })
            .await;

        if answer["type"] == "result" {
            Ok(answer["result"].clone())
        } else {
            Err((
                answer["error"]["kind"]
                    .as_str()
                    .unwrap_or_default()
                    .to_owned(),
                answer["error"]["message"]
                    .as_str()
                    .unwrap_or_default()
                    .to_owned(),
            ))
        }
    }

    pub async fn draw(&mut self, params: Value) -> Result<(Value, Vec<u8>), (String, String)> {
        let (drawing, bytes) = self.with_bytes("draw", params).await?;

        assert_eq!(
            drawing["bytes"].as_u64(),
            Some(bytes.len() as u64),
            "the binary frame holds the bytes the drawing counts"
        );

        Ok((drawing, bytes))
    }

    pub async fn with_bytes(
        &mut self,
        method: &str,
        params: Value,
    ) -> Result<(Value, Vec<u8>), (String, String)> {
        let id = self.next_id;
        let answer = self.request(method, params).await?;

        let index = self.binary.iter().position(|(frame, _)| *frame == id);
        let bytes = index.map_or_else(Vec::new, |index| self.binary.remove(index).1);

        Ok((answer, bytes))
    }

    pub async fn ok(&mut self, method: &str, params: Value) -> Value {
        self.request(method, params)
            .await
            .unwrap_or_else(|error| panic!("{method} failed: {error:?}"))
    }

    pub const fn latest_state(&self) -> Option<&Value> {
        self.state.as_ref()
    }

    pub async fn state(&mut self, what: &str, condition: impl Fn(&Value) -> bool) -> Value {
        if let Some(state) = &self.state
            && condition(state)
            && !self
                .pending
                .iter()
                .any(|message| message["type"] == "state")
        {
            return state.clone();
        }

        let deadline = Instant::now() + DEADLINE;
        loop {
            assert!(
                Instant::now() < deadline,
                "{} never reached {what}",
                self.name
            );

            if let Some(index) = self
                .pending
                .iter()
                .rposition(|message| message["type"] == "state")
            {
                let state = self.pending.remove(index);
                self.pending.retain(|message| message["type"] != "state");
                if condition(&state) {
                    return state;
                }

                continue;
            }

            let message = self.receive().await;
            if message["type"] != "state" {
                self.pending.push(message);
            } else if condition(&message) {
                return message;
            }
        }
    }

    pub async fn inferior(&mut self, inferior: &str) -> Value {
        self.state(&format!("an inferior {inferior}"), |state| {
            state["inferior"]["state"] == inferior
        })
        .await
    }

    pub async fn output_until(&mut self, stream: &str, ends: &str) -> String {
        let mut text = String::new();
        let deadline = Instant::now() + DEADLINE;
        let mut index = 0;
        loop {
            while index < self.pending.len() {
                let message = &self.pending[index];
                if message["type"] == "output" && message["stream"] == stream {
                    text.push_str(message["text"].as_str().unwrap_or_default());
                    self.pending.remove(index);
                } else {
                    index += 1;
                }
            }

            if text.contains(ends) {
                return text;
            }

            assert!(
                Instant::now() < deadline,
                "{} never printed {ends:?}",
                self.name
            );

            let message = self.receive().await;
            self.pending.push(message);
        }
    }

    pub fn save_traffic(&self, name: &str) {
        let Some(directory) = std::env::var_os("MICROSCOPE_WEB_TRANSCRIPTS") else {
            return;
        };

        let root = env!("CARGO_MANIFEST_DIR");
        let text = self
            .traffic
            .iter()
            .map(|line| {
                let mut line = line.clone();

                if let Some(output) = line["message"]["text"].as_str()
                    && output.len() > 200
                {
                    let mut end = 120;
                    while !output.is_char_boundary(end) {
                        end -= 1;
                    }

                    line["message"]["text"] = Value::from(format!("{}…", &output[..end]));
                }

                line.to_string().replace(root, "<root>")
            })
            .collect::<Vec<_>>()
            .join("\n");

        let path = PathBuf::from(directory).join(format!("{name}.jsonl"));
        std::fs::write(&path, text + "\n").expect("write the transcript");
    }
}

pub fn exists(pid: u64) -> bool {
    std::path::Path::new(&format!("/proc/{pid}")).exists()
}

pub fn wait_gone(pid: u64) {
    let deadline = Instant::now() + DEADLINE;
    while exists(pid) {
        assert!(
            Instant::now() < deadline,
            "process {pid} outlived the server"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}
