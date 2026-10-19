//! Conditions and hit conditions on watchpoints: which of a watchpoint's
//! hits stop, how hits are counted, and that hits which do not stop are
//! invisible to steps, pauses, edits, and detaching.

mod support;

use std::sync::Arc;
use std::time::{Duration, Instant};

use microscope::{
    Condition, ConditionOwner, DebuggerEvent, Error, Evaluation, ExitStatus, HitCondition,
    ScalarValue, StopReason, ThreadState, VariableState, VariableValue, WatchAccess, Watchpoint,
    WatchpointHit, WatchpointId, WatchpointOptions,
};
use support::Scenario;

/// The single-threaded hit-count program across the compiler,
/// optimization, and PIE matrix: what the compiler decides is where the
/// condition's variables live after each store.
const COUNTING: [&str; 4] = [
    "hit-counts-gcc-o0",
    "hit-counts-clang-o0",
    "hit-counts-clang-o2",
    "hit-counts-gcc-o2-nopie",
];

fn expression(text: &str) -> microscope::Expression {
    microscope::Expression::parse(text).expect("valid test expression")
}

fn hit_condition(text: &str) -> HitCondition {
    text.parse().expect("test hit condition")
}

fn condition(text: &str) -> Condition {
    Condition::parse(text).expect("test condition")
}

async fn watch(
    scenario: &Scenario,
    text: &str,
    access: WatchAccess,
    options: WatchpointOptions,
) -> Watchpoint {
    scenario
        .operation(
            &format!("watch {text}"),
            scenario
                .handle()
                .watch_with(&expression(text), access, options),
        )
        .await
}

async fn watchpoint(scenario: &mut Scenario, id: WatchpointId) -> Watchpoint {
    scenario
        .snapshot()
        .await
        .watchpoints
        .iter()
        .find(|watchpoint| watchpoint.id == id)
        .cloned()
        .expect("watchpoint exists")
}

/// Waits until a running program has hit watchpoint `id` `count` times.
async fn wait_for_hits(scenario: &Scenario, id: WatchpointId, count: u64) {
    let deadline = Instant::now() + Duration::from_secs(5);

    loop {
        let snapshot = scenario
            .operation("snapshot", scenario.handle().snapshot())
            .await;
        let hit_count = snapshot
            .watchpoints
            .iter()
            .find(|watchpoint| watchpoint.id == id)
            .expect("watchpoint exists")
            .hit_count;
        if hit_count >= count {
            return;
        }

        assert!(
            Instant::now() < deadline,
            "watchpoint {id} reached {hit_count} of {count} hits"
        );
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
}

/// Every hit reported by any thread at the current stop, from each thread's
/// own stop reason.
async fn thread_hits(scenario: &mut Scenario) -> Vec<WatchpointHit> {
    scenario
        .snapshot()
        .await
        .threads
        .iter()
        .filter_map(|thread| match &thread.state {
            ThreadState::Stopped {
                reason: Some(StopReason::Watchpoint { hits }),
            } => Some(hits.to_vec()),
            _ => None,
        })
        .flatten()
        .collect()
}

fn hits(reason: &StopReason) -> &[WatchpointHit] {
    match reason {
        StopReason::Watchpoint { hits } => hits,
        other => panic!("expected a watchpoint stop, got {other:?}"),
    }
}

/// Decodes little-endian watched bytes.
fn value(bytes: Option<&Arc<[u8]>>) -> u64 {
    let bytes = bytes.expect("watched bytes are readable");
    let mut word = [0_u8; 8];
    word[..bytes.len()].copy_from_slice(bytes);
    u64::from_le_bytes(word)
}

/// Whether a condition holds in the selected frame of the current stop.
async fn holds(scenario: &Scenario, text: &str) -> bool {
    match scenario
        .operation(text, scenario.handle().evaluate(&expression(text)))
        .await
    {
        Evaluation::Value { value, .. } => matches!(
            value.state,
            VariableState::Available {
                value: VariableValue::Scalar(ScalarValue::Boolean(true)),
                ..
            }
        ),
        other => panic!("{text} has no value: {other:?}"),
    }
}

async fn global(scenario: &Scenario, name: &str) -> u64 {
    let handle = scenario.handle();
    let address = scenario.operation(name, handle.runtime_address(name)).await;
    scenario.operation(name, handle.read_word(address)).await
}

/// A watched hit as the program determines it: the watchpoint, the hit's
/// number, and the value before and after the access.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Expected {
    watchpoint: WatchpointId,
    hit_count: u64,
    previous: u64,
    current: u64,
}

