use microscope::{BreakpointOptions, BreakpointSpec, ExitStatus, StopReason};

use crate::oracle;
use crate::session::{LINES, NodeSession};
use crate::support::node;

const FIXTURE: &str = "functions.js";

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
    for entry in LINES {
        let breakpoints = NAMES.iter().map(|name| function(name)).collect();
        let (mut session, mut reason, _events) = NodeSession::unlaunched(entry, FIXTURE)
            .run_to_stop(breakpoints)
            .await;
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
        let running = session.scenario.start_resuming().await;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        let mut reasons = pending(&session).await;
        while !reasons[4].is_empty() {
            assert!(
                std::time::Instant::now() < deadline,
                "node {entry}: the nested function stayed pending: {reasons:?}"
            );
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
