//! Attaching to a Go server that runs on its own: its goroutines are
//! listed, a request stops at a breakpoint in its handler, and detaching
//! leaves it serving.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;

use microscope::StopReason;

use crate::invariants::check_go_stop;
use crate::stops::place;
use crate::support::{self, ExternalProcess, Scenario};

const SOURCE: &str = "tests/fixtures/go/served/main.go";

/// The body of the server's answer to a GET of `path`.
fn get(address: &str, path: &str) -> String {
    let mut stream = TcpStream::connect(address).expect("connect to the server");
    stream
        .set_read_timeout(Some(Duration::from_secs(20)))
        .expect("bound the read");
    write!(stream, "GET {path} HTTP/1.0\r\nHost: {address}\r\n\r\n").expect("send the request");

    let mut response = String::new();
    stream
        .read_to_string(&mut response)
        .expect("read the response");

    let (head, body) = response
        .split_once("\r\n\r\n")
        .unwrap_or_else(|| panic!("a response: {response:?}"));
    assert!(head.starts_with("HTTP/1.0 200"), "{head}");
    body.to_owned()
}

#[tokio::test]
async fn an_attached_server_stops_for_a_request_and_serves_on_once_detached() {
    let server = ExternalProcess::spawn(&Scenario::fixture("served-go"));
    let address = server
        .ready_line()
        .strip_prefix("READY ")
        .expect("the server's address")
        .to_owned();
    let mut scenario =
        Scenario::attached("served", server.attach().await).checking_stops(check_go_stop);

    // The server's goroutine waits to accept a connection.
    let tasks = scenario
        .operation("tasks", scenario.handle().tasks(None, 256))
        .await;
    assert!(tasks.gaps.is_empty(), "{:?}", tasks.gaps);
    assert!(
        tasks
            .tasks
            .iter()
            .any(|task| task.detail.as_deref() == Some("IO wait")),
        "{:#?}",
        tasks.tasks
    );

    let line = support::source_line(SOURCE, "// SERVED: count");
    let breakpoint = scenario.add_source_breakpoint("main.go", line).await;

    let first = {
        let address = address.clone();
        std::thread::spawn(move || get(&address, "/count"))
    };

    let reason = scenario.resume_to_stop().await;
    assert!(
        matches!(reason, StopReason::Breakpoint { .. }),
        "{reason:?}"
    );
    assert_eq!(place(&scenario).await, ("main.count".to_owned(), line));

    // Detached, the server answers the request it stopped in, and more.
    scenario.remove_breakpoint(breakpoint.id).await;
    scenario.shutdown().await;
    assert_eq!(first.join().expect("the first request"), "1");
    assert_eq!(get(&address, "/count"), "2");
    assert_eq!(get(&address, "/quit"), "bye");
    assert_eq!(server.wait().code(), Some(0));
}
