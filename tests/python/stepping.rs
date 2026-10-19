//! Python stepping through the agent: over, into, and out of lines, calls,
//! returns, awaits, and raises, from where a breakpoint or a step stopped;
//! and refused, with its reason, from a stop the agent did not report. Then
//! Python's exceptions, raised and uncaught, chosen at that stop.
//!
//! One run of the fixture takes every step in turn on each build, so each
//! step also begins where the last one ended. The fixture prints what it
//! computed, so stopping must change nothing.

use microscope::{
    BreakpointOptions, BreakpointSpec, CodeRole, Error, ExceptionStops, ExitStatus,
    LanguageException, LanguageExceptionKind, LineNumber, StackFrame, StepKind, StopReason,
};

use crate::extension::extension_directory;
use crate::session::PythonSession;
use crate::support::python;

const FIXTURE: &str = "stepping.py";
const NATIVE: &str = "stepping_native.py";

/// The line of `fixture` that carries `marker`.
fn line(fixture: &str, marker: &str) -> LineNumber {
    let text = std::fs::read_to_string(python::fixture(fixture)).expect("read the fixture");
    let index = text
        .lines()
        .position(|line| line.ends_with(&format!("# {marker}")))
        .unwrap_or_else(|| panic!("no line is marked {marker}"));
    LineNumber::new(index as u64 + 1).expect("a one-based line")
}

fn at(fixture: &str, marker: &str) -> (BreakpointSpec, BreakpointOptions) {
    (
        BreakpointSpec::Source {
            path: python::fixture(fixture),
            line: line(fixture, marker),
        },
        BreakpointOptions {
            pending: true,
            ..BreakpointOptions::default()
        },
    )
}

/// The selected frame of the stop, and every frame inner to it.
async fn selected(session: &mut PythonSession) -> (StackFrame, Vec<StackFrame>) {
    let snapshot = session.scenario.snapshot().await;
    let id = snapshot.selected_frame.expect("a selected frame");
    let trace = session
        .scenario
        .operation("backtrace", session.scenario.handle().backtrace())
        .await;
    let index = trace
        .frames
        .iter()
        .position(|frame| frame.id == id)
        .expect("the selected frame is in the backtrace");
    (trace.frames[index].clone(), trace.frames[..index].to_vec())
}

/// Checks the stop is `expected` and selects the program's frame running
/// `function` at the line marked `marker`, with only machinery inner to it.
async fn assert_at(
    session: &mut PythonSession,
    reason: &StopReason,
    expected: &StopReason,
    function: &str,
    marker: &str,
) {
    let context = session.context();
    assert_eq!(
        reason,
        expected,
        "{context}: at {function} {marker}: {}",
        session.output()
    );
    let (frame, inner) = selected(session).await;
    let recorded = frame
        .recorded
        .as_ref()
        .unwrap_or_else(|| panic!("{context}: the selected frame is not Python's: {frame:?}"));
    assert_eq!(
        (
            &*recorded.function,
            recorded.place.as_ref().map(|place| place.line)
        ),
        (
            function,
            Some(u32::try_from(line(&session.fixture, marker).get()).expect("a small line"))
        ),
        "{context}: stopped at {reason:?}"
    );
    assert_eq!(frame.role, CodeRole::Ordinary, "{context}");
    assert!(
        inner
            .iter()
            .all(|hidden| hidden.role == CodeRole::Interpreter),
        "{context}: machinery inner to the program's frame is hidden: {inner:?}"
    );
}

async fn step(session: &mut PythonSession, kind: StepKind) -> StopReason {
    session.scenario.step_to_stop(kind).await
}

const fn stepped(kind: StepKind) -> StopReason {
    StopReason::Step { kind }
}

