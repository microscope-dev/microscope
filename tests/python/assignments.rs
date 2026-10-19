//! Python variables given new values where the agent stopped their thread:
//! each value the evaluator computes, and an object another name holds,
//! reaches the variable the program then uses, under a new stop. Where the
//! write could race another thread, or the stop is not the agent's, or the
//! interpreter is free-threaded, it is refused and nothing runs.

use microscope::{
    BreakpointOptions, BreakpointSpec, Error, Evaluation, EvaluationMode, Expression,
    ExpressionErrorKind, InferiorState, InspectionLimits, LineNumber, StopContext, StopReason,
    VariableState,
};

use crate::session::PythonSession;
use crate::support::python;

const FIXTURE: &str = "assignments.py";

/// The line of the fixture marked `marker`.
fn line_of(marker: &str) -> LineNumber {
    let text = std::fs::read_to_string(python::fixture(FIXTURE)).expect("read the fixture");
    let index = text
        .lines()
        .position(|line| line.ends_with(&format!("# {marker}")))
        .unwrap_or_else(|| panic!("no line is marked {marker}"));
    LineNumber::new(index as u64 + 1).expect("a one-based line")
}

fn breakpoint(marker: &str) -> (BreakpointSpec, BreakpointOptions) {
    (
        BreakpointSpec::Source {
            path: python::fixture(FIXTURE),
            line: line_of(marker),
        },
        BreakpointOptions {
            pending: true,
            ..BreakpointOptions::default()
        },
    )
}

/// Assigns with `text` in `context`, or in the selected frame.
async fn assign(
    session: &PythonSession,
    context: Option<StopContext>,
    text: &str,
) -> microscope::Result<Evaluation> {
    let expression = Expression::parse(text).expect("an expression");
    let handle = session.scenario.handle();
    let limits = InspectionLimits::default();
    let request = async {
        match context {
            Some(context) => {
                handle
                    .at(context)
                    .evaluate_with(&expression, EvaluationMode::Assign, limits)
                    .await
            }
            None => {
                handle
                    .evaluate_with(&expression, EvaluationMode::Assign, limits)
                    .await
            }
        }
    };

    session.scenario.attempt(text, request).await
}

/// How the variable an assignment gave presents, read again where the
/// assignment's stop published it, which selects the variable's frame.
async fn assigned(session: &mut PythonSession, text: &str, function: &str) -> String {
    let context = session.context();
    let before = stop_id(session).await;
    let evaluation = assign(session, None, text)
        .await
        .unwrap_or_else(|error| panic!("{context}: `{text}` failed: {error}"));

    let snapshot = session.scenario.snapshot().await;
    let InferiorState::Stopped {
        stop_id, reason, ..
    } = &snapshot.inferior
    else {
        panic!("{context}: `{text}` left the process running");
    };
    assert!(
        matches!(reason, StopReason::Assignment) && *stop_id != before,
        "{context}: `{text}` is published under a new stop: {reason:?}"
    );

    let trace = session
        .scenario
        .operation("backtrace", session.scenario.handle().backtrace())
        .await;
    let selected = trace
        .frames
        .iter()
        .find(|frame| Some(frame.id) == snapshot.selected_frame)
        .and_then(|frame| frame.recorded.as_ref())
        .map(|recorded| recorded.function.to_string());
    assert_eq!(
        selected.as_deref(),
        Some(function),
        "{context}: `{text}` selects the variable's frame"
    );

    match evaluation {
        Evaluation::Value { value, .. } => match value.state {
            VariableState::Available {
                presentation: Some(presentation),
                ..
            } => presentation.summary.to_string(),
            state => panic!("{context}: `{text}` presents nothing: {state:?}"),
        },
        other => panic!("{context}: `{text}` has no value: {other:?}"),
    }
}

async fn stop_id(session: &mut PythonSession) -> microscope::StopId {
    match session.scenario.snapshot().await.inferior {
        InferiorState::Stopped { stop_id, .. } => stop_id,
        state => panic!("{}: not stopped: {state:?}", session.context()),
    }
}

/// Checks `text` is refused as `kind`, with `reason` in its message,
/// leaving the stop as it was.
async fn refused(
    session: &mut PythonSession,
    context: Option<StopContext>,
    text: &str,
    kind: ExpressionErrorKind,
    reason: &str,
) {
    let name = session.context();
    let before = stop_id(session).await;

    match assign(session, context, text).await {
        Err(Error::Expression(error)) if error.kind == kind && error.message.contains(reason) => {}
        other => panic!("{name}: `{text}` is not refused for {reason:?}: {other:?}"),
    }

    assert_eq!(
        stop_id(session).await,
        before,
        "{name}: refusing `{text}` ran nothing"
    );
}

