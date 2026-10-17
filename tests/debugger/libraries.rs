//! Breakpoints in shared libraries, which load after the program starts.

use super::*;
use microscope::{Breakpoint, DebuggerEvent, LoadedModuleSnapshot};

/// A function breakpoint kept while no loaded module defines the function.
pub async fn pending_function(scenario: &Scenario, name: &str) -> Breakpoint {
    scenario
        .operation(
            "add pending breakpoint",
            scenario.handle().add_breakpoint_with(
                BreakpointSpec::Function(name.to_owned()),
                microscope::BreakpointOptions {
                    pending: true,
                    ..microscope::BreakpointOptions::default()
                },
            ),
        )
        .await
}

fn library_locations(breakpoint: &Breakpoint) -> usize {
    breakpoint
        .locations
        .iter()
        .filter(|location| location.library.is_some())
        .count()
}

/// The addresses of a breakpoint's locations, all in libraries.
fn library_addresses(breakpoint: &Breakpoint) -> Vec<u64> {
    breakpoint
        .locations
        .iter()
        .map(|location| match &location.location {
            BreakpointLocation::Virtual(address) => address.get(),
            BreakpointLocation::Image(address) => panic!("{address:?} is in the program"),
            BreakpointLocation::Managed(managed) => panic!("{managed:?} is a runtime's"),
        })
        .collect()
}

pub async fn breakpoint(scenario: &mut Scenario, id: microscope::BreakpointId) -> Breakpoint {
    scenario
        .snapshot()
        .await
        .breakpoints
        .iter()
        .find(|breakpoint| breakpoint.id == id)
        .expect("breakpoint")
        .clone()
}

#[tokio::test]
async fn breakpoints_in_a_library_the_program_links_resolve_before_it_runs() {
    let mut scenario = Scenario::launch("module-frames-gcc-o0");

    // Without the pending option, a function nothing defines or imports is
    // refused, but one the program imports waits for its library.
    let refused = scenario
        .attempt(
            "add missing breakpoint",
            scenario
                .handle()
                .add_breakpoint(BreakpointSpec::Function("dso_absent".to_owned())),
        )
        .await;
    assert!(matches!(
        refused,
        Err(microscope::Error::FunctionNotFound(_))
    ));
    let pending = scenario
        .operation(
            "add imported breakpoint",
            scenario
                .handle()
                .add_breakpoint(BreakpointSpec::Function("dso_apply".to_owned())),
        )
        .await;
    assert!(
        pending.locations.is_empty(),
        "nothing defines it before launch"
    );
    let reason = scenario.run_to_stop().await;
    let StopReason::Breakpoint { hits, .. } = &reason else {
        panic!("stopped for {reason:?}");
    };

    assert_eq!(hits[0].breakpoint, pending.id);
    let resolved = breakpoint(&mut scenario, pending.id).await;
    assert_eq!(library_locations(&resolved), 1);
    let trace = scenario
        .operation("backtrace", scenario.handle().backtrace())
        .await;
    assert_eq!(
        trace.frames[0]
            .function
            .as_ref()
            .map(|function| &*function.name),
        Some("dso_apply")
    );
    scenario.shutdown().await;
}

#[tokio::test]
async fn stepping_out_of_a_library_frame_unwinds_through_the_library() {
    let mut scenario = Scenario::launch("module-frames-gcc-o0");
    pending_function(&scenario, "dso_apply").await;
    let reason = scenario.run_to_stop().await;
    assert!(
        matches!(reason, StopReason::Breakpoint { .. }),
        "{reason:?}"
    );

    // Only the library's call-frame information finds the caller.
    assert_eq!(
        scenario.step_to_stop(StepKind::Out).await,
        StopReason::Step {
            kind: StepKind::Out
        }
    );
    let location = scenario
        .operation("caller", scenario.handle().current_location())
        .await;
    assert_eq!(location_function(&location), Some("main"));
    assert_eq!(location_line(&location), Some(33));
    scenario.shutdown().await;
}