#[tokio::test]
async fn conditions_and_hit_conditions_choose_which_hits_stop() {
    for fixture in COUNTING {
        let mut scenario = Scenario::launch(fixture);
        scenario.add_breakpoint("caller").await;
        assert!(matches!(
            scenario.run_to_stop().await,
            StopReason::Breakpoint { .. }
        ));
        scenario.remove_all_breakpoints().await;

        // The condition reads the storing function's parameter after the
        // store; the hit condition counts every changing store, including
        // the second of the two inlined additions per call.
        let calls = watch(
            &scenario,
            "last_call",
            WatchAccess::Write,
            WatchpointOptions {
                condition: Some(condition("call % 10 == 0")),
                ..WatchpointOptions::default()
            },
        )
        .await;
        let totals = watch(
            &scenario,
            "shared_total",
            WatchAccess::Change,
            WatchpointOptions {
                hit_condition: Some(hit_condition("%7")),
                ..WatchpointOptions::default()
            },
        )
        .await;
        assert_eq!(calls.condition, Some(condition("call % 10 == 0")));
        assert_eq!(totals.hit_condition, Some(hit_condition("%7")));

        // A declined hit's value is the one the next hit changes, as gdb's
        // old value is.
        let mut expected = Vec::new();
        let (mut last_call, mut total) = (0, 0);
        let (mut call_hits, mut total_hits) = (0, 0);

        for call in 1..=40 {
            call_hits += 1;
            if call % 10 == 0 {
                expected.push(Expected {
                    watchpoint: calls.id,
                    hit_count: call_hits,
                    previous: last_call,
                    current: call,
                });
            }

            last_call = call;
            for added in [call, call + 1000] {
                total_hits += 1;
                if total_hits % 7 == 0 {
                    expected.push(Expected {
                        watchpoint: totals.id,
                        hit_count: total_hits,
                        previous: total,
                        current: total + added,
                    });
                }

                total += added;
            }
        }

        let mut stops = Vec::new();

        loop {
            let reason = scenario.resume_to_stop().await;
            if reason == StopReason::Exited(ExitStatus::Code(0)) {
                break;
            }

            let [hit] = hits(&reason) else {
                panic!("{fixture}: one hit per stop: {reason:?}");
            };

            stops.push(Expected {
                watchpoint: hit.watchpoint,
                hit_count: hit.hit_count,
                previous: value(hit.previous.as_ref()),
                current: value(hit.current.as_ref()),
            });

            // One thread, so the count is the stop's hit.
            assert_eq!(
                watchpoint(&mut scenario, hit.watchpoint).await.hit_count,
                hit.hit_count,
                "{fixture}"
            );
            if hit.watchpoint == calls.id {
                assert!(holds(&scenario, "call % 10 == 0").await, "{fixture}");
            }
        }

        assert_eq!(stops, expected, "{fixture}");
        scenario.shutdown().await;
    }
}

