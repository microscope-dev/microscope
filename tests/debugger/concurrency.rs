//! Execution control while other threads run: steps that let every thread
//! run, and breakpoint and watchpoint edits while the inferior runs.

use super::*;
use microscope::{ExceptionDisposition, ResumeScope, ThreadId, WatchAccess, WatchpointSpec};

/// The thread-steps fixture, unoptimized and optimized.
const THREAD_STEPS: [&str; 2] = ["thread-steps-gcc-o0", "thread-steps-clang-o2"];

fn thread_steps_line(needle: &str) -> u64 {
    source_line("tests/fixtures/c/thread-steps.c", needle)
}

async fn stopped_thread(scenario: &mut Scenario) -> ThreadId {
    match scenario.snapshot().await.inferior {
        InferiorState::Stopped { thread_id, .. } => thread_id,
        other => panic!("the inferior is not stopped: {other:?}"),
    }
}

/// The selected frame's function and source line.
async fn position(scenario: &Scenario) -> (Option<String>, Option<u64>) {
    let location = scenario
        .operation("location", scenario.handle().current_location())
        .await;
    (
        location
            .image
            .function
            .map(|function| function.name.to_string()),
        location.image.source.map(|source| source.line.get()),
    )
}

/// Sets the 32-bit flag `name` to 1 without touching the bytes after it.
async fn set_flag(scenario: &Scenario, name: &str) {
    let address = scenario
        .operation(name, scenario.handle().runtime_address(name))
        .await;
    let word = scenario
        .operation(name, scenario.handle().read_word(address))
        .await;
    scenario
        .operation(
            name,
            scenario
                .handle()
                .write_word(address, (word & !0xffff_ffff) | 1),
        )
        .await;
}

/// Launches thread-steps with `arguments` and runs to a breakpoint on the
/// line containing `needle`, returning the main thread.
async fn thread_steps_at(fixture: &str, arguments: &[&str], needle: &str) -> (Scenario, ThreadId) {
    let mut scenario = Scenario::launch(fixture);
    scenario
        .add_source_breakpoint("thread-steps.c", thread_steps_line(needle))
        .await;
    let reason = scenario
        .run_with_to_stop(LaunchOptions {
            arguments: arguments.iter().map(Into::into).collect(),
            ..LaunchOptions::default()
        })
        .await;
    assert!(
        matches!(reason, StopReason::Breakpoint { .. }),
        "{fixture}: {reason:?}"
    );
    let main = stopped_thread(&mut scenario).await;
    scenario.remove_all_breakpoints().await;

    (scenario, main)
}

#[tokio::test]
async fn next_over_a_join_runs_the_thread_it_waits_for() {
    for fixture in THREAD_STEPS {
        let (mut scenario, main) = thread_steps_at(fixture, &[], "joins the gated worker").await;

        // The worker waits at a gate this line opens, so it is still alive
        // here, and the step completes only because it runs while it does.
        assert_eq!(scenario.snapshot().await.threads.len(), 2, "{fixture}");
        assert_eq!(
            scenario.step_to_stop(StepKind::OverSource).await,
            StopReason::Step {
                kind: StepKind::OverSource
            },
            "{fixture}"
        );
        assert_eq!(stopped_thread(&mut scenario).await, main, "{fixture}");
        assert_eq!(
            position(&scenario).await,
            (
                Some("main".to_owned()),
                Some(thread_steps_line("return atomic_load(&worker_finished)"))
            ),
            "{fixture}"
        );
        assert_eq!(
            scenario.resume_to_stop().await,
            StopReason::Exited(ExitStatus::Code(0)),
            "{fixture}"
        );
        scenario.shutdown().await;
    }
}

#[tokio::test]
async fn another_threads_breakpoint_ends_a_step_and_retracts_its_plan() {
    for fixture in THREAD_STEPS {
        let (mut scenario, main) = thread_steps_at(fixture, &[], "joins the gated worker").await;

        // The worker cannot reach its breakpoint before this line opens its
        // gate.
        let worker = scenario.add_breakpoint("worker_reached").await;
        let reason = scenario.step_to_stop(StepKind::OverSource).await;
        assert!(
            matches!(
                &reason,
                StopReason::Breakpoint { hits, .. } if hits.iter().map(|hit| hit.breakpoint).eq([worker.id])
            ),
            "{fixture}: the worker's breakpoint must end the step: {reason:?}"
        );
        assert_ne!(stopped_thread(&mut scenario).await, main, "{fixture}");
        assert_eq!(
            position(&scenario).await.0.as_deref(),
            Some("worker_reached"),
            "{fixture}"
        );

        // The interrupted step's internal breakpoints are gone, so the join
        // completes and the program exits without another stop.
        assert_eq!(
            scenario.resume_to_stop().await,
            StopReason::Exited(ExitStatus::Code(0)),
            "{fixture}"
        );
        scenario.shutdown().await;
    }
}

