//! Backtraces across modules and call-frame information.

use super::*;

#[tokio::test]
async fn dwarf_cfi_unwinds_and_finishes_across_the_compiler_and_linker_matrix() {
    for fixture in ["unwind-o0", "unwind-o2", "unwind-nopie", "unwind-clang-o2"] {
        let mut scenario = Scenario::launch(fixture);

        scenario.add_breakpoint("deepest").await;

        assert!(matches!(
            scenario.run_to_stop().await,
            StopReason::Breakpoint { .. }
        ));

        let source = scenario
            .operation("source context", scenario.handle().source_context(1))
            .await;

        assert!(source.recorded.ends_with("unwind.c"));
        assert!(source.recorded.is_absolute());
        assert!(
            source
                .lines
                .iter()
                .any(|line| line.text.contains("deepest")),
            "unexpected {fixture} source context: {source:?}"
        );

        let trace = scenario
            .operation("backtrace", scenario.handle().backtrace())
            .await;

        let names: Vec<_> = trace
            .frames
            .iter()
            .filter_map(|frame| frame.function.as_ref())
            .map(|function| function.name.as_ref())
            .collect();

        assert!(
            names.starts_with(&["deepest", "middle", "outer", "main"]),
            "unexpected {fixture} backtrace: {trace:?}"
        );

        assert!(
            trace.frames.len() >= 4,
            "backtrace was truncated: {trace:?}"
        );

        // Unwinding continues through libc's start code to its undefined
        // return address rather than stopping at the main image's boundary.
        assert_eq!(
            trace.termination,
            UnwindTermination::Complete,
            "unexpected {fixture} backtrace: {trace:?}"
        );

        // Finishing uses the same unwind information to find the caller.
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

        assert_eq!(location_function(&location), Some("middle"), "{fixture}");

        scenario.shutdown().await;
    }
}

