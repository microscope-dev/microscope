//! Two tasks deadlocked on each other's `tokio::sync::Mutex`: both are
//! listed waiting at the same await for the semaphore under the mutex, and
//! what each holds and what each waits for, read from their suspended
//! frames, close the cycle: each waits for the mutex the other holds.

use std::collections::BTreeMap;

use microscope::{
    Evaluation, ExecutionContext, Expression, StackFrameId, StopContext, StopId, StopReason,
    TaskId, TaskState, VariableState, VariableValue,
};

use crate::invariants::checked;
use crate::stops::line;
use crate::support::Scenario;
use crate::workers::tasks;

const BUILDS: [&str; 4] = [
    "tokio-deadlock-o0",
    "tokio-deadlock-o3",
    "tokio-deadlock-1.52-o0",
    "tokio-deadlock-1.52-o3",
];
const SOURCE: &str = "deadlock/src/main.rs";

/// The address an expression names in frame `frame` of a task.
async fn address(
    scenario: &Scenario,
    stop: StopId,
    task: TaskId,
    frame: StackFrameId,
    text: &str,
) -> u64 {
    let expression = Expression::parse(text).expect("an expression");
    let evaluation = scenario
        .operation(
            text,
            scenario
                .handle()
                .at(StopContext {
                    stop,
                    execution: ExecutionContext::Task(task),
                    frame,
                })
                .evaluate(&expression),
        )
        .await;

    match evaluation {
        Evaluation::Value { value, .. } => match value.state {
            VariableState::Available {
                value: VariableValue::Address(address),
                ..
            } => address.address.get(),
            other => panic!("task {task}, frame {frame:?}, {text}: {other:?}"),
        },
        other => panic!("task {task}, frame {frame:?}, {text}: {other:?}"),
    }
}

/// Both tasks wait for the semaphore under a mutex, at the line that locks
/// the second, and the mutex each waits for is the one the other holds.
#[tokio::test]
async fn each_deadlocked_task_waits_for_the_mutex_the_other_holds() {
    for fixture in BUILDS {
        let mut scenario = checked(fixture);
        scenario.add_breakpoint("truth_reached").await;

        assert!(matches!(
            scenario.run_to_stop().await,
            StopReason::Breakpoint { .. }
        ));

        let (listed, gaps) = tasks(&scenario, 64).await;
        assert!(gaps.is_empty(), "{fixture}: {gaps:?}");

        let [left, right] = &listed[..] else {
            panic!("{fixture}: {listed:#?}");
        };

        for task in [left, right] {
            assert_eq!(task.state, TaskState::Blocked, "{fixture}: {task:#?}");
            assert!(
                task.detail
                    .as_deref()
                    .is_some_and(|detail| detail.contains("permits")),
                "{fixture}: {task:#?}"
            );
        }

        // Each task's frames: what tokio's `lock` waits on, and the guard
        // its own function holds.
        let mut holds = BTreeMap::new();
        let mut waits = BTreeMap::new();
        for task in [left, right] {
            let stop = scenario.snapshot().await.stop_id.expect("stopped");
            let trace = scenario
                .operation(
                    "backtrace",
                    scenario
                        .handle()
                        .at(StopContext {
                            stop,
                            execution: ExecutionContext::Task(task.id),
                            frame: StackFrameId::INNERMOST,
                        })
                        .backtrace(),
                )
                .await;

            let index_of = |name: &str| {
                trace
                    .frames
                    .iter()
                    .find(|frame| {
                        frame
                            .function
                            .as_ref()
                            .is_some_and(|function| function.name.starts_with(name))
                    })
                    .unwrap_or_else(|| panic!("{fixture}: no {name}: {trace:#?}"))
                    .id
            };

            let grab = trace
                .frames
                .iter()
                .find(|frame| frame.id == index_of("grab"))
                .expect("the frame found");

            assert_eq!(
                grab.source.as_ref().map(|source| source.line.get()),
                Some(line(SOURCE, "// AWAIT: lock")),
                "{fixture}: {grab:#?}"
            );

            holds.insert(
                task.id.number,
                address(&scenario, stop, task.id, index_of("grab"), "held.lock").await,
            );

            waits.insert(
                task.id.number,
                address(&scenario, stop, task.id, index_of("lock"), "_ref__self").await,
            );
        }

        let (a, b) = (left.id.number, right.id.number);
        assert_ne!(holds[&a], holds[&b], "{fixture}");
        assert_eq!(waits[&a], holds[&b], "{fixture}");
        assert_eq!(waits[&b], holds[&a], "{fixture}");
        scenario.shutdown().await;
    }
}
