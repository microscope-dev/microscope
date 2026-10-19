//! Async functions under a small executor of the program's own, with no
//! runtime crate: names, breakpoints, steps within a poll, and values.

use std::process::Stdio;

use microscope::{
    BreakpointSpec, LaunchOptions, LineNumber, StepKind, StopReason, ValueChildQuery,
    ValueChildRelationship, ValueChildren, VariableKind, VariableState, VariableUnavailableReason,
};

use crate::stops::{backtrace, evaluated, frames_to, integer, line, locals, place};
use crate::support::Scenario;

const BUILDS: [&str; 4] = [
    "tokio-std-async-o0",
    "tokio-std-async-o3",
    "tokio-std-async-1.52-o0",
    "tokio-std-async-1.52-o3",
];
const SOURCE: &str = "std-async/src/main.rs";

/// A fixture launched to its first stop at the breakpoint `spec`.
async fn stopped_at(fixture: &str, spec: BreakpointSpec) -> Scenario {
    let mut scenario = Scenario::launch(fixture);
    scenario.add_breakpoint_spec(spec).await;
    let reason = scenario
        .run_with_to_stop(LaunchOptions {
            stdout: Some(Stdio::null()),
            ..LaunchOptions::default()
        })
        .await;

    assert!(
        matches!(reason, StopReason::Breakpoint { .. }),
        "{fixture}: {reason:?}"
    );
    scenario
}

/// A fixture launched to its first stop at the breakpoint `spec`, which
/// is then removed, so that only steps stop it.
async fn stopped_once(fixture: &str, spec: BreakpointSpec) -> Scenario {
    let mut scenario = Scenario::launch(fixture);
    let breakpoint = scenario.add_breakpoint_spec(spec).await;
    let reason = scenario
        .run_with_to_stop(LaunchOptions {
            stdout: Some(Stdio::null()),
            ..LaunchOptions::default()
        })
        .await;

    assert!(
        matches!(reason, StopReason::Breakpoint { .. }),
        "{fixture}: {reason:?}"
    );
    scenario.remove_breakpoint(breakpoint.id).await;
    scenario
}

fn at(marker: &str) -> BreakpointSpec {
    BreakpointSpec::Source {
        path: SOURCE.into(),
        line: LineNumber::new(line(SOURCE, marker)).expect("one-based"),
    }
}

/// Each async body is named for the function its programmer wrote, and an
/// async block for its function and number, in both builds: optimization
/// inlines the bodies into one another, and their frames stay.
#[tokio::test]
async fn async_bodies_are_named_for_their_functions() {
    for fixture in BUILDS {
        let scenario = stopped_at(fixture, at("// STEP: leaf-after")).await;
        let trace = backtrace(&scenario).await;
        assert_eq!(
            frames_to(&trace, "run"),
            [
                ("leaf".to_owned(), line(SOURCE, "// STEP: leaf-after")),
                ("middle".to_owned(), line(SOURCE, "// AWAIT: middle")),
                (
                    "main::{async block#0}".to_owned(),
                    line(SOURCE, "// STEP: task-a")
                ),
                ("run".to_owned(), line(SOURCE, "task.as_mut().poll")),
            ],
            "{fixture}: {trace:#?}"
        );
        scenario.shutdown().await;
    }
}

/// `break leaf` binds the body of the async function, past its dispatch on
/// the state, never the function that only builds its future: it stops at
/// the body's first statement, once per call.
#[tokio::test]
async fn a_function_breakpoint_binds_an_async_body() {
    for fixture in BUILDS {
        let mut scenario = Scenario::launch(fixture);
        let breakpoint = scenario.add_breakpoint("leaf").await;
        assert_eq!(breakpoint.locations.len(), 1, "{fixture}: {breakpoint:#?}");
        let mut ids = Vec::new();
        let mut reason = scenario
            .run_with_to_stop(LaunchOptions {
                stdout: Some(Stdio::null()),
                ..LaunchOptions::default()
            })
            .await;

        while matches!(reason, StopReason::Breakpoint { .. }) {
            assert_eq!(
                place(&scenario).await,
                ("leaf".to_owned(), line(SOURCE, "// STEP: leaf")),
                "{fixture}"
            );
            ids.push(integer(&scenario, "id").await);
            assert!(ids.len() <= 2, "{fixture}: {ids:?}");
            reason = scenario.resume_to_stop().await;
        }

        assert!(
            matches!(reason, StopReason::Exited { .. }),
            "{fixture}: {reason:?}"
        );

        // Optimization may leave the argument without a location, but
        // never with another value.
        assert_eq!(ids.len(), 2, "{fixture}");
        for (id, expected) in ids.iter().zip([3, 4]) {
            assert!(
                *id == Some(expected) || (fixture.ends_with("o3") && id.is_none()),
                "{fixture}: {ids:?}"
            );
        }

        scenario.shutdown().await;
    }
}

