//! Connecting, staying connected, and stopping.

use std::time::{Duration, Instant};

use axum::extract::ws::Message;
use axum::http::StatusCode;

use crate::service::Service;
use crate::service::proto::{Goodbye, Protocol, goodbye, server_message};
use crate::{Agent, Scratch, TOKEN};

fn logged_in(service: &Service, name: &str) -> Scratch {
    let scratch = Scratch::new(name);
    scratch.log_in(&format!("server = \"{}\"\n", service.url));
    scratch
}

#[test]
fn says_hello_with_its_token() {
    let service = Service::start();
    let scratch = logged_in(&service, "hello");
    let mut agent = Agent::start(&scratch, &[]);

    let connection = service.accept();
    assert_eq!(
        connection.authorization.as_deref(),
        Some(&*format!("Bearer {TOKEN}"))
    );
    assert!(
        connection
            .user_agent
            .as_deref()
            .is_some_and(|agent| agent.starts_with("microscope/")),
        "{:?}",
        connection.user_agent
    );

    let hello = connection.hello();
    assert_eq!(hello.protocol(), Protocol::V1);
    assert_eq!(hello.agent_version, env!("CARGO_PKG_VERSION"));
    let hostname = std::fs::read_to_string("/proc/sys/kernel/hostname").unwrap();
    assert_eq!(hello.hostname, hostname.trim());
    assert!(!hello.accepts_commands);

    connection.welcome(crate::service::CHUNK, crate::service::WINDOW);
    agent.line(&format!("connected to {} as dev@example.com", service.url));
}

#[test]
fn the_server_flag_overrides_the_settings() {
    let service = Service::start();
    let scratch = Scratch::new("server-flag");
    scratch.log_in("server = \"http://127.0.0.1:1/\"\n");
    let _agent = Agent::start(&scratch, &["--server", &service.url]);
    service.welcomed();
}

#[test]
fn refused_tokens_stop_the_agent() {
    let service = Service::start();
    let scratch = logged_in(&service, "unauthorized");
    service.refuse_next(StatusCode::UNAUTHORIZED);
    let agent = Agent::start(&scratch, &[]);

    let (status, stderr) = agent.exit();
    assert_eq!(status.code(), Some(1), "{stderr}");
    assert!(stderr.contains("microscope agent login"), "{stderr}");
}

#[test]
fn busy_services_are_tried_again() {
    let service = Service::start();
    let scratch = logged_in(&service, "busy");
    service.refuse_next(StatusCode::SERVICE_UNAVAILABLE);
    service.refuse_next(StatusCode::TOO_MANY_REQUESTS);
    let mut agent = Agent::start(&scratch, &[]);

    agent.line("the service is busy; connecting again in");
    agent.line("as many agents connected as the service allows");
    service.welcomed();
    agent.line("connected to");
}

#[test]
fn unreachable_services_are_tried_again() {
    let scratch = Scratch::new("unreachable");
    // Nothing listens on port 1.
    scratch.log_in("server = \"http://127.0.0.1:1/\"\n");
    let mut agent = Agent::start(&scratch, &[]);
    agent.line("connecting to http://127.0.0.1:1/ failed");
    agent.line("connecting again in");
}

#[test]
fn dropped_connections_reconnect() {
    let service = Service::start();
    let scratch = logged_in(&service, "reconnect");
    let mut agent = Agent::start(&scratch, &[]);

    drop(service.welcomed());
    agent.line("connecting again in");

    // The first retry waits a second at most.
    let started = Instant::now();
    service.welcomed();
    assert!(started.elapsed() < Duration::from_secs(5));
}

#[test]
fn a_goodbye_that_allows_a_retry_reconnects() {
    let service = Service::start();
    let scratch = logged_in(&service, "goodbye-retry");
    let mut agent = Agent::start(&scratch, &[]);

    let connection = service.welcomed();
    connection.send(server_message::Body::Goodbye(Goodbye {
        reason: goodbye::Reason::ShuttingDown.into(),
        message: String::new(),
        retry: true,
    }));
    agent.line("the service is restarting; connecting again in");
    service.welcomed();
}

