mod support;

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use microscope::{
    BreakpointSpec, DebuggerEvent, Error, ExitStatus, InferiorState, RegisterRole, StepKind,
    StopReason, ThreadId, ThreadState, VirtualAddress, WatchAccess, WatchScope, Watchpoint,
    WatchpointHit, WatchpointId, WatchpointInvalidation, WatchpointOptions, WatchpointSpec,
};
use support::Scenario;

/// The single-threaded fixture across the compiler, optimization, and PIE
/// matrix, for what the compiler decides: where variables live and how
/// steps cross its code. Each phase function performs one kind of access.
const MATRIX: [&str; 3] = ["watch-gcc-o0", "watch-clang-o2", "watch-gcc-o2-nopie"];

/// One build, for phases written in assembly or made of system calls, and
/// for the debugger's own bookkeeping.
const FIXTURE: &str = MATRIX[0];

fn expression(text: &str) -> microscope::Expression {
    microscope::Expression::parse(text).expect("valid test expression")
}

async fn watch(scenario: &Scenario, text: &str, access: WatchAccess) -> Watchpoint {
    scenario
        .operation(
            &format!("watch {text}"),
            scenario.handle().watch(&expression(text), access),
        )
        .await
}

async fn symbol(scenario: &Scenario, name: &str) -> VirtualAddress {
    scenario
        .operation(
            &format!("resolve {name}"),
            scenario.handle().runtime_address(name),
        )
        .await
}

/// Runs to a function breakpoint through the signals earlier fixture phases
/// raise, failing on any other stop.
async fn run_to(scenario: &mut Scenario, function: &str) {
    scenario.add_breakpoint(function).await;
    let mut reason = scenario.run_to_stop().await;

    for _ in 0..4 {
        match reason {
            StopReason::Breakpoint { .. } => return,
            StopReason::Exception(ref exception) if matches!(exception.code, 10 | 17) => {
                reason = scenario.resume_to_stop().await;
            }
            other => panic!("expected to reach {function}, got {other:?}"),
        }
    }

    panic!("{function} was not reached");
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
    assert!(bytes.len() <= 8, "test values fit in a word");
    let mut word = [0_u8; 8];
    word[..bytes.len()].copy_from_slice(bytes);
    u64::from_le_bytes(word)
}

/// Asserts that the stop is exactly one hit of `watchpoint` changing the
/// watched bytes from `previous` to `current`.
fn assert_single_hit(reason: &StopReason, watchpoint: WatchpointId, previous: u64, current: u64) {
    let [hit] = hits(reason) else {
        panic!("expected exactly one hit, got {reason:?}");
    };

    assert_eq!(hit.watchpoint, watchpoint, "{reason:?}");
    assert_eq!(value(hit.previous.as_ref()), previous, "{reason:?}");
    assert_eq!(value(hit.current.as_ref()), current, "{reason:?}");
    assert_eq!(hit.changed(), previous != current, "{reason:?}");
}

async fn program_counter(scenario: &Scenario) -> u64 {
    let registers = scenario
        .operation("read registers", scenario.handle().registers())
        .await;
    support::register_u64(&registers, RegisterRole::ProgramCounter)
}

async fn stopped_function(scenario: &Scenario) -> Option<String> {
    scenario
        .operation("locate stop", scenario.handle().current_location())
        .await
        .image
        .function
        .map(|function| function.name.to_string())
}

async fn selected_thread(scenario: &mut Scenario) -> ThreadId {
    scenario
        .snapshot()
        .await
        .selected
        .and_then(microscope::ExecutionContext::as_thread)
        .expect("a stopped inferior has a selected thread")
}

/// Resumes to a clean exit through the signals the fixture's later phases
/// raise: SIGUSR1 from its handler phase and SIGCHLD from its fork phase.
async fn resume_to_exit(scenario: &mut Scenario) {
    for _ in 0..4 {
        match scenario.resume_to_stop().await {
            StopReason::Exited(ExitStatus::Code(0)) => return,
            StopReason::Exception(exception) if matches!(exception.code, 10 | 17) => {}
            other => panic!("expected a clean exit, got {other:?}"),
        }
    }

    panic!("the inferior did not exit");
}

/// Collects every watchpoint event currently buffered on a subscription.
fn watch_events(
    events: &mut tokio::sync::broadcast::Receiver<DebuggerEvent>,
) -> Vec<DebuggerEvent> {
    std::iter::from_fn(|| events.try_recv().ok())
        .filter(|event| {
            matches!(
                event,
                DebuggerEvent::WatchpointsChanged { .. }
                    | DebuggerEvent::WatchpointsInvalidated { .. }
            )
        })
        .collect()
}

#[tokio::test]
async fn write_watchpoints_report_every_store_after_its_instruction() {
    for fixture in MATRIX {
        let mut scenario = Scenario::launch(fixture);
        run_to(&mut scenario, "scalar_stores").await;
        let mut events = scenario.handle().subscribe();

        let watchpoint = watch(&scenario, "watch_i32", WatchAccess::Write).await;
        assert_eq!(watchpoint.address, symbol(&scenario, "watch_i32").await);
        assert_eq!(watchpoint.byte_size, 4, "{fixture}");
        assert_eq!(watchpoint.coverage.len(), 1, "{fixture}");
        assert_eq!(watchpoint.access, WatchAccess::Write);
        let main_module = scenario
            .operation("list modules", scenario.handle().loaded_modules())
            .await
            .modules
            .iter()
            .find(|module| module.path.as_path() == scenario.handle().executable())
            .expect("main module is loaded")
            .module
            .id;
        assert_eq!(
            watchpoint.scope,
            WatchScope::Static {
                module: main_module
            },
            "{fixture}"
        );
        assert!(matches!(
            watch_events(&mut events).as_slice(),
            [DebuggerEvent::WatchpointsChanged { .. }]
        ));
        assert_eq!(
            scenario.snapshot().await.watchpoints.as_ref(),
            std::slice::from_ref(&watchpoint)
        );

        for (hit_count, (previous, current)) in (1..).zip([(0, 1), (1, 2), (2, 2), (2, 42)]) {
            let reason = scenario.resume_to_stop().await;
            assert_single_hit(&reason, watchpoint.id, previous, current);
            assert_eq!(hits(&reason)[0].hit_count, hit_count);
            assert_eq!(
                hits(&reason)[0].thread,
                selected_thread(&mut scenario).await
            );
            assert_eq!(
                stopped_function(&scenario).await.as_deref(),
                Some("scalar_stores"),
                "{fixture}: the stop follows the store inside its function"
            );

            let snapshot = scenario.snapshot().await;
            assert!(matches!(
                snapshot.inferior,
                InferiorState::Stopped {
                    reason: StopReason::Watchpoint { .. },
                    ..
                }
            ));
            assert_eq!(
                scenario
                    .operation(
                        "read watched word",
                        scenario.handle().read_memory(watchpoint.address, 4)
                    )
                    .await
                    .bytes
                    .as_ref(),
                u32::try_from(current)
                    .expect("test value fits")
                    .to_le_bytes()
            );
        }

        let removed = scenario
            .operation(
                "remove watchpoint",
                scenario.handle().remove_watchpoint(watchpoint.id),
            )
            .await;

        let counted = Watchpoint {
            hit_count: 4,
            ..watchpoint
        };

        assert_eq!(removed, counted);
        assert!(scenario.snapshot().await.watchpoints.is_empty());

        // One watchpoint stays armed through the process's exit.
        let armed = watch(&scenario, "watch_wide.words[3]", WatchAccess::Write).await;
        let mut events = scenario.handle().subscribe();
        resume_to_exit(&mut scenario).await;
        assert!(scenario.snapshot().await.watchpoints.is_empty());
        assert!(
            matches!(
                watch_events(&mut events).as_slice(),
                [DebuggerEvent::WatchpointsChanged { .. }]
            ),
            "{fixture}: exiting discards {armed:?}"
        );

        // Watchpoints never carry into the next run; it reaches the
        // breakpoint and exits without a watch stop.
        assert!(matches!(
            scenario.run_to_stop().await,
            StopReason::Breakpoint { .. }
        ));
        assert!(scenario.snapshot().await.watchpoints.is_empty());
        resume_to_exit(&mut scenario).await;
        scenario.shutdown().await;
    }
}

#[tokio::test]
async fn watch_ranges_split_into_aligned_slots_until_capacity_runs_out() {
    let mut scenario = Scenario::launch(FIXTURE);
    run_to(&mut scenario, "size_stores").await;
    let capabilities = scenario.handle().watchpoint_capabilities();
    assert_eq!(capabilities.slots, 4);
    assert_eq!(capabilities.max_slot_bytes, 8);
    let kinds = [
        WatchAccess::Change,
        WatchAccess::Write,
        WatchAccess::ReadWrite,
    ];
    assert_eq!(capabilities.access.as_ref(), kinds);

    // A C array carries no size of its own: its size is its elements'.
    for text in ["watch_oversized", "watch_oversized.bytes"] {
        let oversized = scenario
            .handle()
            .watch(&expression(text), WatchAccess::Write)
            .await;
        assert!(
            matches!(oversized, Err(Error::WatchpointCapacity { required, available: 4 }) if required > 4),
            "{text}: {oversized:?}"
        );
    }

    let words = watch(&scenario, "watch_wide.words", WatchAccess::Write).await;
    assert_eq!((words.byte_size, words.coverage.len()), (32, 4));
    scenario
        .operation("free slots", scenario.handle().remove_watchpoint(words.id))
        .await;
    let byte = watch(&scenario, "watch_u8", WatchAccess::Write).await;
    let half = watch(&scenario, "watch_u16", WatchAccess::Write).await;
    let word = watch(&scenario, "watch_u64", WatchAccess::Write).await;
    assert_eq!([byte.byte_size, half.byte_size, word.byte_size], [1, 2, 8]);
    let packed = scenario
        .handle()
        .watch(&expression("watch_packed.field"), WatchAccess::Write)
        .await;
    assert!(
        matches!(
            packed,
            Err(Error::WatchpointCapacity {
                required: 3,
                available: 1
            })
        ),
        "{packed:?}"
    );
    assert_eq!(scenario.snapshot().await.watchpoints.len(), 3);

    for id in [byte.id, half.id] {
        scenario
            .operation("free slots", scenario.handle().remove_watchpoint(id))
            .await;
    }

    let packed = watch(&scenario, "watch_packed.field", WatchAccess::Write).await;
    assert_eq!(packed.byte_size, 4);
    assert_eq!(packed.address.get() % 8, 1, "the field is misaligned");
    assert_eq!(
        packed
            .coverage
            .iter()
            .map(|range| range.end.get() - range.start.get())
            .collect::<Vec<_>>(),
        [1, 2, 1],
        "an exact cover of naturally aligned spans"
    );
    assert!(matches!(
        scenario
            .handle()
            .watch(&expression("watch_array[3]"), WatchAccess::Write)
            .await,
        Err(Error::WatchpointCapacity {
            required: 1,
            available: 0
        })
    ));

    assert_single_hit(
        &scenario.resume_to_stop().await,
        word.id,
        0x1111_1111_1111_1111,
        0x4444_4444_4444_4444,
    );
    scenario
        .operation("free a slot", scenario.handle().remove_watchpoint(word.id))
        .await;

    let element = watch(&scenario, "watch_array[3]", WatchAccess::Write).await;
    assert_eq!(
        element.address.get(),
        symbol(&scenario, "watch_array").await.get() + 12
    );
    assert_single_hit(&scenario.resume_to_stop().await, packed.id, 0, 0x5555_5555);
    assert_single_hit(&scenario.resume_to_stop().await, element.id, 0, 33);

    let removed = scenario
        .operation(
            "remove all watchpoints",
            scenario.handle().remove_all_watchpoints(),
        )
        .await;
    assert_eq!(removed.len(), 2);
    resume_to_exit(&mut scenario).await;
    scenario.shutdown().await;
}

