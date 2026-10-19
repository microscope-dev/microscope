//! Steps through deferred calls: those a function returns through, those
//! the runtime calls for it, and those a panic runs as it unwinds.

use std::process::Stdio;

use microscope::{BreakpointSpec, LaunchOptions, LineNumber, StepKind, StopReason};

use crate::stops::{integer, place};
use crate::support::{self, Scenario};

const BUILDS: [&str; 2] = ["defers-go-o0", "defers-go-o2"];
const SOURCE: &str = "tests/fixtures/go/defers/main.go";

fn line(marker: &str) -> u64 {
    support::source_line(SOURCE, &format!("// DEFER: {marker}"))
}

/// A fixture stopped at a marked line, with no breakpoint left.
async fn stopped_at(fixture: &str, marker: &str) -> Scenario {
    let mut scenario = crate::invariants::checked(fixture);
    let breakpoint = scenario
        .add_breakpoint_spec(BreakpointSpec::Source {
            path: "defers/main.go".into(),
            line: LineNumber::new(line(marker)).expect("one-based"),
        })
        .await;

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

/// Takes each step, checking the function and marked line it ends at, all
/// in the goroutine the walk began in.
async fn walk(scenario: &mut Scenario, fixture: &str, steps: &[(StepKind, &str, &str)]) {
    let task = integer(scenario, "$task").await;

    for &(kind, function, marker) in steps {
        let reason = scenario.step_to_stop(kind).await;
        assert_eq!(reason, StopReason::Step { kind }, "{fixture}: to {marker}");
        assert_eq!(
            place(scenario).await,
            (function.to_owned(), line(marker)),
            "{fixture}: {kind:?} to {marker}"
        );
        assert_eq!(integer(scenario, "$task").await, task, "{fixture}");
    }
}

#[tokio::test]
async fn stepping_in_at_a_return_enters_its_deferred_calls() {
    use StepKind::{IntoSource, Out, OverSource};
    for fixture in BUILDS {
        let optimized = fixture.ends_with("o2");

        // Optimized, the compiler calls a function's few defers itself as
        // it returns; unoptimized, the runtime calls them at its closing
        // brace. Stepping out of one goes on through the runtime to the
        // program's next statement.
        let mut scenario = stopped_at(fixture, "direct return").await;
        let steps = if optimized {
            vec![
                (IntoSource, "main.cleanup", "cleanup body"),
                (Out, "main.main", "looped call"),
            ]
        } else {
            // The function then reloads its result on its return line.
            vec![
                (IntoSource, "main.direct", "direct end"),
                (IntoSource, "main.cleanup", "cleanup"),
                (Out, "main.direct", "direct return"),
            ]
        };

        walk(&mut scenario, fixture, &steps).await;
        scenario.shutdown().await;

        // The runtime calls a loop's defers, the last registered first,
        // and a step goes on from one into the next, then returns to the
        // function, which reloads its result on its return line.
        let mut scenario = stopped_at(fixture, "looped return").await;
        let (first, then) = if optimized {
            // A frameless function's first line is where a step into it
            // stops.
            (
                vec![
                    (IntoSource, "main.looped", "looped end"),
                    (IntoSource, "main.cleanup", "cleanup body"),
                ],
                vec![
                    (OverSource, "main.cleanup", "cleanup end"),
                    (OverSource, "main.cleanup", "cleanup body"),
                    (OverSource, "main.cleanup", "cleanup end"),
                ],
            )
        } else {
            (
                vec![
                    (IntoSource, "main.looped", "looped end"),
                    (IntoSource, "main.cleanup", "cleanup"),
                    (OverSource, "main.cleanup", "cleanup body"),
                ],
                vec![
                    (OverSource, "main.cleanup", "cleanup end"),
                    (OverSource, "main.cleanup", "cleanup"),
                    (OverSource, "main.cleanup", "cleanup body"),
                    (OverSource, "main.cleanup", "cleanup end"),
                ],
            )
        };

        walk(&mut scenario, fixture, &first).await;
        assert_eq!(integer(&scenario, "note").await, Some(1), "{fixture}");
        walk(&mut scenario, fixture, &then).await;
        assert_eq!(integer(&scenario, "note").await, Some(0), "{fixture}");
        walk(
            &mut scenario,
            fixture,
            &[
                (OverSource, "main.looped", "looped return"),
                (OverSource, "main.looped", "looped end"),
                (OverSource, "main.main", "rescue call"),
            ],
        )
        .await;
        scenario.shutdown().await;
    }
}

#[tokio::test]
async fn stepping_over_a_return_runs_its_deferred_calls() {
    for fixture in BUILDS {
        for (from, next) in [
            ("direct return", "looped call"),
            ("looped return", "rescue call"),
        ] {
            let mut scenario = stopped_at(fixture, from).await;

            // The runtime runs the deferred calls at the closing brace,
            // and the function then reloads its result on its return line;
            // optimized, the compiler calls a few defers itself instead.
            let mut steps = vec![(StepKind::OverSource, "main.main", next)];
            if fixture.ends_with("o0") || from == "looped return" {
                let (function, end) = if from == "direct return" {
                    ("main.direct", "direct end")
                } else {
                    ("main.looped", "looped end")
                };

                steps.splice(
                    0..0,
                    [
                        (StepKind::OverSource, function, end),
                        (StepKind::OverSource, function, from),
                        (StepKind::OverSource, function, end),
                    ],
                );
            }

            walk(&mut scenario, fixture, &steps).await;
            scenario.shutdown().await;
        }
    }
}

#[tokio::test]
async fn a_panic_stops_a_step_in_the_deferred_call_it_runs() {
    for fixture in BUILDS {
        for kind in [StepKind::OverSource, StepKind::Out] {
            let from = if kind == StepKind::Out {
                "panic"
            } else {
                "explode call"
            };

            let mut scenario = stopped_at(fixture, from).await;
            walk(
                &mut scenario,
                fixture,
                &[
                    (kind, "main.rescue.func1", "rescuer"),
                    (StepKind::OverSource, "main.rescue.func1", "recover"),
                    (StepKind::OverSource, "main.rescue.func1", "rescuer end"),
                    // The deferred call recovered, so its function returns.
                    (StepKind::OverSource, "main.main", "done"),
                ],
            )
            .await;
            scenario.shutdown().await;
        }
    }
}
