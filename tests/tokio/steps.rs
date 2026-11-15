use std::process::Stdio;

use microscope::{
    BreakpointSpec, InferiorState, LaunchOptions, LineNumber, StepKind, StopReason, TaskEnding,
    TaskState, ThreadActivity, VariableKind,
};

use crate::invariants::checked;
use crate::stops::{integer, line, place};
use crate::support::Scenario;
use crate::workers::tasks;

const BUILDS: [&str; 4] = [
    "tokio-steps-o0",
    "tokio-steps-o3",
    "tokio-steps-1.52-o0",
    "tokio-steps-1.52-o3",
];
const SOURCE: &str = "steps/src/main.rs";

fn optimized(fixture: &str) -> bool {
    fixture.ends_with("-o3")
}

fn modes(fixture: &str) -> impl Iterator<Item = Option<&'static str>> {
    [None, Some("current")]
        .into_iter()
        .chain((!optimized(fixture)).then_some(Some("local")))
}

fn at(marker: &str) -> BreakpointSpec {
    BreakpointSpec::Source {
        path: SOURCE.into(),
        line: LineNumber::new(line(SOURCE, marker)).expect("one-based"),
    }
}

async fn stopped_once(fixture: &str, mode: Option<&str>, marker: &str) -> Scenario {
    let mut scenario = checked(fixture);
    let breakpoint = scenario.add_breakpoint_spec(at(marker)).await;
    let reason = scenario
        .run_with_to_stop(LaunchOptions {
            arguments: mode.into_iter().map(Into::into).collect(),
            stdout: Some(Stdio::null()),
            ..LaunchOptions::default()
        })
        .await;

    assert!(
        matches!(reason, StopReason::Breakpoint { .. }),
        "{fixture} {mode:?}: {reason:?}"
    );
    scenario.remove_breakpoint(breakpoint.id).await;
    scenario
}

pub async fn stopped_task(scenario: &mut Scenario) -> u64 {
    let snapshot = scenario.snapshot().await;
    let InferiorState::Stopped { thread_id, .. } = snapshot.inferior else {
        panic!("not stopped: {:?}", snapshot.inferior);
    };

    let thread = snapshot
        .threads
        .iter()
        .find(|thread| thread.id == thread_id)
        .expect("the stopped thread is listed");

    match &thread.activity {
        Some(ThreadActivity::Task { task, .. }) => task.number,
        other => panic!("thread {thread_id} runs no task: {other:?}"),
    }
}

async fn step(scenario: &mut Scenario, kind: StepKind) -> (String, u64) {
    let reason = scenario.step_to_stop(kind).await;
    assert_eq!(reason, StopReason::Step { kind });
    place(scenario).await
}

#[tokio::test]
async fn next_over_a_pending_await_ends_on_the_next_line_of_its_task() {
    for fixture in BUILDS {
        for mode in modes(fixture) {
            for (marker, function, after) in [
                ("// AWAIT: inner", "inner", "// STEP: inner-after"),
                ("// AWAIT: outer", "outer", "// STEP: outer-after"),
            ] {
                let context = format!("{fixture} {mode:?} {marker}");
                let mut scenario = stopped_once(fixture, mode, marker).await;
                let task = stopped_task(&mut scenario).await;
                let me = integer(&scenario, "me").await;
                assert!(
                    me == Some(i128::from(task)) || me.is_none() && optimized(fixture),
                    "{context}: {me:?}"
                );
                assert_eq!(
                    step(&mut scenario, StepKind::OverSource).await,
                    (function.to_owned(), line(SOURCE, after)),
                    "{context}"
                );
                assert_eq!(stopped_task(&mut scenario).await, task, "{context}");

                assert_eq!(integer(&scenario, "me").await, None, "{context}");
                scenario.shutdown().await;
            }
        }
    }
}