#[tokio::test]
async fn one_store_reports_every_watchpoint_it_touches() {
    let mut scenario = Scenario::launch(FIXTURE);
    run_to(&mut scenario, "paired_store").await;

    let first = watch(&scenario, "watch_pair.first", WatchAccess::Write).await;
    let second = watch(&scenario, "watch_pair.second", WatchAccess::Write).await;

    // The whole record shares both members' slots.
    let pair = watch(&scenario, "watch_pair", WatchAccess::Write).await;
    assert_eq!(pair.byte_size, 16);

    // Two slots remain, so a further 16 bytes fit and then nothing does.
    let wide = watch(&scenario, "watch_wide.words[2]", WatchAccess::Write).await;
    let spare = watch(&scenario, "watch_wide.words[3]", WatchAccess::Write).await;
    assert!(matches!(
        scenario
            .handle()
            .watch(&expression("watch_u8"), WatchAccess::Write)
            .await,
        Err(Error::WatchpointCapacity {
            required: 1,
            available: 0
        })
    ));

    for id in [wide.id, spare.id] {
        scenario
            .operation("remove spare", scenario.handle().remove_watchpoint(id))
            .await;
    }

    let reason = scenario.resume_to_stop().await;
    let reported = hits(&reason)
        .iter()
        .map(|hit| (hit.watchpoint, hit.changed()))
        .collect::<BTreeMap<_, _>>();
    assert_eq!(
        reported,
        BTreeMap::from([(first.id, true), (second.id, true), (pair.id, true)]),
        "one 16-byte store must report all three watchpoints"
    );
    let pair_hit = hits(&reason)
        .iter()
        .find(|hit| hit.watchpoint == pair.id)
        .expect("pair hit");
    assert_eq!(pair_hit.current.as_deref(), Some(&[0xff_u8; 16][..]));
    assert_eq!(pair_hit.previous.as_deref(), Some(&[0_u8; 16][..]));

    scenario
        .operation("remove all", scenario.handle().remove_all_watchpoints())
        .await;
    resume_to_exit(&mut scenario).await;
    scenario.shutdown().await;
}

#[tokio::test]
async fn accesses_that_do_not_change_the_value_are_still_reported() {
    let mut scenario = Scenario::launch(FIXTURE);
    run_to(&mut scenario, "failed_exchange").await;
    let watchpoint = watch(&scenario, "watch_u64", WatchAccess::Write).await;
    let reason = scenario.resume_to_stop().await;

    // A failing locked compare-exchange still writes its destination.
    assert_single_hit(
        &reason,
        watchpoint.id,
        0x4444_4444_4444_4444,
        0x4444_4444_4444_4444,
    );
    assert_eq!(
        stopped_function(&scenario).await.as_deref(),
        Some("failed_exchange")
    );
    scenario
        .operation(
            "remove watchpoint",
            scenario.handle().remove_watchpoint(watchpoint.id),
        )
        .await;
    resume_to_exit(&mut scenario).await;
    scenario.shutdown().await;
}

#[tokio::test]
async fn repeated_string_stores_report_each_iteration_at_the_string_instruction() {
    let mut scenario = Scenario::launch(FIXTURE);
    run_to(&mut scenario, "repeated_store").await;
    let watchpoint = watch(&scenario, "watch_wide.words[1]", WatchAccess::Write).await;

    let mut previous = 0_u64;
    let mut program_counters = BTreeSet::new();

    for written in 1..=8_u32 {
        let reason = scenario.resume_to_stop().await;
        let current = u64::from_le_bytes(std::array::from_fn(|index| {
            if index < written as usize { 0x41 } else { 0 }
        }));
        assert_single_hit(&reason, watchpoint.id, previous, current);
        previous = current;
        program_counters.insert(program_counter(&scenario).await);
    }

    assert_eq!(
        program_counters.len(),
        1,
        "every iteration stops at the unfinished string instruction"
    );

    scenario
        .operation(
            "remove watchpoint",
            scenario.handle().remove_watchpoint(watchpoint.id),
        )
        .await;
    resume_to_exit(&mut scenario).await;
    scenario.shutdown().await;
}

#[tokio::test]
async fn access_watchpoints_report_loads_and_read_only_watches_are_refused() {
    let mut scenario = Scenario::launch(FIXTURE);
    run_to(&mut scenario, "read_access").await;

    let address = symbol(&scenario, "watch_i32").await;

    for result in [
        scenario
            .handle()
            .watch(&expression("watch_i32"), WatchAccess::Read)
            .await,
        scenario
            .handle()
            .add_watchpoint(
                WatchpointSpec::Location {
                    address,
                    byte_size: 4,
                },
                WatchAccess::Read,
            )
            .await,
    ] {
        assert!(
            matches!(
                result,
                Err(Error::UnsupportedWatchAccess(WatchAccess::Read))
            ),
            "{result:?}"
        );
    }

    assert!(scenario.snapshot().await.watchpoints.is_empty());

    let watchpoint = watch(&scenario, "watch_i32", WatchAccess::ReadWrite).await;
    assert_eq!(watchpoint.access, WatchAccess::ReadWrite);
    let reason = scenario.resume_to_stop().await;
    assert_single_hit(&reason, watchpoint.id, 42, 42);

    assert_eq!(
        stopped_function(&scenario).await.as_deref(),
        Some("read_access")
    );
    scenario
        .operation(
            "remove watchpoint",
            scenario.handle().remove_watchpoint(watchpoint.id),
        )
        .await;
    resume_to_exit(&mut scenario).await;
    scenario.shutdown().await;
}

#[tokio::test]
async fn expression_watchpoints_keep_watching_the_location_they_resolved() {
    let mut scenario = Scenario::launch(FIXTURE);
    run_to(&mut scenario, "retarget_pointer").await;

    let target = scenario
        .operation(
            "resolve pointee",
            scenario
                .handle()
                .resolve_watch_target(&expression("*watch_pointer")),
        )
        .await;
    assert_eq!(target.address(), symbol(&scenario, "pointee_first").await);
    assert_eq!(target.byte_size(), 4);
    assert_eq!(
        target.scope(),
        &WatchScope::Location,
        "storage reached through a pointer has no known lifetime"
    );
    let watchpoint = scenario
        .operation(
            "watch pointee",
            scenario
                .handle()
                .add_watchpoint(WatchpointSpec::Target(Box::new(target)), WatchAccess::Write),
        )
        .await;

    // `*watch_pointer = 2` writes the second pointee and is not reported.
    assert_single_hit(&scenario.resume_to_stop().await, watchpoint.id, 0, 1);
    assert_single_hit(&scenario.resume_to_stop().await, watchpoint.id, 1, 3);

    scenario
        .operation(
            "remove watchpoint",
            scenario.handle().remove_watchpoint(watchpoint.id),
        )
        .await;
    resume_to_exit(&mut scenario).await;
    scenario.shutdown().await;
}

#[tokio::test]
async fn kernel_writes_are_invisible_so_previous_is_the_last_observed_value() {
    let mut scenario = Scenario::launch(FIXTURE);
    run_to(&mut scenario, "kernel_write").await;
    let watchpoint = watch(&scenario, "watch_u64", WatchAccess::Write).await;

    // read(2) stores 0x1234 inside the kernel without a hit; the next
    // report is the user-mode increment, measured against the value the
    // debugger last observed before resuming.
    let reason = scenario.resume_to_stop().await;
    assert_single_hit(&reason, watchpoint.id, 0x4444_4444_4444_4444, 0x1235);
    assert_eq!(
        stopped_function(&scenario).await.as_deref(),
        Some("kernel_write")
    );
    scenario
        .operation(
            "remove watchpoint",
            scenario.handle().remove_watchpoint(watchpoint.id),
        )
        .await;
    resume_to_exit(&mut scenario).await;
    scenario.shutdown().await;
}

#[tokio::test]
async fn debugger_writes_are_not_reported_but_refresh_the_previous_value() {
    let mut scenario = Scenario::launch(FIXTURE);
    run_to(&mut scenario, "scalar_stores").await;
    let watchpoint = watch(&scenario, "watch_i32", WatchAccess::Write).await;

    let word = scenario
        .operation("read word", scenario.handle().read_word(watchpoint.address))
        .await;
    scenario
        .operation(
            "write watched word",
            scenario
                .handle()
                .write_word(watchpoint.address, (word & !0xffff_ffff) | 0x1f4),
        )
        .await;
    assert_single_hit(&scenario.resume_to_stop().await, watchpoint.id, 0x1f4, 1);

    scenario
        .operation(
            "remove watchpoint",
            scenario.handle().remove_watchpoint(watchpoint.id),
        )
        .await;
    resume_to_exit(&mut scenario).await;
    scenario.shutdown().await;
}

#[tokio::test]
async fn signal_handler_writes_are_reported_inside_the_handler() {
    let mut scenario = Scenario::launch(FIXTURE);
    run_to(&mut scenario, "handler_write").await;
    let watchpoint = watch(&scenario, "watch_i32", WatchAccess::Write).await;

    assert!(matches!(
        scenario.resume_to_stop().await,
        StopReason::Exception(exception) if exception.code == 10
    ));
    let reason = scenario.resume_to_stop().await;
    assert_single_hit(&reason, watchpoint.id, 42, 99);
    assert_eq!(
        stopped_function(&scenario).await.as_deref(),
        Some("write_in_handler")
    );

    scenario
        .operation(
            "remove watchpoint",
            scenario.handle().remove_watchpoint(watchpoint.id),
        )
        .await;
    resume_to_exit(&mut scenario).await;
    scenario.shutdown().await;
}

