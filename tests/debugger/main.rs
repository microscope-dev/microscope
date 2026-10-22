#[path = "../support/mod.rs"]
mod support;

mod attach;
mod concurrency;
mod debug_files;
mod exec;
mod execution;
mod expressions;
#[cfg(debug_assertions)]
mod flight_recorder;
mod forks;
mod gallery;
mod globals;
mod identities;
mod libraries;
mod locations;
mod metadata;
mod names;
mod signals;
mod step_targets;
mod stepping;
mod tail_frames;
mod unreadable;
mod unwind;
mod values;
mod variables;
mod views;
mod writes;

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::sync::Arc;

use microscope::{
    Architecture, BreakpointLocation, BreakpointSpec, ByteOrder, CodeInstanceKind, Debugger,
    EntryProvenance, Error, ExitStatus, InferiorState, InlineFrameLookup, LaunchOptions,
    ModuleImage, PointerWidth, ProcessId, RegisterRole, ScalarValue, SourceContext, SourceFile,
    SourceLocation, StepKind, StopReason, ThreadState, UnwindTermination, VariableKind,
    VariableState, VariableUnavailableReason, VirtualAddress,
};

use nix::sys::signal::{Signal, kill};
use nix::unistd::Pid;
use support::{Scenario, frame_modules, position_of, register_u64, source_line};
use tokio::time::{Duration, timeout};

fn available_value(state: &VariableState) -> &microscope::VariableValue {
    match state {
        VariableState::Available { value, .. } => value,
        state => panic!("value was not available: {state:?}"),
    }
}

fn available_children(state: &VariableState) -> &Arc<microscope::ValueChildrenReference> {
    match state {
        VariableState::Available {
            children: microscope::ValueChildren::Available(reference),
            ..
        } => reference,
        state => panic!("value had no available children: {state:?}"),
    }
}

async fn child_page(
    scenario: &Scenario,
    state: &VariableState,
    offset: u64,
    limit: u32,
) -> microscope::ValueChildPage {
    scenario
        .operation(
            &format!(
                "value children {offset}..{}",
                offset.saturating_add(u64::from(limit))
            ),
            scenario.handle().value_children(
                available_children(state).clone(),
                microscope::ValueChildQuery { offset, limit },
            ),
        )
        .await
}

fn named_child<'a>(page: &'a microscope::ValueChildPage, name: &str) -> &'a microscope::ValueChild {
    page.children
        .iter()
        .find(|child| {
            matches!(
                &child.relationship,
                microscope::ValueChildRelationship::Member(member)
                    if member.name.as_deref() == Some(name)
            )
        })
        .unwrap_or_else(|| panic!("value page has no member named {name}: {page:?}"))
}

fn assert_signed(state: &VariableState, expected: i128, context: &str) {
    assert!(
        matches!(
            available_value(state),
            microscope::VariableValue::Scalar(ScalarValue::Signed(value)) if *value == expected
        ),
        "{context}: {state:?}"
    );
}

async fn record_page(
    scenario: &Scenario,
    state: &VariableState,
    minimum_children: usize,
    context: &str,
) -> microscope::ValueChildPage {
    assert!(
        matches!(
            available_value(state),
            microscope::VariableValue::Record
                | microscope::VariableValue::Union
                | microscope::VariableValue::Variant { .. }
        ),
        "{context}: value was not an aggregate: {state:?}"
    );

    let reference = available_children(state);
    assert!(
        reference.total() >= u64::try_from(minimum_children).expect("child count fits u64"),
        "{context}: aggregate had too few children: {state:?}"
    );

    let limit = u32::try_from(reference.total().min(256)).expect("bounded page fits u32");
    if limit == 0 {
        return microscope::ValueChildPage {
            stop_id: reference.stop_id(),
            offset: 0,
            total: 0,
            children: Arc::from([]),
            completion: microscope::InspectionCompletion::Complete,
            usage: microscope::InspectionUsage::default(),
        };
    }

    child_page(scenario, state, 0, limit).await
}

fn value_expression(components: &[&str]) -> microscope::Expression {
    let (first, rest) = components.split_first().expect("a name");
    rest.iter().fold(
        microscope::Expression::name(first).expect("a name"),
        |expression, member| expression.member(member).expect("a member"),
    )
}

fn parsed_value_expression(expression: &str) -> microscope::Expression {
    microscope::Expression::parse(expression)
        .unwrap_or_else(|error| panic!("parse test expression {expression:?}: {error}"))
}

async fn load_fixture_image(fixture: &str) -> Arc<ModuleImage> {
    let debugger = Debugger::new(Scenario::fixture(fixture))
        .unwrap_or_else(|error| panic!("load {fixture} metadata: {error}"));
    let image = debugger.handle().module_image();

    debugger
        .shutdown()
        .await
        .unwrap_or_else(|error| panic!("shut down {fixture} metadata session: {error}"));

    image
}