#[tokio::test]
#[expect(
    clippy::too_many_lines,
    reason = "one stopped process visits each cross-module unwind boundary in order"
)]
async fn backtraces_unwind_through_shared_libraries_and_libc() {
    for fixture in [
        "module-frames-gcc-o0",
        "module-frames-clang-o2",
        "module-frames-gcc-nopie",
    ] {
        let mut scenario = Scenario::launch(fixture);
        let comparator = scenario.add_breakpoint("compare_values").await;
        scenario.add_breakpoint("module_callback").await;

        // main -> sort_values -> libc qsort -> compare_values: the comparator's
        // caller is described only by libc's own call-frame information.
        assert!(matches!(
            scenario.run_to_stop().await,
            StopReason::Breakpoint { .. }
        ));

        let modules = scenario
            .operation("modules", scenario.handle().loaded_modules())
            .await;

        let trace = scenario
            .operation("backtrace", scenario.handle().backtrace())
            .await;

        let frames = frame_modules(&trace, &modules);
        assert_eq!(
            frames[0],
            (fixture.to_owned(), Some("compare_values".to_owned())),
            "{fixture}: {frames:#?}"
        );

        let sort = position_of(&frames, fixture, "sort_values");
        let main = position_of(&frames, fixture, "main");
        assert!(sort < main, "{fixture}: {frames:#?}");
        assert!(
            frames[1..sort]
                .iter()
                .all(|(module, _)| module.starts_with("libc.so")),
            "{fixture}: qsort frames must belong to libc: {frames:#?}"
        );

        assert!(
            sort > 1,
            "{fixture}: no libc frame was reconstructed: {frames:#?}"
        );

        // libc has no debug information, so its symbol tables name every
        // frame; so too the main image's start code, which has none either.
        for frame in trace.frames.iter() {
            assert!(
                frame.function.is_some() || frame.symbol.is_some(),
                "{fixture}: unnamed frame {frame:#?}"
            );
        }

        let start = trace.frames.last().expect("frames");
        assert_eq!(
            frames.last().map(|(module, _)| module.as_str()),
            Some(fixture)
        );

        assert_eq!(
            start.symbol.as_ref().map(|symbol| symbol.name.as_ref()),
            Some("_start"),
            "{fixture}: {start:#?}"
        );

        // glibc's entry code declares the return address undefined, so a
        // complete trace proves the walk crossed back through libc into _start.
        assert_eq!(
            trace.termination,
            UnwindTermination::Complete,
            "{fixture}: {frames:#?}"
        );

        assert!(
            frames[main + 1..]
                .iter()
                .any(|(module, _)| module.starts_with("libc.so")),
            "{fixture}: main's caller must be libc's start code: {frames:#?}"
        );

        // main -> dso_apply (shared library with DWARF) -> module_callback.
        scenario.remove_breakpoint(comparator.id).await;
        assert!(matches!(
            scenario.resume_to_stop().await,
            StopReason::Breakpoint { .. }
        ));

        let trace = scenario
            .operation("backtrace", scenario.handle().backtrace())
            .await;
        let frames = frame_modules(&trace, &modules);
        assert_eq!(
            frames[..3],
            [
                (fixture.to_owned(), Some("module_callback".to_owned())),
                (
                    "libmodule-frames.so".to_owned(),
                    Some("dso_apply".to_owned())
                ),
                (fixture.to_owned(), Some("main".to_owned())),
            ],
            "{fixture}: {frames:#?}"
        );

        let library_source = trace.frames[1]
            .source
            .as_ref()
            .unwrap_or_else(|| panic!("{fixture}: dso_apply frame has no source"));

        assert!(
            trace.frames[1].code_instance.is_some(),
            "{fixture}: dso_apply frame lost its code instance"
        );

        assert_eq!(
            library_source.line.get(),
            6,
            "{fixture}: {library_source:?}"
        );

        assert_eq!(trace.termination, UnwindTermination::Complete);

        // abort() raises SIGABRT inside libc: the innermost frames have no
        // main-image metadata, yet the walk must still reach the caller.
        assert!(matches!(
            scenario.resume_to_stop().await,
            StopReason::Exception(exception) if exception.code == 6
        ));

        let trace = scenario
            .operation("backtrace", scenario.handle().backtrace())
            .await;

        let frames = frame_modules(&trace, &modules);
        assert!(
            frames[0].0.starts_with("libc.so"),
            "{fixture}: SIGABRT must stop inside libc: {frames:#?}"
        );

        let caller = position_of(&frames, fixture, "abort_in_libc");
        assert!(
            caller < position_of(&frames, fixture, "main"),
            "{fixture}: {frames:#?}"
        );

        assert_eq!(
            trace.frames[caller - 1]
                .symbol
                .as_ref()
                .map(|symbol| symbol.name.as_ref()),
            Some("abort"),
            "{fixture}: {trace:#?}"
        );

        // The stop location is described by the module that contains it.
        let location = scenario
            .operation("location", scenario.handle().current_location())
            .await;

        let libc = modules
            .modules
            .iter()
            .find(|record| {
                record
                    .path
                    .file_name()
                    .is_some_and(|name| name.to_string_lossy().starts_with("libc.so"))
            })
            .expect("libc is loaded");

        assert_eq!(location.module, libc.module.id, "{fixture}");
        assert_eq!(location.image.symbol, trace.frames[0].symbol, "{fixture}");
        assert!(location.image.function.is_none(), "{fixture}");
        assert!(
            frames[..caller]
                .iter()
                .all(|(module, _)| module.starts_with("libc.so")),
            "{fixture}: {frames:#?}"
        );

        assert_eq!(trace.termination, UnwindTermination::Complete);

        scenario.shutdown().await;
    }
}

/// Resumes past the preemption signals the Go runtime sends itself.
async fn go_stop(scenario: &mut Scenario, mut reason: StopReason) -> StopReason {
    while matches!(&reason, StopReason::Exception(exception) if exception.code == 23) {
        reason = scenario.resume_to_stop().await;
    }

    reason
}

