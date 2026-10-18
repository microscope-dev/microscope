//! Native stacks through the code CPython's JIT writes to memory, which it
//! registers, with the call-frame information that unwinds it, through the
//! GDB JIT interface: a stop in C that the JIT's code called unwinds through
//! that code to the program's outermost frame, names it, and places the
//! Python frames it runs; and a profile's samples do as much.

use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use microscope::profiler::{Profile, RecordOptions, StackEnding, Watch, record_watched};
use microscope::{BreakpointSpec, CodeRole, StopReason, UnwindTermination};

use crate::extension::extension_directory;
use crate::session::PythonSession;
use crate::support::python;

const FIXTURE: &str = "jit.py";

/// The line of the fixture marked `marker`.
fn line(marker: &str) -> u32 {
    let text = std::fs::read_to_string(python::fixture(FIXTURE)).expect("read the fixture");
    let index = text
        .lines()
        .position(|line| line.ends_with(&format!("# {marker}")))
        .unwrap_or_else(|| panic!("no line is marked {marker}"));
    u32::try_from(index + 1).expect("a small line")
}

/// On 3.15, whose JIT `PYTHON_JIT=1` turns on.
#[tokio::test]
async fn a_stack_unwinds_through_the_jits_code() {
    let directory = extension_directory("3.15");
    let mut session =
        PythonSession::launch_with("3.15", FIXTURE, &[&directory], &[("PYTHON_JIT", "1")]).await;
    session.wait_for_output("WARM").await;
    let context = session.context();
    assert!(
        session.output().contains("JIT True"),
        "{context}: the JIT is off: {}",
        session.output()
    );
    session
        .scenario
        .operation(
            "break in reached",
            session
                .scenario
                .handle()
                .add_breakpoint(BreakpointSpec::Function("reached".to_owned())),
        )
        .await;
    let reason = session.finish_run().await;
    assert!(
        matches!(
            reason,
            StopReason::Breakpoint {
                address: Some(_),
                ..
            }
        ),
        "{context}: {reason:?}"
    );

    let trace = session
        .scenario
        .operation("backtrace", session.scenario.handle().backtrace())
        .await;
    let named = |name: &str| {
        trace.frames.iter().position(|frame| {
            frame
                .symbol
                .as_ref()
                .is_some_and(|symbol| &*symbol.name == name)
        })
    };
    let jit = named("py::jit:executor")
        .unwrap_or_else(|| panic!("{context}: no frame of the JIT's code: {trace:#?}"));
    assert_eq!(
        trace.frames[jit].role,
        CodeRole::Interpreter,
        "{context}: the JIT's code is the interpreter's machinery"
    );
    let spin = trace
        .frames
        .iter()
        .position(|frame| {
            frame.recorded.as_ref().is_some_and(|recorded| {
                &*recorded.function == "spin"
                    && recorded.place.as_ref().map(|place| place.line) == Some(line("SPIN_CALL"))
            })
        })
        .unwrap_or_else(|| panic!("{context}: no frame of spin: {trace:#?}"));
    assert!(
        trace.unfollowed.is_empty() && jit < spin,
        "{context}: the Python frames are placed outer to the JIT's code: {trace:#?}"
    );
    assert!(
        trace.termination == UnwindTermination::Complete && named("_start").is_some(),
        "{context}: the stack unwinds to its outermost frame: {trace:#?}"
    );
    session.shutdown().await;
}

/// The JIT's code, named as the debugger names it.
const JIT_CODE: &str = "py::jit:executor";

/// How many samples in the JIT's code a profile waits for, and how long.
const JIT_SAMPLES: usize = 200;
const JIT_DEADLINE: Duration = Duration::from_secs(30);

/// A profile of the loop as the JIT's code runs it names that code and
/// unwinds through it to the program's outermost frame. The objects are
/// read as their samples are, so the program runs until the profile holds
/// enough.
#[test]
fn a_profile_unwinds_through_the_jits_code() {
    let mut command = Command::new(python::interpreter("3.15"));
    command
        .args(["-P", "-s", "-B"])
        .arg(python::fixture(FIXTURE))
        .arg(extension_directory("3.15"))
        .env("PYTHON_JIT", "1")
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let jitted = |profile: &Profile| {
        profile
            .stacks()
            .into_iter()
            .filter(|stack| stack.functions.iter().any(|function| function == JIT_CODE))
            .collect::<Vec<_>>()
    };

    let stop = AtomicBool::new(false);
    let deadline = Instant::now() + JIT_DEADLINE;
    let recording = record_watched(
        command,
        &RecordOptions::default(),
        &mut Watch {
            every: Duration::from_millis(50),
            publish: &mut |profile| {
                if jitted(&profile).len() >= JIT_SAMPLES || Instant::now() > deadline {
                    stop.store(true, Ordering::Relaxed);
                }
            },
            stop: &stop,
            save: None,
        },
    )
    .expect("record");

    let jitted = jitted(&recording.profile);
    assert!(
        jitted.len() >= JIT_SAMPLES,
        "{} samples in the JIT's code",
        jitted.len()
    );

    // Each through the JIT's code reaches the program's outermost frame;
    // a few may be caught as a trace sets up its frame.
    let short: Vec<_> = jitted
        .iter()
        .filter(|stack| {
            stack.ending != StackEnding::Complete
                || stack
                    .functions
                    .last()
                    .is_none_or(|outermost| outermost != "_start")
        })
        .collect();
    assert!(
        short.len() * 50 < jitted.len(),
        "{} of {} short: {short:#?}",
        short.len(),
        jitted.len()
    );
}
