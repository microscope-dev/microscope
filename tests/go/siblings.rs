//! Goroutines that run the same function at once. A step stays with the
//! goroutine it began in, whichever thread the scheduler resumes it on,
//! whatever siblings run meanwhile on its thread, and wherever the runtime
//! copies its stack.

use std::collections::BTreeSet;
use std::process::Stdio;

use microscope::{ExecutionContext, LaunchOptions, LineNumber, StepKind, StopReason};

use crate::stops::{address, integer, place};
use crate::support::{self, Scenario};

const BUILDS: [&str; 2] = ["siblings-go-o0", "siblings-go-o2"];
const SOURCE: &str = "tests/fixtures/go/siblings/main.go";

/// The lines of the loop, in the order each round runs them.
const ROUND: [&str; 5] = [
    "// WALK: add",
    "// WALK: yield",
    "// WALK: deep",
    "// WALK: mix",
    "// WALK: loop",
];

/// How many steps each walk takes: enough rounds for the scheduler to move
/// the goroutine, and for its stack to grow more than once.
const STEPS: usize = 60;

/// A program stopped in one goroutine's loop, with no breakpoint left, and
/// the runtime given `processors` processors.
async fn in_the_loop(fixture: &str, processors: &str) -> Scenario {
    let mut scenario = crate::invariants::checked(fixture);
    let breakpoint = scenario
        .add_breakpoint_spec(microscope::BreakpointSpec::Source {
            path: "siblings/main.go".into(),
            line: LineNumber::new(support::source_line(SOURCE, ROUND[0])).expect("one-based"),
        })
        .await;

    let reason = scenario
        .run_with_to_stop(LaunchOptions {
            environment: vec![("GOMAXPROCS".into(), Some(processors.into()))],
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

#[tokio::test]
async fn a_step_stays_with_its_goroutine() {
    let round = ROUND.map(|marker| support::source_line(SOURCE, marker));
    for fixture in BUILDS {
        // One processor runs every sibling on the stepping thread; several
        // resume the goroutine wherever one is free.
        for processors in ["1", "4"] {
            let context = format!("{fixture} with {processors} processors");
            let mut scenario = in_the_loop(fixture, processors).await;
            let task = integer(&scenario, "$task").await.expect("a task");

            // The program's own name for the goroutine, where it is known.
            let id = integer(&scenario, "id").await;
            if fixture.ends_with("o0") {
                assert_eq!(id, Some(task), "{context}");
            }

            let mut threads = BTreeSet::new();
            let mut walked = Vec::new();

            for _ in 0..STEPS {
                let reason = scenario.step_to_stop(StepKind::OverSource).await;
                assert_eq!(
                    reason,
                    StopReason::Step {
                        kind: StepKind::OverSource
                    },
                    "{context}"
                );
                assert_eq!(integer(&scenario, "$task").await, Some(task), "{context}");
                if let (Some(id), Some(now)) = (id, integer(&scenario, "id").await) {
                    assert_eq!(now, id, "{context}: the program's own id changed");
                }

                let (function, line) = place(&scenario).await;
                assert_eq!(function, "main.work", "{context}: line {line}");
                walked.push(line);
                if let Some(ExecutionContext::Thread(thread)) = scenario.snapshot().await.selected {
                    threads.insert(thread);
                }
            }

            // Unoptimized, the walk visits every line of each round in
            // order; optimized code may visit them in another.
            if fixture.ends_with("o0") {
                let expected = round.iter().copied().cycle().skip(1).take(STEPS);
                assert_eq!(walked, expected.collect::<Vec<_>>(), "{context}");
            } else {
                assert!(
                    walked.iter().all(|line| round.contains(line)),
                    "{context}: {walked:?}"
                );
            }

            eprintln!("{context}: the goroutine ran on threads {threads:?}");

            // Every sibling returns to the same place, and only this
            // goroutine's return ends the step.
            let reason = scenario.step_to_stop(StepKind::Out).await;
            assert_eq!(
                reason,
                StopReason::Step {
                    kind: StepKind::Out
                },
                "{context}"
            );

            assert_eq!(integer(&scenario, "$task").await, Some(task), "{context}");
            let (function, _) = place(&scenario).await;
            assert_eq!(function, "main.main.func1", "{context}");
            scenario.shutdown().await;
        }
    }
}

#[tokio::test]
async fn finish_returns_to_its_caller_after_the_stack_moves() {
    let recurse = support::source_line(SOURCE, "// recurse");
    for fixture in BUILDS {
        let mut scenario = crate::invariants::checked(fixture);

        // Each goroutine first reaches this depth in its first deep call,
        // whose deeper calls then grow its stack again.
        let breakpoint = scenario
            .operation(
                "conditional breakpoint",
                scenario.handle().add_breakpoint_with(
                    microscope::BreakpointSpec::Function("main.deep".into()),
                    microscope::BreakpointOptions {
                        condition: Some(
                            microscope::Condition::parse("depth == 16").expect("parses"),
                        ),
                        ..microscope::BreakpointOptions::default()
                    },
                ),
            )
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

        let task = integer(&scenario, "$task").await.expect("a task");
        let trace = scenario
            .operation("backtrace", scenario.handle().backtrace())
            .await;
        let caller = trace.frames.get(1).expect("a caller").id;

        scenario
            .operation("select the caller", scenario.handle().select_frame(caller))
            .await;
        let before = address(&scenario, "&pad").await;
        let innermost = trace.frames[0].id;
        scenario
            .operation(
                "select the innermost frame",
                scenario.handle().select_frame(innermost),
            )
            .await;

        let reason = scenario.step_to_stop(StepKind::Out).await;
        assert_eq!(
            reason,
            StopReason::Step {
                kind: StepKind::Out
            },
            "{fixture}"
        );
        assert_eq!(integer(&scenario, "$task").await, Some(task), "{fixture}");
        assert_eq!(place(&scenario).await, ("main.deep".to_owned(), recurse));
        if let Some(depth) = integer(&scenario, "depth").await {
            assert_eq!(depth, 17, "{fixture}");
        }

        // The caller's frame is where the runtime copied it.
        if fixture.ends_with("o0") {
            let after = address(&scenario, "&pad").await;
            assert!(before.is_some() && after.is_some(), "{fixture}");
            assert_ne!(after, before, "{fixture}: the stack never moved");
        }

        scenario.shutdown().await;
    }
}

#[tokio::test]
async fn stepping_through_the_scheduler_returns_to_the_same_goroutine() {
    for fixture in BUILDS {
        let mut scenario = crate::invariants::checked(fixture);
        let breakpoint = scenario.add_breakpoint("runtime.Gosched").await;
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
        let task = integer(&scenario, "$task").await.expect("a task");

        // A step begun in the runtime may stop there, but it passes over
        // the switch to the scheduler's stack, which parks the goroutine,
        // and goes on where the goroutine resumes, back to the program.
        let mut visited = Vec::new();

        loop {
            assert!(visited.len() < 200, "{fixture}: {visited:?}");
            let reason = scenario.step_to_stop(StepKind::IntoSource).await;
            assert_eq!(
                reason,
                StopReason::Step {
                    kind: StepKind::IntoSource
                },
                "{fixture}: after {visited:?}"
            );
            assert_eq!(integer(&scenario, "$task").await, Some(task), "{fixture}");
            let (function, _) = place(&scenario).await;

            // Optimized, the first statement may be in the next call.
            if function.starts_with("main.") {
                break;
            }

            visited.push(function);
        }

        assert!(
            visited
                .iter()
                .all(|function| function.starts_with("runtime.")),
            "{fixture}: {visited:?}"
        );
        scenario.shutdown().await;
    }
}
