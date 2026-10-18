//! Instruction and source stepping, inline frames, and finish.

use super::*;

#[tokio::test]
async fn source_step_into_stops_after_the_physical_prologue_with_readable_parameters() {
    for case in entry_boundary_cases() {
        let mut scenario = Scenario::new(
            format!("step entry boundary {}", case.fixture),
            Scenario::fixture(case.fixture),
        );
        let expected = expected_physical_entry(&scenario, &case);
        scenario
            .add_source_breakpoint(case.source, case.call_line)
            .await;
        assert!(matches!(
            scenario.run_to_stop().await,
            StopReason::Breakpoint { .. }
        ));

        let mut entered = false;
        for _ in 0..32 {
            assert_eq!(
                scenario.step_to_stop(StepKind::IntoSource).await,
                StopReason::Step {
                    kind: StepKind::IntoSource
                },
                "{} step-in terminated before entering {}",
                case.fixture,
                case.function
            );
            let location = scenario
                .operation("step-in location", scenario.handle().current_location())
                .await;
            if location
                .image
                .function
                .as_ref()
                .is_some_and(|function| function.name.as_ref() == case.function)
            {
                entered = true;
                assert_entry_stop(&scenario, &case, expected).await;
                break;
            }
        }

        assert!(
            entered,
            "{} did not enter {} within the source-step budget",
            case.fixture, case.function
        );
        scenario.shutdown().await;
    }
}

#[tokio::test]
async fn clang_o0_inline_steps_cover_entry_body_return_caller_and_exit() {
    let fixture = "stepping-boundaries-clang-o0";
    let (mut scenario, _) =
        launch_boundary_scenario("Clang O0 inline lifecycle".into(), fixture).await;
    let inlined = boundary_source_step(
        &mut scenario,
        StepKind::IntoSource,
        "first inline statement",
    )
    .await;
    assert_eq!(location_function(&inlined), Some("inline_adjust"));
    assert_eq!(location_line(&inlined), Some(24));
    let physical = inlined.image.physical_instance;
    let sink = fixture_symbol_address(&scenario, &inlined, "boundary_sink");
    assert_eq!(boundary_sink_value(&scenario, sink).await, 0);

    for (expected_line, expected_sink) in [(25, 0), (26, 6)] {
        let location =
            boundary_source_step(&mut scenario, StepKind::OverSource, "next inline statement")
                .await;
        assert_eq!(location_line(&location), Some(expected_line));
        assert_eq!(location_function(&location), Some("inline_adjust"));
        assert_eq!(location.image.physical_instance, physical);
        assert_eq!(
            boundary_sink_value(&scenario, sink).await,
            expected_sink,
            "inline statement {expected_line} has the wrong stop-before side effects"
        );
    }

    let caller = boundary_source_step(
        &mut scenario,
        StepKind::OverSource,
        "logical caller after inline return",
    )
    .await;
    assert_eq!(caller.image.physical_instance, physical);
    assert_eq!(location_function(&caller), Some("main"));
    assert_eq!(location_line(&caller), Some(30));

    let following_call = boundary_source_step(
        &mut scenario,
        StepKind::OverSource,
        "statement following inline call",
    )
    .await;
    assert_eq!(location_function(&following_call), Some("main"));
    assert_eq!(location_line(&following_call), Some(31));

    assert_eq!(
        scenario.resume_to_stop().await,
        StopReason::Exited(ExitStatus::Code(0))
    );
    assert_eq!(scenario.shutdown().await, Some(ExitStatus::Code(0)));
}

#[tokio::test]
async fn next_walks_the_entire_boundary_fixture_to_a_normal_exit() {
    for fixture in [
        "stepping-boundaries-gcc-o0",
        "stepping-boundaries-clang-o0",
        "stepping-boundaries-gcc-o2",
        "stepping-boundaries-clang-o2",
    ] {
        let (mut scenario, _) =
            launch_boundary_scenario(format!("full next walk {fixture}"), fixture).await;
        let call = advance_to_boundary_inline_call(&mut scenario, fixture).await;
        let sink = fixture_symbol_address(&scenario, &call, "boundary_sink");

        for (expected_line, expected_sink) in [(31, 6), (33, 11), (34, 22), (35, 4)] {
            let mut reached = false;
            for _ in 0..3 {
                let location = boundary_source_step(
                    &mut scenario,
                    StepKind::OverSource,
                    "full next walk location",
                )
                .await;
                assert_eq!(
                    location_function(&location),
                    Some("main"),
                    "{fixture}: {location:?}"
                );
                let line = location_line(&location).expect("main next stop has source");
                assert!(
                    line <= expected_line,
                    "{fixture} skipped past expected line {expected_line} to {line}"
                );
                if line == expected_line {
                    reached = true;
                    break;
                }
            }

            assert!(
                reached,
                "{fixture} did not reach main line {expected_line} within the step budget"
            );
            assert_eq!(
                boundary_sink_value(&scenario, sink).await,
                expected_sink,
                "{fixture} did not execute the expected callee before main line {expected_line}"
            );
        }

        let mut exit = scenario.step_to_stop(StepKind::OverSource).await;
        if matches!(exit, StopReason::Step { .. }) {
            let closing_brace = scenario
                .operation(
                    "optional closing-brace stop",
                    scenario.handle().current_location(),
                )
                .await;
            assert_eq!(
                location_line(&closing_brace),
                Some(36),
                "{fixture} added an unexpected stop after main's return"
            );
            exit = scenario.step_to_stop(StepKind::OverSource).await;
        }

        assert_eq!(
            exit,
            StopReason::Exited(ExitStatus::Code(0)),
            "{fixture} did not preserve the inferior's normal exit while next completed"
        );
        assert_eq!(scenario.shutdown().await, Some(ExitStatus::Code(0)));
    }
}

#[tokio::test]
async fn finish_distinguishes_inline_and_physical_frames_across_the_boundary_fixture() {
    for fixture in [
        "stepping-boundaries-gcc-o0",
        "stepping-boundaries-clang-o0",
        "stepping-boundaries-gcc-o2",
        "stepping-boundaries-clang-o2",
    ] {
        let (mut scenario, main) =
            launch_boundary_scenario(format!("inline and physical finish {fixture}"), fixture)
                .await;
        let main_physical = main.image.physical_instance;

        let inlined =
            boundary_source_step(&mut scenario, StepKind::IntoSource, "inline activation").await;
        assert_eq!(
            location_function(&inlined),
            Some("inline_adjust"),
            "{fixture}: {inlined:?}"
        );
        assert_eq!(inlined.image.physical_instance, main_physical);
        assert_eq!(location_line(&inlined), Some(24), "{fixture}: {inlined:?}");
        let sink = fixture_symbol_address(&scenario, &inlined, "boundary_sink");
        assert_eq!(
            boundary_sink_value(&scenario, sink).await,
            0,
            "{fixture} executed inline user work before its entry stop"
        );

        let after_inline =
            boundary_source_step(&mut scenario, StepKind::Out, "caller after inline finish").await;
        assert_eq!(after_inline.image.physical_instance, main_physical);
        assert_eq!(location_function(&after_inline), Some("main"));
        let after_inline_line = location_line(&after_inline).expect("inline finish has source");
        assert!(
            (30..=31).contains(&after_inline_line),
            "{fixture} finished inline_adjust at unexpected line {after_inline_line}"
        );
        advance_boundary_to_line(&mut scenario, fixture, 31).await;

        for case in [
            (31, "marked_returns", 11, 33),
            (33, "marked_returns", 22, 34),
            (34, "no_prologue", 4, 35),
        ] {
            finish_boundary_physical_call(&mut scenario, fixture, main_physical, sink, case).await;
        }

        advance_boundary_to_line(&mut scenario, fixture, 35).await;
        assert_eq!(
            scenario.step_to_stop(StepKind::Out).await,
            StopReason::Exited(ExitStatus::Code(0)),
            "{fixture} top-level finish did not preserve normal process exit"
        );
        assert_eq!(scenario.shutdown().await, Some(ExitStatus::Code(0)));
    }
}

#[tokio::test]
async fn rust_o0_inline_steps_cross_source_holes_and_return_to_the_caller() {
    let fixture = "stepping-boundaries-rust-o0";
    let (mut scenario, _) =
        launch_boundary_scenario("Rust O0 inline lifecycle".into(), fixture).await;
    advance_boundary_to_line(&mut scenario, fixture, 39).await;

    let inlined = boundary_source_step(
        &mut scenario,
        StepKind::IntoSource,
        "first Rust inline statement",
    )
    .await;
    assert_eq!(location_function(&inlined), Some("inline_adjust"));
    assert_eq!(location_line(&inlined), Some(31));
    let physical = inlined.image.physical_instance;
    let sink = fixture_symbol_address(&scenario, &inlined, "RUST_BOUNDARY_SINK");
    assert_eq!(boundary_sink_value(&scenario, sink).await, 0);

    for (expected_line, expected_sink) in [(32, 0), (33, 6)] {
        let location = boundary_source_step(
            &mut scenario,
            StepKind::OverSource,
            "next Rust inline statement",
        )
        .await;
        assert_eq!(location_function(&location), Some("inline_adjust"));
        assert_eq!(location_line(&location), Some(expected_line));
        assert_eq!(location.image.physical_instance, physical);
        assert_eq!(boundary_sink_value(&scenario, sink).await, expected_sink);
    }

    let caller = boundary_source_step(
        &mut scenario,
        StepKind::OverSource,
        "Rust caller after inline return",
    )
    .await;
    assert_eq!(location_function(&caller), Some("main"));
    assert_eq!(location_line(&caller), Some(39));
    assert_eq!(caller.image.physical_instance, physical);

    let following_call = boundary_source_step(
        &mut scenario,
        StepKind::OverSource,
        "Rust statement following inline call",
    )
    .await;
    assert_eq!(location_function(&following_call), Some("main"));
    assert_eq!(location_line(&following_call), Some(40));

    assert_eq!(
        scenario.resume_to_stop().await,
        StopReason::Exited(ExitStatus::Code(0))
    );
    assert_eq!(scenario.shutdown().await, Some(ExitStatus::Code(0)));
}

