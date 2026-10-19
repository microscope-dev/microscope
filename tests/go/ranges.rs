//! Loops over iterator functions, whose bodies are functions the iterator
//! calls. A step over or out of a body treats it as its loop's own code:
//! `next` goes from one pass of the body to the next and on past the loop,
//! and `finish` runs the rest of the loop, never stopping in the iterator.

use std::process::Stdio;

use microscope::{BreakpointSpec, LaunchOptions, LineNumber, StepKind, StopReason};

use crate::stops::{integer, place};
use crate::support::{self, Scenario};

const BUILDS: [&str; 2] = ["ranges-go-o0", "ranges-go-o2"];
const SOURCE: &str = "tests/fixtures/go/ranges/main.go";

/// A program stopped at the first pass of a loop's body, at `marker`.
async fn in_the_body(fixture: &str, marker: &str) -> Scenario {
    let mut scenario = crate::invariants::checked(fixture);
    let breakpoint = scenario
        .add_breakpoint_spec(BreakpointSpec::Source {
            path: "ranges/main.go".into(),
            line: LineNumber::new(support::source_line(SOURCE, marker)).expect("one-based"),
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

/// Steps over until the program leaves `functions`, and returns each stop's
/// line, with the body's `v` where the program's debug information keeps
/// it.
async fn walk(
    scenario: &mut Scenario,
    functions: &[&str],
    context: &str,
) -> Vec<(u64, Option<i128>)> {
    let mut walked = Vec::new();

    loop {
        assert!(walked.len() < 64, "{context}: {walked:?}");
        let reason = scenario.step_to_stop(StepKind::OverSource).await;
        assert_eq!(
            reason,
            StopReason::Step {
                kind: StepKind::OverSource
            },
            "{context}: after {walked:?}"
        );
        let (function, line) = place(scenario).await;
        assert!(
            functions.contains(&function.as_str()),
            "{context}: stopped in {function} at line {line} after {walked:?}"
        );

        let body = function.contains("-range");
        walked.push((
            line,
            if body {
                integer(scenario, "v").await
            } else {
                None
            },
        ));

        if !body {
            return walked;
        }
    }
}

/// Checks a walk against the markers of the lines it must visit, each with
/// the value of `v` there; `v` may be unavailable, but never wrong.
fn assert_walk(walked: &[(u64, Option<i128>)], expected: &[(&str, i128)], context: &str) {
    let lines = walked.iter().map(|(line, _)| *line).collect::<Vec<_>>();
    let expected_lines = expected
        .iter()
        .map(|(marker, _)| support::source_line(SOURCE, &format!("// WALK: {marker}")))
        .collect::<Vec<_>>();
    assert_eq!(lines, expected_lines, "{context}: {walked:?}");

    for ((_, shown), (marker, value)) in walked.iter().zip(expected) {
        if let Some(shown) = shown {
            assert_eq!(shown, value, "{context}: v at {marker}");
        }
    }
}

#[tokio::test]
async fn stepping_over_a_loop_body_stays_in_the_loop() {
    // Each pass of the body begins on the loop's line, as any loop's does;
    // `break` leaves the loop for the line after it.
    let counted = [
        ("counted check", 0),
        ("counted end", 0),
        ("counted loop", 1),
        ("counted add", 1),
        ("counted check", 1),
        ("counted end", 1),
        ("counted loop", 2),
        ("counted add", 2),
        ("counted check", 2),
        ("counted end", 2),
        ("counted loop", 3),
        ("counted add", 3),
        ("counted check", 3),
        ("counted break", 3),
        ("counted after", 0),
    ];

    // Inlined with its iterator, a body has no code on the loop's line.
    let evens = [
        ("evens end", 0),
        ("evens loop", 2),
        ("evens add", 2),
        ("evens end", 2),
        ("evens loop", 4),
        ("evens add", 4),
        ("evens end", 4),
        ("evens after", 0),
    ];
    let inlined_evens = [("evens add", 2), ("evens add", 4), ("evens after", 0)];

    for fixture in BUILDS {
        let mut scenario = in_the_body(fixture, "// WALK: counted add").await;
        let walked = walk(
            &mut scenario,
            &["main.counted", "main.counted-range1"],
            fixture,
        )
        .await;
        assert_walk(&walked, &counted, fixture);
        scenario.shutdown().await;

        let mut scenario = in_the_body(fixture, "// WALK: evens add").await;
        let walked = walk(&mut scenario, &["main.evens", "main.evens-range1"], fixture).await;

        let expected = if fixture.ends_with("o0") {
            &evens[..]
        } else {
            &inlined_evens[..]
        };

        assert_walk(&walked, expected, fixture);
        scenario.shutdown().await;
    }
}

#[tokio::test]
async fn stepping_over_a_loop_enters_its_body() {
    let line = |marker: &str| support::source_line(SOURCE, marker);
    for fixture in BUILDS {
        for (function, body, marker, first) in [
            (
                "main.counted",
                "main.counted-range1",
                "// WALK: counted loop",
                "// WALK: counted add",
            ),
            (
                "main.evens",
                "main.evens-range1",
                "// WALK: evens loop",
                "// WALK: evens add",
            ),
        ] {
            let context = format!("{fixture} {function}");
            let mut scenario = crate::invariants::checked(fixture);
            let breakpoint = scenario.add_breakpoint(function).await;

            scenario
                .run_with_to_stop(LaunchOptions {
                    stdout: Some(Stdio::null()),
                    ..LaunchOptions::default()
                })
                .await;

            scenario.remove_breakpoint(breakpoint.id).await;

            // To the loop's line in the function, then into its body.
            loop {
                let (here, at) = place(&scenario).await;
                assert_eq!(here, function, "{context}: at line {at}");
                if at == line(marker) {
                    break;
                }

                assert!(
                    at < line(marker),
                    "{context}: stepped past the loop to {at}"
                );
                let reason = scenario.step_to_stop(StepKind::OverSource).await;
                assert_eq!(reason, over(), "{context}");
            }

            let reason = scenario.step_to_stop(StepKind::OverSource).await;
            assert_eq!(reason, over(), "{context}");
            assert_eq!(
                place(&scenario).await,
                (body.to_owned(), line(first)),
                "{context}"
            );
            scenario.shutdown().await;
        }
    }
}

#[tokio::test]
async fn finishing_a_loop_body_runs_the_rest_of_the_loop() {
    let line = |marker: &str| support::source_line(SOURCE, marker);
    for fixture in BUILDS {
        for (function, marker, after) in [
            ("main.counted", "counted add", "counted after"),
            ("main.evens", "evens add", "evens after"),
        ] {
            let prefix = function.trim_start_matches("main.");
            let loop_lines = [
                line(&format!("// WALK: {prefix} loop")),
                line(&format!("// WALK: {prefix} end")),
            ];
            let (marker, after) = (format!("// WALK: {marker}"), format!("// WALK: {after}"));
            let context = format!("{fixture} {function}");
            let mut scenario = in_the_body(fixture, &marker).await;
            let reason = scenario.step_to_stop(StepKind::Out).await;
            assert_eq!(
                reason,
                StopReason::Step {
                    kind: StepKind::Out
                },
                "{context}"
            );

            // The loop is done. The step stops where the function goes on
            // after the call that ran it, on the loop's lines, or at the
            // line after the loop when the loop was inlined; the next line
            // is the one after the loop.
            let (here, at) = place(&scenario).await;
            assert_eq!(here, function, "{context}: at line {at}");

            if at != line(&after) {
                assert!(loop_lines.contains(&at), "{context}: at line {at}");
                let reason = scenario.step_to_stop(StepKind::OverSource).await;
                assert_eq!(reason, over(), "{context}");
                assert_eq!(
                    place(&scenario).await,
                    (function.to_owned(), line(&after)),
                    "{context}"
                );
            }

            scenario.shutdown().await;
        }
    }
}

const fn over() -> StopReason {
    StopReason::Step {
        kind: StepKind::OverSource,
    }
}

#[tokio::test]
async fn a_backtrace_marks_the_iterators_between_a_body_and_its_loop() {
    for fixture in BUILDS {
        for (marker, function, iterator) in [
            ("// WALK: counted add", "main.counted", "main.Count.func1"),
            ("// WALK: evens add", "main.evens", "main.Evens.func1"),
        ] {
            let context = format!("{fixture} {function}");
            let scenario = in_the_body(fixture, marker).await;

            let trace = scenario
                .operation("backtrace", scenario.handle().backtrace())
                .await;
            let names = trace
                .frames
                .iter()
                .map(|frame| frame.function.as_ref().map(|function| &*function.name))
                .collect::<Vec<_>>();
            assert_eq!(
                names.get(..3),
                Some(
                    &[
                        Some(format!("{function}-range1").as_str()),
                        Some(iterator),
                        Some(function),
                    ][..]
                ),
                "{context}"
            );

            // The iterator runs the loop of the frame that holds it; nothing
            // else iterates a loop.
            let mut expected = vec![None; trace.frames.len()];
            expected[1] = Some(2);
            assert_eq!(trace.loop_iterators(), expected, "{context}");

            // Unoptimized, the variables the body uses from its loop's
            // function are its own.
            if fixture.ends_with("o0") {
                assert!(integer(&scenario, "total").await.is_some(), "{context}");
            }

            scenario.shutdown().await;
        }
    }
}
