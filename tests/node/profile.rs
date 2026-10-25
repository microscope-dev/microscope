//! Profiles of Node programs: each thread's JavaScript frames, by line,
//! from the engines that read them beside the samples, held to the frames
//! V8's own call sites reported (`tests/fixtures/node/truth.js`).

use std::collections::BTreeMap;
use std::io::Write as _;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::atomic::AtomicBool;
use std::time::{Duration, Instant};

use microscope::profiler::{
    Granularity, Look, Profile, PythonEngine, Query, RecordOptions, Source, Watch, record_watched,
};

use crate::support::truth::Report;
use crate::support::{ScratchDir, node};
use crate::truth::{Thread, threads};

/// The entry the profiler tests run on: the reader is shared, and these
/// test its use.
pub const ENTRY: &str = "26";

/// How long a recording holds its program for a profile to be ready
/// before it releases it anyway, and the test fails on what it lacks.
const READY_DEADLINE: Duration = Duration::from_secs(30);

/// The snapshots a recording must hold, past its deadline, before it is
/// released unready, and how long it may wait for them (see
/// `tests/python/profile.rs`).
const ENOUGH_SNAPSHOTS: u64 = 10;
const SNAPSHOT_DEADLINE: Duration = Duration::from_secs(100);

/// How often each engine reads the program.
const EVERY: Duration = Duration::from_millis(100);

/// Each path of `source`'s call tree in `look` that weight ends at,
/// outermost first, by line, with the weight that ends there.
pub fn paths(profile: &Profile, source: Source, look: Look) -> BTreeMap<Vec<String>, u64> {
    let keys = profile.keys(Granularity::Line);
    let tree = keys.aggregate(&Query {
        source,
        look,
        ..Query::default()
    });

    (0..tree.len())
        .filter(|node| tree.own[*node] > 0)
        .map(|node| {
            let mut path = Vec::new();
            let mut at = u32::try_from(node).expect("a node");
            while let Some(&key) = tree.key.get(at as usize) {
                path.push(keys.name(key));
                at = tree.parent[at as usize];
            }

            path.reverse();
            (path, tree.own[node])
        })
        .collect()
}

/// A frame V8 reported, as a profile names a line: its function, its
/// file's last component, and its line.
fn line_key(function: &str, file: &str, line: Option<u32>) -> String {
    let function = if function == "-" { "" } else { function };
    let file = file.rsplit('/').next().unwrap_or(file);
    format!("{function} {file}:{}", line.unwrap_or(0))
}

/// The frames of each thread V8 reported, outermost first, as the paths of
/// a profile's interpreted look begin.
pub fn expected(reported: &[Thread]) -> Vec<(String, Vec<String>)> {
    reported
        .iter()
        .map(|thread| {
            let frames = thread
                .frames
                .iter()
                .rev()
                .map(|frame| line_key(&frame.function, &frame.file, frame.line))
                .collect();
            (thread.name.clone(), frames)
        })
        .collect()
}

/// Each thread whose frames begin no path of `paths`, with its frames and
/// the paths.
pub fn missing(
    expected: &[(String, Vec<String>)],
    paths: &BTreeMap<Vec<String>, u64>,
) -> Vec<String> {
    expected
        .iter()
        .filter(|(_, frames)| !paths.keys().any(|path| path.starts_with(frames)))
        .map(|(name, frames)| format!("thread {name}: no path begins {frames:#?}"))
        .collect()
}

