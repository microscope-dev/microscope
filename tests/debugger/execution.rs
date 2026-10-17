//! Breakpoints, threads, signals, memory, and session lifecycle.

use super::*;

#[tokio::test]
async fn gcc_o2_entry_policy_does_not_execute_a_real_first_statement() {
    let fixture = "stepping-boundaries-gcc-o2";
    let mut scenario = Scenario::new("GCC O2 zero-length prologue", Scenario::fixture(fixture));
    let raw_entry = {
        let image = scenario.handle().module_image();
        let function = image.function_named("no_prologue").expect("no_prologue");
        image
            .instances_for_function(function.id())
            .find(|instance| matches!(instance.kind(), CodeInstanceKind::OutOfLine))
            .expect("physical no_prologue")
            .ranges()
            .next()
            .unwrap()
            .start
    };

    let breakpoint = scenario.add_breakpoint("no_prologue").await;
    assert_eq!(
        single_image_breakpoint_address(&breakpoint),
        raw_entry,
        "the conservative GCC fallback skipped the first store"
    );

    assert!(matches!(
        scenario.run_to_stop().await,
        StopReason::Breakpoint { .. }
    ));
    let location = scenario
        .operation(
            "zero-prologue location",
            scenario.handle().current_location(),
        )
        .await;
    assert_eq!(location.image.address, raw_entry);
    let sink = scenario
        .handle()
        .module_image()
        .symbol_named("boundary_sink")
        .expect("boundary_sink symbol")
        .address();
    let sink = relocate_image_address(sink, &location);
    let word = scenario
        .operation(
            "boundary sink before first instruction",
            scenario.handle().read_word(sink),
        )
        .await;
    assert_eq!(
        u32::try_from(word).expect("boundary sink value fits u32"),
        22,
        "no_prologue's first store executed before its entry stop"
    );
    scenario.shutdown().await;
}

#[tokio::test]
async fn source_line_and_file_qualified_function_breakpoints_stop_where_they_name() {
    let mut scenario = Scenario::launch("basic");
    let line = scenario.add_source_breakpoint("basic.c", 11).await;
    assert_eq!(line.locations.len(), 1);
    scenario
        .add_file_function_breakpoint("tests/fixtures/c/basic.c", "breakpoint_target")
        .await;
    for expected in [6, 11] {
        assert!(matches!(
            scenario.resume_or_run().await,
            StopReason::Breakpoint { .. }
        ));
        let context = scenario
            .operation("source context", scenario.handle().source_context(0))
            .await;
        assert_eq!(context.line.get(), expected);
        assert!(context.recorded.ends_with("tests/fixtures/c/basic.c"));
    }

    scenario.shutdown().await;
}

#[tokio::test]
async fn breakpoint_edits_publish_one_revision_or_change_nothing() {
    let mut scenario = Scenario::launch("basic");
    let first = scenario.add_breakpoint("main").await;
    let second = scenario.add_breakpoint("breakpoint_target").await;
    let revision = scenario.snapshot().await.revision;
    assert_eq!(scenario.remove_all_breakpoints().await, vec![first, second]);
    let before = scenario.snapshot().await;
    assert_eq!(before.revision, revision + 1);
    assert!(before.breakpoints.is_empty());

    // Edits that fail change nothing.
    assert!(matches!(
        scenario
            .handle()
            .remove_breakpoint(microscope::BreakpointId::new(999))
            .await,
        Err(Error::BreakpointNotFound(999))
    ));
    let source = |path: &str, line| BreakpointSpec::Source {
        path: path.into(),
        line: microscope::LineNumber::new(line).unwrap(),
    };

    assert!(matches!(
        scenario
            .handle()
            .add_breakpoint(source("missing.c", 1))
            .await,
        Err(Error::SourceFileNotFound(_))
    ));
    assert!(matches!(
        scenario
            .handle()
            .add_breakpoint(source("basic.c", 999))
            .await,
        Err(Error::SourceLineUnavailable { .. })
    ));
    let after = scenario.snapshot().await;
    assert_eq!(after.revision, before.revision);
    assert_eq!(after.breakpoints, before.breakpoints);

    // Deleting one of two breakpoints at a site keeps the other, and
    // deleting the one a thread stopped at lets it run the instruction.
    let function = scenario.add_breakpoint("breakpoint_target").await;
    let source = scenario.add_source_breakpoint("basic.c", 6).await;
    assert_eq!(function.locations[0].location, source.locations[0].location);
    let revision = scenario.snapshot().await.revision;
    assert_eq!(scenario.remove_breakpoint(function.id).await, function);
    let snapshot = scenario.snapshot().await;
    assert_eq!(snapshot.revision, revision + 1);
    assert_eq!(snapshot.breakpoints.as_ref(), std::slice::from_ref(&source));
    assert!(matches!(
        scenario.run_to_stop().await,
        StopReason::Breakpoint { .. }
    ));
    scenario.remove_breakpoint(source.id).await;
    assert_eq!(
        scenario.resume_to_stop().await,
        StopReason::Exited(ExitStatus::Code(0))
    );
    scenario.shutdown().await;
}

