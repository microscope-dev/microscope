//! Changing values and memory at a stop.

use super::*;

/// Stops `fixture` in `function` at its first statement after `marker`'s
/// line, as a source breakpoint there does.
async fn stopped_at_line(fixture: &str, source: &str, marker: &str) -> Scenario {
    let mut scenario = Scenario::launch(fixture);
    let line = source_line(&format!("tests/fixtures/c/{source}"), marker);
    scenario.add_source_breakpoint(source, line).await;

    assert!(matches!(
        scenario.run_to_stop().await,
        StopReason::Breakpoint { .. }
    ));
    scenario
}

fn signed(value: &microscope::InspectedValue) -> i128 {
    match available_value(&value.state) {
        microscope::VariableValue::Scalar(ScalarValue::Signed(value))
        | microscope::VariableValue::Enumeration {
            value: microscope::IntegerValue::Signed(value),
            ..
        } => *value,
        other => panic!("not a signed value: {other:?}"),
    }
}

/// Evaluates `text` in a frame, assigning when `mode` allows.
async fn evaluate(
    view: &microscope::StopView<'_>,
    text: &str,
    mode: microscope::EvaluationMode,
) -> microscope::Result<microscope::InspectedValue> {
    let expression = microscope::Expression::parse(text).map_err(Error::Expression)?;
    match view
        .evaluate_with(&expression, mode, microscope::InspectionLimits::default())
        .await?
    {
        microscope::Evaluation::Value { value, .. } => Ok(value),
        other => panic!("`{text}` has no value: {other:?}"),
    }
}

/// Assigns `value` to `target`, as `set var` does.
async fn assign(
    view: &microscope::StopView<'_>,
    target: &str,
    value: &str,
) -> microscope::Result<microscope::InspectedValue> {
    evaluate(
        view,
        &format!("{target} = {value}"),
        microscope::EvaluationMode::Assign,
    )
    .await
}

async fn read(
    view: &microscope::StopView<'_>,
    text: &str,
) -> microscope::Result<microscope::InspectedValue> {
    evaluate(view, text, microscope::EvaluationMode::Read).await
}

/// A view of a frame of the stopped thread.
async fn frame_view(scenario: &mut Scenario, level: u32) -> microscope::StopContext {
    let snapshot = scenario.snapshot().await;
    let microscope::InferiorState::Stopped {
        stop_id, thread_id, ..
    } = snapshot.inferior
    else {
        panic!("not stopped");
    };

    let trace = scenario
        .operation("backtrace", scenario.handle().backtrace())
        .await;

    microscope::StopContext {
        stop: stop_id,
        execution: thread_id.into(),
        frame: trace.frames[usize::try_from(level).expect("level")].id,
    }
}

#[tokio::test]
async fn assignments_change_memory_the_program_then_uses() {
    let mut scenario = stopped_at_line(
        "variables-gcc-o0",
        "variables.c",
        "return **pointer_pointer",
    )
    .await;
    let innermost = frame_view(&mut scenario, 0).await;
    let handle = scenario.handle().clone();
    let view = handle.at(innermost);

    let assigned = scenario
        .operation("assign", assign(&view, "pointee", "100"))
        .await;
    assert_eq!(signed(&assigned), 100);

    // The pointer sees the new value: the write went to memory.
    let through = scenario.operation("inspect", read(&view, "*pointer")).await;
    assert_eq!(signed(&through), 100);
    let doubled = scenario
        .operation(
            "assign expression",
            assign(&view, "parameter", "parameter * 2 + 1"),
        )
        .await;
    assert_eq!(signed(&doubled), 81);

    for (target, value, reason) in [
        ("pair", "1", "cannot be assigned"),
        ("pointee", "1.5", "1.5 does not fit `int` exactly"),
        (
            "pointee",
            "no_such_value",
            "no variable is named `no_such_value` here",
        ),
        (
            "pointee + 1",
            "2",
            "is a computed value, which cannot be assigned",
        ),
    ] {
        let error = scenario
            .attempt("refused assignment", assign(&view, target, value))
            .await;
        let message = error.map(|_| ()).map_err(|error| error.to_string());
        assert!(
            message
                .as_ref()
                .is_err_and(|message| message.contains(reason)),
            "{target} = {value}: {message:?}"
        );
    }

    let caller = handle.at(frame_view(&mut scenario, 1).await);
    let error = scenario
        .attempt(
            "narrow assignment",
            assign(&caller, "unsigned_character", "300"),
        )
        .await;
    assert_eq!(
        error.map(|_| ()).map_err(|error| error.to_string()),
        Err("300 does not fit `unsigned char` exactly".to_owned())
    );

    // A caller's variable in memory can change too; main checks it.
    let changed = scenario
        .operation("assign in caller", assign(&caller, "signed_int", "5"))
        .await;
    assert_eq!(signed(&changed), 5);

    scenario.remove_all_breakpoints().await;
    assert_eq!(
        scenario.resume_to_stop().await,
        StopReason::Exited(ExitStatus::Code(1))
    );
    scenario.shutdown().await;
}