/// The frames a Go program recorded with `runtime.CallersFrames`: function,
/// file, and line, innermost first, inline frames included.
fn go_truth_frames(output: &str) -> Vec<(String, String, u64)> {
    output
        .lines()
        .filter_map(|line| {
            let mut fields = line.strip_prefix("TRUTH\tframe\t")?.split('\t');
            Some((
                fields.next()?.to_owned(),
                fields.next()?.to_owned(),
                fields.next()?.parse().ok()?,
            ))
        })
        .collect()
}

/// Every caller's frame pointer is recovered, and is where its prologue
/// pointed it: just below the return address its own caller's frame holds,
/// which the stack table places independently of the saved word.
async fn check_go_frame_pointers(
    scenario: &Scenario,
    trace: &microscope::Backtrace,
    fixture: &str,
) {
    let mut physical = Vec::new();
    for frame in trace
        .frames
        .iter()
        .filter(|frame| frame.kind == microscope::FrameKind::Physical)
    {
        scenario
            .operation("select frame", scenario.handle().select_frame(frame.id))
            .await;
        let registers = scenario
            .operation("registers", scenario.handle().registers())
            .await;
        let register = |name: &str| {
            registers
                .registers
                .iter()
                .find(|register| &*register.register.name == name)
                .and_then(|register| register.bytes.as_deref())
                .map(|bytes| u64::from_le_bytes(bytes.try_into().expect("a 64-bit register")))
        };

        physical.push((frame, register("rbp"), register("rsp")));
    }

    assert!(physical.len() >= 8, "{fixture}: {trace:#?}");

    // The innermost frame stopped in its prologue, and the outermost has
    // no caller to check it by.
    for pair in physical[1..].windows(2) {
        let [(frame, rbp, _), (_, _, caller_rsp)] = pair else {
            unreachable!("windows of two")
        };

        let context = format!(
            "{fixture}: frame {} {:?}",
            frame.level,
            frame.function.as_ref().map(|function| &function.name)
        );
        let caller_rsp = caller_rsp.unwrap_or_else(|| panic!("{context}: no caller rsp"));
        assert_eq!(*rbp, Some(caller_rsp - 16), "{context}");
    }
}

