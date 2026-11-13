//! A recording of a Node program's system calls: each call's stack names
//! the JavaScript frames that made it, in every tier, as V8's own call
//! sites named them just before the call (`tests/fixtures/node/syscalls.js`).

use std::collections::BTreeMap;
use std::process::{Command, Stdio};

use microscope::profiler::Profile;
use serde_json::Value;

use crate::support::truth::Report;
use crate::support::{ScratchDir, held, node};

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

/// The JavaScript frames of a call's record's stack, innermost first, past
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

/// The fixture as the tests run it; with `hold`, it waits for its
/// standard input (`syscalls.js`).
pub fn program(hold: bool) -> Command {
    let mut program = Command::new(node::node(ENTRY));
    program
        .arg("--allow-natives-syntax")
        .arg(node::fixture("syscalls.js"))
        .args(hold.then_some("--hold"));
    program
}

/// `microscope record`, run as `command`, keeping the fixture's opens by
/// the ptrace engine into `trace`.
pub fn command(mut command: Command, trace: &std::path::Path) -> Command {
    let program = program(false);
    command
        .args(["record", "--syscall-engine", "ptrace"])
        .args(["--syscall-filter", "trace=openat", "-o"])
        .arg(trace)
        .arg("--")
        .arg(program.get_program())
        .args(program.get_args())
        .env_remove("NODE_OPTIONS")
        .stdin(Stdio::null());
    command
}
/// Runs `command`, which records into `trace`: what the fixture reported,
/// and the records of the calls kept.
pub fn run(mut command: Command, trace: &std::path::Path) -> (BTreeMap<String, Call>, Vec<Value>) {
    let output = command.output().expect("run microscope");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "microscope failed: {stderr}");
    let profile = Profile::read(trace)
        .unwrap_or_else(|error| panic!("{error}: {stderr}"))
        .profile;
    (
        reported(&String::from_utf8_lossy(&output.stdout)),
        held::records(&profile),
    )
}
/// The ptrace engine names each call's JavaScript frames, of every tier,
/// on the main thread and a worker, as V8 named them.
#[test]
fn each_calls_javascript_frames_are_the_ones_v8_reported() {
    let scratch = ScratchDir::new("node-syscalls");
    let trace = scratch.path().join("trace.usprof");
    let (reported, records) = run(command(held::unprivileged(), &trace), &trace);
    let lies = lies(&reported, &records);
    assert!(lies.is_empty(), "{}", lies.join("\n"));
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

/// The fixture, recorded by `microscope` with `engine` and `--hold`: once a window's
/// records hold one that satisfies `ready`, the fixture is told to make
/// its calls, and once each of them is kept its standard input ends, which
/// ends it. What the fixture reported, and the records.
pub fn run_held(
    microscope: Command,
    engine: &str,
    ready: impl Fn(&Value) -> bool,
) -> (BTreeMap<String, Call>, Vec<Value>) {
    let mut program = program(true);
    program.env_remove("NODE_OPTIONS");
    let opened = |records: &[Value]| {
        OPENS
            .iter()
            .all(|(path, _)| records.iter().any(|record| record["path"] == *path))
    };
    let (profile, stderr) = held::recorded_until(
        microscope,
        &program,
        &[
            "--syscall-engine",
            engine,
            "--syscall-filter",
            "trace=openat",
        ],
        2,
        |window| {
            let records = held::records(window);
            match (records.iter().any(&ready), opened(&records)) {
                (false, _) => 0,
                (true, false) => 1,
                (true, true) => 2,
            }
        },
    );
    (reported(&stderr), held::records(&profile))
}
/// Whether `record` is of an open the fixture probes with while it waits,
/// with the probing function among its JavaScript frames.
pub fn probed(record: &Value) -> bool {
    record["path"] == "/dev/random"
        && record["stack"]
            .as_array()
            .is_some_and(|stack| stack.iter().any(|frame| frame["function"] == "probe"))
}

/// A call's record of an open of `path` on thread 7 whose stack holds
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
/// function or ends saying it is incomplete: before the first open of
/// /dev/random whose stack names `probe`, the engine may not yet know V8
/// or the program's code, and any reason will do; from there, the reason
/// must be that its JavaScript frames are.
pub fn frameless_lies(records: &[Value]) -> Vec<String> {
    let names = |record: &Value, function: &str| {
        record["stack"]
            .as_array()
            .is_some_and(|stack| stack.iter().any(|frame| frame["function"] == function))
    };
    let ends = |record: &Value| {
        record["stack"]
            .as_array()
            .and_then(|stack| stack.last())
            .and_then(|end| end["ends"].as_str())
            .map(str::to_owned)
    };
    let Some(ready) = records
        .iter()
        .position(|record| record["path"] == "/dev/random" && names(record, "probe"))
    else {
        return vec!["no open of /dev/random names probe".into()];
    };
    let opens = |records: &[Value]| -> Vec<Value> {
        records
            .iter()
            .filter(|record| record["path"] == "/dev/zero" && !names(record, "framelessOpens"))
            .cloned()
            .collect()
    };
    if !records[ready..]
        .iter()
        .any(|record| record["path"] == "/dev/zero")
    {
        return vec!["no open of /dev/zero after the engine named probe".into()];
    }
    let early = opens(&records[..ready])
        .into_iter()
        .filter(|record| ends(record).is_none());
    let late = opens(&records[ready..]).into_iter().filter(|record| {
        !ends(record).is_some_and(|reason| reason.starts_with("interpreted frames"))
    });
    early
        .chain(late)
        .map(|record| format!("an open of /dev/zero hides its missing frames: {record:#}"))
        .collect()
}

/// The frameless check fails on a stack that shows neither the function
/// that made its open nor that its JavaScript frames are incomplete, and
/// on a trace in which the engine named no JavaScript frames.
#[test]
fn the_frameless_check_fails_on_a_silent_stack() {
    let ending = |reason: &str| {
        let mut ended = record("/dev/zero", &[]);
        ended["stack"] = serde_json::json!([{"symbol": "open_path"}, {"ends": reason}]);
        ended
    };
    let probe = record("/dev/random", &[("probe", None, None)]);
    let named = record("/dev/zero", &[("framelessOpens", None, None)]);
    let cut = ending("interpreted frames ended early");
    let unmapped = ending("unmapped return address");
    let silent = record("/dev/zero", &[]);

    let honest = [unmapped.clone(), probe.clone(), named.clone(), cut.clone()];
    assert!(frameless_lies(&honest).is_empty());
    // A silent open, after the engine named probe or before it.
    assert_eq!(
        frameless_lies(&[probe.clone(), cut.clone(), silent.clone()]).len(),
        1
    );
    assert_eq!(
        frameless_lies(&[silent, probe.clone(), cut.clone()]).len(),
        1
    );
    // Once the engine is ready, only missing JavaScript frames excuse it.
    assert_eq!(
        frameless_lies(&[probe.clone(), unmapped, cut.clone()]).len(),
        1
    );
    assert_eq!(frameless_lies(&[cut, named]).len(), 1);
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
        let (_, ptrace) = run_held(held::unprivileged(), "ptrace", |_| true);
        let (reported, records) = run_held(held::bpf(), "bpf", probed);
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
        /// The opens of /dev/zero kept once the engine names JavaScript
        /// frames, past which the fixture is ended.
        const OPENS: usize = 20;

        let mut program = Command::new(node::node(ENTRY));
        program
            .arg(node::fixture("frameless.js"))
            .arg(crate::support::Scenario::fixture("node-frameless.node"))
            .arg("--opens")
            .env_remove("NODE_OPTIONS");
        let enough = |profile: &Profile| {
            let records = held::records(profile);
            let ready = records.iter().position(probed);
            ready.is_some_and(|ready| {
                records[ready..]
                    .iter()
                    .filter(|record| record["path"] == "/dev/zero")
                    .count()
                    >= OPENS
            })
        };
        let (profile, _) = held::recorded_until(
            held::bpf(),
            &program,
            &["--syscall-filter", "trace=openat"],
            1,
            |window| usize::from(enough(window)),
        );
        let lies = frameless_lies(&held::records(&profile));
        assert!(lies.is_empty(), "{}", lies.join("\n"));
    }
}
