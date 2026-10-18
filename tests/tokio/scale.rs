//! A hundred thousand parked tasks: every one is listed once, a page at a
//! time, and a page reads a bounded number of bytes for each task it
//! lists, wherever in the list it begins.

use std::collections::BTreeSet;
use std::fs::File;
use std::process::Stdio;

use microscope::{LaunchOptions, StopReason, TaskPage, TaskState};

use crate::support::truth::Report;
use crate::support::{Scenario, ScratchDir};

const FIXTURE: &str = "tokio-scale-o0";
const TASKS: usize = 100_000;

/// The tasks one page asks for.
const PAGE: usize = 1_024;

/// What listing one parked task may read: its header and its link in the
/// list, its future's chain of awaits, and what the future it waits on
/// says of itself. A task here reads under a hundred bytes; reading the
/// list again on a page would read seven megabytes.
const BYTES_PER_TASK: u64 = 256;

/// What a page may read besides its tasks: where the runtime is and what
/// each thread runs.
const BYTES_PER_PAGE: u64 = 16 * 1_024;

/// Every task is listed once, as the program reports it, each parked, and
/// no page reads more than its tasks and a fixed amount more.
#[tokio::test]
async fn a_hundred_thousand_tasks_are_listed_at_a_bounded_cost_a_page() {
    let scratch = ScratchDir::new("scale");
    let output = scratch.path().join("stdout");

    // The invariants read every task at every stop, which is not for this
    // many; the pages here check what they would.
    let mut scenario = Scenario::launch(FIXTURE);
    scenario.add_breakpoint("truth_reached").await;
    let reason = scenario
        .run_with_to_stop(LaunchOptions {
            stdout: Some(Stdio::from(File::create(&output).expect("an output file"))),
            ..LaunchOptions::default()
        })
        .await;
    assert!(
        matches!(reason, StopReason::Breakpoint { .. }),
        "{reason:?}"
    );
    let expected = Report::read(&output)
        .lines()
        .filter(|fields| fields.first().is_some_and(|kind| kind == "task"))
        .map(|fields| {
            fields
                .get(1)
                .and_then(|id| id.parse::<u64>().ok())
                .expect("a task's id")
        })
        .collect::<BTreeSet<_>>();
    assert_eq!(expected.len(), TASKS);

    let mut listed = BTreeSet::new();
    let mut from = None;
    loop {
        let TaskPage {
            tasks,
            next,
            gaps,
            usage,
        } = scenario
            .operation("tasks", scenario.handle().tasks(from, PAGE))
            .await;
        assert!(gaps.is_empty(), "after {}: {gaps:?}", listed.len());
        assert!(tasks.len() <= PAGE);
        let count = u64::try_from(tasks.len()).expect("a page's size fits");
        assert!(
            usage.memory_bytes <= BYTES_PER_PAGE + BYTES_PER_TASK * count,
            "after {}, {count} tasks read {usage:?}",
            listed.len()
        );
        for task in tasks.iter() {
            assert_eq!(task.state, TaskState::Blocked, "{task:#?}");
            assert!(listed.insert(task.id.number), "listed twice: {task:#?}");
        }

        match next {
            Some(next) => from = Some(next),
            None => break,
        }
    }

    assert_eq!(listed, expected);
    scenario.shutdown().await;
}