#[tokio::test]
async fn next_goes_round_a_loop_of_awaits() {
    for fixture in BUILDS {
        for mode in modes(fixture) {
            let context = format!("{fixture} {mode:?}");
            let mut scenario = stopped_once(fixture, mode, "// STEP: round").await;
            let task = stopped_task(&mut scenario).await;
            let mut rounds = Vec::new();

            loop {
                let (function, at) = step(&mut scenario, StepKind::OverSource).await;
                assert_eq!(function, "rounds", "{context}: {rounds:?}");
                assert_eq!(stopped_task(&mut scenario).await, task, "{context}");
                if at == line(SOURCE, "// STEP: round") {
                    rounds.push(integer(&scenario, "round").await);
                }

                if at == line(SOURCE, "// STEP: rounds-after") {
                    break;
                }

                assert!(rounds.len() < 3, "{context}: {rounds:?}");
            }

            assert_eq!(rounds, [Some(1), Some(2)], "{context}");
            scenario.shutdown().await;
        }
    }
}

#[tokio::test]
async fn finish_returns_to_the_awaiter_in_the_same_task() {
    for fixture in BUILDS {
        for mode in modes(fixture) {
            for (from, awaiter, after) in [
                ("// STEP: outer-after", "task", "// STEP: task"),
                ("// STEP: inner", "outer", "// AWAIT: outer"),
                ("// STEP: round", "task", "// AWAIT: rounds"),
            ] {
                let after = if optimized(fixture) && from == "// STEP: inner" {
                    "// STEP: outer-after"
                } else {
                    after
                };

                let context = format!("{fixture} {mode:?} {from}");
                let mut scenario = stopped_once(fixture, mode, from).await;
                let task = stopped_task(&mut scenario).await;
                assert_eq!(
                    step(&mut scenario, StepKind::Out).await,
                    (awaiter.to_owned(), line(SOURCE, after)),
                    "{context}"
                );
                assert_eq!(stopped_task(&mut scenario).await, task, "{context}");
                let returned = scenario
                    .operation("variables", scenario.handle().variables())
                    .await
                    .variables
                    .iter()
                    .filter(|variable| variable.kind == VariableKind::Returned)
                    .map(|variable| format!("{:?}", variable.state))
                    .collect::<Vec<_>>();

                let ready = |poll: &String| poll.contains("Ready") && !poll.contains("Pending");
                assert!(
                    matches!(&returned[..], [poll] if ready(poll))
                        || returned.is_empty() && optimized(fixture),
                    "{context}: {returned:#?}"
                );
                scenario.shutdown().await;
            }
        }
    }
}

#[tokio::test]
async fn a_step_past_a_tasks_end_says_it_finished() {
    for fixture in BUILDS {
        for mode in modes(fixture) {
            for kind in [StepKind::OverSource, StepKind::Out] {
                let context = format!("{fixture} {mode:?} {kind:?}");
                let mut scenario = stopped_once(fixture, mode, "// STEP: task-last").await;
                let task = stopped_task(&mut scenario).await;
                let output = integer(&scenario, "got")
                    .await
                    .zip(integer(&scenario, "more").await)
                    .map(|(got, more)| got + more);
                assert!(output.is_some() || optimized(fixture), "{context}");
                let mut reason = scenario.step_to_stop(kind).await;

                if reason == (StopReason::Step { kind }) {
                    assert_eq!(place(&scenario).await.0, "task", "{context}");
                    reason = scenario.step_to_stop(kind).await;
                }

                let StopReason::TaskEnded {
                    kind: ended,
                    task: finished,
                    ending: TaskEnding::Finished,
                } = reason
                else {
                    panic!("{context}: {reason:?}");
                };

                assert_eq!((ended, finished.number), (kind, task), "{context}");
                if kind == StepKind::Out {
                    let returned = scenario
                        .operation("variables", scenario.handle().variables())
                        .await
                        .variables
                        .iter()
                        .filter(|variable| variable.kind == VariableKind::Returned)
                        .map(|variable| format!("{:?}", variable.state))
                        .collect::<Vec<_>>();
                    let ready = output.map_or_else(
                        || "summary: \"Ready(".to_owned(),
                        |output| format!("summary: \"Ready({output})\""),
                    );
                    assert!(
                        matches!(&returned[..], [poll] if poll.contains(&ready)),
                        "{context}: {output:?}: {returned:#?}"
                    );
                }

                scenario.shutdown().await;
            }
        }
    }
}

