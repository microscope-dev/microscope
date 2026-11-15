use std::process::{Command, Stdio};

use std::sync::Arc;

use microscope::profiler::Profile;
use microscope::syscalls::{Abi, SyscallId, TraceEvent};
use microscope::{CatchPhase, Condition, DebugFileOptions, FrameKind, LaunchOptions, StopReason};
use serde_json::Value;

use crate::support::{Scenario, ScratchDir, held, python};

fn line(marker: &str) -> u64 {
    let source = std::fs::read_to_string(python::fixture("syscalls.py")).expect("read the fixture");
    source
        .lines()
        .zip(1..)
        .find_map(|(text, number)| text.ends_with(&format!("# {marker}")).then_some(number))
        .unwrap_or_else(|| panic!("no line marked {marker}"))
}

fn opened<'a>(records: &'a [Value], path: &str) -> &'a Value {
    records
        .iter()
        .find(|record| record["path"] == path)
        .unwrap_or_else(|| panic!("no open of {path} in {records:#?}"))
}

fn program(interpreter: &str) -> Command {
    let mut program = Command::new(python::interpreter(interpreter));
    program
        .args(["-I", "-B"])
        .arg(python::fixture("syscalls.py"));
    program
}

fn recorded(interpreter: &str) -> Profile {
    let directory = ScratchDir::new("python-syscalls");
    let saved = directory.path().join("trace.usprof");
    let program = program(interpreter);
    let mut command = held::unprivileged();
    command
        .args(["record", "--syscall-engine", "ptrace"])
        .args(["--syscall-filter", "trace=openat", "-o"])
        .arg(&saved)
        .arg("--")
        .arg(program.get_program())
        .args(program.get_args())
        .stdin(Stdio::null());
    if let Some(debug) = python::debug_directory(interpreter) {
        command.env("NIX_DEBUG_INFO_DIRS", debug);
    }

    let output = command.output().expect("run microscope");
    assert!(
        output.status.success(),
        "{interpreter}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    Profile::read(&saved).expect("the recording reads").profile
}
fn python_frames(record: &Value) -> Vec<(usize, String)> {
    let fixture = python::fixture("syscalls.py");
    record["stack"]
        .as_array()
        .unwrap_or_else(|| panic!("no stack: {record}"))
        .iter()
        .enumerate()
        .filter(|(_, frame)| frame["function"].is_string())
        .map(|(index, frame)| {
            let file = frame["file"].as_str().unwrap_or_default();
            let name = if file == fixture.to_str().expect("a path") {
                format!(
                    "{}:{}",
                    frame["function"].as_str().expect("a name"),
                    frame["line"]
                )
            } else {
                frame["function"].as_str().expect("a name").to_owned()
            };
            (index, name)
        })
        .collect()
}

const TRACED: [&str; 4] = ["3.12", "3.14", "3.15t", "standalone-3.14"];

fn check_frames(interpreter: &str, records: &[Value]) {
    let main = opened(records, "/proc/self/stat");
    let frames = python_frames(main);
    let names: Vec<&str> = frames.iter().map(|(_, name)| name.as_str()).collect();
    assert_eq!(
        names,
        [
            format!("opens:{}", line("OPENS")),
            format!("<module>:{}", line("MAIN")),
        ],
        "{interpreter}: {main:#}"
    );

    let ends = main["stack"]
        .as_array()
        .expect("a stack")
        .last()
        .and_then(|end| end.get("ends"));
    assert_eq!(ends, None, "{interpreter}: {main:#}");

    let worker = opened(records, "/dev/null");
    let frames = python_frames(worker);
    let names: Vec<&str> = frames.iter().map(|(_, name)| name.as_str()).collect();
    assert_eq!(
        names[..3],
        [
            format!("opens:{}", line("OPENS")),
            format!("through_c:{}", line("THROUGH_C")),
            format!("worker:{}", line("WORKER")),
        ],
        "{interpreter}: {worker:#}"
    );

    assert!(
        names[3..].contains(&"Thread.run"),
        "{interpreter}: {worker:#}"
    );

    assert!(
        frames[1].0 > frames[0].0 + 1,
        "{interpreter}: no native frame between the activations: {worker:#}"
    );
}

#[test]
fn a_python_programs_calls_name_the_python_lines_that_made_them() {
    for interpreter in TRACED {
        let profile = recorded(interpreter);
        check_frames(interpreter, &held::records(&profile));
        if interpreter != "3.14" {
            continue;
        }

        let stacks: Vec<Vec<String>> = profile
            .syscalls()
            .events
            .iter()
            .filter_map(|event| match &event.event {
                TraceEvent::Call(_) => event.stack,
                _ => None,
            })
            .filter(|&stack| {
                profile.syscall_site(stack)
                    == Some(format!("opens · syscalls.py:{}", line("OPENS")))
            })
            .map(|stack| profile.syscall_stack(stack).0)
            .collect();

        assert_eq!(stacks.len(), 2, "{stacks:#?}");
        assert!(
            stacks.iter().any(|functions| {
                let at = |name| functions.iter().position(|function| function == name);
                at("opens") < at("_PyEval_EvalFrameDefault") && at("through_c").is_some()
            }),
            "{stacks:#?}"
        );
    }
}
#[tokio::test]
async fn a_catchpoint_stops_with_the_python_frames_that_made_the_call() {
    let interpreter = "3.14t";
    let debug_files = DebugFileOptions {
        directories: python::debug_directory(interpreter).into_iter().collect(),
        ..DebugFileOptions::default()
    };
    let mut scenario = Scenario::with_debug_files(
        "python catchpoint",
        python::interpreter(interpreter),
        &debug_files,
    );
    let calls: Arc<[SyscallId]> =
        Arc::from([SyscallId::named(Abi::X86_64, "openat").expect("a call")]);
    let condition = Condition::parse(r#"$path == "/dev/null""#).expect("a condition");
    scenario
        .operation(
            "catch openat",
            scenario
                .handle()
                .add_catchpoint(calls, CatchPhase::Entry, Some(condition)),
        )
        .await;

    let stop = scenario
        .run_with_to_stop(LaunchOptions {
            arguments: vec![
                "-I".into(),
                "-B".into(),
                python::fixture("syscalls.py").into_os_string(),
            ],
            ..LaunchOptions::default()
        })
        .await;

    assert!(
        matches!(&stop, StopReason::Syscall(caught) if caught.path.as_deref() == Some("/dev/null")),
        "{stop:?}"
    );

    let trace = scenario
        .operation("backtrace", scenario.handle().backtrace())
        .await;

    let python: Vec<(usize, String)> = trace
        .frames
        .iter()
        .enumerate()
        .filter(|(_, frame)| matches!(frame.kind, FrameKind::Interpreted { .. }))
        .filter_map(|(index, frame)| {
            let recorded = frame.recorded.as_ref()?;
            let line = recorded.place.as_ref().map_or(0, |place| place.line);
            Some((index, format!("{}:{line}", recorded.function)))
        })
        .collect();

    let names: Vec<&str> = python.iter().map(|(_, name)| name.as_str()).collect();
    assert_eq!(
        names[..3],
        [
            format!("opens:{}", line("OPENS")),
            format!("through_c:{}", line("THROUGH_C")),
            format!("worker:{}", line("WORKER")),
        ],
        "{trace:#?}"
    );

    assert!(python[1].0 > python[0].0 + 1, "{trace:#?}");
    assert_eq!(trace.frames[0].kind, FrameKind::Physical, "{trace:#?}");
    scenario.shutdown().await;
}

mod bpf {
    use super::*;

    #[test]
    #[ignore = "needs CAP_BPF and CAP_PERFMON: run with `just test-bpf`"]
    fn the_bpf_engine_names_the_python_lines_the_ptrace_engine_does() {
        let probed = |profile: &Profile| {
            held::records(profile).iter().any(|record| {
                record["path"] == "/dev/random"
                    && python_frames(record)
                        .iter()
                        .any(|(_, name)| name.starts_with("probe:"))
            })
        };
        let opened = |profile: &Profile| {
            let records = held::records(profile);
            ["/proc/self/stat", "/dev/null"]
                .iter()
                .all(|path| records.iter().any(|record| record["path"] == *path))
        };

        for interpreter in TRACED {
            let mut program = program(interpreter);
            program.arg("--hold");
            let (profile, _) = held::recorded_until(
                held::bpf(),
                &program,
                &["--syscall-filter", "trace=openat"],
                2,
                |window| match (probed(window), opened(window)) {
                    (false, _) => 0,
                    (true, false) => 1,
                    (true, true) => 2,
                },
            );
            check_frames(interpreter, &held::records(&profile));
        }
    }
}