/// From the breakpoint at `DEEP`, steps over a line of the caller selected,
/// as pdb's `next` after `up` does: the call's rest runs, and the caller
/// stops at its next line.
async fn over_in_the_selected_caller(session: &mut PythonSession) {
    let reason = session.scenario.resume_to_stop().await;
    assert!(
        matches!(reason, StopReason::Breakpoint { address: None, .. }),
        "{}: {reason:?}",
        session.context()
    );
    assert_at(session, &reason, &reason.clone(), "deep", "DEEP").await;
    session.select_python_frame("shallow").await;
    let reason = step(session, StepKind::OverSource).await;
    let over = stepped(StepKind::OverSource);
    assert_at(session, &reason, &over, "shallow", "SHALLOW_AFTER").await;
}

/// Steps out of the module's frame, at a stop the agent did not report,
/// and checks the step is refused, with why.
async fn refused_step_out_of_module(session: &PythonSession) {
    let trace = session
        .scenario
        .operation("backtrace", session.scenario.handle().backtrace())
        .await;
    let module = trace
        .frames
        .iter()
        .find(|frame| {
            frame
                .recorded
                .as_ref()
                .is_some_and(|recorded| &*recorded.function == "<module>")
        })
        .unwrap_or_else(|| panic!("{}: no module frame: {trace:?}", session.context()))
        .id;
    session
        .scenario
        .operation("select", session.scenario.handle().select_frame(module))
        .await;
    let refused = session.scenario.handle().step(StepKind::Out).await;
    assert!(
        matches!(&refused, Err(Error::FrameStepUnsupported(reason)) if reason.contains("where its agent stopped the thread")),
        "{}: {refused:?}",
        session.context()
    );
}

/// From the pause at `parked`, which only native code has run to, stops at
/// a raised exception, where it is raised, with the exception as a value;
/// then, with only uncaught ones chosen, where one leaves its thread's code.
async fn exceptions(session: &mut PythonSession) {
    let choose = |names: &[&str]| {
        names
            .iter()
            .try_fold(ExceptionStops::NONE, |stops, name| stops.with(name, true))
            .expect("known filters")
    };
    let context = session.context();
    session
        .scenario
        .operation(
            "choose exceptions",
            session
                .scenario
                .handle()
                .set_exception_stops(choose(&["python-raised"])),
        )
        .await;
    session.go_on().await;
    let reason = session.finish_run().await;
    let StopReason::LanguageException(LanguageException {
        kind: LanguageExceptionKind::Raised,
        message,
        value: Some(value),
    }) = &reason
    else {
        panic!("{context}: {reason:?}: {}", session.output());
    };
    assert_eq!(&**message, "KeyError: 'missing'", "{context}");
    assert_at(session, &reason, &reason.clone(), "lookup", "LOOKUP").await;
    let summary = session.summary(value).await;
    assert!(
        summary.contains("KeyError"),
        "{context}: {value} is {summary}"
    );

    session
        .scenario
        .operation(
            "choose exceptions",
            session
                .scenario
                .handle()
                .set_exception_stops(choose(&["python-uncaught"])),
        )
        .await;
    let reason = session.scenario.resume_to_stop().await;
    let StopReason::LanguageException(LanguageException {
        kind: LanguageExceptionKind::Unhandled,
        message,
        value: Some(_),
    }) = &reason
    else {
        panic!("{context}: {reason:?}: {}", session.output());
    };
    let raised = format!(
        "RuntimeError: worker failed, raised in worker at {}:{}",
        python::fixture(FIXTURE).display(),
        line(FIXTURE, "WORKER_RAISE")
    );
    assert_eq!(&**message, raised, "{context}");
    let (frame, _) = selected(session).await;
    let function = frame.recorded.as_ref().map(|recorded| &*recorded.function);
    assert_eq!(function, Some("Thread.run"), "{context}: {frame:?}");
}

