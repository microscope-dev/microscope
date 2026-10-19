//! Transports, malformed input, lost events, and abrupt ends.

use std::io::{BufRead as _, BufReader, Read as _, Write as _};
use std::net::{SocketAddr, TcpStream};
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;

use serde_json::{Value, json};

use crate::dap::{Configuration, Dap, Profile, fixture};
use crate::support::{self, ExternalProcess, ScratchDir};

/// `microscope dap --listen` on an unused port, killed on drop so a failing
/// test leaves nothing behind.
struct ListeningAdapter {
    child: std::process::Child,
    address: SocketAddr,
}

impl ListeningAdapter {
    /// Starts the adapter, logging its traffic to `log`, and waits for it
    /// to announce its address.
    fn start(log: &Path) -> Self {
        let child = Command::new(env!("CARGO_BIN_EXE_microscope"))
            .args(["dap", "--listen", "127.0.0.1:0", "--log"])
            .arg(log)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .expect("start the adapter");
        let mut adapter = Self {
            child,
            address: SocketAddr::from(([127, 0, 0, 1], 0)),
        };

        let mut line = String::new();
        BufReader::new(adapter.child.stderr.as_mut().expect("stderr"))
            .read_line(&mut line)
            .expect("read the listening address");
        adapter.address = line
            .trim()
            .strip_prefix("microscope dap listening on ")
            .unwrap_or_else(|| panic!("unexpected announcement {line:?}"))
            .parse()
            .expect("an address");

        adapter
    }

    /// Asks the adapter to stop, as an editor does, and checks that it
    /// exits cleanly.
    fn stop(mut self) {
        nix::sys::signal::kill(
            nix::unistd::Pid::from_raw(i32::try_from(self.child.id()).expect("pid")),
            nix::sys::signal::Signal::SIGTERM,
        )
        .expect("signal the adapter");
        assert!(self.child.wait().expect("wait").success());
    }
}

impl Drop for ListeningAdapter {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn tcp_serves_one_client_at_a_time_and_refuses_browsers() {
    let directory = ScratchDir::new("dap-listen");
    let log = directory.path().join("traffic.log");
    let adapter = ListeningAdapter::start(&log);

    // A web page's request carries Origin; the adapter drops it unanswered.
    let mut browser = TcpStream::connect(adapter.address).expect("connect");
    browser
        .set_read_timeout(Some(Duration::from_secs(10)))
        .expect("set a read timeout");
    let request = r#"{"seq":1,"type":"request","command":"initialize","arguments":{}}"#;
    write!(
        browser,
        "Origin: http://example.com\r\nContent-Length: {}\r\n\r\n{request}",
        request.len()
    )
    .expect("write");
    let mut answer = Vec::new();
    browser.read_to_end(&mut answer).expect("read");
    assert!(answer.is_empty(), "{}", String::from_utf8_lossy(&answer));

    let mut first = Dap::connect("tcp first", adapter.address);
    let started = first.launch(
        Profile::VsCode,
        &fixture("spin"),
        json!({}),
        &Configuration::default(),
    );
    first.event(started.mark, "process", |_| true);

    // A second client connects and asks during the first session, and is
    // served once it ends.
    let mut second = Dap::connect("tcp second", adapter.address);
    let initialize = second.send("initialize", json!({"adapterID": "microscope"}));
    first.finish();
    second.success(initialize);
    let launch = second.send("launch", json!({"program": fixture("basic")}));
    second.request("configurationDone", Value::Null);
    second.success(launch);
    assert_eq!(
        second.event(launch.mark, "exited", |_| true),
        json!({"exitCode": 0})
    );
    second.finish();
    adapter.stop();

    // The adapter logs each client's arrival and departure, which never
    // overlap: each client is accepted only after the one before is gone.
    let log = std::fs::read_to_string(&log).expect("read the traffic log");
    let sessions = log
        .lines()
        .filter_map(|line| line.strip_prefix("-- client "))
        .collect::<Vec<_>>();