#[tokio::test]
async fn a_handler_run_before_a_breakpoint_repair_reports_its_write() {
    let mut scenario = Scenario::launch(FIXTURE);
    run_to(&mut scenario, "await_signal").await;
    let watchpoint = watch(&scenario, "watch_i32", WatchAccess::Write).await;
    let InferiorState::Stopped { process_id, .. } = scenario.snapshot().await.inferior else {
        panic!("inferior is stopped");
    };

    // The signal arrives while the thread must still step over its
    // breakpoint, so the handler runs before the breakpoint is repaired.
    nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(i32::try_from(process_id.get()).expect("pid fits i32")),
        nix::sys::signal::Signal::SIGUSR1,
    )
    .expect("signal the inferior");
    assert!(matches!(
        scenario.resume_to_stop().await,
        StopReason::Exception(exception) if exception.code == 10
    ));
    assert_single_hit(&scenario.resume_to_stop().await, watchpoint.id, 99, 99);
    assert_eq!(
        stopped_function(&scenario).await.as_deref(),
        Some("write_in_handler")
    );

    scenario
        .operation(
            "remove watchpoint",
            scenario.handle().remove_watchpoint(watchpoint.id),
        )
        .await;

    // The breakpoint the handler interrupted is repaired without being
    // reported again.
    resume_to_exit(&mut scenario).await;
    scenario.shutdown().await;
}

#[tokio::test]
async fn a_handler_storing_the_same_value_before_a_breakpoint_repair_is_transparent() {
    let mut scenario = Scenario::launch(FIXTURE);
    run_to(&mut scenario, "await_signal").await;
    let watchpoint = watch(&scenario, "watch_i32", WatchAccess::Change).await;
    let InferiorState::Stopped { process_id, .. } = scenario.snapshot().await.inferior else {
        panic!("inferior is stopped");
    };

    nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(i32::try_from(process_id.get()).expect("pid fits i32")),
        nix::sys::signal::Signal::SIGUSR1,
    )
    .expect("signal the inferior");
    assert!(matches!(
        scenario.resume_to_stop().await,
        StopReason::Exception(exception) if exception.code == 10
    ));

    // The handler stores the 99 already there. The thread then returns
    // to its breakpoint and steps over it, and neither is reported: the
    // next stop is the fork phase's change.
    let mut reason = scenario.resume_to_stop().await;
    while matches!(&reason, StopReason::Exception(exception) if exception.code == 17) {
        reason = scenario.resume_to_stop().await;
    }

    assert_single_hit(&reason, watchpoint.id, 99, 4321);
    assert_eq!(
        stopped_function(&scenario).await.as_deref(),
        Some("fork_write")
    );

    scenario
        .operation(
            "remove watchpoint",
            scenario.handle().remove_watchpoint(watchpoint.id),
        )
        .await;
    resume_to_exit(&mut scenario).await;
    scenario.shutdown().await;
}

#[tokio::test]
async fn forked_children_do_not_inherit_watchpoints() {
    let mut scenario = Scenario::launch(FIXTURE);
    run_to(&mut scenario, "fork_write").await;
    let watchpoint = watch(&scenario, "watch_i32", WatchAccess::Write).await;

    // A child that inherited the watchpoint would die by SIGTRAP and the
    // parent would exit 73 instead of storing 4321. Its SIGCHLD does not
    // stop the parent.
    assert_single_hit(&scenario.resume_to_stop().await, watchpoint.id, 99, 4321);

    scenario
        .operation(
            "remove watchpoint",
            scenario.handle().remove_watchpoint(watchpoint.id),
        )
        .await;
    resume_to_exit(&mut scenario).await;
    scenario.shutdown().await;
}

#[tokio::test]
async fn vdso_writes_are_reported_in_user_mode() {
    let mut scenario = Scenario::launch(FIXTURE);
    run_to(&mut scenario, "vdso_write").await;
    let watchpoint = watch(&scenario, "watch_time", WatchAccess::Write).await;
    assert_eq!(watchpoint.byte_size, 16);

    let reason = scenario.resume_to_stop().await;
    assert!(
        hits(&reason)
            .iter()
            .all(|hit| hit.watchpoint == watchpoint.id)
    );
    let InferiorState::Stopped { process_id, .. } = scenario.snapshot().await.inferior else {
        panic!("inferior is stopped");
    };

    let pc = program_counter(&scenario).await;
    let vdso = support::vdso_mapping(process_id);
    assert!(
        vdso.contains(&pc),
        "the store happened in the vDSO, pc {pc:#x} outside {vdso:x?}"
    );

    // The vDSO's own call-frame information leads back to the watching code.
    let modules = scenario
        .operation("modules", scenario.handle().loaded_modules())
        .await;
    let trace = scenario
        .operation("backtrace", scenario.handle().backtrace())
        .await;
    let frames = support::frame_modules(&trace, &modules);
    assert_eq!(frames[0].0, support::VDSO, "{frames:#?}");
    let writer = support::position_of(&frames, FIXTURE, "vdso_write");
    assert_eq!(
        frames[writer + 1],
        (FIXTURE.to_owned(), Some("main".to_owned())),
        "{frames:#?}"
    );

    scenario
        .operation(
            "remove watchpoint",
            scenario.handle().remove_watchpoint(watchpoint.id),
        )
        .await;
    resume_to_exit(&mut scenario).await;
    scenario.shutdown().await;
}

#[tokio::test]
async fn a_breakpoint_right_after_a_watched_store_is_still_reported() {
    for step_instead in [false, true] {
        let mut scenario = Scenario::launch(FIXTURE);
        run_to(&mut scenario, "store_then_breakpoint").await;
        let after = symbol(&scenario, "after_watched_store").await;
        scenario
            .add_breakpoint_spec(BreakpointSpec::Address(after))
            .await;
        let watchpoint = watch(&scenario, "watch_u16", WatchAccess::Write).await;

        assert_single_hit(&scenario.resume_to_stop().await, watchpoint.id, 0x2222, 7);
        assert_eq!(program_counter(&scenario).await, after.get());
        let next = if step_instead {
            scenario.step_to_stop(StepKind::Instruction).await
        } else {
            scenario.resume_to_stop().await
        };

        assert_eq!(
            support::breakpoint_address(&next),
            after,
            "the breakpoint at the next instruction is not skipped"
        );
        scenario.shutdown().await;
    }
}

#[tokio::test]
async fn stepping_over_a_breakpoint_on_a_watched_store_reports_the_store() {
    for kind in [None, Some(StepKind::Instruction)] {
        let mut scenario = Scenario::launch(FIXTURE);

        // The function entry is the store itself, so the address
        // breakpoint is installed from the preceding phase.
        run_to(&mut scenario, "store_then_breakpoint").await;
        let site = symbol(&scenario, "watched_store_site").await;
        scenario
            .add_breakpoint_spec(BreakpointSpec::Address(site))
            .await;
        assert_eq!(
            support::breakpoint_address(&scenario.resume_to_stop().await),
            site
        );
        let watchpoint = watch(&scenario, "watch_i32", WatchAccess::Write).await;

        // The store executes during the step over the breakpoint, which
        // must surface the hit instead of silently continuing.
        let reason = match kind {
            Some(kind) => scenario.step_to_stop(kind).await,
            None => scenario.resume_to_stop().await,
        };

        assert_single_hit(&reason, watchpoint.id, 4321, 5);
        assert!(program_counter(&scenario).await > site.get());
        let site_bytes = scenario
            .operation("read store site", scenario.handle().read_memory(site, 1))
            .await;
        assert_ne!(site_bytes.bytes.as_ref(), [0xcc], "breakpoints stay hidden");

        scenario
            .operation(
                "remove watchpoint",
                scenario.handle().remove_watchpoint(watchpoint.id),
            )
            .await;
        resume_to_exit(&mut scenario).await;
        scenario.shutdown().await;
    }
}

#[tokio::test]
async fn source_steps_end_at_a_watched_store_and_leave_no_plan_behind() {
    for fixture in MATRIX {
        let mut scenario = Scenario::launch(fixture);
        run_to(&mut scenario, "step_over_writer").await;
        let watchpoint = watch(&scenario, "watch_i32", WatchAccess::Write).await;

        let reason = scenario.step_to_stop(StepKind::OverSource).await;
        assert_single_hit(&reason, watchpoint.id, 5, 77);
        assert_eq!(
            stopped_function(&scenario).await.as_deref(),
            Some("nested_writer"),
            "{fixture}: next stops inside the callee that wrote"
        );
        scenario
            .operation(
                "remove watchpoint",
                scenario.handle().remove_watchpoint(watchpoint.id),
            )
            .await;

        // A leftover step-plan breakpoint would stop here.
        resume_to_exit(&mut scenario).await;
        scenario.shutdown().await;
    }
}

#[tokio::test]
async fn finish_ends_at_a_watched_store_before_the_frame_returns() {
    for fixture in MATRIX {
        let mut scenario = Scenario::launch(fixture);
        run_to(&mut scenario, "scalar_stores").await;
        let watchpoint = watch(&scenario, "watch_i32", WatchAccess::Write).await;
        assert_single_hit(
            &scenario.step_to_stop(StepKind::Out).await,
            watchpoint.id,
            0,
            1,
        );
        assert_eq!(
            stopped_function(&scenario).await.as_deref(),
            Some("scalar_stores")
        );
        scenario
            .operation(
                "remove watchpoint",
                scenario.handle().remove_watchpoint(watchpoint.id),
            )
            .await;
        resume_to_exit(&mut scenario).await;
        scenario.shutdown().await;
    }
}

#[tokio::test]
async fn static_locals_are_watched_as_static_storage() {
    for fixture in MATRIX {
        let mut scenario = Scenario::launch(fixture);
        run_to(&mut scenario, "static_local_counter").await;
        let watchpoint = watch(&scenario, "calls", WatchAccess::Write).await;
        assert!(
            matches!(watchpoint.scope, WatchScope::Static { .. }),
            "{fixture}: {:?}",
            watchpoint.scope
        );
        assert_single_hit(&scenario.resume_to_stop().await, watchpoint.id, 0, 1);
        assert!(matches!(
            scenario.resume_to_stop().await,
            StopReason::Breakpoint { .. }
        ));
        assert_single_hit(&scenario.resume_to_stop().await, watchpoint.id, 1, 2);
        resume_to_exit(&mut scenario).await;
        scenario.shutdown().await;
    }
}

