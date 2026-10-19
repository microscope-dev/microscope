//! Enabling and disabling breakpoints and watchpoints, temporary
//! breakpoints, and running to a location with `advance`.

mod support;

use std::path::PathBuf;
use std::time::{Duration, Instant};

use microscope::{
    Breakpoint, BreakpointHit, BreakpointId, BreakpointLocation, BreakpointOptions, BreakpointSpec,
    DebuggerEvent, ExitStatus, HitCondition, LineNumber, StepKind, StopReason, ThreadState,
    VirtualAddress, WatchAccess, Watchpoint, WatchpointId, WatchpointOptions,
};
use support::Scenario;

const HIT_COUNTS: &str = "tests/fixtures/c/hit-counts.c";

fn condition(text: &str) -> HitCondition {
    text.parse().expect("test hit condition")
}

fn function(name: &str) -> BreakpointSpec {
    BreakpointSpec::Function(name.to_owned())
}

fn source(path: &str, marker: &str) -> BreakpointSpec {
    BreakpointSpec::Source {
        path: PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(path),
        line: LineNumber::new(support::source_line(path, marker)).expect("one-based line"),
    }
}

async fn add(scenario: &Scenario, spec: BreakpointSpec, options: BreakpointOptions) -> Breakpoint {
    scenario
        .operation(
            "add breakpoint",
            scenario.handle().add_breakpoint_with(spec, options),
        )
        .await
}

async fn counting(scenario: &Scenario, name: &str, hits: &str) -> Breakpoint {
    add(
        scenario,
        function(name),
        BreakpointOptions {
            hit_condition: Some(condition(hits)),
            ..BreakpointOptions::default()
        },
    )
    .await
}

async fn set_enabled(scenario: &Scenario, id: BreakpointId, enabled: bool) -> Breakpoint {
    scenario
        .operation(
            "set breakpoint enabled",
            scenario.handle().set_breakpoint_enabled(id, enabled),
        )
        .await
}

async fn breakpoint(scenario: &mut Scenario, id: BreakpointId) -> Option<Breakpoint> {
    scenario
        .snapshot()
        .await
        .breakpoints
        .iter()
        .find(|breakpoint| breakpoint.id == id)
        .cloned()
}

/// Waits until a running program has reached breakpoint `id` `count` times.
async fn wait_for_hits(scenario: &Scenario, id: BreakpointId, count: u64) {
    wait_for(scenario, count, |snapshot| {
        snapshot
            .breakpoints
            .iter()
            .find(|breakpoint| breakpoint.id == id)
            .expect("breakpoint exists")
            .hit_count
    })
    .await;
}

async fn wait_for_watch_hits(scenario: &Scenario, id: WatchpointId, count: u64) {
    wait_for(scenario, count, |snapshot| {
        snapshot
            .watchpoints
            .iter()
            .find(|watchpoint| watchpoint.id == id)
            .expect("watchpoint exists")
            .hit_count
    })
    .await;
}