fn single_image_breakpoint_address(
    breakpoint: &microscope::Breakpoint,
) -> microscope::ImageAddress {
    assert_eq!(breakpoint.locations.len(), 1);
    match breakpoint.locations[0].location {
        BreakpointLocation::Image(address) => address,
        BreakpointLocation::Virtual(_) | BreakpointLocation::Managed(_) => {
            panic!("function breakpoint was not image-based")
        }
    }
}

async fn dereference_named(
    scenario: &Scenario,
    name: &str,
    depth: usize,
) -> microscope::DereferencedValue {
    let variable = scenario
        .operation("inspect pointer", scenario.handle().variable(name))
        .await;
    let mut state = variable.state;
    let mut result = None;

    for _ in 0..depth {
        let reference = match state {
            VariableState::Available {
                dereference: microscope::DereferenceState::Available(reference),
                ..
            } => reference,
            other => panic!("{name} is not dereferenceable: {other:?}"),
        };

        let value = scenario
            .operation(
                "dereference pointer",
                scenario.handle().dereference(reference),
            )
            .await;

        state = value.state.clone();
        result = Some(value);
    }

    result.expect("positive dereference depth")
}

async fn assert_slice_values(
    scenario: &Scenario,
    variable: &microscope::Variable,
    capacity: Option<u64>,
    expected: &[i128],
    fixture: &str,
) {
    let microscope::VariableValue::Slice {
        length,
        capacity: actual_capacity,
    } = available_value(&variable.state)
    else {
        panic!("{fixture}: expected decoded slice, got {variable:?}");
    };

    assert_eq!(*length, u64::try_from(expected.len()).unwrap(), "{fixture}");
    assert_eq!(*actual_capacity, capacity, "{fixture}");

    let page = if expected.is_empty() {
        None
    } else {
        Some(
            child_page(
                scenario,
                &variable.state,
                0,
                u32::try_from(expected.len()).expect("test slice length fits u32"),
            )
            .await,
        )
    };

    let values: Vec<i128> = page
        .as_ref()
        .map_or(&[][..], |page| page.children.as_ref())
        .iter()
        .map(|child| match available_value(&child.state) {
            microscope::VariableValue::Scalar(microscope::ScalarValue::Signed(value)) => *value,
            other => panic!("{fixture}: expected scalar slice element, got {other:?}"),
        })
        .collect();

    assert_eq!(values, expected, "{fixture}");
}

fn location_function(location: &microscope::ExecutionLocation) -> Option<&str> {
    location
        .image
        .function
        .as_ref()
        .map(|function| function.name.as_ref())
}

fn location_line(location: &microscope::ExecutionLocation) -> Option<u64> {
    location
        .image
        .source
        .as_ref()
        .map(|source| source.line.get())
}

#[derive(Clone, Copy)]
struct EntryBoundaryCase {
    fixture: &'static str,
    function: &'static str,
    source: &'static str,
    call_line: u64,
    parameter: &'static str,
    expected_value: i64,
    gcc_fallback_line: Option<u64>,
}

const fn entry_boundary_cases() -> [EntryBoundaryCase; 6] {
    [
        EntryBoundaryCase {
            fixture: "variables-parameters-gcc-o0",
            function: "all_parameters",
            source: "variables-parameters.c",
            call_line: 66,
            parameter: "signed_int",
            expected_value: -1_234_567,
            gcc_fallback_line: Some(21),
        },
        EntryBoundaryCase {
            fixture: "variables-parameters-gcc-o2",
            function: "all_parameters",
            source: "variables-parameters.c",
            call_line: 66,
            parameter: "signed_int",
            expected_value: -1_234_567,
            // This optimized leaf has no prologue. Its first instruction is
            // the line-22 store, so advancing to the next distinct statement
            // would silently execute real user work.
            gcc_fallback_line: Some(20),
        },
        EntryBoundaryCase {
            fixture: "variables-parameters-clang-o0",
            function: "all_parameters",
            source: "variables-parameters.c",
            call_line: 66,
            parameter: "signed_int",
            expected_value: -1_234_567,
            gcc_fallback_line: None,
        },
        EntryBoundaryCase {
            fixture: "variables-parameters-clang-o2",
            function: "all_parameters",
            source: "variables-parameters.c",
            call_line: 66,
            parameter: "signed_int",
            expected_value: -1_234_567,
            gcc_fallback_line: None,
        },
        EntryBoundaryCase {
            fixture: "variables-rust-o0",
            function: "inspect_scalars",
            source: "variables.rs",
            call_line: 93,
            parameter: "signed_value",
            expected_value: -42,
            gcc_fallback_line: None,
        },
        EntryBoundaryCase {
            fixture: "variables-rust-o2",
            function: "inspect_scalars",
            source: "variables.rs",
            call_line: 93,
            parameter: "signed_value",
            expected_value: -42,
            gcc_fallback_line: None,
        },
    ]
}