/// A breakpoint on an await's line stops when execution arrives at the
/// await, once per arrival, never again as the future resumes there after
/// it was pending; and it binds no copy of the line in the code that drops
/// the future.
#[tokio::test]
async fn await_lines_stop_on_arrival_only() {
    for fixture in BUILDS {
        for (marker, arrivals) in [
            ("// AWAIT: leaf", 2),
            ("// AWAIT: middle", 2),
            ("// AWAIT: walk", 3),
        ] {
            let mut scenario = Scenario::launch(fixture);
            let breakpoint = scenario.add_breakpoint_spec(at(marker)).await;
            assert_eq!(
                breakpoint.locations.len(),
                1,
                "{fixture} {marker}: {breakpoint:#?}"
            );
            let mut stops = 0;
            let mut reason = scenario
                .run_with_to_stop(LaunchOptions {
                    stdout: Some(Stdio::null()),
                    ..LaunchOptions::default()
                })
                .await;

            while matches!(reason, StopReason::Breakpoint { .. }) {
                stops += 1;
                assert!(stops <= arrivals, "{fixture} {marker}");
                reason = scenario.resume_to_stop().await;
            }

            assert!(
                matches!(reason, StopReason::Exited { .. }),
                "{fixture} {marker}: {reason:?}"
            );
            assert_eq!(stops, arrivals, "{fixture} {marker}");
            scenario.shutdown().await;
        }
    }
}

/// What a test expects of a variable an async body lists.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Expected {
    /// An integer with this value.
    Integer(i128),

    /// A value that is not an integer.
    Other,

    /// No value: it was last written before the await at this marker's
    /// line, which this poll resumed from, and the future did not keep it.
    NotSaved(&'static str),
}

/// An async body's frame lists each of its variables once, each with the
/// value it has now: never the compiler's temporaries, nor the field of
/// the future that captured an argument beside the variable the body moved
/// it into. A variable the body last wrote before the await it resumed
/// from, which the future did not keep, has no value: its stack slot holds
/// whatever other polls left there.
#[tokio::test]
async fn an_async_frame_lists_its_variables_once_with_current_values() {
    use Expected::{Integer, NotSaved, Other};
    for fixture in BUILDS {
        let mut scenario = Scenario::launch(fixture);
        for marker in [
            "// STEP: walk-body",
            "// STEP: leaf-after",
            "// STEP: middle-after",
        ] {
            scenario.add_breakpoint_spec(at(marker)).await;
        }

        let mut reason = scenario
            .run_with_to_stop(LaunchOptions {
                stdout: Some(Stdio::null()),
                ..LaunchOptions::default()
            })
            .await;
        let (mut walks, mut leaves, mut middles) = (0, 0, 0);

        while matches!(reason, StopReason::Breakpoint { .. }) {
            let (function, _) = place(&scenario).await;

            let expected = match function.as_str() {
                "walk" => {
                    let step = walks;
                    walks += 1;
                    vec![
                        ("id", Integer(5)),
                        ("total", Integer((0..step).sum())),
                        ("iter", Other),
                        ("step", Integer(step)),
                    ]
                }
                "leaf" => {
                    let id = 3 + leaves;
                    leaves += 1;
                    vec![
                        ("id", Integer(id)),
                        ("doubled", Integer(id * 2)),
                        ("label", Other),
                        ("resumed", Integer(7)),
                    ]
                }
                "middle" => {
                    let id = 3 + middles;
                    middles += 1;
                    vec![
                        ("id", NotSaved("// AWAIT: middle")),
                        ("base", NotSaved("// AWAIT: middle")),
                        ("first", Integer(id + 11)),
                        ("second", Integer(id * 2 + 13)),
                    ]
                }
                other => panic!("{fixture}: stopped in {other}"),
            };

            check_variables(fixture, &function, &locals(&scenario).await, &expected);

            // The body's future, which the compiler passes it unnamed, is
            // `$future`.
            let future = evaluated(&scenario, "$future").await;
            assert_eq!(
                future
                    .type_info
                    .map(|info| info.name.to_string())
                    .as_deref(),
                Some("{async_fn_env#0}"),
                "{fixture} {function}"
            );
            assert!(
                matches!(future.state, VariableState::Available { .. }),
                "{fixture} {function}: {:?}",
                future.state
            );
            reason = scenario.resume_to_stop().await;
        }

        assert!(
            matches!(reason, StopReason::Exited { .. }),
            "{fixture}: {reason:?}"
        );

        assert_eq!((walks, leaves, middles), (3, 2, 2), "{fixture}");
        scenario.shutdown().await;
    }
}