/// Records `fixture` on the entry with `options`, holding it at its
/// checkpoint until its profile so far is `ready` for what it reported, or
/// the deadline, then releasing it: the profile, and what it reported.
pub fn record_parked(
    fixture: &str,
    checkpoint: &str,
    options: &RecordOptions,
    mut ready: impl FnMut(&Profile, &[Thread]) -> bool,
) -> (Profile, Vec<Thread>) {
    let scratch = ScratchDir::new("node-profile");
    let output = scratch.path().join("output");
    let (reader, mut writer) = std::io::pipe().expect("pipe");
    let mut command = Command::new(node::node(ENTRY));
    command
        .arg(node::fixture(fixture))
        .env_clear()
        .stdin(reader)
        .stdout(std::fs::File::create(&output).expect("the fixture's output"))
        .stderr(Stdio::null());

    let stop = AtomicBool::new(false);
    let mut released = false;
    let started = Instant::now();
    let mut reported = Vec::new();
    let recording = record_watched(
        command,
        options,
        &mut Watch {
            every: Duration::from_millis(50),
            publish: &mut |profile| {
                if reported.is_empty() {
                    reported = parked(&output, checkpoint);
                }
                let waited = started.elapsed();
                let given_up = waited > READY_DEADLINE
                    && (profile.task_snapshots().0 >= ENOUGH_SNAPSHOTS
                        || waited > SNAPSHOT_DEADLINE);
                if !released && ((!reported.is_empty() && ready(&profile, &reported)) || given_up) {
                    writer.write_all(b"\n").expect("release the program");
                    released = true;
                }
            },
            stop: &stop,
            save: None,
        },
    )
    .expect("record");

    assert!(recording.status.success(), "{}", recording.status);
    if reported.is_empty() {
        reported = parked(&output, checkpoint);
    }
    (recording.profile, reported)
}

/// The threads the fixture writing `output` reported at `checkpoint`, once
/// it has.
fn parked(output: &Path, checkpoint: &str) -> Vec<Thread> {
    let text = std::fs::read_to_string(output).unwrap_or_default();
    if !text.contains("\nREADY\n") {
        return Vec::new();
    }
    Report::parse(&text)
        .checkpoint(checkpoint)
        .map(threads)
        .unwrap_or_default()
}

/// Snapshots that stop the program find each thread where it parks, its
/// JavaScript frames by line in the interpreted look, as V8 reported them.
#[test]
fn stopped_snapshots_find_each_threads_javascript_lines() {
    let options = RecordOptions {
        python: Some(PythonEngine::Snapshot { every: EVERY }),
        ..RecordOptions::default()
    };
    let ready = |profile: &Profile, reported: &[Thread]| {
        missing(
            &expected(reported),
            &paths(profile, Source::Waits, Look::Python),
        )
        .is_empty()
    };
    let (profile, reported) = record_parked("stacks.js", "parked", &options, ready);
    assert_eq!(reported.len(), 3, "{reported:#?}");

    let (taken, paused, _) = profile.task_snapshots();
    assert!(taken > 0 && paused > 0, "{taken} {paused}");
    let paths = paths(&profile, Source::Waits, Look::Python);
    let missing = missing(&expected(&reported), &paths);
    assert!(missing.is_empty(), "{}\n{paths:#?}", missing.join("\n"));
}

/// The comparison fails on each lie it exists to catch: a dropped frame, a
/// wrong line, and a wrong function.
#[test]
fn the_path_comparison_fails_on_each_lie() {
    let truth = vec![(
        "main".to_owned(),
        vec!["outer a.js:9".to_owned(), "inner a.js:3".to_owned()],
    )];
    let path = |frames: &[&str]| -> BTreeMap<Vec<String>, u64> {
        let mut native = frames
            .iter()
            .map(|frame| (*frame).to_owned())
            .collect::<Vec<_>>();
        native.push("read".to_owned());
        std::iter::once((native, 1)).collect()
    };
    assert!(missing(&truth, &path(&["outer a.js:9", "inner a.js:3"])).is_empty());
    for lie in [
        path(&["inner a.js:3"]),
        path(&["outer a.js:9", "inner a.js:4"]),
        path(&["outer a.js:9", "other a.js:3"]),
    ] {
        assert_eq!(missing(&truth, &lie).len(), 1, "{lie:?}");
    }
}