#[tokio::test]
#[expect(
    clippy::too_many_lines,
    reason = "one stop checks its snapshot, registers, breakpoints, and memory together"
)]
async fn breakpoint_memory_and_event_state_follow_one_consistent_scenario() {
    let mut scenario = Scenario::new("breakpoint lifecycle", Scenario::fixture("basic"));

    assert!(matches!(
        scenario.handle().registers().await,
        Err(Error::NotRunning)
    ));

    let mut breakpoint = scenario.add_breakpoint("breakpoint_target").await;
    let first = scenario.run_to_stop().await;

    let first_address = match first {
        StopReason::Breakpoint {
            address: Some(address),
            ..
        } => address,
        other => panic!("expected breakpoint, got {other:?}"),
    };

    let location = scenario
        .operation("current location", scenario.handle().current_location())
        .await;

    assert_eq!(location.address, first_address);
    assert_eq!(location_function(&location), Some("breakpoint_target"));

    let source = location
        .image
        .source
        .as_ref()
        .expect("source location")
        .clone();
    let source_file = scenario
        .handle()
        .module_image()
        .source_file(source.file)
        .expect("source file")
        .clone();

    assert!(source_file.path.ends_with("basic.c"));
    assert!(source_file.path.is_absolute());
    assert!(source.line.get() > 0);

    let context = scenario
        .operation("source context", scenario.handle().source_context(3))
        .await;

    assert_basic_source_context(&context, &source_file, &source);

    let image_address = single_image_breakpoint_address(&breakpoint);

    assert_ne!(
        first_address.get(),
        image_address.get(),
        "PIE was not relocated"
    );

    let snapshot = scenario.snapshot().await;

    assert_eq!(snapshot.revision, scenario.last_revision());
    assert!(matches!(
        &snapshot.inferior,
        InferiorState::Stopped { reason, .. } if *reason == first
    ));
    breakpoint.hit_count = 1;
    assert_eq!(
        snapshot.breakpoints.as_ref(),
        std::slice::from_ref(&breakpoint)
    );

    let registers = scenario
        .operation("read registers", scenario.handle().registers())
        .await;

    assert_register_snapshot(&registers, &snapshot, first_address);

    let duplicate = scenario.add_breakpoint("breakpoint_target").await;

    assert_eq!(duplicate, breakpoint);
    assert_eq!(
        scenario.snapshot().await.breakpoints.as_ref(),
        std::slice::from_ref(&breakpoint)
    );

    let main_breakpoint = scenario.add_breakpoint("main").await;

    assert_eq!(
        scenario.snapshot().await.breakpoints.as_ref(),
        &[breakpoint.clone(), main_breakpoint]
    );

    let value_address = scenario
        .operation(
            "resolve microscope_value",
            scenario.handle().runtime_address("microscope_value"),
        )
        .await;

    assert_eq!(
        scenario
            .operation(
                "read microscope_value",
                scenario.handle().read_word(value_address)
            )
            .await,
        0x1122_3344_5566_7788
    );

    // Reads are bounded, belong to the stop, and hide breakpoint traps.
    let value_bytes = scenario
        .operation(
            "read microscope_value bytes",
            scenario.handle().read_memory(value_address, 8),
        )
        .await;
    assert_eq!(
        value_bytes.bytes.as_ref(),
        0x1122_3344_5566_7788_u64.to_le_bytes()
    );
    assert_eq!(
        value_bytes.completion,
        microscope::MemoryReadCompletion::Complete
    );
    assert_eq!(value_bytes.revision, scenario.last_revision());
    assert_eq!(Some(value_bytes.stop_id), snapshot.stop_id);
    assert_eq!(
        value_bytes.target,
        scenario.handle().module_image().target()
    );
    let breakpoint_bytes = scenario
        .operation(
            "read logical breakpoint bytes",
            scenario.handle().read_memory(first_address, 1),
        )
        .await;
    assert_eq!(
        breakpoint_bytes.completion,
        microscope::MemoryReadCompletion::Complete
    );
    assert_ne!(breakpoint_bytes.bytes.as_ref(), [0xcc]);
    let empty = scenario
        .operation(
            "read an empty memory range",
            scenario.handle().read_memory(value_address, 0),
        )
        .await;
    assert!(empty.bytes.is_empty());
    assert_eq!(empty.completion, microscope::MemoryReadCompletion::Complete);
    assert!(matches!(
        scenario.handle().read_memory(value_address, 65_537).await,
        Err(Error::MemoryReadTooLarge {
            requested: 65_537,
            maximum: 65_536,
        })
    ));
    assert!(matches!(
        scenario
            .handle()
            .read_memory(VirtualAddress::new(u64::MAX), 1)
            .await,
        Err(Error::AddressOverflow)
    ));

    assert_eq!(
        support::breakpoint_address(&scenario.resume_to_stop().await),
        first_address
    );
    assert_eq!(
        scenario.resume_to_stop().await,
        StopReason::Exited(ExitStatus::Code(0))
    );

    scenario.shutdown().await;
}

#[tokio::test]
async fn shutdown_kills_and_reaps_a_running_inferior() {
    let mut running = Scenario::new("shutdown running", Scenario::fixture("spin"));
    assert!(matches!(
        running.handle().variables().await,
        Err(Error::NotRunning)
    ));
    let run = running.start_running().await;
    assert!(matches!(
        running.snapshot().await.inferior,
        InferiorState::Running { .. }
    ));
    assert!(matches!(
        running.handle().registers().await,
        Err(Error::NotStopped)
    ));
    assert!(matches!(
        running.handle().variables().await,
        Err(Error::NotStopped)
    ));

    let status = running.shutdown().await.expect("inferior exit event");

    assert!(matches!(
        status,
        ExitStatus::Terminated(exception) if exception.code == 9
    ));
    assert!(matches!(
        run.await.expect("run task"),
        Ok(StopReason::Exited(ExitStatus::Terminated(exception))) if exception.code == 9
    ));
}

#[tokio::test]
async fn stops_in_a_shared_library_show_that_librarys_own_source() {
    let mut scenario = Scenario::new("library stop", Scenario::fixture("module-frames-gcc-o0"));
    scenario.add_breakpoint("main").await;
    scenario.run_to_stop().await;
    let modules = scenario
        .operation("modules", scenario.handle().loaded_modules())
        .await;
    let library = modules
        .modules
        .iter()
        .find(|record| record.path.ends_with("libmodule-frames.so"))
        .expect("library is loaded");
    let image = scenario
        .operation(
            "library image",
            scenario.handle().loaded_module_image(library.module.id),
        )
        .await;
    let function = image.function_named("dso_apply").expect("dso_apply");
    let entry = image
        .instances_for_function(function.id())
        .find_map(microscope::CodeInstance::breakpoint_entry)
        .expect("dso_apply entry");
    let address = library
        .module
        .virtual_address(entry.address)
        .expect("relocated entry");
    scenario
        .add_breakpoint_spec(BreakpointSpec::Address(address))
        .await;
    assert!(matches!(
        scenario.resume_to_stop().await,
        StopReason::Breakpoint { .. }
    ));

    // Source-file identifiers belong to the library's image; resolving them
    // in the main image would name main.c, or no file at all.
    let location = scenario
        .operation("location", scenario.handle().current_location())
        .await;
    assert_eq!(location.module, library.module.id);
    assert_eq!(
        location.image.function.map(|function| function.name),
        Some(Arc::from("dso_apply"))
    );
    let context = scenario
        .operation("source", scenario.handle().source_context(1))
        .await;
    assert!(
        context
            .recorded
            .ends_with("tests/fixtures/c/module-frames/library.c"),
        "{context:#?}"
    );
    assert!(
        context
            .lines
            .iter()
            .any(|line| line.text.contains("callback(adjusted)")),
        "{context:#?}"
    );
    scenario.shutdown().await;
}

#[tokio::test]
async fn inline_function_breakpoints_resolve_every_concrete_instance() {
    for fixture in [
        "inline-gcc-o1",
        "inline-gcc-o2",
        "inline-clang-o1",
        "inline-clang-o2",
    ] {
        let mut scenario = Scenario::launch(fixture);
        let expected_instances = {
            let image = scenario.handle().module_image();
            let function = image.function_named("leaf").expect("leaf function");

            image
                .instances_for_function(function.id())
                .map(microscope::CodeInstance::id)
                .collect::<BTreeSet<_>>()
        };

        let breakpoint = scenario.add_breakpoint("leaf").await;
        let resolved_instances = breakpoint
            .locations
            .iter()
            .flat_map(|location| location.code_instances.iter().copied())
            .collect::<BTreeSet<_>>();
        let resolved_addresses = breakpoint
            .locations
            .iter()
            .map(|location| location.location.clone())
            .collect::<BTreeSet<_>>();

        assert_eq!(resolved_instances, expected_instances, "{fixture}");
        assert_eq!(
            resolved_addresses.len(),
            breakpoint.locations.len(),
            "{fixture} retained duplicate physical sites"
        );
        assert!(
            breakpoint
                .locations
                .iter()
                .all(|location| matches!(location.location, BreakpointLocation::Image(_))),
            "{fixture} function breakpoint was not image-relative"
        );

        let duplicate = scenario.add_breakpoint("leaf").await;
        assert_eq!(duplicate, breakpoint, "{fixture}");
        assert_eq!(
            scenario.snapshot().await.breakpoints.as_ref(),
            &[breakpoint],
            "{fixture} duplicated one logical breakpoint"
        );

        scenario.shutdown().await;
    }
}

