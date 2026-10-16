//! Python programs' stacks at native stops.

use microscope::{
    CodeRole, CoreDumpOptions, ExecutionContext, FrameKind, InferiorState, StackFrameId,
    StackSegment, StopContext, ThreadId,
};

use crate::session::PythonSession;
use crate::support::Scenario;
use crate::support::truth::Report;

/// The interpreters with debug files, whose call machinery DWARF names.
const DESCRIBED: [&str; 3] = ["3.12", "3.13", "3.14"];

/// The evaluation loop and the calls around it are the interpreter's, in
/// the frames of every thread; the C functions Python calls are not.
#[tokio::test]
async fn the_interpreters_loop_and_calls_are_its_own() {
    for interpreter in DESCRIBED {
        let mut session = PythonSession::launch(interpreter, "stacks.py", &[]).await;
        let truth = session.stop_at("parked").await;
        let threads: Vec<u64> = truth
            .iter()
            .filter(|fields| fields[0] == "thread")
            .map(|fields| fields[1].parse().expect("a native thread id"))
            .collect();
        assert_eq!(threads.len(), 3, "{}: {truth:?}", session.context());
        for thread in threads {
            let trace = session.backtrace(ThreadId::new(thread)).await;
            let context = format!("{} thread {thread}: {trace:#?}", session.context());
            let named = |name: &str| {
                trace.frames.iter().find(|frame| {
                    frame
                        .function
                        .as_ref()
                        .is_some_and(|function| &*function.name == name)
                })
            };
            let evaluating = named("_PyEval_EvalFrameDefault").expect(&context);
            assert_eq!(evaluating.role, CodeRole::Interpreter, "{context}");
            if let Some(vectorcall) = named("_PyFunction_Vectorcall") {
                assert_eq!(vectorcall.role, CodeRole::Interpreter, "{context}");
            }
        }

        session.shutdown().await;
    }
}

/// Every interpreter in the matrix, as the flake names it.
const MATRIX: [&str; 9] = [
    "3.12",
    "3.13",
    "3.13t",
    "3.14",
    "3.14t",
    "3.15",
    "3.15t",
    "debian-3.13",
    "ubuntu-3.12",
];

/// A Python frame as CPython reported it: its qualified name, file, and
/// line.
type Reported = (String, String, String);

/// Each thread a checkpoint reported: its native id, whether it printed
/// the report, and its Python frames, innermost first.
fn reported_threads(truth: &[Vec<String>]) -> Vec<(u64, bool, Vec<Reported>)> {
    let mut threads: Vec<(u64, bool, Vec<Reported>)> = Vec::new();
    for fields in truth {
        match fields[0].as_str() {
            "thread" => threads.push((
                fields[1].parse().expect("a native thread id"),
                fields[3] == "printer",
                Vec::new(),
            )),
            "frame" => threads
                .last_mut()
                .expect("a frame follows its thread")
                .2
                .push((fields[3].clone(), fields[4].clone(), fields[5].clone())),
            _ => {}
        }
    }

    threads
}

/// Checks that a thread's backtrace shows the Python frames CPython
/// `reported`, past `beneath` innermost frames the thread entered after it
/// reported them, each segment before the activation of the evaluation
/// loop that runs it.
fn assert_python_frames(
    trace: &microscope::Backtrace,
    reported: &[Reported],
    beneath: usize,
    context: &str,
) {
    let context = format!("{context}: {trace:#?}");
    assert!(trace.unfollowed.is_empty(), "{context}");
    let shown: Vec<Reported> = trace
        .frames
        .iter()
        .filter(|frame| matches!(frame.kind, FrameKind::Interpreted { .. }))
        .map(|frame| {
            let recorded = frame.recorded.as_ref().expect(&context);
            let place = recorded.place.as_ref().expect(&context);
            (
                recorded.function.to_string(),
                place.path.to_string(),
                place.line.to_string(),
            )
        })
        .collect();
    assert_eq!(shown.len(), reported.len() + beneath, "{context}");
    assert!(shown.ends_with(reported), "{context}\n{reported:#?}");

    for (index, frame) in trace.frames.iter().enumerate() {
        let ends = matches!(frame.kind, FrameKind::Interpreted { .. })
            && trace
                .frames
                .get(index + 1)
                .is_some_and(|next| !matches!(next.kind, FrameKind::Interpreted { .. }));
        if ends {
            let activation = &trace.frames[index + 1];
            assert_eq!(activation.role, CodeRole::Interpreter, "{context}");
            assert_eq!(activation.segment, StackSegment::Thread, "{context}");
        }
    }
}

/// Each thread's Python frames are CPython's own, in order, each placed
/// before the activation of the evaluation loop that runs it, on every
/// interpreter in the matrix.
#[tokio::test]
async fn each_threads_python_frames_are_the_ones_cpython_reports() {
    for interpreter in MATRIX {
        let mut session = PythonSession::launch(interpreter, "stacks.py", &[]).await;
        let truth = session.stop_at("parked").await;
        let threads = reported_threads(&truth);
        assert_eq!(threads.len(), 3, "{}: {truth:?}", session.context());
        for (thread, printer, reported) in threads {
            let trace = session.backtrace(ThreadId::new(thread)).await;
            // The printer reported its frames from its checkpoint's caller,
            // and parks in two frames beneath it.
            let beneath = if printer { 2 } else { 0 };
            let context = format!("{} thread {thread}", session.context());
            assert_python_frames(&trace, &reported, beneath, &context);
        }

        session.shutdown().await;
    }
}

/// A core dump's threads show the Python frames the program reported just
/// before it aborted, on interpreters that each lay out or run their
/// frames their own way.
#[tokio::test]
async fn a_cores_python_frames_are_the_ones_cpython_reported() {
    for interpreter in ["3.13", "3.14t", "3.15", "debian-3.13"] {
        let name = format!("python-stacks-{interpreter}.core");
        let core = Scenario::fixture(&name);
        let log = std::fs::read_to_string(core.with_extension("core.log"))
            .expect("the core's log; run `just build-test-programs`");
        let truth = Report::parse(&log)
            .checkpoint("parked")
            .expect("the checkpoint the core was taken at")
            .to_vec();
        let threads = reported_threads(&truth);
        assert_eq!(threads.len(), 3, "{name}: {truth:?}");

        let mut scenario = Scenario::open_core(&name, &CoreDumpOptions::new(core));
        let InferiorState::Stopped { stop_id, .. } = scenario.snapshot().await.inferior else {
            panic!("{name}: the core is not stopped");
        };

        for (thread, printer, reported) in threads {
            let context = StopContext {
                stop: stop_id,
                execution: ExecutionContext::Thread(ThreadId::new(thread)),
                frame: StackFrameId::INNERMOST,
            };
            let trace = scenario
                .operation(
                    "thread backtrace",
                    scenario.handle().at(context).backtrace(),
                )
                .await;

            // The printer aborts in its checkpoint, beneath what it reported.
            let beneath = usize::from(printer);
            assert_python_frames(
                &trace,
                &reported,
                beneath,
                &format!("{name} thread {thread}"),
            );
        }

        scenario.shutdown().await;
    }
}
