//! A fixture traced with `--hold`, so a test waits on what the trace shows
//! rather than on time: the fixture probes until the test tells it to go,
//! makes its calls, and lives until the test has read their records. A BPF
//! engine reads a runtime a moment after the program starts, and reads a
//! frame's code as it drains, which a program that exited has taken away.

use std::io::{BufRead as _, BufReader, Read as _, Write as _};
use std::process::{Command, Stdio};
use std::sync::mpsc::channel;
use std::time::{Duration, Instant};

use serde_json::Value;

/// How long the engine may take to read the runtime, which waits for its
/// debug information to load, and then to trace the calls.
const DEADLINE: Duration = Duration::from_secs(180);

/// Runs `command`, which traces a fixture as ndjson onto standard output.
/// Once a record satisfies `ready`, the fixture is told to go; once the
/// records satisfy `done`, its standard input ends, which ends it. The
/// trace's records, and microscope's standard error.
pub fn trace(
    mut command: Command,
    ready: impl Fn(&Value) -> bool,
    done: impl Fn(&[Value]) -> bool,
) -> (Vec<Value>, String) {
    /// Kills and reaps the traced command if the test fails.
    struct Running(std::process::Child);
    impl Drop for Running {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    let child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("run microscope");
    let mut running = Running(child);
    let mut stdin = running.0.stdin.take();
    let stdout = running.0.stdout.take().expect("microscope's stdout");
    let mut stderr = running.0.stderr.take().expect("microscope's stderr");
    let (lines, read) = channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            if lines.send(line).is_err() {
                break;
            }
        }
    });
    let errors = std::thread::spawn(move || {
        let mut text = String::new();
        let _ = stderr.read_to_string(&mut text);
        text
    });

    // A fixture reports the truth about itself on standard output as it
    // loads, which the trace shares.
    let record = |line: String| {
        (!line.starts_with("TRUTH\t"))
            .then(|| serde_json::from_str(&line).unwrap_or_else(|error| panic!("{line}: {error}")))
    };

    let deadline = Instant::now() + DEADLINE;
    let mut records: Vec<Value> = Vec::new();
    let mut told = false;
    while stdin.is_some() {
        let left = deadline.saturating_duration_since(Instant::now());
        let line = read
            .recv_timeout(left)
            .unwrap_or_else(|error| panic!("the trace ended or stalled ({error}): {records:#?}"))
            .expect("read the trace");
        let Some(record) = record(line) else {
            continue;
        };
        if !told && ready(&record) {
            stdin
                .as_mut()
                .expect("the fixture's stdin")
                .write_all(b"go\n")
                .expect("tell the fixture to go");
            told = true;
        }
        records.push(record);
        if told && done(&records) {
            stdin = None;
        }
    }

    for line in &read {
        records.extend(record(line.expect("read the trace")));
    }
    let status = running.0.wait().expect("reap microscope");
    let stderr = errors.join().expect("read microscope's stderr");
    assert!(status.success(), "microscope failed: {stderr}");
    (records, stderr)
}
