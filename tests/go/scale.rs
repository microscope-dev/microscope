//! A hundred thousand parked goroutines. Paging through the program's
//! goroutines returns each exactly once, as many as the runtime counts,
//! leaving the runtime's own out before paging; each page's work is
//! bounded however many goroutines there are; and a goroutine's frames
//! are found without reading every other goroutine.

use std::collections::BTreeSet;
use std::process::Stdio;

use microscope::{
    ExecutionContext, InferiorState, LaunchOptions, StackFrameId, StopContext, StopReason,
    UnwindTermination,
};

use crate::support::{Scenario, ScratchDir};

/// Goroutines read in one page.
const PAGE: usize = 500;

#[tokio::test]
async fn a_hundred_thousand_goroutines_page_once_each_with_bounded_work() {
    let scratch = ScratchDir::new("scale");
    let output = scratch.path().join("stdout");
    let mut scenario = Scenario::launch("scale-go");
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
    let counted: usize = printed
        .trim()
        .strip_prefix("goroutines ")
        .and_then(|count| count.parse().ok())
        .unwrap_or_else(|| panic!("{printed}"));

    let mut numbers = BTreeSet::new();
    let mut from = None;
    let mut last = None;

    loop {
        let page = scenario
            .operation("program tasks", scenario.handle().program_tasks(from, PAGE))
            .await;
        assert!(page.gaps.is_empty(), "{:?}", page.gaps);
        assert!(page.tasks.len() <= PAGE);

        // Reading a page never reads the whole goroutine list.
        assert!(
            page.usage.memory_reads <= 32 * PAGE as u64,
            "{:?}",
            page.usage
        );
        assert!(
            page.usage.memory_bytes <= 256 * PAGE as u64,
            "{:?}",
            page.usage
        );

        for task in page.tasks.iter() {
            assert!(!task.internal, "{task:?}");
            assert!(numbers.insert(task.id.number), "{task:?} again");
            last = Some(task.id);
        }

        match page.next {
            Some(next) => from = Some(next),
            None => break,
        }
    }

    assert_eq!(numbers.len(), counted);

    let snapshot = scenario.snapshot().await;
    let InferiorState::Stopped { stop_id, .. } = snapshot.inferior else {
        panic!("not stopped");
    };

    let task = last.expect("a goroutine");
    let trace = scenario
        .operation(
            "backtrace",
            scenario
                .handle()
                .at(StopContext {
                    stop: stop_id,
                    execution: ExecutionContext::Task(task),
                    frame: StackFrameId::INNERMOST,
                })
                .backtrace(),
        )
        .await;

    assert_eq!(trace.termination, UnwindTermination::Complete);
    assert!(
        trace.frames.iter().any(|frame| frame
            .function
            .as_ref()
            .is_some_and(|function| function.name.as_ref() == "main.park")),
        "{trace:#?}"
    );
    scenario.shutdown().await;
}