#[tokio::test]
async fn breakpoints_follow_a_library_through_dlopen_and_dlclose() {
    let mut scenario = Scenario::launch("globals-shared");
    let touch = pending_function(&scenario, "dso_touch").await;
    scenario.add_breakpoint("after_unload").await;
    let mut events = scenario.handle().subscribe();

    // The library is opened, used, closed, and opened again: its breakpoint
    // stops in each use, and is pending between them.
    for (round, expected) in [(1, "dso_touch"), (1, "after_unload"), (2, "dso_touch")] {
        let reason = scenario.resume_or_run().await;
        let StopReason::Breakpoint { hits, .. } = &reason else {
            panic!("round {round} stopped for {reason:?}");
        };

        let stopped = scenario
            .snapshot()
            .await
            .breakpoints
            .iter()
            .find(|breakpoint| breakpoint.id == hits[0].breakpoint)
            .expect("hit breakpoint")
            .spec
            .to_string();
        assert_eq!(stopped, expected, "round {round}");
        let touch_now = breakpoint(&mut scenario, touch.id).await;
        let expected_locations = usize::from(expected == "dso_touch");
        assert_eq!(
            library_locations(&touch_now),
            expected_locations,
            "round {round}"
        );
    }

    let changes = std::iter::from_fn(|| events.try_recv().ok())
        .filter(|event| matches!(event, DebuggerEvent::BreakpointsChanged { .. }))
        .count();
    assert!(
        changes >= 3,
        "loads and unloads change the breakpoint: {changes}"
    );
    assert_eq!(
        scenario.resume_to_stop().await,
        StopReason::Exited(ExitStatus::Code(0))
    );

    // A new process starts with the library unloaded again.
    assert!(
        breakpoint(&mut scenario, touch.id)
            .await
            .locations
            .is_empty()
    );
    scenario.shutdown().await;
}

#[tokio::test]
async fn source_breakpoints_resolve_in_library_source_once_it_loads() {
    let mut scenario = Scenario::launch("globals-shared");
    let path =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/c/shared/library.c");
    let line = source_line("tests/fixtures/c/shared/library.c", "return *dso_pointer");
    let breakpoint = scenario
        .operation(
            "add pending source breakpoint",
            scenario.handle().add_breakpoint_with(
                BreakpointSpec::Source {
                    path: path.clone(),
                    line: microscope::LineNumber::new(line).expect("line"),
                },
                microscope::BreakpointOptions {
                    pending: true,
                    ..microscope::BreakpointOptions::default()
                },
            ),
        )
        .await;
    assert!(breakpoint.locations.is_empty());
    assert!(matches!(
        scenario.run_to_stop().await,
        StopReason::Breakpoint { hits, .. } if hits[0].breakpoint == breakpoint.id
    ));
    let location = scenario
        .operation("location", scenario.handle().current_location())
        .await;
    assert_eq!(location_line(&location), Some(line));
    scenario.shutdown().await;
}

#[tokio::test]
async fn functions_without_debug_information_break_at_their_symbol() {
    let mut scenario = Scenario::launch("reexec");
    let puts = pending_function(&scenario, "puts").await;
    let reason = scenario
        .run_with_to_stop(LaunchOptions {
            arguments: vec!["one".into()],
            ..LaunchOptions::default()
        })
        .await;
    let StopReason::Breakpoint { hits, .. } = &reason else {
        panic!("stopped for {reason:?}");
    };

    assert_eq!(hits[0].breakpoint, puts.id);
    assert_eq!(
        library_locations(&breakpoint(&mut scenario, puts.id).await),
        1
    );
    let trace = scenario
        .operation("backtrace", scenario.handle().backtrace())
        .await;
    let symbol = trace.frames[0].symbol.as_ref().expect("a symbol frame");

    // libc's `_IO_puts` names the same entry.
    assert!(
        symbol.name.ends_with("puts") && symbol.offset == 0,
        "{symbol:?}"
    );
    assert_eq!(
        trace.frames[1]
            .function
            .as_ref()
            .map(|function| &*function.name),
        Some("reexecuted")
    );
    scenario.shutdown().await;
}

/// Runs to the first stop at `breakpoint`, past the hits of others.
async fn run_to_breakpoint(scenario: &mut Scenario, breakpoint: microscope::BreakpointId) {
    for stop in 0..64 {
        let reason = if stop == 0 {
            scenario.run_to_stop().await
        } else {
            scenario.resume_to_stop().await
        };

        let StopReason::Breakpoint { hits, .. } = &reason else {
            panic!("stopped for {reason:?}");
        };

        if hits.iter().any(|hit| hit.breakpoint == breakpoint) {
            return;
        }
    }

    panic!("breakpoint {breakpoint} was not reached in 64 stops");
}

