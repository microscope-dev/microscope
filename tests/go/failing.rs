//! Programs that fail, debugged from start to finish. Each case runs once
//! on its own first, and the debugged run must end the same way: with the
//! same exit status, and with the message the runtime printed. Where the
//! runtime reports a panic nothing recovers, or a fatal error, the session
//! stops there first, with the runtime's message and the frame at fault
//! selected.

use std::process::{Command, Stdio};

use microscope::{
    Evaluation, ExceptionStops, ExitStatus, Expression, LanguageExceptionKind, LaunchOptions,
    StopReason, TaskSnapshot, UnwindTermination, VariableState,
};

use crate::stops::{integer, place};
use crate::support::{self, Scenario, ScratchDir};

const BUILDS: [&str; 2] = ["failing-go-o0", "failing-go-o2"];
const SOURCE: &str = "tests/fixtures/go/failing/main.go";

/// Where a case stops first.
#[derive(Clone, Copy)]
enum Expected {
    /// A panic nothing recovered, with the frame selected that raised it,
    /// and the line marked in the source.
    Unhandled(&'static str, &'static str),

    /// A fatal error.
    Fatal,

    /// The program's own breakpoint instruction.
    ProgramBreakpoint,

    /// No stop: the program ends as it does alone.
    Nothing,
}

const CASES: [(&str, Expected); 18] = [
    (
        "nil-map",
        Expected::Unhandled("main.writeNilMap", "nil map"),
    ),
    // The fault's frame is selected, below the runtime's signal panic.
    (
        "nil-dereference",
        Expected::Unhandled("main.dereference", "dereference"),
    ),
    ("recovered", Expected::Nothing),
    ("index", Expected::Unhandled("main.index", "index")),
    // Errors and stringers print as their text, other values as the
    // runtime prints them.
    ("wrapped", Expected::Unhandled("main.raise", "raise")),
    ("stringer", Expected::Unhandled("main.raise", "raise")),
    ("float", Expected::Unhandled("main.raise", "raise")),
    ("custom", Expected::Unhandled("main.raise", "raise")),
    ("custom-float", Expected::Unhandled("main.raise", "raise")),
    ("goroutine", Expected::Unhandled("main.raise", "raise")),
    // A panic in a deferred call during a panic, and one raised again
    // after it was recovered, print the panics before it.
    ("nested", Expected::Unhandled("main.raise", "raise")),
    ("repanic", Expected::Unhandled("main.raise", "raise")),
    ("deadlock", Expected::Fatal),
    ("goexit", Expected::Fatal),
    ("unlock", Expected::Fatal),
    ("overflow", Expected::Fatal),
    ("breakpoint", Expected::ProgramBreakpoint),
    ("exit", Expected::Nothing),
];