#[tokio::test]
async fn rust_o2_inline_steps_follow_optimized_statements_and_return_to_the_caller() {
    let fixture = "stepping-boundaries-rust-o2";
    let (mut scenario, _) =
        launch_boundary_scenario("Rust O2 inline lifecycle".into(), fixture).await;
    advance_boundary_to_line(&mut scenario, fixture, 39).await;

    let first = boundary_source_step(
        &mut scenario,
        StepKind::IntoSource,
        "first optimized Rust inline statement",
    )
    .await;
    assert_eq!(location_function(&first), Some("inline_adjust"));
    assert_eq!(location_line(&first), Some(31));
    let physical = first.image.physical_instance;
    let sink = fixture_symbol_address(&scenario, &first, "RUST_BOUNDARY_SINK");
    assert_eq!(boundary_sink_value(&scenario, sink).await, 0);

    let second = boundary_source_step(
        &mut scenario,
        StepKind::OverSource,
        "second optimized Rust inline statement",
    )
    .await;
    assert_eq!(location_function(&second), Some("inline_adjust"));
    assert_eq!(location_line(&second), Some(32));
    assert_eq!(second.image.physical_instance, physical);
    assert_eq!(boundary_sink_value(&scenario, sink).await, 0);

    let caller = boundary_source_step(
        &mut scenario,
        StepKind::OverSource,
        "optimized Rust caller after inline return",
    )
    .await;
    assert_eq!(location_function(&caller), Some("main"));
    assert_eq!(location_line(&caller), Some(40));
    assert_eq!(caller.image.physical_instance, physical);
    assert_eq!(boundary_sink_value(&scenario, sink).await, 6);

    assert_eq!(
        scenario.resume_to_stop().await,
        StopReason::Exited(ExitStatus::Code(0))
    );
    assert_eq!(scenario.shutdown().await, Some(ExitStatus::Code(0)));
}

#[tokio::test]
async fn next_walks_the_entire_rust_boundary_fixture_to_a_normal_exit() {
    for fixture in ["stepping-boundaries-rust-o0", "stepping-boundaries-rust-o2"] {
        let (mut scenario, main) =
            launch_boundary_scenario(format!("full Rust next walk {fixture}"), fixture).await;
        let sink = fixture_symbol_address(&scenario, &main, "RUST_BOUNDARY_SINK");
        let expected: &[(u64, u64)] = if fixture.ends_with("o0") {
            &[
                (39, 0),
                (40, 6),
                (41, 11),
                (42, 11),
                (43, 11),
                (44, 22),
                (45, 4),
            ]
        } else {
            &[(39, 0), (40, 6), (41, 11), (43, 11), (44, 22), (45, 4)]
        };

        for &(expected_line, expected_sink) in expected {
            let location = advance_boundary_to_line(&mut scenario, fixture, expected_line).await;
            assert_eq!(location_function(&location), Some("main"));
            assert_eq!(boundary_sink_value(&scenario, sink).await, expected_sink);
        }

        let mut exit = scenario.step_to_stop(StepKind::OverSource).await;
        if matches!(exit, StopReason::Step { .. }) {
            let closing = scenario
                .operation(
                    "optional Rust closing-brace stop",
                    scenario.handle().current_location(),
                )
                .await;
            assert_eq!(location_line(&closing), Some(46), "{fixture}");
            exit = scenario.step_to_stop(StepKind::OverSource).await;
        }

        assert_eq!(exit, StopReason::Exited(ExitStatus::Code(0)), "{fixture}");
        assert_eq!(scenario.shutdown().await, Some(ExitStatus::Code(0)));
    }
}

#[tokio::test]
async fn finish_distinguishes_rust_inline_and_physical_frames() {
    for fixture in ["stepping-boundaries-rust-o0", "stepping-boundaries-rust-o2"] {
        let (mut scenario, main) =
            launch_boundary_scenario(format!("Rust finish lifecycle {fixture}"), fixture).await;
        let main_physical = main.image.physical_instance;
        advance_boundary_to_line(&mut scenario, fixture, 39).await;

        let inlined = boundary_source_step(
            &mut scenario,
            StepKind::IntoSource,
            "Rust inline activation",
        )
        .await;
        assert_eq!(location_function(&inlined), Some("inline_adjust"));
        assert_eq!(inlined.image.physical_instance, main_physical);
        let returned = boundary_source_step(
            &mut scenario,
            StepKind::Out,
            "caller after Rust inline finish",
        )
        .await;
        assert_eq!(location_function(&returned), Some("main"));
        assert_eq!(returned.image.physical_instance, main_physical);
        assert!((39..=40).contains(&location_line(&returned).expect("Rust caller source")));

        let sink = fixture_symbol_address(&scenario, &returned, "RUST_BOUNDARY_SINK");
        for case in [
            (40, "marked_returns", 11, 42),
            (43, "marked_returns", 22, 44),
            (44, "no_prologue", 4, 45),
        ] {
            finish_boundary_physical_call(&mut scenario, fixture, main_physical, sink, case).await;
        }

        advance_boundary_to_line(&mut scenario, fixture, 45).await;
        assert_eq!(
            scenario.step_to_stop(StepKind::Out).await,
            StopReason::Exited(ExitStatus::Code(0)),
            "{fixture}"
        );
        assert_eq!(scenario.shutdown().await, Some(ExitStatus::Code(0)));
    }
}

/// Steps from a return line whose epilogue the compiler marked complete in
/// the caller, never at the marker; stepping out ends at the return
/// address, which a step over runs past to the caller's next line.
#[tokio::test]
async fn steps_from_marked_epilogues_complete_in_the_caller() {
    for kind in [StepKind::OverSource, StepKind::IntoSource, StepKind::Out] {
        let mut scenario = Scenario::new(
            format!("marked epilogues {kind:?}"),
            Scenario::fixture("stepping-boundaries-clang-o2"),
        );
        let markers = epilogue_markers(&scenario, "marked_returns");
        assert_eq!(markers.len(), 2, "the fixture has two marked return paths");
        scenario
            .add_source_breakpoint("stepping-boundaries.c", 11)
            .await;
        scenario
            .add_source_breakpoint("stepping-boundaries.c", 15)
            .await;

        for return_line in [11, 15] {
            let context = format!("{kind:?} from line {return_line}");
            let reason = scenario.resume_or_run().await;
            assert!(
                matches!(reason, StopReason::Breakpoint { .. }),
                "{context}: {reason:?}"
            );
            let before = scenario
                .operation("return line", scenario.handle().current_location())
                .await;
            assert_eq!(location_line(&before), Some(return_line), "{context}");
            let return_address = scenario
                .operation("backtrace", scenario.handle().backtrace())
                .await
                .frames
                .iter()
                .filter(|frame| frame.kind != microscope::FrameKind::Inline)
                .nth(1)
                .expect("the return line's caller")
                .instruction
                .expect("a thread's frame has an instruction");

            assert_eq!(
                scenario.step_to_stop(kind).await,
                StopReason::Step { kind },
                "{context}"
            );
            let after = scenario
                .operation("caller after return", scenario.handle().current_location())
                .await;
            assert_eq!(
                location_function(&after),
                Some("main"),
                "{context}: {after:?}"
            );
            assert!(
                !markers.contains(&after.image.address),
                "{context}: {after:?}"
            );
            if kind == StepKind::Out {
                assert_eq!(after.address, return_address, "{context}");
            }
        }

        assert_eq!(
            scenario.resume_to_stop().await,
            StopReason::Exited(ExitStatus::Code(0))
        );
        scenario.shutdown().await;
    }
}

/// Stepping over a function's last line stops in its caller, even when the
/// caller's line calls the function again at the same stack depth before
/// reaching a statement: the second call is a new frame.
#[tokio::test]
async fn stepping_over_a_return_stops_in_the_caller_before_a_second_call() {
    let line_of = |marker| source_line("tests/fixtures/c/repeated-calls.c", marker);
    for fixture in ["repeated-calls-gcc-o0", "repeated-calls-clang-o0"] {
        let mut scenario = Scenario::new(
            format!("step over a return {fixture}"),
            Scenario::fixture(fixture),
        );
        scenario
            .add_source_breakpoint("repeated-calls.c", line_of("LOAD_RETURN"))
            .await;
        assert!(matches!(
            scenario.run_to_stop().await,
            StopReason::Breakpoint { .. }
        ));
        scenario
            .operation(
                "remove the breakpoint",
                scenario.handle().remove_all_breakpoints(),
            )
            .await;

        // A compiler may give the closing brace a line of its own, which
        // the first step stops on.
        let mut location = None;
        for _ in 0..2 {
            assert_eq!(
                scenario.step_to_stop(StepKind::OverSource).await,
                StopReason::Step {
                    kind: StepKind::OverSource
                },
                "{fixture}"
            );
            let here = scenario
                .operation("location", scenario.handle().current_location())
                .await;
            let closing = location_line(&here) == Some(line_of("LOAD_RETURN") + 1);
            location = Some(here);
            if !closing {
                break;
            }
        }

        let location = location.expect("a step was taken");
        assert_eq!(
            location_function(&location),
            Some("main"),
            "{fixture} stepped over load's return to {location:?}"
        );
        assert_eq!(
            scenario.resume_to_stop().await,
            StopReason::Exited(ExitStatus::Code(0)),
            "{fixture}"
        );
        scenario.shutdown().await;
    }
}