#[tokio::test]
async fn steps_complete_on_their_thread_while_another_runs_the_same_code() {
    for fixture in THREAD_STEPS {
        let (mut scenario, main) = thread_steps_at(fixture, &["race"], "main's shared call").await;

        // The racer keeps calling the functions the main thread steps
        // through, reaching every internal breakpoint of its steps.
        for function in ["shared_caller", "shared_work"] {
            let mut entered = false;
            for _ in 0..16 {
                assert_eq!(
                    scenario.step_to_stop(StepKind::IntoSource).await,
                    StopReason::Step {
                        kind: StepKind::IntoSource
                    },
                    "{fixture}"
                );
                assert_eq!(stopped_thread(&mut scenario).await, main, "{fixture}");
                if position(&scenario).await.0.as_deref() == Some(function) {
                    entered = true;
                    break;
                }
            }

            assert!(entered, "{fixture}: never stepped into {function}");
        }

        // Optimized code may fold the lines, so `next` can return early.
        let optimized = !fixture.ends_with("-o0");
        let lines = ["value += 3;", "thread_steps_sink = value;"].map(thread_steps_line);

        for line in lines {
            assert_eq!(
                scenario.step_to_stop(StepKind::OverSource).await,
                StopReason::Step {
                    kind: StepKind::OverSource
                },
                "{fixture}"
            );
            assert_eq!(stopped_thread(&mut scenario).await, main, "{fixture}");
            let (function, stopped_line) = position(&scenario).await;
            if optimized && function.as_deref() != Some("shared_work") {
                break;
            }

            assert_eq!(function.as_deref(), Some("shared_work"), "{fixture}");
            if !optimized {
                assert_eq!(stopped_line, Some(line), "{fixture}");
            }
        }

        for caller in ["shared_caller", "main"] {
            if optimized && position(&scenario).await.0.as_deref() == Some(caller) {
                continue;
            }

            assert_eq!(
                scenario.step_to_stop(StepKind::Out).await,
                StopReason::Step {
                    kind: StepKind::Out
                },
                "{fixture}"
            );
            assert_eq!(stopped_thread(&mut scenario).await, main, "{fixture}");
            assert_eq!(
                position(&scenario).await.0.as_deref(),
                Some(caller),
                "{fixture}"
            );
        }

        assert_eq!(
            scenario.resume_to_stop().await,
            StopReason::Exited(ExitStatus::Code(0)),
            "{fixture}"
        );
        scenario.shutdown().await;
    }
}

#[tokio::test]
async fn a_single_thread_step_keeps_every_other_thread_stopped() {
    for fixture in THREAD_STEPS {
        let (mut scenario, main) = thread_steps_at(fixture, &["race"], "main's shared call").await;
        let rounds = scenario
            .operation(
                "racer_rounds address",
                scenario.handle().runtime_address("racer_rounds"),
            )
            .await;
        let before = scenario
            .operation("racer rounds", scenario.handle().read_word(rounds))
            .await;

        for kind in [StepKind::IntoSource, StepKind::OverSource, StepKind::Out] {
            assert_eq!(
                scenario.step_alone_to_stop(kind).await,
                StopReason::Step { kind },
                "{fixture}"
            );
            assert_eq!(stopped_thread(&mut scenario).await, main, "{fixture}");
        }

        // The racer never sleeps, so it would have counted rounds had it run.
        assert_eq!(
            scenario
                .operation("racer rounds", scenario.handle().read_word(rounds))
                .await,
            before,
            "{fixture}: the racer ran during single-thread steps"
        );

        // A single-thread scope names the stepping thread.
        let snapshot = scenario.snapshot().await;
        let racer = snapshot
            .threads
            .iter()
            .map(|thread| thread.id)
            .find(|thread| *thread != main)
            .expect("racer thread");
        assert!(matches!(
            scenario
                .handle()
                .start_step(
                    snapshot.stop_id.expect("stopped"),
                    main,
                    microscope::StackFrameId::INNERMOST,
                    StepKind::Instruction,
                    ResumeScope::Thread(racer),
                    ExceptionDisposition::Pass,
                )
                .await,
            Err(Error::StepScopeMismatch { stepping, resumed })
                if stepping == main && resumed == racer
        ));
        assert_eq!(
            scenario.resume_to_stop().await,
            StopReason::Exited(ExitStatus::Code(0)),
            "{fixture}"
        );
        scenario.shutdown().await;
    }
}