/// Function and line breakpoints bind in Go images with DWARF, without it,
/// and without symbols too, where Go's own function table names and unwinds
/// every frame. The backtrace at the checkpoint equals the frames the
/// program recorded itself, inline frames included.
#[tokio::test]
async fn go_backtraces_match_the_runtimes_own_frames_with_and_without_dwarf() {
    let source = "tests/fixtures/go/callers/main.go";
    let marker = source_line(source, "// descend");
    for fixture in [
        "callers-go",
        "callers-go-stripped",
        "callers-go-external-stripped",
    ] {
        let scratch = support::ScratchDir::new("go-callers");
        let output_path = scratch.path().join("stdout");
        let output = fs::File::create(&output_path).expect("create the fixture's output");
        let mut scenario = Scenario::launch(fixture);
        let line = scenario
            .add_source_breakpoint("callers/main.go", marker)
            .await;
        let reason = scenario
            .run_with_to_stop(LaunchOptions {
                stdout: Some(std::process::Stdio::from(output)),
                ..LaunchOptions::default()
            })
            .await;
        let reason = go_stop(&mut scenario, reason).await;
        assert!(
            matches!(reason, StopReason::Breakpoint { .. }),
            "{fixture}: {reason:?}"
        );
        let image = scenario.handle().module_image();
        let trace = scenario
            .operation("backtrace", scenario.handle().backtrace())
            .await;
        let top = &trace.frames[0];
        assert_eq!(
            top.function.as_ref().map(|function| function.name.as_ref()),
            Some("main.(*walker).descend"),
            "{fixture}: {trace:#?}"
        );
        assert_eq!(
            top.source.as_ref().map(|source| source.line.get()),
            Some(marker),
            "{fixture}"
        );

        scenario.remove_breakpoint(line.id).await;
        scenario.add_breakpoint("main.reached").await;
        let reason = scenario.resume_to_stop().await;
        let reason = go_stop(&mut scenario, reason).await;
        assert!(
            matches!(reason, StopReason::Breakpoint { .. }),
            "{fixture}: {reason:?}"
        );
        let truth = go_truth_frames(&fs::read_to_string(&output_path).expect("read output"));
        assert!(truth.len() >= 8, "{fixture}: {truth:?}");
        let trace = scenario
            .operation("backtrace", scenario.handle().backtrace())
            .await;
        let frames = trace
            .frames
            .iter()
            .map(|frame| {
                let source = frame.source.as_ref();
                (
                    frame
                        .function
                        .as_ref()
                        .map_or_else(String::new, |function| function.name.to_string()),
                    source
                        .and_then(|source| image.source_file(source.file))
                        .map_or_else(String::new, |file| file.path.display().to_string()),
                    source.map_or(0, |source| source.line.get()),
                )
            })
            .collect::<Vec<_>>();

        // The checkpoint recorded its callers' frames, which follow the
        // checkpoint's own and that of the function it stopped in.
        assert_eq!(frames[0].0, "main.reached", "{fixture}: {frames:#?}");
        assert_eq!(frames[1].0, "main.checkpoint", "{fixture}: {frames:#?}");
        assert_eq!(
            frames.get(2..2 + truth.len()),
            Some(truth.as_slice()),
            "{fixture}: {trace:#?}"
        );

        check_go_frame_pointers(&scenario, &trace, fixture).await;

        scenario.remove_all_breakpoints().await;
        let reason = scenario.resume_to_stop().await;
        assert_eq!(
            go_stop(&mut scenario, reason).await,
            StopReason::Exited(ExitStatus::Code(0)),
            "{fixture}"
        );
        scenario.shutdown().await;
    }
}

