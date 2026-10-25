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
/// with `arguments` into `trace`, `-` for standard output; with `hold`,
/// the fixture waits for its standard input (`syscalls.js`).
pub fn command(
    mut command: Command,
    arguments: &[&str],
    trace: &std::path::Path,
    hold: bool,
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
        .args(hold.then_some("--hold"))
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

/// The functions whose frames the fixture runs compiled where they open
/// their paths, whose lines the BPF engine leaves unknown: it does not
/// prove which compiled code a frame ran (`runtime_model/node/sampled.rs`).
const COMPILED: [&str; 2] = ["turbofanOpens", "sparkplugOpens"];

/// The function of `node:fs` each of the fixture's opens runs.
const NODE_OPENS: &str = "openSync";

/// How each open the BPF engine traced, in `records`, differs from what
/// the fixture reported of it and from the frames the ptrace engine traced
/// of it, in `ptrace`, one line per difference. A frame of the fixture's
/// own script has V8's function, file and line, or, where it ran compiled
/// code, V8's function and no line; a frame of Node's own scripts has
/// V8's function and V8's line or none, as Node's scripts may run compiled
/// code where the fixture says nothing of it.
pub fn bpf_lies(
    reported: &BTreeMap<String, Call>,
    records: &[Value],
    ptrace: &[Value],
) -> Vec<String> {
    let mut lies = Vec::new();
    for (path, _) in OPENS {
        let Some(truth) = reported.get(path) else {
            lies.push(format!("{path}: the fixture reported no such open"));
            continue;
        };
        // Node opens some of the paths as it starts; the fixture's open is
        // the last.
        let find = |records: &[Value]| {
            records
                .iter()
                .rev()
                .find(|record| record["path"] == path)
                .map(traced)
        };
        let (Some(mut seen), Some(other)) = (find(records), find(ptrace)) else {
            lies.push(format!("{path}: an engine traced no such open"));
            continue;
        };
        // node:fs's frame that opens the path, which the fixture's many
        // opens compile, has no file where its line is unknown.
        let opens = seen
            .frames
            .iter()
            .take_while(|frame| **frame == (NODE_OPENS.to_owned(), "-".to_owned(), None))
            .count();
        seen.frames.drain(..opens);
        if seen.thread != truth.thread {
            lies.push(format!(
                "{path}: on thread {}, not {}",
                seen.thread, truth.thread
            ));
        }
        let names = |call: &Call| -> Vec<String> {
            call.frames
                .iter()
                .map(|(function, _, _)| function.clone())
                .collect()
        };
        if names(&seen) != names(&other) {
            lies.push(format!(
                "{path}: functions\n  bpf    {:?}\n  ptrace {:?}",
                names(&seen),
                names(&other)
            ));
        }
        let honest = seen.frames.len() == truth.frames.len()
            && seen.frames.iter().zip(&truth.frames).all(|(seen, truth)| {
                let unknown = (truth.0.clone(), "-".to_owned(), None);
                if COMPILED.contains(&truth.0.as_str()) {
                    *seen == unknown
                } else if truth.1.starts_with("node:") {
                    *seen == *truth || *seen == unknown
                } else {
                    *seen == *truth
                }
            });
        if !honest {
            lies.push(format!(
                "{path}: frames\n  microscope {:?}\n  V8         {:?}",
                seen.frames, truth.frames
            ));
        }
    }
    lies
}

/// The fixture, traced by `command` with `--hold` onto standard output:
/// once a record satisfies `ready`, the fixture is told to make its calls,
/// and once each of them is traced its standard input ends, which ends it.
/// What the fixture reported, and the trace's records.
pub fn run_held(
    mut command: Command,
    ready: impl Fn(&Value) -> bool,
) -> (BTreeMap<String, Call>, Vec<Value>) {
    use std::io::{BufRead, Read, Write};

    /// How long the engine may take to name JavaScript frames, which waits
    /// for Node's debug information to load, and then to trace the calls.
    const DEADLINE: std::time::Duration = std::time::Duration::from_secs(180);

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
    let (lines, read) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        for line in std::io::BufReader::new(stdout).lines() {
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

    let deadline = std::time::Instant::now() + DEADLINE;
    let mut records: Vec<Value> = Vec::new();
    let mut told = false;
    while stdin.is_some() {
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        let line = read
            .recv_timeout(left)
            .unwrap_or_else(|error| panic!("the trace ended or stalled ({error}): {records:#?}"))
            .expect("read the trace");
        // truth.js reports the process on standard output as it loads.
        if line.starts_with("TRUTH\t") {
            continue;
        }
        let record: Value =
            serde_json::from_str(&line).unwrap_or_else(|error| panic!("{line}: {error}"));
        if !told && ready(&record) {
            stdin
                .as_mut()
                .expect("the fixture's stdin")
                .write_all(b"go\n")
                .expect("tell the fixture to go");
            told = true;
        }
        records.push(record);
        if told
            && OPENS
                .iter()
                .all(|(path, _)| records.iter().any(|record| record["path"] == *path))
        {
            stdin = None;
        }
    }

    for line in &read {
        let line = line.expect("read the trace");
        if line.starts_with("TRUTH\t") {
            continue;
        }
        records.push(serde_json::from_str(&line).unwrap_or_else(|error| panic!("{line}: {error}")));
    }
    let status = running.0.wait().expect("reap microscope");
    let stderr = errors.join().expect("read microscope's stderr");
    assert!(status.success(), "microscope failed: {stderr}");
    (reported(&stderr), records)
}

/// Whether `record` is of an open the fixture probes with while it waits,
/// with the probing function among its JavaScript frames.
pub fn probed(record: &Value) -> bool {
    record["path"] == "/dev/random"
        && record["stack"]
            .as_array()
            .is_some_and(|stack| stack.iter().any(|frame| frame["function"] == "probe"))
}

/// An ndjson record of an open of `path` on thread 7 whose stack holds
/// `frames`, each a function and, where known, its file and line.
fn record(path: &str, frames: &[(&str, Option<&str>, Option<u32>)]) -> Value {
    let stack: Vec<Value> = frames
        .iter()
        .map(|(function, file, line)| match (file, line) {
            (Some(file), Some(line)) => {
                serde_json::json!({"function": function, "file": file, "line": line})
            }
            _ => serde_json::json!({"function": function}),
        })
        .collect();
    serde_json::json!({"path": path, "tid": 7, "stack": stack})
}

/// How the opens of `frameless.js --opens` traced in `records`, in the
/// order they were made, hide that JavaScript frames are missing, one line
/// per open. Every open of /dev/zero is made by `framelessOpens`, through
/// an addon that holds no frame pointer, so its stack either names that
/// function or ends saying its JavaScript frames are incomplete. Opens
/// before the first of /dev/random whose stack names `probe` came before
/// the engine read V8, and only from there must its stacks name them.
pub fn frameless_lies(records: &[Value]) -> Vec<String> {
    let names = |record: &Value, function: &str| {
        record["stack"]
            .as_array()
            .is_some_and(|stack| stack.iter().any(|frame| frame["function"] == function))
    };
    let Some(ready) = records
        .iter()
        .position(|record| record["path"] == "/dev/random" && names(record, "probe"))
    else {
        return vec!["no open of /dev/random names probe".into()];
    };
    let opens: Vec<&Value> = records[ready..]
        .iter()
        .filter(|record| record["path"] == "/dev/zero")
        .collect();
    if opens.is_empty() {
        return vec!["no open of /dev/zero after the engine named probe".into()];
    }
    opens
        .iter()
        .filter(|record| {
            let ends = record["stack"]
                .as_array()
                .and_then(|stack| stack.last())
                .and_then(|end| end["ends"].as_str())
                .is_some_and(|reason| reason.starts_with("interpreted frames"));
            !names(record, "framelessOpens") && !ends
        })
        .map(|record| format!("an open of /dev/zero hides its missing frames: {record:#}"))
        .collect()
}

/// The frameless check fails on a stack that shows neither the function
/// that made its open nor that its JavaScript frames are incomplete, and
/// on a trace in which the engine named no JavaScript frames.
#[test]
fn the_frameless_check_fails_on_a_silent_stack() {
    let probe = record("/dev/random", &[("probe", None, None)]);
    let named = record("/dev/zero", &[("framelessOpens", None, None)]);
    let mut cut = record("/dev/zero", &[]);
    cut["stack"] =
        serde_json::json!([{"symbol": "open_path"}, {"ends": "interpreted frames ended early"}]);
    let silent = record("/dev/zero", &[]);
    let early = record("/dev/zero", &[]);

    assert!(frameless_lies(&[early.clone(), probe.clone(), named.clone(), cut.clone()]).is_empty());
    assert_eq!(
        frameless_lies(&[probe.clone(), cut.clone(), silent]).len(),
        1
    );
    assert_eq!(frameless_lies(&[early, cut.clone(), named]).len(), 1);
    assert_eq!(
        frameless_lies(&[probe, record("/dev/random", &[])]).len(),
        1
    );
}

/// The BPF comparison fails on each lie it exists to catch: a compiled
/// frame's line where it has none proven, a lost line of the fixture's
/// interpreted frame, a wrong function, a dropped frame, a wrong line of
/// Node's own, and functions the ptrace engine does not find.
#[test]
fn the_bpf_comparison_fails_on_each_lie() {
    let truth = |frames: &[(&str, &str, Option<u32>)]| -> Vec<JsFrame> {
        frames
            .iter()
            .map(|(function, file, line)| ((*function).to_owned(), (*file).to_owned(), *line))
            .collect()
    };
    let fixture = [
        ("turbofanOpens", "/a.js", Some(3)),
        ("ignitionOpens", "/a.js", Some(9)),
        ("emit", "node:events", Some(20)),
    ];
    let mut reported = BTreeMap::new();
    for (path, tier) in OPENS {
        reported.insert(
            path.to_owned(),
            Call {
                thread: 7,
                tier: tier.to_owned(),
                frames: truth(&fixture),
            },
        );
    }
    let honest_frames = [
        ("turbofanOpens", None, None),
        ("ignitionOpens", Some("/a.js"), Some(9)),
        ("emit", None, None),
    ];
    let honest: Vec<Value> = OPENS
        .iter()
        .map(|(path, _)| record(path, &honest_frames))
        .collect();
    let ptrace: Vec<Value> = OPENS
        .iter()
        .map(|(path, _)| {
            record(
                path,
                &fixture.map(|(function, file, line)| (function, Some(file), line)),
            )
        })
        .collect();
    assert_eq!(bpf_lies(&reported, &honest, &ptrace), Vec::<String>::new());

    for lie in [
        record(
            "/dev/full",
            &[
                ("turbofanOpens", Some("/a.js"), Some(3)),
                ("ignitionOpens", Some("/a.js"), Some(9)),
                ("emit", None, None),
            ],
        ),
        record(
            "/dev/full",
            &[
                ("turbofanOpens", None, None),
                ("ignitionOpens", None, None),
                ("emit", None, None),
            ],
        ),
        record(
            "/dev/full",
            &[
                ("turbofanOpens", None, None),
                ("other", Some("/a.js"), Some(9)),
                ("emit", None, None),
            ],
        ),
        record(
            "/dev/full",
            &[("turbofanOpens", None, None), ("emit", None, None)],
        ),
        record(
            "/dev/full",
            &[
                ("turbofanOpens", None, None),
                ("ignitionOpens", Some("/a.js"), Some(9)),
                ("emit", Some("node:events"), Some(21)),
            ],
        ),
    ] {
        let mut records = honest.clone();
        records[0] = lie.clone();
        assert!(!bpf_lies(&reported, &records, &ptrace).is_empty(), "{lie}");
    }

    let mut other = ptrace;
    other[0] = record(
        "/dev/full",
        &[
            ("inner", None, None),
            ("ignitionOpens", Some("/a.js"), Some(9)),
            ("emit", None, None),
        ],
    );
    assert!(!bpf_lies(&reported, &honest, &other).is_empty());
}

/// The BPF engine, whose program walks each call's JavaScript frames as it
/// returns, held to the frames V8 reported and to the functions the ptrace
/// engine names. It needs `CAP_BPF` and `CAP_PERFMON`, so it runs only
/// through `just test-bpf`.
mod bpf {
    use super::*;

    #[test]
    #[ignore = "needs CAP_BPF and CAP_PERFMON: run with `just test-bpf`"]
    fn the_bpf_engine_names_the_javascript_frames_v8_and_the_ptrace_engine_do() {
        let launcher = std::env::var_os("MICROSCOPE_BPF_LAUNCHER")
            .expect("MICROSCOPE_BPF_LAUNCHER names the microscope-bpf wrapper");
        let stdout = std::path::Path::new("-");

        let (_, ptrace) = run_held(
            command(
                Command::new(env!("CARGO_BIN_EXE_microscope")),
                &["--engine", "ptrace", "-k"],
                stdout,
                true,
            ),
            |_| true,
        );

        let mut microscope = Command::new(&launcher);
        microscope.arg(env!("CARGO_BIN_EXE_microscope"));
        // The engine unwinds native frames by a copy of the stack, as
        // glibc's system call wrappers keep no frame pointer.
        let (reported, records) = run_held(
            command(
                microscope,
                &["--engine", "bpf", "-k=dwarf", "-k"],
                stdout,
                true,
            ),
            probed,
        );
        let lies = bpf_lies(&reported, &records, &ptrace);
        assert!(lies.is_empty(), "{}", lies.join("\n"));
    }

    /// A call made where rbp holds no frame pointer, under JavaScript
    /// frames, leaves the walk nothing to follow; the stack, unwound
    /// through V8's code by call-frame information, says its JavaScript
    /// frames ended early, and never stands complete without them.
    #[test]
    #[ignore = "needs CAP_BPF and CAP_PERFMON: run with `just test-bpf`"]
    fn a_call_the_walk_cannot_follow_says_its_javascript_frames_are_missing() {
        use std::io::BufRead as _;

        /// How long the engine may take to read V8, which waits for
        /// Node's debug information to load, and then to trace the opens.
        const DEADLINE: std::time::Duration = std::time::Duration::from_secs(180);

        /// The opens of /dev/zero traced once the engine names JavaScript
        /// frames, past which the fixture is ended.
        const OPENS: usize = 20;

        /// Kills and reaps the traced command if the test fails.
        struct Running(std::process::Child);
        impl Drop for Running {
            fn drop(&mut self) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }

        let launcher = std::env::var_os("MICROSCOPE_BPF_LAUNCHER")
            .expect("MICROSCOPE_BPF_LAUNCHER names the microscope-bpf wrapper");
        let child = Command::new(launcher)
            .arg(env!("CARGO_BIN_EXE_microscope"))
            .args(["syscalls", "-f", "-e", "trace=openat", "--format", "ndjson"])
            .args(["--engine", "bpf", "-k=dwarf", "-k", "-o", "-", "--"])
            .arg(node::node(ENTRY))
            .arg(node::fixture("frameless.js"))
            .arg(crate::support::Scenario::fixture("node-frameless.node"))
            .arg("--opens")
            .env_remove("NODE_OPTIONS")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("run microscope");
        let mut running = Running(child);
        let mut stdin = running.0.stdin.take();
        let stdout = running.0.stdout.take().expect("microscope's stdout");
        let (sent, read) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            for line in std::io::BufReader::new(stdout).lines() {
                if sent.send(line).is_err() {
                    break;
                }
            }
        });

        let deadline = std::time::Instant::now() + DEADLINE;
        let mut records: Vec<Value> = Vec::new();
        let mut ready = false;
        let mut after = 0;
        for line in std::iter::from_fn(|| {
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            read.recv_timeout(left).ok()
        }) {
            let line = line.expect("read the trace");
            let record: Value =
                serde_json::from_str(&line).unwrap_or_else(|error| panic!("{line}: {error}"));
            ready |= record["path"] == "/dev/random"
                && record["stack"]
                    .as_array()
                    .is_some_and(|stack| stack.iter().any(|frame| frame["function"] == "probe"));
            after += usize::from(ready && record["path"] == "/dev/zero");
            records.push(record);
            if after >= OPENS {
                stdin = None;
            }
        }
        let status = running.0.wait().expect("reap microscope");
        let mut stderr = String::new();
        std::io::Read::read_to_string(
            running.0.stderr.as_mut().expect("microscope's stderr"),
            &mut stderr,
        )
        .expect("read microscope's stderr");
        assert!(
            stdin.is_none() && status.success(),
            "the trace stalled or failed ({status}): {stderr}"
        );
        let lies = frameless_lies(&records);
        assert!(lies.is_empty(), "{}", lies.join("\n"));
    }
}