#[tokio::test]
async fn breakpoints_are_edited_while_running_without_publishing_a_stop() {
    let mut scenario = Scenario::launch("spin");
    let unreached = scenario.add_breakpoint("unreached").await;
    let handle = scenario.handle().clone();
    let run = scenario.start_running().await;
    let InferiorState::Running {
        execution_id: Some(execution),
        ..
    } = scenario.snapshot().await.inferior
    else {
        panic!("the inferior is not running");
    };

    let mut events = handle.subscribe();

    let removed = scenario
        .operation(
            "remove while running",
            handle.remove_breakpoint(unreached.id),
        )
        .await;
    assert_eq!(removed, unreached);
    assert!(scenario.snapshot().await.breakpoints.is_empty());
    let added = scenario
        .operation(
            "add while running",
            handle.add_breakpoint(BreakpointSpec::Source {
                path: "spin.c".into(),
                line: microscope::LineNumber::new(10).expect("line"),
            }),
        )
        .await;

    // The loop reaches the new breakpoint, ending the launch's execution.
    let reason = timeout(Duration::from_secs(5), run)
        .await
        .expect("the new breakpoint was not reached")
        .expect("run task")
        .expect("run");
    assert!(
        matches!(&reason, StopReason::Breakpoint { hits, .. } if hits.iter().map(|hit| hit.breakpoint).eq([added.id])),
        "{reason:?}"
    );
    let mut published = Vec::new();
    while let Ok(event) = events.try_recv() {
        published.push(event);
    }

    assert!(
        !published
            .iter()
            .any(|event| matches!(event, microscope::DebuggerEvent::InferiorContinued { .. })),
        "the edits published a resume: {published:?}"
    );
    let stops = published
        .iter()
        .filter_map(|event| match event {
            microscope::DebuggerEvent::InferiorStopped { execution_id, .. } => Some(*execution_id),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        stops,
        [Some(execution)],
        "only the breakpoint stops, under the launch's execution"
    );
    assert_eq!(
        published
            .iter()
            .filter(|event| matches!(event, microscope::DebuggerEvent::BreakpointsChanged { .. }))
            .count(),
        2
    );
    scenario.shutdown().await;
}

/// Waits for the next stop event, which must name `breakpoint`, and resumes
/// the process from it.
async fn resume_from_hit(
    scenario: &Scenario,
    events: &mut tokio::sync::broadcast::Receiver<microscope::DebuggerEvent>,
    breakpoint: microscope::BreakpointId,
    context: &str,
) {
    loop {
        let event = timeout(Duration::from_secs(5), events.recv())
            .await
            .unwrap_or_else(|_| panic!("{context}: no stop"))
            .unwrap_or_else(|error| panic!("{context}: event stream failed: {error}"));
        let microscope::DebuggerEvent::InferiorStopped {
            process_id,
            stop_id,
            reason,
            ..
        } = event
        else {
            continue;
        };

        assert!(
            matches!(&reason, StopReason::Breakpoint { hits, .. } if hits.iter().map(|hit| hit.breakpoint).eq([breakpoint])),
            "{context}: {reason:?}"
        );
        scenario
            .operation(
                "continue after a hit",
                scenario.handle().continue_execution(
                    stop_id,
                    ResumeScope::Process(process_id),
                    ExceptionDisposition::Pass,
                ),
            )
            .await;
        return;
    }
}

#[tokio::test]
async fn breakpoints_edited_while_threads_hit_them_report_only_current_breakpoints() {
    let mut scenario = Scenario::launch("hot-calls");
    let handle = scenario.handle().clone();
    let mut events = handle.subscribe();
    let _run = scenario.start_running().await;

    for round in 0..64 {
        let context = format!("round {round}");
        let breakpoint = scenario
            .operation(
                "add hot breakpoint",
                handle.add_breakpoint(BreakpointSpec::Function("hot_function".into())),
            )
            .await;
        if round % 2 == 1 {
            // Four threads call the function continuously, so it is hit.
            resume_from_hit(&scenario, &mut events, breakpoint.id, &context).await;
        }

        // Removing races with the threads' traps: a hit before the removal
        // stops the program, and one the removal overtakes is dropped.
        let removed = scenario
            .operation(
                "remove hot breakpoint",
                handle.remove_breakpoint(breakpoint.id),
            )
            .await;
        assert_eq!(removed.id, breakpoint.id);

        let mut removal = None;
        let mut stop = None;
        loop {
            match events.try_recv() {
                Ok(microscope::DebuggerEvent::BreakpointsChanged { revision }) => {
                    removal = Some(revision);
                }

                Ok(microscope::DebuggerEvent::InferiorStopped {
                    revision,
                    process_id,
                    stop_id,
                    reason,
                    ..
                }) => {
                    assert!(
                        matches!(&reason, StopReason::Breakpoint { hits, .. } if hits.iter().map(|hit| hit.breakpoint).eq([breakpoint.id])),
                        "{context}: {reason:?}"
                    );
                    stop = Some((revision, process_id, stop_id));
                }
                Ok(_) => {}
                Err(tokio::sync::broadcast::error::TryRecvError::Empty) => break,
                Err(error) => panic!("{context}: event stream failed: {error}"),
            }
        }

        if let Some((revision, process_id, stop_id)) = stop {
            let removal = removal.expect("the removal is published");
            assert!(
                revision < removal,
                "{context}: a removed breakpoint stopped the program"
            );
            scenario
                .operation(
                    "continue after a hit",
                    handle.continue_execution(
                        stop_id,
                        ResumeScope::Process(process_id),
                        ExceptionDisposition::Pass,
                    ),
                )
                .await;
        }
    }

    // Release the callers; nothing else stops the program.
    let reason = timeout(Duration::from_secs(2), handle.pause())
        .await
        .expect("pause timed out")
        .expect("pause");
    assert_eq!(reason, StopReason::Pause);
    set_flag(&scenario, "hot_stop").await;
    assert_eq!(
        scenario.resume_to_stop().await,
        StopReason::Exited(ExitStatus::Code(0))
    );
    scenario.shutdown().await;
}

#[tokio::test]
async fn watchpoints_are_armed_and_disarmed_while_running() {
    let mut scenario = Scenario::launch("hot-calls");
    let handle = scenario.handle().clone();
    let run = scenario.start_running().await;
    let flag = scenario
        .operation("hot_stop", handle.runtime_address("hot_stop"))
        .await;

    // Every caller reads the flag, so a read watchpoint armed while
    // running reports the next read.
    let reads = scenario
        .operation(
            "arm while running",
            handle.add_watchpoint(
                WatchpointSpec::Location {
                    address: flag,
                    byte_size: 4,
                },
                WatchAccess::ReadWrite,
            ),
        )
        .await;
    let reason = timeout(Duration::from_secs(5), run)
        .await
        .expect("the watchpoint did not fire")
        .expect("run task")
        .expect("run");
    assert!(
        matches!(&reason, StopReason::Watchpoint { hits } if hits.iter().all(|hit| hit.watchpoint == reads.id)),
        "{reason:?}"
    );
    scenario.drain_pending_events();
    scenario
        .operation("disarm at the stop", handle.remove_watchpoint(reads.id))
        .await;
    let stale = scenario
        .operation(
            "resolve the flag",
            handle.resolve_watch_target(&microscope::Expression::parse("hot_stop").expect("path")),
        )
        .await;

    // A write watchpoint on the flag never fires, so disarming it while
    // running resumes silently.
    let writes = scenario
        .operation(
            "arm write watchpoint",
            handle.add_watchpoint(
                WatchpointSpec::Location {
                    address: flag,
                    byte_size: 4,
                },
                WatchAccess::Write,
            ),
        )
        .await;
    let resumed = scenario.start_resuming().await;

    // A target resolved at an earlier stop is armed only at that stop.
    assert!(matches!(
        handle
            .add_watchpoint(WatchpointSpec::Target(Box::new(stale)), WatchAccess::Write)
            .await,
        Err(Error::StaleStop)
    ));
    assert_eq!(
        scenario
            .operation("disarm while running", handle.remove_watchpoint(writes.id))
            .await,
        writes
    );
    assert!(scenario.snapshot().await.watchpoints.is_empty());
    assert!(matches!(
        scenario.snapshot().await.inferior,
        InferiorState::Running { .. }
    ));
    assert_eq!(
        timeout(Duration::from_secs(2), handle.pause())
            .await
            .expect("pause timed out")
            .expect("pause"),
        StopReason::Pause
    );
    set_flag(&scenario, "hot_stop").await;
    drop(resumed);
    assert_eq!(
        scenario.resume_to_stop().await,
        StopReason::Exited(ExitStatus::Code(0))
    );
    scenario.shutdown().await;
}

#[tokio::test]
async fn a_named_threads_start_routine_finishes_when_the_thread_exits() {
    for fixture in THREAD_STEPS {
        let (mut scenario, worker) =
            thread_steps_at(fixture, &["spin"], "thread_steps_sink += 1;").await;

        // Threads are named as they name themselves; Linux keeps the first 15
        // bytes of a name.
        let names = scenario
            .snapshot()
            .await
            .threads
            .iter()
            .map(|thread| {
                (
                    thread.id == worker,
                    thread.name.as_deref().map(str::to_owned),
                )
            })
            .collect::<Vec<_>>();
        assert!(
            names.contains(&(false, Some(fixture[..15].to_owned())))
                && names.contains(&(true, Some("gated-worker".to_owned()))),
            "{names:?}"
        );
        assert_eq!(
            scenario.step_to_stop(StepKind::Out).await,
            StopReason::Step {
                kind: StepKind::Out
            },
            "{fixture}"
        );
        assert_eq!(
            position(&scenario).await.0.as_deref(),
            Some("gated_worker"),
            "{fixture}"
        );

        // The start routine returns into the C library, which has no source
        // to stop in, and the thread exits while the main thread runs.
        assert_eq!(
            scenario.step_to_stop(StepKind::Out).await,
            StopReason::ThreadExited {
                thread_id: worker,
                status: ExitStatus::Code(0),
            },
            "{fixture}"
        );
        assert!(
            !scenario
                .snapshot()
                .await
                .threads
                .iter()
                .any(|thread| thread.id == worker),
            "{fixture}"
        );
        set_flag(&scenario, "main_released").await;
        assert_eq!(
            scenario.resume_to_stop().await,
            StopReason::Exited(ExitStatus::Code(0)),
            "{fixture}"
        );
        scenario.shutdown().await;
    }
}

/// A main thread continued alone that exits while other threads live ends
/// its execution at its exit event, with the code it passed to `exit`.
/// Linux reports the main thread's exit status only once every other thread
/// has exited, which threads held stopped never do. However the session
/// then ends, nothing is left behind.
#[tokio::test]
async fn a_main_thread_continued_alone_ends_its_execution_when_it_exits() {
    let program = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("build/golden/threads/threads-gcc-O0");
    let source = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/golden/threads/threads.c"
    );

    // The main thread's call, the last of the two.
    let main_exits = fs::read_to_string(source)
        .expect("read the golden program")
        .lines()
        .collect::<Vec<_>>()
        .iter()
        .rposition(|line| line.contains("rt_exit(LEADER_STATUS);"))
        .map(|index| u64::try_from(index + 1).expect("line fits u64"))
        .expect("main's exit");

    for ending in ["exits", "is killed", "is shut down"] {
        let mut scenario = Scenario::new(
            format!("main thread exits alone, then the process {ending}"),
            &program,
        );

        // Every worker must call `share` before it can exit.
        scenario.add_breakpoint("share").await;
        let exit = scenario
            .add_source_breakpoint("threads.c", main_exits)
            .await;
        let reason = scenario
            .run_with_to_stop(LaunchOptions {
                arguments: vec!["2".into(), "leader".into()],
                ..LaunchOptions::default()
            })
            .await;
        assert!(
            matches!(reason, StopReason::Breakpoint { .. }),
            "{reason:?}"
        );
        let InferiorState::Stopped { process_id, .. } = scenario.snapshot().await.inferior else {
            panic!("expected a stop");
        };

        let main = ThreadId::new(process_id.get());
        let at_exit = |snapshot: &microscope::StateSnapshot| {
            snapshot.threads.iter().any(|thread| {
                thread.id == main
                    && matches!(&thread.state, ThreadState::Stopped {
                        reason: Some(StopReason::Breakpoint { hits, .. }),
                    } if hits.iter().any(|hit| hit.breakpoint == exit.id))
            })
        };

        if !at_exit(&scenario.snapshot().await) {
            let reason = scenario.continue_alone_to_stop(main).await;
            assert!(at_exit(&scenario.snapshot().await), "{reason:?}");
        }

        scenario.remove_all_breakpoints().await;

        assert_eq!(
            scenario.continue_alone_to_stop(main).await,
            StopReason::ThreadExited {
                thread_id: main,
                status: ExitStatus::Code(3),
            },
            "{ending}"
        );
        let threads = scenario.snapshot().await.threads;
        assert!(
            !threads.is_empty() && threads.iter().all(|thread| thread.id != main),
            "{ending}: {threads:?}"
        );
        match ending {
            "exits" => assert_eq!(
                scenario.resume_to_stop().await,
                StopReason::Exited(ExitStatus::Code(3))
            ),
            "is killed" => {
                scenario.operation("kill", scenario.handle().kill()).await;
            }
            _ => {}
        }

        scenario.shutdown().await;
    }
}
