use std::collections::BTreeMap;
use std::process::Stdio;

use microscope::{
    BreakpointId, BreakpointOptions, BreakpointSpec, Condition, Evaluation, ExecutionContext,
    Expression, InferiorState, LaunchOptions, ScalarValue, StackFrameId, StepKind, StopContext,
    StopReason, ThreadId, ThreadState, VariableState, VariableValue,
};

use crate::invariants::checked;
use crate::stops::{integer, place};
use crate::support::{Scenario, ScratchDir};

const BUILDS: [&str; 2] = ["torture-go-o0", "torture-go-o2"];

const WORKERS: i128 = 8;
const ROUNDS: i128 = 200;
const EVERY: i128 = 25;

const MOST_STOPS: usize = 1000;

async fn integer_in(scenario: &Scenario, thread: ThreadId, text: &str) -> i128 {
    let snapshot = scenario
        .operation("snapshot", scenario.handle().snapshot())
        .await;
    let InferiorState::Stopped { stop_id, .. } = snapshot.inferior else {
        panic!("not stopped");
    };

    let expression = Expression::parse(text).expect("an expression");
    let at = scenario.handle().at(StopContext {
        stop: stop_id,
        execution: ExecutionContext::Thread(thread),
        frame: StackFrameId::INNERMOST,
    });

    match scenario.operation(text, at.evaluate(&expression)).await {
        Evaluation::Value {
            value:
                microscope::InspectedValue {
                    state:
                        VariableState::Available {
                            value: VariableValue::Scalar(ScalarValue::Signed(value)),
                            ..
                        },
                    ..
                },
            ..
        } => value,
        other => panic!("{text} on thread {thread}: {other:?}"),
    }
}

async fn record_hits(
    scenario: &Scenario,
    breakpoint: BreakpointId,
    hits: &mut BTreeMap<i128, Vec<i128>>,
) {
    let snapshot = scenario
        .operation("snapshot", scenario.handle().snapshot())
        .await;

    for thread in snapshot.threads.iter() {
        let ThreadState::Stopped {
            reason: Some(StopReason::Breakpoint { hits: hit, .. }),
        } = &thread.state
        else {
            continue;
        };

        if !hit.iter().any(|hit| hit.breakpoint == breakpoint) {
            continue;
        }

        let id = integer_in(scenario, thread.id, "id").await;
        let round = integer_in(scenario, thread.id, "round").await;
        let rounds = hits.entry(id).or_default();
        assert!(
            !rounds.contains(&round),
            "worker {id} hit round {round} again"
        );
        rounds.push(round);
    }
}

#[tokio::test]
async fn steps_under_load_stay_with_their_goroutines_and_lose_no_hit() {
    for fixture in BUILDS {
        let scratch = ScratchDir::new("torture");
        let output = scratch.path().join("stdout");
        let mut scenario = checked(fixture);
        let breakpoint = scenario
            .operation(
                "conditional breakpoint",
                scenario.handle().add_breakpoint_with(
                    BreakpointSpec::Function("main.step".into()),
                    BreakpointOptions {
                        condition: Some(
                            Condition::parse(&format!("round % {EVERY} == 0")).expect("parses"),
                        ),
                        ..BreakpointOptions::default()
                    },
                ),
            )
            .await
            .id;

        let mut reason = scenario
            .run_with_to_stop(LaunchOptions {
                stdout: Some(Stdio::from(
                    std::fs::File::create(&output).expect("create standard output"),
                )),
                ..LaunchOptions::default()
            })
            .await;
        let mut hits = BTreeMap::new();

        let mut stepping = None;
        let mut stops = 0;

        loop {
            stops += 1;
            assert!(stops < MOST_STOPS, "{fixture}: too many stops");
            if let StopReason::Exited(status) = reason {
                assert_eq!(status, microscope::ExitStatus::Code(0), "{fixture}");
                break;
            }

            record_hits(&scenario, breakpoint, &mut hits).await;
            let next = match &reason {
                StopReason::Breakpoint { .. } => {
                    let task = integer(&scenario, "$task").await.expect("a goroutine");
                    let id = integer(&scenario, "id").await.expect("the worker");
                    stepping = Some((task, id));
                    StepKind::OverSource
                }
                StopReason::Step { kind } => {
                    let (task, id) = stepping.expect("a step began");
                    let context = format!("{fixture}: {kind:?} on worker {id}");
                    assert_eq!(integer(&scenario, "$task").await, Some(task), "{context}");
                    assert_eq!(integer(&scenario, "id").await, Some(id), "{context}");
                    let (function, _) = place(&scenario).await;

                    match kind {
                        StepKind::OverSource => {
                            assert_eq!(function, "main.step", "{context}");
                            StepKind::Out
                        }
                        StepKind::Out => {
                            assert_eq!(function, "main.work", "{context}");
                            stepping = None;
                            reason = scenario.resume_to_stop().await;
                            continue;
                        }
                        other => panic!("{context}: {other:?}"),
                    }
                }
                other => panic!("{fixture}: {other:?}"),
            };

            reason = scenario.step_to_stop(next).await;
        }

        scenario.shutdown().await;

        let every = (0..ROUNDS).step_by(EVERY as usize).collect::<Vec<_>>();
        assert_eq!(hits.len() as i128, WORKERS, "{fixture}: {hits:?}");
        for (id, mut rounds) in hits {
            rounds.sort_unstable();
            assert_eq!(rounds, every, "{fixture}: worker {id}");
        }

        assert_eq!(
            std::fs::read_to_string(&output).expect("read standard output"),
            "every worker's work is right\n",
            "{fixture}"
        );
    }
}