/// Continues to a stop in the C library's `strlen` from the fixture's
/// `measure`, which clang calls by a tail call that leaves `main` its
/// caller.
async fn stop_in_strlen(scenario: &mut Scenario, strlen: microscope::BreakpointId) {
    let reason = scenario.resume_to_stop().await;
    let StopReason::Breakpoint { hits, .. } = &reason else {
        panic!("stopped for {reason:?}");
    };

    assert_eq!(hits[0].breakpoint, strlen);
    let trace = scenario
        .operation("backtrace", scenario.handle().backtrace())
        .await;
    let names = trace
        .frames
        .iter()
        .take(2)
        .map(|frame| {
            frame
                .symbol
                .as_ref()
                .map(|symbol| (&*symbol.name, symbol.offset))
        })
        .collect::<Vec<_>>();

    // The implementation, not the resolver, which is `strlen` itself.
    let Some((implementation, 0)) = names[0] else {
        panic!("stopped in {names:?}");
    };

    assert!(implementation.starts_with("__strlen_"), "{names:?}");
    assert!(
        matches!(names[1], Some(("measure" | "main", _))),
        "{names:?}"
    );
}

/// An indirect function's symbol names its resolver, which the loader calls
/// to choose an implementation for the machine. A breakpoint on the
/// function stops in the implementation chosen, as gdb's does: read from a
/// GOT slot the loader filled, or else caught as the resolver returns,
/// before a static program's start relocates it.
async fn indirect_function_pending_before_the_run(fixture: &str) {
    let mut scenario = Scenario::launch(fixture);
    let measure = pending_function(&scenario, "measure").await;
    let strlen = pending_function(&scenario, "strlen").await;

    // The loader and the C library measure strings of their own first.
    run_to_breakpoint(&mut scenario, measure.id).await;
    let resolved = breakpoint(&mut scenario, strlen.id).await;
    assert!(!resolved.locations.is_empty(), "{resolved:?}");
    stop_in_strlen(&mut scenario, strlen.id).await;
    scenario.shutdown().await;
}

#[tokio::test]
async fn indirect_functions_break_where_the_loader_chose_at_startup() {
    indirect_function_pending_before_the_run("measure-gcc-nodebug").await;
}

#[tokio::test]
async fn indirect_functions_break_where_the_loader_chose_for_a_lazy_program() {
    indirect_function_pending_before_the_run("measure-clang-nopie-lazy").await;
}

#[tokio::test]
async fn indirect_functions_break_where_a_static_program_chose() {
    indirect_function_pending_before_the_run("measure-gcc-static").await;
}

/// Added once the program runs, a breakpoint on an indirect function finds
/// the implementation its resolver chose long before.
#[tokio::test]
async fn indirect_functions_added_later_break_where_their_resolver_chose() {
    let mut scenario = Scenario::launch("measure-gcc-static");
    let measure = pending_function(&scenario, "measure").await;
    run_to_breakpoint(&mut scenario, measure.id).await;
    let strlen = scenario
        .operation(
            "add breakpoint",
            scenario
                .handle()
                .add_breakpoint(BreakpointSpec::Function("strlen".to_owned())),
        )
        .await;
    assert_eq!(strlen.locations.len(), 1, "{strlen:?}");
    stop_in_strlen(&mut scenario, strlen.id).await;
    scenario.shutdown().await;
}