/// The frame running `function` on a thread other than the stop's.
async fn other_threads_frame(session: &mut PythonSession, function: &str) -> StopContext {
    let context = session.context();
    let snapshot = session.scenario.snapshot().await;
    let InferiorState::Stopped {
        stop_id, thread_id, ..
    } = snapshot.inferior
    else {
        panic!("{context}: not stopped");
    };

    let waiter = snapshot
        .threads
        .iter()
        .map(|thread| thread.id)
        .find(|&thread| thread != thread_id)
        .unwrap_or_else(|| panic!("{context}: no second thread"));

    let trace = session
        .scenario
        .operation(
            "backtrace",
            session
                .scenario
                .handle()
                .at(StopContext {
                    stop: stop_id,
                    execution: waiter.into(),
                    frame: microscope::StackFrameId::INNERMOST,
                })
                .backtrace(),
        )
        .await;

    let wait = trace
        .frames
        .iter()
        .find(|frame| {
            frame
                .recorded
                .as_ref()
                .is_some_and(|recorded| &*recorded.function == function)
        })
        .unwrap_or_else(|| panic!("{context}: no frame of {function}: {trace:#?}"));

    StopContext {
        stop: stop_id,
        execution: waiter.into(),
        frame: wait.id,
    }
}

async fn launch(interpreter: &str) -> PythonSession {
    let (session, reason, _) = PythonSession::launch_to_stop(
        interpreter,
        FIXTURE,
        &[],
        vec![breakpoint("ALONE_LINE"), breakpoint("BESIDE_LINE")],
    )
    .await;
    assert!(
        matches!(reason, StopReason::Breakpoint { address: None, .. }),
        "{}: {reason:?}: {}",
        session.context(),
        session.output()
    );

    session
}

async fn each_value_reaches_the_program(interpreter: &str) {
    let mut session = launch(interpreter).await;
    let context = session.context();
    let alone = |text| (text, "alone");

    for ((text, function), shown) in [
        (alone("count = 41"), "41"),
        (alone("count += 1"), "42"),
        (alone("ratio = 2.25"), "2.25"),
        (alone("name = \"after\""), "'after'"),
        (alone("flag = true"), "True"),
        (alone("held = other"), "[1, 2]"),
        (alone("gone = None"), "None"),
    ] {
        let summary = assigned(&mut session, text, function).await;
        assert_eq!(summary, shown, "{context}: `{text}`");
    }

    let kind = ExpressionErrorKind::Assignment;
    refused(&mut session, None, "LIMIT = 5", kind, "only a variable").await;
    refused(&mut session, None, "name = \"\\xff\"", kind, "UTF-8").await;
    refused(
        &mut session,
        None,
        "other.append = 1",
        ExpressionErrorKind::NotAnLvalue,
        "only a variable is assigned",
    )
    .await;

    let reason = session.scenario.resume_to_stop().await;
    assert!(
        matches!(reason, StopReason::Breakpoint { address: None, .. }),
        "{context}: {reason:?}"
    );

    let output = session.output();
    assert!(
        output.contains("COUNT 42 RATIO 2.25 NAME after FLAG True HELD True GONE None LIMIT 3"),
        "{context}: the program uses each value given: {output}"
    );

    // Another thread runs: the write could race it, and that thread was
    // stopped where the agent did not stop it.
    refused(&mut session, None, "count = 7", kind, "another thread").await;
    let waiting = other_threads_frame(&mut session, "beside.<locals>.wait").await;
    refused(
        &mut session,
        Some(waiting),
        "started = 1",
        kind,
        "agent stopped",
    )
    .await;

    let reason = session.scenario.resume_to_stop().await;
    assert!(
        matches!(reason, StopReason::Exited(_)),
        "{context}: {reason:?}"
    );

    let output = session.output();
    assert!(
        output.contains("BESIDE COUNT 1") && output.contains("ASSIGNMENTS DONE"),
        "{context}: nothing refused was given: {output}"
    );
    session.shutdown().await;
}

#[tokio::test]
async fn each_value_reaches_the_program_on_3_14() {
    each_value_reaches_the_program("3.14").await;
}

#[tokio::test]
async fn each_value_reaches_the_program_on_3_15() {
    each_value_reaches_the_program("3.15").await;
}

/// A free-threaded interpreter's other threads may hold what a write
/// changes whenever they run, so nothing is given there.
#[tokio::test]
async fn a_free_threaded_interpreter_refuses_assignments() {
    let mut session = launch("3.14t").await;
    refused(
        &mut session,
        None,
        "count = 41",
        ExpressionErrorKind::Assignment,
        "free-threaded",
    )
    .await;

    let reason = session.scenario.resume_to_stop().await;
    assert!(
        matches!(reason, StopReason::Breakpoint { address: None, .. }),
        "{reason:?}"
    );
    assert!(
        session.output().contains("COUNT 1 "),
        "{}",
        session.output()
    );
    session.shutdown().await;
}

/// CPython before 3.14 takes the agent through a launch's bootstrap, whose
/// thread runs beside the program's, and 3.12's frame variables are a copy:
/// nothing is given there, and the refusal says so.
#[tokio::test]
async fn an_interpreter_before_3_14_refuses_assignments() {
    for interpreter in ["3.12", "3.13"] {
        let mut session = launch(interpreter).await;
        refused(
            &mut session,
            None,
            "count = 41",
            ExpressionErrorKind::Assignment,
            "CPython 3.14 or later",
        )
        .await;

        let reason = session.scenario.resume_to_stop().await;
        assert!(
            matches!(reason, StopReason::Breakpoint { address: None, .. }),
            "{interpreter}: {reason:?}"
        );
        assert!(
            session.output().contains("COUNT 1 "),
            "{interpreter}: {}",
            session.output()
        );
        session.shutdown().await;
    }
}
