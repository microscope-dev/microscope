//! System-call catchpoints: stopping at a call's entry and exit with its
//! arguments, result, and path; conditions that read them; every call of
//! threads that call at once counted; steps that make a caught call; and
//! catching from a running program.

mod support;

use std::sync::Arc;

use microscope::syscalls::{Abi, SyscallId};
use microscope::{
    BreakpointSpec, CatchPhase, Catchpoint, Condition, ConditionOwner, DebuggerEvent, ExitStatus,
    LaunchOptions, ScalarValue, StepKind, StopReason, SyscallStop, VariableState, VariableValue,
    VirtualAddress,
};
use object::{Object as _, ObjectSymbol as _};
use support::Scenario;

/// The program across a compiler and position-independence pair: the
/// calls go through libc's wrappers either way.
const FIXTURES: [&str; 2] = ["catch-gcc-o0", "catch-clang-o2-nopie"];

/// The address a fixture's symbol names in its file.
fn symbol(fixture: &str, name: &str) -> u64 {
    let bytes = std::fs::read(Scenario::fixture(fixture)).expect("read the fixture");
    let file = object::File::parse(&*bytes).expect("parse the fixture");
    file.symbols()
        .find(|symbol| symbol.name() == Ok(name))
        .unwrap_or_else(|| panic!("{fixture} defines no {name}"))
        .address()
}

fn call(name: &str) -> SyscallId {
    SyscallId::named(Abi::X86_64, name).expect("a call x86-64 names")
}

async fn catch(
    scenario: &Scenario,
    calls: &[&str],
    phase: CatchPhase,
    condition: Option<&str>,
) -> Catchpoint {
    let calls: Arc<[SyscallId]> = calls.iter().map(|name| call(name)).collect();
    let condition = condition.map(|text| Condition::parse(text).expect("test condition"));

    scenario
        .operation(
            "catch syscall",
            scenario.handle().add_catchpoint(calls, phase, condition),
        )
        .await
}

fn caught(reason: &StopReason) -> &SyscallStop {
    match reason {
        StopReason::Syscall(stop) => stop,
        other => panic!("expected a catchpoint's stop, got {other:?}"),
    }
}

/// Whether a boolean expression holds in the selected frame.
async fn holds(scenario: &Scenario, text: &str) -> bool {
    let expression = microscope::Expression::parse(text).expect("test expression");
    match scenario
        .operation(text, scenario.handle().evaluate(&expression))
        .await
    {
        microscope::Evaluation::Value { value, .. } => match value.state {
            VariableState::Available {
                value: VariableValue::Scalar(ScalarValue::Boolean(holds)),
                ..
            } => holds,
            state => panic!("`{text}` has no truth: {state:?}"),
        },
        other => panic!("`{text}` is not a value: {other:?}"),
    }
}

async fn hit_count(scenario: &mut Scenario, catchpoint: &Catchpoint) -> u64 {
    scenario
        .snapshot()
        .await
        .catchpoints
        .iter()
        .find(|listed| listed.id == catchpoint.id)
        .expect("the catchpoint is listed")
        .hit_count
}