/// The vDSO's symbols name code the program runs, so function breakpoints
/// resolve there, as gdb's do: libc binds `time` straight to the vDSO's, and
/// libc's own `clock_gettime` calls the vDSO's.
#[tokio::test]
async fn function_breakpoints_resolve_in_the_vdso() {
    const FIXTURE: &str = "vdso-gcc-o0";
    let mut scenario = Scenario::launch(FIXTURE);
    let time = pending_function(&scenario, "time").await;
    let clock = pending_function(&scenario, "clock_gettime").await;
    let reason = scenario
        .run_with_to_stop(LaunchOptions {
            arguments: vec!["calls".into()],
            ..LaunchOptions::default()
        })
        .await;
    let modules = scenario
        .operation("modules", scenario.handle().loaded_modules())
        .await;
    let vdso = support::vdso_module(&modules).module.id;
    let libc = modules
        .modules
        .iter()
        .find(|record| {
            record
                .path
                .file_name()
                .is_some_and(|name| name.to_string_lossy().starts_with("libc.so"))
        })
        .expect("libc is loaded")
        .module
        .id;
    let libraries = |breakpoint: &Breakpoint| {
        breakpoint
            .locations
            .iter()
            .map(|location| location.library)
            .collect::<BTreeSet<_>>()
    };

    // libc's own `time` names its resolver, which never runs as `time`.
    let time = breakpoint(&mut scenario, time.id).await;
    assert_eq!(libraries(&time), BTreeSet::from([Some(vdso)]), "{time:#?}");
    let clock = breakpoint(&mut scenario, clock.id).await;
    assert_eq!(
        libraries(&clock),
        BTreeSet::from([Some(libc), Some(vdso)]),
        "{clock:#?}"
    );

    // Each stop is at the start of a function, in the module the
    // breakpoint's location belongs to, below the program's caller.
    let expected = [
        (time.id, vdso, "vdso_time"),
        (clock.id, libc, "vdso_clock"),
        (clock.id, vdso, "vdso_clock"),
    ];
    let mut reason = reason;
    for (index, (id, module, caller)) in expected.into_iter().enumerate() {
        assert!(
            matches!(&reason, StopReason::Breakpoint { hits, .. } if hits[0].breakpoint == id),
            "stop {index}: {reason:?}"
        );
        let trace = scenario
            .operation("backtrace", scenario.handle().backtrace())
            .await;
        let frames = frame_modules(&trace, &modules);
        assert_eq!(
            trace.frames[0].module,
            Some(module),
            "stop {index}: {frames:#?}"
        );
        let symbol = trace.frames[0].symbol.as_ref().expect("a symbol");
        assert_eq!(symbol.offset, 0, "stop {index}: {symbol:?}");
        assert!(
            position_of(&frames, FIXTURE, caller) <= 2,
            "stop {index}: {frames:#?}"
        );
        reason = scenario.resume_to_stop().await;
    }

    assert_eq!(reason, StopReason::Exited(ExitStatus::Code(0)));
    scenario.shutdown().await;
}

/// A process may move its vDSO, as a checkpoint restore does, or unmap it.
/// The vDSO's module is read again where it moved to, and goes with it. A
/// trap the debugger wrote moves with the code, so it is taken out where it
/// went: a function breakpoint follows its function, an address breakpoint
/// loses its location, and neither writes into memory that left.
#[tokio::test]
async fn breakpoints_and_the_vdso_module_follow_the_vdso_as_it_moves_and_goes() {
    let mut scenario = Scenario::launch("vdso-gcc-o0");
    let getcpu = pending_function(&scenario, "__vdso_getcpu").await;
    scenario.add_breakpoint("vdso_moved").await;
    scenario.add_breakpoint("vdso_unmapped").await;
    let mut events = scenario.handle().subscribe();
    let reason = scenario
        .run_with_to_stop(LaunchOptions {
            arguments: vec!["move".into()],
            ..LaunchOptions::default()
        })
        .await;
    let InferiorState::Stopped { process_id, .. } = scenario.snapshot().await.inferior else {
        panic!("the program is stopped");
    };

    let mut vdso_events = || {
        std::iter::from_fn(|| events.try_recv().ok())
            .filter_map(|event| match event {
                DebuggerEvent::ModuleLoaded { module, .. } => Some((true, module)),
                DebuggerEvent::ModuleUnloaded { module, .. } => Some((false, module)),
                _ => None,
            })
            .filter(|(_, module)| module.path.as_os_str() == support::VDSO)
            .map(|(loaded, module)| (loaded, module.module.load_bias.get()))
            .collect::<Vec<_>>()
    };

    let first = support::vdso_mapping(process_id).start;
    let offset = support::breakpoint_address(&reason).get() - first;
    assert_eq!(
        library_addresses(&breakpoint(&mut scenario, getcpu.id).await),
        [first + offset]
    );
    let code = read_byte(&scenario, first + offset).await;
    assert_ne!(code, 0xcc);

    // The address breakpoint shares the function breakpoint's site.
    let address = scenario
        .add_breakpoint_spec(BreakpointSpec::Address(VirtualAddress::new(first + offset)))
        .await;

    assert!(matches!(
        scenario.resume_to_stop().await,
        StopReason::Breakpoint { .. }
    ));
    let modules = scenario
        .operation("modules", scenario.handle().loaded_modules())
        .await;
    let moved = support::vdso_mapping(process_id).start;
    assert_eq!(support::vdso_module(&modules).module.load_bias.get(), moved);
    let target = scenario
        .operation("address", scenario.handle().variable("address"))
        .await;
    assert!(
        matches!(
            available_value(&target.state),
            microscope::VariableValue::Address(value) if value.address.get() == moved
        ),
        "{target:?}"
    );

    // The vDSO the program started with was registered, then replaced.
    let changes = vdso_events();
    assert_eq!(changes, [(true, first), (false, first), (true, moved)]);
    assert_ne!(first, moved);
    assert_eq!(
        library_addresses(&breakpoint(&mut scenario, getcpu.id).await),
        [moved + offset]
    );
    assert!(library_addresses(&breakpoint(&mut scenario, address.id).await).is_empty());
    assert_eq!(read_byte(&scenario, moved + offset).await, code);

    // The program calls getcpu where the vDSO moved to.
    let reason = scenario.resume_to_stop().await;
    assert_eq!(support::breakpoint_address(&reason).get(), moved + offset);
    let address = scenario
        .add_breakpoint_spec(BreakpointSpec::Address(VirtualAddress::new(moved + offset)))
        .await;

    assert!(matches!(
        scenario.resume_to_stop().await,
        StopReason::Breakpoint { .. }
    ));
    let modules = scenario
        .operation("modules", scenario.handle().loaded_modules())
        .await;
    assert!(
        modules
            .modules
            .iter()
            .all(|record| record.path.as_os_str() != support::VDSO),
        "{modules:#?}"
    );
    assert_eq!(vdso_events(), [(false, moved)]);
    for id in [getcpu.id, address.id] {
        assert!(library_addresses(&breakpoint(&mut scenario, id).await).is_empty());
    }

    scenario.remove_all_breakpoints().await;
    assert_eq!(
        scenario.resume_to_stop().await,
        StopReason::Exited(ExitStatus::Code(0))
    );
    scenario.shutdown().await;
}

