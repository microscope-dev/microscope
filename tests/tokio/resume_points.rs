//! The resume points the debugger decodes from each async body's dispatch,
//! checked against the processor running it: at every entry to a body,
//! the test reads the state its future holds and steps one instruction at
//! a time until the dispatch leaves for that state, which must be where
//! the debugger says it goes. At a body's first entry it first writes each
//! of the coroutine's states into the future in turn, so that states the
//! program never resumes from are checked too, and puts the registers and
//! the state back before the program goes on.

use std::collections::{BTreeMap, BTreeSet};
use std::process::Stdio;
use std::sync::Arc;

use microscope::{
    AddressRange, BreakpointSpec, CodeInstanceKind, CoroutineStateKind, EvaluationMode, Expression,
    ImageAddress, InspectionLimits, LaunchOptions, ModuleImage, StepKind, StopReason,
    VirtualAddress,
};

use crate::stops::{integer, line};
use crate::support::Scenario;

/// What the test knows of one body that runs its own dispatch.
struct Body {
    name: String,

    /// Where the future's state number is, and its size.
    state: (u64, u64),
    dispatch: Arc<[AddressRange<ImageAddress>]>,

    /// The state's number, its kind, the line it waits at, and where the
    /// debugger says the dispatch leaves for it.
    points: BTreeMap<u64, (CoroutineStateKind, u64, ImageAddress)>,
}

/// Every out-of-line async body whose dispatch the debugger decoded, by
/// where its code begins.
fn bodies(image: &ModuleImage) -> BTreeMap<ImageAddress, Body> {
    let mut bodies = BTreeMap::new();
    for instance in image.code_instances() {
        if !matches!(instance.kind(), CodeInstanceKind::OutOfLine) {
            continue;
        }

        let Some(function) = image.function(instance.function()) else {
            continue;
        };

        let (Some(Ok(points)), Some(Ok(coroutine))) = (
            image.resume_points(instance.id()),
            function.coroutine().and_then(|ty| image.coroutine(ty)),
        ) else {
            continue;
        };

        let entry = instance
            .ranges()
            .map(|range| range.start)
            .min()
            .expect("a body has code");

        bodies.insert(
            entry,
            Body {
                name: function.name().to_string(),
                state: (coroutine.state.offset, coroutine.state.size),
                dispatch: Arc::clone(&points.dispatch),
                points: points
                    .points
                    .iter()
                    .map(|point| {
                        let state = coroutine
                            .states
                            .iter()
                            .find(|state| state.value == point.state)
                            .expect("a point's state is the coroutine's");
                        let line = state
                            .location
                            .as_ref()
                            .map_or(0, |location| location.line.get());
                        (point.state, (state.kind, line, point.address))
                    })
                    .collect(),
            },
        );
    }

    bodies
}

