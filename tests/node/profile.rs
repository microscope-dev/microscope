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