#[tokio::test]
async fn enumerations_take_their_enumerators_names() {
    let mut scenario = Scenario::launch("enums-c-gcc-o0");
    scenario.add_breakpoint("inspect_enums").await;
    assert!(matches!(
        scenario.run_to_stop().await,
        StopReason::Breakpoint { .. }
    ));
    let context = frame_view(&mut scenario, 1).await;
    let handle = scenario.handle().clone();
    let caller = handle.at(context);
    let assigned = scenario
        .operation(
            "assign enumerator",
            assign(&caller, "signed_value", "SIGNED_ZERO"),
        )
        .await;
    assert_eq!(signed(&assigned), 0);

    // inspect_enums now sees a different value than main set.
    scenario.remove_all_breakpoints().await;
    assert_eq!(
        scenario.resume_to_stop().await,
        StopReason::Exited(ExitStatus::Code(1))
    );
    scenario.shutdown().await;
}

#[tokio::test]
async fn registers_hold_whole_variables_of_the_innermost_frame_only() {
    let mut scenario = Scenario::launch("variables-gcc-o2");
    scenario.add_breakpoint("pointer_target").await;
    assert!(matches!(
        scenario.run_to_stop().await,
        StopReason::Breakpoint { .. }
    ));
    let context = frame_view(&mut scenario, 0).await;
    let handle = scenario.handle().clone();
    let innermost = handle.at(context);
    let current = scenario
        .operation("inspect", read(&innermost, "parameter"))
        .await;
    assert!(
        matches!(
            current.state,
            VariableState::Available {
                source: microscope::VariableValueSource::Register(_),
                ..
            }
        ),
        "the optimized parameter lives in a register: {current:?}"
    );

    let assigned = scenario
        .operation("assign register", assign(&innermost, "parameter", "41"))
        .await;
    assert_eq!(signed(&assigned), 41);

    // The optimized caller's variables are constants with no storage.
    let context = frame_view(&mut scenario, 1).await;
    let caller = handle.at(context);
    let refused = scenario
        .attempt("assign constant", assign(&caller, "signed_int", "1"))
        .await;
    assert_eq!(
        refused.map(|_| ()).map_err(|error| error.to_string()),
        Err("the debug information computes it; it has no storage".to_owned())
    );

    // The function computes with the register's new value.
    scenario.remove_all_breakpoints().await;
    assert_eq!(
        scenario.resume_to_stop().await,
        StopReason::Exited(ExitStatus::Code(1))
    );
    scenario.shutdown().await;
}

#[tokio::test]
async fn memory_writes_keep_breakpoints_installed_and_span_words() {
    let mut scenario = Scenario::launch("basic");
    let breakpoint = scenario.add_breakpoint("breakpoint_target").await;
    let reason = scenario.run_to_stop().await;
    let address = crate::support::breakpoint_address(&reason);
    let handle = scenario.handle().clone();

    // The program's bytes at the breakpoint, written back over the trap.
    let code = scenario
        .operation("read code", handle.read_memory(address, 16))
        .await;
    let written = scenario
        .operation("write code", handle.write_memory(address, &code.bytes))
        .await;
    assert_eq!(written, 16);

    let again = scenario
        .operation("read code again", handle.read_memory(address, 16))
        .await;
    assert_eq!(again.bytes, code.bytes, "traps stay hidden");

    // The second call still stops at the breakpoint.
    assert!(matches!(
        scenario.resume_to_stop().await,
        StopReason::Breakpoint { hits, .. } if hits[0].breakpoint == breakpoint.id
    ));

    // Bytes across a word boundary, starting mid-word.
    let global = scenario
        .operation("address", handle.runtime_address("microscope_value"))
        .await;
    let start = VirtualAddress::new(global.get() + 3);

    scenario
        .operation("write across words", handle.write_memory(start, &[0xaa; 5]))
        .await;

    let value = scenario
        .operation("read value", handle.read_memory(global, 8))
        .await;
    assert_eq!(
        &*value.bytes,
        &[0x88, 0x77, 0x66, 0xaa, 0xaa, 0xaa, 0xaa, 0xaa]
    );

    let unwritable = scenario
        .attempt(
            "write nowhere",
            handle.write_memory(VirtualAddress::new(8), &[1]),
        )
        .await;
    assert_eq!(
        unwritable.map_err(|error| error.to_string()),
        Err("memory at 0x8 cannot be written".to_owned())
    );
    scenario.shutdown().await;
}