    let browser = browser.local_addr().expect("browser address");
    let peers = sessions
        .chunks(2)
        .map(|pair| match pair {
            [connected, disconnected] => {
                let peer = connected
                    .strip_suffix(" connected")
                    .unwrap_or_else(|| panic!("{connected:?} is not an arrival:\n{log}"));
                assert_eq!(
                    disconnected.strip_suffix(" disconnected"),
                    Some(peer),
                    "{log}"
                );
                peer
            }
            _ => panic!("a client never left:\n{log}"),
        })
        .collect::<Vec<_>>();
    assert_eq!(peers.len(), 3, "{log}");
    assert_eq!(peers[0], browser.to_string(), "{log}");
}

#[test]
fn malformed_messages_are_answered_when_possible_and_never_end_the_session() {
    let mut dap = Dap::start("malformed");
    dap.initialize(Profile::VsCode);
    let missing_type = dap.send_raw(1000, "threads", r#"{"seq":1000,"command":"threads"}"#);
    assert_eq!(dap.failure(missing_type), "the message has no `type`");
    let mark = dap.mark();
    dap.write("{not json");
    assert!(
        dap.output_containing(mark, "important", "ignored a malformed message")
            .contains("not valid JSON")
    );
    assert_eq!(
        dap.request_error(
            "setBreakpoints",
            json!({"source": {"path": "/a.c"}, "breakpoints": "all"})
        ),
        "invalid setBreakpoints arguments at breakpoints: invalid type: string \"all\", expected a sequence"
    );
    assert_eq!(
        dap.request_error("stackTrace", json!({"threadId": "one"})),
        "invalid stackTrace arguments at threadId: invalid type: string \"one\", expected i64"
    );
    dap.finish();
}

#[test]
fn broken_framing_ends_the_session_and_the_program() {
    let mut dap = Dap::start("framing");
    let started = dap.launch(
        Profile::VsCode,
        &fixture("spin"),
        json!({}),
        &Configuration::default(),
    );
    dap.event(started.mark, "process", |_| true);
    dap.write_bytes(b"Content-Length: lots\r\n\r\n");
    dap.wait_for_exit();
    dap.close_stdin();
    dap.finish();
}

#[test]
fn killing_the_adapter_kills_its_program_and_releases_attached_ones() {
    let mut launched = Dap::start("killed launched");
    let started = launched.launch(
        Profile::VsCode,
        &fixture("spin"),
        json!({}),
        &Configuration::default(),
    );
    launched.event(started.mark, "process", |_| true);
    let pid = launched.process_id().expect("process");
    launched.signal(nix::sys::signal::Signal::SIGKILL);
    launched.abandon();

    // Nothing reports the death of a process this test did not start.
    support::wait_until("the launched program is gone with its adapter", || {
        std::fs::read_to_string(format!("/proc/{pid}/status"))
            .map_or(true, |status| status.contains("State:\tZ"))
    });

    let mut process = ExternalProcess::spawn(&fixture("attach"));
    let mut attached = Dap::start("killed attached");
    let started = attached.begin(
        Profile::VsCode,
        (
            "attach",
            json!({"pid": process.process_id().get(), "stopOnEntry": true}),
        ),
        &Configuration::default(),
    );
    attached.stopped(started.mark);
    attached.signal(nix::sys::signal::Signal::SIGKILL);
    attached.abandon();

    // The kernel detaches a dead tracer's tracees and resumes the ones it
    // held stopped: this one is untraced and back waiting for its input.
    let status = std::fs::read_to_string(format!("/proc/{}/status", process.process_id()))
        .expect("the attached process lives");
    assert!(
        status
            .lines()
            .any(|line| line.split_whitespace().eq(["TracerPid:", "0"])),
        "{status}"
    );
    let state = status
        .lines()
        .find(|line| line.starts_with("State:"))
        .expect("state");
    assert!(!state.contains("stopped"), "{state}");

    process.release();
    let status = process.wait();
    assert_eq!(status.code(), Some(23), "{status:?}");
}

#[test]
fn missed_events_are_recovered_from_the_debuggers_state() {
    // With room for one event, the adapter misses most of each burst of
    // thread starts and exits, and must catch up from the debugger's state:
    // at every stop, the threads the client was told of are the threads
    // there are. A stop waits for every exiting thread to be gone, so no
    // thread starts or exits while the program is stopped.
    let mut dap = Dap::start_in("lagged threads", &[("MICROSCOPE_EVENT_CAPACITY", "1")]);
    let started = dap.launch(
        Profile::Helix,
        &fixture("thread-stress"),
        json!({}),
        &Configuration {
            functions: vec!["churn_breakpoint".to_owned()],
            ..Configuration::default()
        },
    );
    let mut mark = started.mark;

    for _ in 0..16 {
        let stop = dap.stopped(mark);
        let threads = dap.request("threads", Value::Null)["threads"]
            .as_array()
            .expect("threads")
            .iter()
            .map(|thread| thread["id"].as_i64().expect("id"))
            .collect::<std::collections::BTreeSet<_>>();
        assert_eq!(dap.known_threads(), threads);
        assert!(threads.contains(&stop.thread));
        let resumed = dap.send("continue", json!({"threadId": stop.thread}));
        dap.success(resumed);
        mark = resumed.mark;
    }

    dap.request("setFunctionBreakpoints", json!({"breakpoints": []}));

    // A thread can reach the breakpoint before its removal takes effect,
    // which stops the program there once more.
    let (kind, body) = dap.next_event(mark, &["stopped", "exited"]);
    let exited = if kind == "stopped" {
        assert!(
            matches!(
                body["reason"].as_str(),
                Some("function breakpoint" | "breakpoint")
            ),
            "{body}"
        );
        let resumed = dap.send("continue", json!({"threadId": body["threadId"]}));
        dap.success(resumed);
        dap.event(resumed.mark, "exited", |_| true)
    } else {
        body
    };

    assert_eq!(exited, json!({"exitCode": 0}));
    dap.event(mark, "terminated", |_| true);
    dap.finish();

    // Libraries load and unload in bursts too: at every stop, the modules
    // the client was told of are the modules loaded.
    let mut dap = Dap::start_in("lagged modules", &[("MICROSCOPE_EVENT_CAPACITY", "1")]);
    let started = dap.launch(
        Profile::VsCode,
        &fixture("globals-shared"),
        json!({}),
        &Configuration {
            functions: vec!["dso_touch".to_owned(), "after_unload".to_owned()],
            ..Configuration::default()
        },
    );
    let mut mark = started.mark;

    for _ in 0..3 {
        let stop = dap.stopped(mark);
        let modules = dap.request("modules", json!({}))["modules"]
            .as_array()
            .expect("modules")
            .iter()
            .map(|module| module["id"].as_str().expect("id").to_owned())
            .collect::<std::collections::BTreeSet<_>>();
        assert_eq!(dap.known_modules(), modules);
        let resumed = dap.send("continue", json!({"threadId": stop.thread}));
        dap.success(resumed);
        mark = resumed.mark;
    }

    assert_eq!(dap.event(mark, "exited", |_| true), json!({"exitCode": 0}));
    dap.finish();
}

#[test]
fn requests_cancelled_while_waiting_their_turn_answer_that_they_were_cancelled() {
    // The adapter waits for the client's terminal inside the launch, so the
    // requests sent meanwhile wait their turn.
    let mut dap = Dap::start("cancel waiting requests");
    dap.hold_terminals();
    dap.initialize(Profile::VsCode);
    let launch = dap.send(
        "launch",
        json!({"program": fixture("spin"), "console": "integratedTerminal"}),
    );
    let done = dap.send("configurationDone", Value::Null);
    let cancelled = dap.send("threads", Value::Null);
    let kept = dap.send("threads", Value::Null);
    let cancel = dap.send("cancel", json!({"requestId": cancelled.seq}));
    dap.release_terminals();
    dap.success(done);
    dap.success(launch);
    let response = dap.response(cancelled);
    assert_eq!(
        (&response["success"], &response["message"]),
        (&json!(false), &json!("cancelled"))
    );
    dap.success(kept);
    dap.success(cancel);
    dap.finish();
}