/// How a case ends when it runs alone: its exit code, standard output,
/// and standard error.
fn alone(fixture: &str, case: &str) -> (Option<i32>, String, String) {
    let output = Command::new(Scenario::fixture(fixture))
        .arg(case)
        .stdin(Stdio::null())
        .output()
        .expect("run the fixture alone");
    (
        output.status.code(),
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

/// The task numbered `number`, if it is listed.
async fn listed(scenario: &Scenario, number: u64) -> Option<TaskSnapshot> {
    let page = scenario
        .operation("tasks", scenario.handle().tasks(None, 64))
        .await;
    page.tasks
        .iter()
        .find(|task| task.id.number == number)
        .cloned()
}

#[tokio::test]
async fn failing_programs_stop_where_the_runtime_reports_them() {
    for fixture in BUILDS {
        // One session launches every case in turn, since loading the
        // build's debug information is most of what a case costs.
        let mut scenario = crate::invariants::checked(fixture);
        for (case, expected) in CASES {
            let context = format!("{fixture} {case}");
            let (status, output, printed) = alone(fixture, case);

            let scratch = ScratchDir::new("failing");
            let errors = scratch.path().join("stderr");
            let out = scratch.path().join("stdout");
            let reason = scenario
                .run_with_to_stop(LaunchOptions {
                    arguments: vec![case.into()],
                    stdout: Some(Stdio::from(
                        std::fs::File::create(&out).expect("create standard output"),
                    )),
                    stderr: Some(Stdio::from(
                        std::fs::File::create(&errors).expect("create standard error"),
                    )),
                    ..LaunchOptions::default()
                })
                .await;

            let reason = match expected {
                Expected::Nothing => reason,
                Expected::ProgramBreakpoint => {
                    assert!(
                        matches!(reason, StopReason::ProgramBreakpoint { .. }),
                        "{context}: {reason:?}"
                    );
                    scenario.resume_to_stop().await
                }
                Expected::Unhandled(..) | Expected::Fatal => {
                    let StopReason::LanguageException(exception) = &reason else {
                        panic!("{context}: {reason:?}");
                    };

                    // The runtime prints the same message, which for a
                    // fault goes on with the signal that raised it.
                    assert!(
                        printed.contains(exception.message.as_ref()),
                        "{context}: {:?} is not in {printed}",
                        exception.message
                    );

                    if let Expected::Unhandled(function, marker) = expected {
                        assert_eq!(exception.kind, LanguageExceptionKind::Unhandled);
                        let line = support::source_line(SOURCE, &format!("// FAIL: {marker}"));
                        assert_eq!(
                            place(&scenario).await,
                            (function.to_owned(), line),
                            "{context}"
                        );

                        // The value the program panicked with can be shown.
                        let text = exception.value.as_deref().expect("a panic's value");
                        let expression = Expression::parse(text).expect("an expression");
                        let Evaluation::Value { value, .. } = scenario
                            .operation(text, scenario.handle().evaluate(&expression))
                            .await
                        else {
                            panic!("{context}: {text} is not a value");
                        };

                        assert!(
                            matches!(value.state, VariableState::Available { .. }),
                            "{context}: {text} is {:?}",
                            value.state
                        );
                        let main = if case == "goroutine" { None } else { Some(1) };
                        let task = integer(&scenario, "$task").await;
                        assert_eq!(task == Some(1), main.is_some(), "{context}: {task:?}");
                    } else {
                        assert_eq!(exception.kind, LanguageExceptionKind::Fatal);
                    }

                    check_case(&scenario, case, &context).await;

                    // Go's checkdead unlocks the scheduler before it calls
                    // fatal, and nothing marks the program panicking until
                    // fatal runs on, so another thread going idle meanwhile
                    // may report the same deadlock; each report stops.
                    let checkdead = matches!(case, "deadlock" | "goexit");
                    let mut next = scenario.resume_to_stop().await;
                    while checkdead
                        && matches!(&next, StopReason::LanguageException(again)
                            if again.kind == LanguageExceptionKind::Fatal
                                && again.message == exception.message)
                    {
                        next = scenario.resume_to_stop().await;
                    }

                    next
                }
            };

            let StopReason::Exited(ExitStatus::Code(code)) = reason else {
                panic!("{context}: {reason:?}");
            };

            // A program's own breakpoint kills it when no debugger runs it,
            // and runs on past it when one does.
            let debugged = std::fs::read_to_string(&errors).expect("read standard error");
            if matches!(expected, Expected::ProgramBreakpoint) {
                assert_eq!(code, 0, "{context}");
                assert_eq!(debugged, "", "{context}");
            } else {
                assert_eq!(Some(code), status.map(i64::from), "{context}");

                // What the program printed, as a recovered panic's handler,
                // or nothing from the deferred calls `os.Exit` skips.
                let shown = std::fs::read_to_string(&out).expect("read standard output");
                assert_eq!(shown, output, "{context}");
                assert_eq!(
                    debugged.lines().next(),
                    printed.lines().next(),
                    "{context}: {debugged}"
                );
            }
        }

        scenario.shutdown().await;
    }
}

/// What holds at a case's stop beyond its message and frame.
async fn check_case(scenario: &Scenario, case: &str, context: &str) {
    match case {
        // The other goroutines are listed.
        "goroutine" => {
            assert!(listed(scenario, 1).await.is_some(), "{context}");
        }

        // Each says what it waits for.
        "deadlock" => assert_eq!(
            listed(scenario, 1)
                .await
                .and_then(|task| task.detail)
                .as_deref(),
            Some("select (no cases)"),
            "{context}"
        ),
        // The overflowing stack is deeper than a backtrace goes, which
        // says so.
        "overflow" => {
            let trace = scenario
                .operation("backtrace", scenario.handle().backtrace())
                .await;
            assert_eq!(
                trace.termination,
                UnwindTermination::DepthLimit,
                "{context}"
            );
        }
        _ => {}
    }
}

/// A case launched with these exceptions stopping, to its first stop.
async fn launched(fixture: &str, case: &str, stops: ExceptionStops) -> (Scenario, StopReason) {
    let mut scenario = crate::invariants::checked(fixture);
    scenario
        .operation(
            "exception stops",
            scenario.handle().set_exception_stops(stops),
        )
        .await;

    let reason = scenario
        .run_with_to_stop(LaunchOptions {
            arguments: vec![case.into()],
            stdout: Some(Stdio::null()),
            stderr: Some(Stdio::null()),
            ..LaunchOptions::default()
        })
        .await;
    (scenario, reason)
}

fn exception(reason: &StopReason, context: &str) -> (LanguageExceptionKind, String) {
    let StopReason::LanguageException(exception) = reason else {
        panic!("{context}: {reason:?}");
    };

    (exception.kind, exception.message.to_string())
}

#[tokio::test]
async fn exceptions_stop_as_chosen() {
    let raised = ExceptionStops::default()
        .with("raised", true)
        .expect("Go declares its panics");
    for fixture in BUILDS {
        // Each panic stops as it is raised, in the frame that raised it,
        // the one the program recovers from too. The runtime's own errors
        // read as their methods would put them.
        let (mut scenario, reason) = launched(fixture, "recovered", raised).await;
        let (kind, message) = exception(&reason, fixture);
        assert_eq!(kind, LanguageExceptionKind::Raised, "{fixture}");
        assert_eq!(
            message, "panic: runtime error: invalid memory address or nil pointer dereference",
            "{fixture}"
        );

        let line = support::source_line(SOURCE, "// FAIL: dereference");
        assert_eq!(
            place(&scenario).await,
            ("main.dereference".to_owned(), line)
        );

        let reason = scenario.resume_to_stop().await;
        assert_eq!(reason, StopReason::Exited(ExitStatus::Code(0)), "{fixture}");
        scenario.shutdown().await;

        // One nothing recovers stops again as it ends the program.
        let (mut scenario, reason) = launched(fixture, "float", raised).await;
        let first = exception(&reason, fixture);
        assert_eq!(
            first,
            (LanguageExceptionKind::Raised, "panic: 1.5".to_owned())
        );

        let reason = scenario.resume_to_stop().await;
        let last = exception(&reason, fixture);
        assert_eq!(
            last,
            (LanguageExceptionKind::Unhandled, "panic: 1.5".to_owned())
        );

        let reason = scenario.resume_to_stop().await;
        assert_eq!(reason, StopReason::Exited(ExitStatus::Code(2)), "{fixture}");
        scenario.shutdown().await;

        // With none chosen, a fault the runtime turns into a panic neither
        // stops for its signal nor for the panic.
        let none = ExceptionStops::NONE;
        let (scenario, reason) = launched(fixture, "nil-dereference", none).await;
        assert_eq!(reason, StopReason::Exited(ExitStatus::Code(2)), "{fixture}");
        scenario.shutdown().await;
    }
}

/// The runtime preempts a goroutine through the stack check that begins
/// most functions: it sets the goroutine's stack guard so the check fails,
/// and the goroutine yields, to run the function again from its first
/// instruction, perhaps on another thread. A panic preempted as it begins
/// is still one panic, which stops once.
#[tokio::test]
async fn a_panic_preempted_as_it_begins_stops_once() {
    let raised = ExceptionStops::default()
        .with("raised", true)
        .expect("Go declares its panics");
    for fixture in BUILDS {
        let (mut scenario, reason) = launched(fixture, "recovered", raised).await;
        let (kind, _) = exception(&reason, fixture);
        assert_eq!(kind, LanguageExceptionKind::Raised, "{fixture}");

        // In the runtime's frame, r14 holds the goroutine, whose stack
        // guard the check compares the stack pointer with at 16(R14).
        let trace = scenario
            .operation("backtrace", scenario.handle().backtrace())
            .await;

        scenario
            .operation(
                "select the runtime's frame",
                scenario.handle().select_frame(trace.frames[0].id),
            )
            .await;

        let g = integer(&scenario, "$r14").await.expect("the goroutine");
        let guard = u64::try_from(g).expect("an address") + 16;

        // `stackPreempt`, which always fails the check.
        scenario
            .operation(
                "request preemption",
                scenario.handle().write_word(
                    microscope::VirtualAddress::new(guard),
                    0xffff_ffff_ffff_fade,
                ),
            )
            .await;
        let reason = scenario.resume_to_stop().await;
        assert_eq!(reason, StopReason::Exited(ExitStatus::Code(0)), "{fixture}");
        scenario.shutdown().await;
    }
}