async fn every_kind_of_step(build: &str) {
    let (mut session, reason, _) = PythonSession::launch_to_stop(
        build,
        FIXTURE,
        &[],
        vec![
            at(FIXTURE, "CALLER_START"),
            at(FIXTURE, "TASK_AWAIT"),
            at(FIXTURE, "HANDLES_CALL"),
            at(FIXTURE, "DEEP"),
        ],
    )
    .await;
    let hit = |reason: &StopReason| matches!(reason, StopReason::Breakpoint { address: None, .. });
    assert!(
        hit(&reason),
        "{}: {reason:?}: {}",
        session.context(),
        session.output()
    );
    let breakpoint = reason.clone();
    assert_at(&mut session, &reason, &breakpoint, "caller", "CALLER_START").await;

    // Over a line, into a call, through it, and back out to the caller at
    // the call, then on to its next line.
    let over = stepped(StepKind::OverSource);
    let into = stepped(StepKind::IntoSource);
    let reason = step(&mut session, StepKind::OverSource).await;
    assert_at(&mut session, &reason, &over, "caller", "CALLER_CALL").await;
    let reason = step(&mut session, StepKind::IntoSource).await;
    assert_at(&mut session, &reason, &into, "callee", "CALLEE_FIRST").await;
    let reason = step(&mut session, StepKind::OverSource).await;
    assert_at(&mut session, &reason, &over, "callee", "CALLEE_LAST").await;
    assert_eq!(
        (
            session.integer("value").await,
            session.integer("doubled").await
        ),
        (1, 2),
        "{}: the callee's locals",
        session.context()
    );
    let reason = step(&mut session, StepKind::OverSource).await;
    assert_at(&mut session, &reason, &over, "caller", "CALLER_CALL").await;
    let reason = step(&mut session, StepKind::OverSource).await;
    assert_at(&mut session, &reason, &over, "caller", "CALLER_AFTER").await;
    assert_eq!(session.integer("result").await, 3, "{}", session.context());

    // Out of the frame, to its caller at the call.
    let reason = step(&mut session, StepKind::Out).await;
    assert_at(
        &mut session,
        &reason,
        &stepped(StepKind::Out),
        "outer",
        "OUTER_CALL",
    )
    .await;

    // Over an await stays with the task, across its suspension.
    let reason = session.scenario.resume_to_stop().await;
    assert!(hit(&reason), "{}: {reason:?}", session.context());
    assert_at(&mut session, &reason, &reason.clone(), "task", "TASK_AWAIT").await;
    let reason = step(&mut session, StepKind::OverSource).await;
    assert_at(&mut session, &reason, &over, "task", "TASK_END").await;

    // Over a call that raises, to where its handler matches it.
    let reason = session.scenario.resume_to_stop().await;
    assert!(hit(&reason), "{}: {reason:?}", session.context());
    assert_at(
        &mut session,
        &reason,
        &reason.clone(),
        "handles",
        "HANDLES_CALL",
    )
    .await;
    let reason = step(&mut session, StepKind::OverSource).await;
    assert_at(&mut session, &reason, &over, "handles", "HANDLES_EXCEPT").await;
    over_in_the_selected_caller(&mut session).await;

    // From a stop the agent did not report, the thread would run on
    // unfollowed before the step could begin: the step is refused, with
    // why, and nothing runs.
    session.resume().await;
    session.stop_at("parked").await;
    refused_step_out_of_module(&session).await;

    exceptions(&mut session).await;

    let reason = session.scenario.resume_to_stop().await;
    let output = session.output();
    assert_eq!(
        reason,
        StopReason::Exited(ExitStatus::Code(0)),
        "{}: {output}",
        session.context()
    );
    for printed in [
        "OUTER 5",
        "TASK 2",
        "HANDLED True",
        "SHALLOW 2",
        "CAUGHT caught",
        "STEPPING DONE",
    ] {
        assert!(
            output.contains(printed),
            "{}: no {printed:?} in {output}",
            session.context()
        );
    }
    session.shutdown().await;
}

/// Checks the stop is `expected` and selects the extension's native
/// `reached`, at a line its debug information describes.
async fn assert_in_reached(
    session: &mut PythonSession,
    reason: &StopReason,
    expected: &StopReason,
) {
    let context = session.context();
    assert_eq!(reason, expected, "{context}: {}", session.output());
    let (frame, _) = selected(session).await;
    let function = frame.function.as_ref().map(|function| &*function.name);
    assert!(
        frame.recorded.is_none() && function == Some("reached") && frame.source.is_some(),
        "{context}: {frame:?}"
    );
}