#[tokio::test]
async fn a_condition_that_cannot_be_evaluated_stops_and_says_why() {
    let mut scenario = Scenario::launch(COUNTING[0]);
    scenario.add_breakpoint("caller").await;
    scenario.run_to_stop().await;
    scenario.remove_all_breakpoints().await;
    let watched = watch(
        &scenario,
        "last_call",
        WatchAccess::Write,
        WatchpointOptions {
            condition: Some(condition("no_such_value > 1")),
            ..WatchpointOptions::default()
        },
    )
    .await;

    let mut events = scenario.handle().subscribe();
    let reason = scenario.resume_to_stop().await;
    let [hit] = hits(&reason) else {
        panic!("one hit: {reason:?}");
    };

    assert_eq!((hit.watchpoint, hit.hit_count), (watched.id, 1));
    let failures = std::iter::from_fn(|| events.try_recv().ok())
        .filter_map(|event| match event {
            DebuggerEvent::ConditionFailed { owner, error, .. } => Some((owner, error)),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        failures,
        [(
            ConditionOwner::Watchpoint(watched.id),
            Arc::from("no variable is named `no_such_value` here")
        )]
    );

    // Conditions are amended at a stop, keeping the count, and requests
    // for a watchpoint that does not exist fail without changing anything.
    let handle = scenario.handle();
    let amended = scenario
        .operation(
            "clear condition",
            handle.set_watchpoint_condition(watched.id, None),
        )
        .await;
    assert_eq!((amended.condition, amended.hit_count), (None, 1));
    let amended = scenario
        .operation(
            "stop at the fifth hit",
            handle.set_watchpoint_hit_condition(watched.id, Some(hit_condition("==5"))),
        )
        .await;
    assert_eq!(amended.hit_condition, Some(hit_condition("==5")));
    for result in [
        handle
            .set_watchpoint_condition(WatchpointId::new(99), Some(condition("1 == 1")))
            .await,
        handle
            .set_watchpoint_hit_condition(WatchpointId::new(99), None)
            .await,
    ] {
        assert!(
            matches!(result, Err(Error::WatchpointNotFound(99))),
            "{result:?}"
        );
    }

    assert_eq!(watchpoint(&mut scenario, watched.id).await, amended);

    let reason = scenario.resume_to_stop().await;
    let [hit] = hits(&reason) else {
        panic!("one hit: {reason:?}");
    };

    assert_eq!(hit.hit_count, 5);
    assert_eq!(
        value(hit.previous.as_ref()),
        4,
        "declined hits were observed"
    );
    assert_eq!(value(hit.current.as_ref()), 5);
    assert_eq!(
        scenario.resume_to_stop().await,
        StopReason::Exited(ExitStatus::Code(0))
    );
    scenario.shutdown().await;
}

/// Four workers each add 1 through 100 to one total at once. Every store
/// is a hit, and only the hits whose thread added 50 stop.
#[tokio::test]
async fn concurrent_hits_are_each_counted_once_and_judged_in_their_thread() {
    const WORKERS: usize = 4;
    const CALLS: u64 = 100;

    let mut scenario = Scenario::launch("hit-count-threads");
    scenario.add_breakpoint("main").await;
    scenario.run_to_stop().await;
    scenario.remove_all_breakpoints().await;
    let watched = watch(
        &scenario,
        "total",
        WatchAccess::Write,
        WatchpointOptions {
            condition: Some(condition("value == 50")),
            ..WatchpointOptions::default()
        },
    )
    .await;
    let joined = support::source_line(
        "tests/fixtures/c/hit-count-threads.c",
        "uint64_t expected =",
    );
    scenario
        .add_source_breakpoint("hit-count-threads.c", joined)
        .await;

    // Workers that hit while every thread stopped report in their own
    // thread's reason.
    let mut stopped = Vec::new();

    let reason = loop {
        let reason = scenario.resume_to_stop().await;
        if !matches!(reason, StopReason::Watchpoint { .. }) {
            break reason;
        }

        let reported = thread_hits(&mut scenario).await;
        assert!(reported.iter().any(|hit| hits(&reason).contains(hit)));
        for hit in reported {
            assert_eq!(hit.watchpoint, watched.id);
            scenario
                .operation(
                    "select hitting thread",
                    scenario.handle().select_context(hit.thread),
                )
                .await;
            assert!(holds(&scenario, "value == 50").await, "{hit:?}");
            stopped.push((hit.thread, hit.hit_count));
        }
    };

    assert!(
        matches!(reason, StopReason::Breakpoint { .. }),
        "{reason:?}"
    );

    // Each worker stops once, at its own hit's number.
    let mut threads = stopped
        .iter()
        .map(|(thread, _)| *thread)
        .collect::<Vec<_>>();
    threads.sort_unstable();
    threads.dedup();
    assert_eq!(threads.len(), WORKERS, "{stopped:?}");
    assert_eq!(stopped.len(), WORKERS, "{stopped:?}");
    let mut numbers = stopped.iter().map(|(_, hit)| *hit).collect::<Vec<_>>();
    numbers.sort_unstable();
    numbers.dedup();
    assert_eq!(
        numbers.len(),
        WORKERS,
        "each hit has its own number: {stopped:?}"
    );
    assert_eq!(
        watchpoint(&mut scenario, watched.id).await.hit_count,
        WORKERS as u64 * CALLS
    );
    scenario.shutdown().await;
}

#[tokio::test]
async fn pause_amend_and_shutdown_interrupt_endless_declined_hits() {
    let mut scenario = Scenario::launch("watch-steady-spin");
    scenario.add_breakpoint("store_steady").await;
    scenario.run_to_stop().await;
    scenario.remove_all_breakpoints().await;

    // Every store keeps the value at 7, so every hit is declined.
    let watched = watch(
        &scenario,
        "steady",
        WatchAccess::Write,
        WatchpointOptions {
            condition: Some(condition("steady != 7")),
            ..WatchpointOptions::default()
        },
    )
    .await;

    // A pause lands among declined hits and is reported as a pause.
    let running = scenario.start_resuming().await;
    wait_for_hits(&scenario, watched.id, 100).await;
    scenario.operation("pause", scenario.handle().pause()).await;
    assert_eq!(
        running.await.expect("resume task").expect("resume"),
        StopReason::Pause
    );
    let at_pause = watchpoint(&mut scenario, watched.id).await.hit_count;
    let stores = global(&scenario, "stores").await;

    // A store is counted before the thread counts it in `stores`.
    assert!(
        stores <= at_pause && at_pause <= stores + 4,
        "{stores} stores, {at_pause} hits"
    );

    // An amended condition applies to the running program's next hit.
    let resumed = scenario.start_resuming().await;
    let amended = scenario
        .operation(
            "amend while running",
            scenario.handle().set_watchpoint_condition(watched.id, None),
        )
        .await;
    assert!(amended.hit_count >= at_pause);
    let reason = resumed.await.expect("resume task").expect("resume");
    let [hit] = hits(&reason) else {
        panic!("one hit: {reason:?}");
    };

    assert_eq!(hit.watchpoint, watched.id);
    assert!(hit.hit_count > amended.hit_count);
    assert_eq!(
        (value(hit.previous.as_ref()), value(hit.current.as_ref())),
        (7, 7)
    );

    // Shutting down while hits are being declined reaps the inferior.
    scenario
        .operation(
            "never stop",
            scenario
                .handle()
                .set_watchpoint_hit_condition(watched.id, Some(hit_condition("==1000000000"))),
        )
        .await;
    let _running = scenario.start_resuming().await;
    wait_for_hits(&scenario, watched.id, hit.hit_count + 100).await;
    scenario.shutdown().await;
}

#[tokio::test]
async fn attached_processes_decline_hits_and_detach_mid_run_unharmed() {
    let child = support::ExternalProcess::spawn_running(&Scenario::fixture("watch-steady-spin"));
    let mut scenario = Scenario::attached("attached watch conditions", child.attach().await);
    let watched = watch(
        &scenario,
        "steady",
        WatchAccess::Write,
        WatchpointOptions {
            hit_condition: Some(hit_condition("%50")),
            ..WatchpointOptions::default()
        },
    )
    .await;
    for stop in 1..=3 {
        let reason = scenario.resume_to_stop().await;
        let [hit] = hits(&reason) else {
            panic!("one hit: {reason:?}");
        };

        assert_eq!((hit.watchpoint, hit.hit_count), (watched.id, 50 * stop));
    }

    let stores = global(&scenario, "stores").await;

    // Detaching while hits are being declined disarms every thread first:
    // a leftover debug register would kill the process with SIGTRAP.
    scenario
        .operation(
            "never stop",
            scenario
                .handle()
                .set_watchpoint_hit_condition(watched.id, Some(hit_condition("==1000000000"))),
        )
        .await;
    let _running = scenario.start_resuming().await;
    wait_for_hits(&scenario, watched.id, 350).await;
    scenario.shutdown().await;
    let mut reattached = Scenario::attached("reattached", child.attach().await);
    let snapshot = reattached.snapshot().await;
    assert_eq!(snapshot.threads.len(), 4, "every thread survived");
    assert!(snapshot.watchpoints.is_empty());
    assert!(global(&reattached, "stores").await >= stores + 190);
    reattached.shutdown().await;
}

/// Runs the watch fixture to a function breakpoint through the signals its
/// earlier phases raise.
async fn run_to(scenario: &mut Scenario, function: &str) {
    scenario.add_breakpoint(function).await;
    let mut reason = scenario.run_to_stop().await;
    while let StopReason::Exception(exception) = &reason {
        assert!(matches!(exception.code, 10 | 17), "{reason:?}");
        reason = scenario.resume_to_stop().await;
    }

    assert!(
        matches!(reason, StopReason::Breakpoint { .. }),
        "{reason:?}"
    );
}

/// A breakpoint on a watched store is stepped over by executing the store,
/// whose declined hit completes the step over as if unwatched.
#[tokio::test]
async fn a_declined_store_stepped_over_at_a_breakpoint_is_transparent() {
    for kind in [None, Some(microscope::StepKind::Instruction)] {
        let mut scenario = Scenario::launch("watch-gcc-o0");
        run_to(&mut scenario, "store_then_breakpoint").await;
        let site = scenario
            .operation(
                "resolve site",
                scenario.handle().runtime_address("watched_store_site"),
            )
            .await;
        scenario
            .add_breakpoint_spec(microscope::BreakpointSpec::Address(site))
            .await;
        assert_eq!(
            support::breakpoint_address(&scenario.resume_to_stop().await),
            site
        );
        let watched = watch(
            &scenario,
            "watch_i32",
            WatchAccess::Write,
            WatchpointOptions {
                condition: Some(condition("watch_i32 != 5")),
                ..WatchpointOptions::default()
            },
        )
        .await;

        if let Some(kind) = kind {
            assert_eq!(scenario.step_to_stop(kind).await, StopReason::Step { kind });
            assert_eq!(watchpoint(&mut scenario, watched.id).await.hit_count, 1);
        }

        // The next store holds the condition, and changes the value the
        // declined store left.
        let reason = scenario.resume_to_stop().await;
        let [hit] = hits(&reason) else {
            panic!("one hit: {reason:?}");
        };

        assert_eq!(hit.hit_count, 2);
        assert_eq!(
            (value(hit.previous.as_ref()), value(hit.current.as_ref())),
            (5, 77)
        );
        scenario.shutdown().await;
    }
}
