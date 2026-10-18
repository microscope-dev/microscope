//! Watches of Python variables: a variable of a frame is watched where it
//! keeps its object, each store that gives it another object stops in the
//! frame with the values before and after, as Python shows them, and the
//! watch ends with the frame. Neither the agent nor a Python breakpoint
//! takes part, so every build watches so.

use microscope::{
    Error, Expression, StackFrame, StopReason, WatchAccess, WatchScope, Watchpoint,
    WatchpointInvalidation, WatchpointSpec,
};

use crate::session::PythonSession;
use crate::support::python;

const FIXTURE: &str = "watches.py";

fn expression(text: &str) -> Expression {
    Expression::parse(text).expect("a valid expression")
}

/// The line of the fixture marked `marker`.
fn line(marker: &str) -> u32 {
    let text = std::fs::read_to_string(python::fixture(FIXTURE)).expect("read the fixture");
    let index = text
        .lines()
        .position(|line| line.ends_with(&format!("# {marker}")))
        .unwrap_or_else(|| panic!("no line is marked {marker}"));
    u32::try_from(index + 1).expect("a small line")
}

/// The stop's selected frame.
async fn selected(session: &mut PythonSession) -> StackFrame {
    let id = session
        .scenario
        .snapshot()
        .await
        .selected_frame
        .expect("a selected frame");
    let trace = session
        .scenario
        .operation("backtrace", session.scenario.handle().backtrace())
        .await;
    trace
        .frames
        .iter()
        .find(|frame| frame.id == id)
        .cloned()
        .expect("the selected frame is in the backtrace")
}

/// Checks the stop is a hit of the watch that changed `total` from
/// `previous` to `current`, in `count` at the store.
async fn changed(session: &mut PythonSession, reason: &StopReason, previous: &str, current: &str) {
    let context = session.context();
    let StopReason::Watchpoint { hits } = reason else {
        panic!("{context}: {reason:?}: {}", session.output());
    };
    assert_eq!(
        (
            hits[0].previous_value.as_deref(),
            hits[0].current_value.as_deref()
        ),
        (Some(previous), Some(current)),
        "{context}"
    );
    let frame = selected(session).await;
    let recorded = frame
        .recorded
        .as_ref()
        .unwrap_or_else(|| panic!("{context}: the selected frame is not Python's: {frame:?}"));
    assert_eq!(
        (
            &*recorded.function,
            recorded.place.as_ref().map(|place| place.line)
        ),
        ("count", Some(line("COUNT_ADD"))),
        "{context}"
    );

    // The source shown is the frame's, not the interpreter's.
    let source = session
        .scenario
        .operation("source", session.scenario.handle().source_context(0))
        .await;
    assert_eq!(
        (source.path.file_name(), source.line.get()),
        (
            Some(std::ffi::OsStr::new(FIXTURE)),
            u64::from(line("COUNT_ADD"))
        ),
        "{context}"
    );
}

/// Watches the selected frame's variable `name` for changes.
async fn watch(session: &PythonSession, name: &str) -> Watchpoint {
    let handle = session.scenario.handle();
    let target = session
        .scenario
        .operation(
            &format!("resolve {name}"),
            handle.resolve_watch_target(&expression(name)),
        )
        .await;
    assert!(
        matches!(target.scope(), WatchScope::Interpreted { .. }) && target.byte_size() == 8,
        "{}: {target:?}",
        session.context()
    );
    session
        .scenario
        .operation(
            &format!("watch {name}"),
            handle.add_watchpoint(
                WatchpointSpec::Target(Box::new(target)),
                WatchAccess::Change,
            ),
        )
        .await
}

async fn a_variable_is_watched_for_its_frames_life(interpreter: &str) {
    let mut session = PythonSession::launch(interpreter, FIXTURE, &[]).await;
    session.stop_at("counting").await;
    let context = session.context();
    let handle = session.scenario.handle();
    let trace = session
        .scenario
        .operation("backtrace", handle.backtrace())
        .await;
    let count = trace
        .frames
        .iter()
        .find(|frame| {
            frame
                .recorded
                .as_ref()
                .is_some_and(|recorded| &*recorded.function == "count")
        })
        .unwrap_or_else(|| panic!("{context}: no frame of count: {trace:?}"))
        .id;
    session
        .scenario
        .operation("select count", handle.select_frame(count))
        .await;

    // Only a variable itself is watched: an object an expression reaches
    // moves and changes as the interpreter likes.
    let refused = handle.resolve_watch_target(&expression("total.real")).await;
    assert!(
        matches!(&refused, Err(Error::WatchTargetUnsupported(reason)) if reason.contains("variable of the frame")),
        "{context}: {refused:?}"
    );
    let total = watch(&session, "total").await;

    // Adding 0 stores the object it held, which is no change.
    session.go_on().await;
    let reason = session.finish_run().await;
    changed(&mut session, &reason, "0", "1").await;
    let reason = session.scenario.resume_to_stop().await;
    changed(&mut session, &reason, "1", "3").await;
    let reason = session.scenario.resume_to_stop().await;
    changed(&mut session, &reason, "3", "6").await;

    // The next call's frame takes the returned one's place, storing what
    // the unbound `spare` held: the watch ends there all the same, rather
    // than go on to report the new frame's `spare`.
    session
        .scenario
        .operation(
            "unwatch total",
            session.scenario.handle().remove_watchpoint(total.id),
        )
        .await;
    watch(&session, "spare").await;
    let reason = session.scenario.resume_to_stop().await;
    let StopReason::WatchpointInvalidated { invalidated } = &reason else {
        panic!("{context}: {reason:?}: {}", session.output());
    };
    assert_eq!(
        invalidated[0].reason,
        WatchpointInvalidation::ScopeExited,
        "{context}"
    );
    let reason = session.scenario.resume_to_stop().await;
    assert_eq!(
        reason,
        StopReason::Exited(microscope::ExitStatus::Code(0)),
        "{context}"
    );
    assert!(
        session.output().contains("COUNTED 6 1"),
        "{context}: {}",
        session.output()
    );
    session.shutdown().await;
}

// One test for each layout family: no offsets table, a free-threaded
// 3.13, stack references, a free-threaded tail-calling 3.15, and a build
// linked into its executable.

#[tokio::test]
async fn a_variable_is_watched_for_its_frames_life_on_3_12() {
    a_variable_is_watched_for_its_frames_life("3.12").await;
}

#[tokio::test]
async fn a_variable_is_watched_for_its_frames_life_on_3_13t() {
    a_variable_is_watched_for_its_frames_life("3.13t").await;
}

#[tokio::test]
async fn a_variable_is_watched_for_its_frames_life_on_3_14() {
    a_variable_is_watched_for_its_frames_life("3.14").await;
}

#[tokio::test]
async fn a_variable_is_watched_for_its_frames_life_on_3_15t() {
    a_variable_is_watched_for_its_frames_life("3.15t").await;
}

#[tokio::test]
async fn a_variable_is_watched_for_its_frames_life_on_standalone_3_14() {
    a_variable_is_watched_for_its_frames_life("standalone-3.14").await;
}
