//! A map is never shown wrong while it grows. Go's maps split a full table
//! in two and install the halves in the map's directory one store at a
//! time; at every instruction of that, the map shows only what it holds,
//! or says why it cannot.

use std::collections::BTreeSet;
use std::sync::Arc;

use microscope::{
    Evaluation, Expression, InferiorState, PresentedCount, ScalarValue, StepKind, StopContext,
    StopReason, ValueChildQuery, ValueChildRelationship, ValueChildren, VariableState,
    VariableValue,
};

use crate::invariants::checked;
use crate::support::Scenario;

/// Where Go installs a split table's halves.
const INSTALL: &str = "internal/runtime/maps.(*Map).installTableSplit";

/// The most instructions the installation runs, stepping over its calls.
const MOST_STEPS: usize = 2000;

/// An integer, from the state of a value or a key.
const fn integer(state: &VariableState) -> Option<i128> {
    match state {
        VariableState::Available {
            value: VariableValue::Scalar(ScalarValue::Signed(value)),
            ..
        } => Some(*value),
        _ => None,
    }
}

/// Checks what the stop shows of the map `fill` grows against what the
/// program has put in it: its count, and every entry it lists, unless it
/// refuses to list them. Returns whether it listed them.
async fn check_map(scenario: &Scenario, step: usize) -> Result<bool, String> {
    let snapshot = scenario
        .operation("snapshot", scenario.handle().snapshot())
        .await;
    let InferiorState::Stopped {
        stop_id, thread_id, ..
    } = snapshot.inferior
    else {
        panic!("not stopped");
    };

    let trace = scenario
        .operation("backtrace", scenario.handle().backtrace())
        .await;
    let fill = trace
        .frames
        .iter()
        .find(|frame| {
            frame
                .function
                .as_ref()
                .is_some_and(|function| function.name.as_ref() == "main.fill")
        })
        .unwrap_or_else(|| panic!("step {step}: no fill: {trace:#?}"));
    let at = scenario.handle().at(StopContext {
        stop: stop_id,
        execution: microscope::ExecutionContext::Thread(thread_id),
        frame: fill.id,
    });
    let evaluate = async |text: &str| {
        let expression = Expression::parse(text).expect("an expression");
        match scenario.operation(text, at.evaluate(&expression)).await {
            Evaluation::Value { value, .. } => value,
            other => panic!("step {step}: {text}: {other:?}"),
        }
    };

    let next = integer(&evaluate("next").await.state).expect("next");
    let entries = evaluate("entries").await;
    let VariableState::Available {
        presentation: Some(presentation),
        ..
    } = &entries.state
    else {
        panic!("step {step}: entries: {entries:?}");
    };

    let Some(PresentedCount::Exact(count)) = presentation.count else {
        panic!("step {step}: {presentation:?}");
    };

    // The count is the runtime's own, which the split leaves alone.
    if i128::from(count) != next {
        return Err(format!("{count} entries while {next} are in the map"));
    }

    let ValueChildren::Available(reference) = &presentation.children else {
        panic!("step {step}: no entries: {presentation:?}");
    };

    let mut keys = BTreeSet::new();

    while (keys.len() as u64) < count {
        let page = scenario
            .handle()
            .value_children(
                Arc::clone(reference),
                ValueChildQuery {
                    offset: keys.len() as u64,
                    limit: 256,
                },
            )
            .await;

        let page = match page {
            Ok(page) => page,
            // A refusal says the map cannot be listed as it is now.
            Err(error)
                if error
                    .to_string()
                    .contains(&format!("declares {count} elements")) =>
            {
                return Ok(false);
            }
            Err(error) => panic!("step {step}: {error}"),
        };

        let listed = keys.len();

        for child in page.children.iter() {
            let ValueChildRelationship::Entry { key, .. } = &child.relationship else {
                continue;
            };

            let key = integer(&key.state).expect("an integer key");
            let value = integer(&child.state).expect("an integer value");
            if !(0..next).contains(&key) || value != 7 * key {
                return Err(format!("it shows {key}: {value}"));
            }

            if !keys.insert(key) {
                return Err(format!("it shows {key} twice"));
            }
        }

        assert!(keys.len() > listed, "step {step}: an empty page");
    }

    Ok(true)
}

#[tokio::test]
async fn a_map_is_shown_only_as_it_is_while_a_table_splits() {
    let mut scenario = checked("growing-go");
    let install = scenario.add_breakpoint(INSTALL).await;

    let reason = scenario.run_to_stop().await;
    assert!(
        matches!(reason, StopReason::Breakpoint { .. }),
        "{reason:?}"
    );
    scenario.remove_breakpoint(install.id).await;
    let mut steps = 0;
    let mut refused = 0;

    loop {
        match check_map(&scenario, steps).await {
            Ok(listed) => refused += usize::from(!listed),
            Err(problem) => panic!("after {steps} instructions of the split: {problem}"),
        }

        let reason = scenario.step_to_stop(StepKind::OverInstruction).await;
        assert!(matches!(reason, StopReason::Step { .. }), "{reason:?}");
        steps += 1;

        let trace = scenario
            .operation("backtrace", scenario.handle().backtrace())
            .await;
        let innermost = trace.frames[0]
            .function
            .as_ref()
            .map(|function| function.name.to_string());
        if innermost.as_deref() != Some(INSTALL) {
            break;
        }

        assert!(steps < MOST_STEPS, "the split never returns");
    }

    // Between the stores of the two halves, the directory holds neither
    // table whole, and the map is refused rather than listed short.
    assert!(refused > 0, "{steps} instructions, none refused");
    assert!(check_map(&scenario, steps).await.expect("the split map"));
    scenario.shutdown().await;
}
