//! A program that corrupts the runtime's record of one of its parked
//! goroutines: its saved registers and its stack's bounds. The goroutine
//! is still listed, and its frames are refused with the reason, never
//! read from the garbage; every other goroutine reads as before, and the
//! program runs on to its end.

use std::process::Stdio;

use microscope::{
    Error, ExecutionContext, ExitStatus, InferiorState, LaunchOptions, StackFrameId, StopContext,
    StopReason, UnwindTermination,
};

use crate::support::{Scenario, ScratchDir};

#[tokio::test]
async fn a_corrupted_goroutine_is_refused_and_the_rest_read_as_before() {
    let scratch = ScratchDir::new("corrupt");
    let output = scratch.path().join("stdout");
    let mut scenario = Scenario::launch("corrupt-go");
    scenario.add_breakpoint("main.checkpoint").await;

    let reason = scenario
        .run_with_to_stop(LaunchOptions {
            stdout: Some(Stdio::from(
                std::fs::File::create(&output).expect("create standard output"),
            )),
            ..LaunchOptions::default()
        })
        .await;
    assert!(
        matches!(reason, StopReason::Breakpoint { .. }),
        "{reason:?}"
    );

    let printed = std::fs::read_to_string(&output).expect("read standard output");
    let victim: u64 = printed
        .trim()
        .strip_prefix("corrupted goroutine ")
        .and_then(|number| number.parse().ok())
        .unwrap_or_else(|| panic!("{printed}"));

    let snapshot = scenario.snapshot().await;
    let InferiorState::Stopped { stop_id, .. } = snapshot.inferior else {
        panic!("not stopped");
    };

    let page = scenario
        .operation("tasks", scenario.handle().tasks(None, 64))
        .await;
    assert!(page.gaps.is_empty(), "{page:?}");
    assert!(
        page.tasks.iter().any(|task| task.id.number == victim),
        "{page:?}"
    );

    for task in page.tasks.iter() {
        let at = StopContext {
            stop: stop_id,
            execution: ExecutionContext::Task(task.id),
            frame: StackFrameId::INNERMOST,
        };

        let trace = scenario.handle().at(at).backtrace().await;
        if task.id.number == victim {
            let Err(Error::TaskUnavailable { reason, .. }) = &trace else {
                panic!("goroutine {victim}: {trace:#?}");
            };

            assert!(reason.contains("outside its stack"), "{reason}");
        } else {
            let trace = trace.unwrap_or_else(|error| panic!("{task:?}: {error}"));
            assert_eq!(trace.termination, UnwindTermination::Complete, "{task:?}");
        }
    }

    assert_eq!(
        scenario.resume_to_stop().await,
        StopReason::Exited(ExitStatus::Code(0))
    );
    scenario.shutdown().await;
}
