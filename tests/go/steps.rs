//! Stepping in stops only in code the program wrote. It passes over the
//! runtime's own machinery, the wrappers the compiler generates, and the
//! stack check that grows a goroutine's stack, staying in its goroutine.

use std::process::Stdio;

use microscope::{BreakpointSpec, LaunchOptions, LineNumber, StepKind, StopReason};

use crate::stops::{integer, place};
use crate::support::{self, Scenario};

const SOURCE: &str = "tests/fixtures/go/steps/main.go";

fn line(marker: &str) -> u64 {
    support::source_line(SOURCE, marker)
}

/// A fixture stopped at the first line of `main.run`'s walk, with no
/// breakpoint left, and the goroutine it runs in.
async fn at_the_walk(fixture: &str) -> (Scenario, i128) {
    stopped_at(fixture, "// STEP: map").await
}

/// A fixture stopped at the line `marker` ends, with no breakpoint left,
/// and the goroutine it runs in.
async fn stopped_at(fixture: &str, marker: &str) -> (Scenario, i128) {
    let mut scenario = crate::invariants::checked(fixture);
    let breakpoint = scenario
        .add_breakpoint_spec(BreakpointSpec::Source {
            path: "steps/main.go".into(),
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
    let task = integer(&scenario, "$task").await.expect("a task");
    (scenario, task)
}

#[tokio::test]
async fn stepping_in_stops_only_in_code_the_program_wrote() {
    let fixture = "steps-go-o0";
    let (mut scenario, task) = at_the_walk(fixture).await;

    // Each step, and the function and line it ends at.
    let walk = [
        // Past the runtime's map assignment.
        (StepKind::IntoSource, "main.run", "// STEP: list"),
        (StepKind::IntoSource, "main.run", "// STEP: append"),
        // Past the runtime's growing of the slice.
        (StepKind::IntoSource, "main.run", "// STEP: shape"),
        (StepKind::IntoSource, "main.run", "// STEP: interface"),
        // Through the wrapper the interface's method table calls.
        (StepKind::IntoSource, "main.square.area", "// STEP: area"),
        (StepKind::Out, "main.run", "// STEP: interface"),
        // A `go` statement starts its goroutine in the runtime.
        (StepKind::IntoSource, "main.run", "// STEP: go"),
        (StepKind::IntoSource, "main.run", "// STEP: grow"),
        // Into a function whose stack check grows the stack first.
        (StepKind::IntoSource, "main.fresh", "// STEP: fresh"),
        (StepKind::Out, "main.run", "// STEP: grow"),
        (StepKind::IntoSource, "main.run", "// STEP: after"),
    ];
    for (kind, function, marker) in walk {
        let reason = scenario.step_to_stop(kind).await;
        assert_eq!(reason, StopReason::Step { kind }, "{fixture}: to {marker}");
        assert_eq!(
            place(&scenario).await,
            (function.to_owned(), line(marker)),
            "{fixture}: {kind:?} to {marker}"
        );
        assert_eq!(
            integer(&scenario, "$task").await,
            Some(task),
            "{fixture}: {marker}"
        );
    }

    scenario.shutdown().await;
}

#[tokio::test]
async fn optimized_steps_stop_only_in_code_the_program_wrote() {
    let fixture = "steps-go-o2";
    let (mut scenario, task) = at_the_walk(fixture).await;
    let after = line("// STEP: after");
    let mut visited = Vec::new();

    loop {
        assert!(visited.len() < 40, "{fixture}: {visited:?}");
        let reason = scenario.step_to_stop(StepKind::IntoSource).await;
        assert_eq!(
            reason,
            StopReason::Step {
                kind: StepKind::IntoSource
            },
            "{fixture}: after {visited:?}"
        );
        let (function, line) = place(&scenario).await;
        assert!(
            ["main.run", "main.square.area", "main.fresh"].contains(&function.as_str()),
            "{fixture}: {function}:{line} after {visited:?}"
        );
        assert_eq!(integer(&scenario, "$task").await, Some(task), "{fixture}");
        visited.push((function, line));

        if visited.last().is_some_and(|(_, line)| *line == after) {
            break;
        }
    }

    for function in ["main.square.area", "main.fresh"] {
        assert!(
            visited.iter().any(|(name, _)| name == function),
            "{fixture}: {visited:?}"
        );
    }

    scenario.shutdown().await;
}

/// A function breakpoint stops once per call, even when the call's stack
/// check grows the stack and the function then begins again.
#[tokio::test]
async fn a_function_breakpoint_stops_once_for_a_call_that_grows_the_stack() {
    for fixture in ["steps-go-o0", "steps-go-o2"] {
        let mut scenario = crate::invariants::checked(fixture);
        let fresh = scenario.add_breakpoint("main.fresh").await;
        let after = scenario
            .add_source_breakpoint("steps/main.go", line("// STEP: after"))
            .await;
        let stopped_by = |reason: &StopReason| match reason {
            StopReason::Breakpoint { hits, .. } => {
                hits.iter().map(|hit| hit.breakpoint).collect::<Vec<_>>()
            }
            reason => panic!("{fixture}: {reason:?}"),
        };

        let reason = scenario
            .run_with_to_stop(LaunchOptions {
                stdout: Some(Stdio::null()),
                ..LaunchOptions::default()
            })
            .await;
        assert_eq!(stopped_by(&reason), [fresh.id], "{fixture}");

        let task = integer(&scenario, "$task").await;
        let reason = scenario.resume_to_stop().await;
        assert_eq!(stopped_by(&reason), [after.id], "{fixture}");
        assert_eq!(integer(&scenario, "$task").await, task, "{fixture}");
        scenario.shutdown().await;
    }
}

/// A step into the goroutine a line starts stops where the function the
/// `go` statement names begins, on the new goroutine: through the wrapper
/// that passes the call's arguments, or at once for a closure without
/// any. On a line that starts none, it ends as a step over does.
#[tokio::test]
async fn stepping_into_a_new_goroutine_stops_where_it_begins() {
    let kind = StepKind::IntoNewTask;
    let starts = [
        (
            "// STEP: go",
            "main.spawned",
            "// STEP: spawned",
            "// STEP: send",
        ),
        (
            "// STEP: start",
            "main.main.func1",
            "// STEP: start",
            "// STEP: body",
        ),
    ];
    for fixture in ["steps-go-o0", "steps-go-o2"] {
        for (statement, function, declaration, body) in starts {
            let context = format!("{fixture} from {statement}");
            let (mut scenario, task) = stopped_at(fixture, statement).await;
            assert_eq!(
                scenario.step_to_stop(kind).await,
                StopReason::Step { kind },
                "{context}"
            );
            let (stopped, at) = place(&scenario).await;
            assert_eq!(stopped, function, "{context}");

            // Optimized, the prologue may end on the body's first line.
            assert!(
                [line(declaration), line(body)].contains(&at),
                "{context}: line {at}"
            );
            let started = integer(&scenario, "$task").await.expect("a goroutine");
            assert_ne!(started, task, "{context}");

            // The step belongs to the new goroutine from then on.
            scenario.step_to_stop(StepKind::OverSource).await;
            assert_eq!(
                integer(&scenario, "$task").await,
                Some(started),
                "{context}"
            );
            scenario.shutdown().await;
        }

        let (mut scenario, task) = stopped_at(fixture, "// STEP: grow").await;
        assert_eq!(
            scenario.step_to_stop(kind).await,
            StopReason::Step { kind },
            "{fixture}"
        );
        assert_eq!(
            place(&scenario).await,
            ("main.run".to_owned(), line("// STEP: after")),
            "{fixture}"
        );
        assert_eq!(integer(&scenario, "$task").await, Some(task), "{fixture}");
        scenario.shutdown().await;
    }
}