#[tokio::test]
async fn the_processor_leaves_each_dispatch_where_the_debugger_says() {
    for fixture in ["tokio-std-async-o0", "tokio-std-async-o3"] {
        let mut scenario = Scenario::launch(fixture);

        let reason = scenario
            .run_with_to_stop(LaunchOptions {
                stop_at_entry: true,
                stdout: Some(Stdio::null()),
                ..LaunchOptions::default()
            })
            .await;

        assert!(matches!(reason, StopReason::Entry), "{fixture}: {reason:?}");

        let image = Arc::clone(scenario.handle().module_image());
        let bodies = bodies(&image);
        assert!(!bodies.is_empty(), "{fixture}: no decoded dispatch");

        let modules = scenario
            .operation("modules", scenario.handle().loaded_modules())
            .await;

        let bias = modules.modules[0].module.load_bias.get();

        for entry in bodies.keys() {
            scenario
                .add_breakpoint_spec(BreakpointSpec::Address(VirtualAddress::new(
                    bias + entry.get(),
                )))
                .await;
        }

        let mut seen = BTreeSet::new();
        let mut probed = BTreeSet::new();
        let mut reason = scenario.resume_to_stop().await;
        while matches!(reason, StopReason::Breakpoint { .. }) {
            let entry = register(&scenario, "pc").await;
            let body = &bodies[&ImageAddress::new(entry - bias)];

            // The future is the body's first argument.
            let state_at = VirtualAddress::new(register(&scenario, "rdi").await + body.state.0);

            let read = scenario
                .operation(
                    "state",
                    scenario.handle().read_memory(state_at, body.state.1),
                )
                .await;

            let original = read.bytes.to_vec();
            if probed.insert(entry) {
                probe_every_state(&mut scenario, fixture, body, state_at, entry, bias).await;
                scenario
                    .operation(
                        "restore state",
                        scenario.handle().write_memory(state_at, &original),
                    )
                    .await;
            }

            let state = original
                .iter()
                .rev()
                .fold(0_u64, |value, byte| value << 8 | u64::from(*byte));

            let (kind, _, expected) = body.points[&state];
            assert_eq!(
                leave_dispatch(&mut scenario, body, bias).await,
                expected,
                "{fixture}: {} in state {state} ({kind:?})",
                body.name
            );

            seen.insert((body.name.clone(), state));
            reason = scenario.resume_to_stop().await;
        }

        assert!(
            matches!(reason, StopReason::Exited { .. }),
            "{fixture}: {reason:?}"
        );

        // Every body ran from its start, and resumed from each await the
        // program waits at.
        let waits = ["// AWAIT: leaf", "// AWAIT: middle", "// AWAIT: walk"]
            .map(|marker| line("std-async/src/main.rs", marker));

        for body in bodies.values() {
            for (state, (kind, at, _)) in &body.points {
                let ran = matches!(kind, CoroutineStateKind::Unresumed)
                    || (matches!(kind, CoroutineStateKind::Suspended { .. }) && waits.contains(at));
                if ran {
                    assert!(
                        seen.contains(&(body.name.clone(), *state)),
                        "{fixture}: {} never ran in state {state}: {seen:?}",
                        body.name
                    );
                }
            }
        }

        assert!(
            seen.iter().any(|(_, state)| *state > 2),
            "{fixture}: no body resumed: {seen:?}"
        );

        scenario.shutdown().await;
    }
}

/// The general registers that hold a value on entry to a function, and
/// that its prologue may change.
const SAVED: [&str; 16] = [
    "rax", "rbx", "rcx", "rdx", "rsi", "rdi", "rbp", "rsp", "r8", "r9", "r10", "r11", "r12", "r13",
    "r14", "r15",
];

async fn register(scenario: &Scenario, name: &str) -> u64 {
    u64::try_from(
        integer(scenario, &format!("${name}"))
            .await
            .unwrap_or_else(|| panic!("${name} is unavailable")),
    )
    .expect("a register holds an unsigned value")
}

async fn saved_registers(scenario: &Scenario) -> Vec<(&'static str, u64)> {
    let mut saved = Vec::new();
    for name in SAVED {
        saved.push((name, register(scenario, name).await));
    }

    saved
}

async fn assign(scenario: &Scenario, text: &str) {
    let expression = Expression::parse(text).expect("an assignment");
    scenario
        .operation(
            text,
            scenario.handle().evaluate_with(
                &expression,
                EvaluationMode::Assign,
                InspectionLimits::default(),
            ),
        )
        .await;
}

/// Steps one instruction at a time until the thread leaves `body`'s
/// dispatch, and returns where it went.
async fn leave_dispatch(scenario: &mut Scenario, body: &Body, bias: u64) -> ImageAddress {
    loop {
        let pc = ImageAddress::new(register(scenario, "pc").await - bias);
        if !body.dispatch.iter().any(|range| range.contains(pc)) {
            return pc;
        }

        scenario.step_to_stop(StepKind::Instruction).await;
    }
}

/// Checks where `body`'s dispatch leaves for each of its states, written
/// in turn into the future's state at `state_at`, from the body's `entry`,
/// putting the registers back after each.
async fn probe_every_state(
    scenario: &mut Scenario,
    fixture: &str,
    body: &Body,
    state_at: VirtualAddress,
    entry: u64,
    bias: u64,
) {
    let size = usize::try_from(body.state.1).expect("a small state");
    let saved = saved_registers(scenario).await;

    for (&state, &(kind, _, expected)) in &body.points {
        let bytes = &state.to_le_bytes()[..size];

        scenario
            .operation(
                "write state",
                scenario.handle().write_memory(state_at, bytes),
            )
            .await;

        let left = leave_dispatch(scenario, body, bias).await;
        assert_eq!(
            left, expected,
            "{fixture}: {} written state {state} ({kind:?})",
            body.name
        );

        for (name, value) in &saved {
            assign(scenario, &format!("${name} = {value:#x}")).await;
        }

        assign(scenario, &format!("$pc = {entry:#x}")).await;
    }
}
