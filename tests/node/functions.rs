//! `js:NAME` function breakpoints: a function declared in a script stops
//! at its first call, though the script calls it before anything else
//! runs; so do an arrow held in a constant, a method, and a function in a
//! required module. A function nested in another is compiled only once
//! its parent runs, and is found when the program next resumes, so it
//! stops at its next call. A name nothing has stays pending with why, and
//! a qualified one is refused with the name to use.

use microscope::{BreakpointOptions, BreakpointSpec, ExitStatus, StopReason};

use crate::oracle;
use crate::session::{MATRIX, NodeSession};
use crate::support::node;

const FIXTURE: &str = "functions.js";

/// The line of `file` that carries `marker`.
fn line_of(file: &str, marker: &str) -> u32 {
    let text = std::fs::read_to_string(node::fixture(file)).expect("read the fixture");
    let index = text
        .lines()
        .position(|line| line.contains(&format!("// {marker}")))
        .expect("a marked line");
    u32::try_from(index + 1).expect("a small line")
}

fn function(name: &str) -> (BreakpointSpec, BreakpointOptions) {
    (
        BreakpointSpec::Function(format!("js:{name}")),
        BreakpointOptions {
            pending: true,
            ..BreakpointOptions::default()
        },
    )
}

/// The pending reasons of the run's breakpoints, in the order added.
async fn pending(session: &NodeSession) -> Vec<String> {
    let snapshot = session
        .scenario
        .operation("snapshot", session.scenario.handle().snapshot())
        .await;
    snapshot
        .breakpoints
        .iter()
        .map(|breakpoint| {
            breakpoint
                .pending_locations
                .iter()
                .map(|place| place.reason.to_string())
                .collect::<Vec<_>>()
                .join("; ")
        })
        .collect()
}

/// On every build, each function stops where its body begins, in the
/// order the program calls them, and the nested one once it is compiled.
#[tokio::test]
async fn function_breakpoints_stop_where_each_function_begins() {
    const NAMES: [&str; 6] = ["norm", "handler", "area", "helper", "inner", "never"];
    let expected = [
        ("norm", line_of(FIXTURE, "NORM")),
        ("handler", line_of(FIXTURE, "HANDLER")),
        ("area", line_of(FIXTURE, "AREA")),
        ("helper", line_of("functions_lib.js", "HELPER")),
        ("inner", line_of(FIXTURE, "INNER")),
    ];
    let mut lies = Vec::new();
    for entry in MATRIX {
        let breakpoints = NAMES.iter().map(|name| function(name)).collect();
        let (mut session, mut reason, _events) = NodeSession::unlaunched(entry, FIXTURE)
            .run_to_stop(breakpoints)
            .await;
        // Once Node's runtime is loaded, a qualified name is refused.
        let qualified = session
            .scenario
            .handle()
            .add_breakpoint_with(
                BreakpointSpec::Function("js:Shape.area".to_owned()),
                BreakpointOptions::default(),
            )
            .await
            .map(|breakpoint| breakpoint.id)
            .map_err(|error| error.to_string());
        let mut seen = Vec::new();
        while matches!(reason, StopReason::Breakpoint { .. }) {
            let pause = oracle::selected(&session.scenario).await;
            seen.push((pause.function, pause.line));
            if seen.len() == 4 {
                break;
            }
            reason = session.scenario.resume_to_stop().await;
        }
        // The program waits for its line, with the nested function
        // compiled, and the search that resumed it finds it.
        let running = session.scenario.start_resuming().await;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let mut reasons = pending(&session).await;
        while !reasons[4].is_empty() && std::time::Instant::now() < deadline {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            reasons = pending(&session).await;
        }
        session.feed();
        let mut reason = running.await.expect("the run task").expect("the run");
        if matches!(reason, StopReason::Breakpoint { .. }) {
            let pause = oracle::selected(&session.scenario).await;
            seen.push((pause.function, pause.line));
            reason = session.scenario.resume_to_stop().await;
        }
        let seen = seen
            .iter()
            .map(|(function, line)| (function.as_str(), *line))
            .collect::<Vec<_>>();
        let output = session.output();
        if seen != expected
            || reason != StopReason::Exited(ExitStatus::Code(0))
            || !output.contains("FIRST 2 5 2 6 4")
            || !output.contains("LATER 2")
            || !reasons[5].contains("V8 has compiled no function named never yet")
            || !qualified
                .as_ref()
                .is_err_and(|error| error.contains("use js:area"))
        {
            lies.push(format!(
                "node {entry}: stopped in {seen:?}, then {reason:?}; pending {reasons:?}; js:Shape.area {qualified:?}; output:\n{output}"
            ));
        }
        session.shutdown().await;
    }
    assert!(lies.is_empty(), "{}", lies.join("\n"));
}
