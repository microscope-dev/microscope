//! Steps into a Node-API addon's C functions and back. A step into a call
//! of one stops at the C function's first statement, which the addon's
//! own source marks; a step out of it, or over its last line, ends where
//! V8's own steps from the call end under a plain CDP client
//! (`oracle.rs`): `stepInto` from a call of a function that calls no
//! JavaScript, and `stepInto` then `stepOut` from one that calls back.

use microscope::{BreakpointOptions, BreakpointSpec, LineNumber, StepKind, StopReason};

use crate::oracle::{self, Command, Pause, Planted, Skipping};
use crate::session::{MATRIX, NodeSession};
use crate::support::{Scenario, node};

const FIXTURE: &str = "addon-steps.js";

/// The line of `text` that carries `marker`.
fn marked(text: &str, marker: &str) -> u64 {
    let index = text
        .lines()
        .position(|line| line.contains(marker))
        .unwrap_or_else(|| panic!("no line marked {marker}"));
    index as u64 + 1
}

/// The fixture's line marked `// marker`.
fn script_line(marker: &str) -> u64 {
    let text = std::fs::read_to_string(node::fixture(FIXTURE)).expect("read the fixture");
    marked(&text, &format!("// {marker}"))
}

/// The addon's C line marked `/* marker */`.
fn addon_line(marker: &str) -> u64 {
    let text = std::fs::read_to_string(node::fixture("addon.c")).expect("read the addon");
    marked(&text, &format!("/* {marker} */"))
}

fn breakpoint(marker: &str) -> (BreakpointSpec, BreakpointOptions) {
    (
        BreakpointSpec::Source {
            path: node::fixture(FIXTURE),
            line: LineNumber::new(script_line(marker)).expect("a line"),
        },
        BreakpointOptions {
            pending: true,
            ..BreakpointOptions::default()
        },
    )
}

/// Where V8 pauses from the breakpoint at `marker` doing `commands`.
fn v8_pauses(entry: &str, marker: &str, commands: &[Command]) -> Vec<Pause> {
    let mut pauses = oracle::pauses(
        entry,
        FIXTURE,
        &[Planted {
            line: script_line(marker),
            condition: None,
        }],
        (commands, Skipping::default()),
        "none",
    );
    pauses.truncate(commands.len() + 1);
    pauses
}

/// The innermost frame's function and line at the stop.
async fn native_place(scenario: &Scenario) -> (String, u64) {
    let trace = scenario
        .operation("backtrace", scenario.handle().backtrace())
        .await;
    let innermost = &trace.frames[0];
    (
        innermost
            .function
            .as_ref()
            .map(|function| function.name.to_string())
            .unwrap_or_default(),
        innermost
            .source
            .as_ref()
            .map_or(0, |source| source.line.get()),
    )
}

/// Steps by `kind`, which must end as a step does.
async fn step(session: &mut NodeSession, kind: StepKind) {
    let context = session.context();
    let reason = session.scenario.step_to_stop(kind).await;
    assert!(
        matches!(reason, StopReason::Step { .. }),
        "{context}: {kind:?} ended for {reason:?}\n{}",
        session.output()
    );
}

/// Into `add`, which calls no JavaScript, and out of it again.
async fn into_add_and_out(entry: &str, lies: &mut Vec<String>) {
    let expected = v8_pauses(entry, "CALLS_ADD", &[Command::Into]);
    let (mut session, _, _) = NodeSession::unlaunched(entry, FIXTURE)
        .run_to_stop(vec![breakpoint("CALLS_ADD")])
        .await;
    let context = session.context();
    let mut seen = vec![oracle::selected(&session.scenario).await];

    step(&mut session, StepKind::IntoSource).await;
    let place = native_place(&session.scenario).await;
    if place != ("add".to_owned(), addon_line("ADD_FIRST")) {
        lies.push(format!("{context}: a step into add() stopped at {place:?}"));
    }
    step(&mut session, StepKind::Out).await;
    seen.push(oracle::selected(&session.scenario).await);
    for difference in oracle::differences(&seen, &expected) {
        lies.push(format!("{context}: out of add(): {difference}"));
    }
    session.shutdown().await;
}

/// Into `inspect`, then by `back` to JavaScript: over each of its lines,
/// the one that calls `cb()` among them, until a step leaves it; or out.
async fn into_inspect_and_back(entry: &str, back: StepKind, lies: &mut Vec<String>) {
    let expected = v8_pauses(entry, "CALLS_INSPECT", &[Command::Into, Command::Out]);
    let (mut session, _, _) = NodeSession::unlaunched(entry, FIXTURE)
        .run_to_stop(vec![breakpoint("CALLS_INSPECT")])
        .await;
    let context = session.context();
    let first = oracle::selected(&session.scenario).await;

    step(&mut session, StepKind::IntoSource).await;
    let place = native_place(&session.scenario).await;
    if place != ("inspect".to_owned(), addon_line("INSPECT_FIRST")) {
        lies.push(format!(
            "{context}: a step into inspect() stopped at {place:?}"
        ));
    }
    let calls = addon_line("INSPECT_CALLS");
    let mut past_the_call = back == StepKind::Out;
    for _ in 0..30 {
        step(&mut session, back).await;
        let place = native_place(&session.scenario).await;
        if place.0 != "inspect" {
            break;
        }
        past_the_call |= place.1 > calls;
    }
    if !past_the_call {
        lies.push(format!(
            "{context}: {back:?} never stopped past the line calling cb()"
        ));
    }
    let seen = [first, oracle::selected(&session.scenario).await];
    let expected = [expected[0].clone(), expected[2].clone()];
    for difference in oracle::differences(&seen, &expected) {
        lies.push(format!("{context}: {back:?} from inspect(): {difference}"));
    }
    session.shutdown().await;
}

/// Into `inspect`, then on: V8's step back stops nothing.
async fn into_inspect_and_on(entry: &str, lies: &mut Vec<String>) {
    let (mut session, _, _) = NodeSession::unlaunched(entry, FIXTURE)
        .run_to_stop(vec![breakpoint("CALLS_INSPECT")])
        .await;
    let context = session.context();
    step(&mut session, StepKind::IntoSource).await;
    let reason = session.scenario.resume_to_stop().await;
    let output = session.output();
    if !matches!(reason, StopReason::Exited(_)) || !output.contains("ADDON STEPS DONE") {
        lies.push(format!(
            "{context}: going on from inspect() stopped for {reason:?}: {output}"
        ));
    }
    session.shutdown().await;
}

/// Where, on the build `entry`, a step into an addon and back does not
/// stop where it should.
pub async fn lies(entry: &str) -> Vec<String> {
    let mut lies = Vec::new();
    into_add_and_out(entry, &mut lies).await;
    into_inspect_and_back(entry, StepKind::OverSource, &mut lies).await;
    into_inspect_and_back(entry, StepKind::Out, &mut lies).await;
    into_inspect_and_on(entry, &mut lies).await;
    lies
}

#[tokio::test]
async fn steps_enter_an_addons_c_functions_and_come_back_where_v8_does() {
    let mut found = Vec::new();
    for entry in MATRIX {
        found.extend(lies(entry).await);
    }
    assert!(found.is_empty(), "{}", found.join("\n"));
}