/// Checks that a frame lists the variables `expected` says, in order where
/// nothing is optimized, each once, with no wrong value.
fn check_variables(
    fixture: &str,
    function: &str,
    listed: &[(String, Option<i128>, VariableState)],
    expected: &[(&str, Expected)],
) {
    if fixture.ends_with("o0") {
        assert_eq!(
            listed
                .iter()
                .map(|(name, _, _)| name.as_str())
                .collect::<Vec<_>>(),
            expected.iter().map(|(name, _)| *name).collect::<Vec<_>>(),
            "{fixture} {function}: {listed:#?}"
        );
    }

    // Optimization may leave a variable without a value, but never with a
    // wrong one, or list it twice.
    for (name, value, state) in listed {
        let Some((_, expected)) = expected.iter().find(|(known, _)| known == name) else {
            panic!("{fixture} {function}: {name} listed: {listed:#?}");
        };

        let unavailable = matches!(state, VariableState::Unavailable(_));
        let correct = match expected {
            Expected::Integer(truth) => *value == Some(*truth) || unavailable,
            Expected::Other => true,
            Expected::NotSaved(marker) => {
                matches!(
                    state,
                    VariableState::Unavailable(VariableUnavailableReason::NotSavedAcrossAwait {
                        line
                    }) if line.get() == crate::stops::line(SOURCE, marker)
                ) || (fixture.ends_with("o3") && unavailable)
            }
        };

        assert!(correct, "{fixture} {function}: {name} = {state:?}");
        assert_eq!(
            listed.iter().filter(|(other, _, _)| other == name).count(),
            1,
            "{fixture} {function}: {listed:#?}"
        );
    }
}

/// `step` on a line that awaits an async function's future stops at the
/// function's first line in one step, through the future's construction,
/// `IntoFuture`, `Pin`, and the dispatch on its state.
#[tokio::test]
async fn step_enters_an_awaited_async_function_at_its_first_line() {
    for fixture in BUILDS {
        for (marker, function, first) in [
            ("// STEP: middle-ready", "ready", "// STEP: ready"),
            ("// AWAIT: middle", "leaf", "// STEP: leaf"),
        ] {
            let mut scenario = stopped_at(fixture, at(marker)).await;
            let reason = scenario.step_to_stop(StepKind::IntoSource).await;
            assert_eq!(
                reason,
                StopReason::Step {
                    kind: StepKind::IntoSource
                },
                "{fixture} {marker}"
            );
            assert_eq!(
                place(&scenario).await,
                (function.to_owned(), line(SOURCE, first)),
                "{fixture} {marker}"
            );
            scenario.shutdown().await;
        }
    }
}

/// A future prints as the state it holds: where it waits, with what it
/// keeps there, never as the number that encodes the state. While its body
/// runs, the state is still the await the poll resumed from. Its children
/// are the variables it keeps, then the value as stored; the awaited
/// future, drop flags, and captures the body moved are only under `[raw]`.
#[tokio::test]
async fn a_future_prints_as_its_state() {
    for fixture in BUILDS {
        for (marker, expected, children) in [
            (
                "// STEP: leaf-after",
                "suspended at main.rs:43 {id: 3, doubled: 6, label: \"leaf 3\"}",
                &["id", "doubled", "label", "[raw]"][..],
            ),
            (
                "// STEP: middle-after",
                "suspended at main.rs:51 {first: 14}",
                &["first", "[raw]"],
            ),
        ] {
            let scenario = stopped_at(fixture, at(marker)).await;
            let future = evaluated(&scenario, "$future").await;
            assert_eq!(
                microscope::value_summary(future.type_info.as_ref(), &future.state),
                expected,
                "{fixture} {marker}"
            );

            let VariableState::Available {
                presentation: Some(presentation),
                ..
            } = &future.state
            else {
                panic!("{fixture} {marker}: {future:?}");
            };

            let ValueChildren::Available(reference) = &presentation.children else {
                panic!("{fixture} {marker}: {presentation:?}");
            };

            let page = scenario
                .operation(
                    "children",
                    scenario.handle().value_children(
                        std::sync::Arc::clone(reference),
                        ValueChildQuery {
                            offset: 0,
                            limit: 64,
                        },
                    ),
                )
                .await;

            let names = page
                .children
                .iter()
                .map(|child| match &child.relationship {
                    ValueChildRelationship::Member(member) => {
                        member.name.as_deref().unwrap_or("<anonymous>").to_owned()
                    }
                    ValueChildRelationship::Raw => "[raw]".to_owned(),
                    other => panic!("{fixture} {marker}: {other:?}"),
                })
                .collect::<Vec<_>>();
            assert_eq!(names, children, "{fixture} {marker}");
            scenario.shutdown().await;
        }
    }
}