#[test]
fn a_goodbye_that_forbids_a_retry_stops_the_agent() {
    let service = Service::start();
    let scratch = logged_in(&service, "goodbye-final");
    let agent = Agent::start(&scratch, &[]);

    let connection = service.welcomed();
    connection.send(server_message::Body::Goodbye(Goodbye {
        reason: goodbye::Reason::Unauthenticated.into(),
        message: "the device token was revoked".to_owned(),
        retry: false,
    }));
    let (status, stderr) = agent.exit();
    assert_eq!(status.code(), Some(1));
    assert!(stderr.contains("the device token was revoked"), "{stderr}");
}

#[test]
fn a_goodbye_before_the_welcome_is_heard() {
    let service = Service::start();
    let scratch = logged_in(&service, "goodbye-hello");
    let agent = Agent::start(&scratch, &[]);

    let connection = service.accept();
    connection.hello();
    connection.send(server_message::Body::Goodbye(Goodbye {
        reason: goodbye::Reason::ProtocolUnsupported.into(),
        message: String::new(),
        retry: false,
    }));
    let (status, stderr) = agent.exit();
    assert_eq!(status.code(), Some(1));
    assert!(stderr.contains("upgrade microscope"), "{stderr}");
}

#[test]
fn services_that_never_welcome_are_left() {
    let service = Service::start();
    let scratch = logged_in(&service, "no-welcome");
    let mut agent = Agent::start(&scratch, &[]);

    let connection = service.accept();
    connection.hello();
    agent.line("the service did not answer hello");
    service.welcomed();
}

#[test]
fn silent_services_are_left() {
    let service = Service::start();
    let scratch = logged_in(&service, "silent");
    let mut agent = Agent::start(&scratch, &["--idle-timeout", "1"]);

    let _connection = service.welcomed();
    agent.line("the service stopped answering");
    service.welcomed();
}

#[test]
fn pings_keep_the_connection() {
    let service = Service::start();
    let scratch = logged_in(&service, "pings");
    let _agent = Agent::start(&scratch, &["--idle-timeout", "1"]);

    let connection = service.welcomed();
    for _ in 0..6 {
        std::thread::sleep(Duration::from_millis(400));
        connection.send_raw(Message::Ping(Vec::new().into()));
    }
    service.no_connection_within(Duration::from_millis(100));
    connection.nothing_within(Duration::from_millis(100));
}

#[test]
fn bad_messages_end_the_connection() {
    let service = Service::start();
    let scratch = logged_in(&service, "bad-message");
    let mut agent = Agent::start(&scratch, &[]);

    let connection = service.welcomed();
    connection.send_raw(Message::Binary(b"not zstd".to_vec().into()));
    assert_eq!(connection.closed(), Some(1002));
    agent.line("the service sent a bad message");

    let connection = service.welcomed();
    connection.send_raw(Message::Text("hello".into()));
    assert_eq!(connection.closed(), Some(1002));
    agent.line("the service sent a text message");
}

#[test]
fn interrupting_closes_the_connection_and_exits() {
    let service = Service::start();
    let scratch = logged_in(&service, "interrupt");
    let mut agent = Agent::start(&scratch, &[]);

    let connection = service.welcomed();
    agent.line("connected to");
    agent.interrupt();
    assert_eq!(connection.closed(), Some(1001));
    let (status, _) = agent.exit();
    assert!(status.success());
}

#[test]
fn interrupting_while_waiting_to_reconnect_exits() {
    let scratch = Scratch::new("interrupt-waiting");
    scratch.log_in("server = \"http://127.0.0.1:1/\"\n");
    let mut agent = Agent::start(&scratch, &[]);
    agent.line("connecting again in");
    agent.interrupt();
    let (status, _) = agent.exit();
    assert!(status.success());
}
