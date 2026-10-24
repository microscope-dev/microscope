//! Workers through Node's inspector: a breakpoint in a worker's script
//! stops its thread, with its locals, and steps there are its isolate's;
//! two isolates pausing at once are two stops, each on its own thread;
//! and an exception thrown in a worker stops on the worker's thread.

use microscope::{
    BreakpointOptions, BreakpointSpec, ExceptionStops, ExitStatus, LineNumber, StepKind,
    StopReason, ThreadId,
};

use crate::oracle;
use crate::session::{MATRIX, NodeSession};
use crate::support::node;

const FIXTURE: &str = "workers.js";
const CRUNCH: u32 = 9;
const AFTER: u32 = 10;
const THROWN: u32 = 15;

fn at_crunch() -> (BreakpointSpec, BreakpointOptions) {
    (
        BreakpointSpec::Source {
            path: node::fixture(FIXTURE),
            line: LineNumber::new(u64::from(CRUNCH)).expect("a line"),
        },
        BreakpointOptions {
            pending: true,
            ..BreakpointOptions::default()
        },
    )
}

/// The stopped thread and the summary of its selected frame's local
/// `name`.
async fn where_stopped(session: &NodeSession, name: &str) -> (ThreadId, String) {
    let snapshot = session
        .scenario
        .operation("snapshot", session.scenario.handle().snapshot())
        .await;
    let thread = match snapshot.selected {
        Some(microscope::ExecutionContext::Thread(thread)) => thread,
        other => panic!("{}: selected {other:?}", session.context()),
    };
    let variables = session
        .scenario
        .operation("variables", session.scenario.handle().variables())
        .await;
    let value = variables
        .variables
        .iter()
        .find(|variable| &*variable.name == name)
        .map_or_else(
            || "absent".to_owned(),
            |variable| match &variable.state {
                microscope::VariableState::Available {
                    presentation: Some(presentation),
                    ..
                } => presentation.summary.to_string(),
                other => format!("{other:?}"),
            },
        );
    (thread, value)
}

/// The process's main thread, whose id is the process's.
async fn main_thread(session: &NodeSession) -> ThreadId {
    let snapshot = session
        .scenario
        .operation("snapshot", session.scenario.handle().snapshot())
        .await;
    match snapshot.inferior {
        microscope::InferiorState::Stopped { process_id, .. } => ThreadId::new(process_id.get()),
        other => panic!("{}: not stopped: {other:?}", session.context()),
    }
}

/// On every build, a breakpoint in a worker stops the worker's thread at
/// its line with its locals, and a step over there ends at the worker's
/// next line, not in the main thread.
#[tokio::test]
async fn a_worker_stops_and_steps_on_its_own_thread() {
    let mut lies = Vec::new();
    for entry in MATRIX {
        let (mut session, reason, _events) =
            NodeSession::launch_to_stop(entry, FIXTURE, vec![at_crunch()]).await;
        let context = session.context();
        let main = main_thread(&session).await;
        let pause = oracle::selected(&session.scenario).await;
        let (thread, n) = where_stopped(&session, "n").await;
        if !matches!(reason, StopReason::Breakpoint { address: None, .. })
            || pause.line != CRUNCH
            || thread == main
            || n != "6"
        {
            lies.push(format!(
                "{context}: {reason:?} at {pause} on {thread:?} (main {main:?}), n = {n}"
            ));
        }
        let reason = session.scenario.step_to_stop(StepKind::OverSource).await;
        let pause = oracle::selected(&session.scenario).await;
        let (stepped, squared) = where_stopped(&session, "squared").await;
        if !matches!(reason, StopReason::Step { .. })
            || pause.line != AFTER
            || stepped != thread
            || squared != "36"
        {
            lies.push(format!(
                "{context}: the step ended {reason:?} at {pause} on {stepped:?}, squared = {squared}"
            ));
        }
        let reason = session.scenario.resume_to_stop().await;
        if reason != StopReason::Exited(ExitStatus::Code(0))
            || !session.output().contains("RESULT 37")
        {
            lies.push(format!("{context}: ended {reason:?}\n{}", session.output()));
        }
        session.shutdown().await;
    }
    assert!(lies.is_empty(), "{}", lies.join("\n"));
}

/// On every build, the main thread and a worker pausing at one line are
/// two stops, each on its own thread with its own `n`.
#[tokio::test]
async fn two_isolates_pausing_at_once_stop_each_on_its_thread() {
    let mut lies = Vec::new();
    for entry in MATRIX {
        let session = NodeSession::unlaunched_with(entry, FIXTURE, &["both"]);
        let (mut session, mut reason, _events) = session.run_to_stop(vec![at_crunch()]).await;
        let context = session.context();
        let main = main_thread(&session).await;
        let mut seen = Vec::new();
        while let StopReason::Breakpoint { address: None, .. } = reason {
            let (thread, n) = where_stopped(&session, "n").await;
            seen.push((thread == main, n));
            assert!(seen.len() < 4, "{context}: too many stops");
            reason = session.scenario.resume_to_stop().await;
        }
        seen.sort();
        let expected = vec![(false, "6".to_owned()), (true, "2".to_owned())];
        if seen != expected || reason != StopReason::Exited(ExitStatus::Code(0)) {
            lies.push(format!("{context}: (main, n) {seen:?}, then {reason:?}"));
        }
        session.shutdown().await;
    }
    assert!(lies.is_empty(), "{}", lies.join("\n"));
}

/// On every build, an exception thrown in a worker stops on its thread.
#[tokio::test]
async fn an_exception_in_a_worker_stops_on_its_thread() {
    let mut lies = Vec::new();
    for entry in MATRIX {
        let session = NodeSession::unlaunched(entry, FIXTURE);
        let stops = ExceptionStops::NONE
            .with("javascript-thrown", true)
            .expect("a filter");
        session
            .scenario
            .operation(
                "choose exceptions",
                session.scenario.handle().set_exception_stops(stops),
            )
            .await;
        let (session, reason, _events) = session.run_to_stop(Vec::new()).await;
        let context = session.context();
        let main = main_thread(&session).await;
        // Node's own startup throws nothing a filter stops in user code.
        let pause = oracle::selected(&session.scenario).await;
        let (thread, _) = where_stopped(&session, "n").await;
        if !matches!(reason, StopReason::LanguageException(_))
            || pause.function != "risky"
            || pause.line != THROWN
            || thread == main
        {
            lies.push(format!("{context}: {reason:?} at {pause} on {thread:?}"));
        }
        session.shutdown().await;
    }
    assert!(lies.is_empty(), "{}", lies.join("\n"));
}