/// The kernel maps the vDSO from no file, so its module is read from the
/// process's memory. Its call-frame information carries the walk out of it
/// and restores the registers its callers keep values in, and its symbol
/// table names the code it exports; gdb names the same frames.
#[tokio::test]
async fn backtraces_unwind_through_the_vdso() {
    for variant in ["gcc-o0", "gcc-o2", "clang-o2-nopie"] {
        let fixture = format!("vdso-{variant}");

        // clock_gettime faults in an unexported helper that libc calls;
        // time is the vDSO's own, called straight from the program.
        for (mode, caller) in [("clock", "vdso_clock"), ("time", "vdso_time")] {
            let context = format!("{fixture} {mode}");
            let mut scenario = Scenario::launch(&fixture);
            let reason = scenario
                .run_with_to_stop(LaunchOptions {
                    arguments: vec![mode.into()],
                    ..LaunchOptions::default()
                })
                .await;

            assert!(
                matches!(&reason, StopReason::Exception(exception) if exception.code == 11),
                "{context}: {reason:?}"
            );

            let InferiorState::Stopped { process_id, .. } = scenario.snapshot().await.inferior
            else {
                panic!("{context}: the inferior is stopped");
            };

            let modules = scenario
                .operation("modules", scenario.handle().loaded_modules())
                .await;
            let vdso = support::vdso_module(&modules);

            // The kernel links its vDSO at zero.
            let mapping = support::vdso_mapping(process_id);
            assert_eq!(vdso.module.load_bias.get(), mapping.start, "{context}");

            let trace = scenario
                .operation("backtrace", scenario.handle().backtrace())
                .await;

            let frames = frame_modules(&trace, &modules);
            assert!(
                mapping.contains(
                    &trace.frames[0]
                        .instruction
                        .expect("a thread's frame has an instruction")
                        .get()
                ),
                "{context}: {trace:#?}"
            );

            assert_eq!(frames[0].0, support::VDSO, "{context}: {frames:#?}");
            let level = position_of(&frames, &fixture, caller);
            let innermost = trace.frames[0].symbol.as_ref();
            if mode == "clock" {
                // No symbol covers the helper, and none is guessed.
                assert!(innermost.is_none(), "{context}: {innermost:?}");
                assert_eq!(level, 2, "{context}: {frames:#?}");
                assert!(frames[1].0.starts_with("libc.so"), "{context}: {frames:#?}");
                let wrapper = trace.frames[1].symbol.as_ref().expect("a libc symbol");
                assert!(
                    wrapper.name.ends_with("clock_gettime"),
                    "{context}: {wrapper:?}"
                );
            } else {
                // `time` and `__vdso_time` name the same code.
                let symbol = innermost.expect("a vDSO symbol");
                assert!(
                    ["time", "__vdso_time"].contains(&symbol.name.as_ref()) && symbol.offset > 0,
                    "{context}: {symbol:?}"
                );

                assert_eq!(level, 1, "{context}: {frames:#?}");
            }

            assert_eq!(
                frames[level + 1],
                (fixture.clone(), Some("main".to_owned())),
                "{context}: {frames:#?}"
            );

            assert_eq!(
                trace
                    .frames
                    .last()
                    .and_then(|frame| frame.symbol.as_ref())
                    .map(|symbol| symbol.name.as_ref()),
                Some("_start"),
                "{context}: {frames:#?}"
            );

            assert_eq!(
                trace.termination,
                UnwindTermination::Complete,
                "{context}: {frames:#?}"
            );

            // The stop is located in the vDSO, which has no source.
            let location = scenario
                .operation("location", scenario.handle().current_location())
                .await;

            assert_eq!(location.module, vdso.module.id, "{context}");
            assert_eq!(location.image.symbol, trace.frames[0].symbol, "{context}");
            assert!(location.image.source.is_none(), "{context}");

            // Optimized callers keep `depth` in a register the vDSO and libc
            // saved and restore through their call-frame information.
            scenario
                .operation(
                    "select caller",
                    scenario.handle().select_frame(trace.frames[level].id),
                )
                .await;

            let depth = scenario
                .operation("depth", scenario.handle().variable("depth"))
                .await;

            assert_signed(&depth.state, 42, &context);
            scenario.shutdown().await;
        }
    }
}

/// A C handler returns through glibc's signal trampoline, and the frame
/// the signal interrupted comes from the registers the kernel saved in the
/// signal frame; the walk goes on through it to the program's first frame.
#[tokio::test]
async fn backtraces_unwind_through_a_signal_handler() {
    let mut scenario = Scenario::launch("signals");
    scenario.add_breakpoint("handle_usr1").await;
    let mut reason = scenario.run_to_stop().await;

    // The signal itself stops first, and goes on to the program.
    while let StopReason::Exception(_) = reason {
        reason = scenario.resume_to_stop().await;
    }

    assert!(
        matches!(reason, StopReason::Breakpoint { .. }),
        "{reason:?}"
    );

    let trace = scenario
        .operation("backtrace", scenario.handle().backtrace())
        .await;

    let names = trace
        .frames
        .iter()
        .map(|frame| frame.symbol.as_ref().map(|symbol| symbol.name.to_string()))
        .collect::<Vec<_>>();

    let position = |name: &str| {
        names
            .iter()
            .position(|found| found.as_deref() == Some(name))
            .unwrap_or_else(|| panic!("no {name} in {trace:#?}"))
    };

    assert_eq!(names[0].as_deref(), Some("handle_usr1"), "{trace:#?}");
    let trampoline = position("__restore_rt");
    assert_eq!(trampoline, 1, "{trace:#?}");

    // The trampoline and the interrupted frame are where they are, not
    // after a call.
    for frame in &trace.frames[trampoline..=trampoline + 1] {
        assert_eq!(frame.kind, microscope::FrameKind::Signal, "{trace:#?}");
    }

    let raise = position("raise");
    assert!(raise > trampoline + 1, "{trace:#?}");
    assert_eq!(
        names[raise + 1..raise + 3],
        [Some("signal_point".to_owned()), Some("main".to_owned())],
        "{trace:#?}"
    );

    assert_eq!(names.last().cloned().flatten().as_deref(), Some("_start"));
    assert_eq!(trace.termination, UnwindTermination::Complete, "{trace:#?}");
    scenario.shutdown().await;
}