/// A source step from a function's last line returns through a caller with
/// no statement left into that caller's caller, rather than running to the
/// program's exit; a step over never stops in the next call of the same
/// function, a new frame where the old one was.
#[tokio::test]
async fn source_steps_return_through_a_caller_with_nothing_left_to_run() {
    let line_of = |marker| source_line("tests/fixtures/c/repeated-calls.c", marker);
    for fixture in ["repeated-calls-gcc-o0", "repeated-calls-clang-o0"] {
        for kind in [StepKind::IntoSource, StepKind::OverSource] {
            let mut scenario = Scenario::new(
                format!("step {kind:?} back through relay {fixture}"),
                Scenario::fixture(fixture),
            );
            scenario
                .add_source_breakpoint("repeated-calls.c", line_of("RELAY_CALL"))
                .await;
            assert!(matches!(
                scenario.run_to_stop().await,
                StopReason::Breakpoint { .. }
            ));

            // The first call, relay(3), reaches load's last line.
            scenario
                .add_source_breakpoint("repeated-calls.c", line_of("LOAD_RETURN"))
                .await;
            assert!(matches!(
                scenario.resume_to_stop().await,
                StopReason::Breakpoint { .. }
            ));
            scenario
                .operation(
                    "remove the breakpoints",
                    scenario.handle().remove_all_breakpoints(),
                )
                .await;
            let mut location = None;
            for _ in 0..8 {
                assert_eq!(
                    scenario.step_to_stop(kind).await,
                    StopReason::Step { kind },
                    "{fixture} {kind:?}"
                );
                let here = scenario
                    .operation("location", scenario.handle().current_location())
                    .await;
                let function = here
                    .image
                    .function
                    .as_ref()
                    .map(|function| function.name.to_string());
                if kind == StepKind::OverSource && function.as_deref() == Some("relay") {
                    let index = scenario
                        .operation("relay's index", scenario.handle().variable("index"))
                        .await;
                    assert!(
                        matches!(
                            &index.state,
                            microscope::VariableState::Available {
                                value: microscope::VariableValue::Scalar(ScalarValue::Signed(3)),
                                ..
                            }
                        ),
                        "{fixture}: stepping over stopped in relay's second call: {index:?}"
                    );
                }

                location = Some(here);
                if function.as_deref() == Some("main") {
                    break;
                }
            }

            // Returning into the middle of the call's line, a step may stop
            // there if the compiler marks it a statement, or at the next.
            let location = location.expect("a step was taken");
            let line = location_line(&location);
            assert!(
                [line_of("RELAY_CALL"), line_of("AFTER_RELAY")]
                    .into_iter()
                    .any(|expected| line == Some(expected)),
                "{fixture} {kind:?} returned to {location:?}"
            );
            assert_eq!(
                scenario.resume_to_stop().await,
                StopReason::Exited(ExitStatus::Code(0)),
                "{fixture}"
            );
            scenario.shutdown().await;
        }
    }
}

/// A step over that returns through the frame it returned to goes on from
/// that frame's caller. The golden runtime's `rt_start` calls `rt_exit_group`
/// at the depth `main` was, with no statement between, so stepping over
/// `main`'s last call must not stop in `rt_exit_group` as though it were
/// `main`.
#[tokio::test]
async fn a_step_over_does_not_stop_in_a_new_frame_where_a_returned_one_was() {
    let program = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("build/golden/threads/threads-clang-O0");
    let line = source_line(
        "tests/golden/threads/threads.c",
        "return (int)(rt_load(&counter) % 100);",
    );
    let mut scenario = Scenario::new("step over through main's return", program);
    scenario.add_source_breakpoint("threads.c", line).await;
    assert!(matches!(
        scenario
            .run_with_to_stop(LaunchOptions {
                arguments: vec!["1".into(), "main".into()],
                ..LaunchOptions::default()
            })
            .await,
        StopReason::Breakpoint { .. }
    ));
    scenario
        .operation(
            "remove the breakpoint",
            scenario.handle().remove_all_breakpoints(),
        )
        .await;
    assert_eq!(
        scenario.step_to_stop(StepKind::IntoSource).await,
        StopReason::Step {
            kind: StepKind::IntoSource
        }
    );
    let location = scenario
        .operation("location in rt_load", scenario.handle().current_location())
        .await;
    assert_eq!(
        location_function(&location),
        Some("rt_load"),
        "the golden binary changed"
    );

    // The rest of main and rt_start has no statement before the program
    // exits its group.
    let reason = scenario.step_to_stop(StepKind::OverSource).await;
    if reason != StopReason::Exited(ExitStatus::Code(60)) {
        let location = scenario
            .operation("location", scenario.handle().current_location())
            .await;
        assert_ne!(
            location_function(&location),
            Some("rt_exit_group"),
            "stepping over stopped in a new frame: {reason:?} at {location:?}"
        );
    }

    scenario.shutdown().await;
}

/// A step out to code without debug information runs on until a stop the
/// user sees, which a hit its hit condition declines is not.
#[tokio::test]
async fn a_step_out_to_undescribed_code_does_not_end_at_a_declined_hit() {
    let mut scenario = Scenario::launch("undescribed-caller");
    let breakpoint = scenario.add_breakpoint("callback").await;
    assert!(matches!(
        scenario.run_to_stop().await,
        StopReason::Breakpoint { .. }
    ));

    // The first hit came through the assembly; the second, from main, is
    // declined.
    scenario
        .operation(
            "decline later hits",
            scenario.handle().set_breakpoint_hit_condition(
                breakpoint.id,
                Some(
                    microscope::HitCondition::new(microscope::HitComparison::GreaterOrEqual, 3)
                        .expect("a valid hit condition"),
                ),
            ),
        )
        .await;
    assert_eq!(
        scenario.step_to_stop(StepKind::Out).await,
        StopReason::Exited(ExitStatus::Code(0))
    );
    scenario.shutdown().await;
}

/// Stepping out to a return address no line describes goes on, by single
/// steps, to the caller's first instruction a line describes. Clang marks
/// the code after `main`'s call of `orphan_spawn` in the golden `frames`
/// program as line 0.
#[tokio::test]
async fn a_step_out_to_undescribed_code_in_the_caller_stops_in_the_caller() {
    let program = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("build/golden/frames/frames-clang-O2-nofp");
    let mut scenario = Scenario::new("step out to line 0", program);
    scenario.add_breakpoint("orphan_spawn").await;
    assert!(matches!(
        scenario.run_to_stop().await,
        StopReason::Breakpoint { .. }
    ));
    scenario
        .operation(
            "remove the breakpoint",
            scenario.handle().remove_all_breakpoints(),
        )
        .await;
    assert_eq!(
        scenario.step_to_stop(StepKind::Out).await,
        StopReason::Step {
            kind: StepKind::Out
        }
    );
    let location = scenario
        .operation("location", scenario.handle().current_location())
        .await;
    assert_eq!(location_function(&location), Some("main"), "{location:?}");
    assert!(location.image.source.is_some(), "{location:?}");
    scenario.shutdown().await;
}

#[tokio::test]
async fn an_explicit_user_breakpoint_at_an_epilogue_marker_remains_visible() {
    let fixture = "stepping-boundaries-clang-o2";
    let mut scenario = Scenario::new("explicit epilogue breakpoint", Scenario::fixture(fixture));
    let marker = *epilogue_markers(&scenario, "marked_returns")
        .iter()
        .max()
        .expect("negative return path marker");
    scenario.add_breakpoint("marked_returns").await;
    assert!(matches!(
        scenario.run_to_stop().await,
        StopReason::Breakpoint { .. }
    ));
    let entry = scenario
        .operation(
            "marked function entry",
            scenario.handle().current_location(),
        )
        .await;
    let marker = relocate_image_address(marker, &entry);
    scenario
        .add_breakpoint_spec(microscope::BreakpointSpec::Address(marker))
        .await;

    assert_eq!(
        support::breakpoint_address(&scenario.resume_to_stop().await),
        marker,
        "the internal exit policy hid an explicit user breakpoint"
    );
    scenario.shutdown().await;
}

#[tokio::test]
async fn next_from_an_inline_frame_crosses_a_tail_call_to_the_true_caller() {
    for fixture in ["tail-calls-gcc-o2", "tail-calls-clang-o2"] {
        for (function, inline_function, tail_line, caller_line, expected_sink) in [
            ("outer_tail", "inline_tail", 22, 81, 20),
            ("outer_chain", "inline_chain", 32, 82, 21),
        ] {
            let mut scenario = Scenario::new(
                format!("tail-call next {fixture} {function}"),
                Scenario::fixture(fixture),
            );
            scenario.add_breakpoint(function).await;
            assert!(matches!(
                scenario.run_to_stop().await,
                StopReason::Breakpoint { .. }
            ));
            enter_inline_frame(&mut scenario, fixture, inline_function, tail_line).await;

            let stop =
                boundary_source_step(&mut scenario, StepKind::OverSource, "next across tail call")
                    .await;
            assert_eq!(
                location_function(&stop),
                Some("main"),
                "{fixture} {function} next stopped inside the tail-called function: {stop:?}"
            );
            let line = location_line(&stop).expect("tail-call next stop has caller source");
            assert!(
                (caller_line..=caller_line + 1).contains(&line),
                "{fixture} {function} completed at unexpected main line {line}"
            );
            let sink = fixture_symbol_address(&scenario, &stop, "tail_sink");
            assert_eq!(
                boundary_sink_value(&scenario, sink).await,
                expected_sink,
                "{fixture} {function} stopped before the tail-called work finished"
            );

            assert_eq!(
                scenario.resume_to_stop().await,
                StopReason::Exited(ExitStatus::Code(0)),
                "{fixture} {function}"
            );
            assert_eq!(scenario.shutdown().await, Some(ExitStatus::Code(0)));
        }
    }
}

#[tokio::test]
async fn finish_from_an_inline_frame_crosses_its_parents_tail_call() {
    for fixture in ["tail-calls-gcc-o2", "tail-calls-clang-o2"] {
        let mut scenario = Scenario::new(
            format!("tail-call finish {fixture}"),
            Scenario::fixture(fixture),
        );
        scenario.add_breakpoint("outer_tail").await;
        assert!(matches!(
            scenario.run_to_stop().await,
            StopReason::Breakpoint { .. }
        ));
        enter_inline_frame(&mut scenario, fixture, "inline_tail", 21).await;

        let stop =
            boundary_source_step(&mut scenario, StepKind::Out, "finish across tail call").await;
        assert_eq!(
            location_function(&stop),
            Some("main"),
            "{fixture} finish stopped inside the tail-called function: {stop:?}"
        );
        let line = location_line(&stop).expect("tail-call finish stop has caller source");
        assert!(
            (81..=82).contains(&line),
            "{fixture} finish completed at unexpected main line {line}"
        );
        let sink = fixture_symbol_address(&scenario, &stop, "tail_sink");
        assert_eq!(
            boundary_sink_value(&scenario, sink).await,
            20,
            "{fixture} finish stopped before the tail-called work finished"
        );

        assert_eq!(
            scenario.resume_to_stop().await,
            StopReason::Exited(ExitStatus::Code(0)),
            "{fixture}"
        );
        assert_eq!(scenario.shutdown().await, Some(ExitStatus::Code(0)));
    }
}