#[tokio::test]
async fn inline_breakpoint_hits_select_the_matching_concrete_instance() {
    for fixture in ["inline-gcc-o2", "inline-clang-o2"] {
        let mut scenario = Scenario::launch(fixture);
        let breakpoint = scenario.add_breakpoint("leaf").await;
        let mut reason = scenario.run_to_stop().await;
        let mut hits = 0;
        let mut hit_instances = BTreeSet::new();

        while let StopReason::Breakpoint { .. } = reason {
            hits += 1;
            let location = scenario
                .operation(
                    "inline breakpoint location",
                    scenario.handle().current_location(),
                )
                .await;
            let snapshot = scenario.snapshot().await;
            let microscope::PresentedFrame::Inline(selected) = snapshot
                .presentation
                .as_ref()
                .expect("stopped presentation")
                .frame
            else {
                panic!("{fixture} did not select an inline frame: {snapshot:?}")
            };

            let resolved = breakpoint
                .locations
                .iter()
                .find(|resolved| {
                    resolved.location == BreakpointLocation::Image(location.image.address)
                })
                .expect("hit one resolved breakpoint location");

            assert!(resolved.code_instances.contains(&selected), "{fixture}");
            hit_instances.insert(selected);
            assert_eq!(location_function(&location), Some("leaf"), "{fixture}");

            reason = scenario.resume_to_stop().await;
        }

        assert_eq!(reason, StopReason::Exited(ExitStatus::Code(0)), "{fixture}");
        assert_eq!(
            hits, 5,
            "{fixture} executed an unexpected set of leaf calls"
        );
        let same_line_instances = {
            let image = scenario.handle().module_image();
            hit_instances
                .iter()
                .filter(|instance| {
                    image.code_instance(**instance).is_some_and(|instance| {
                        matches!(
                            &instance.kind(),
                            CodeInstanceKind::Inline {
                                call_site: Some(call_site)
                            } if call_site.line.get() == 30
                        )
                    })
                })
                .count()
        };

        assert_eq!(same_line_instances, 2, "{fixture}");
        scenario.shutdown().await;
    }
}

#[tokio::test]
async fn repeated_debug_sessions_leave_no_inferiors_behind() {
    for iteration in 0..8 {
        let mut scenario = Scenario::new(
            format!("repeated session {iteration}"),
            Scenario::fixture("basic"),
        );

        scenario.add_breakpoint("breakpoint_target").await;

        assert!(matches!(
            scenario.run_to_stop().await,
            StopReason::Breakpoint { .. }
        ));

        scenario.shutdown().await;
    }
}

/// A process the debugger's own process started outside the session, as a
/// shell starts the reader of a `2> >(…)` redirection before it runs the
/// debugger, outlives the inferior without holding up its end.
#[tokio::test]
async fn a_child_from_outside_the_session_never_delays_its_end() {
    let _outsider = support::ExternalProcess::spawn(&Scenario::fixture("attach"));
    let mut scenario = Scenario::new("outsider", Scenario::fixture("basic"));
    assert!(matches!(
        scenario.run_to_stop().await,
        StopReason::Exited(ExitStatus::Code(0))
    ));
    scenario.shutdown().await;

    let mut stopped = Scenario::new("outsider at a stop", Scenario::fixture("basic"));
    stopped.add_breakpoint("breakpoint_target").await;
    assert!(matches!(
        stopped.run_to_stop().await,
        StopReason::Breakpoint { .. }
    ));
    stopped.shutdown().await;
}

#[tokio::test]
async fn linux_wait_ownership_allows_only_one_session_per_host_process() {
    let fixture = Scenario::fixture("basic");
    let first = Debugger::new(&fixture).expect("initialize first debugger");
    assert!(matches!(Debugger::new(&fixture), Err(Error::Backend(_))));

    first.shutdown().await.expect("shut down first debugger");

    let replacement = Debugger::new(&fixture).expect("initialize replacement debugger");
    replacement
        .shutdown()
        .await
        .expect("shut down replacement debugger");
}

#[tokio::test]
async fn pthread_breakpoint_establishes_a_coherent_all_stop_snapshot() {
    let mut scenario = Scenario::new("pthread all-stop", Scenario::fixture("threads"));
    let breakpoint = scenario.add_breakpoint("worker_breakpoint").await;

    assert!(matches!(
        scenario.run_to_stop().await,
        StopReason::Breakpoint { .. }
    ));

    let snapshot = scenario.snapshot().await;
    let (process_id, stop_id, selected) = match snapshot.inferior {
        InferiorState::Stopped {
            process_id,
            stop_id,
            thread_id,
            ..
        } => (process_id, stop_id, thread_id),
        other => panic!("expected all-stop snapshot, got {other:?}"),
    };

    assert_ne!(
        selected.get(),
        process_id.get(),
        "worker thread was selected"
    );
    assert_eq!(snapshot.threads.len(), 3);
    assert!(
        snapshot
            .threads
            .iter()
            .all(|thread| { matches!(thread.state, ThreadState::Stopped { .. }) })
    );

    for thread in snapshot.threads.iter() {
        scenario
            .operation(
                "select stopped thread",
                scenario.handle().select_context(thread.id),
            )
            .await;
        let registers = scenario
            .operation("inspect stopped thread", scenario.handle().registers())
            .await;
        let backtrace = scenario
            .operation("unwind stopped thread", scenario.handle().backtrace())
            .await;
        assert_eq!(registers.context, thread.id.into());
        assert_eq!(backtrace.context, thread.id.into());
        assert!(!backtrace.frames.is_empty());
    }

    scenario
        .operation(
            "restore selected worker",
            scenario.handle().select_context(selected),
        )
        .await;

    // Unknown and unrepresentable thread IDs fail without disturbing the stop.
    for unknown in [0, 1 << 40, u64::MAX].map(microscope::ThreadId::new) {
        assert!(matches!(
            scenario.handle().select_context(unknown).await,
            Err(Error::UnknownThread(thread)) if thread == unknown
        ));
        assert!(matches!(
            scenario
                .handle()
                .continue_execution(
                    stop_id,
                    microscope::ResumeScope::Thread(unknown),
                    microscope::ExceptionDisposition::Pass,
                )
                .await,
            Err(Error::UnknownThread(thread)) if thread == unknown
        ));
    }

    assert_eq!(scenario.snapshot().await.stop_id, Some(stop_id));

    finish_worker_hits(&mut scenario, breakpoint.id).await;
    scenario.shutdown().await;
}