#[tokio::test]
async fn exec_discards_watchpoints_because_the_kernel_cleared_them() {
    let mut scenario = Scenario::new("watch exec", Scenario::fixture("thread-exec"));
    run_to(&mut scenario, "replace_image").await;
    let main = symbol(&scenario, "main").await;
    let watchpoint = scenario
        .operation(
            "watch location",
            scenario.handle().add_watchpoint(
                WatchpointSpec::Location {
                    address: main,
                    byte_size: 1,
                },
                WatchAccess::Write,
            ),
        )
        .await;
    assert_eq!(watchpoint.scope, WatchScope::Location);
    let mut events = scenario.handle().subscribe();

    assert_eq!(
        scenario.resume_to_stop().await,
        StopReason::Exec { followed: false }
    );
    assert!(scenario.snapshot().await.watchpoints.is_empty());
    assert!(matches!(
        watch_events(&mut events).as_slice(),
        [DebuggerEvent::WatchpointsChanged { .. }]
    ));
    assert!(matches!(
        scenario
            .handle()
            .add_watchpoint(
                WatchpointSpec::Location {
                    address: main,
                    byte_size: 1
                },
                WatchAccess::Write
            )
            .await,
        Err(Error::Backend(_))
    ));
    scenario.shutdown().await;
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

#[tokio::test]
async fn every_write_from_every_thread_is_reported_exactly_once() {
    const WORKERS: usize = 2;
    const ITERATIONS: usize = 40;
    const CHURN: usize = 16;

    let mut scenario = Scenario::new("watch threads", Scenario::fixture("watch-threads"));
    run_to(&mut scenario, "before_threads").await;
    let main = selected_thread(&mut scenario).await;

    // Every thread that writes these is created after they were armed.
    let locked = watch(&scenario, "locked_counter", WatchAccess::Write).await;
    let racing = watch(&scenario, "racing_counter", WatchAccess::Write).await;
    scenario.add_breakpoint("tls_ready").await;

    let mut locked_values = Vec::new();
    let mut locked_writers = BTreeSet::new();
    let mut racing_hits = 0;
    let mut racing_writers = BTreeSet::new();
    let mut started = BTreeSet::new();
    let mut events = scenario.handle().subscribe();

    loop {
        let reason = scenario.resume_to_stop().await;
        while let Ok(event) = events.try_recv() {
            if let DebuggerEvent::ThreadStarted { thread_id, .. } = event {
                started.insert(thread_id);
            }
        }

        match reason {
            StopReason::Watchpoint { .. } => {}
            StopReason::Breakpoint { .. } => break,
            other => panic!("unexpected stop while counting: {other:?}"),
        }

        let snapshot = scenario.snapshot().await;
        assert!(matches!(snapshot.inferior, InferiorState::Stopped { .. }));
        for hit in thread_hits(&mut scenario).await {
            assert!(hit.changed() || hit.watchpoint == racing.id);
            if hit.watchpoint == locked.id {
                // Lock-serialized stores are observed one at a time.
                assert_eq!(
                    value(hit.current.as_ref()),
                    value(hit.previous.as_ref()) + 1,
                    "{hit:?}"
                );
                locked_values.push(value(hit.current.as_ref()));
                locked_writers.insert(hit.thread);
            } else {
                assert_eq!(hit.watchpoint, racing.id);
                racing_hits += 1;
                racing_writers.insert(hit.thread);
            }
        }
    }

    let expected = (1..=(WORKERS * ITERATIONS + CHURN) as u64).collect::<Vec<_>>();
    assert_eq!(
        locked_values, expected,
        "every locked store is reported in order"
    );

    // Main's own reset in before_threads is the first racing store.
    assert_eq!(
        racing_hits,
        WORKERS * ITERATIONS + 1,
        "racing stores are each reported once, including coincident hits"
    );
    assert_eq!(locked_writers.len(), WORKERS + CHURN);
    assert!(!locked_writers.contains(&main));
    assert_eq!(racing_writers.len(), WORKERS + 1);
    assert!(racing_writers.contains(&main));
    assert!(
        locked_writers.is_subset(&started),
        "every writer was armed when it started"
    );

    scenario
        .operation("remove all", scenario.handle().remove_all_watchpoints())
        .await;
    scenario
        .operation(
            "remove breakpoints",
            scenario.handle().remove_all_breakpoints(),
        )
        .await;
    resume_to_exit(&mut scenario).await;
    scenario.shutdown().await;
}

#[tokio::test]
async fn thread_local_watchpoints_watch_one_threads_instance_until_it_exits() {
    let mut scenario = Scenario::new("watch tls", Scenario::fixture("watch-threads"));
    run_to(&mut scenario, "tls_ready").await;
    let owner = selected_thread(&mut scenario).await;
    let target = scenario
        .operation(
            "resolve tls",
            scenario
                .handle()
                .resolve_watch_target(&expression("tls_value")),
        )
        .await;
    assert_eq!(target.scope(), &WatchScope::ThreadLocal { thread: owner });
    let watchpoint = scenario
        .operation(
            "watch tls",
            scenario
                .handle()
                .add_watchpoint(WatchpointSpec::Target(Box::new(target)), WatchAccess::Write),
        )
        .await;
    scenario.add_breakpoint("after_join").await;
    let mut events = scenario.handle().subscribe();

    // The owner stores 2 and 3; main stores 5 through a pointer. The other
    // worker's stores to its own instance are never reported.
    let mut writes = BTreeMap::new();

    loop {
        match scenario.resume_to_stop().await {
            StopReason::Watchpoint { .. } => {}
            StopReason::Breakpoint { .. } => break,
            other => panic!("unexpected stop: {other:?}"),
        }

        for hit in thread_hits(&mut scenario).await {
            assert_eq!(hit.watchpoint, watchpoint.id);
            writes
                .entry(hit.thread)
                .or_insert_with(Vec::new)
                .push(value(hit.current.as_ref()));
        }
    }

    // Main's store and the owner's second store may coincide; both are
    // reported, each with the value once every thread stopped.
    assert_eq!(writes.len(), 2, "{writes:?}");
    assert_eq!(writes.get(&owner).map(|values| values[0]), Some(2));
    assert!(writes.get(&owner).is_some_and(|values| values.len() == 2));
    assert!(
        writes
            .iter()
            .any(|(thread, values)| *thread != owner && values.len() == 1)
    );
    assert!(
        writes
            .values()
            .flatten()
            .all(|value| [2, 3, 5].contains(value))
    );

    // The first stop after the owner exited removes the watchpoint.
    let invalidated = std::iter::from_fn(|| events.try_recv().ok())
        .find_map(|event| match event {
            DebuggerEvent::WatchpointsInvalidated { invalidated, .. } => Some(invalidated),
            _ => None,
        })
        .expect("the watchpoint was invalidated");
    assert!(matches!(
        invalidated.as_ref(),
        [entry] if entry.watchpoint.id == watchpoint.id
            && entry.reason == WatchpointInvalidation::OwnerThreadExited
    ));
    assert!(scenario.snapshot().await.watchpoints.is_empty());
    resume_to_exit(&mut scenario).await;
    scenario.shutdown().await;
}

/// The current values of one watchpoint's hits in a watchpoint stop.
fn values_of(reason: &StopReason, watchpoint: WatchpointId) -> Vec<u64> {
    hits(reason)
        .iter()
        .map(|hit| {
            assert_eq!(hit.watchpoint, watchpoint, "{reason:?}");
            value(hit.current.as_ref())
        })
        .collect()
}

const LOCALS: [&str; 2] = ["watch-locals-gcc-o0", "watch-locals-clang-o0"];

fn locals_line(needle: &str) -> u64 {
    let source = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/c/watch-locals.c"),
    )
    .expect("read locals fixture");
    let index = source
        .lines()
        .position(|line| line.trim() == needle)
        .unwrap_or_else(|| panic!("watch-locals.c has no line {needle:?}"));
    u64::try_from(index + 1).expect("line fits u64")
}

/// Watches a frame-scoped local and checks that its scope names the
/// selected thread's current activation.
async fn watch_local(scenario: &mut Scenario, name: &str, access: WatchAccess) -> Watchpoint {
    let thread = selected_thread(scenario).await;
    let watchpoint = watch(scenario, name, access).await;
    assert!(
        matches!(watchpoint.scope, WatchScope::Frame { thread: owner, .. } if owner == thread),
        "{name}: {:?}",
        watchpoint.scope
    );
    watchpoint
}

/// Resumes until the watchpoint stops existing, returning the values of every
/// hit it reported before that and why it was removed. Removal is detected
/// either at a hit on storage the object no longer owns, which stops with
/// `WatchpointInvalidated`, or at the next public stop.
async fn hits_until_invalidated(
    scenario: &mut Scenario,
    watchpoint: WatchpointId,
) -> (Vec<u64>, WatchpointInvalidation, bool) {
    let mut events = scenario.handle().subscribe();
    let mut values = Vec::new();

    loop {
        match scenario.resume_to_stop().await {
            StopReason::Watchpoint { hits } => {
                let [hit] = hits.as_ref() else {
                    panic!("one hit per stop: {hits:?}");
                };

                assert_eq!(hit.watchpoint, watchpoint);
                values.push(value(hit.current.as_ref()));
            }
            StopReason::WatchpointInvalidated { invalidated } => {
                let [entry] = invalidated.as_ref() else {
                    panic!("one invalidation: {invalidated:?}");
                };

                assert_eq!(entry.watchpoint.id, watchpoint);
                assert!(scenario.snapshot().await.watchpoints.is_empty());
                return (values, entry.reason, true);
            }
            StopReason::Breakpoint { .. } => {
                let reason = std::iter::from_fn(|| events.try_recv().ok())
                    .find_map(|event| match event {
                        DebuggerEvent::WatchpointsInvalidated { invalidated, .. } => {
                            invalidated.iter().find_map(|entry| {
                                (entry.watchpoint.id == watchpoint).then_some(entry.reason)
                            })
                        }
                        _ => None,
                    })
                    .expect("the stop after the scope ended invalidated the watchpoint");
                assert!(scenario.snapshot().await.watchpoints.is_empty());
                return (values, reason, false);
            }
            other => panic!("unexpected stop: {other:?}"),
        }
    }
}

#[tokio::test]
async fn frame_watchpoints_end_when_their_activation_returns() {
    for (fixture, access) in LOCALS.into_iter().flat_map(|fixture| {
        [
            (fixture, WatchAccess::Write),
            (fixture, WatchAccess::Change),
        ]
    }) {
        let mut scenario = Scenario::launch(fixture);
        run_to(&mut scenario, "leaf_local").await;
        let watchpoint = watch_local(&mut scenario, "local", access).await;
        scenario
            .add_source_breakpoint("watch-locals.c", locals_line("local += 2;"))
            .await;
        scenario.add_breakpoint("after_return").await;
        let mut events = scenario.handle().subscribe();

        // The first store initializes uninitialized stack, so only the new
        // values are deterministic.
        assert_eq!(
            values_of(&scenario.resume_to_stop().await, watchpoint.id),
            [10]
        );
        assert_eq!(
            values_of(&scenario.resume_to_stop().await, watchpoint.id),
            [11]
        );

        // A stop while the activation is live and inside the object's scope
        // keeps the watchpoint.
        assert!(matches!(
            scenario.resume_to_stop().await,
            StopReason::Breakpoint { .. }
        ));
        assert_eq!(
            scenario.snapshot().await.watchpoints.len(),
            1,
            "{fixture} {access}"
        );
        assert!(watch_events(&mut events).is_empty(), "{fixture} {access}");

        let (values, reason, _) = hits_until_invalidated(&mut scenario, watchpoint.id).await;
        assert_eq!(values, [13], "{fixture} {access}");
        assert_eq!(
            reason,
            WatchpointInvalidation::ScopeExited,
            "{fixture} {access}"
        );
        scenario.shutdown().await;
    }
}

