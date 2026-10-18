//! `microscope syscalls -k` on a Python program: each call's stack names
//! the Python lines that made it, each activation of the interpreter's
//! loop after the frames it runs; and a catchpoint's stop, whose backtrace
//! shows them as any stop's does.

use std::process::{Command, Output, Stdio};

use std::sync::Arc;

use microscope::profiler::Profile;
use microscope::syscalls::{Abi, SyscallId, TraceEvent};
use microscope::{CatchPhase, Condition, DebugFileOptions, FrameKind, LaunchOptions, StopReason};
use serde_json::Value;

use crate::support::{Scenario, ScratchDir, python};

/// The line of the syscalls fixture marked `marker`.
fn line(marker: &str) -> u64 {
    let source = std::fs::read_to_string(python::fixture("syscalls.py")).expect("read the fixture");
    source
        .lines()
        .zip(1..)
        .find_map(|(text, number)| text.ends_with(&format!("# {marker}")).then_some(number))
        .unwrap_or_else(|| panic!("no line marked {marker}"))
}

/// Traces the fixture's opens under `interpreter` with `arguments`.
fn trace(interpreter: &str, arguments: &[&str]) -> Output {
    command(interpreter, arguments)
        .output()
        .expect("run microscope")
}

/// The command that traces the fixture's opens.
fn command(interpreter: &str, arguments: &[&str]) -> Command {
    engine_command(
        Command::new(env!("CARGO_BIN_EXE_microscope")),
        "ptrace",
        interpreter,
        arguments,
    )
}

/// `microscope`, run as `command`, tracing the fixture's opens by
/// `engine`.
fn engine_command(
    mut command: Command,
    engine: &str,
    interpreter: &str,
    arguments: &[&str],
) -> Command {
    command
        .args(["syscalls", "--engine", engine, "-f", "-e", "trace=openat"])
        .args(arguments)
        .arg("--")
        .arg(python::interpreter(interpreter))
        .args(["-I", "-B"])
        .arg(python::fixture("syscalls.py"))
        .stdin(Stdio::null());
    command
}

/// The ndjson record of the open of `path`.
fn opened<'a>(records: &'a [Value], path: &str) -> &'a Value {
    records
        .iter()
        .find(|record| record["path"] == path)
        .unwrap_or_else(|| panic!("no open of {path} in {records:#?}"))
}

/// The Python frames of a record's stack, innermost first, as
/// `function:line`, and the index in the stack of each.
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

/// The interpreters traced: before 3.13's debug offsets, a GIL build, and
/// a free-threaded one.
const TRACED: [&str; 3] = ["3.12", "3.14", "3.15t"];

/// The ndjson records of a trace.
fn records(interpreter: &str, traced: &Output) -> Vec<Value> {
    let stderr = String::from_utf8_lossy(&traced.stderr);
    assert!(traced.status.success(), "{interpreter}: {stderr}");
    String::from_utf8_lossy(&traced.stdout)
        .lines()
        .map(|line| serde_json::from_str(line).unwrap_or_else(|error| panic!("{line}: {error}")))
        .collect()
}

/// That the main thread's open shows `opens` called from the module, and
/// the worker's the frames of both its activations of the loop.
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

    // The worker's opens runs in a second activation of the loop,
    // which Context.run, C, began: a native frame comes between it and
    // through_c, and the frames threading runs the worker in follow.
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
        let traced = trace(interpreter, &["-k", "--format", "ndjson", "-o", "-"]);
        check_frames(interpreter, &records(interpreter, &traced));
    }

    // The text names each Python frame by its file and line.
    let traced = trace("3.14", &["-k"]);
    let stderr = String::from_utf8_lossy(&traced.stderr);
    assert!(traced.status.success(), "{stderr}");
    let wanted = format!(
        " > opens at {}:{}",
        python::fixture("syscalls.py").display(),
        line("OPENS")
    );
    assert!(stderr.lines().any(|text| text == wanted), "{stderr}");

    // A saved trace keeps them, and a call's site is its Python line,
    // not the interpreter's, whose debug information names its C lines.
    let directory = ScratchDir::new("python-syscalls");
    let saved = directory.path().join("trace.usprof");
    let mut traced = command("3.14", &["-k", "-o", saved.to_str().expect("a path")]);
    if let Some(debug) = python::debug_directory("3.14") {
        traced.env("NIX_DEBUG_INFO_DIRS", debug);
    }
    let traced = traced.output().expect("run microscope");
    assert!(
        traced.status.success(),
        "{}",
        String::from_utf8_lossy(&traced.stderr)
    );
    let profile = Profile::read(&saved).expect("the trace reads").profile;
    let syscalls = profile.syscalls();
    let stacks: Vec<Vec<String>> = syscalls
        .events
        .iter()
        .filter_map(|event| match &event.event {
            TraceEvent::Call(_) => event.stack,
            _ => None,
        })
        .filter(|&stack| {
            profile.syscall_site(stack) == Some(format!("opens · syscalls.py:{}", line("OPENS")))
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

    // The worker's frames, each segment before its activation of the loop.
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

/// The BPF engine, whose program reads each call's Python frames as it
/// returns, held to the frames the ptrace engine finds. It needs
/// `CAP_BPF` and `CAP_PERFMON`, so it runs only through `just test-bpf`.
mod bpf {
    use super::*;

    #[test]
    #[ignore = "needs CAP_BPF and CAP_PERFMON: run with `just test-bpf`"]
    fn the_bpf_engine_names_the_python_lines_the_ptrace_engine_does() {
        let launcher = std::env::var_os("MICROSCOPE_BPF_LAUNCHER")
            .expect("MICROSCOPE_BPF_LAUNCHER names the microscope-bpf wrapper");
        for interpreter in TRACED {
            let mut command = Command::new(&launcher);
            command.arg(env!("CARGO_BIN_EXE_microscope"));
            // The engine unwinds native frames by a copy of the stack, as
            // glibc's system call wrappers keep no frame pointer.
            let traced = engine_command(
                command,
                "bpf",
                interpreter,
                &["-k=dwarf", "-k", "--format", "ndjson", "-o", "-"],
            )
            .arg("--settle")
            .output()
            .expect("run microscope");
            check_frames(interpreter, &records(interpreter, &traced));
        }
    }
}