#[tokio::test]
async fn recursive_tail_call_completion_ignores_inner_frames_at_the_shared_return_site() {
    for fixture in ["tail-calls-gcc-o2", "tail-calls-clang-o2"] {
        let mut scenario = Scenario::new(
            format!("recursive tail-call next {fixture}"),
            Scenario::fixture(fixture),
        );

        // The first descend_tail activation (value == 2) tail-calls
        // mutual_tail(1), whose inner recursion returns through the same
        // code address as this step's own return site. Only the outer
        // return, distinguished by the stack pointer, may complete the step.
        scenario.add_breakpoint("descend_tail").await;
        assert!(matches!(
            scenario.run_to_stop().await,
            StopReason::Breakpoint { .. }
        ));
        scenario.remove_all_breakpoints().await;
        enter_inline_frame(&mut scenario, fixture, "inline_descend", 44).await;

        let stop = boundary_source_step(
            &mut scenario,
            StepKind::OverSource,
            "next across recursive tail call",
        )
        .await;
        assert_eq!(
            location_function(&stop),
            Some("mutual_tail"),
            "{fixture}: {stop:?}"
        );
        let line = location_line(&stop).expect("recursive tail-call stop has caller source");
        assert!(
            (55..=57).contains(&line),
            "{fixture} completed at unexpected mutual_tail line {line}"
        );
        let probe = fixture_symbol_address(&scenario, &stop, "tail_probe");
        assert_eq!(
            boundary_sink_value(&scenario, probe).await,
            1,
            "{fixture} completed in an inner recursive frame instead of the starting caller"
        );

        assert_eq!(
            scenario.resume_to_stop().await,
            StopReason::Exited(ExitStatus::Code(0)),
            "{fixture}"
        );
        assert_eq!(scenario.shutdown().await, Some(ExitStatus::Code(0)));
    }
}

#[tokio::test]
async fn next_from_an_inline_frame_runs_regular_callees_at_full_speed() {
    for fixture in ["tail-calls-gcc-o2", "tail-calls-clang-o2"] {
        let mut scenario = Scenario::new(
            format!("inline regular-call next {fixture}"),
            Scenario::fixture(fixture),
        );
        scenario.add_breakpoint("outer_over_call").await;
        assert!(matches!(
            scenario.run_to_stop().await,
            StopReason::Breakpoint { .. }
        ));
        enter_inline_frame(&mut scenario, fixture, "inline_over_call", 70).await;

        let stop = boundary_source_step(
            &mut scenario,
            StepKind::OverSource,
            "next over long-running regular call",
        )
        .await;
        assert_eq!(location_function(&stop), Some("inline_over_call"));
        assert_eq!(location_line(&stop), Some(71));
        let counter = fixture_symbol_address(&scenario, &stop, "tail_counter");
        assert_eq!(
            boundary_sink_value(&scenario, counter).await,
            200_000,
            "{fixture} stopped before the regular callee completed"
        );

        assert_eq!(
            scenario.resume_to_stop().await,
            StopReason::Exited(ExitStatus::Code(0)),
            "{fixture}"
        );
        assert_eq!(scenario.shutdown().await, Some(ExitStatus::Code(0)));
    }
}

#[tokio::test]
async fn finish_from_an_inline_frame_runs_regular_callees_at_full_speed() {
    for fixture in ["tail-calls-gcc-o2", "tail-calls-clang-o2"] {
        let mut scenario = Scenario::new(
            format!("inline regular-call finish {fixture}"),
            Scenario::fixture(fixture),
        );
        scenario.add_breakpoint("outer_over_call").await;
        assert!(matches!(
            scenario.run_to_stop().await,
            StopReason::Breakpoint { .. }
        ));
        enter_inline_frame(&mut scenario, fixture, "inline_over_call", 70).await;

        let stop = boundary_source_step(
            &mut scenario,
            StepKind::Out,
            "finish through long-running regular call",
        )
        .await;
        assert_eq!(location_function(&stop), Some("outer_over_call"));
        let line = location_line(&stop).expect("regular-call finish stop has parent source");
        assert!(
            (77..=78).contains(&line),
            "{fixture} finish completed at unexpected outer_over_call line {line}"
        );
        let counter = fixture_symbol_address(&scenario, &stop, "tail_counter");
        assert_eq!(
            boundary_sink_value(&scenario, counter).await,
            200_000,
            "{fixture} stopped before the regular callee completed"
        );

        assert_eq!(
            scenario.resume_to_stop().await,
            StopReason::Exited(ExitStatus::Code(0)),
            "{fixture}"
        );
        assert_eq!(scenario.shutdown().await, Some(ExitStatus::Code(0)));
    }
}

#[tokio::test]
async fn zig_o0_steps_through_inline_code_and_unwinds_logical_and_physical_frames() {
    let fixture = "stepping-boundaries-zig-o0";
    let mut scenario = Scenario::launch(fixture);
    scenario
        .add_source_breakpoint("stepping-boundaries.zig", 29)
        .await;
    assert!(matches!(
        scenario.run_to_stop().await,
        StopReason::Breakpoint { .. }
    ));
    assert_eq!(
        scenario.step_to_stop(StepKind::OverSource).await,
        StopReason::Step {
            kind: StepKind::OverSource
        }
    );
    assert_eq!(
        scenario.step_to_stop(StepKind::IntoSource).await,
        StopReason::Step {
            kind: StepKind::IntoSource
        }
    );
    let inline = scenario
        .operation("Zig inline location", scenario.handle().current_location())
        .await;
    assert_eq!(location_function(&inline), Some("inlineAdjust"));
    assert_eq!(location_line(&inline), Some(30));

    let trace = scenario
        .operation("Zig inline backtrace", scenario.handle().backtrace())
        .await;
    let names = trace
        .frames
        .iter()
        .filter_map(|frame| frame.function.as_ref())
        .map(|function| function.name.as_ref())
        .collect::<Vec<_>>();
    assert!(names.starts_with(&["inlineAdjust", "main"]), "{trace:?}");

    assert_eq!(
        scenario.step_to_stop(StepKind::Out).await,
        StopReason::Step {
            kind: StepKind::Out
        }
    );
    let caller = scenario
        .operation("Zig inline caller", scenario.handle().current_location())
        .await;
    assert_eq!(location_function(&caller), Some("main"));
    assert_eq!(
        scenario.resume_to_stop().await,
        StopReason::Exited(ExitStatus::Code(0))
    );
    assert_eq!(scenario.shutdown().await, Some(ExitStatus::Code(0)));
}

#[tokio::test]
async fn optimized_zig_steps_into_and_finishes_a_physical_call() {
    let fixture = "stepping-boundaries-zig-o2";
    let mut scenario = Scenario::launch(fixture);
    scenario
        .add_source_breakpoint("stepping-boundaries.zig", 29)
        .await;
    assert!(matches!(
        scenario.run_to_stop().await,
        StopReason::Breakpoint { .. }
    ));
    assert_eq!(
        scenario.step_to_stop(StepKind::OverSource).await,
        StopReason::Step {
            kind: StepKind::OverSource
        }
    );
    assert_eq!(
        scenario.step_to_stop(StepKind::IntoSource).await,
        StopReason::Step {
            kind: StepKind::IntoSource
        }
    );
    let entered = scenario
        .operation("optimized Zig callee", scenario.handle().current_location())
        .await;
    assert_eq!(location_function(&entered), Some("markedReturns"));

    let trace = scenario
        .operation("optimized Zig backtrace", scenario.handle().backtrace())
        .await;
    let names = trace
        .frames
        .iter()
        .filter_map(|frame| frame.function.as_ref())
        .map(|function| function.name.as_ref())
        .collect::<Vec<_>>();
    assert!(names.starts_with(&["markedReturns", "main"]), "{trace:?}");

    assert_eq!(
        scenario.step_to_stop(StepKind::Out).await,
        StopReason::Step {
            kind: StepKind::Out
        }
    );
    let returned = scenario
        .operation("optimized Zig caller", scenario.handle().current_location())
        .await;
    assert_eq!(location_function(&returned), Some("main"));
    assert_eq!(
        scenario.resume_to_stop().await,
        StopReason::Exited(ExitStatus::Code(0))
    );
    assert_eq!(scenario.shutdown().await, Some(ExitStatus::Code(0)));
}

#[tokio::test]
async fn virtual_steps_reveal_inline_frames_without_running_the_inferior() {
    for fixture in ["inline-gcc-o2", "inline-clang-o2"] {
        let mut scenario = Scenario::launch(fixture);
        scenario.add_breakpoint("caller").await;

        assert!(matches!(
            scenario.run_to_stop().await,
            StopReason::Breakpoint { .. }
        ));
        let initial = scenario.snapshot().await;
        let instruction = scenario
            .operation("caller location", scenario.handle().current_location())
            .await;
        assert_inline_location(fixture, &instruction, "caller", 28, instruction.address);

        let mut events = scenario.handle().subscribe();
        assert_eq!(
            scenario.step_to_stop(StepKind::IntoSource).await,
            StopReason::Step {
                kind: StepKind::IntoSource
            }
        );
        let middle = scenario
            .operation("middle location", scenario.handle().current_location())
            .await;
        assert_inline_location(fixture, &middle, "middle", 14, instruction.address);
        assert_no_continued_event(fixture, &mut events);

        let mut events = scenario.handle().subscribe();
        scenario.step_to_stop(StepKind::IntoSource).await;
        let leaf = scenario
            .operation("leaf location", scenario.handle().current_location())
            .await;
        assert_inline_location(fixture, &leaf, "leaf", 7, instruction.address);
        assert_no_continued_event(fixture, &mut events);
        assert_ne!(
            initial.stop_id,
            scenario.snapshot().await.stop_id,
            "{fixture}"
        );

        let trace = scenario
            .operation("inline backtrace", scenario.handle().backtrace())
            .await;
        assert_inline_backtrace(fixture, &trace);

        // Finishing the inline instances leaves the physical frame.
        assert_eq!(
            scenario.step_to_stop(StepKind::Out).await,
            StopReason::Step {
                kind: StepKind::Out
            },
            "{fixture}"
        );
        let location = scenario
            .operation(
                "location after finish",
                scenario.handle().current_location(),
            )
            .await;
        assert_eq!(location_function(&location), Some("caller"), "{fixture}");
        assert_eq!(location_line(&location), Some(29), "{fixture}");

        scenario.shutdown().await;
    }
}