#[tokio::test]
async fn recursive_activations_keep_their_own_frame_watchpoint() {
    for fixture in LOCALS {
        let mut scenario = Scenario::launch(fixture);
        run_to(&mut scenario, "recurse").await;
        scenario
            .operation(
                "stop breaking on deeper calls",
                scenario.handle().remove_all_breakpoints(),
            )
            .await;

        // The outermost activation's value survives its callees returning.
        let watchpoint = watch_local(&mut scenario, "frame_value", WatchAccess::Write).await;
        scenario.add_breakpoint("after_return").await;

        let (values, reason, _) = hits_until_invalidated(&mut scenario, watchpoint.id).await;
        assert_eq!(values, [2, 203, 303], "{fixture}");
        assert_eq!(reason, WatchpointInvalidation::ScopeExited, "{fixture}");
        scenario.shutdown().await;
    }
}

#[tokio::test]
async fn block_watchpoints_end_when_execution_leaves_the_block() {
    for fixture in LOCALS {
        let mut scenario = Scenario::launch(fixture);
        scenario
            .add_source_breakpoint("watch-locals.c", locals_line("inner += 1;"))
            .await;
        assert!(matches!(
            scenario.run_to_stop().await,
            StopReason::Breakpoint { .. }
        ));
        let watchpoint = watch_local(&mut scenario, "inner", WatchAccess::Write).await;

        // The same activation is still live in the next block.
        scenario
            .add_source_breakpoint("watch-locals.c", locals_line("reused += 1;"))
            .await;

        let (values, reason, _) = hits_until_invalidated(&mut scenario, watchpoint.id).await;
        assert_eq!(values, [6], "{fixture}");
        assert_eq!(reason, WatchpointInvalidation::ScopeExited, "{fixture}");
        scenario.shutdown().await;
    }
}

#[tokio::test]
async fn a_tail_call_replacing_the_activation_ends_its_watchpoints() {
    for fixture in LOCALS {
        let mut scenario = Scenario::launch(fixture);
        run_to(&mut scenario, "tail_caller").await;
        let watchpoint = watch_local(&mut scenario, "mine", WatchAccess::Write).await;

        // The callee runs at the replaced activation's frame address.
        scenario.add_breakpoint("tail_callee").await;

        let (values, reason, _) = hits_until_invalidated(&mut scenario, watchpoint.id).await;
        assert_eq!(values, [1, 2], "{fixture}");
        assert_eq!(reason, WatchpointInvalidation::ScopeExited, "{fixture}");
        scenario.shutdown().await;
    }
}

#[tokio::test]
async fn longjmp_past_an_activation_ends_its_watchpoints() {
    for fixture in LOCALS {
        let mut scenario = Scenario::launch(fixture);
        scenario
            .add_source_breakpoint("watch-locals.c", locals_line("doomed += 1;"))
            .await;
        assert!(matches!(
            scenario.run_to_stop().await,
            StopReason::Breakpoint { .. }
        ));
        let watchpoint = watch_local(&mut scenario, "doomed", WatchAccess::Write).await;
        scenario.add_breakpoint("after_longjmp").await;

        let (values, reason, _) = hits_until_invalidated(&mut scenario, watchpoint.id).await;
        assert_eq!(values, [2], "{fixture}");
        assert_eq!(reason, WatchpointInvalidation::ScopeExited, "{fixture}");
        scenario.shutdown().await;
    }
}

#[tokio::test]
async fn a_frame_watchpoint_ends_with_its_owner_thread() {
    for fixture in LOCALS {
        let mut scenario = Scenario::launch(fixture);
        scenario
            .add_source_breakpoint("watch-locals.c", locals_line("owned += 1;"))
            .await;
        assert!(matches!(
            scenario.run_to_stop().await,
            StopReason::Breakpoint { .. }
        ));
        let watchpoint = watch_local(&mut scenario, "owned", WatchAccess::Write).await;
        scenario.add_breakpoint("after_owner_exit").await;

        let (values, reason, at_hit) = hits_until_invalidated(&mut scenario, watchpoint.id).await;
        assert_eq!(values, [4], "{fixture}");

        // Thread teardown may reuse the dead frame's storage while the thread
        // still exists; otherwise the thread's exit ends the watchpoint.
        assert_eq!(
            reason,
            if at_hit {
                WatchpointInvalidation::ScopeExited
            } else {
                WatchpointInvalidation::OwnerThreadExited
            },
            "{fixture}"
        );
        scenario.shutdown().await;
    }
}

/// Runs a slot-thief fixture to the point where its perf breakpoints hold
/// debug registers. A sandbox that forbids `perf_event_open` cannot run these
/// scenarios, which fails them loudly instead of passing vacuously.
async fn run_to_stolen_slots(scenario: &mut Scenario) {
    scenario.add_breakpoint("slots_taken").await;
    match scenario.run_to_stop().await {
        StopReason::Breakpoint { .. } => {}
        StopReason::Exited(ExitStatus::Code(77)) => panic!(
            "perf_event_open(PERF_TYPE_BREAKPOINT) is unavailable; these scenarios need \
             kernel.perf_event_paranoid <= 2 and no seccomp filter on perf_event_open"
        ),
        other => panic!("unexpected stop before the slots were taken: {other:?}"),
    }
}

#[tokio::test]
async fn kernel_capacity_exhaustion_is_reported_without_arming_anything() {
    let mut scenario = Scenario::new("slot thief", Scenario::fixture("watch-slot-thief-main"));
    run_to_stolen_slots(&mut scenario).await;
    let main = selected_thread(&mut scenario).await;
    let mut events = scenario.handle().subscribe();

    // The planner sees four free slots, but perf holds three of them.
    let first = watch(&scenario, "thief_target[0]", WatchAccess::Write).await;
    let refused = scenario
        .handle()
        .watch(&expression("thief_target[1]"), WatchAccess::Write)
        .await;
    assert!(
        matches!(refused, Err(Error::WatchpointHardwareBusy { thread }) if thread == main),
        "{refused:?}"
    );
    assert_eq!(
        scenario.snapshot().await.watchpoints.as_ref(),
        std::slice::from_ref(&first)
    );
    assert_eq!(
        watch_events(&mut events).len(),
        1,
        "only the success is published"
    );

    // Only the first watchpoint is armed, and it still works.
    assert_single_hit(&scenario.resume_to_stop().await, first.id, 0, 1);
    resume_to_exit(&mut scenario).await;
    scenario.shutdown().await;
}

#[tokio::test]
async fn arming_rolls_back_every_thread_when_a_later_thread_has_no_capacity() {
    let mut scenario = Scenario::new(
        "slot thief worker",
        Scenario::fixture("watch-slot-thief-worker"),
    );
    run_to_stolen_slots(&mut scenario).await;
    let main = selected_thread(&mut scenario).await;
    let threads = scenario.snapshot().await.threads;
    assert_eq!(threads.len(), 2);

    let refused = scenario
        .handle()
        .watch(&expression("thief_target[0]"), WatchAccess::Write)
        .await;
    assert!(
        matches!(refused, Err(Error::WatchpointHardwareBusy { thread }) if thread != main),
        "the worker holding every slot refuses arming: {refused:?}"
    );
    assert!(scenario.snapshot().await.watchpoints.is_empty());

    // The main thread was armed first and must have been rolled back: its
    // write to the target does not stop.
    resume_to_exit(&mut scenario).await;
    scenario.shutdown().await;
}

/// The watch-attach fixture, which reports the address of the word it writes
/// once released.
struct AttachTarget {
    process: support::ExternalProcess,
    watched: u64,
}

impl AttachTarget {
    fn spawn() -> Self {
        let process = support::ExternalProcess::spawn(&Scenario::fixture("watch-attach"));
        let watched = process
            .ready_line()
            .strip_prefix("READY 0x")
            .and_then(|address| u64::from_str_radix(address, 16).ok())
            .unwrap_or_else(|| panic!("unexpected readiness {:?}", process.ready_line()));
        Self { process, watched }
    }

    /// Arms DR0 on the target from a tracer that then exits without
    /// detaching, as a crashed debugger would.
    fn orphan_watchpoint(&self) {
        let status = std::process::Command::new(Scenario::fixture("watch-orphaner"))
            .arg(self.process.process_id().get().to_string())
            .arg(format!("{:x}", self.watched))
            .status()
            .expect("run orphaning tracer");
        assert!(status.success(), "orphaning tracer failed: {status}");
    }
}

#[tokio::test]
async fn attaching_clears_watchpoints_a_dead_tracer_left_armed() {
    use std::os::unix::process::ExitStatusExt as _;

    // Control: without microscope the orphaned watchpoint kills the target, so
    // the scenario below cannot pass vacuously.
    let mut unattended = AttachTarget::spawn();
    unattended.orphan_watchpoint();
    unattended.process.release();
    assert_eq!(
        unattended.process.wait().signal(),
        Some(5),
        "SIGTRAP kills the target"
    );

    // Detaching immediately must leave the target able to write.
    let mut target = AttachTarget::spawn();
    target.orphan_watchpoint();
    let debugger = target.process.attach().await;
    debugger.shutdown().await.expect("detach");
    target.process.release();
    assert_eq!(target.process.wait().code(), Some(0));

    // While attached, the foreign registers must already be clear: a hit
    // in a slot no watchpoint owns would be an unclassifiable stop.
    let mut target = AttachTarget::spawn();
    target.orphan_watchpoint();
    let debugger = target.process.attach().await;
    target.process.release();
    assert_eq!(
        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            debugger.handle().resume()
        )
        .await
        .expect("resume timed out")
        .expect("resume attached target"),
        StopReason::Exited(ExitStatus::Code(0))
    );
    debugger.shutdown().await.expect("shut down");
    drop(target);
}