/// Resumes the `threads` fixture until it exits, after which each of its two
/// workers has hit `breakpoint` once. The second hit stops on its own unless it was
/// reported with the first.
async fn finish_worker_hits(scenario: &mut Scenario, breakpoint: microscope::BreakpointId) {
    let mut reason = scenario.resume_to_stop().await;
    if let StopReason::Breakpoint { hits, .. } = &reason {
        assert_eq!(
            **hits,
            [microscope::BreakpointHit {
                breakpoint,
                hit_count: 2
            }]
        );
        reason = scenario.resume_to_stop().await;
    }

    assert_eq!(reason, StopReason::Exited(ExitStatus::Code(0)));
    let hits = scenario
        .snapshot()
        .await
        .breakpoints
        .iter()
        .find(|candidate| candidate.id == breakpoint)
        .expect("worker breakpoint")
        .hit_count;
    assert_eq!(hits, 2, "each worker hit its breakpoint once");
}

#[tokio::test]
async fn repeated_thread_creation_and_exit_loses_no_breakpoint_events() {
    let mut scenario = Scenario::new("thread registry churn", Scenario::fixture("thread-stress"));
    scenario.add_breakpoint("churn_breakpoint").await;
    let mut lagging = scenario.handle().subscribe();

    assert!(matches!(
        scenario.run_to_stop().await,
        StopReason::Breakpoint { .. }
    ));
    for iteration in 1..64 {
        assert!(
            matches!(
                scenario.resume_to_stop().await,
                StopReason::Breakpoint { .. }
            ),
            "missing breakpoint for iteration {iteration}"
        );
    }

    assert_eq!(
        scenario.resume_to_stop().await,
        StopReason::Exited(ExitStatus::Code(0))
    );
    assert!(matches!(
        lagging.recv().await,
        Err(tokio::sync::broadcast::error::RecvError::Lagged(_))
    ));
    assert!(matches!(
        scenario.snapshot().await.inferior,
        InferiorState::NotRunning
    ));

    scenario.shutdown().await;
}

#[tokio::test]
async fn a_thread_scoped_continue_stops_cleanly_when_its_thread_exits() {
    let mut scenario = Scenario::new("thread scoped exit", Scenario::fixture("threads"));
    let breakpoint = scenario.add_breakpoint("worker_breakpoint").await;
    scenario.run_to_stop().await;

    let snapshot = scenario.snapshot().await;
    let (process, stop, thread) = match snapshot.inferior {
        InferiorState::Stopped {
            process_id,
            stop_id,
            thread_id,
            ..
        } => (process_id, stop_id, thread_id),
        other => panic!("expected stopped inferior, got {other:?}"),
    };

    let mut events = scenario.handle().subscribe();
    let execution = scenario
        .operation(
            "continue one worker",
            scenario.handle().continue_execution(
                stop,
                microscope::ResumeScope::Thread(thread),
                microscope::ExceptionDisposition::Pass,
            ),
        )
        .await;
    let reason = timeout(Duration::from_secs(2), async {
        loop {
            if let microscope::DebuggerEvent::InferiorStopped {
                execution_id: Some(event_execution),
                reason,
                ..
            } = events.recv().await.expect("event stream closed")
                && event_execution == execution
            {
                break reason;
            }
        }
    })
    .await
    .expect("thread exit stop timed out");
    assert!(matches!(
        reason,
        StopReason::ThreadExited {
            thread_id,
            status: ExitStatus::Code(0),
        } if thread_id == thread
    ));
    let stopped = scenario.snapshot().await;
    assert!(matches!(
        stopped.inferior,
        InferiorState::Stopped {
            process_id,
            ..
        } if process_id == process
    ));

    finish_worker_hits(&mut scenario, breakpoint.id).await;
    scenario.shutdown().await;
}

#[tokio::test]
async fn signal_delivery_is_preserved_and_user_sigtrap_is_not_a_breakpoint() {
    let mut scenario = Scenario::new("signal pass", Scenario::fixture("signals"));
    scenario.add_breakpoint("signal_point").await;
    scenario.run_to_stop().await;

    assert!(matches!(
        scenario.resume_to_stop().await,
        StopReason::Exception(exception) if exception.code == 10
    ));
    assert!(matches!(
        scenario.resume_to_stop().await,
        StopReason::Exception(exception) if exception.code == 5
    ));
    assert_eq!(
        scenario.resume_to_stop().await,
        StopReason::Exited(ExitStatus::Code(0))
    );
    scenario.shutdown().await;

    let mut suppressed = Scenario::new("signal suppress", Scenario::fixture("signals"));
    suppressed.add_breakpoint("signal_point").await;
    suppressed.run_to_stop().await;
    assert!(matches!(
        suppressed.resume_to_stop().await,
        StopReason::Exception(exception) if exception.code == 10
    ));
    assert!(matches!(
        suppressed
            .resume_with_exception(microscope::ExceptionDisposition::Suppress)
            .await,
        StopReason::Exception(exception) if exception.code == 5
    ));
    assert_eq!(
        suppressed.resume_to_stop().await,
        StopReason::Exited(ExitStatus::Code(42))
    );
    suppressed.shutdown().await;
}

#[tokio::test]
async fn synchronous_faults_are_retained_and_delivered() {
    let mut scenario = Scenario::new("fatal signal", Scenario::fixture("fatal-signal"));
    scenario.add_breakpoint("fault").await;
    scenario.run_to_stop().await;

    assert!(matches!(
        scenario.resume_to_stop().await,
        StopReason::Exception(exception) if exception.code == 11
    ));
    assert!(matches!(
        scenario.resume_to_stop().await,
        StopReason::Exited(ExitStatus::Terminated(exception)) if exception.code == 11
    ));

    scenario.shutdown().await;
}

#[tokio::test]
async fn forked_children_are_released_without_inherited_breakpoints() {
    let mut scenario = Scenario::launch("fork");
    scenario.add_breakpoint("shared_work").await;
    assert!(matches!(
        scenario.run_to_stop().await,
        StopReason::Breakpoint { .. }
    ));

    // The child also runs shared_work; an inherited trap would kill it with
    // SIGTRAP and the parent would exit 5. Its SIGCHLD does not stop the
    // parent.
    assert_eq!(
        scenario.resume_to_stop().await,
        StopReason::Exited(ExitStatus::Code(0))
    );
    scenario.shutdown().await;
}

#[tokio::test]
async fn stops_survive_executable_mappings_that_name_no_file() {
    let mut scenario = Scenario::new("memfd mapping", Scenario::fixture("memfd-exec"));
    scenario.add_breakpoint("after_mapping").await;
    assert!(matches!(
        scenario.run_to_stop().await,
        StopReason::Breakpoint { .. }
    ));
    assert_eq!(
        scenario.resume_to_stop().await,
        StopReason::Exited(ExitStatus::Code(0))
    );
    scenario.shutdown().await;
}

