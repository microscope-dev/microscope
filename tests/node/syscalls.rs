use std::collections::BTreeMap;
use std::process::{Command, Stdio};

use microscope::profiler::Profile;
use serde_json::Value;

use crate::support::truth::Report;
use crate::support::{ScratchDir, held, node};

const ENTRY: &str = "26";

const OPENS: [(&str, &str); 4] = [
    ("/dev/full", "turbofan"),
    ("/dev/zero", "sparkplug"),
    ("/dev/null", "ignition"),
    ("/dev/urandom", "ignition"),
];

pub type JsFrame = (String, String, Option<u32>);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Call {
    pub thread: u64,
    pub tier: String,
    pub frames: Vec<JsFrame>,
}

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

pub fn program(hold: bool) -> Command {
    let mut program = Command::new(node::node(ENTRY));
    program
        .arg("--allow-natives-syntax")
        .arg(node::fixture("syscalls.js"))
        .args(hold.then_some("--hold"));
    program
}

pub fn command(mut command: Command, trace: &std::path::Path) -> Command {
    let program = program(false);
    command
        .args(["record", "--allow-stopping"])
        .args(["--syscall-filter", "trace=openat", "-o"])
        .arg(trace)
        .arg("--")
        .arg(program.get_program())
        .args(program.get_args())
        .env_remove("NODE_OPTIONS")
        .stdin(Stdio::null());
    command
}
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
#[test]
fn each_calls_javascript_frames_are_the_ones_v8_reported() {
    let scratch = ScratchDir::new("node-syscalls");
    let trace = scratch.path().join("trace.usprof");
    let (reported, records) = run(command(held::unprivileged(), &trace), &trace);
    let lies = lies(&reported, &records);
    assert!(lies.is_empty(), "{}", lies.join("\n"));
}
const COMPILED: [&str; 2] = ["turbofanOpens", "sparkplugOpens"];

const NODE_OPENS: &str = "openSync";

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
            if engine == "ptrace" {
                "--allow-stopping"
            } else {
                "--syscalls=calls"
            },
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
pub fn probed(record: &Value) -> bool {
    record["path"] == "/dev/random"
        && record["stack"]
            .as_array()
            .is_some_and(|stack| stack.iter().any(|frame| frame["function"] == "probe"))
}

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
    assert_eq!(
        frameless_lies(&[probe.clone(), cut.clone(), silent.clone()]).len(),
        1
    );
    assert_eq!(
        frameless_lies(&[silent, probe.clone(), cut.clone()]).len(),
        1
    );
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

    #[test]
    #[ignore = "needs CAP_BPF and CAP_PERFMON: run with `just test-bpf`"]
    fn a_call_the_walk_cannot_follow_says_its_javascript_frames_are_missing() {
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
