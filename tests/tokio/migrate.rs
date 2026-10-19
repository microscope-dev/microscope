//! A task that parks on one worker and resumes on another thread, while a
//! task of the program's holds the thread it left in `block_in_place`:
//! `next` over the await follows the task to the thread it resumes on, the
//! held thread still runs the holding task, the thread that took over its
//! worker's core is a worker, and the program agrees on every thread.

use std::collections::BTreeSet;
use std::fs::File;
use std::process::Stdio;

use microscope::{
    InferiorState, LaunchOptions, StepKind, StopReason, TaskState, ThreadActivity, ThreadId,
};

use crate::invariants::checked;
use crate::steps::stopped_task;
use crate::stops::{integer, line, place};
use crate::support::truth::Report;
use crate::support::{Scenario, ScratchDir};
use crate::workers::{activities, tasks};

const BUILDS: [&str; 4] = [
    "tokio-migrate-o0",
    "tokio-migrate-o3",
    "tokio-migrate-1.52-o0",
    "tokio-migrate-1.52-o3",
];
const SOURCE: &str = "migrate/src/main.rs";

async fn stopped_thread(scenario: &mut Scenario) -> ThreadId {
    match scenario.snapshot().await.inferior {
        InferiorState::Stopped { thread_id, .. } => thread_id,
        other => panic!("not stopped: {other:?}"),
    }
}

/// The fields of the program's one `TRUTH` line of `kind`.
fn reported(text: &str, kind: &str) -> Vec<u64> {
    let report = Report::parse(text);
    let fields = report
        .lines()
        .filter(|fields| fields.first().is_some_and(|first| first == kind))
        .collect::<Vec<_>>();

    let [fields] = fields[..] else {
        panic!("{kind}: {text}");
    };

    fields[1..]
        .iter()
        .map(|field| field.parse().expect("a number"))
        .collect()
}

/// Stopped where the task awaits, `next` waits while the task parks and a
/// holder takes its thread, then ends on the next line in the same task on
/// another thread. There the left thread runs the holder, in its
/// `block_in_place`, and two other threads hold the runtime's two cores.
#[tokio::test]
async fn next_over_an_await_follows_its_task_to_another_thread() {
    for fixture in BUILDS {
        let scratch = ScratchDir::new("migrate");
        let output = scratch.path().join("stdout");
        let mut scenario = checked(fixture);
        let awaits = scenario
            .add_source_breakpoint(SOURCE, line(SOURCE, "// AWAIT: migrate"))
            .await;

        let reason = scenario
            .run_with_to_stop(LaunchOptions {
                stdout: Some(Stdio::from(File::create(&output).expect("an output file"))),
                ..LaunchOptions::default()
            })
            .await;

        assert!(
            matches!(reason, StopReason::Breakpoint { .. }),
            "{fixture}: {reason:?}"
        );

        let task = stopped_task(&mut scenario).await;
        let before = stopped_thread(&mut scenario).await;
        let threads_before = activities(&mut scenario)
            .await
            .into_keys()
            .collect::<BTreeSet<_>>();

        scenario.remove_breakpoint(awaits.id).await;

        assert_eq!(
            scenario.step_to_stop(StepKind::OverSource).await,
            StopReason::Step {
                kind: StepKind::OverSource
            },
            "{fixture}"
        );

        assert_eq!(
            place(&scenario).await,
            ("migrating".to_owned(), line(SOURCE, "// STEP: migrated")),
            "{fixture}"
        );

        assert_eq!(stopped_task(&mut scenario).await, task, "{fixture}");
        let after = stopped_thread(&mut scenario).await;
        assert_ne!(after, before, "{fixture}");

        let me = integer(&scenario, "me").await;
        assert!(
            me == Some(i128::from(task)) || me.is_none() && fixture.ends_with("o3"),
            "{fixture}: {me:?}"
        );

        let text = std::fs::read_to_string(&output).expect("the output");
        let [holder, held] = reported(&text, "holder")[..] else {
            panic!("{fixture}: {text}");
        };

        assert_eq!(held, before.get(), "{fixture}");

        let activities = activities(&mut scenario).await;
        assert!(
            matches!(
                activities.get(&before),
                Some(ThreadActivity::Task { task, .. }) if task.number == holder
            ),
            "{fixture}: {activities:?}"
        );

        let (listed, gaps) = tasks(&scenario, 64).await;
        assert!(gaps.is_empty(), "{fixture}: {gaps:?}");

        let holding = listed
            .iter()
            .find(|listed| listed.id.number == holder)
            .unwrap_or_else(|| panic!("{fixture}: {listed:#?}"));

        assert_eq!(
            (holding.state.clone(), holding.thread),
            (TaskState::Running, Some(before)),
            "{fixture}"
        );

        // The worker that took over the held thread's core is new, and
        // holds a core as the other worker does: it runs the migrated task
        // or is idle.
        let cores = activities
            .iter()
            .filter(|(thread, activity)| {
                **thread != before
                    && match activity {
                        ThreadActivity::Idle => true,
                        ThreadActivity::Task { task: running, .. } => running.number == task,
                        _ => false,
                    }
            })
            .map(|(thread, _)| *thread)
            .collect::<BTreeSet<_>>();

        assert_eq!(cores.len(), 2, "{fixture}: {activities:?}");
        assert!(cores.contains(&after), "{fixture}: {activities:?}");
        assert!(
            cores.iter().any(|core| !threads_before.contains(core)),
            "{fixture}: {threads_before:?} {activities:?}"
        );

        assert!(matches!(
            scenario.resume_to_stop().await,
            StopReason::Exited(_)
        ));

        let text = std::fs::read_to_string(&output).expect("the output");
        assert_eq!(
            reported(&text, "migrated"),
            [task, before.get(), after.get()],
            "{fixture}"
        );

        scenario.shutdown().await;
    }
}