fn jump_line(marker: &str) -> u64 {
    source_line("tests/fixtures/c/jump.c", &format!("jump: {marker}"))
}

fn jump_source(marker: &str) -> BreakpointSpec {
    BreakpointSpec::Source {
        path: "jump.c".into(),
        line: microscope::LineNumber::new(jump_line(marker)).expect("a line"),
    }
}

/// Stops `jump` at the first line of `checked`, which, run on, exits 111.
async fn stopped_in_checked() -> (Scenario, microscope::StopId, microscope::ThreadId) {
    let mut scenario = Scenario::launch("jump");
    scenario
        .add_source_breakpoint("jump.c", jump_line("start"))
        .await;
    assert!(matches!(
        scenario.run_to_stop().await,
        StopReason::Breakpoint { .. }
    ));
    let InferiorState::Stopped {
        stop_id, thread_id, ..
    } = scenario.snapshot().await.inferior
    else {
        panic!("not stopped");
    };

    (scenario, stop_id, thread_id)
}

async fn stopped_line(scenario: &Scenario) -> Option<u64> {
    let location = scenario
        .operation("location", scenario.handle().current_location())
        .await;
    location_line(&location)
}

/// A jump moves the thread without running it, and publishes the stop
/// again under a new stop, so that what clients read of the old one fails.
#[tokio::test]
async fn a_jump_moves_a_thread_within_its_function_under_a_new_stop() {
    let (mut scenario, before, thread) = stopped_in_checked().await;

    // The thread has yet to arrive at a breakpoint where it lands.
    let target = scenario
        .add_source_breakpoint("jump.c", jump_line("target"))
        .await;
    assert_eq!(
        scenario.jump_to_stop(jump_source("target")).await,
        StopReason::Jump
    );
    let InferiorState::Stopped {
        stop_id, reason, ..
    } = scenario.snapshot().await.inferior
    else {
        panic!("not stopped");
    };

    assert_ne!(stop_id, before);
    assert_eq!(reason, StopReason::Jump);
    assert_eq!(stopped_line(&scenario).await, Some(jump_line("target")));

    // Nothing ran: `status = value` never did.
    assert_eq!(
        support::evaluate_value(&scenario, "status").await.signed(),
        0
    );
    let stale = scenario
        .attempt(
            "jump from the old stop",
            scenario
                .handle()
                .start_jump(before, thread, jump_source("return")),
        )
        .await;
    assert!(matches!(stale, Err(Error::StaleStop)), "{stale:?}");
    let StopReason::Breakpoint { hits, .. } = scenario.resume_to_stop().await else {
        panic!("the breakpoint where the thread landed did not stop it");
    };

    assert_eq!(hits[0].breakpoint, target.id);
    assert_eq!(stopped_line(&scenario).await, Some(jump_line("target")));
    assert_eq!(
        support::evaluate_value(&scenario, "status").await.signed(),
        0
    );

    // Only `status += 100` runs.
    assert_eq!(
        scenario.resume_to_stop().await,
        StopReason::Exited(ExitStatus::Code(100))
    );
    scenario.shutdown().await;
}

/// A jump stays in the function the thread is stopped in, at one place,
/// and one it refuses changes nothing.
#[tokio::test]
async fn a_jump_is_refused_out_of_its_function_or_to_several_places() {
    let (mut scenario, before, _) = stopped_in_checked().await;
    for (spec, message) in [
        (
            BreakpointSpec::Function("elsewhere".to_owned()),
            "elsewhere has no code in the function the thread is stopped in",
        ),
        (jump_source("call"), "has no code in the function"),
        (
            jump_source("inlined"),
            "has code in several places of the function",
        ),
    ] {
        let refused = scenario
            .attempt("refused jump", scenario.handle().jump(spec.clone()))
            .await;
        let message_text = refused.map_err(|error| error.to_string());
        assert!(
            message_text
                .as_ref()
                .is_err_and(|text| text.contains(message)),
            "{spec}: {message_text:?}"
        );
    }

    let InferiorState::Stopped { stop_id, .. } = scenario.snapshot().await.inferior else {
        panic!("not stopped");
    };

    assert_eq!(stop_id, before);
    assert_eq!(
        scenario.resume_to_stop().await,
        StopReason::Exited(ExitStatus::Code(111))
    );
    scenario.shutdown().await;
}