/// A handler on an alternate signal stack returns through the same trampoline
/// as one on the interrupted stack, but the frames it unwinds to are not
/// adjacent on the stack. The walk must still reach the interrupted frames
/// and the program's first frame.
#[tokio::test]
async fn backtraces_unwind_through_a_handler_on_an_alternate_stack() {
    let mut scenario = Scenario::launch("signal-altstack");
    scenario.add_breakpoint("handle_usr1").await;
    let mut reason = scenario.run_to_stop().await;
    while let StopReason::Exception(_) = reason {
        reason = scenario.resume_to_stop().await;
    }

    assert!(
        matches!(reason, StopReason::Breakpoint { .. }),
        "{reason:?}"
    );

    let trace = scenario
        .operation("backtrace", scenario.handle().backtrace())
        .await;

    let names = trace
        .frames
        .iter()
        .map(|frame| frame.symbol.as_ref().map(|symbol| symbol.name.to_string()))
        .collect::<Vec<_>>();

    assert_eq!(names[0].as_deref(), Some("handle_usr1"), "{trace:#?}");
    assert_eq!(names[1].as_deref(), Some("__restore_rt"), "{trace:#?}");
    assert_eq!(
        trace.frames[1].kind,
        microscope::FrameKind::Signal,
        "{trace:#?}"
    );

    let raise = names
        .iter()
        .position(|name| name.as_deref() == Some("raise"))
        .unwrap_or_else(|| panic!("no raise in {trace:#?}"));

    assert_eq!(
        names[raise + 1..raise + 3],
        [Some("deliver".to_owned()), Some("main".to_owned())],
        "{trace:#?}"
    );

    assert_eq!(names.last().cloned().flatten().as_deref(), Some("_start"));
    assert_eq!(trace.termination, UnwindTermination::Complete, "{trace:#?}");
    scenario.shutdown().await;
}

/// GCC describes a function that realigns its stack through a register
/// holding the incoming stack pointer with call-frame expressions: its
/// CFA is read through the saved pointer, and its caller's frame pointer
/// is where a `DW_CFA_expression` rule computes. The walk goes through it,
/// and the caller's variables, found from its frame pointer, are its own.
#[tokio::test]
async fn backtraces_unwind_through_frames_described_by_expressions() {
    for fixture in ["realigned-gcc-o0", "realigned-gcc-o2", "realigned-clang-o2"] {
        let mut scenario = Scenario::launch(fixture);
        scenario.add_breakpoint("leaf").await;
        assert!(
            matches!(scenario.run_to_stop().await, StopReason::Breakpoint { .. }),
            "{fixture}"
        );

        let trace = scenario
            .operation("backtrace", scenario.handle().backtrace())
            .await;

        let names = trace
            .frames
            .iter()
            .map(|frame| {
                frame
                    .function
                    .as_ref()
                    .map(|function| function.name.to_string())
            })
            .collect::<Vec<_>>();

        assert_eq!(
            names[..3],
            [
                Some("leaf".to_owned()),
                Some("realigned".to_owned()),
                Some("main".to_owned())
            ],
            "{fixture}: {trace:#?}"
        );

        assert_eq!(
            trace.termination,
            UnwindTermination::Complete,
            "{fixture}: {trace:#?}"
        );

        scenario
            .operation(
                "select main",
                scenario.handle().select_frame(trace.frames[2].id),
            )
            .await;

        let local = scenario
            .operation("local", scenario.handle().variable("local"))
            .await;

        assert_eq!(
            available_value(&local.state),
            &microscope::VariableValue::Scalar(microscope::ScalarValue::Signed(41)),
            "{fixture}: {local:?}"
        );

        scenario.shutdown().await;
    }
}