/// From Python into the extension's C function, back out of it, and over
/// its last line, each to the next line of the Python that called it; into
/// what nothing describes, a builtin and a stripped extension's function,
/// which run through to the next line; and out of the C function from a
/// breakpoint there, where the agent, which has a breakpoint elsewhere,
/// watches none of the program's lines.
async fn steps_between_python_and_c(build: &str) {
    let directory = extension_directory(build);
    let (mut session, reason, _) = PythonSession::launch_to_stop(
        build,
        NATIVE,
        &[&directory],
        vec![at(NATIVE, "FIRST_CALL"), at(NATIVE, "UNUSED")],
    )
    .await;
    let StopReason::Breakpoint {
        address: None,
        hits,
    } = &reason
    else {
        panic!("{}: {reason:?}: {}", session.context(), session.output());
    };
    let first = hits[0].breakpoint;
    assert_at(&mut session, &reason, &reason.clone(), "main", "FIRST_CALL").await;
    session
        .scenario
        .operation(
            "remove breakpoint",
            session.scenario.handle().remove_breakpoint(first),
        )
        .await;
    let into = stepped(StepKind::IntoSource);
    let over = stepped(StepKind::OverSource);
    let out = stepped(StepKind::Out);

    let reason = step(&mut session, StepKind::IntoSource).await;
    assert_in_reached(&mut session, &reason, &into).await;
    let reason = step(&mut session, StepKind::Out).await;
    assert_at(&mut session, &reason, &out, "main", "SECOND_CALL").await;

    let reason = step(&mut session, StepKind::IntoSource).await;
    assert_in_reached(&mut session, &reason, &into).await;
    let mut reason = step(&mut session, StepKind::OverSource).await;
    for _ in 0..8 {
        if selected(&mut session).await.0.recorded.is_some() {
            break;
        }
        reason = step(&mut session, StepKind::OverSource).await;
    }
    assert_at(&mut session, &reason, &over, "main", "COUNTED").await;

    let reason = step(&mut session, StepKind::IntoSource).await;
    assert_at(&mut session, &reason, &into, "main", "BARE_CALL").await;
    let reason = step(&mut session, StepKind::IntoSource).await;
    assert_at(&mut session, &reason, &into, "main", "THIRD_CALL").await;

    // The call into Python on the way acknowledges the agent's idle
    // configuration before the breakpoint stops the thread in C.
    session
        .scenario
        .operation(
            "add breakpoint",
            session
                .scenario
                .handle()
                .add_breakpoint(BreakpointSpec::Function("reached".to_owned())),
        )
        .await;
    let reason = session.scenario.resume_to_stop().await;
    assert!(
        matches!(
            reason,
            StopReason::Breakpoint {
                address: Some(_),
                ..
            }
        ),
        "{}: {reason:?}",
        session.context()
    );
    assert_in_reached(&mut session, &reason, &reason.clone()).await;
    let reason = step(&mut session, StepKind::Out).await;
    assert_at(&mut session, &reason, &out, "main", "NATIVE_END").await;

    let reason = session.scenario.resume_to_stop().await;
    let output = session.output();
    assert_eq!(
        reason,
        StopReason::Exited(ExitStatus::Code(0)),
        "{}: {output}",
        session.context()
    );
    assert!(
        output.contains("NATIVE 2"),
        "{}: {output}",
        session.context()
    );
    session.shutdown().await;
}

/// The line of the extension's C source that carries `text`.
fn extension_line(text: &str) -> u64 {
    let source = std::fs::read_to_string(python::fixture("extension.c")).expect("read the source");
    let index = source
        .lines()
        .position(|line| line.contains(text))
        .unwrap_or_else(|| panic!("no line of extension.c holds {text}"));
    index as u64 + 1
}

/// The C function the selected frame runs, and its line.
async fn native_line(session: &mut PythonSession) -> (Option<String>, Option<u64>) {
    let (frame, _) = selected(session).await;
    (
        frame.function.map(|function| function.name.to_string()),
        frame.source.map(|source| source.line.get()),
    )
}

