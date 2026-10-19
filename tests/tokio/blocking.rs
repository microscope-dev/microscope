//! A blocking pool of one thread, with one closure running and two queued:
//! the running closure is its thread's task, running there, and the queued
//! ones are listed as queued in the pool, by the ids the program gives
//! them. A breakpoint in a closure stops in its task on the pool's thread,
//! and each queued closure runs as its own task in turn.

use std::fs::File;
use std::process::Stdio;

use microscope::{InferiorState, LaunchOptions, StopReason, TaskState, ThreadActivity, ThreadId};

use crate::invariants::checked;
use crate::steps::stopped_task;
use crate::stops::line;
use crate::support::truth::Report;
use crate::support::{Scenario, ScratchDir};
use crate::workers::{activities, tasks};

const BUILDS: [&str; 4] = [
    "tokio-blocking-o0",
    "tokio-blocking-o3",
    "tokio-blocking-1.52-o0",
    "tokio-blocking-1.52-o3",
];
const SOURCE: &str = "blocking/src/main.rs";

/// What the program reported at its checkpoint.
#[derive(Debug, Default)]
struct Truth {
    blocker: u64,
    queued: Vec<u64>,
    pool: i32,
}

impl Truth {
    fn parse(text: &str) -> Self {
        let mut truth = Self::default();
        for fields in Report::parse(text).lines() {
            match fields.iter().map(String::as_str).collect::<Vec<_>>()[..] {
                ["task", id, "release", _] => truth.blocker = id.parse().expect("an id"),
                ["queued", id] => truth.queued.push(id.parse().expect("an id")),
                ["pool", tid] => truth.pool = tid.parse().expect("a thread id"),
                _ => {}
            }
        }

        truth
    }
}

async fn stopped_thread(scenario: &mut Scenario) -> ThreadId {
    match scenario.snapshot().await.inferior {
        InferiorState::Stopped { thread_id, .. } => thread_id,
        other => panic!("not stopped: {other:?}"),
    }
}

/// At the checkpoint the pool's thread runs the blocker's task, and the
/// two queued closures wait in its queue in the order they were spawned.
/// Released, the blocker stops at a breakpoint in its closure, on the
/// pool's thread, and each queued closure then stops in its own task.
#[tokio::test]
async fn the_running_and_queued_closures_are_the_pools_tasks() {
    for fixture in BUILDS {
        let scratch = ScratchDir::new("blocking");
        let output = scratch.path().join("stdout");
        let mut scenario = checked(fixture);
        scenario.add_breakpoint("truth_reached").await;

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

        let truth = Truth::parse(&std::fs::read_to_string(&output).expect("the output"));
        assert_eq!(truth.queued.len(), 2, "{fixture}: {truth:?}");

        let pool = ThreadId::new(truth.pool.try_into().expect("a positive thread id"));

        let (listed, gaps) = tasks(&scenario, 64).await;
        assert!(gaps.is_empty(), "{fixture}: {gaps:?}");
        assert_eq!(listed.len(), 3, "{fixture}: {listed:#?}");

        let listed_as = |id: u64| {
            listed
                .iter()
                .find(|task| task.id.number == id)
                .unwrap_or_else(|| panic!("{fixture}: task {id}: {listed:#?}"))
        };

        let running = listed_as(truth.blocker);

        assert_eq!(
            (running.id.number, running.state.clone(), running.thread),
            (truth.blocker, TaskState::Running, Some(pool)),
            "{fixture}: {running:#?}"
        );

        assert_eq!(
            running.detail.as_deref(),
            Some("running a blocking closure"),
            "{fixture}"
        );

        for id in &truth.queued {
            let task = listed_as(*id);
            assert_eq!(
                (task.id.number, task.state.clone(), task.thread),
                (*id, TaskState::Runnable, None),
                "{fixture}: {task:#?}"
            );
            assert_eq!(
                task.detail.as_deref(),
                Some("queued in the blocking pool"),
                "{fixture}"
            );
        }

        assert!(
            matches!(
                activities(&mut scenario).await.get(&pool),
                Some(ThreadActivity::Task { task, .. }) if task.number == truth.blocker
            ),
            "{fixture}"
        );

        let released = scenario
            .add_source_breakpoint(SOURCE, line(SOURCE, "// BLOCKER: released"))
            .await;

        assert!(matches!(
            scenario.resume_to_stop().await,
            StopReason::Breakpoint { .. }
        ));

        assert_eq!(stopped_thread(&mut scenario).await, pool, "{fixture}");
        assert_eq!(
            stopped_task(&mut scenario).await,
            truth.blocker,
            "{fixture}"
        );

        scenario.remove_breakpoint(released.id).await;

        let runs = scenario
            .add_source_breakpoint(SOURCE, line(SOURCE, "// QUEUED: runs"))
            .await;

        for id in &truth.queued {
            assert!(matches!(
                scenario.resume_to_stop().await,
                StopReason::Breakpoint { .. }
            ));
            assert_eq!(stopped_thread(&mut scenario).await, pool, "{fixture}");
            assert_eq!(stopped_task(&mut scenario).await, *id, "{fixture}");
        }

        scenario.remove_breakpoint(runs.id).await;
        scenario.shutdown().await;
    }
}