#[tokio::test]
async fn detaching_disarms_watchpoints_even_while_they_are_being_hit() {
    // Detaching races the target's next hit; repeat it so a trap queued at
    // the interrupt, or a register left armed, is caught reliably.
    for _ in 0..8 {
        let mut target = AttachTarget::spawn();
        let debugger = target.process.attach().await;
        let handle = debugger.handle();
        let watchpoint = handle
            .watch(&expression("attach_watched"), WatchAccess::Write)
            .await
            .expect("watch attached global");
        assert_eq!(watchpoint.address.get(), target.watched);

        target.process.release();
        for expected in 1..=3_u64 {
            let reason = tokio::time::timeout(std::time::Duration::from_secs(5), handle.resume())
                .await
                .expect("resume timed out")
                .expect("resume attached target");
            assert_single_hit(&reason, watchpoint.id, expected - 1, expected);
        }

        // Detach while the target runs and keeps hitting the watchpoint. A
        // debug register left armed, or a hit's SIGTRAP left queued, would
        // kill it after detaching.
        let mut events = handle.subscribe();
        let microscope::InferiorState::Stopped {
            process_id,
            stop_id,
            ..
        } = handle.snapshot().await.expect("snapshot").inferior
        else {
            panic!("attached target is stopped");
        };

        handle
            .continue_execution(
                stop_id,
                microscope::ResumeScope::Process(process_id),
                microscope::ExceptionDisposition::Pass,
            )
            .await
            .expect("continue attached target");
        debugger.shutdown().await.expect("detach while running");
        assert!(
            std::iter::from_fn(|| events.try_recv().ok())
                .any(|event| matches!(event, DebuggerEvent::InferiorDetached { .. }))
        );

        let status = target.process.wait();
        assert_eq!(
            status.code(),
            Some(0),
            "every write completed after detach: {status}"
        );
    }
}

#[tokio::test]
async fn watchpoints_work_across_the_rust_and_zig_matrix() {
    for (fixture, function, global) in [
        ("watch-rust-o0", "watch_ready", "watch::WATCHED"),
        ("watch-rust-o2", "watch_ready", "watch::WATCHED"),
        ("watch-zig-o0", "watchReady", "watch.watched"),
        ("watch-zig-o2", "watchReady", "watch.watched"),
    ] {
        let mut scenario = Scenario::launch(fixture);
        run_to(&mut scenario, function).await;
        let watchpoint = watch(&scenario, global, WatchAccess::Write).await;
        assert!(
            matches!(watchpoint.scope, WatchScope::Static { .. }),
            "{fixture}: {:?}",
            watchpoint.scope
        );
        assert_eq!(watchpoint.byte_size, 8, "{fixture}");

        // A change watchpoint on the same bytes shares the hardware slot.
        let changes = watch(&scenario, global, WatchAccess::Change).await;
        let mut values = BTreeMap::<_, Vec<_>>::new();

        loop {
            match scenario.resume_to_stop().await {
                StopReason::Watchpoint { hits } => {
                    for hit in hits.iter() {
                        values
                            .entry(hit.watchpoint)
                            .or_default()
                            .push(value(hit.current.as_ref()));
                    }
                }
                StopReason::Exited(ExitStatus::Code(0)) => break,
                other => panic!("{fixture}: unexpected stop {other:?}"),
            }
        }

        assert_eq!(values[&watchpoint.id], [1, 2, 3, 3], "{fixture}");
        assert_eq!(values[&changes.id], [1, 2, 3], "{fixture}");
        scenario.shutdown().await;
    }
}

#[tokio::test]
async fn go_watchpoints_follow_goroutines_onto_new_threads() {
    let mut scenario = Scenario::launch("watch-go-o0");
    scenario.add_breakpoint("main.watchReady").await;
    let mut reason = scenario.run_to_stop().await;
    while matches!(&reason, StopReason::Exception(exception) if exception.code == 23) {
        reason = scenario.resume_to_stop().await;
    }

    assert!(
        matches!(reason, StopReason::Breakpoint { .. }),
        "{reason:?}"
    );
    let watchpoint = watch(&scenario, "main.watchedCounter", WatchAccess::Write).await;

    // A Go local exists only past the line declaring it.
    let source = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/go/watch/main.go"),
    )
    .expect("read the fixture's source");
    let declared = source
        .lines()
        .position(|line| line.contains("local := int64(5)"))
        .expect("the fixture declares local");
    scenario
        .add_source_breakpoint(
            "main.go",
            u64::try_from(declared + 2).expect("lines fit u64"),
        )
        .await;

    let mut hit_count = 0;
    let mut writers = BTreeSet::new();

    loop {
        match scenario.resume_to_stop().await {
            StopReason::Watchpoint { .. } => {
                for hit in thread_hits(&mut scenario).await {
                    assert_eq!(hit.watchpoint, watchpoint.id);
                    hit_count += 1;
                    writers.insert(hit.thread);
                }
            }
            StopReason::Exception(exception) if exception.code == 23 => {}
            StopReason::Breakpoint { .. } => break,
            other => panic!("unexpected stop {other:?}"),
        }
    }

    // watchReady's reset plus 4 goroutines x 5 increments.
    assert_eq!(hit_count, 21);
    assert!(writers.len() >= 2, "goroutines ran on several threads");

    scenario
        .operation("remove all", scenario.handle().remove_all_watchpoints())
        .await;
    scenario
        .operation(
            "remove breakpoints",
            scenario.handle().remove_all_breakpoints(),
        )
        .await;

    loop {
        match scenario.resume_to_stop().await {
            StopReason::Exited(ExitStatus::Code(0)) => break,
            StopReason::Exception(exception) if exception.code == 23 => {}
            other => panic!("unexpected stop {other:?}"),
        }
    }

    scenario.shutdown().await;
}

#[tokio::test]
async fn invalid_requests_fail_with_typed_errors_and_leave_state_unchanged() {
    let mut scenario = Scenario::launch(FIXTURE);
    let location = |address, byte_size| WatchpointSpec::Location { address, byte_size };

    // No process exists yet.
    assert!(matches!(
        scenario
            .handle()
            .add_watchpoint(location(VirtualAddress::new(0x1000), 8), WatchAccess::Write)
            .await,
        Err(Error::NotRunning)
    ));
    assert!(matches!(
        scenario
            .handle()
            .resolve_watch_target(&expression("watch_i32"))
            .await,
        Err(Error::NotRunning)
    ));
    assert!(matches!(
        scenario
            .handle()
            .remove_watchpoint(WatchpointId::new(1))
            .await,
        Err(Error::WatchpointNotFound(1))
    ));

    run_to(&mut scenario, "scalar_stores").await;
    let mut events = scenario.handle().subscribe();
    let user_limit = 0x7fff_ffff_f000_u64;
    for (address, byte_size) in [
        (0x1000, 0),
        (u64::MAX - 3, 8),
        (user_limit - 4, 8),
        (user_limit, 1),
        (0xffff_ffff_8100_0000, 8),
    ] {
        let result = scenario
            .handle()
            .add_watchpoint(
                location(VirtualAddress::new(address), byte_size),
                WatchAccess::Write,
            )
            .await;
        assert!(
            matches!(result, Err(Error::InvalidWatchRange { .. })),
            "{address:#x}+{byte_size}: {result:?}"
        );
    }

    assert!(matches!(
        scenario
            .handle()
            .add_watchpoint(
                location(VirtualAddress::new(0x1000), 40),
                WatchAccess::Write
            )
            .await,
        Err(Error::WatchpointCapacity {
            required: 5,
            available: 4
        })
    ));
    assert!(matches!(
        scenario
            .handle()
            .remove_watchpoint(WatchpointId::new(99))
            .await,
        Err(Error::WatchpointNotFound(99))
    ));
    let removed = scenario.handle().remove_all_watchpoints().await;
    assert!(removed.is_ok_and(|removed| removed.is_empty()));
    assert!(matches!(
        scenario
            .handle()
            .watch(&expression("no_such_global"), WatchAccess::Write)
            .await,
        Err(Error::Expression(error)) if error.kind == microscope::ExpressionErrorKind::UnknownName
    ));
    assert!(scenario.snapshot().await.watchpoints.is_empty());
    assert!(
        watch_events(&mut events).is_empty(),
        "failed requests publish nothing"
    );

    // A target resolved at an earlier stop cannot be armed at a later one.
    let stale = scenario
        .operation(
            "resolve",
            scenario
                .handle()
                .resolve_watch_target(&expression("watch_i32")),
        )
        .await;
    let kind = StepKind::Instruction;
    assert_eq!(scenario.step_to_stop(kind).await, StopReason::Step { kind });
    assert!(matches!(
        scenario
            .handle()
            .add_watchpoint(WatchpointSpec::Target(Box::new(stale)), WatchAccess::Write)
            .await,
        Err(Error::StaleStop)
    ));
    scenario.shutdown().await;
}

#[tokio::test]
async fn values_without_watchable_memory_are_refused_with_typed_errors() {
    // Registers, computed values, and entry values in optimized code.
    for (fixture, refusals) in [
        (
            "variables-parameters-gcc-o2",
            &[("boolean", "register")][..],
        ),
        (
            "variables-parameters-clang-o2",
            &[("boolean", "computed"), ("signed_character", "unavailable")][..],
        ),
    ] {
        let mut scenario = Scenario::launch(fixture);
        scenario
            .add_source_breakpoint("variables-parameters.c", 22)
            .await;
        assert!(matches!(
            scenario.run_to_stop().await,
            StopReason::Breakpoint { .. }
        ));

        for &(name, expected) in refusals {
            let result = scenario
                .handle()
                .resolve_watch_target(&expression(name))
                .await;
            match (expected, &result) {
                ("register" | "computed", Err(Error::WatchTargetNotInMemory(reason))) => {
                    assert!(reason.contains(expected), "{fixture}: {reason}");
                }
                ("unavailable", Err(Error::WatchTargetUnavailable(_))) => {}
                _ => panic!("{fixture} {name}: expected {expected}, got {result:?}"),
            }
        }

        assert!(scenario.snapshot().await.watchpoints.is_empty());
        scenario.shutdown().await;
    }

    // A bit-field member is extracted from its storage, not addressable.
    let mut scenario = Scenario::launch("records-c-gcc-o0");
    run_to(&mut scenario, "inspect_records").await;
    let result = scenario
        .handle()
        .resolve_watch_target(&expression("bits.second"))
        .await;
    assert!(
        matches!(&result, Err(Error::WatchTargetNotInMemory(reason)) if reason.contains("bit-field")),
        "{result:?}"
    );

    // The pointer parameter is frame storage; the record it points to has
    // no lifetime the debugger can know.
    let pointer = scenario
        .operation(
            "resolve record pointer",
            scenario
                .handle()
                .resolve_watch_target(&expression("record")),
        )
        .await;
    assert_eq!(pointer.byte_size(), 8);
    assert!(
        matches!(pointer.scope(), WatchScope::Frame { .. }),
        "{pointer:?}"
    );
    let member = scenario
        .operation(
            "resolve record member",
            scenario
                .handle()
                .resolve_watch_target(&expression("record.inner")),
        )
        .await;
    assert_eq!(member.byte_size(), 8);
    assert_eq!(member.scope(), &WatchScope::Location);
    scenario.shutdown().await;

    // A debug-information constant has no storage.
    let mut scenario = Scenario::launch("globals-cpp-gcc-o0");
    run_to(&mut scenario, "inspect_globals").await;
    let result = scenario
        .handle()
        .resolve_watch_target(&expression("fixture::Holder::constexpr_member"))
        .await;
    assert!(
        matches!(&result, Err(Error::WatchTargetNotInMemory(reason)) if reason.contains("constant")),
        "{result:?}"
    );
    scenario.shutdown().await;
}

