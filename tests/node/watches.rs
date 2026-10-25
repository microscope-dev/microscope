//! Watches of JavaScript variables: a register's ends as the next call
//! builds its frame in its place, and an argument's ends with its call;
//! a captured variable's follows its context through the collections that
//! move it, reporting each store the program prints and nothing a
//! collection rewrites; a frame V8 replaces on a loop, from Sparkplug's
//! code or the interpreter's, ends its watch; and a variable V8 14.6
//! keeps in a context cell is refused.

use microscope::{
    BreakpointOptions, BreakpointSpec, DebuggerEvent, Error, Expression, InferiorState, LineNumber,
    StopReason, WatchAccess, WatchScope, Watchpoint, WatchpointInvalidation, WatchpointSpec,
};
use tokio::sync::broadcast;

use crate::session::{MATRIX, NodeSession, major};
use crate::support::node;

const FIXTURE: &str = "watches.js";

/// The line of the fixture that carries `marker`.
fn line_of(marker: &str) -> LineNumber {
    let text = std::fs::read_to_string(node::fixture(FIXTURE)).expect("read the fixture");
    let index = text
        .lines()
        .position(|line| line.ends_with(&format!("// {marker}")))
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

/// The line the stop is at.
async fn stopped_line(session: &NodeSession) -> Option<u32> {
    let trace = session
        .scenario
        .operation("backtrace", session.scenario.handle().backtrace())
        .await;
    trace.frames.iter().find_map(|frame| {
        frame
            .recorded
            .as_ref()?
            .place
            .as_ref()
            .map(|place| place.line)
    })
}

/// Watches the selected frame's variable `name` for changes.
async fn watch(session: &NodeSession, name: &str) -> microscope::Result<Watchpoint> {
    let handle = session.scenario.handle();
    let expression = Expression::parse(name).expect("an expression");
    let target = handle.resolve_watch_target(&expression).await?;
    assert!(
        matches!(target.scope(), WatchScope::Interpreted { .. }) && target.byte_size() == 8,
        "{}: {target:?}",
        session.context()
    );
    handle
        .add_watchpoint(
            WatchpointSpec::Target(Box::new(target)),
            WatchAccess::Change,
        )
        .await
}

/// Each hit's watchpoint, value before, and value after, and each ended
/// watch's reason, from the stops until the one at `marker`.
async fn run_to(
    session: &mut NodeSession,
    marker: &str,
    events: &mut broadcast::Receiver<DebuggerEvent>,
) -> (
    Vec<(u64, String, String)>,
    Vec<(u64, WatchpointInvalidation)>,
) {
    let context = session.context();
    let mut hits = Vec::new();
    let mut ended = Vec::new();
    let line = u32::try_from(line_of(marker).get()).expect("a small line");
    loop {
        let reason = session.scenario.resume_to_stop().await;
        while let Ok(event) = events.try_recv() {
            if let DebuggerEvent::WatchpointsInvalidated { invalidated, .. } = event {
                ended.extend(
                    invalidated
                        .iter()
                        .map(|entry| (entry.watchpoint.id.get(), entry.reason)),
                );
            }
        }
        match reason {
            StopReason::Watchpoint { hits: found } => hits.extend(found.iter().map(|hit| {
                (
                    hit.watchpoint.get(),
                    hit.previous_value.as_deref().unwrap_or("?").to_owned(),
                    hit.current_value.as_deref().unwrap_or("?").to_owned(),
                )
            })),
            StopReason::WatchpointInvalidated { .. } => {}
            StopReason::Breakpoint { .. } => {
                // The second call stops at its own breakpoint on the way.
                if stopped_line(session).await == Some(line) {
                    return (hits, ended);
                }
            }
            other => panic!("{context}: {other:?}\n{}", session.output()),
        }
    }
}

/// Removes the breakpoint on the line marked `marker`.
async fn remove_breakpoint_at(session: &mut NodeSession, marker: &str) {
    let line = line_of(marker);
    let snapshot = session.scenario.snapshot().await;
    let id = snapshot
        .breakpoints
        .iter()
        .find(|breakpoint| {
            matches!(&breakpoint.spec, BreakpointSpec::Source { line: at, .. } if *at == line)
        })
        .unwrap_or_else(|| panic!("{}: no breakpoint at {marker}", session.context()))
        .id;
    session.scenario.remove_breakpoint(id).await;
}

/// The watchpoints the debugger keeps now.
async fn watchpoints(session: &mut NodeSession) -> Vec<Watchpoint> {
    session.scenario.snapshot().await.watchpoints.to_vec()
}

#[expect(
    clippy::too_many_lines,
    reason = "one run walks each kind of watch in turn"
)]
pub async fn variables_are_watched_for_their_frames_life(entry: &str) {
    let (mut owned, reason, mut events) =
        NodeSession::unlaunched_with_node(entry, FIXTURE, &["--expose-gc", "--stress-compaction"])
            .run_to_stop(
                [
                    "COUNTER",
                    "MOVED",
                    "MOVED_END",
                    "CELLED",
                    "HOT",
                    "SPIN",
                    "DONE",
                ]
                .into_iter()
                .map(breakpoint)
                .collect(),
            )
            .await;
    let session = &mut owned;
    let context = session.context();
    assert!(
        matches!(reason, StopReason::Breakpoint { .. })
            && stopped_line(session).await == u32::try_from(line_of("COUNTER").get()).ok(),
        "{context}: {reason:?}\n{}",
        session.output()
    );

    // The receiver never changes, and a script's binding is kept where
    // no frame reaches it.
    for name in ["this", "globalThis"] {
        let refused = watch(session, name).await;
        assert!(
            matches!(&refused, Err(Error::WatchTargetUnsupported(_))),
            "{context}: `{name}` is refused: {refused:?}"
        );
    }

    // A register of the first call: its watch ends as the second call's
    // frame is built in its place, and reports nothing of that frame.
    let total = watch(session, "total").await.expect("watch total").id.get();
    let (hits, ended) = run_to(session, "COUNTER", &mut events).await;
    assert_eq!(
        hits,
        [
            (total, "0".to_owned(), "1".to_owned()),
            (total, "1".to_owned(), "3".to_owned()),
            (total, "3".to_owned(), "6".to_owned()),
        ],
        "{context}: the register's stores in the first call\n{}",
        session.output()
    );
    assert_eq!(
        ended,
        [(total, WatchpointInvalidation::ScopeExited)],
        "{context}: the watch ends with its call"
    );
    // An argument of the second call, which its body never stores: its
    // watch ends with the call too.
    let rounds = watch(session, "rounds")
        .await
        .expect("watch rounds")
        .id
        .get();
    let (hits, ended) = run_to(session, "MOVED", &mut events).await;
    assert!(hits.is_empty(), "{context}: {hits:?}");
    assert_eq!(
        ended,
        [(rounds, WatchpointInvalidation::ScopeExited)],
        "{context}: the watch ends with its call"
    );
    assert!(
        session.output().contains("COUNTER 6\nCOUNTER 10\n"),
        "{context}: {}",
        session.output()
    );

    // A captured variable, in a context the collections move.
    let a = watch(session, "a").await.expect("watch a");
    let (hits, ended) = run_to(session, "MOVED_END", &mut events).await;
    let id = a.id.get();
    assert_eq!(
        hits,
        [
            (id, "null".to_owned(), "{ round: 1 }".to_owned()),
            (id, "{ round: 1 }".to_owned(), "{ round: 2 }".to_owned()),
            (id, "{ round: 2 }".to_owned(), "{ round: 3 }".to_owned()),
        ],
        "{context}: each store the program made, and nothing a collection rewrote\n{}",
        session.output()
    );
    assert!(ended.is_empty(), "{context}: {ended:?}");
    // The scenario holds only if a collection moved the context.
    let now = watchpoints(session).await;
    let followed = now
        .iter()
        .find(|watchpoint| watchpoint.id == a.id)
        .unwrap_or_else(|| panic!("{context}: the watch of `a` is kept: {now:?}"));
    assert_ne!(
        followed.address, a.address,
        "{context}: a collection moved the context"
    );
    assert!(
        session.output().contains("MOVED 1\nMOVED 2\nMOVED 3\n"),
        "{context}: {}",
        session.output()
    );
    let (hits, ended) = run_to(session, "CELLED", &mut events).await;
    assert!(hits.is_empty(), "{context}: {hits:?}");
    assert_eq!(
        ended,
        [(id, WatchpointInvalidation::ScopeExited)],
        "{context}: the watch of `a` ends with its frame"
    );

    // V8 14.6 keeps a small context's number in a cell, whose fields the
    // number moves between.
    let x = watch(session, "x").await;
    if major(entry) >= 26 {
        assert!(
            matches!(&x, Err(Error::WatchTargetUnsupported(reason)) if reason.contains("context cell")),
            "{context}: {x:?}"
        );
        let (hits, _) = run_to(session, "HOT", &mut events).await;
        assert!(hits.is_empty(), "{context}: {hits:?}");
    } else {
        let id = x.expect("watch x").id.get();
        let (hits, _) = run_to(session, "HOT", &mut events).await;
        assert_eq!(
            hits,
            [(id, "0".to_owned(), "1".to_owned())],
            "{context}: {}",
            session.output()
        );
    }

    // V8 replaces the loop's frame by optimized code, which keeps the
    // argument elsewhere, entering it from Sparkplug's code. V8 optimizes
    // no function with a breakpoint.
    let n = watch(session, "n").await.expect("watch n").id.get();
    remove_breakpoint_at(session, "HOT").await;
    let (hits, ended) = run_to(session, "SPIN", &mut events).await;
    assert!(hits.is_empty(), "{context}: {hits:?}");
    assert_eq!(
        ended,
        [(n, WatchpointInvalidation::FrameReplaced)],
        "{context}: {}",
        session.output()
    );
    assert!(
        session.output().contains("HOT 70000000\n"),
        "{context}: {}",
        session.output()
    );

    // V8 enters the allocating loop's replacement from the interpreter's
    // bytecode handler, whose frame is below the replaced one.
    let start = watch(session, "start").await.expect("watch start").id.get();
    remove_breakpoint_at(session, "SPIN").await;
    let (hits, ended) = run_to(session, "DONE", &mut events).await;
    assert!(hits.is_empty(), "{context}: {hits:?}");
    assert_eq!(
        ended,
        [(start, WatchpointInvalidation::FrameReplaced)],
        "{context}: {}",
        session.output()
    );
    assert!(
        session.output().contains("SPIN 1000006\n"),
        "{context}: {}",
        session.output()
    );
    assert!(watchpoints(session).await.is_empty(), "{context}");
    let snapshot = session.scenario.snapshot().await;
    assert!(
        matches!(snapshot.inferior, InferiorState::Stopped { .. }),
        "{context}"
    );
    owned.shutdown().await;
}

/// A build that does not name V8's collector, as Debian's stripped
/// libnode does not, refuses every watch of a JavaScript variable, which
/// no collection could be followed through.
pub async fn watches_are_refused_without_the_collector(entry: &str) {
    let (mut owned, reason, _) =
        NodeSession::unlaunched_with_node(entry, FIXTURE, &["--expose-gc"])
            .run_to_stop(vec![breakpoint("COUNTER")])
            .await;
    let context = owned.context();
    assert!(
        matches!(reason, StopReason::Breakpoint { .. }),
        "{context}: {reason:?}"
    );
    for name in ["total", "rounds"] {
        let refused = watch(&owned, name).await;
        assert!(
            matches!(&refused, Err(Error::WatchTargetUnsupported(reason)) if reason.contains("collector")),
            "{context}: `{name}`: {refused:?}"
        );
    }
    assert!(watchpoints(&mut owned).await.is_empty(), "{context}");
    owned.shutdown().await;
}

#[tokio::test]
async fn variables_are_watched_for_their_frames_life_on_every_build() {
    for entry in MATRIX {
        variables_are_watched_for_their_frames_life(entry).await;
    }
}
