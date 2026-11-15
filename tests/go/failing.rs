use std::process::{Command, Stdio};

use microscope::{
    Evaluation, ExceptionStops, ExitStatus, Expression, LanguageExceptionKind, LaunchOptions,
    StopReason, TaskSnapshot, UnwindTermination, VariableState,
};

use crate::stops::{integer, place};
use crate::support::{self, Scenario, ScratchDir};

const BUILDS: [&str; 2] = ["failing-go-o0", "failing-go-o2"];
const SOURCE: &str = "tests/fixtures/go/failing/main.go";

#[derive(Clone, Copy)]
enum Expected {
    Unhandled(&'static str, &'static str),

    Fatal,

    ProgramBreakpoint,

    Nothing,
}

const CASES: [(&str, Expected); 18] = [
    (
        "nil-map",
        Expected::Unhandled("main.writeNilMap", "nil map"),
    ),
    (
        "nil-dereference",
        Expected::Unhandled("main.dereference", "dereference"),
    ),
    ("recovered", Expected::Nothing),
    ("index", Expected::Unhandled("main.index", "index")),
    ("wrapped", Expected::Unhandled("main.raise", "raise")),
    ("stringer", Expected::Unhandled("main.raise", "raise")),
    ("float", Expected::Unhandled("main.raise", "raise")),
    ("custom", Expected::Unhandled("main.raise", "raise")),
    ("custom-float", Expected::Unhandled("main.raise", "raise")),
    ("goroutine", Expected::Unhandled("main.raise", "raise")),
    ("nested", Expected::Unhandled("main.raise", "raise")),
    ("repanic", Expected::Unhandled("main.raise", "raise")),
    ("deadlock", Expected::Fatal),
    ("goexit", Expected::Fatal),
    ("unlock", Expected::Fatal),
    ("overflow", Expected::Fatal),
    ("breakpoint", Expected::ProgramBreakpoint),
    ("exit", Expected::Nothing),
];

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

            let debugged = std::fs::read_to_string(&errors).expect("read standard error");
            if matches!(expected, Expected::ProgramBreakpoint) {
                assert_eq!(code, 0, "{context}");
                assert_eq!(debugged, "", "{context}");
            } else {
                assert_eq!(Some(code), status.map(i64::from), "{context}");

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

async fn check_case(scenario: &Scenario, case: &str, context: &str) {
    match case {
        "goroutine" => {
            assert!(listed(scenario, 1).await.is_some(), "{context}");
        }

        "deadlock" => assert_eq!(
            listed(scenario, 1)
                .await
                .and_then(|task| task.detail)
                .as_deref(),
            Some("select (no cases)"),
            "{context}"
        ),
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

        let none = ExceptionStops::NONE;
        let (scenario, reason) = launched(fixture, "nil-dereference", none).await;
        assert_eq!(reason, StopReason::Exited(ExitStatus::Code(2)), "{fixture}");
        scenario.shutdown().await;
    }
}

#[tokio::test]
async fn a_panic_preempted_as_it_begins_stops_once() {
    let raised = ExceptionStops::default()
        .with("raised", true)
        .expect("Go declares its panics");
    for fixture in BUILDS {
        let (mut scenario, reason) = launched(fixture, "recovered", raised).await;
        let (kind, _) = exception(&reason, fixture);
        assert_eq!(kind, LanguageExceptionKind::Raised, "{fixture}");

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