#[tokio::test]
async fn next_skips_inline_descendants_of_the_selected_caller() {
    for fixture in ["inline-gcc-o2", "inline-clang-o2"] {
        let mut scenario = Scenario::launch(fixture);
        scenario.add_breakpoint("caller").await;
        scenario.run_to_stop().await;

        assert_eq!(
            scenario.step_to_stop(StepKind::OverSource).await,
            StopReason::Step {
                kind: StepKind::OverSource
            },
            "{fixture}"
        );
        let location = scenario
            .operation(
                "location after inline next",
                scenario.handle().current_location(),
            )
            .await;
        assert_eq!(location_function(&location), Some("caller"), "{fixture}");
        assert_eq!(location_line(&location), Some(29), "{fixture}");

        scenario.shutdown().await;
    }
}

#[tokio::test]
async fn inline_next_is_owned_by_the_selected_thread() {
    for fixture in ["inline-threads-gcc-o2", "inline-threads-clang-o2"] {
        let mut scenario = Scenario::launch(fixture);
        scenario.add_breakpoint("thread_caller").await;
        scenario.run_to_stop().await;

        // The other worker runs during the step; without the breakpoint it
        // runs the same code, reaching the step's internal breakpoints.
        scenario.remove_all_breakpoints().await;
        let before = scenario.snapshot().await;
        let selected = before
            .selected
            .and_then(microscope::ExecutionContext::as_thread)
            .expect("selected worker thread");

        assert_eq!(
            scenario.step_to_stop(StepKind::OverSource).await,
            StopReason::Step {
                kind: StepKind::OverSource
            },
            "{fixture}"
        );
        let after = scenario.snapshot().await;
        let location = scenario
            .operation(
                "thread inline next location",
                scenario.handle().current_location(),
            )
            .await;

        assert_eq!(
            after
                .selected
                .and_then(microscope::ExecutionContext::as_thread),
            Some(selected),
            "{fixture}"
        );
        assert_eq!(
            location_function(&location),
            Some("thread_caller"),
            "{fixture}"
        );
        assert_eq!(location_line(&location), Some(19), "{fixture}");
        scenario.shutdown().await;
    }
}

#[tokio::test]
async fn instruction_step_moves_the_pc_before_rebuilding_inline_presentation() {
    for fixture in ["inline-gcc-o2", "inline-clang-o2"] {
        let mut scenario = Scenario::launch(fixture);
        scenario.add_breakpoint("caller").await;
        scenario.run_to_stop().await;
        let before = scenario
            .operation(
                "location before stepi",
                scenario.handle().current_location(),
            )
            .await;

        assert_eq!(
            scenario.step_to_stop(StepKind::Instruction).await,
            StopReason::Step {
                kind: StepKind::Instruction
            },
            "{fixture}"
        );
        let after = scenario
            .operation("location after stepi", scenario.handle().current_location())
            .await;
        let snapshot = scenario.snapshot().await;

        assert_ne!(after.address, before.address, "{fixture}");
        assert_eq!(
            snapshot
                .presentation
                .as_ref()
                .map(|presentation| presentation.instruction),
            Some(after.address),
            "{fixture}"
        );

        scenario.shutdown().await;
    }
}

#[tokio::test]
async fn instruction_step_explicitly_delivers_a_pending_signal() {
    let mut scenario = Scenario::new("step pending signal", Scenario::fixture("signals"));
    scenario.add_breakpoint("signal_point").await;
    scenario.run_to_stop().await;
    assert!(matches!(
        scenario.resume_to_stop().await,
        StopReason::Exception(exception) if exception.code == 10
    ));

    assert_eq!(
        scenario.step_to_stop(StepKind::Instruction).await,
        StopReason::Step {
            kind: StepKind::Instruction
        }
    );
    assert!(matches!(
        scenario.resume_to_stop().await,
        StopReason::Exception(exception) if exception.code == 5
    ));
    assert_eq!(
        scenario.resume_to_stop().await,
        StopReason::Exited(ExitStatus::Code(0))
    );

    scenario.shutdown().await;
}

#[tokio::test]
async fn instruction_step_executes_the_instruction_hidden_by_a_breakpoint() {
    let mut scenario = Scenario::new("instruction step", Scenario::fixture("basic"));
    scenario.add_breakpoint("breakpoint_target").await;
    let StopReason::Breakpoint {
        address: Some(address),
        ..
    } = scenario.run_to_stop().await
    else {
        panic!("expected breakpoint")
    };

    let disassembly = scenario
        .operation(
            "disassemble breakpoint_target",
            scenario.handle().disassemble(microscope::DisassemblyQuery {
                range: microscope::DisassemblyRange::Function(address),
                syntax: microscope::AssemblySyntax::Intel,
            }),
        )
        .await;
    let microscope::DisassemblyView::Function { blocks, .. } = disassembly.view else {
        panic!("a function query returned a window");
    };

    let hidden = blocks
        .iter()
        .flat_map(|block| block.instructions.iter())
        .find(|instruction| instruction.address == address)
        .expect("the breakpoint is on an instruction");

    assert_eq!(
        scenario.step_to_stop(StepKind::Instruction).await,
        StopReason::Step {
            kind: StepKind::Instruction
        }
    );

    // The original instruction ran, not the trap: execution continues
    // after it rather than one byte past the breakpoint.
    let registers = scenario
        .operation("registers after step", scenario.handle().registers())
        .await;
    assert_eq!(
        register_u64(&registers, RegisterRole::ProgramCounter),
        hidden.end().get(),
        "{hidden:?}"
    );
    assert!(hidden.end().get() > address.get() + 1, "{hidden:?}");

    assert!(matches!(
        scenario.resume_to_stop().await,
        StopReason::Breakpoint { .. }
    ));
    scenario.shutdown().await;
}

#[tokio::test]
async fn a_user_breakpoint_interrupts_finish_at_a_shared_site() {
    let mut scenario = Scenario::new("shared plan breakpoint", Scenario::fixture("unwind-o0"));
    scenario.add_breakpoint("deepest").await;
    scenario.run_to_stop().await;

    let trace = scenario
        .operation("backtrace", scenario.handle().backtrace())
        .await;
    let return_address = trace.frames[1]
        .instruction
        .expect("a thread's frame has an instruction");
    let breakpoint = scenario
        .operation(
            "add breakpoint at return address",
            scenario
                .handle()
                .add_breakpoint(microscope::BreakpointSpec::Address(return_address)),
        )
        .await;
    assert_eq!(breakpoint.locations.len(), 1);
    assert_eq!(
        breakpoint.locations[0].location,
        BreakpointLocation::Virtual(return_address)
    );

    assert_eq!(
        support::breakpoint_address(&scenario.step_to_stop(StepKind::Out).await),
        return_address
    );
    assert_eq!(
        scenario.resume_to_stop().await,
        StopReason::Exited(ExitStatus::Code(1))
    );

    scenario.shutdown().await;
}

#[tokio::test]
async fn source_next_steps_over_calls_but_preserves_user_breakpoints() {
    let mut step_over = Scenario::new("next over call", Scenario::fixture("unwind-o0"));
    step_over.add_breakpoint("middle").await;
    step_over.run_to_stop().await;

    assert_eq!(
        step_over.step_to_stop(StepKind::OverSource).await,
        StopReason::Step {
            kind: StepKind::OverSource
        }
    );
    let location = step_over
        .operation("location after next", step_over.handle().current_location())
        .await;
    assert_eq!(location_function(&location), Some("middle"));
    assert_eq!(
        location_line(&location),
        Some(12),
        "next ran the call on line 11 to its return"
    );
    step_over.shutdown().await;

    let mut interrupted = Scenario::new("next interruption", Scenario::fixture("unwind-o0"));
    interrupted.add_breakpoint("middle").await;
    interrupted.add_breakpoint("deepest").await;
    interrupted.run_to_stop().await;

    assert!(matches!(
        interrupted.step_to_stop(StepKind::OverSource).await,
        StopReason::Breakpoint { .. }
    ));
    let location = interrupted
        .operation(
            "location after interrupted next",
            interrupted.handle().current_location(),
        )
        .await;
    assert_eq!(location_function(&location), Some("deepest"));

    interrupted.shutdown().await;
}

#[tokio::test]
async fn source_steps_skip_non_statement_line_rows() {
    // GCC at -O2 marks the trailing rows of middle (line 13) and deepest
    // (line 8) as non-statement rows, where source steps must not stop.
    let mut next = Scenario::new("next unwind-o2", Scenario::fixture("unwind-o2"));
    next.add_breakpoint("middle").await;
    next.run_to_stop().await;

    assert_eq!(
        next.step_to_stop(StepKind::OverSource).await,
        StopReason::Step {
            kind: StepKind::OverSource
        }
    );
    let location = next
        .operation(
            "location after first next",
            next.handle().current_location(),
        )
        .await;
    assert_eq!(location_line(&location), Some(12));

    assert_eq!(
        next.step_to_stop(StepKind::OverSource).await,
        StopReason::Step {
            kind: StepKind::OverSource
        }
    );
    let location = next
        .operation(
            "location after second next",
            next.handle().current_location(),
        )
        .await;
    assert_eq!(
        location_function(&location),
        Some("outer"),
        "next stopped on a non-statement row instead of finishing middle"
    );
    next.shutdown().await;

    let mut step = Scenario::new("step unwind-o2", Scenario::fixture("unwind-o2"));
    step.add_breakpoint("deepest").await;
    step.run_to_stop().await;

    assert_eq!(
        step.step_to_stop(StepKind::IntoSource).await,
        StopReason::Step {
            kind: StepKind::IntoSource
        }
    );
    let location = step
        .operation(
            "location after first step",
            step.handle().current_location(),
        )
        .await;
    assert_eq!(location_line(&location), Some(7));

    assert_eq!(
        step.step_to_stop(StepKind::IntoSource).await,
        StopReason::Step {
            kind: StepKind::IntoSource
        }
    );
    let location = step
        .operation(
            "location after second step",
            step.handle().current_location(),
        )
        .await;
    assert_eq!(
        location_function(&location),
        Some("middle"),
        "step stopped on a non-statement row instead of returning to middle"
    );

    // The return address in middle sits on a non-statement row for the
    // already-executed call line (11); the step must continue to the next
    // statement row even though the activation changed at the return.
    assert_eq!(
        location_line(&location),
        Some(12),
        "step completed on a non-statement row after the activation changed"
    );
    step.shutdown().await;
}