#[tokio::test]
async fn a_task_condition_stops_only_in_its_task() {
    for fixture in BUILDS {
        for mode in modes(fixture) {
            let context = format!("{fixture} {mode:?}");
            let mut scenario = stopped_once(fixture, mode, "// STEP: task").await;
            let task = stopped_task(&mut scenario).await;
            let round = scenario.add_breakpoint_spec(at("// STEP: round")).await;
            scenario
                .operation(
                    "condition",
                    scenario.handle().set_breakpoint_condition(
                        round.id,
                        Some(
                            microscope::Condition::parse(&format!("$task == {task}"))
                                .expect("a condition"),
                        ),
                    ),
                )
                .await;
            let mut rounds = Vec::new();
            let mut reason = scenario.resume_to_stop().await;

            while matches!(reason, StopReason::Breakpoint { .. }) {
                assert_eq!(stopped_task(&mut scenario).await, task, "{context}");
                rounds.push(integer(&scenario, "round").await);
                assert!(rounds.len() <= 3, "{context}: {rounds:?}");
                reason = scenario.resume_to_stop().await;
            }

            assert!(
                matches!(reason, StopReason::Exited(_)),
                "{context}: {reason:?}"
            );
            assert_eq!(rounds, [Some(0), Some(1), Some(2)], "{context}");

            let hits = scenario
                .snapshot()
                .await
                .breakpoints
                .iter()
                .find(|breakpoint| breakpoint.id == round.id)
                .expect("the breakpoint")
                .hit_count;
            assert_eq!(hits, 9, "{context}: three tasks go round three times");
            scenario.shutdown().await;
        }
    }
}

#[tokio::test]
async fn tasks_at_the_gate_stay_suspended_until_the_first_task_resumes() {
    for fixture in BUILDS {
        for mode in modes(fixture) {
            let context = format!("{fixture} {mode:?}");
            let scenario = stopped_once(fixture, mode, "// STEP: inner-after").await;
            let page = scenario
                .operation("tasks", scenario.handle().tasks(None, 16))
                .await;
            let suspended: Vec<_> = page
                .tasks
                .iter()
                .filter(|task| !task.internal && task.state == TaskState::Blocked)
                .collect();
            assert_eq!(suspended.len(), 2, "{context}: {:#?}", page.tasks);

            for task in suspended {
                assert_eq!(task.thread, None, "{context}: {task:#?}");
                scenario
                    .operation("select task", scenario.handle().select_context(task.id))
                    .await;
                let trace = crate::stops::backtrace(&scenario).await;
                assert!(
                    trace.frames.iter().any(|frame| {
                        frame
                            .function
                            .as_ref()
                            .is_some_and(|function| function.name.as_ref() == "inner")
                            && frame.source.as_ref().is_some_and(|source| {
                                source.line.get() == line(SOURCE, "// AWAIT: inner")
                            })
                    }),
                    "{context}: {}: {trace:#?}",
                    task.id,
                );
            }

            scenario.shutdown().await;
        }
    }
}