/// `next` over an await whose future is pending waits for its own future
/// to be polled again, past the polls of the other task that runs the same
/// function meanwhile, and ends at the line after the await.
#[tokio::test]
async fn next_over_a_pending_await_ends_after_it_in_its_own_future() {
    for fixture in ["tokio-std-async-o0"] {
        let mut scenario = stopped_once(fixture, at("// AWAIT: leaf")).await;
        assert_eq!(integer(&scenario, "id").await, Some(3), "{fixture}");

        let reason = scenario.step_to_stop(StepKind::OverSource).await;
        assert_eq!(
            reason,
            StopReason::Step {
                kind: StepKind::OverSource
            },
            "{fixture}"
        );
        assert_eq!(
            place(&scenario).await,
            ("leaf".to_owned(), line(SOURCE, "// STEP: leaf-after")),
            "{fixture}"
        );
        assert_eq!(integer(&scenario, "doubled").await, Some(6), "{fixture}");
        assert_eq!(integer(&scenario, "id").await, Some(3), "{fixture}");
        assert_eq!(integer(&scenario, "resumed").await, Some(7), "{fixture}");
        scenario.shutdown().await;
    }
}

/// `next` over an await in a loop stops at each line its future reaches
/// as the loop goes round, however often the await is pending.
#[tokio::test]
async fn next_over_an_await_in_a_loop_goes_round_the_loop() {
    for fixture in ["tokio-std-async-o0"] {
        let mut scenario = stopped_once(fixture, at("// AWAIT: walk")).await;
        let mut lines = Vec::new();

        while lines.len() < 8 {
            let reason = scenario.step_to_stop(StepKind::OverSource).await;
            assert_eq!(
                reason,
                StopReason::Step {
                    kind: StepKind::OverSource
                },
                "{fixture}: {lines:?}"
            );
            let (function, line) = place(&scenario).await;
            assert_eq!(function, "walk", "{fixture}: {lines:?}");
            lines.push((line, integer(&scenario, "total").await));
        }

        let body = line(SOURCE, "// STEP: walk-body");
        let wait = line(SOURCE, "// AWAIT: walk");

        // Each pass adds the step to the total, then awaits again.
        let passes = lines
            .iter()
            .filter(|(line, _)| *line == wait)
            .map(|(_, total)| total.expect("the total is available"))
            .collect::<Vec<_>>();
        assert_eq!(passes, [1, 3], "{fixture}: {lines:?}");
        assert!(
            lines.iter().any(|(line, _)| *line == body),
            "{fixture}: {lines:?}"
        );
        scenario.shutdown().await;
    }
}

/// `finish` from an async function whose await is pending runs it to its
/// return, through every poll, and stops in its own awaiter.
#[tokio::test]
async fn finish_from_an_async_function_returns_to_its_own_awaiter() {
    for fixture in ["tokio-std-async-o0"] {
        let mut scenario = stopped_once(fixture, at("// STEP: label")).await;
        assert_eq!(integer(&scenario, "id").await, Some(3), "{fixture}");
        let reason = scenario.step_to_stop(StepKind::Out).await;
        assert_eq!(
            reason,
            StopReason::Step {
                kind: StepKind::Out
            },
            "{fixture}"
        );
        assert_eq!(
            place(&scenario).await,
            ("middle".to_owned(), line(SOURCE, "// AWAIT: middle")),
            "{fixture}"
        );

        // The awaiter is the one whose `ready(13)` returned 14.
        assert_eq!(integer(&scenario, "first").await, Some(14), "{fixture}");

        // Its last poll returned what the function did.
        let returned = scenario
            .operation("variables", scenario.handle().variables())
            .await
            .variables
            .iter()
            .filter(|variable| variable.kind == VariableKind::Returned)
            .map(|variable| format!("{:?}", variable.state))
            .collect::<Vec<_>>();
        assert!(
            matches!(&returned[..], [poll] if poll.contains("Ready") && !poll.contains("Pending")),
            "{fixture}: {returned:#?}"
        );
        scenario.shutdown().await;
    }
}