#[tokio::test]
async fn job_control_stops_are_classified_without_inventing_a_pending_signal() {
    let mut scenario = Scenario::new("job control", Scenario::fixture("job-control"));

    assert!(matches!(
        scenario.run_to_stop().await,
        StopReason::Exception(exception) if exception.code == 19
    ));
    assert!(matches!(
        scenario.resume_to_stop().await,
        StopReason::Exception(exception) if exception.code == 19
    ));

    let process = match scenario.snapshot().await.inferior {
        InferiorState::Stopped { process_id, .. } => process_id,
        other => panic!("expected stopped inferior, got {other:?}"),
    };

    kill(
        Pid::from_raw(i32::try_from(process.get()).expect("process ID fits i32")),
        Signal::SIGCONT,
    )
    .expect("continue stopped process group");

    assert!(matches!(
        scenario.resume_to_stop().await,
        StopReason::Exception(exception) if exception.code == 18
    ));
    assert_eq!(
        scenario.resume_to_stop().await,
        StopReason::Exited(ExitStatus::Code(0))
    );

    scenario.shutdown().await;
}

#[tokio::test]
async fn nonleader_exec_rewrites_the_thread_registry_and_invalidates_the_image() {
    let mut scenario = Scenario::new("nonleader exec", Scenario::fixture("thread-exec"));

    assert_eq!(
        scenario.run_to_stop().await,
        StopReason::Exec { followed: false }
    );
    let snapshot = scenario.snapshot().await;
    assert_eq!(snapshot.threads.len(), 1);
    assert!(matches!(
        scenario.handle().resume().await,
        Err(Error::Backend(_))
    ));

    // The catalog describes the program from before the exec, so every
    // view through it refuses.
    assert!(matches!(
        scenario.handle().variables().await,
        Err(Error::Backend(_))
    ));
    assert!(matches!(
        scenario.handle().backtrace().await,
        Err(Error::Backend(_))
    ));
    assert!(matches!(
        scenario.handle().current_location().await,
        Err(Error::Backend(_))
    ));
    assert!(matches!(
        scenario.snapshot().await.presentation,
        Some(microscope::FramePresentation {
            frame: microscope::PresentedFrame::Physical,
            ..
        })
    ));

    scenario.shutdown().await;
}

#[tokio::test]
async fn stale_stop_tokens_allow_exactly_one_client_to_resume() {
    let mut scenario = Scenario::new("competing clients", Scenario::fixture("basic"));
    scenario.add_breakpoint("breakpoint_target").await;
    scenario.run_to_stop().await;

    let snapshot = scenario.snapshot().await;
    let (process, stop) = match snapshot.inferior {
        InferiorState::Stopped {
            process_id,
            stop_id,
            ..
        } => (process_id, stop_id),
        other => panic!("expected stopped inferior, got {other:?}"),
    };

    let first = scenario.handle().clone();
    let second = scenario.handle().clone();
    let (first, second) = tokio::join!(
        first.continue_execution(
            stop,
            microscope::ResumeScope::Process(process),
            microscope::ExceptionDisposition::Pass,
        ),
        second.continue_execution(
            stop,
            microscope::ResumeScope::Process(process),
            microscope::ExceptionDisposition::Pass,
        ),
    );

    let accepted = usize::from(first.is_ok()) + usize::from(second.is_ok());
    assert_eq!(accepted, 1, "exactly one client must control a stop");
    let rejected = if first.is_err() { first } else { second };
    assert!(
        matches!(rejected, Err(Error::NotStopped | Error::StaleStop)),
        "unexpected competing resume result: {rejected:?}"
    );

    scenario.shutdown().await;
}

#[tokio::test]
async fn launch_options_start_the_program_with_its_arguments_environment_directory_and_streams() {
    use std::io::Read as _;
    use std::io::Write as _;
    use std::process::Stdio;

    let directory = support::ScratchDir::new("launch-directory");
    let (stdin_reader, mut stdin_writer) = std::io::pipe().expect("stdin pipe");
    let (mut stdout_reader, stdout_writer) = std::io::pipe().expect("stdout pipe");
    let (mut stderr_reader, stderr_writer) = std::io::pipe().expect("stderr pipe");
    stdin_writer
        .write_all(b"typed line\n")
        .expect("write stdin");
    drop(stdin_writer);

    let mut scenario = Scenario::launch("process-environment");
    let reason = scenario
        .run_with_to_stop(LaunchOptions {
            arguments: vec!["first".into(), "second word".into()],
            environment: vec![
                (
                    "MICROSCOPE_FIXTURE_VALUE".into(),
                    Some("from launch".into()),
                ),
                ("MICROSCOPE_FIXTURE_REMOVED".into(), Some("set".into())),
                ("MICROSCOPE_FIXTURE_REMOVED".into(), None),
            ],
            working_directory: Some(directory.path().to_owned()),
            stdin: Some(Stdio::from(stdin_reader)),
            stdout: Some(Stdio::from(stdout_writer)),
            stderr: Some(Stdio::from(stderr_writer)),
            stop_at_entry: false,
        })
        .await;

    // argc counts the program name and both arguments.
    assert_eq!(reason, StopReason::Exited(ExitStatus::Code(3)));
    scenario.shutdown().await;

    let mut stdout = String::new();
    stdout_reader
        .read_to_string(&mut stdout)
        .expect("read inferior stdout");
    let mut stderr = String::new();
    stderr_reader
        .read_to_string(&mut stderr)
        .expect("read inferior stderr");
    let directory = directory
        .path()
        .canonicalize()
        .expect("canonical scratch directory");
    assert_eq!(
        stdout,
        format!(
            "argument 1: first\nargument 2: second word\nvalue: from launch\nremoved: absent\ndirectory: {}\n",
            directory.display()
        )
    );
    assert_eq!(stderr, "input: typed line\n");
}

#[tokio::test]
async fn launched_programs_run_at_the_same_addresses_every_time() {
    // As under gdb, a rerun shows the same pointers and stack contents even
    // when the debugger itself runs randomized.
    support::randomize_addresses();
    let mut layouts = Vec::new();
    for _ in 0..2 {
        let mut scenario = Scenario::launch("basic");
        scenario.add_breakpoint("main").await;
        assert!(matches!(
            scenario.run_to_stop().await,
            StopReason::Breakpoint { .. }
        ));
        let InferiorState::Stopped { process_id, .. } = scenario.snapshot().await.inferior else {
            panic!("the program stopped in main");
        };

        layouts.push(
            fs::read_to_string(format!("/proc/{}/maps", process_id.get()))
                .expect("read the program's mappings"),
        );
        scenario.shutdown().await;
    }

    assert_eq!(layouts[0], layouts[1]);
}

#[tokio::test]
async fn stop_at_entry_ends_the_launch_before_the_loader_runs() {
    let mut scenario = Scenario::launch("basic");
    scenario.add_breakpoint("main").await;
    let reason = scenario
        .run_with_to_stop(LaunchOptions {
            stop_at_entry: true,
            ..LaunchOptions::default()
        })
        .await;
    assert_eq!(reason, StopReason::Entry);

    // The first instruction belongs to the dynamic loader, which has not
    // mapped the program's libraries yet.
    let registers = scenario
        .operation("entry registers", scenario.handle().registers())
        .await;
    let pc = register_u64(&registers, RegisterRole::ProgramCounter);
    let description = scenario
        .operation(
            "describe the entry instruction",
            scenario.handle().describe_address(VirtualAddress::new(pc)),
        )
        .await;
    let module = description
        .module
        .as_ref()
        .expect("the entry is in a loaded module");
    assert!(
        module.path.to_string_lossy().contains("ld-linux"),
        "the entry stop is not in the dynamic loader: {description:?}"
    );
    let modules = scenario
        .operation("entry modules", scenario.handle().loaded_modules())
        .await;
    assert!(
        !modules
            .modules
            .iter()
            .any(|module| module.path.to_string_lossy().contains("libc.so")),
        "the C library is mapped before the loader ran: {modules:?}"
    );

    // Execution from the entry stop reaches user breakpoints as usual.
    assert!(matches!(
        scenario.resume_to_stop().await,
        StopReason::Breakpoint { .. }
    ));
    assert_eq!(
        scenario
            .operation("main location", scenario.handle().current_location())
            .await
            .image
            .function
            .map(|function| function.name.to_string()),
        Some("main".to_owned())
    );
    scenario.shutdown().await;
}