/// A library's code moved with mremap(2) carries the debugger's trap with
/// it. The program runs the moved code before anything else stops it, as a
/// restored checkpoint does, and stops at the function breakpoint where the
/// code went, without the trap left in what it runs.
#[tokio::test]
async fn function_breakpoints_follow_library_code_as_it_moves() {
    let mut scenario = Scenario::launch("moved-code");
    let answer = pending_function(&scenario, "moved_answer").await;
    let reason = scenario.run_to_stop().await;
    let library = |modules: &LoadedModuleSnapshot| {
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
    let code = read_byte(&scenario, first + offset).await;
    assert_ne!(code, 0xcc);

    let reason = scenario.resume_to_stop().await;
    let modules = scenario
        .operation("modules", scenario.handle().loaded_modules())
        .await;
    let moved = library(&modules);
    assert_ne!(moved, first);
    assert_eq!(support::breakpoint_address(&reason).get(), moved + offset);
    assert_eq!(breakpoint(&mut scenario, answer.id).await.hit_count, 2);
    assert_eq!(
        library_addresses(&breakpoint(&mut scenario, answer.id).await),
        [moved + offset]
    );
    assert_eq!(read_byte(&scenario, moved + offset).await, code);

    scenario.remove_all_breakpoints().await;
    assert_eq!(
        scenario.resume_to_stop().await,
        StopReason::Exited(ExitStatus::Code(0))
    );
    scenario.shutdown().await;
}

/// Memory mapped over a breakpoint's site no longer holds its trap, so the
/// breakpoint loses the location, and deleting it leaves the new memory as
/// the program wrote it.
#[tokio::test]
async fn a_breakpoint_whose_memory_is_replaced_loses_its_location() {
    let mut scenario = Scenario::launch("vdso-gcc-o0");
    let getcpu = pending_function(&scenario, "__vdso_getcpu").await;
    scenario.add_breakpoint("vdso_replaced").await;
    let reason = scenario
        .run_with_to_stop(LaunchOptions {
            arguments: vec!["replace".into()],
            ..LaunchOptions::default()
        })
        .await;
    let site = support::breakpoint_address(&reason);
    let address = scenario
        .add_breakpoint_spec(BreakpointSpec::Address(site))
        .await;
    scenario.remove_breakpoint(getcpu.id).await;

    assert!(matches!(
        scenario.resume_to_stop().await,
        StopReason::Breakpoint { .. }
    ));
    assert!(
        breakpoint(&mut scenario, address.id)
            .await
            .locations
            .is_empty()
    );
    assert_eq!(read_byte(&scenario, site.get()).await, 0x90);
    scenario.remove_all_breakpoints().await;
    assert_eq!(
        scenario.resume_to_stop().await,
        StopReason::Exited(ExitStatus::Code(0))
    );
    scenario.shutdown().await;
}

/// The byte at `address` as the program sees it, without the debugger's
/// traps.
async fn read_byte(scenario: &Scenario, address: u64) -> u8 {
    let read = scenario
        .operation(
            "read memory",
            scenario
                .handle()
                .read_memory(VirtualAddress::new(address), 1),
        )
        .await;
    read.bytes[0]
}