#[tokio::test]
async fn step_into_crosses_library_calls_without_line_info() {
    // Line 6 calls getpid() through the PLT, which has no line rows and whose
    // call-frame information is a DWARF expression.
    let mut scenario = Scenario::new("step over libc", Scenario::fixture("step-over-libc"));
    scenario.add_breakpoint("call_libc").await;
    scenario.run_to_stop().await;

    assert_eq!(
        scenario.step_to_stop(StepKind::IntoSource).await,
        StopReason::Step {
            kind: StepKind::IntoSource
        }
    );
    let location = scenario
        .operation(
            "location after library step",
            scenario.handle().current_location(),
        )
        .await;
    assert_eq!(location_function(&location), Some("call_libc"));
    assert_eq!(location_line(&location), Some(7));

    assert_eq!(
        scenario.resume_to_stop().await,
        StopReason::Exited(ExitStatus::Code(0))
    );
    scenario.shutdown().await;
}

/// Stops at the `syscall` instruction of the C library's `getpid`, found by
/// disassembling the function its ELF symbol names.
async fn stop_at_getpid_system_call(scenario: &mut Scenario) -> VirtualAddress {
    scenario.add_breakpoint("call_libc").await;
    scenario.run_to_stop().await;
    let modules = scenario
        .operation("modules", scenario.handle().loaded_modules())
        .await;
    let libc = modules
        .modules
        .iter()
        .find(|record| {
            record
                .path
                .file_name()
                .is_some_and(|name| name == "libc.so.6")
        })
        .expect("libc is loaded");
    let image = scenario
        .operation(
            "libc image",
            scenario.handle().loaded_module_image(libc.module.id),
        )
        .await;
    let getpid = image
        .symbols()
        .find(|symbol| symbol.name() == "getpid")
        .expect("libc defines getpid");
    let disassembly = scenario
        .operation(
            "disassemble getpid",
            scenario.handle().disassemble(microscope::DisassemblyQuery {
                range: microscope::DisassemblyRange::Function(VirtualAddress::new(
                    libc.module.load_bias.get() + getpid.address().get(),
                )),
                syntax: microscope::AssemblySyntax::Intel,
            }),
        )
        .await;
    let microscope::DisassemblyView::Function { blocks, .. } = disassembly.view else {
        panic!("a function query returned a window");
    };

    let syscall = blocks
        .iter()
        .flat_map(|block| block.instructions.iter())
        .find(|instruction| {
            matches!(
                &instruction.content,
                microscope::InstructionContent::Decoded(decoded)
                    if decoded.mnemonic() == Some("syscall")
            )
        })
        .expect("getpid issues a system call")
        .address;
    scenario
        .add_breakpoint_spec(BreakpointSpec::Address(syscall))
        .await;
    assert_eq!(
        support::breakpoint_address(&scenario.resume_to_stop().await),
        syscall
    );
    syscall
}

#[tokio::test]
async fn instruction_steps_and_breakpoint_repairs_cross_system_calls() {
    // Linux reports a single step across `syscall` from the system call's
    // exit path, with a different trap code than other steps.
    let mut scenario = Scenario::new("step syscall", Scenario::fixture("step-over-libc"));
    let syscall = stop_at_getpid_system_call(&mut scenario).await;
    assert_eq!(
        scenario.step_to_stop(StepKind::Instruction).await,
        StopReason::Step {
            kind: StepKind::Instruction
        }
    );
    let registers = scenario
        .operation("registers", scenario.handle().registers())
        .await;
    assert_eq!(
        register_u64(&registers, RegisterRole::ProgramCounter),
        syscall.get() + 2
    );
    let rax = registers
        .registers
        .iter()
        .find(|value| value.register.name.as_ref() == "rax")
        .expect("rax");
    assert_eq!(
        rax.bytes.as_deref(),
        Some(
            &registers
                .context
                .as_thread()
                .expect("a thread")
                .get()
                .to_le_bytes()[..]
        )
    );
    assert_eq!(
        scenario.resume_to_stop().await,
        StopReason::Exited(ExitStatus::Code(0))
    );
    scenario.shutdown().await;

    // Resuming from a breakpoint on the system call steps over it the same
    // way before running on.
    let mut scenario = Scenario::new("repair syscall", Scenario::fixture("step-over-libc"));
    stop_at_getpid_system_call(&mut scenario).await;
    assert_eq!(
        scenario.resume_to_stop().await,
        StopReason::Exited(ExitStatus::Code(0))
    );
    scenario.shutdown().await;
}

#[tokio::test]
async fn next_instruction_runs_a_recursive_call_until_this_activation_returns() {
    let call_line = source_line(
        "tests/fixtures/c/frames.c",
        "int64_t below = frames_recurse(depth - 1, seed);",
    );
    let mut scenario = Scenario::launch("frames-gcc-o0");
    scenario.add_source_breakpoint("frames.c", call_line).await;
    assert!(matches!(
        scenario.run_to_stop().await,
        StopReason::Breakpoint { .. }
    ));
    scenario.remove_all_breakpoints().await;
    let pc = |registers: &microscope::RegisterSnapshot| {
        register_u64(registers, RegisterRole::ProgramCounter)
    };

    // An ordinary instruction is one step, like stepi.
    let before = pc(&scenario
        .operation("registers", scenario.handle().registers())
        .await);
    assert_eq!(
        scenario.step_to_stop(StepKind::OverInstruction).await,
        StopReason::Step {
            kind: StepKind::OverInstruction
        }
    );
    let after = pc(&scenario
        .operation("registers", scenario.handle().registers())
        .await);
    assert!(
        after > before && after - before < 16,
        "{before:#x} -> {after:#x}"
    );

    // Reach the recursive call itself.
    let mut call = None;
    for _ in 0..16 {
        let address = pc(&scenario
            .operation("registers", scenario.handle().registers())
            .await);
        let opcode = scenario
            .operation(
                "opcode",
                scenario
                    .handle()
                    .read_memory(VirtualAddress::new(address), 1),
            )
            .await;
        if opcode.bytes.as_ref() == [0xe8] {
            call = Some(address);
            break;
        }

        scenario.step_to_stop(StepKind::Instruction).await;
    }

    let call = call.expect("a direct call follows the argument setup");
    let depth = scenario
        .operation("backtrace", scenario.handle().backtrace())
        .await
        .frames
        .len();

    // Deeper activations return to the same address first; only this
    // activation's return completes the step.
    assert_eq!(
        scenario.step_to_stop(StepKind::OverInstruction).await,
        StopReason::Step {
            kind: StepKind::OverInstruction
        }
    );
    assert_eq!(
        pc(&scenario
            .operation("registers", scenario.handle().registers())
            .await),
        call + 5
    );
    assert_eq!(
        scenario
            .operation("backtrace", scenario.handle().backtrace())
            .await
            .frames
            .len(),
        depth
    );
    assert_eq!(
        scenario.resume_to_stop().await,
        StopReason::Exited(ExitStatus::Code(0))
    );
    scenario.shutdown().await;
}

/// The line of `orphan-frames.c` that `marker` labels.
fn orphan_line(marker: &str) -> u64 {
    source_line("tests/fixtures/c/orphan-frames.c", marker)
}

async fn current_line(scenario: &Scenario) -> Option<u64> {
    scenario
        .operation("location", scenario.handle().current_location())
        .await
        .image
        .source
        .map(|source| source.line.get())
}

/// Steps `kind` and checks that it stops on the line `marker` labels.
async fn step_to_orphan_line(scenario: &mut Scenario, kind: StepKind, marker: &str) {
    assert_eq!(scenario.step_to_stop(kind).await, StopReason::Step { kind });
    assert_eq!(current_line(scenario).await, Some(orphan_line(marker)));
}

/// Stepping out of a frame whose caller cannot be unwound is refused, and
/// leaves the program stopped where it was.
async fn assert_finish_refused(scenario: &Scenario, marker: &str) {
    let refused = scenario
        .attempt("finish", scenario.handle().step(StepKind::Out))
        .await
        .expect_err("stepping out of a frame without a trustworthy caller is refused");
    assert!(
        refused.to_string().contains("caller"),
        "the refusal explains the caller is unknown: {refused}"
    );
    assert_eq!(current_line(scenario).await, Some(orphan_line(marker)));
}

/// A frame whose caller cannot be unwound still steps by source line, and
/// its program runs on unharmed. A fiber started on a fresh stack has no
/// readable return address; a frame whose return address a stack overflow
/// overwrote points into program data, where no trap may be planted.
#[tokio::test]
async fn frames_without_a_trustworthy_caller_step_and_leave_their_program_intact() {
    let mut scenario = Scenario::launch("orphan-frames");
    scenario.add_breakpoint("fiber_main").await;
    assert!(matches!(
        scenario.run_to_stop().await,
        StopReason::Breakpoint { .. }
    ));
    assert_eq!(
        current_line(&scenario).await,
        Some(orphan_line("FIBER_FIRST"))
    );
    step_to_orphan_line(&mut scenario, StepKind::IntoSource, "FIBER_SECOND").await;
    step_to_orphan_line(&mut scenario, StepKind::OverSource, "FIBER_THIRD").await;
    assert_finish_refused(&scenario, "FIBER_THIRD").await;
    step_to_orphan_line(&mut scenario, StepKind::OverSource, "FIBER_EXIT").await;
    assert_eq!(
        scenario.resume_to_stop().await,
        StopReason::Exited(ExitStatus::Code(0))
    );
    scenario.shutdown().await;

    let mut scenario = Scenario::new("orphan-frames smash", Scenario::fixture("orphan-frames"));
    scenario.add_breakpoint("smash").await;
    let options = LaunchOptions {
        arguments: vec!["smash".into()],
        ..LaunchOptions::default()
    };

    assert!(matches!(
        scenario.run_with_to_stop(options).await,
        StopReason::Breakpoint { .. }
    ));
    assert_eq!(
        current_line(&scenario).await,
        Some(orphan_line("FRAME_ADDRESS"))
    );
    step_to_orphan_line(&mut scenario, StepKind::OverSource, "OVERWRITE_RETURN").await;
    step_to_orphan_line(&mut scenario, StepKind::OverSource, "READ_CANARY").await;
    assert_finish_refused(&scenario, "READ_CANARY").await;
    step_to_orphan_line(&mut scenario, StepKind::OverSource, "CHECK_CANARY").await;

    // The program exits with 0 only if the data its smashed return address
    // points to still holds what it stored.
    assert_eq!(
        scenario.resume_to_stop().await,
        StopReason::Exited(ExitStatus::Code(0))
    );
    scenario.shutdown().await;
}