/// Assigning `$pc` moves the thread anywhere, as a jump does.
#[tokio::test]
async fn assigning_the_program_counter_moves_the_thread_under_a_new_stop() {
    let (mut scenario, before, _) = stopped_in_checked().await;
    let target = scenario
        .add_source_breakpoint("jump.c", jump_line("target"))
        .await;
    let BreakpointLocation::Image(image_address) = target.locations[0].location else {
        panic!("{target:?}");
    };

    let modules = scenario
        .operation("modules", scenario.handle().loaded_modules())
        .await;
    let address = modules.modules[0].module.load_bias.get() + image_address.get();

    scenario.remove_breakpoint(target.id).await;

    let context = frame_view(&mut scenario, 0).await;
    let handle = scenario.handle().clone();
    let assigned = scenario
        .operation(
            "assign $pc",
            assign(&handle.at(context), "$pc", &format!("{address:#x}")),
        )
        .await;
    assert!(
        matches!(
            available_value(&assigned.state),
            microscope::VariableValue::Scalar(ScalarValue::Unsigned(value)) if *value == u128::from(address)
        ),
        "{assigned:?}"
    );
    let InferiorState::Stopped {
        stop_id, reason, ..
    } = scenario.snapshot().await.inferior
    else {
        panic!("not stopped");
    };

    assert_ne!(stop_id, before);
    assert_eq!(reason, StopReason::Jump);
    assert_eq!(stopped_line(&scenario).await, Some(jump_line("target")));
    assert_eq!(
        scenario.resume_to_stop().await,
        StopReason::Exited(ExitStatus::Code(100))
    );
    scenario.shutdown().await;
}

/// A register assigned in the innermost frame is what the thread computes
/// with; a caller's registers are what unwinding recovered, and refused.
#[tokio::test]
async fn assigned_registers_are_what_the_thread_computes_with() {
    let mut scenario = Scenario::launch("jump");
    scenario.add_breakpoint("elsewhere").await;
    assert!(matches!(
        scenario.run_to_stop().await,
        StopReason::Breakpoint { .. }
    ));
    let caller = frame_view(&mut scenario, 1).await;
    let handle = scenario.handle().clone();
    let refused = scenario
        .attempt(
            "assign a caller's register",
            assign(&handle.at(caller), "$rbx", "1"),
        )
        .await;
    assert!(
        refused
            .as_ref()
            .is_err_and(|error| error.to_string().contains("a caller's registers")),
        "{refused:?}"
    );

    // Back in main, the call's result is in rax, which main stores next.
    scenario.remove_all_breakpoints().await;
    assert!(matches!(
        scenario.step_to_stop(StepKind::Out).await,
        StopReason::Step { .. }
    ));

    let innermost = frame_view(&mut scenario, 0).await;
    let view = handle.at(innermost);
    let rax = scenario.operation("read $rax", read(&view, "$rax")).await;
    assert!(
        matches!(
            available_value(&rax.state),
            microscope::VariableValue::Scalar(ScalarValue::Unsigned(7))
        ),
        "{rax:?}"
    );

    scenario
        .operation("assign $rax", assign(&view, "$rax", "42"))
        .await;

    assert_eq!(
        scenario.resume_to_stop().await,
        StopReason::Exited(ExitStatus::Code(42))
    );
    scenario.shutdown().await;
}

/// A thread stopped in a system call that the kernel would restart, by
/// moving it back to the call as it resumes, resumes where it was moved
/// to instead.
#[tokio::test]
async fn a_thread_moved_out_of_a_system_call_resumes_where_it_was_moved() {
    let mut scenario = Scenario::launch("jump");
    assert_eq!(
        scenario
            .run_with_to_stop(LaunchOptions {
                arguments: vec!["wait".into()],
                stop_at_entry: true,
                ..LaunchOptions::default()
            })
            .await,
        StopReason::Entry
    );

    let _resumed = scenario.start_resuming().await;
    let InferiorState::Running { process_id, .. } = scenario.snapshot().await.inferior else {
        panic!("not running");
    };

    support::wait_for_system_call(process_id, 34);
    let reason = scenario.operation("pause", scenario.handle().pause()).await;
    assert_eq!(reason, StopReason::Pause);

    let registers = scenario
        .operation("registers", scenario.handle().registers())
        .await;
    assert_eq!(
        registers
            .registers
            .iter()
            .find(|value| &*value.register.name == "orig_rax")
            .and_then(|value| value.bytes.as_deref())
            .map(|bytes| bytes[0]),
        Some(34),
        "stopped in pause(2)"
    );
    assert_eq!(
        scenario.jump_to_stop(jump_source("resumed")).await,
        StopReason::Jump
    );
    assert_eq!(
        scenario.resume_to_stop().await,
        StopReason::Exited(ExitStatus::Code(5))
    );
    scenario.shutdown().await;
}