#[tokio::test]
async fn pause_during_launch_ends_the_launch_execution_in_a_coherent_stop() {
    let mut scenario = Scenario::new("pause during launch", Scenario::fixture("spin"));
    let run = scenario.start_launching().await;

    // The initial exec stop may be processed before or after this request;
    // either order must end the launch execution in one inspectable pause.
    let reason = timeout(Duration::from_secs(2), scenario.handle().pause())
        .await
        .expect("pause timed out")
        .expect("pause failed");
    assert_eq!(reason, StopReason::Pause);
    let launched = timeout(Duration::from_secs(2), run)
        .await
        .expect("run timed out")
        .expect("run task panicked")
        .expect("run failed");
    assert_eq!(launched, StopReason::Pause);
    assert!(matches!(
        scenario.snapshot().await.inferior,
        InferiorState::Stopped {
            reason: StopReason::Pause,
            ..
        }
    ));
    scenario
        .operation("read paused registers", scenario.handle().registers())
        .await;

    scenario.shutdown().await;
}

#[tokio::test]
async fn pause_stops_a_process_whose_main_thread_exited() {
    let mut scenario = Scenario::launch("exited-leader");
    let _run = scenario.start_running().await;
    let InferiorState::Running { process_id, .. } = scenario.snapshot().await.inferior else {
        panic!("the inferior is not running");
    };

    // Linux reports the main thread's exit only after the worker's, so the
    // stop cannot wait for it.
    wait_for_zombie(process_id);
    let reason = scenario.operation("pause", scenario.handle().pause()).await;
    assert_eq!(reason, StopReason::Pause);

    // As after attaching, the exited main thread is not listed.
    let snapshot = scenario.snapshot().await;
    assert_eq!(snapshot.threads.len(), 1, "{:?}", snapshot.threads);
    assert!(matches!(
        snapshot.threads[0].state,
        ThreadState::Stopped { .. }
    ));
    scenario
        .operation("read the worker's registers", scenario.handle().registers())
        .await;

    // A breakpoint added at the stop is in place when the worker resumes.
    let breakpoint = scenario.add_source_breakpoint("exited-leader.c", 9).await;
    let reason = scenario.resume_to_stop().await;
    assert!(
        matches!(&reason, StopReason::Breakpoint { hits, .. } if hits.iter().map(|hit| hit.breakpoint).eq([breakpoint.id])),
        "{reason:?}"
    );
    scenario.shutdown().await;
}

#[tokio::test]
async fn pause_cancels_an_active_source_execution_plan() {
    let mut scenario = Scenario::new("pause source plan", Scenario::fixture("step"));
    scenario.add_breakpoint("step_forever").await;
    scenario.run_to_stop().await;

    // The breakpoint is in the loop body, where it would end the finish
    // before the pause could.
    scenario.remove_all_breakpoints().await;

    let snapshot = scenario.snapshot().await;
    let (process, stop, thread) = match snapshot.inferior {
        InferiorState::Stopped {
            process_id,
            stop_id,
            thread_id,
            ..
        } => (process_id, stop_id, thread_id),
        other => panic!("expected stopped inferior, got {other:?}"),
    };

    scenario
        .operation(
            "start nonterminating finish",
            scenario.handle().start_step(
                stop,
                thread,
                microscope::StackFrameId::INNERMOST,
                StepKind::Out,
                microscope::ResumeScope::Process(process),
                microscope::ExceptionDisposition::Pass,
            ),
        )
        .await;

    let reason = timeout(Duration::from_secs(2), scenario.handle().pause())
        .await
        .expect("pause timed out")
        .expect("pause failed");
    assert_eq!(reason, StopReason::Pause);
    assert!(
        scenario
            .snapshot()
            .await
            .threads
            .iter()
            .all(|thread| matches!(thread.state, ThreadState::Stopped { .. }))
    );

    // A finish breakpoint the pause left behind would stop the program
    // before it exits.
    let release = scenario
        .operation(
            "resolve step release",
            scenario.handle().runtime_address("step_release"),
        )
        .await;
    scenario
        .operation(
            "release step loop",
            scenario.handle().write_word(release, 1),
        )
        .await;
    assert_eq!(
        scenario.resume_to_stop().await,
        StopReason::Exited(ExitStatus::Code(0)),
        "a canceled source-step plan left state that interrupted execution"
    );
    assert_eq!(scenario.shutdown().await, Some(ExitStatus::Code(0)));
}