#[tokio::test]
async fn post_mortem_targets_refuse_watchpoints() {
    let core = Scenario::fixture("crash-gcc-o0-segv.core");
    let scenario = Scenario::open_core("core watch", &microscope::CoreDumpOptions::new(core));
    assert!(matches!(
        scenario
            .handle()
            .watch(&expression("main"), WatchAccess::Write)
            .await,
        Err(Error::PostMortemTarget)
    ));
    for result in [
        scenario
            .handle()
            .add_watchpoint(
                WatchpointSpec::Location {
                    address: VirtualAddress::new(0x1000),
                    byte_size: 8,
                },
                WatchAccess::Write,
            )
            .await
            .map(|_| ()),
        scenario
            .handle()
            .remove_watchpoint(WatchpointId::new(1))
            .await
            .map(|_| ()),
        scenario.handle().remove_all_watchpoints().await.map(|_| ()),
    ] {
        assert!(matches!(result, Err(Error::PostMortemTarget)), "{result:?}");
    }

    scenario.shutdown().await;
}

#[tokio::test]
async fn repeated_sessions_with_armed_watchpoints_leave_no_processes_behind() {
    for iteration in 0..4 {
        let fixture = MATRIX[iteration % MATRIX.len()];
        let mut scenario = Scenario::launch(fixture);
        run_to(&mut scenario, "scalar_stores").await;
        watch(&scenario, "watch_i32", WatchAccess::Write).await;
        watch(&scenario, "watch_u64", WatchAccess::ReadWrite).await;
        if iteration % 2 == 0 {
            // Shut down while the process runs between hits.
            let running = scenario.start_resuming().await;
            drop(running);
        }

        scenario.shutdown().await;
    }
}

#[tokio::test]
async fn unloading_a_module_ends_watchpoints_on_its_static_storage() {
    let fixture = "globals-shared";
    let mut scenario = Scenario::launch(fixture);
    run_to(&mut scenario, "after_load").await;
    let library = scenario
        .operation("list modules", scenario.handle().loaded_modules())
        .await
        .modules
        .iter()
        .find(|module| module.path.ends_with("libglobals.so"))
        .expect("the library is loaded")
        .module
        .id;
    let owned = watch(&scenario, "dso_external", WatchAccess::Write).await;
    assert_eq!(owned.scope, WatchScope::Static { module: library });

    // The same bytes reached through a pointer have no known owner.
    let pointed = watch(&scenario, "*cross_module_pointer", WatchAccess::Write).await;
    assert_eq!(pointed.address, owned.address);
    assert_eq!(pointed.scope, WatchScope::Location);
    scenario.add_breakpoint("after_unload").await;
    let mut events = scenario.handle().subscribe();

    assert!(matches!(
        scenario.resume_to_stop().await,
        StopReason::Breakpoint { .. }
    ));
    let invalidated = std::iter::from_fn(|| events.try_recv().ok())
        .find_map(|event| match event {
            DebuggerEvent::WatchpointsInvalidated { invalidated, .. } => Some(invalidated),
            _ => None,
        })
        .expect("unloading invalidated the library's watchpoint");
    assert!(matches!(
        invalidated.as_ref(),
        [entry] if entry.watchpoint == owned
            && entry.reason == WatchpointInvalidation::ModuleUnloaded
    ));
    assert_eq!(
        scenario.snapshot().await.watchpoints.as_ref(),
        std::slice::from_ref(&pointed)
    );

    scenario
        .operation(
            "remove pointer watchpoint",
            scenario.handle().remove_watchpoint(pointed.id),
        )
        .await;
    resume_to_exit(&mut scenario).await;
    scenario.shutdown().await;
}

#[tokio::test]
async fn attached_processes_arm_threads_they_create_later() {
    let mut target = AttachTarget::spawn();
    let debugger = target.process.attach().await;
    let handle = debugger.handle();
    let main = handle
        .snapshot()
        .await
        .expect("snapshot")
        .selected
        .and_then(microscope::ExecutionContext::as_thread)
        .expect("selected thread");
    let watchpoint = handle
        .watch(&expression("attach_watched"), WatchAccess::Write)
        .await
        .expect("watch attached global");
    target.process.release();

    let mut values = Vec::new();
    let mut writers = BTreeSet::new();

    loop {
        let reason = tokio::time::timeout(std::time::Duration::from_secs(5), handle.resume())
            .await
            .expect("resume timed out")
            .expect("resume attached target");

        match reason {
            StopReason::Watchpoint { hits } => {
                let [hit] = hits.as_ref() else {
                    panic!("one writer at a time: {hits:?}");
                };

                assert_eq!(hit.watchpoint, watchpoint.id);
                values.push(value(hit.current.as_ref()));
                writers.insert(hit.thread);
            }
            StopReason::Exited(ExitStatus::Code(0)) => break,
            other => panic!("unexpected stop: {other:?}"),
        }
    }

    assert_eq!(values, (1..=200).collect::<Vec<_>>());
    assert_eq!(
        writers.len(),
        2,
        "the main thread and a thread created later"
    );
    assert!(writers.contains(&main));
    debugger.shutdown().await.expect("shut down");
    drop(target);
}

/// One change a watchpoint reported: what it watched, and its value before
/// and after.
type Change = (String, u64, u64);

/// An expression to watch for changes, with the condition its stops need.
type Watched<'a> = (&'a str, Option<&'a str>);