#[tokio::test]
async fn a_step_of_a_suspended_task_waits_for_it_to_resume() {
    for fixture in BUILDS {
        for mode in modes(fixture) {
            for (kind, from, function, marker) in [
                (
                    StepKind::OverSource,
                    "inner",
                    "inner",
                    "// STEP: inner-after",
                ),
                (StepKind::Out, "outer", "task", "// STEP: task"),
            ] {
                let context = format!("{fixture} {mode:?} {kind:?}");

                let mut scenario = stopped_once(fixture, mode, "// STEP: inner-after").await;
                let page = scenario
                    .operation("tasks", scenario.handle().tasks(None, 16))
                    .await;
                let task = page
                    .tasks
                    .iter()
                    .find(|task| task.thread.is_none())
                    .unwrap_or_else(|| panic!("{context}: {:#?}", page.tasks))
                    .id;
                scenario
                    .operation("select task", scenario.handle().select_context(task))
                    .await;
                let frame = crate::stops::backtrace(&scenario)
                    .await
                    .frames
                    .iter()
                    .find(|frame| {
                        frame
                            .function
                            .as_ref()
                            .is_some_and(|function| *function.name == *from)
                    })
                    .unwrap_or_else(|| panic!("{context}: no frame of {from}"))
                    .id;
                scenario
                    .operation("select frame", scenario.handle().select_frame(frame))
                    .await;
                assert_eq!(
                    step(&mut scenario, kind).await,
                    (function.to_owned(), line(SOURCE, marker)),
                    "{context}"
                );
                assert_eq!(stopped_task(&mut scenario).await, task.number, "{context}");

                if kind == StepKind::OverSource && !optimized(fixture) {
                    assert_eq!(
                        integer(&scenario, "before").await,
                        Some(i128::from(task.number) * 10),
                        "{context}"
                    );
                }

                scenario.shutdown().await;
            }
        }
    }
}

#[tokio::test]
async fn another_tasks_breakpoint_ends_a_waiting_step() {
    for fixture in BUILDS {
        let mode = Some("current");
        let mut scenario = stopped_once(fixture, mode, "// AWAIT: inner").await;
        let task = stopped_task(&mut scenario).await;
        scenario.add_breakpoint_spec(at("// STEP: inner")).await;
        let reason = scenario.step_to_stop(StepKind::OverSource).await;
        assert!(
            matches!(reason, StopReason::Breakpoint { .. }),
            "{fixture}: {reason:?}"
        );
        assert_eq!(
            place(&scenario).await,
            ("inner".to_owned(), line(SOURCE, "// STEP: inner")),
            "{fixture}"
        );
        let other = stopped_task(&mut scenario).await;
        assert_ne!(other, task, "{fixture}");

        let reason = scenario.resume_to_stop().await;
        assert!(
            matches!(reason, StopReason::Breakpoint { .. }),
            "{fixture}: {reason:?}"
        );
        assert!(
            ![task, other].contains(&stopped_task(&mut scenario).await),
            "{fixture}"
        );
        let reason = scenario.resume_to_stop().await;
        assert!(
            matches!(reason, StopReason::Exited(_)),
            "{fixture}: {reason:?}"
        );
        scenario.shutdown().await;
    }
}

#[tokio::test]
async fn instruction_steps_and_advance_in_an_async_function() {
    for fixture in BUILDS {
        for mode in modes(fixture) {
            let context = format!("{fixture} {mode:?}");
            let mut scenario = stopped_once(fixture, mode, "// STEP: round").await;
            let task = stopped_task(&mut scenario).await;

            for kind in [StepKind::Instruction, StepKind::OverInstruction] {
                assert_eq!(
                    scenario.step_to_stop(kind).await,
                    StopReason::Step { kind },
                    "{context}"
                );
                assert_eq!(place(&scenario).await.0, "rounds", "{context} {kind:?}");
                assert_eq!(stopped_task(&mut scenario).await, task, "{context}");
            }

            let after = line(SOURCE, "// STEP: rounds-after");
            assert_eq!(
                scenario
                    .advance_to_stop(BreakpointSpec::Source {
                        path: SOURCE.into(),
                        line: LineNumber::new(after).expect("one-based"),
                    })
                    .await,
                StopReason::Step {
                    kind: StepKind::Advance
                },
                "{context}"
            );
            assert_eq!(
                place(&scenario).await,
                ("rounds".to_owned(), after),
                "{context}"
            );
            assert_eq!(stopped_task(&mut scenario).await, task, "{context}");

            if !optimized(fixture) {
                assert_eq!(integer(&scenario, "total").await, Some(3), "{context}");
            }

            scenario.shutdown().await;
        }
    }
}