/// Stepping over from an instruction no debug information describes, such
/// as a program's entry point, runs to the first source statement it
/// reaches, as stepping in does, with or without call-frame information.
#[tokio::test]
async fn stepping_over_from_undescribed_code_stops_at_the_first_source_statement() {
    for fixture in ["freestanding-entry", "freestanding-entry-bare"] {
        let mut scenario = Scenario::launch(fixture);
        let entry = scenario
            .run_with_to_stop(LaunchOptions {
                stop_at_entry: true,
                ..LaunchOptions::default()
            })
            .await;
        assert_eq!(entry, StopReason::Entry);
        assert_eq!(
            scenario.step_to_stop(StepKind::OverSource).await,
            StopReason::Step {
                kind: StepKind::OverSource
            }
        );
        let location = scenario
            .operation("location", scenario.handle().current_location())
            .await;
        assert_eq!(location_function(&location), Some("main"));
        assert_eq!(
            location_line(&location),
            Some(source_line(
                "tests/fixtures/c/freestanding-entry.c",
                "FIRST_STATEMENT"
            ))
        );
        assert_eq!(
            scenario.resume_to_stop().await,
            StopReason::Exited(ExitStatus::Code(3))
        );
        scenario.shutdown().await;
    }
}

/// Breakpoints on two nested inlined functions that begin at one
/// instruction, `rt_exit_group` and the `rt_syscall3` inlined into it in the
/// golden runtime, hit together. One chain of calls holds both, so the stop
/// presents the innermost, as gdb does, and backtraces and source steps
/// work there.
#[tokio::test]
async fn nested_inline_breakpoints_hit_together_present_the_innermost() {
    let program = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("build/golden/straight/straight-clang-O2");
    let mut scenario = Scenario::new("nested-inline-breakpoints", program);
    scenario.add_breakpoint("rt_exit_group").await;
    scenario.add_breakpoint("rt_syscall3").await;
    let loop_head = VirtualAddress::new(0x40_1334);
    let mut reason = scenario.run_to_stop().await;
    for _ in 0..16 {
        if matches!(&reason, StopReason::Breakpoint { address, hits } if *address == Some(loop_head) && hits.len() == 2)
        {
            break;
        }

        reason = scenario.resume_to_stop().await;
    }

    assert!(
        matches!(&reason, StopReason::Breakpoint { address, hits } if *address == Some(loop_head) && hits.len() == 2),
        "the golden binary changed: {reason:?}"
    );
    let presentation = scenario
        .snapshot()
        .await
        .presentation
        .expect("a presentation");
    assert!(
        matches!(presentation.frame, microscope::PresentedFrame::Inline(_)),
        "{presentation:?}"
    );
    let backtrace = scenario
        .operation("backtrace", scenario.handle().backtrace())
        .await;
    let functions = backtrace
        .frames
        .iter()
        .take(2)
        .map(|frame| {
            frame
                .function
                .as_ref()
                .map(|function| function.name.as_ref())
        })
        .collect::<Vec<_>>();
    assert_eq!(functions, [Some("rt_syscall3"), Some("rt_exit_group")]);
    assert_eq!(
        scenario.step_to_stop(StepKind::OverSource).await,
        StopReason::Step {
            kind: StepKind::OverSource
        }
    );
    scenario.shutdown().await;
}

async fn launch_boundary_scenario(
    name: String,
    fixture: &str,
) -> (Scenario, microscope::ExecutionLocation) {
    let mut scenario = Scenario::new(name, Scenario::fixture(fixture));
    scenario.add_breakpoint("main").await;
    assert!(matches!(
        scenario.run_to_stop().await,
        StopReason::Breakpoint { .. }
    ));
    let main = scenario
        .operation("main activation", scenario.handle().current_location())
        .await;
    (scenario, main)
}

async fn boundary_source_step(
    scenario: &mut Scenario,
    kind: StepKind,
    operation: &str,
) -> microscope::ExecutionLocation {
    assert_eq!(
        scenario.step_to_stop(kind).await,
        StopReason::Step { kind },
        "{operation}"
    );
    scenario
        .operation(operation, scenario.handle().current_location())
        .await
}

fn fixture_symbol_address(
    scenario: &Scenario,
    location: &microscope::ExecutionLocation,
    symbol: &str,
) -> VirtualAddress {
    let address = scenario
        .handle()
        .module_image()
        .symbol_named(symbol)
        .unwrap_or_else(|error| panic!("missing fixture symbol {symbol}: {error}"))
        .address();
    relocate_image_address(address, location)
}

async fn boundary_sink_value(scenario: &Scenario, sink: VirtualAddress) -> u64 {
    scenario
        .operation("boundary sink", scenario.handle().read_word(sink))
        .await
        & u64::from(u32::MAX)
}

async fn advance_boundary_to_line(
    scenario: &mut Scenario,
    fixture: &str,
    target: u64,
) -> microscope::ExecutionLocation {
    for _ in 0..4 {
        let location = scenario
            .operation(
                "advance boundary call site",
                scenario.handle().current_location(),
            )
            .await;
        assert_eq!(location_function(&location), Some("main"));
        let line = location_line(&location).expect("main call site has source");
        if line == target {
            return location;
        }

        assert!(
            line < target,
            "{fixture} skipped target line {target} and stopped at {line}"
        );
        boundary_source_step(scenario, StepKind::OverSource, "advance boundary call site").await;
    }

    panic!("{fixture} did not reach main line {target} within the step budget");
}

async fn finish_boundary_physical_call(
    scenario: &mut Scenario,
    fixture: &str,
    main_physical: Option<microscope::CodeInstanceId>,
    sink: VirtualAddress,
    case: (u64, &str, u64, u64),
) {
    let (call_line, callee, expected_sink, last_caller_line) = case;
    advance_boundary_to_line(scenario, fixture, call_line).await;
    let entered =
        boundary_source_step(scenario, StepKind::IntoSource, "entered physical callee").await;
    assert_eq!(location_function(&entered), Some(callee));
    assert_ne!(entered.image.physical_instance, main_physical);

    let returned =
        boundary_source_step(scenario, StepKind::Out, "caller after physical finish").await;
    assert_eq!(location_function(&returned), Some("main"));
    assert_eq!(returned.image.physical_instance, main_physical);
    let line = location_line(&returned).expect("physical finish has caller source");
    assert!(
        (call_line..=last_caller_line).contains(&line),
        "{fixture} finished {callee} at unexpected line {line}"
    );
    assert_eq!(
        boundary_sink_value(scenario, sink).await,
        expected_sink,
        "{fixture} finished the wrong path through {callee}"
    );
}

/// Steps into a fixture function until the selected frame is the named
/// inline instance stopped at the requested source line.
async fn enter_inline_frame(scenario: &mut Scenario, fixture: &str, function: &str, line: u64) {
    for _ in 0..8 {
        let location = scenario
            .operation(
                "inline frame location",
                scenario.handle().current_location(),
            )
            .await;
        if location_function(&location) == Some(function) && location_line(&location) == Some(line)
        {
            return;
        }

        boundary_source_step(scenario, StepKind::IntoSource, "enter inline frame").await;
    }

    panic!("{fixture} did not reach {function}:{line} within the step budget");
}

async fn advance_to_boundary_inline_call(
    scenario: &mut Scenario,
    fixture: &str,
) -> microscope::ExecutionLocation {
    for _ in 0..2 {
        let location = scenario
            .operation("boundary inline call", scenario.handle().current_location())
            .await;
        let is_main_call = location
            .image
            .function
            .as_ref()
            .is_some_and(|function| function.name.as_ref() == "main")
            && location
                .image
                .source
                .as_ref()
                .is_some_and(|source| source.line.get() == 30);
        if is_main_call {
            return location;
        }

        assert_eq!(
            scenario.step_to_stop(StepKind::OverSource).await,
            StopReason::Step {
                kind: StepKind::OverSource
            },
            "{fixture}"
        );
    }

    panic!("{fixture} did not reach the inline_adjust call in main");
}

fn epilogue_markers(scenario: &Scenario, function: &str) -> BTreeSet<microscope::ImageAddress> {
    let image = scenario.handle().module_image();
    let function = image.function_named(function).expect("marked function");
    let instance = image
        .instances_for_function(function.id())
        .find(|instance| matches!(instance.kind(), CodeInstanceKind::OutOfLine))
        .expect("physical marked function");
    image
        .statement_rows()
        .filter(|row| row.flags.epilogue_begin() && instance.contains(row.address))
        .map(|row| row.address)
        .collect()
}

fn assert_inline_location(
    fixture: &str,
    location: &microscope::ExecutionLocation,
    function: &str,
    line: u64,
    address: VirtualAddress,
) {
    assert_eq!(location.address, address, "{fixture}");
    assert_eq!(location_function(location), Some(function), "{fixture}");
    assert_eq!(location_line(location), Some(line), "{fixture}");
}

fn assert_no_continued_event(
    fixture: &str,
    events: &mut tokio::sync::broadcast::Receiver<microscope::DebuggerEvent>,
) {
    assert!(
        std::iter::from_fn(|| events.try_recv().ok())
            .all(|event| !matches!(event, microscope::DebuggerEvent::InferiorContinued { .. })),
        "{fixture} virtual step emitted InferiorContinued"
    );
}