/// What a flight recording shows of a write to the program, or of a turn
/// to its inspector: a planted trap, a stored word or register, a signal
/// that opens the inspector, and a CDP command.
#[cfg(debug_assertions)]
const WRITES: [&str; 6] = [
    "install breakpoint",
    "reinstall breakpoint",
    "PTRACE_POKEDATA",
    "PTRACE_SETREGS",
    "SIGUSR1",
    " to Session",
];

/// Each line of a flight recording that shows a write.
#[cfg(debug_assertions)]
fn writes(recording: &str) -> Vec<&str> {
    recording
        .lines()
        .filter(|line| WRITES.iter().any(|write| line.contains(write)))
        .collect()
}

/// Profiling a Node program with stopped snapshots, and tracing its system
/// calls with their stacks, only read it: the flight recordings of both
/// show snapshots taken and calls traced, and no write to the program.
#[cfg(debug_assertions)]
#[test]
fn profiling_and_tracing_write_nothing() {
    let scratch = ScratchDir::new("node-read-only");
    let recording = scratch.path().join("record.log");
    let (reader, mut writer) = std::io::pipe().expect("pipe");
    let mut record = Command::new(env!("CARGO_BIN_EXE_microscope"))
        .env("MICROSCOPE_FLIGHT_RECORDING", &recording)
        .args(["record", "--python", "snapshot=0.1", "-o"])
        .arg(scratch.path().join("profile.json"))
        .arg("--")
        .arg(node::node(ENTRY))
        .arg(node::fixture("stacks.js"))
        .env_remove("NODE_OPTIONS")
        .stdin(reader)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("run microscope record");

    // Three snapshots, each a session that ended.
    let mut text = String::new();
    crate::support::wait_until("three snapshots end", || {
        text = std::fs::read_to_string(&recording).unwrap_or_default();
        text.matches("request shutdown").count() >= 3
    });
    writer.write_all(b"\n").expect("release the program");
    assert!(record.wait().expect("microscope record").success());
    let text = std::fs::read_to_string(&recording).expect("the recording");
    assert!(text.contains("request attach"), "{text}");
    let written = writes(&text);
    assert!(
        written.is_empty(),
        "the snapshots wrote:\n{}",
        written.join("\n")
    );

    let trace = scratch.path().join("trace.ndjson");
    let tracing = scratch.path().join("syscalls.log");
    let mut microscope = Command::new(env!("CARGO_BIN_EXE_microscope"));
    microscope.env("MICROSCOPE_FLIGHT_RECORDING", &tracing);
    let (reported, records) = crate::syscalls::run(
        crate::syscalls::command(microscope, &["--engine", "ptrace", "-k"], &trace, false),
        &trace,
    );
    assert!(!reported.is_empty() && !records.is_empty());
    let text = std::fs::read_to_string(&tracing).expect("the recording");
    let written = writes(&text);
    assert!(
        written.is_empty(),
        "the tracer wrote:\n{}",
        written.join("\n")
    );
}

/// The check fails on each write it exists to catch.
#[cfg(debug_assertions)]
#[test]
fn the_write_check_fails_on_each_write() {
    for write in [
        "   1.0 controller install breakpoint 0x10 in 7 for Loader",
        "   1.0 controller reinstall breakpoint 0x10 in 7",
        "   1.0 controller PTRACE_POKEDATA 7 0x10 0x0000000000000000",
        "   1.0 controller PTRACE_SETREGS 7 rip=0x10",
        "   1.0 controller tgkill 7 7 SIGUSR1",
        "   1.0 controller Runtime.evaluate to Session(1): Ok(1)",
    ] {
        let recording = format!("   0.5 controller request attach 7\n{write}\n");
        assert_eq!(writes(&recording), [write], "{write}");
    }
    assert!(writes("   0.5 controller PTRACE_SEIZE 7 exit_kill=false\n").is_empty());
}
