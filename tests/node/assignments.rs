//! JavaScript variables given values through V8's inspector where V8
//! paused their thread: each kind of binding a frame reaches, given each
//! kind of value, reaches the program, which prints it after the stop the
//! assignment published; the value read again from memory is the one V8
//! lists. What V8 does not assign, or not exactly, is refused and nothing
//! runs.

use microscope::{
    BreakpointOptions, BreakpointSpec, Error, Evaluation, EvaluationMode, Expression,
    ExpressionErrorKind, InferiorState, InspectionLimits, LineNumber, SourceLanguage, StopContext,
    StopReason, VariableState,
};

use crate::session::{MATRIX, NodeSession};
use crate::support::node;

const FIXTURE: &str = "assignments.js";

/// The line of the fixture that carries `marker`.
fn line_of(marker: &str) -> LineNumber {
    let text = std::fs::read_to_string(node::fixture(FIXTURE)).expect("read the fixture");
    let index = text
        .lines()
        .position(|line| line.contains(&format!("// {marker}")))
        .unwrap_or_else(|| panic!("no line marked {marker}"));
    LineNumber::new(index as u64 + 1).expect("a one-based line")
}

fn breakpoint(marker: &str) -> (BreakpointSpec, BreakpointOptions) {
    (
        BreakpointSpec::Source {
            path: node::fixture(FIXTURE),
            line: line_of(marker),
        },
        BreakpointOptions {
            pending: true,
            ..BreakpointOptions::default()
        },
    )
}

async fn stop_id(session: &NodeSession) -> microscope::StopId {
    match session
        .scenario
        .operation("snapshot", session.scenario.handle().snapshot())
        .await
        .inferior
    {
        InferiorState::Stopped { stop_id, .. } => stop_id,
        state => panic!("{}: not stopped: {state:?}", session.context()),
    }
}

