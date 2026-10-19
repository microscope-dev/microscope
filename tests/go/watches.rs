//! A goroutine's local watched while the runtime moves its stack. The
//! watch follows the local to each new stack, sees every store to it from
//! its own goroutine and none from its siblings', never stops in the
//! runtime's copy, and ends when the function returns.

use std::collections::BTreeSet;
use std::process::Stdio;

use microscope::{
    DebuggerEvent, LaunchOptions, StopReason, WatchAccess, WatchScope, Watchpoint,
    WatchpointInvalidation,
};

use crate::stops::{address, integer};
use crate::support::Scenario;

const BUILDS: [&str; 2] = ["watched-go-o0", "watched-go-o2"];

/// A program stopped where a goroutine's `watched` first calls `bump`,
/// with the goroutine's `counter` watched, and a breakpoint after every
/// goroutine has returned.
async fn watching(fixture: &str, access: WatchAccess) -> (Scenario, Watchpoint, i128, u64) {
    let mut scenario = crate::invariants::checked(fixture);
    let bump = scenario.add_breakpoint("main.bump").await;
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
    scenario.remove_breakpoint(bump.id).await;
    let task = integer(&scenario, "$task").await.expect("a goroutine");
    select_caller(&scenario).await;
    let before = address(&scenario, "&counter")
        .await
        .expect("counter's address");
    let expression = microscope::Expression::parse("counter").expect("parses");
    let watchpoint = scenario
        .operation(
            "watch counter",
            scenario.handle().watch(&expression, access),
        )
        .await;
    let WatchScope::Task { task: owner, .. } = watchpoint.scope else {
        panic!("{fixture}: {:?}", watchpoint.scope);
    };

    assert_eq!(i128::from(owner.number), task, "{fixture}");
    assert_eq!(watchpoint.address.get(), before, "{fixture}");

    scenario.add_breakpoint("main.finished").await;
    (scenario, watchpoint, task, before)
}

/// Selects the innermost frame's caller.
async fn select_caller(scenario: &Scenario) {
    let trace = scenario
        .operation("backtrace", scenario.handle().backtrace())
        .await;
    let caller = trace.frames.get(1).expect("a caller").id;
    scenario
        .operation("select the caller", scenario.handle().select_frame(caller))
        .await;
}

#[tokio::test]
async fn a_watched_local_follows_its_stack_until_its_function_returns() {
    for fixture in BUILDS {
        for access in [WatchAccess::Change, WatchAccess::ReadWrite] {
            let context = format!("{fixture} {access:?}");
            let (mut scenario, watchpoint, task, before) = watching(fixture, access).await;
            let mut events = scenario.handle().subscribe();
            let mut values = Vec::new();
            let mut addresses = BTreeSet::from([before]);

            let reason = loop {
                match scenario.resume_to_stop().await {
                    StopReason::Watchpoint { hits } => {
                        let [hit] = hits.as_ref() else {
                            panic!("{context}: one hit per stop: {hits:?}");
                        };

                        assert_eq!(hit.watchpoint, watchpoint.id, "{context}");

                        // Only the goroutine's own store, never the runtime
                        // copying its stack.
                        assert_eq!(integer(&scenario, "$task").await, Some(task), "{context}");
                        let trace = scenario
                            .operation("backtrace", scenario.handle().backtrace())
                            .await;
                        let function = trace.frames[0]
                            .function
                            .as_ref()
                            .map(|function| function.name.to_string());

                        // The store in bump, or, read too, watched returning
                        // the local.
                        let functions: &[&str] = match access {
                            WatchAccess::ReadWrite => &["main.bump", "main.watched"],
                            _ => &["main.bump"],
                        };

                        assert!(
                            function
                                .as_deref()
                                .is_some_and(|name| functions.contains(&name)),
                            "{context}: {function:?}"
                        );

                        let bytes = hit.current.as_deref().expect("the local is readable");
                        let mut word = [0; 8];
                        word.copy_from_slice(bytes);
                        values.push(u64::from_le_bytes(word));

                        // The watch is where the local is now.
                        if function.as_deref() == Some("main.bump") {
                            select_caller(&scenario).await;
                        }

                        let now = address(&scenario, "&counter").await.expect("an address");
                        let watched = scenario.snapshot().await.watchpoints[0].address.get();
                        assert_eq!(watched, now, "{context}");
                        addresses.insert(now);
                    }
                    StopReason::WatchpointInvalidated { invalidated } => {
                        let [entry] = invalidated.as_ref() else {
                            panic!("{context}: one invalidation: {invalidated:?}");
                        };

                        break entry.reason;
                    }
                    StopReason::Breakpoint { .. } => {
                        break std::iter::from_fn(|| events.try_recv().ok())
                            .find_map(|event| match event {
                                DebuggerEvent::WatchpointsInvalidated { invalidated, .. } => {
                                    invalidated.first().map(|entry| entry.reason)
                                }
                                _ => None,
                            })
                            .unwrap_or_else(|| panic!("{context}: the watch outlived its frame"));
                    }

                    // Go preempts its threads with SIGURG.
                    StopReason::Exception(exception) if exception.code == 23 => {}
                    other => panic!("{context}: {other:?}"),
                }
            };

            assert_eq!(reason, WatchpointInvalidation::ScopeExited, "{context}");
            assert!(
                scenario.snapshot().await.watchpoints.is_empty(),
                "{context}"
            );

            // Each round adds its number; reading the local stops too.
            if access == WatchAccess::Change {
                assert_eq!(values, [1, 3, 6, 10], "{context}");
            } else {
                assert!(values.len() >= 4, "{context}: {values:?}");
            }

            assert!(
                addresses.len() > 1,
                "{context}: the stack never moved from {before:#x}"
            );
            scenario.shutdown().await;
        }
    }
}

/// A disabled watch holds no debug registers, which the controller's
/// invariant check enforces after every message, as its stack moves.
#[tokio::test]
async fn a_disabled_watch_stays_disarmed_while_its_stack_moves() {
    for fixture in BUILDS {
        let (mut scenario, watchpoint, _, _) = watching(fixture, WatchAccess::Change).await;
        let disabled = scenario
            .operation(
                "disable the watch",
                scenario
                    .handle()
                    .set_watchpoint_enabled(watchpoint.id, false),
            )
            .await;
        assert!(!disabled.enabled, "{fixture}");

        loop {
            match scenario.resume_to_stop().await {
                StopReason::Breakpoint { .. } => break,
                // The watch's function returning ends it.
                StopReason::WatchpointInvalidated { .. } => {}

                // Go preempts its threads with SIGURG.
                StopReason::Exception(exception) if exception.code == 23 => {}
                other => panic!("{fixture}: a disabled watch stopped the program: {other:?}"),
            }
        }

        scenario.shutdown().await;
    }
}