#[tokio::test]
async fn source_path_maps_find_sources_of_programs_built_elsewhere() {
    use microscope::{Error, SourcePathMap};
    use std::path::{Path, PathBuf};
    use support::ScratchDir;

    let mut scenario = Scenario::launch("basic-relocated");
    scenario.add_source_breakpoint("basic.c", 11).await;
    assert!(matches!(
        scenario.run_to_stop().await,
        StopReason::Breakpoint { .. }
    ));
    let recorded = PathBuf::from("/nonexistent/microscope/tests/fixtures/c/basic.c");
    let repository = Path::new(env!("CARGO_MANIFEST_DIR"));
    let map = |rules: &[(&Path, &Path)]| {
        let mut map = SourcePathMap::new();
        for (from, to) in rules {
            map.push(from, to).unwrap();
        }

        scenario.handle().clone().with_source_paths(map)
    };

    // Unmapped, the recorded path names nothing on this machine.
    assert!(matches!(
        scenario.handle().source_context(0).await,
        Err(Error::SourceFileMissing { path, tried }) if path == recorded && tried == [recorded.clone()]
    ));

    // The first rewrite that exists is read and reported; the debug
    // information keeps naming the recorded file.
    let copy = ScratchDir::new("source-map");
    let copied = copy.path().join("tests/fixtures/c/basic.c");
    fs::create_dir_all(copied.parent().unwrap()).unwrap();
    let source = fs::read_to_string(repository.join("tests/fixtures/c/basic.c")).unwrap();
    fs::write(
        &copied,
        source.replace(
            "first = breakpoint_target();",
            "first = breakpoint_target(); /* copy */",
        ),
    )
    .unwrap();
    let absent = copy.path().join("absent");
    let handle = map(&[
        (
            Path::new("/nonexistent/microscope/tests/fixtures/c/basic.c/x"),
            &absent,
        ),
        (Path::new("/nonexistent/microscope"), &absent),
        (
            Path::new("/nonexistent/microscope/tests"),
            &copy.path().join("tests"),
        ),
        (Path::new("/nonexistent"), repository.parent().unwrap()),
    ]);
    let context = scenario
        .operation("mapped source", handle.source_context(1))
        .await;
    assert_eq!(*context.path, copied);
    assert_eq!(*context.recorded, recorded);
    assert_eq!(context.line.get(), 11);
    assert_eq!(
        &*context.lines[0].text,
        "    uint64_t first = breakpoint_target(); /* copy */"
    );

    // Rules match whole leading components, and a missing source names
    // every place it was looked for.
    let handle = map(&[
        (Path::new("/nonexist"), repository),
        (Path::new("/nonexistent/microscope"), &absent),
    ]);
    assert!(matches!(
        handle.source_context(0).await,
        Err(Error::SourceFileMissing { path, tried })
            if path == recorded && tried == [absent.join("tests/fixtures/c/basic.c"), recorded.clone()]
    ));

    // A candidate that exists but cannot be read is reported, not skipped.
    let unreadable = copy.path().join("unreadable");
    fs::create_dir_all(unreadable.join("tests/fixtures/c/basic.c")).unwrap();
    let handle = map(&[
        (Path::new("/nonexistent/microscope"), &unreadable),
        (Path::new("/nonexistent/microscope"), repository),
    ]);
    assert!(matches!(
        handle.source_context(0).await,
        Err(Error::SourceFileRead { path, .. }) if path == unreadable.join("tests/fixtures/c/basic.c")
    ));

    // Each handle keeps its own map.
    let mapped = map(&[(Path::new("/nonexistent/microscope"), repository)]);
    assert_eq!(
        *scenario
            .operation("repository source", mapped.source_context(0))
            .await
            .path,
        repository.join("tests/fixtures/c/basic.c")
    );
    assert!(scenario.handle().source_context(0).await.is_err());
    assert_eq!(
        scenario.resume_to_stop().await,
        StopReason::Exited(microscope::ExitStatus::Code(0))
    );
    scenario.shutdown().await;
}

#[tokio::test]
async fn function_breakpoints_stop_at_every_function_with_the_name() {
    let mut scenario = Scenario::launch("same-names");
    let image = scenario.handle().module_image().clone();

    // A file that only calls shared_entry describes it by a declaration,
    // which never competes with its definition.
    assert!(image.function_named("shared_entry").is_ok());
    assert!(matches!(
        image.function_named("helper"),
        Err(Error::DuplicateFunction(name)) if name == "helper"
    ));
    assert!(matches!(
        scenario
            .handle()
            .add_breakpoint(BreakpointSpec::Function("absent".into()))
            .await,
        Err(Error::FunctionNotFound(name)) if name == "absent"
    ));

    let helper = scenario.add_breakpoint("helper").await;
    assert_eq!(helper.locations.len(), 2, "{helper:?}");
    let shared = scenario.add_breakpoint("shared_entry").await;
    assert_eq!(shared.locations.len(), 1, "{shared:?}");

    for (breakpoint, file) in [
        (&helper, "main.c"),
        (&shared, "other.c"),
        (&helper, "other.c"),
    ] {
        let reason = scenario.resume_or_run().await;
        assert!(
            matches!(&reason, StopReason::Breakpoint { hits, .. } if hits.iter().map(|hit| hit.breakpoint).eq([breakpoint.id])),
            "{reason:?}"
        );
        let location = scenario
            .operation("location", scenario.handle().current_location())
            .await;
        let source = location.image.source.expect("source");
        let path = image
            .source_file(source.file)
            .expect("source file")
            .path
            .clone();
        assert!(path.ends_with(file), "{path:?} is not {file}");
    }

    assert_eq!(
        scenario.resume_to_stop().await,
        StopReason::Exited(ExitStatus::Code(0))
    );
    scenario.shutdown().await;
}

#[tokio::test]
async fn source_breakpoints_move_to_the_next_line_with_code_in_their_function() {
    let path = "tests/fixtures/c/line-sliding.c";
    let line = |needle| microscope::LineNumber::new(source_line(path, needle)).expect("line");
    let comment = line("a comment inside the function");
    let blank = microscope::LineNumber::new(comment.get() + 1).expect("line");
    let code = line("the next code after the comment");
    let between = line("a comment between the functions");

    let mut scenario = Scenario::launch("line-sliding");
    let image = scenario.handle().module_image().clone();
    let file = image
        .source_file_matching(std::path::Path::new("line-sliding.c"))
        .expect("source file")
        .id;

    // Resolving a moved line again leaves it where it is.
    for requested in [comment, blank, code] {
        assert_eq!(image.breakpoint_line(file, requested), Some(code));
    }

    assert_eq!(image.breakpoint_line(file, between), None);
    let last = microscope::LineNumber::new(u64::MAX).expect("line");
    let lines = image
        .breakpoint_lines(file, microscope::LineNumber::new(1).expect("line")..=last)
        .collect::<Vec<_>>();
    assert!(lines.contains(&code) && lines.contains(&line("int doubled")));
    assert!(!lines.contains(&comment) && !lines.contains(&blank) && !lines.contains(&between));

    assert!(matches!(
        scenario
            .handle()
            .add_breakpoint(BreakpointSpec::Source {
                path: "line-sliding.c".into(),
                line: between,
            })
            .await,
        Err(Error::SourceLineUnavailable { line, .. }) if line == between.get()
    ));
    let moved = scenario
        .add_source_breakpoint("line-sliding.c", comment.get())
        .await;
    let exact = scenario
        .add_source_breakpoint("line-sliding.c", code.get())
        .await;
    assert_eq!(moved.locations, exact.locations);
    assert!(matches!(
        scenario.run_to_stop().await,
        StopReason::Breakpoint { hits, .. }
            if hits.iter().map(|hit| hit.breakpoint).eq([moved.id, exact.id])
    ));
    let location = scenario
        .operation("location", scenario.handle().current_location())
        .await;
    assert_eq!(location.image.source.map(|source| source.line), Some(code));
    assert_eq!(
        scenario.resume_to_stop().await,
        StopReason::Exited(ExitStatus::Code(0))
    );
    scenario.shutdown().await;
}

