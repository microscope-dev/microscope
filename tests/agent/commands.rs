//! Commands from the service, which run only when the user allows them.

use std::process::{Command, Stdio};

use prost::bytes::Bytes;

use crate::service::proto::{
    Command as ServiceCommand, CommandResult, ListProcesses, Record, UploadResume, agent_message,
    command, command_result, server_message,
};
use crate::service::{Connection, Service};
use crate::{Agent, Scratch};

fn allowed(service: &Service, name: &str, allow: bool) -> Scratch {
    let scratch = Scratch::new(name);
    scratch.log_in(&format!(
        "server = \"{}\"\nallow-remote = {allow}\n",
        service.url
    ));
    scratch
}

fn ask(connection: &Connection, id: u64, kind: command::Kind) -> command_result::Result {
    connection.send(server_message::Body::Command(ServiceCommand {
        id,
        kind: Some(kind),
    }));
    match connection.recv() {
        agent_message::Body::CommandResult(CommandResult {
            id: answered,
            result,
        }) => {
            assert_eq!(answered, id);
            result.expect("a result")
        }
        other => panic!("expected a command's result, got {other:?}"),
    }
}

fn error(result: command_result::Result) -> String {
    match result {
        command_result::Result::Error(error) => error.message,
        other => panic!("expected an error, got {other:?}"),
    }
}

/// A process to record, killed when dropped.
struct Sleeper(std::process::Child);

impl Sleeper {
    fn start() -> Self {
        Self(
            Command::new("sleep")
                .arg("60")
                .stdin(Stdio::null())
                .spawn()
                .unwrap(),
        )
    }

    fn pid(&self) -> u32 {
        self.0.id()
    }
}

impl Drop for Sleeper {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn commands_are_refused_unless_allowed() {
    let service = Service::start();
    let scratch = allowed(&service, "refused", false);
    let _agent = Agent::start(&scratch, &[]);

    let connection = service.accept();
    assert!(!connection.hello().accepts_commands);
    connection.welcome(crate::service::CHUNK, crate::service::WINDOW);
    let refused = error(ask(
        &connection,
        7,
        command::Kind::ListProcesses(ListProcesses {}),
    ));
    assert!(refused.contains("allow-remote"), "{refused}");
}

#[test]
fn the_allow_remote_flag_allows_commands() {
    let service = Service::start();
    let scratch = allowed(&service, "flag", false);
    let mut agent = Agent::start(&scratch, &["--allow-remote"]);
    agent.line("the service may record this user's processes");
    let connection = service.accept();
    assert!(connection.hello().accepts_commands);
}

#[test]
fn lists_this_users_processes() {
    let service = Service::start();
    let scratch = allowed(&service, "list", true);
    let sleeper = Sleeper::start();
    let _agent = Agent::start(&scratch, &[]);

    let connection = service.accept();
    assert!(connection.hello().accepts_commands);
    connection.welcome(crate::service::CHUNK, crate::service::WINDOW);
    let command_result::Result::Processes(list) = ask(
        &connection,
        1,
        command::Kind::ListProcesses(ListProcesses {}),
    ) else {
        panic!("expected a list");
    };
    let sleeping = list
        .processes
        .iter()
        .find(|process| process.pid == sleeper.pid())
        .expect("the list holds the sleeping process");
    assert_eq!(sleeping.name, "sleep");
    assert!(
        sleeping.command_line.ends_with("sleep 60"),
        "{}",
        sleeping.command_line
    );
    assert!(list.processes.iter().all(|process| process.pid != 1));
}

#[test]
fn records_a_process_and_uploads_the_recording() {
    let service = Service::start();
    let scratch = allowed(&service, "record", true);
    let sleeper = Sleeper::start();
    let mut agent = Agent::start(&scratch, &[]);
    let connection = service.welcomed();

    let result = ask(
        &connection,
        2,
        command::Kind::Record(Record {
            pid: sleeper.pid(),
            seconds: 1,
        }),
    );
    let command_result::Result::RecordStarted(started) = result else {
        panic!("expected the recording to start, got {result:?}");
    };
    assert_eq!(started.upload_key.len(), 16);
    agent.line(&format!("recording process {} for 1 s", sleeper.pid()));

    // Once recorded, it is uploaded under the key the answer gave.
    let begin = match connection.recv() {
        agent_message::Body::UploadBegin(begin) => begin,
        other => panic!("expected an upload, got {other:?}"),
    };
    assert_eq!(begin.upload_key, started.upload_key);
    assert_eq!(begin.name, "sleep");
    connection.send(server_message::Body::UploadResume(UploadResume {
        upload_key: begin.upload_key.clone(),
        offset: 0,
    }));
    let mut bytes = Vec::new();
    loop {
        match connection.recv() {
            agent_message::Body::UploadChunk(chunk) => {
                bytes.extend_from_slice(&chunk.data);
                connection.send(server_message::Body::UploadAck(
                    crate::service::proto::UploadAck {
                        upload_key: begin.upload_key.clone(),
                        offset: bytes.len() as u64,
                    },
                ));
            }
            agent_message::Body::UploadEnd(_) => break,
            other => panic!("expected a chunk, got {other:?}"),
        }
    }
    assert!(bytes.starts_with(b"USCOPEPF"));
    assert_eq!(bytes.len() as u64, begin.size);
}

#[test]
fn recording_what_cannot_be_recorded_fails() {
    let service = Service::start();
    let scratch = allowed(&service, "record-bad", true);
    let sleeper = Sleeper::start();
    let _agent = Agent::start(&scratch, &[]);
    let connection = service.welcomed();

    let record = |id, pid, seconds| {
        ask(
            &connection,
            id,
            command::Kind::Record(Record { pid, seconds }),
        )
    };
    let message = error(record(1, sleeper.pid(), 0));
    assert!(message.contains("1 to 600 seconds"), "{message}");
    let message = error(record(2, sleeper.pid(), 601));
    assert!(message.contains("1 to 600 seconds"), "{message}");
    let message = error(record(3, u32::MAX, 1));
    assert!(message.contains("no process"), "{message}");
    if !rustix::process::geteuid().is_root() {
        let message = error(record(4, 1, 1));
        assert!(message.contains("another user"), "{message}");
    }
}

#[test]
fn unknown_commands_get_an_answer() {
    let service = Service::start();
    let scratch = allowed(&service, "unknown", true);
    let _agent = Agent::start(&scratch, &[]);
    let connection = service.welcomed();

    connection.send(server_message::Body::Command(ServiceCommand {
        id: 9,
        kind: None,
    }));
    match connection.recv() {
        agent_message::Body::CommandResult(CommandResult {
            id: 9,
            result: Some(command_result::Result::Error(error)),
        }) => assert!(error.message.contains("upgrade"), "{}", error.message),
        other => panic!("expected an error, got {other:?}"),
    }
    let _ = Bytes::new();
}