/// Into an extension's function, then over its lines by breakpoints, as in
/// the program's own executable: over a line that calls back into Python,
/// to the next line of the C function. Without them a step over in a
/// shared library single-stepped through the interpreter until the agent's
/// acknowledgement timed out.
async fn steps_over_the_lines_of_an_extension(build: &str) {
    let directory = extension_directory(build);
    let (mut session, reason, _) =
        PythonSession::launch_to_stop(build, NATIVE, &[&directory], vec![at(NATIVE, "INSPECT")])
            .await;
    let StopReason::Breakpoint {
        address: None,
        hits,
    } = &reason
    else {
        panic!("{}: {reason:?}: {}", session.context(), session.output());
    };

    // The callback runs on the breakpoint's line.
    let breakpoint = hits[0].breakpoint;
    session
        .scenario
        .operation(
            "remove breakpoint",
            session.scenario.handle().remove_breakpoint(breakpoint),
        )
        .await;

    let reason = step(&mut session, StepKind::IntoSource).await;
    assert_eq!(
        reason,
        stepped(StepKind::IntoSource),
        "{}",
        session.context()
    );
    let call = extension_line("PyObject_CallNoArgs(args[4])");
    let mut at = native_line(&mut session).await;
    for _ in 0..8 {
        if at.1 == Some(call) {
            break;
        }

        let reason = step(&mut session, StepKind::OverSource).await;
        assert_eq!(
            reason,
            stepped(StepKind::OverSource),
            "{}: {at:?}",
            session.context()
        );
        at = native_line(&mut session).await;
        assert_eq!(at.0.as_deref(), Some("inspect"), "{}", session.context());
    }
    assert_eq!(at.1, Some(call), "{}", session.context());

    let reason = step(&mut session, StepKind::OverSource).await;
    assert_eq!(
        reason,
        stepped(StepKind::OverSource),
        "{}",
        session.context()
    );
    let (function, line) = native_line(&mut session).await;
    assert_eq!(
        function.as_deref(),
        Some("inspect"),
        "{}",
        session.context()
    );
    assert!(
        line.is_some_and(|line| line > call),
        "{}: {line:?}",
        session.context()
    );

    let reason = session.scenario.resume_to_stop().await;
    assert_eq!(
        reason,
        StopReason::Exited(ExitStatus::Code(0)),
        "{}: {}",
        session.context(),
        session.output()
    );
    session.shutdown().await;
}

#[tokio::test]
async fn steps_over_the_lines_of_an_extension_on_each_build() {
    for build in ["3.14", "3.14t", "3.15"] {
        steps_over_the_lines_of_an_extension(build).await;
    }
}

/// On each build the extension is built for that PEP 768 injects the agent
/// into.
#[tokio::test]
async fn steps_between_python_and_c_on_each_build() {
    for build in ["3.14", "3.14t", "3.15"] {
        steps_between_python_and_c(build).await;
    }
}

// One test for each build PEP 768 injects the agent into, and for each the
// launch bootstrap gives it to, which the test runner runs at once: a
// process holds one debugger session.

#[tokio::test]
async fn every_kind_of_step_on_3_12() {
    every_kind_of_step("3.12").await;
}

#[tokio::test]
async fn every_kind_of_step_on_3_13() {
    every_kind_of_step("3.13").await;
}

#[tokio::test]
async fn every_kind_of_step_on_3_14() {
    every_kind_of_step("3.14").await;
}

#[tokio::test]
async fn every_kind_of_step_on_3_14t() {
    every_kind_of_step("3.14t").await;
}

#[tokio::test]
async fn every_kind_of_step_on_3_15() {
    every_kind_of_step("3.15").await;
}

#[tokio::test]
async fn every_kind_of_step_on_3_15t() {
    every_kind_of_step("3.15t").await;
}

#[tokio::test]
async fn every_kind_of_step_on_standalone_3_14() {
    every_kind_of_step("standalone-3.14").await;
}