/// Assigns with `text` in `context`, or in the selected frame.
async fn assign(
    session: &NodeSession,
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

/// The summary of what `text` reads in the selected frame.
async fn read(session: &NodeSession, text: &str) -> String {
    let expression = Expression::parse(text).expect("an expression");
    match session.scenario.handle().evaluate(&expression).await {
        Ok(Evaluation::Value { value, .. }) => match value.state {
            VariableState::Available {
                presentation: Some(presentation),
                ..
            } => presentation.summary.to_string(),
            state => format!("<{state:?}>"),
        },
        other => format!("<{other:?}>"),
    }
}

/// How the variable `text` gave presents, read again where the stop the
/// assignment published, which selects the variable's frame, `function`.
async fn assigned(session: &NodeSession, text: &str, function: &str) -> String {
    let context = session.context();
    let before = stop_id(session).await;
    let evaluation = assign(session, None, text)
        .await
        .unwrap_or_else(|error| panic!("{context}: `{text}` failed: {error}"));

    let snapshot = session
        .scenario
        .operation("snapshot", session.scenario.handle().snapshot())
        .await;
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

/// Checks `text` is refused as `kind`, with `reason` in its message,
/// leaving the stop as it was.
async fn refused(
    session: &NodeSession,
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

/// Checks V8 refuses `text`, which it was asked to make, with `reason` in
/// the refusal.
async fn refused_by_v8(session: &NodeSession, text: &str, reason: &str) {
    let name = session.context();
    match assign(session, None, text).await {
        Err(Error::Expression(error))
            if error.kind == ExpressionErrorKind::Assignment
                && error.message.contains("V8 did not make it")
                && error.message.contains(reason) => {}
        other => panic!("{name}: V8 does not refuse `{text}` for {reason:?}: {other:?}"),
    }
}

/// Selects the frame of the JavaScript function `function` on the stop's
/// thread, and returns its context.
async fn select(session: &NodeSession, function: &str) -> StopContext {
    let handle = session.scenario.handle();
    let trace = session
        .scenario
        .operation("backtrace", handle.backtrace())
        .await;
    let frame = trace
        .frames
        .iter()
        .find(|frame| {
            frame.recorded.as_ref().is_some_and(|recorded| {
                recorded.language == SourceLanguage::JavaScript
                    && recorded.function.as_ref() == function
            })
        })
        .unwrap_or_else(|| panic!("{}: no frame of {function}", session.context()));
    session
        .scenario
        .operation("select frame", handle.select_frame(frame.id))
        .await;
    let snapshot = session
        .scenario
        .operation("snapshot", session.scenario.handle().snapshot())
        .await;
    let InferiorState::Stopped {
        stop_id, thread_id, ..
    } = snapshot.inferior
    else {
        panic!("{}: not stopped", session.context());
    };
    StopContext {
        stop: stop_id,
        execution: thread_id.into(),
        frame: frame.id,
    }
}

/// The name of a register V8 names no variable in the selected frame.
async fn unnamed_register(session: &NodeSession) -> String {
    let handle = session.scenario.handle();
    let snapshot = session
        .scenario
        .operation("variables", handle.variables())
        .await;
    snapshot
        .variables
        .iter()
        .find(|variable| variable.kind == microscope::VariableKind::Unnamed)
        .map_or_else(
            || panic!("{}: no unnamed register", session.context()),
            |variable| variable.name.to_string(),
        )
}

pub async fn each_binding_takes_each_value(entry: &str) {
    let (mut owned, reason, _) =
        NodeSession::unlaunched_with_node(entry, FIXTURE, &["--allow-natives-syntax"])
            .run_to_stop(vec![breakpoint("ASSIGN"), breakpoint("CALLEE")])
            .await;
    let session = &mut owned;
    let context = session.context();
    assert!(
        matches!(reason, StopReason::Breakpoint { .. }),
        "{context}: {reason:?}\n{}",
        session.output()
    );
    let first = select(session, "inner").await;

    let inner = |text| (text, "inner");
    for ((text, function), shown) in [
        (inner("param = 41"), "41"),
        (inner("local += 1"), "13"),
        (inner("fixed = \"changed\""), "'changed'"),
        (
            inner("big = 12345678901234567890n"),
            "12345678901234567890n",
        ),
        (inner("maybe = null"), "null"),
        (inner("gone = 0.5"), "0.5"),
        (inner("blocked = true"), "true"),
        (inner("captured = 50"), "50"),
        (inner("tally = 7"), "7"),
        (inner("holder = other"), "{ b: 2 }"),
        // In the same pause: V8's listing of the scope still names the
        // object the variable held as it paused.
        (inner("holder = holder"), "{ b: 2 }"),
        (inner("box = other"), "{ b: 2 }"),
    ] {
        let summary = assigned(session, text, function).await;
        assert_eq!(summary, shown, "{context}: `{text}`");
        // V8's own listing, read anew, agrees with memory.
        assert_eq!(
            read(session, text.split(' ').next().expect("a name")).await,
            shown,
            "{context}: `{text}` reads back"
        );
    }

    let kind = ExpressionErrorKind::Assignment;
    // The stop the first assignment was made at is gone.
    match assign(session, Some(first), "param = 1").await {
        Err(Error::StaleStop) => {}
        other => panic!("{context}: a stale assignment is refused: {other:?}"),
    }
    refused(
        session,
        None,
        "param = 9007199254740993",
        kind,
        "approximately",
    )
    .await;
    refused(
        session,
        None,
        "holder.a = 5",
        ExpressionErrorKind::NotAnLvalue,
        "",
    )
    .await;
    let register = unnamed_register(session).await;
    refused(session, None, &format!("{register} = 1"), kind, "register").await;
    // outer()'s frame reaches the context inner() gave `box` through,
    // whose value no listing of the pause names.
    select(session, "outer").await;
    // The refusal comes once V8 listed the frame's scopes, which the
    // program resumed to answer.
    match assign(session, None, "box = box").await {
        Err(Error::Expression(error))
            if error.kind == kind && error.message.contains("during this pause") => {}
        other => panic!("{context}: `box = box` is not refused: {other:?}"),
    }

    session.scenario.resume_to_stop().await;
    let output = session.output();
    assert!(
        output.contains(
            "ASSIGNED { param: 41, local: 13, fixed: 'changed', big: 12345678901234567890n, \
             maybe: null, gone: 0.5, blocked: true, captured: 50, tally: 7 }"
        ) && output.contains("HOLDER true { b: 2 }")
            && output.contains("BOX true { b: 2 }"),
        "{context}: the program uses each value given: {output}"
    );

    elsewhere(session).await;
    owned.shutdown().await;
}

/// At the stop in `callee()`: `hot()`'s frame runs Turbofan code, whose
/// context V8 assigns and whose registers it does not; then, at a stop V8
/// did not pause, nothing is assigned.
async fn elsewhere(session: &mut NodeSession) {
    let context = session.context();
    let kind = ExpressionErrorKind::Assignment;
    let output = session.output();
    // hot() runs Turbofan code: its context is assigned, its registers are not.
    assert!(
        output.contains("HOT TIER turbofan"),
        "{context}: hot() is optimized: {output}"
    );
    select(session, "hot").await;
    refused_by_v8(session, "kept = 1", "optimized code keeps").await;
    select(session, "hot").await;
    // Microscope cannot read an optimized frame's variables back; the
    // answer says so, and the program shows the value given.
    match assign(session, None, "shared = 100").await {
        Ok(Evaluation::Value { value, .. }) => assert!(
            matches!(&value.state, VariableState::Unavailable(reason)
                if format!("{reason:?}").contains("optimized")),
            "{context}: `shared = 100` reads back {:?}",
            value.state
        ),
        other => panic!("{context}: `shared = 100` is made: {other:?}"),
    }

    session.resume().await;
    session.stop_at("parked").await;
    let output = session.output();
    assert!(
        output.contains("HOT 496"),
        "{context}: the optimized frame uses the value given: {output}"
    );

    // V8 has not paused the thread, which waits in a read.
    select(session, "parks").await;
    refused(session, None, "tally = 2", kind, "only where it paused").await;
    session.proceed().await;
    session.wrote("ASSIGNMENTS DONE").await;
    assert!(
        session.output().contains("PARKED waiting=1 tally=7"),
        "{context}: nothing refused was given: {}",
        session.output()
    );
}

#[tokio::test]
async fn each_binding_takes_each_value_on_every_build() {
    for entry in MATRIX {
        each_binding_takes_each_value(entry).await;
    }
}