#[tokio::test]
async fn killing_ends_the_inferior_but_keeps_the_session() {
    let mut scenario = Scenario::launch("basic");
    assert!(matches!(
        scenario.handle().kill().await,
        Err(Error::NotRunning)
    ));
    let breakpoint = scenario.add_breakpoint("breakpoint_target").await;
    assert!(matches!(
        scenario.run_to_stop().await,
        StopReason::Breakpoint { .. }
    ));
    let mut events = scenario.handle().subscribe();
    let first = match scenario.snapshot().await.inferior {
        InferiorState::Stopped { process_id, .. } => process_id,
        other => panic!("not stopped: {other:?}"),
    };

    scenario
        .operation("kill a stopped inferior", scenario.handle().kill())
        .await;
    assert!(matches!(
        scenario.snapshot().await.inferior,
        InferiorState::NotRunning
    ));
    let exits = std::iter::from_fn(|| events.try_recv().ok())
        .filter_map(|event| match event {
            microscope::DebuggerEvent::InferiorExited {
                process_id, status, ..
            } => Some((process_id, status)),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert!(
        matches!(
            exits.as_slice(),
            [(process, ExitStatus::Terminated(info))] if *process == first && info.code == 9
        ),
        "{exits:?}"
    );
    assert!(!std::path::Path::new(&format!("/proc/{first}")).exists());

    // The same program launches again, keeping its breakpoints.
    let relaunched = scenario.run_to_stop().await;
    assert!(
        matches!(&relaunched, StopReason::Breakpoint { hits, .. } if hits.iter().map(|hit| hit.breakpoint).eq([breakpoint.id])),
        "{relaunched:?}"
    );
    scenario.shutdown().await;
}

#[tokio::test]
async fn killing_a_running_or_attached_process_ends_it() {
    let mut scenario = Scenario::launch("spin");
    let _run = scenario.start_running().await;
    scenario
        .operation("kill a running inferior", scenario.handle().kill())
        .await;
    assert!(matches!(
        scenario.snapshot().await.inferior,
        InferiorState::NotRunning
    ));
    scenario.shutdown().await;

    let child = support::ExternalProcess::spawn(&Scenario::fixture("attach"));
    let process = child.process_id();
    let debugger = child.attach().await;
    let mut events = debugger.handle().subscribe();
    timeout(Duration::from_secs(2), debugger.handle().kill())
        .await
        .expect("kill timed out")
        .expect("kill the attached process");
    assert!(
        std::iter::from_fn(|| events.try_recv().ok()).any(|event| matches!(
            event,
            microscope::DebuggerEvent::InferiorExited {
                process_id,
                status: ExitStatus::Terminated(info),
                ..
            } if process_id == process && info.code == 9
        ))
    );
    debugger.shutdown().await.expect("shut down");

    // The test spawned the process, so the debugger's wait reaped it.
    assert!(!std::path::Path::new(&format!("/proc/{process}")).exists());
}

#[tokio::test]
async fn terminating_asks_the_program_to_end_without_stopping_it() {
    let sigterm = microscope::signal_named("SIGTERM").expect("SIGTERM");
    let mut scenario = Scenario::launch("terminate");
    assert!(
        scenario
            .operation("SIGTERM policy", scenario.handle().signal_policy(sigterm))
            .await
            .stop,
        "SIGTERM stops by default"
    );

    // While running, once the program handles the signal.
    scenario.add_breakpoint("terminate_tick").await;
    assert!(matches!(
        scenario.run_to_stop().await,
        StopReason::Breakpoint { .. }
    ));
    scenario.remove_all_breakpoints().await;
    let running = scenario.start_resuming().await;
    scenario
        .operation("terminate", scenario.handle().terminate())
        .await;
    assert_eq!(
        timeout(Duration::from_secs(5), running)
            .await
            .expect("no exit")
            .expect("resume task")
            .expect("resume"),
        StopReason::Exited(ExitStatus::Code(7))
    );

    // While stopped: the program resumes to receive the signal.
    scenario.add_breakpoint("terminate_tick").await;
    assert!(matches!(
        scenario.run_to_stop().await,
        StopReason::Breakpoint { .. }
    ));
    scenario.remove_all_breakpoints().await;
    let mut events = scenario.handle().subscribe();
    scenario
        .operation("terminate", scenario.handle().terminate())
        .await;
    loop {
        match timeout(Duration::from_secs(5), events.recv())
            .await
            .expect("no exit")
            .expect("events")
        {
            microscope::DebuggerEvent::InferiorExited { status, .. } => {
                assert_eq!(status, ExitStatus::Code(7));
                break;
            }
            microscope::DebuggerEvent::InferiorStopped { reason, .. } => {
                panic!("terminating stopped the program: {reason:?}")
            }
            _ => {}
        }
    }

    scenario.shutdown().await;
}

#[tokio::test]
async fn a_line_breakpoint_stops_where_the_line_begins_not_at_its_later_statements() {
    let line_of = |marker| source_line("tests/fixtures/c/reexec.c", marker);
    let mut scenario = Scenario::launch("reexec");

    // The loop's header has statements for its start and for each test of
    // its condition; only its start is a place to stop.
    let header = line_of("for (int index = 1;");
    let image = scenario.handle().module_image().clone();
    let file = image
        .source_file_matching("reexec.c".as_ref())
        .expect("source");
    let line = microscope::LineNumber::new(header).expect("line");
    assert!(image.statement_addresses(file.id, line).count() > 1);
    let loop_start = scenario.add_source_breakpoint("reexec.c", header).await;
    assert_eq!(loop_start.locations.len(), 1);

    // Its body runs once per argument, stopping each time.
    let body = scenario
        .add_source_breakpoint("reexec.c", line_of("puts(argv[index]);"))
        .await;
    let mut hits = Vec::new();
    loop {
        match scenario.resume_or_run().await {
            StopReason::Breakpoint { hits: stop, .. } => hits.push(stop[0].breakpoint),
            // The first image only executes itself again.
            StopReason::Exec { followed: true } => {}
            StopReason::Exited(_) => break,
            other => panic!("stopped for {other:?}"),
        }
    }

    assert_eq!(hits, [loop_start.id, body.id]);
    scenario.shutdown().await;
}

fn assert_basic_source_context(
    context: &SourceContext,
    source_file: &SourceFile,
    source: &SourceLocation,
) {
    let current = context
        .lines
        .iter()
        .find(|line| line.number == context.line)
        .expect("current source line");

    assert_eq!(context.recorded, source_file.path);
    assert_eq!(context.line, source.line);
    assert_eq!(context.column, source.column);
    assert_eq!(context.line.get(), 6);
    assert_eq!(current.text.as_ref(), "    return microscope_value;");
    assert_eq!(
        context
            .lines
            .first()
            .expect("first source line")
            .number
            .get(),
        3
    );
    assert_eq!(
        context.lines.last().expect("last source line").number.get(),
        9
    );
}

fn assert_register_snapshot(
    registers: &microscope::RegisterSnapshot,
    state: &microscope::StateSnapshot,
    instruction: VirtualAddress,
) {
    assert_eq!(registers.revision, state.revision);
    assert_eq!(registers.target.architecture, Architecture::X86_64);
    assert_eq!(registers.target.byte_order, ByteOrder::Little);
    assert_eq!(registers.target.pointer_width, PointerWidth::Bits64);
    assert_eq!(
        registers.context.as_thread().expect("a thread").get(),
        match &state.inferior {
            InferiorState::Stopped { process_id, .. } => process_id.get(),
            _ => panic!("inferior was not stopped"),
        }
    );
    assert_eq!(
        register_u64(registers, RegisterRole::ProgramCounter),
        instruction.get()
    );
    assert_ne!(register_u64(registers, RegisterRole::StackPointer), 0);
    assert_ne!(register_u64(registers, RegisterRole::FramePointer), 0);
    assert!(registers.registers.iter().any(|value| {
        value.register.name.as_ref() == "rax"
            && value.register.bits == 64
            && value.bytes.as_ref().is_some_and(|bytes| bytes.len() == 8)
    }));
}
