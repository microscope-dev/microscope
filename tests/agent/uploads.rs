//! Uploading the outbox: resumable, windowed, and kept until the service
//! has it.

use std::time::Duration;

use prost::bytes::Bytes;
use sha2::{Digest as _, Sha256};

use crate::service::proto::{
    RecordingKind, UploadAck, UploadBegin, UploadDone, UploadFailed, UploadResume, agent_message,
    server_message, upload_failed,
};
use crate::service::{CHUNK, Connection, Service, WINDOW};
use crate::{Agent, Scratch, capture, eventually};

fn logged_in(service: &Service, name: &str) -> Scratch {
    let scratch = Scratch::new(name);
    scratch.log_in(&format!("server = \"{}\"\n", service.url));
    scratch
}

fn begin(connection: &Connection) -> UploadBegin {
    match connection.recv() {
        agent_message::Body::UploadBegin(begin) => begin,
        other => panic!("expected an upload to begin, got {other:?}"),
    }
}

fn resume(connection: &Connection, begin: &UploadBegin, offset: u64) {
    connection.send(server_message::Body::UploadResume(UploadResume {
        upload_key: begin.upload_key.clone(),
        offset,
    }));
}

fn ack(connection: &Connection, begin: &UploadBegin, offset: u64) {
    connection.send(server_message::Body::UploadAck(UploadAck {
        upload_key: begin.upload_key.clone(),
        offset,
    }));
}

fn done(connection: &Connection, begin: &UploadBegin, url: &str) {
    connection.send(server_message::Body::UploadDone(UploadDone {
        upload_key: begin.upload_key.clone(),
        recording_id: "abc".to_owned(),
        url: url.to_owned(),
    }));
}

fn failed(
    connection: &Connection,
    begin: &UploadBegin,
    reason: upload_failed::Reason,
    retryable: bool,
) {
    connection.send(server_message::Body::UploadFailed(UploadFailed {
        upload_key: begin.upload_key.clone(),
        reason: reason.into(),
        message: "no room".to_owned(),
        retryable,
    }));
}