fn expected_physical_entry(
    scenario: &Scenario,
    case: &EntryBoundaryCase,
) -> microscope::ImageAddress {
    let image = scenario.handle().module_image();
    let function = image
        .function_named(case.function)
        .unwrap_or_else(|error| panic!("{} missing {}: {error}", case.fixture, case.function));

    let instance = image
        .instances_for_function(function.id())
        .find(|instance| matches!(instance.kind(), CodeInstanceKind::OutOfLine))
        .unwrap_or_else(|| panic!("{} missing physical {}", case.fixture, case.function));

    if let Some(marker) = image
        .statement_rows()
        .find(|row| row.flags.prologue_end() && instance.contains(row.address))
    {
        return marker.address;
    }

    let line = case
        .gcc_fallback_line
        .expect("markerless entry case has an explicit conservative expectation");

    image
        .statement_rows()
        .find(|row| {
            instance.contains(row.address)
                && row.flags.is_statement()
                && row
                    .location
                    .as_ref()
                    .is_some_and(|location| location.line.get() == line)
        })
        .map_or_else(
            || {
                panic!(
                    "{} missing expected markerless entry line {line}",
                    case.fixture
                )
            },
            |row| row.address,
        )
}

async fn assert_entry_stop(
    scenario: &Scenario,
    case: &EntryBoundaryCase,
    expected: microscope::ImageAddress,
) {
    let location = scenario
        .operation(
            "physical entry location",
            scenario.handle().current_location(),
        )
        .await;
    assert_eq!(location.image.address, expected, "{}", case.fixture);
    assert_eq!(
        location_function(&location),
        Some(case.function),
        "{}",
        case.fixture
    );

    let parameter = scenario
        .operation(
            "entry parameter value",
            scenario.handle().variable(case.parameter),
        )
        .await;

    assert_variable_value(
        &parameter,
        ScalarValue::Signed(i128::from(case.expected_value)),
    );
}

const fn relocate_image_address(
    address: microscope::ImageAddress,
    location: &microscope::ExecutionLocation,
) -> VirtualAddress {
    let load_bias = location
        .address
        .get()
        .checked_sub(location.image.address.get())
        .expect("runtime address includes the image load bias");

    VirtualAddress::new(
        load_bias
            .checked_add(address.get())
            .expect("relocated test address fits u64"),
    )
}

async fn run_go_to_breakpoint(scenario: &mut Scenario, fixture: &str) {
    let mut reason = scenario.run_to_stop().await;

    for _ in 0..32 {
        match reason {
            StopReason::Breakpoint { .. } => return,
            StopReason::Exception(ref exception) if exception.code == 23 => {
                reason = scenario.resume_to_stop().await;
            }
            _ => panic!("{fixture} stopped unexpectedly before its user breakpoint: {reason:?}"),
        }
    }

    panic!("{fixture} did not reach its user breakpoint after 32 runtime signals");
}

async fn resume_go_to_exit(scenario: &mut Scenario, fixture: &str) {
    let mut reason = scenario.resume_to_stop().await;

    for _ in 0..32 {
        match reason {
            StopReason::Exited(ExitStatus::Code(0)) => return,
            StopReason::Breakpoint { .. } => {
                reason = scenario.resume_to_stop().await;
            }

            // The runtime's SIGURG preemption never stops the program.
            _ => panic!("{fixture} stopped unexpectedly while exiting: {reason:?}"),
        }
    }

    panic!("{fixture} did not exit after 32 breakpoint stops");
}

fn assert_variable_value(variable: &microscope::Variable, expected: impl Into<ScalarValue>) {
    assert_eq!(
        available_value(&variable.state),
        &microscope::VariableValue::Scalar(expected.into())
    );
}

/// The text summary of a variable, or `None` when it has none.
fn text_of(variables: &[microscope::Variable], name: &str) -> Option<microscope::TextSummary> {
    let variable = variables
        .iter()
        .find(|variable| &*variable.name == name)
        .unwrap_or_else(|| panic!("no variable {name} in {variables:?}"));

    let VariableState::Available { text, .. } = &variable.state else {
        panic!("{name} is not available: {:?}", variable.state);
    };

    text.as_deref().cloned()
}

/// Waits until `process`'s main thread is a zombie: it exited, and nothing
/// has reaped it.
fn wait_for_zombie(process: ProcessId) {
    let stat = format!("/proc/{process}/stat");

    support::wait_until("the main thread exits", || {
        fs::read_to_string(&stat).is_ok_and(|stat| {
            // Fields resume after the command name's final parenthesis.
            stat.rsplit_once(')')
                .and_then(|(_, fields)| fields.split_whitespace().next())
                == Some("Z")
        })
    });
}
