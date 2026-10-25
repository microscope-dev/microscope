//! `microscope syscalls -k` on a Node program: each call's stack names the
//! JavaScript frames that made it, in every tier, as V8's own call sites
//! named them just before the call (`tests/fixtures/node/syscalls.js`).

use std::collections::BTreeMap;
use std::process::{Command, Stdio};

use serde_json::Value;

use crate::support::truth::Report;
use crate::support::{ScratchDir, node};

/// The entry the profiler and syscall tests run on: the reader is shared,
/// and these test its use.
const ENTRY: &str = "26";

/// Each path the fixture opens for the tests, and the tier of the frame
/// that opens it.
const OPENS: [(&str, &str); 4] = [
    ("/dev/full", "turbofan"),
    ("/dev/zero", "sparkplug"),
    ("/dev/null", "ignition"),
    ("/dev/urandom", "ignition"),
];

/// A JavaScript frame: its function, file, and line, each `-` or `None`
/// where there is none.
pub type JsFrame = (String, String, Option<u32>);

/// What the fixture reported of one open: the thread that made it, the tier
/// of the frame that made it, and its frames, innermost first.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Call {
    pub thread: u64,
    pub tier: String,
    pub frames: Vec<JsFrame>,
}

/// The opens the fixture reported, by path.
pub fn reported(output: &str) -> BTreeMap<String, Call> {
    let mut calls = BTreeMap::new();
    for fields in Report::parse(output).lines() {
        match fields {
            [kind, path, thread, tier] if kind == "call" => {
                calls.insert(
                    path.clone(),
                    Call {
                        thread: thread.parse().expect("a thread id"),
                        tier: tier.clone(),
                        frames: Vec::new(),
                    },
                );
            }
            [kind, path, _depth, function, file, line] if kind == "frame" => {
                let call = calls.get_mut(path).expect("a frame of a reported call");
                call.frames
                    .push((function.clone(), file.clone(), line.parse().ok()));
            }
            _ => {}
        }
    }
    calls
}

/// The JavaScript frames of an ndjson record's stack, innermost first, past
/// those of Node's own scripts that run the call, with its thread.
pub fn traced(record: &Value) -> Call {
    let frames = record["stack"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or_default()
        .iter()
        .filter(|frame| frame["function"].is_string())
        .map(|frame| {
            let function = frame["function"].as_str().unwrap_or_default();
            (
                if function.is_empty() { "-" } else { function }.to_owned(),
                frame["file"].as_str().unwrap_or("-").to_owned(),
                frame["line"]
                    .as_u64()
                    .and_then(|line| u32::try_from(line).ok()),
            )
        })
        .skip_while(|(_, file, _)| file.starts_with("node:"))
        .collect();
    Call {
        thread: record["tid"].as_u64().unwrap_or_default(),
        tier: String::new(),
        frames,
    }
}

/// How each open microscope traced differs from what the fixture reported
/// of it, one line per difference.
pub fn lies(reported: &BTreeMap<String, Call>, records: &[Value]) -> Vec<String> {
    let mut lies = Vec::new();
    for (path, tier) in OPENS {
        let Some(truth) = reported.get(path) else {
            lies.push(format!("{path}: the fixture reported no such open"));
            continue;
        };
        if truth.tier != tier {
            lies.push(format!(
                "{path}: the fixture's frame ran {} code, not {tier}: it proves nothing",
                truth.tier
            ));
        }
        let Some(record) = records.iter().find(|record| record["path"] == path) else {
            lies.push(format!("{path}: microscope traced no such open"));
            continue;
        };
        let seen = traced(record);
        if seen.thread != truth.thread {
            lies.push(format!(
                "{path}: on thread {}, not {}",
                seen.thread, truth.thread
            ));
        }
        if seen.frames != truth.frames {
            lies.push(format!(
                "{path}: frames\n  microscope {:?}\n  V8         {:?}",
                seen.frames, truth.frames
            ));
        }
    }
    lies
}

/// `microscope syscalls`, run as `command`, tracing the fixture's opens
/// with `arguments` into `trace`.
pub fn command(
    mut command: Command,
    arguments: &[&str],
    trace: &std::path::Path,
    settle: bool,
) -> Command {
    command
        .args(["syscalls", "-f", "-e", "trace=openat", "--format", "ndjson"])
        .args(arguments)
        .arg("-o")
        .arg(trace)
        .arg("--")
        .arg(node::node(ENTRY))
        .arg("--allow-natives-syntax")
        .arg(node::fixture("syscalls.js"))
        .args(settle.then_some("--settle"))
        .env_remove("NODE_OPTIONS")
        .stdin(Stdio::null());
    command
}

/// Runs `command`, which traces into `trace`: what the fixture reported,
/// and the trace's records.
pub fn run(mut command: Command, trace: &std::path::Path) -> (BTreeMap<String, Call>, Vec<Value>) {
    let output = command.output().expect("run microscope");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "microscope failed: {stderr}");
    let text = std::fs::read_to_string(trace).unwrap_or_else(|error| panic!("{error}: {stderr}"));
    let records = text
        .lines()
        .map(|line| serde_json::from_str(line).unwrap_or_else(|error| panic!("{line}: {error}")))
        .collect();
    (reported(&String::from_utf8_lossy(&output.stdout)), records)
}

/// The ptrace engine names each call's JavaScript frames, of every tier,
/// on the main thread and a worker, as V8 named them.
#[test]
fn each_calls_javascript_frames_are_the_ones_v8_reported() {
    let scratch = ScratchDir::new("node-syscalls");
    let trace = scratch.path().join("trace.ndjson");
    let microscope = Command::new(env!("CARGO_BIN_EXE_microscope"));
    let (reported, records) = run(
        command(microscope, &["--engine", "ptrace", "-k"], &trace, false),
        &trace,
    );
    let lies = lies(&reported, &records);
    assert!(lies.is_empty(), "{}", lies.join("\n"));
}

/// The comparison fails on each lie it exists to catch: a dropped frame, a
/// wrong line, a wrong function, and another thread.
#[test]
fn the_comparison_fails_on_each_lie() {
    let frame = |function: &str, line| (function.to_owned(), "/a.js".to_owned(), Some(line));
    let truth = |thread| Call {
        thread,
        tier: String::new(),
        frames: vec![frame("inner", 3), frame("outer", 9)],
    };
    let mut reported = BTreeMap::new();
    for (path, tier) in OPENS {
        reported.insert(
            path.to_owned(),
            Call {
                tier: tier.to_owned(),
                ..truth(7)
            },
        );
    }
    let record = |path: &str, thread: u64, frames: &[(&str, u32)]| {
        let stack: Vec<Value> = frames
            .iter()
            .map(|(function, line)| {
                serde_json::json!({"function": function, "file": "/a.js", "line": line})
            })
            .collect();
        serde_json::json!({"path": path, "tid": thread, "stack": stack})
    };
    let honest: Vec<Value> = OPENS
        .iter()
        .map(|(path, _)| record(path, 7, &[("inner", 3), ("outer", 9)]))
        .collect();
    assert_eq!(lies(&reported, &honest), Vec::<String>::new());

    for lie in [
        record("/dev/full", 7, &[("outer", 9)]),
        record("/dev/full", 7, &[("inner", 4), ("outer", 9)]),
        record("/dev/full", 7, &[("other", 3), ("outer", 9)]),
        record("/dev/full", 8, &[("inner", 3), ("outer", 9)]),
    ] {
        let mut records = honest.clone();
        records[0] = lie.clone();
        assert_eq!(lies(&reported, &records).len(), 1, "{lie}");
    }
}