#[tokio::test]
async fn step_task_on_a_spawn_line_stops_at_the_new_tasks_first_line() {
    for fixture in BUILDS {
        for mode in modes(fixture) {
            let context = format!("{fixture} {mode:?}");
            let spawn = if mode == Some("local") {
                "// SPAWN: local"
            } else {
                "// SPAWN: runtime"
            };

            let mut scenario = stopped_once(fixture, mode, "let handle = match").await;
            assert_eq!(
                step(&mut scenario, StepKind::OverSource).await,
                ("main".to_owned(), line(SOURCE, spawn)),
                "{context}"
            );
            assert_eq!(
                step(&mut scenario, StepKind::IntoNewTask).await,
                ("task".to_owned(), line(SOURCE, "// FIRST: task")),
                "{context}"
            );
            let task = stopped_task(&mut scenario).await;
            let (listed, _) = tasks(&scenario, 64).await;
            assert!(
                listed
                    .iter()
                    .filter(|listed| !listed.internal)
                    .all(|listed| listed.id.number >= task),
                "{context}: task {task} of {listed:#?}"
            );
            assert_eq!(
                step(&mut scenario, StepKind::OverSource).await,
                ("task".to_owned(), line(SOURCE, "// STEP: task")),
                "{context}"
            );
            assert_eq!(stopped_task(&mut scenario).await, task, "{context}");
            let me = integer(&scenario, "me").await;
            assert!(
                me == Some(i128::from(task)) || me.is_none() && optimized(fixture),
                "{context}: {me:?}"
            );
            scenario.shutdown().await;
        }
    }
}

#[tokio::test]
async fn step_task_on_a_line_that_spawns_none_ends_as_next_does() {
    let fixture = BUILDS[0];
    for mode in modes(fixture) {
        for marker in ["// SPAWNS: none", "// WAKES: gate"] {
            let mut ends = Vec::new();

            for kind in [StepKind::OverSource, StepKind::IntoNewTask] {
                let mut scenario = stopped_once(fixture, mode, marker).await;
                ends.push(step(&mut scenario, kind).await);
                scenario.shutdown().await;
            }

            assert_eq!(ends[0], ends[1], "{mode:?} {marker}");
            assert_ne!(ends[0].1, line(SOURCE, marker), "{mode:?} {marker}");
        }
    }
}

#[tokio::test]
async fn step_task_from_a_task_follows_the_task_it_spawns() {
    for fixture in BUILDS {
        let context = fixture;
        let mut scenario = stopped_once(fixture, Some("nested"), "// BEFORE: nested").await;
        let parent = stopped_task(&mut scenario).await;

        let mut place = step(&mut scenario, StepKind::IntoNewTask).await;
        if place == ("parent".to_owned(), line(SOURCE, "// SPAWN: nested")) {
            place = step(&mut scenario, StepKind::IntoNewTask).await;
        }

        assert_eq!(
            place,
            ("child".to_owned(), line(SOURCE, "// FIRST: child")),
            "{context}"
        );
        let child = stopped_task(&mut scenario).await;
        assert_ne!(child, parent, "{context}");
        assert_eq!(
            step(&mut scenario, StepKind::OverSource).await.0,
            "child",
            "{context}"
        );
        assert_eq!(stopped_task(&mut scenario).await, child, "{context}");
        let me = integer(&scenario, "me").await;
        assert!(
            me == Some(i128::from(child)) || me.is_none() && optimized(fixture),
            "{context}: {me:?}"
        );
        scenario.shutdown().await;
    }
}