async fn wait_for(
    scenario: &Scenario,
    count: u64,
    hit_count: impl Fn(&microscope::StateSnapshot) -> u64,
) {
    let deadline = Instant::now() + Duration::from_secs(5);

    loop {
        let snapshot = scenario
            .operation("snapshot", scenario.handle().snapshot())
            .await;
        let reached = hit_count(&snapshot);
        if reached >= count {
            return;
        }

        assert!(
            Instant::now() < deadline,
            "reached {reached} of {count} hits"
        );
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
}

async fn global(scenario: &Scenario, name: &str) -> u64 {
    let handle = scenario.handle();
    let address = scenario.operation(name, handle.runtime_address(name)).await;
    scenario.operation(name, handle.read_word(address)).await
}

async fn line(scenario: &Scenario) -> u64 {
    scenario
        .operation("location", scenario.handle().current_location())
        .await
        .image
        .source
        .expect("stop has a source line")
        .line
        .get()
}

async fn pc(scenario: &Scenario) -> VirtualAddress {
    scenario
        .operation("location", scenario.handle().current_location())
        .await
        .address
}

fn hits(reason: &StopReason) -> &[BreakpointHit] {
    match reason {
        StopReason::Breakpoint { hits, .. } => hits,
        other => panic!("expected a breakpoint stop, got {other:?}"),
    }
}

/// A breakpoint disabled while the program runs counts nothing and stops
/// nowhere, keeps its count and definition, and counts again once enabled.
/// Adding its definition again meanwhile makes a new, enabled breakpoint.
#[tokio::test]
async fn a_breakpoint_disabled_while_running_keeps_its_count_until_enabled() {
    let mut scenario = Scenario::launch("hit-count-spin");
    let spun = counting(&scenario, "spun", "==1000000000").await;
    let running = scenario.start_running().await;
    wait_for_hits(&scenario, spun.id, 100).await;

    let disabled = set_enabled(&scenario, spun.id, false).await;
    assert!(!disabled.enabled);
    assert_eq!(disabled.hit_condition, spun.hit_condition);
    let frozen = disabled.hit_count;

    // A breakpoint that stops 50 hits later shows the program ran on.
    let probe = counting(&scenario, "spun", "==50").await;
    let stop = running.await.expect("run task").expect("run");
    assert!(
        hits(&stop).iter().all(|hit| hit.breakpoint == probe.id),
        "{stop:?}"
    );
    let disabled = breakpoint(&mut scenario, spun.id).await.expect("kept");
    assert_eq!(disabled.hit_count, frozen);
    assert!(!disabled.enabled);
    assert_eq!(disabled.locations, spun.locations, "its last locations");
    scenario.remove_breakpoint(probe.id).await;

    let again = counting(&scenario, "spun", "==1000000000").await;
    assert_ne!(again.id, spun.id, "a disabled breakpoint is not reused");
    assert!(again.enabled);
    scenario.remove_breakpoint(again.id).await;

    let enabled = set_enabled(&scenario, spun.id, true).await;
    assert!(enabled.enabled);
    assert_eq!(enabled.hit_count, frozen);
    let _running = scenario.start_resuming().await;
    wait_for_hits(&scenario, spun.id, frozen + 100).await;
    scenario.shutdown().await;
}

/// A disabled breakpoint owns no trap, so its library's code moving takes
/// none along and leaves its locations where the code was. Enabling it
/// resolves it again where the code went rather than writing a trap where
/// the code used to be.
#[tokio::test]
async fn enabling_a_breakpoint_resolves_it_where_its_code_moved() {
    let mut scenario = Scenario::launch("moved-code");
    let answer = add(
        &scenario,
        function("moved_answer"),
        BreakpointOptions {
            pending: true,
            ..BreakpointOptions::default()
        },
    )
    .await;
    let reason = scenario.run_to_stop().await;
    let library = |modules: &microscope::LoadedModuleSnapshot| {
        modules
            .modules
            .iter()
            .find(|record| record.path.ends_with("libmoved-code.so"))
            .expect("the library is loaded")
            .module
            .load_bias
            .get()
    };

    let modules = scenario
        .operation("modules", scenario.handle().loaded_modules())
        .await;
    let first = library(&modules);
    let offset = support::breakpoint_address(&reason).get() - first;
    set_enabled(&scenario, answer.id, false).await;

    // Stop once the library's code has moved.
    scenario
        .add_breakpoint_spec(source(
            "tests/fixtures/c/moved-code/main.c",
            "int (*moved)(int)",
        ))
        .await;
    assert!(matches!(
        scenario.resume_to_stop().await,
        StopReason::Breakpoint { .. }
    ));
    let modules = scenario
        .operation("modules", scenario.handle().loaded_modules())
        .await;
    assert_ne!(library(&modules), first, "the code moved");
    let moved = library(&modules) + offset;

    let enabled = set_enabled(&scenario, answer.id, true).await;
    let locations = enabled
        .locations
        .iter()
        .map(|location| location.location.clone())
        .collect::<Vec<_>>();
    assert_eq!(
        locations,
        [BreakpointLocation::Virtual(VirtualAddress::new(moved))]
    );
    let reason = scenario.resume_to_stop().await;
    assert_eq!(support::breakpoint_address(&reason).get(), moved);
    assert_eq!(hits(&reason)[0].hit_count, 2);
    scenario.remove_all_breakpoints().await;
    assert_eq!(
        scenario.resume_to_stop().await,
        StopReason::Exited(ExitStatus::Code(0))
    );
    scenario.shutdown().await;
}

/// Stores made while a change watchpoint was disabled are not reported once
/// it is enabled again: it takes the bytes it then finds as the last
/// observed value. Every store here repeats the value they made.
#[tokio::test]
async fn a_watchpoint_enabled_again_reports_no_change_made_while_disabled() {
    let mut scenario = Scenario::launch("watch-steady-spin");
    scenario.add_breakpoint("store_steady").await;
    scenario.run_to_stop().await;
    scenario.remove_all_breakpoints().await;
    let handle = scenario.handle().clone();
    let watch = |access, options| {
        let expression = microscope::Expression::parse("steady").expect("expression");
        let handle = handle.clone();
        async move { handle.watch_with(&expression, access, options).await }
    };

    // Counts every store without stopping, to show the program ran on.
    let stores = scenario
        .operation(
            "watch stores",
            watch(
                WatchAccess::Write,
                WatchpointOptions {
                    condition: Some(
                        microscope::Condition::parse("steady != 7").expect("condition"),
                    ),
                    ..WatchpointOptions::default()
                },
            ),
        )
        .await;
    let changes: Watchpoint = scenario
        .operation(
            "watch changes",
            watch(WatchAccess::Change, WatchpointOptions::default()),
        )
        .await;
    let handle = scenario.handle().clone();
    let set_enabled = |enabled| {
        let handle = handle.clone();
        async move { handle.set_watchpoint_enabled(changes.id, enabled).await }
    };

    assert!(
        !scenario
            .operation("disable", set_enabled(false))
            .await
            .enabled
    );
    let steady = scenario
        .operation("steady", scenario.handle().runtime_address("steady"))
        .await;
    scenario
        .operation(
            "write steady",
            scenario.handle().write_memory(steady, &5_u64.to_le_bytes()),
        )
        .await;

    // The workers store 7 over the 5 while the watchpoint is disabled.
    let running = scenario.start_resuming().await;
    wait_for_watch_hits(&scenario, stores.id, 100).await;
    let enabled = scenario.operation("enable", set_enabled(true)).await;
    assert!(enabled.enabled);
    let at_enable = scenario
        .operation("snapshot", scenario.handle().snapshot())
        .await
        .watchpoints
        .iter()
        .find(|watchpoint| watchpoint.id == stores.id)
        .expect("watchpoint")
        .hit_count;
    wait_for_watch_hits(&scenario, stores.id, at_enable + 100).await;
    scenario.operation("pause", scenario.handle().pause()).await;
    assert_eq!(
        running.await.expect("resume task").expect("resume"),
        StopReason::Pause
    );
    let changes = scenario
        .snapshot()
        .await
        .watchpoints
        .iter()
        .find(|watchpoint| watchpoint.id == changes.id)
        .cloned()
        .expect("watchpoint");
    assert_eq!(changes.hit_count, 0);
    scenario.shutdown().await;
}

/// A temporary breakpoint survives the hits its hit condition declines and
/// is deleted by the stop it causes, which clients hear of before the
/// change to the breakpoints.
#[tokio::test]
async fn a_temporary_breakpoint_is_deleted_by_the_stop_it_causes() {
    let mut scenario = Scenario::launch("hit-counts-gcc-o0");
    let temporary = add(
        &scenario,
        function("counted"),
        BreakpointOptions {
            hit_condition: Some(condition("==3")),
            temporary: true,
            ..BreakpointOptions::default()
        },
    )
    .await;
    assert!(temporary.temporary);
    let mut events = scenario.handle().subscribe();
    let reason = scenario.run_to_stop().await;
    assert_eq!(
        hits(&reason),
        [BreakpointHit {
            breakpoint: temporary.id,
            hit_count: 3,
        }]
    );
    assert_eq!(global(&scenario, "last_call").await, 2);
    assert!(breakpoint(&mut scenario, temporary.id).await.is_none());
    let order = std::iter::from_fn(|| events.try_recv().ok())
        .filter_map(|event| match event {
            DebuggerEvent::InferiorStopped { .. } => Some("stopped"),
            DebuggerEvent::BreakpointsChanged { .. } => Some("changed"),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(order.last_chunk(), Some(&["stopped", "changed"]));
    assert_eq!(
        scenario.resume_to_stop().await,
        StopReason::Exited(ExitStatus::Code(0))
    );
    scenario.shutdown().await;
}

/// Every thread that reached a temporary breakpoint by its stop is reported
/// in that one stop, and each runs on unharmed once the trap is gone.
#[tokio::test]
async fn threads_reaching_a_temporary_breakpoint_together_share_its_stop() {
    let mut scenario = Scenario::launch("hit-count-threads");
    let temporary = add(
        &scenario,
        function("contended"),
        BreakpointOptions {
            temporary: true,
            ..BreakpointOptions::default()
        },
    )
    .await;
    let reason = scenario.run_to_stop().await;
    assert_eq!(hits(&reason)[0].breakpoint, temporary.id);
    let snapshot = scenario.snapshot().await;
    assert!(snapshot.breakpoints.is_empty());

    for thread in snapshot.threads.iter() {
        if let ThreadState::Stopped {
            reason: Some(reason @ StopReason::Breakpoint { .. }),
        } = &thread.state
        {
            assert_eq!(hits(reason)[0].breakpoint, temporary.id);
        }
    }

    // The program checks every call ran exactly once.
    assert_eq!(
        scenario.resume_to_stop().await,
        StopReason::Exited(ExitStatus::Code(0))
    );
    scenario.shutdown().await;
}

/// `advance` stops where its location is reached, or where the selected
/// frame returns first, and creates nothing a client sees.
#[tokio::test]
async fn advance_runs_to_a_location_or_the_frames_return() {
    let mut scenario = Scenario::launch("hit-counts-gcc-o0");
    let loop_call = scenario
        .add_breakpoint_spec(source(HIT_COUNTS, "caller(call);"))
        .await;
    scenario.run_to_stop().await;
    scenario.remove_breakpoint(loop_call.id).await;

    // From the location itself, it runs until the location comes round.
    let at = line(&scenario).await;
    assert_eq!(
        scenario
            .advance_to_stop(source(HIT_COUNTS, "caller(call);"))
            .await,
        StopReason::Step {
            kind: StepKind::Advance
        }
    );
    assert_eq!(line(&scenario).await, at);
    assert_eq!(global(&scenario, "last_call").await, 1);

    assert_eq!(
        scenario.advance_to_stop(function("counted")).await,
        StopReason::Step {
            kind: StepKind::Advance
        }
    );
    assert_eq!(global(&scenario, "last_call").await, 1);

    // `main` is never reached again, so `counted` returns first.
    assert_eq!(
        scenario.advance_to_stop(function("main")).await,
        StopReason::Step {
            kind: StepKind::Out
        }
    );
    assert_eq!(global(&scenario, "last_call").await, 2);
    assert_eq!(
        line(&scenario).await,
        support::source_line(HIT_COUNTS, "counted(call);")
    );
    assert!(scenario.snapshot().await.breakpoints.is_empty());
    scenario.shutdown().await;
}

/// A thread that single-stepped onto a new breakpoint has yet to arrive
/// there, so an advance from it counts that arrival before going on.
#[tokio::test]
async fn advance_from_a_new_breakpoint_counts_the_arrival_it_stands_at() {
    let mut scenario = Scenario::launch("hit-counts-gcc-o0");
    let loop_call = scenario
        .add_breakpoint_spec(source(HIT_COUNTS, "caller(call);"))
        .await;
    scenario.run_to_stop().await;
    scenario.remove_breakpoint(loop_call.id).await;
    let at = pc(&scenario).await;
    scenario.step_to_stop(StepKind::OverInstruction).await;

    while pc(&scenario).await != at {
        scenario.step_to_stop(StepKind::OverInstruction).await;
    }

    let declining = add(
        &scenario,
        source(HIT_COUNTS, "caller(call);"),
        BreakpointOptions {
            condition: Some(microscope::Condition::parse("call > 3").expect("condition")),
            ..BreakpointOptions::default()
        },
    )
    .await;
    assert_eq!(
        scenario
            .advance_to_stop(source(HIT_COUNTS, "caller(call);"))
            .await,
        StopReason::Step {
            kind: StepKind::Advance
        }
    );
    assert_eq!(global(&scenario, "last_call").await, 2);
    let declining = breakpoint(&mut scenario, declining.id)
        .await
        .expect("still set");
    assert_eq!(declining.hit_count, 2);
    scenario.shutdown().await;
}

/// A breakpoint met on the way ends an advance, and the advance leaves no
/// trap behind: the program then runs to its end.
#[tokio::test]
async fn advance_ends_at_a_breakpoint_on_the_way() {
    let mut scenario = Scenario::launch("hit-counts-gcc-o0");
    scenario.add_breakpoint("caller").await;
    scenario.run_to_stop().await;
    let counted = scenario.add_breakpoint("counted").await;
    let reason = scenario
        .advance_to_stop(source(HIT_COUNTS, "shared(call + SECOND_SITE_OFFSET);"))
        .await;
    assert_eq!(hits(&reason)[0].breakpoint, counted.id);
    scenario.remove_all_breakpoints().await;
    assert_eq!(
        scenario.resume_to_stop().await,
        StopReason::Exited(ExitStatus::Code(0))
    );
    scenario.shutdown().await;
}
