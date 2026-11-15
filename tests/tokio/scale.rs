use std::collections::BTreeSet;
use std::fs::File;
use std::process::Stdio;

use microscope::{LaunchOptions, StopReason, TaskPage, TaskState};

use crate::support::truth::Report;
use crate::support::{Scenario, ScratchDir};

const FIXTURE: &str = "tokio-scale-o0";
const TASKS: usize = 2_049;

const PAGE: usize = 32;

const BYTES_PER_TASK: u64 = 256;

const BYTES_PER_PAGE: u64 = 4 * 1_024;

#[tokio::test]
async fn tasks_are_listed_once_at_a_bounded_cost_a_page() {
    let scratch = ScratchDir::new("scale");
    let output = scratch.path().join("stdout");

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