fn assert_inline_backtrace(fixture: &str, trace: &microscope::Backtrace) {
    let frames: Vec<_> = trace
        .frames
        .iter()
        .filter_map(|frame| {
            frame
                .function
                .as_ref()
                .map(|function| (frame, function.name.as_ref()))
        })
        .collect();

    assert_eq!(
        frames
            .iter()
            .take(4)
            .map(|(_, name)| *name)
            .collect::<Vec<_>>(),
        ["leaf", "middle", "caller", "main"],
        "unexpected {fixture} frames: {trace:?}"
    );
    assert!(frames[..2].iter().all(|(frame, _)| {
        frame.kind == microscope::FrameKind::Inline && frame.code_instance.is_some()
    }));
    assert_eq!(frames[2].0.kind, microscope::FrameKind::Physical);
    assert_eq!(
        frames[..3]
            .iter()
            .map(|(frame, _)| frame.instruction)
            .collect::<Vec<_>>(),
        vec![frames[0].0.instruction; 3]
    );
    assert_eq!(
        frames[..3]
            .iter()
            .map(|(frame, _)| frame.source.as_ref().map(|source| source.line.get()))
            .collect::<Vec<_>>(),
        [Some(7), Some(14), Some(28)]
    );
}

/// Odin's procedures are named by package, and step and unwind as C's do.
#[tokio::test]
async fn odin_steps_into_a_procedure_and_back_to_its_caller() {
    for fixture in ["values-odin-o0", "values-odin-o2"] {
        let mut scenario = Scenario::launch(fixture);
        scenario.add_source_breakpoint("values.odin", 182).await;
        assert!(matches!(
            scenario.run_to_stop().await,
            StopReason::Breakpoint { .. }
        ));
        assert_eq!(
            scenario.step_to_stop(StepKind::IntoSource).await,
            StopReason::Step {
                kind: StepKind::IntoSource
            }
        );
        let entered = scenario
            .operation("Odin callee", scenario.handle().current_location())
            .await;
        assert_eq!(
            location_function(&entered),
            Some("values::add"),
            "{fixture}"
        );
        let trace = scenario
            .operation("Odin backtrace", scenario.handle().backtrace())
            .await;
        let names = trace
            .frames
            .iter()
            .filter_map(|frame| frame.function.as_ref())
            .map(|function| function.name.as_ref())
            .collect::<Vec<_>>();
        assert!(
            names.starts_with(&["values::add", "values::main"]),
            "{fixture}: {trace:?}"
        );
        assert_eq!(
            scenario.step_to_stop(StepKind::Out).await,
            StopReason::Step {
                kind: StepKind::Out
            }
        );
        let caller = scenario
            .operation("Odin caller", scenario.handle().current_location())
            .await;
        assert_eq!(
            location_function(&caller),
            Some("values::main"),
            "{fixture}"
        );
        assert_eq!(
            scenario.resume_to_stop().await,
            StopReason::Exited(ExitStatus::Code(0))
        );
        assert_eq!(scenario.shutdown().await, Some(ExitStatus::Code(0)));
    }
}

/// gfortran's procedures step and unwind as C's do; its main program is
/// `MAIN__`, whatever the program calls it, and `main` names it rather than
/// the C wrapper gfortran calls it from.
#[tokio::test]
async fn fortran_steps_into_a_procedure_and_back_to_its_caller() {
    for fixture in ["values-fortran-o0", "values-fortran-o2"] {
        let mut scenario = Scenario::launch(fixture);
        scenario.add_breakpoint("main").await;
        assert!(matches!(
            scenario.run_to_stop().await,
            StopReason::Breakpoint { .. }
        ));
        let started = scenario
            .operation("Fortran main", scenario.handle().current_location())
            .await;
        assert_eq!(location_function(&started), Some("MAIN__"), "{fixture}");
        scenario.add_source_breakpoint("values.f90", 266).await;
        assert!(matches!(
            scenario.resume_to_stop().await,
            StopReason::Breakpoint { .. }
        ));
        assert_eq!(
            scenario.step_to_stop(StepKind::IntoSource).await,
            StopReason::Step {
                kind: StepKind::IntoSource
            }
        );
        let entered = scenario
            .operation("Fortran callee", scenario.handle().current_location())
            .await;
        assert_eq!(location_function(&entered), Some("add"), "{fixture}");
        let trace = scenario
            .operation("Fortran backtrace", scenario.handle().backtrace())
            .await;
        let names = trace
            .frames
            .iter()
            .filter_map(|frame| frame.function.as_ref())
            .map(|function| function.name.as_ref())
            .collect::<Vec<_>>();
        assert!(
            names.starts_with(&["add", "MAIN__", "main"]),
            "{fixture}: {trace:?}"
        );
        assert_eq!(
            scenario.step_to_stop(StepKind::Out).await,
            StopReason::Step {
                kind: StepKind::Out
            }
        );
        let caller = scenario
            .operation("Fortran caller", scenario.handle().current_location())
            .await;
        assert_eq!(location_function(&caller), Some("MAIN__"), "{fixture}");
        assert_eq!(
            scenario.resume_to_stop().await,
            StopReason::Exited(ExitStatus::Code(0))
        );
        assert_eq!(scenario.shutdown().await, Some(ExitStatus::Code(0)));
    }
}

/// D's functions step and unwind as C's do; its main function is `D main`,
/// and druntime's frames are named by their symbols.
#[tokio::test]
async fn d_steps_into_a_function_and_back_to_its_caller() {
    for fixture in ["values-d-o0", "values-d-o2"] {
        let mut scenario = Scenario::launch(fixture);
        scenario.add_source_breakpoint("values.d", 209).await;
        assert!(matches!(
            scenario.run_to_stop().await,
            StopReason::Breakpoint { .. }
        ));
        assert_eq!(
            scenario.step_to_stop(StepKind::IntoSource).await,
            StopReason::Step {
                kind: StepKind::IntoSource
            }
        );
        let entered = scenario
            .operation("D callee", scenario.handle().current_location())
            .await;
        assert_eq!(location_function(&entered), Some("add"), "{fixture}");
        let trace = scenario
            .operation("D backtrace", scenario.handle().backtrace())
            .await;
        let names = trace
            .frames
            .iter()
            .filter_map(|frame| frame.function.as_ref())
            .map(|function| function.name.as_ref())
            .collect::<Vec<_>>();
        assert!(
            names.starts_with(&["add", "D main"]),
            "{fixture}: {trace:?}"
        );

        // druntime, which has no debug information, by its symbols' names.
        assert!(
            trace.frames.iter().any(|frame| frame
                .symbol
                .as_ref()
                .and_then(microscope::SymbolLocation::demangled_name)
                .as_deref()
                == Some("rt.dmain2._d_run_main2.runAll")),
            "{fixture}: {trace:?}"
        );
        assert_eq!(
            scenario.step_to_stop(StepKind::Out).await,
            StopReason::Step {
                kind: StepKind::Out
            }
        );
        let caller = scenario
            .operation("D caller", scenario.handle().current_location())
            .await;
        assert_eq!(location_function(&caller), Some("D main"), "{fixture}");
        assert_eq!(
            scenario.resume_to_stop().await,
            StopReason::Exited(ExitStatus::Code(0))
        );
        assert_eq!(scenario.shutdown().await, Some(ExitStatus::Code(0)));
    }
}

/// Nim's procedures step and unwind as the C they compile to does, named
/// as Nim names them.
#[tokio::test]
async fn nim_steps_into_a_procedure_and_back_to_its_caller() {
    for fixture in ["values-nim-gcc-o0", "values-nim-clang-o2"] {
        let mut scenario = Scenario::launch(fixture);
        scenario.add_source_breakpoint("values.nim", 102).await;
        assert!(matches!(
            scenario.run_to_stop().await,
            StopReason::Breakpoint { .. }
        ));
        assert_eq!(
            scenario.step_to_stop(StepKind::IntoSource).await,
            StopReason::Step {
                kind: StepKind::IntoSource
            }
        );
        let entered = scenario
            .operation("Nim callee", scenario.handle().current_location())
            .await;
        assert_eq!(
            location_function(&entered),
            Some("values::add"),
            "{fixture}"
        );
        assert_eq!(
            scenario.step_to_stop(StepKind::Out).await,
            StopReason::Step {
                kind: StepKind::Out
            }
        );
        let caller = scenario
            .operation("Nim caller", scenario.handle().current_location())
            .await;
        assert_eq!(
            location_function(&caller),
            Some("NimMainModule"),
            "{fixture}"
        );
        assert_eq!(
            scenario.resume_to_stop().await,
            StopReason::Exited(ExitStatus::Code(0))
        );
        assert_eq!(scenario.shutdown().await, Some(ExitStatus::Code(0)));
    }
}

/// GNAT's subprograms step and unwind as C's do, named as Ada names them.
#[tokio::test]
async fn ada_steps_into_a_subprogram_and_back_to_its_caller() {
    for fixture in ["values-ada-o0", "values-ada-o2"] {
        let mut scenario = Scenario::launch(fixture);
        scenario.add_source_breakpoint("values.adb", 222).await;
        assert!(matches!(
            scenario.run_to_stop().await,
            StopReason::Breakpoint { .. }
        ));
        assert_eq!(
            scenario.step_to_stop(StepKind::IntoSource).await,
            StopReason::Step {
                kind: StepKind::IntoSource
            }
        );
        let entered = scenario
            .operation("Ada callee", scenario.handle().current_location())
            .await;
        assert_eq!(location_function(&entered), Some("values.add"), "{fixture}");
        assert_eq!(
            scenario.step_to_stop(StepKind::Out).await,
            StopReason::Step {
                kind: StepKind::Out
            }
        );
        let caller = scenario
            .operation("Ada caller", scenario.handle().current_location())
            .await;
        assert_eq!(location_function(&caller), Some("values"), "{fixture}");
        assert_eq!(
            scenario.resume_to_stop().await,
            StopReason::Exited(ExitStatus::Code(0))
        );
        assert_eq!(scenario.shutdown().await, Some(ExitStatus::Code(0)));
    }
}