/// Runs a fixture to its end under gdb with a `watch` on each expression,
/// which stops only where a store changes the value, and collects what gdb
/// reported.
fn gdb_changes(fixture: &str, expressions: &[Watched<'_>]) -> Vec<Change> {
    let mut command = std::process::Command::new("gdb");
    command.args(["--quiet", "--nx", "--batch"]);
    for setup in [
        "set debuginfod enabled off",
        "set pagination off",
        "handle SIGUSR1 nostop noprint pass",
        "break main",
        "run",
    ] {
        command.args(["-ex", setup]);
    }

    for (expression, condition) in expressions {
        let condition = condition.map_or_else(String::new, |condition| format!(" if {condition}"));
        command.args(["-ex", &format!("watch {expression}{condition}")]);
    }

    for _ in 0..48 {
        command.args(["-ex", "continue"]);
    }

    let output = command
        .arg(Scenario::fixture(fixture))
        .output()
        .expect("run gdb");
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(
        text.contains("exited normally"),
        "{fixture}: gdb did not run the program to its end:\n{text}"
    );
    let number = |line: &str, prefix: &str| {
        line.strip_prefix(prefix)
            .and_then(|rest| rest.split_whitespace().next())
            .and_then(|value| value.parse::<u64>().ok())
    };

    let lines = text
        .lines()
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>();

    lines
        .windows(3)
        .filter_map(|window| {
            let expression = window[0]
                .strip_prefix("Hardware watchpoint ")?
                .split_once(": ")?
                .1;
            Some((
                expression.to_owned(),
                number(window[1], "Old value = ")?,
                number(window[2], "New value = ")?,
            ))
        })
        .collect()
}

/// Runs a fixture to its end with a change watchpoint on each expression
/// and collects what they reported.
async fn microscope_changes(fixture: &str, expressions: &[Watched<'_>]) -> Vec<Change> {
    let mut scenario = Scenario::launch(fixture);
    run_to(&mut scenario, "main").await;
    let mut watched = BTreeMap::new();
    for (expression, condition) in expressions {
        let options = WatchpointOptions {
            condition: condition.map(|text| microscope::Condition::parse(text).expect("condition")),
            ..WatchpointOptions::default()
        };

        let watchpoint = scenario
            .operation(
                &format!("watch {expression}"),
                scenario.handle().watch_with(
                    &microscope::Expression::parse(expression).expect("expression"),
                    WatchAccess::Change,
                    options,
                ),
            )
            .await;
        assert_eq!(watchpoint.access, WatchAccess::Change);
        watched.insert(watchpoint.id, *expression);
    }

    let mut changes = Vec::new();

    loop {
        match scenario.resume_to_stop().await {
            StopReason::Watchpoint { hits } => {
                for hit in hits.iter() {
                    assert!(hit.changed(), "{fixture}: {hit:?}");
                    changes.push((
                        watched[&hit.watchpoint].to_owned(),
                        value(hit.previous.as_ref()),
                        value(hit.current.as_ref()),
                    ));
                }
            }
            StopReason::Exception(exception) if matches!(exception.code, 10 | 17) => {}
            StopReason::Exited(ExitStatus::Code(0)) => break,
            other => panic!("{fixture}: unexpected stop {other:?}"),
        }
    }

    scenario.shutdown().await;
    changes
}

/// gdb's `watch` is an independent oracle for which stores change a value:
/// stores of the value already there, including a failed `lock cmpxchg`
/// writing its destination back, are not reported, each `rep stos`
/// iteration that changes a byte is, and a kernel write is seen only once a
/// later store differs from the value last observed.
#[tokio::test]
async fn change_watchpoints_report_the_changes_gdb_watch_reports() {
    for fixture in MATRIX {
        for expressions in [
            &[
                "watch_i32",
                "watch_u64",
                "watch_wide.words[0]",
                "watch_steady",
            ][..],
            &["watch_kernel", "watch_pair.first", "watch_u8", "watch_u16"][..],
        ] {
            let expressions = expressions
                .iter()
                .map(|expression| (*expression, None))
                .collect::<Vec<_>>();
            let expected = gdb_changes(fixture, &expressions);
            assert!(expected.len() >= 5, "{fixture}: {expected:?}");
            assert_eq!(
                microscope_changes(fixture, &expressions).await,
                expected,
                "{fixture}: changes differ from gdb's"
            );
        }
    }
}

/// gdb's conditional `watch` is an independent oracle for what a change
/// that a condition declines leaves: the value the next reported change is
/// a change from, as each check sees it.
#[tokio::test]
async fn conditional_change_watchpoints_report_the_changes_gdb_reports() {
    for fixture in ["hit-counts-gcc-o0", "hit-counts-clang-o2"] {
        let expressions = [
            ("last_call", Some("last_call % 7 == 3")),
            ("shared_total", Some("shared_total % 3 == 0")),
        ];
        let expected = gdb_changes(fixture, &expressions);
        assert!(expected.len() >= 10, "{fixture}: {expected:?}");

        // Each reported call follows a declined one, the old value.
        assert!(
            expected
                .iter()
                .filter(|(name, ..)| name == "last_call")
                .all(|(_, old, new)| old + 1 == *new),
            "{fixture}: {expected:?}"
        );
        assert_eq!(
            microscope_changes(fixture, &expressions).await,
            expected,
            "{fixture}: changes differ from gdb's"
        );
    }
}

/// Runs a script of steps through `steady_stores`, whose lines and callee
/// store the value the variable already holds, and returns where each step
/// stopped. The script depends only on where each step stops: it steps out
/// only of the callee and ends once execution leaves both functions.
async fn steady_step_script(fixture: &str, access: Option<WatchAccess>) -> Vec<(StopReason, u64)> {
    steady_step_script_with(fixture, access, WatchpointOptions::default()).await
}

async fn steady_step_script_with(
    fixture: &str,
    access: Option<WatchAccess>,
    options: WatchpointOptions,
) -> Vec<(StopReason, u64)> {
    use StepKind::{Instruction, IntoSource, Out, OverInstruction, OverSource};
    let mut scenario = Scenario::launch(fixture);
    run_to(&mut scenario, "steady_stores").await;
    scenario
        .operation(
            "remove breakpoints",
            scenario.handle().remove_all_breakpoints(),
        )
        .await;
    if let Some(access) = access {
        scenario
            .operation(
                "watch watch_steady",
                scenario
                    .handle()
                    .watch_with(&expression("watch_steady"), access, options),
            )
            .await;
    }

    let mut stops = Vec::new();
    for kind in [
        OverSource,
        IntoSource,
        Instruction,
        Out,
        OverSource,
        OverInstruction,
        IntoSource,
        Instruction,
        Out,
        OverSource,
        IntoSource,
        Out,
        Instruction,
        OverSource,
        OverSource,
        OverSource,
        OverSource,
        OverSource,
    ] {
        let function = stopped_function(&scenario).await;
        let kind = match function.as_deref() {
            Some("steady_stores") if kind == Out => OverSource,
            Some("steady_store" | "steady_stores") => kind,
            _ => break,
        };

        let reason = scenario.step_to_stop(kind).await;
        stops.push((reason, program_counter(&scenario).await));
    }

    if access == Some(WatchAccess::Change) {
        // The watchpoint stayed armed throughout and still reports the
        // first store that changes the value, but not the one repeating it.
        let reason = scenario.resume_to_stop().await;
        assert_single_hit(&reason, hits(&reason)[0].watchpoint, 7, 8);
        assert_eq!(
            stopped_function(&scenario).await.as_deref(),
            Some("steady_change"),
            "{fixture}"
        );
        resume_to_exit(&mut scenario).await;
    }

    scenario.shutdown().await;
    stops
}

#[tokio::test]
async fn steps_cross_stores_that_change_nothing_or_are_declined_as_if_unwatched() {
    for fixture in MATRIX {
        let plain = steady_step_script(fixture, None).await;
        assert!(plain.len() >= 10, "{fixture}: {plain:?}");
        assert_eq!(
            steady_step_script(fixture, Some(WatchAccess::Change)).await,
            plain,
            "{fixture}"
        );

        // The script does cross stores: a write watchpoint ends steps there.
        let written = steady_step_script(fixture, Some(WatchAccess::Write)).await;
        assert!(
            written
                .iter()
                .any(|(reason, _)| matches!(reason, StopReason::Watchpoint { .. })),
            "{fixture}: {written:?}"
        );

        // Unless its condition or hit condition declines every store.
        for options in [
            WatchpointOptions {
                condition: Some(
                    microscope::Condition::parse("watch_steady != 7").expect("condition"),
                ),
                ..WatchpointOptions::default()
            },
            WatchpointOptions {
                hit_condition: Some("==1000".parse().expect("hit condition")),
                ..WatchpointOptions::default()
            },
        ] {
            assert_eq!(
                steady_step_script_with(fixture, Some(WatchAccess::Write), options.clone()).await,
                plain,
                "{fixture}: {options:?}"
            );
        }
    }
}

#[tokio::test]
async fn a_debugger_write_is_what_the_next_store_is_judged_against() {
    let mut scenario = Scenario::launch(FIXTURE);
    run_to(&mut scenario, "scalar_stores").await;
    let watchpoint = watch(&scenario, "watch_i32", WatchAccess::Change).await;

    // Writing the value the program stores next hides that store.
    let word = scenario
        .operation("read word", scenario.handle().read_word(watchpoint.address))
        .await;
    scenario
        .operation(
            "write watched word",
            scenario
                .handle()
                .write_word(watchpoint.address, (word & !0xffff_ffff) | 1),
        )
        .await;
    assert_single_hit(&scenario.resume_to_stop().await, watchpoint.id, 1, 2);
    assert_single_hit(&scenario.resume_to_stop().await, watchpoint.id, 2, 42);

    scenario
        .operation(
            "remove watchpoint",
            scenario.handle().remove_watchpoint(watchpoint.id),
        )
        .await;
    resume_to_exit(&mut scenario).await;
    scenario.shutdown().await;
}

/// Reads a global's word.
async fn global(scenario: &Scenario, name: &str) -> u64 {
    let address = symbol(scenario, name).await;
    scenario
        .operation(
            &format!("read {name}"),
            scenario.handle().read_word(address),
        )
        .await
}

/// Each change hit's `previous` is the value at the previous stop, when the
/// debugger last observed it, so racing stores that leave the value as the
/// last stop saw it are never reported, and every report is a change.
#[tokio::test]
async fn racing_threads_report_changes_from_the_value_each_stop_observed() {
    const WORKERS: u64 = 2;
    const ITERATIONS: u64 = 40;
    const CHURN: u64 = 16;

    let mut scenario = Scenario::new("watch changes", Scenario::fixture("watch-threads"));
    run_to(&mut scenario, "before_threads").await;
    let locked = watch(&scenario, "locked_counter", WatchAccess::Change).await;
    let racing = watch(&scenario, "racing_counter", WatchAccess::Change).await;
    scenario
        .operation(
            "remove breakpoints",
            scenario.handle().remove_all_breakpoints(),
        )
        .await;

    let mut locked_values = Vec::new();
    let mut racing_stops = 0;
    let mut observed = BTreeMap::from([
        (locked.id, global(&scenario, "locked_counter").await),
        (racing.id, global(&scenario, "racing_counter").await),
    ]);

    loop {
        match scenario.resume_to_stop().await {
            StopReason::Watchpoint { .. } => {}
            StopReason::Exited(ExitStatus::Code(0)) => break,
            other => panic!("unexpected stop: {other:?}"),
        }

        let stop_hits = thread_hits(&mut scenario).await;
        assert!(!stop_hits.is_empty());
        for hit in &stop_hits {
            assert!(hit.changed(), "{hit:?}");
            assert_eq!(value(hit.previous.as_ref()), observed[&hit.watchpoint]);
        }

        if let Some(hit) = stop_hits.iter().find(|hit| hit.watchpoint == locked.id) {
            locked_values.push(value(hit.current.as_ref()));
        }

        racing_stops += u64::from(stop_hits.iter().any(|hit| hit.watchpoint == racing.id));
        observed = BTreeMap::from([
            (locked.id, global(&scenario, "locked_counter").await),
            (racing.id, global(&scenario, "racing_counter").await),
        ]);
    }

    // Every locked increment is a change; `after_join`'s `+= 0` is not.
    assert_eq!(
        locked_values,
        (1..=WORKERS * ITERATIONS + CHURN).collect::<Vec<_>>()
    );

    // The reset to zero in `before_threads` stores what is already there.
    assert!(
        (1..=WORKERS * ITERATIONS).contains(&racing_stops),
        "{racing_stops} racing stops"
    );
    scenario.shutdown().await;
}

#[tokio::test]
async fn stores_that_change_nothing_never_stop_threads_receiving_signals() {
    let mut scenario = Scenario::launch("watch-steady");
    run_to(&mut scenario, "main").await;
    let watchpoint = watch(&scenario, "steady", WatchAccess::Change).await;
    scenario
        .operation(
            "remove breakpoints",
            scenario.handle().remove_all_breakpoints(),
        )
        .await;
    scenario.add_breakpoint("finished").await;

    let mut signals = 0;
    let mut reason = scenario.resume_to_stop().await;
    while let StopReason::Exception(exception) = &reason {
        assert_eq!(exception.code, nix::sys::signal::Signal::SIGUSR1 as u64);
        signals += 1;
        reason = scenario
            .resume_with_exception(microscope::ExceptionDisposition::Pass)
            .await;
    }

    assert!(
        matches!(reason, StopReason::Breakpoint { .. }),
        "{reason:?}"
    );
    assert_eq!(signals, 12);
    let stores = global(&scenario, "stores").await;
    assert!(stores >= 12 * 40, "workers stored while signals arrived");

    let reason = scenario.resume_to_stop().await;
    assert_single_hit(&reason, watchpoint.id, 7, 8);
    assert_eq!(global(&scenario, "finished_stores").await, stores);
    assert_eq!(
        scenario.resume_to_stop().await,
        StopReason::Exited(ExitStatus::Code(0))
    );
    scenario.shutdown().await;
}

/// Resumes and pauses until the spinning threads have made `count` more
/// stores, none of which stopped, and returns the total.
async fn pause_after_stores(scenario: &mut Scenario, count: u64) -> u64 {
    let target = global(scenario, "stores").await + count;

    loop {
        let running = scenario.start_resuming().await;
        tokio::task::yield_now().await;
        scenario.operation("pause", scenario.handle().pause()).await;
        let reason = running.await.expect("resume task").expect("resume");
        assert_eq!(reason, StopReason::Pause);
        assert!(thread_hits(scenario).await.is_empty());
        let stores = global(scenario, "stores").await;
        if stores >= target {
            return stores;
        }
    }
}

#[tokio::test]
async fn pause_and_shutdown_interrupt_endless_stores_that_change_nothing() {
    let mut scenario = Scenario::launch("watch-steady-spin");
    run_to(&mut scenario, "store_steady").await;
    scenario
        .operation(
            "remove breakpoints",
            scenario.handle().remove_all_breakpoints(),
        )
        .await;
    watch(&scenario, "steady", WatchAccess::Change).await;
    pause_after_stores(&mut scenario, 200).await;

    // Shutting down while stores are being resolved reaps the inferior.
    let _running = scenario.start_resuming().await;
    scenario.shutdown().await;
}

#[tokio::test]
async fn attached_processes_detach_unharmed_while_stores_change_nothing() {
    let child = support::ExternalProcess::spawn_running(&Scenario::fixture("watch-steady-spin"));
    let mut scenario = Scenario::attached("attached steady", child.attach().await);
    let watchpoint = watch(&scenario, "steady", WatchAccess::Change).await;
    let stores = pause_after_stores(&mut scenario, 200).await;
    assert_eq!(scenario.snapshot().await.watchpoints[0].id, watchpoint.id);

    // Detaching while stores are being resolved disarms every thread first:
    // a leftover debug register would kill the process with SIGTRAP.
    let _running = scenario.start_resuming().await;
    scenario.shutdown().await;
    let mut reattached = Scenario::attached("reattached", child.attach().await);
    let snapshot = reattached.snapshot().await;
    assert_eq!(snapshot.threads.len(), 4, "every thread survived");
    assert!(snapshot.watchpoints.is_empty());
    assert!(global(&reattached, "stores").await > stores);
    reattached.shutdown().await;
}