#[tokio::test]
async fn a_catchpoint_stops_at_a_calls_entry_and_exit_with_what_it_passed_and_returned() {
    for fixture in FIXTURES {
        let mut scenario = Scenario::launch(fixture);
        let openat = catch(&scenario, &["openat"], CatchPhase::Both, None).await;

        // The dynamic loader opens libraries first; each of its calls stops
        // at its entry and its exit.
        let mut entry = scenario.run_to_stop().await;
        while caught(&entry).path.as_deref() != Some("/nonexistent/microscope-catch") {
            entry = scenario.resume_to_stop().await;
        }

        let before = hit_count(&mut scenario, &openat).await;
        let stop = caught(&entry);
        assert_eq!(
            (stop.call, stop.returned, stop.catchpoints.as_ref()),
            (call("openat"), None, [openat.id].as_slice()),
            "{fixture}"
        );
        assert_eq!(stop.path.as_deref(), Some("/nonexistent/microscope-catch"));
        assert_eq!(stop.args[0].cast_signed(), -100, "AT_FDCWD");
        assert!(holds(&scenario, r#"$path == "/nonexistent/microscope-catch""#).await);
        assert!(holds(&scenario, "(int)$arg0 == -100").await);

        let exit = scenario.resume_to_stop().await;
        let stop = caught(&exit);
        assert_eq!(stop.returned, Some(-2), "ENOENT, {fixture}");
        assert_eq!(
            stop.args[0].cast_signed(),
            -100,
            "the exit keeps the arguments"
        );
        assert!(holds(&scenario, "$ret == -2").await);
        assert_eq!(hit_count(&mut scenario, &openat).await, before + 1);

        scenario
            .operation(
                "remove the catchpoint",
                scenario.handle().remove_catchpoint(openat.id),
            )
            .await;
        assert_eq!(
            scenario.resume_to_stop().await,
            StopReason::Exited(ExitStatus::Code(0)),
            "{fixture}"
        );
        scenario.shutdown().await;
    }
}

#[tokio::test]
async fn a_condition_reads_the_call_and_one_it_cannot_judge_stops() {
    let mut scenario = Scenario::launch(FIXTURES[0]);

    // Only the write to stdout stops; the one to stderr is counted.
    let writes = catch(&scenario, &["write"], CatchPhase::Entry, Some("$arg0 == 1")).await;
    let stop = scenario.run_to_stop().await;
    assert_eq!(caught(&stop).args[0], 1);
    assert_eq!(hit_count(&mut scenario, &writes).await, 2);
    scenario
        .operation(
            "remove the catchpoint",
            scenario.handle().remove_catchpoint(writes.id),
        )
        .await;
    scenario.shutdown().await;

    // A call that names no path has no `$path`, so the condition fails and
    // the call stops, as a breakpoint's unjudgeable condition does.
    let mut scenario = Scenario::launch(FIXTURES[0]);
    let failing = catch(
        &scenario,
        &["write"],
        CatchPhase::Entry,
        Some("$path == \"x\""),
    )
    .await;
    let mut events = scenario.handle().subscribe();
    let stop = scenario.run_to_stop().await;
    assert_eq!(caught(&stop).args[0], 2, "the first write stops");
    let failures: Vec<_> = std::iter::from_fn(|| events.try_recv().ok())
        .filter_map(|event| match event {
            DebuggerEvent::ConditionFailed { owner, error, .. } => Some((owner, error)),
            _ => None,
        })
        .collect();
    assert_eq!(
        failures,
        [(
            ConditionOwner::Catchpoint(failing.id),
            Arc::from("the call names no path microscope could read")
        )]
    );
    scenario.shutdown().await;
}

#[tokio::test]
async fn threads_calling_at_once_have_every_call_counted_and_run_to_their_end() {
    let mut scenario = Scenario::launch(FIXTURES[0]);

    // A condition that never holds makes every call an internal stop.
    let getppid = catch(
        &scenario,
        &["getppid"],
        CatchPhase::Both,
        Some("$arg0 == 12345"),
    )
    .await;
    let options = LaunchOptions {
        arguments: vec!["threads".into()],
        ..LaunchOptions::default()
    };

    assert_eq!(
        scenario.run_with_to_stop(options).await,
        StopReason::Exited(ExitStatus::Code(0))
    );

    // Four threads, a thousand calls each, entry and exit.
    assert_eq!(hit_count(&mut scenario, &getppid).await, 8000);
    scenario.shutdown().await;
}

#[tokio::test]
async fn a_catchpoint_added_while_the_program_runs_stops_its_next_call() {
    let mut scenario = Scenario::launch("catch-spin");
    let running = scenario.start_running().await;
    let getppid = catch(&scenario, &["getppid"], CatchPhase::Entry, None).await;
    let stop = running.await.expect("the run task").expect("the run");
    let stop = caught(&stop);
    assert_eq!(
        (stop.call, stop.catchpoints.as_ref()),
        (call("getppid"), [getppid.id].as_slice())
    );
    scenario.shutdown().await;
}

/// Instruction steps until one stops for something else, which is returned.
async fn step_until_not_a_step(scenario: &mut Scenario) -> StopReason {
    for _ in 0..10_000 {
        match scenario.step_to_stop(StepKind::Instruction).await {
            StopReason::Step { .. } => {}
            reason => return reason,
        }
    }

    panic!("ten thousand steps stopped for nothing else");
}

/// An instruction step that makes a call stops at its catchpoints as
/// continuing does: a step from a caught entry stops at the exit, and a
/// step onto `syscall` stops at the entry. A call none of them catches
/// leaves the step where a step across `syscall` would.
#[tokio::test]
async fn steps_that_make_a_caught_call_stop_at_its_entry_and_exit() {
    let mut scenario = Scenario::launch(FIXTURES[0]);
    catch(&scenario, &["write"], CatchPhase::Both, None).await;
    let entry = scenario.run_to_stop().await;
    assert_eq!((caught(&entry).args[0], caught(&entry).returned), (2, None));
    let exit = scenario.step_to_stop(StepKind::Instruction).await;
    assert_eq!(
        (caught(&exit).args[0], caught(&exit).returned),
        (2, Some(10))
    );
    let next = step_until_not_a_step(&mut scenario).await;
    assert_eq!((caught(&next).args[0], caught(&next).returned), (1, None));
    scenario.shutdown().await;

    // The stderr write's exit and the stdout write's entry stop nothing.
    let mut scenario = Scenario::launch(FIXTURES[0]);
    catch(&scenario, &["write"], CatchPhase::Entry, Some("$arg0 == 2")).await;
    catch(&scenario, &["write"], CatchPhase::Exit, Some("$arg0 == 1")).await;
    let entry = scenario.run_to_stop().await;
    assert_eq!(caught(&entry).args[0], 2);
    let stepped = scenario.step_to_stop(StepKind::Instruction).await;
    assert!(matches!(stepped, StopReason::Step { .. }), "{stepped:?}");
    assert!(holds(&scenario, "$rax == 10").await, "the call returned");
    let next = step_until_not_a_step(&mut scenario).await;
    assert_eq!(
        (caught(&next).args[0], caught(&next).returned),
        (1, Some(10))
    );
    scenario.shutdown().await;
}

/// A thread stopped in a call the kernel will restart, as one attaching
/// interrupted, makes the call again when it next runs. An instruction step
/// from there stops at the restarted call's entry, which a catchpoint
/// catches, rather than running the call unseen.
#[tokio::test]
async fn a_step_into_a_restarted_call_stops_at_its_entry() {
    let child = support::ExternalProcess::spawn(&Scenario::fixture("attach-restart"));

    // Attaching interrupts pause(2), which then restarts.
    support::wait_for_system_call(child.process_id(), 34);
    let mut scenario = Scenario::attached("attached in pause", child.attach().await);
    catch(&scenario, &["pause"], CatchPhase::Entry, None).await;
    let entry = scenario.step_to_stop(StepKind::Instruction).await;
    assert_eq!(
        (caught(&entry).call, caught(&entry).returned),
        (call("pause"), None)
    );
    scenario.shutdown().await;
}

/// A breakpoint planted on a `syscall` instruction does not hide the call
/// from a catchpoint: stepping over its trap stops at the call's entry,
/// and the call's exit stops next.
#[tokio::test]
async fn a_call_under_a_breakpoint_stops_at_its_catchpoint() {
    for fixture in FIXTURES {
        let mut scenario = Scenario::launch(fixture);
        scenario
            .add_breakpoint_spec(BreakpointSpec::Function("main".into()))
            .await;
        let options = LaunchOptions {
            arguments: vec!["raw".into()],
            ..LaunchOptions::default()
        };

        let stop = scenario.run_with_to_stop(options).await;
        assert!(matches!(stop, StopReason::Breakpoint { .. }), "{stop:?}");
        let modules = scenario
            .operation("modules", scenario.handle().loaded_modules())
            .await;
        let bias = modules
            .modules
            .iter()
            .find(|record| record.path.ends_with(fixture))
            .expect("the program is loaded")
            .module
            .load_bias
            .get();
        let address = VirtualAddress::new(bias + symbol(fixture, "raw_syscall"));
        scenario
            .add_breakpoint_spec(BreakpointSpec::Address(address))
            .await;
        catch(&scenario, &["getppid"], CatchPhase::Both, None).await;
        let stop = scenario.resume_to_stop().await;
        assert_eq!(support::breakpoint_address(&stop), address, "{fixture}");
        let entry = scenario.resume_to_stop().await;
        assert_eq!(
            (caught(&entry).call, caught(&entry).returned),
            (call("getppid"), None),
            "{fixture}"
        );
        let after = address.get() + 2;
        assert!(
            holds(&scenario, &format!("$rip == {after}")).await,
            "the raw call stops, {fixture}"
        );
        let exit = scenario.resume_to_stop().await;
        assert!(caught(&exit).returned.is_some(), "{fixture}");
        scenario.shutdown().await;
    }
}