/// Takes the chunks from `offset` to the end, acknowledging each, and
/// returns their bytes.
fn take_chunks(connection: &Connection, begin: &UploadBegin, mut offset: u64) -> Vec<u8> {
    let mut bytes = Vec::new();
    loop {
        match connection.recv() {
            agent_message::Body::UploadChunk(chunk) => {
                assert_eq!(chunk.upload_key, begin.upload_key);
                assert_eq!(chunk.offset, offset, "chunks go in order");
                assert!(!chunk.data.is_empty());
                assert!(chunk.data.len() <= CHUNK as usize);
                offset += chunk.data.len() as u64;
                bytes.extend_from_slice(&chunk.data);
                ack(connection, begin, offset);
            }
            agent_message::Body::UploadEnd(end) => {
                assert_eq!(end.upload_key, begin.upload_key);
                assert_eq!(offset, begin.size);
                return bytes;
            }
            other => panic!("expected a chunk, got {other:?}"),
        }
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().fold(String::new(), |mut text, byte| {
        use std::fmt::Write as _;
        write!(text, "{byte:02x}").unwrap();
        text
    })
}

#[test]
fn uploads_queued_recordings_and_keeps_their_links() {
    let service = Service::start();
    let scratch = logged_in(&service, "upload");
    let bytes = capture(700 << 10);
    let key = scratch.queue(&bytes, "server");
    let mut agent = Agent::start(&scratch, &[]);

    let connection = service.welcomed();
    let begin = begin(&connection);
    assert_eq!(hex(&begin.upload_key), key);
    assert_eq!(begin.kind(), RecordingKind::Usprof);
    assert_eq!(begin.size, bytes.len() as u64);
    assert_eq!(begin.sha256.as_ref(), Sha256::digest(&bytes).as_slice());
    assert_eq!(begin.capture_revision, 1);
    assert_eq!(begin.name, "server");

    resume(&connection, &begin, 0);
    assert_eq!(take_chunks(&connection, &begin, 0), bytes);
    done(&connection, &begin, "http://service/recordings/abc");
    agent.line("uploaded server: http://service/recordings/abc");

    let outbox = scratch.outbox();
    eventually("the recording leaves the outbox", || {
        !outbox.join(format!("{key}.usprof")).exists()
    });
    let url = std::fs::read_to_string(outbox.join(format!("{key}.url"))).unwrap();
    assert_eq!(url.trim(), "http://service/recordings/abc");
    assert!(!outbox.join(format!("{key}.name")).exists());
}

#[test]
fn uploads_go_one_at_a_time_oldest_first() {
    let service = Service::start();
    let scratch = logged_in(&service, "upload-order");
    let first = scratch.queue(&capture(100), "first");
    std::thread::sleep(Duration::from_millis(20));
    let second = scratch.queue(&capture(200), "second");
    let _agent = Agent::start(&scratch, &[]);

    let connection = service.welcomed();
    for key in [first, second] {
        let begin = begin(&connection);
        assert_eq!(hex(&begin.upload_key), key);
        resume(&connection, &begin, 0);
        take_chunks(&connection, &begin, 0);
        done(&connection, &begin, "http://service/r");
    }
}

#[test]
fn recordings_queued_while_connected_are_uploaded() {
    let service = Service::start();
    let scratch = logged_in(&service, "upload-later");
    let mut agent = Agent::start(&scratch, &[]);
    let connection = service.welcomed();
    agent.line("connected to");
    connection.nothing_within(Duration::from_millis(200));

    let key = scratch.queue(&capture(100), "later");
    let begin = begin(&connection);
    assert_eq!(hex(&begin.upload_key), key);
}

#[test]
fn chunks_stay_within_the_window() {
    let service = Service::start();
    let scratch = logged_in(&service, "window");
    let bytes = capture(usize::try_from(WINDOW).unwrap() * 3);
    scratch.queue(&bytes, "big");
    let _agent = Agent::start(&scratch, &[]);

    let connection = service.welcomed();
    let begin = begin(&connection);
    resume(&connection, &begin, 0);

    // Unacknowledged, the agent sends one window and waits.
    let mut sent = 0;
    while sent < WINDOW {
        match connection.recv() {
            agent_message::Body::UploadChunk(chunk) => sent += chunk.data.len() as u64,
            other => panic!("expected a chunk, got {other:?}"),
        }
    }
    assert_eq!(sent, WINDOW);
    connection.nothing_within(Duration::from_millis(300));

    // Each acknowledgement opens the window again.
    ack(&connection, &begin, u64::from(CHUNK));
    match connection.recv() {
        agent_message::Body::UploadChunk(chunk) => assert_eq!(chunk.offset, WINDOW),
        other => panic!("expected a chunk, got {other:?}"),
    }
    connection.nothing_within(Duration::from_millis(300));
}

#[test]
fn uploads_resume_where_the_service_says() {
    let service = Service::start();
    let scratch = logged_in(&service, "resume");
    let bytes = capture(300 << 10);
    scratch.queue(&bytes, "resumed");
    let _agent = Agent::start(&scratch, &[]);

    let connection = service.welcomed();
    let begin = begin(&connection);
    let offset = 100_000;
    resume(&connection, &begin, offset);
    let rest = take_chunks(&connection, &begin, offset);
    assert_eq!(rest, bytes[usize::try_from(offset).unwrap()..]);
}

#[test]
fn interrupted_uploads_begin_again_under_the_same_key() {
    let service = Service::start();
    let scratch = logged_in(&service, "interrupted");
    let bytes = capture(300 << 10);
    let key = scratch.queue(&bytes, "interrupted");
    let _agent = Agent::start(&scratch, &[]);

    let connection = service.welcomed();
    let first = begin(&connection);
    resume(&connection, &first, 0);
    match connection.recv() {
        agent_message::Body::UploadChunk(_) => {}
        other => panic!("expected a chunk, got {other:?}"),
    }
    drop(connection);

    let connection = service.welcomed();
    let again = begin(&connection);
    assert_eq!(hex(&again.upload_key), key);
    assert_eq!(again.sha256, first.sha256);
    resume(&connection, &again, u64::from(CHUNK));
    let rest = take_chunks(&connection, &again, u64::from(CHUNK));
    assert_eq!(rest, bytes[CHUNK as usize..]);
}

#[test]
fn recordings_the_service_already_has_finish_at_once() {
    let service = Service::start();
    let scratch = logged_in(&service, "already");
    let key = scratch.queue(&capture(100), "already");
    let mut agent = Agent::start(&scratch, &[]);

    let connection = service.welcomed();
    let begin = begin(&connection);
    done(&connection, &begin, "http://service/recordings/old");
    agent.line("uploaded already: http://service/recordings/old");
    eventually("the recording leaves the outbox", || {
        !scratch.outbox().join(format!("{key}.usprof")).exists()
    });
}

#[test]
fn recordings_refused_for_good_are_kept_aside() {
    let service = Service::start();
    let scratch = logged_in(&service, "refused");
    let key = scratch.queue(&capture(100), "refused");
    let next = {
        std::thread::sleep(Duration::from_millis(20));
        scratch.queue(&capture(100), "next")
    };
    let mut agent = Agent::start(&scratch, &[]);

    let connection = service.welcomed();
    let first = begin(&connection);
    assert_eq!(hex(&first.upload_key), key);
    failed(&connection, &first, upload_failed::Reason::Quota, false);
    agent.line("the service refused refused: no room");

    let outbox = scratch.outbox();
    eventually("the recording is set aside", || {
        outbox.join(format!("{key}.usprof.failed")).exists()
    });
    let error = std::fs::read_to_string(outbox.join(format!("{key}.error"))).unwrap();
    assert_eq!(error.trim(), "no room");

    // The outbox goes on.
    let second = begin(&connection);
    assert_eq!(hex(&second.upload_key), next);
}

#[test]
fn recordings_the_service_cannot_take_now_wait() {
    let service = Service::start();
    let scratch = logged_in(&service, "later");
    let key = scratch.queue(&capture(100), "later");
    let mut agent = Agent::start(&scratch, &[]);

    let connection = service.welcomed();
    let first = begin(&connection);
    failed(&connection, &first, upload_failed::Reason::Internal, true);
    agent.line("could not take later now: no room; trying again in a minute");

    // It stays queued, and is not tried again at once.
    connection.nothing_within(Duration::from_secs(3));
    assert!(scratch.outbox().join(format!("{key}.usprof")).exists());
}

#[test]
fn files_that_are_not_recordings_are_never_sent() {
    let service = Service::start();
    let scratch = logged_in(&service, "not-capture");
    let key = scratch.queue(b"just some text", "notes");
    let mut agent = Agent::start(&scratch, &[]);

    let connection = service.welcomed();
    agent.line("not uploading notes: it is not a microscope recording");
    connection.nothing_within(Duration::from_millis(300));
    assert!(
        scratch
            .outbox()
            .join(format!("{key}.usprof.failed"))
            .exists()
    );
}

#[test]
fn watched_segments_are_uploaded_once_and_kept() {
    let service = Service::start();
    let scratch = logged_in(&service, "watch");
    let segments = scratch.path().join("server-segments");
    std::fs::create_dir_all(&segments).unwrap();
    let bytes = capture(1000);
    std::fs::write(segments.join("0000000000000001.usprof"), &bytes).unwrap();
    std::fs::write(segments.join(".0000000000000002.usprof.partial"), b"half").unwrap();
    let watch = segments.to_str().unwrap();

    let agent = Agent::start(&scratch, &["--watch", watch]);
    let connection = service.welcomed();
    let first = begin(&connection);
    assert_eq!(first.name, "server-segments");
    assert_eq!(first.sha256.as_ref(), Sha256::digest(&bytes).as_slice());
    resume(&connection, &first, 0);
    assert_eq!(take_chunks(&connection, &first, 0), bytes);
    done(&connection, &first, "http://service/r/1");
    assert!(segments.join("0000000000000001.usprof").exists());
    connection.nothing_within(Duration::from_secs(3));
    drop(agent);
    drop(connection);

    // After a restart, only the new segment goes.
    std::fs::write(segments.join("0000000000000002.usprof"), capture(500)).unwrap();
    let _agent = Agent::start(&scratch, &["--watch", watch]);
    let connection = service.welcomed();
    let second = begin(&connection);
    resume(&connection, &second, 0);
    take_chunks(&connection, &second, 0);
    done(&connection, &second, "http://service/r/2");
    connection.nothing_within(Duration::from_secs(3));
}

#[test]
fn record_upload_prints_the_link() {
    let service = Service::start();
    let scratch = logged_in(&service, "record-upload");
    let _agent = Agent::start(&scratch, &[]);
    let connection = service.welcomed();

    let output = scratch.path().join("true.usprof");
    let recording = scratch
        .microscope()
        .args(["record", "--upload", "-o"])
        .arg(&output)
        .args(["--", "true"])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();

    let begin = begin(&connection);
    assert_eq!(begin.name, "true");
    resume(&connection, &begin, 0);
    let bytes = take_chunks(&connection, &begin, 0);
    assert_eq!(bytes, std::fs::read(&output).unwrap());
    done(&connection, &begin, "http://service/recordings/new");

    let recorded = recording.wait_with_output().unwrap();
    assert!(recorded.status.success(), "{recorded:?}");
    assert_eq!(
        String::from_utf8_lossy(&recorded.stdout),
        "http://service/recordings/new\n"
    );
    let stderr = String::from_utf8_lossy(&recorded.stderr);
    assert!(stderr.contains("stays private to your account"), "{stderr}");
}

#[test]
fn record_upload_reports_a_refusal() {
    let service = Service::start();
    let scratch = logged_in(&service, "record-refused");
    let _agent = Agent::start(&scratch, &[]);
    let connection = service.welcomed();

    let recording = scratch
        .microscope()
        .args(["record", "--upload", "-o"])
        .arg(scratch.path().join("true.usprof"))
        .args(["--", "true"])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();

    let begin = begin(&connection);
    failed(&connection, &begin, upload_failed::Reason::TooLarge, false);
    let recorded = recording.wait_with_output().unwrap();
    assert!(!recorded.status.success());
    assert!(recorded.stdout.is_empty());
    assert!(
        String::from_utf8_lossy(&recorded.stderr)
            .contains("the service refused the recording: no room")
    );
}

#[test]
fn upload_keys_are_sent_as_bytes() {
    // The key in a file's name is the key on the wire.
    let service = Service::start();
    let scratch = logged_in(&service, "key-bytes");
    let key = scratch.queue(&capture(10), "k");
    let _agent = Agent::start(&scratch, &[]);
    let connection = service.welcomed();
    let begin = begin(&connection);
    assert_eq!(begin.upload_key.len(), 16);
    assert_eq!(
        begin.upload_key,
        Bytes::from(
            (0..16)
                .map(|i| u8::from_str_radix(&key[i * 2..i * 2 + 2], 16).unwrap())
                .collect::<Vec<_>>(),
        )
    );
}
