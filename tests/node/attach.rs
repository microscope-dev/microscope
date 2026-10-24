//! Attaching to a node started without its inspector: microscope opens it
//! with SIGUSR1, hides its address, and connects to its socket, not to the
//! program's own; breakpoints then stop the main thread and a worker that
//! starts after the attach.

use std::process::{Command, Stdio};

use microscope::{BreakpointOptions, BreakpointSpec, Debugger, ExitStatus, LineNumber, StopReason};

use crate::oracle;
use crate::session::MATRIX;
use crate::support::{ExternalProcess, Scenario, ScratchDir, node};

const FIXTURE: &str = "attach.js";
const SERVED: u32 = 10;

/// On every build, an attached node stops at a JavaScript breakpoint on
/// its main thread and then in a worker it starts after the attach, and
/// prints no inspector address.
#[tokio::test]
async fn an_attached_node_stops_in_javascript_and_in_a_new_worker() {
    let mut lies = Vec::new();
    for entry in MATRIX {
        let scratch = ScratchDir::new("node-attach");
        let errors = scratch.path().join("stderr");
        let mut child = ExternalProcess::spawn_command(
            Command::new(node::node(entry))
                .arg(node::fixture(FIXTURE))
                .env_clear()
                .stderr(Stdio::from(
                    std::fs::File::create(&errors).expect("the fixture's errors"),
                )),
        );
        let debugger = Debugger::attach(child.process_id())
            .await
            .expect("attach the node");
        let mut scenario = Scenario::attached(format!("node {entry} attach"), debugger);
        scenario
            .operation(
                "add breakpoint",
                scenario.handle().add_breakpoint_with(
                    BreakpointSpec::Source {
                        path: node::fixture(FIXTURE),
                        line: LineNumber::new(u64::from(SERVED)).expect("a line"),
                    },
                    BreakpointOptions {
                        pending: true,
                        ..BreakpointOptions::default()
                    },
                ),
            )
            .await;
        // The inspector opens and V8 confirms the breakpoint while the node
        // runs, before the byte it waits for arrives.
        let running = scenario.start_resuming().await;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        loop {
            let snapshot = scenario.snapshot().await;
            let breakpoint = &snapshot.breakpoints[0];
            if !breakpoint.locations.is_empty() && breakpoint.pending_locations.is_empty() {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "node {entry}: the breakpoint stays pending: {breakpoint:?}"
            );
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        child.release();
        let reason = running.await.expect("the run task").expect("the run");
        let mut seen = Vec::new();
        let mut reason = reason;
        while let StopReason::Breakpoint { address: None, .. } = reason {
            let pause = oracle::selected(&scenario).await;
            seen.push(pause.line);
            assert!(seen.len() < 4, "node {entry}: too many stops");
            reason = scenario.resume_to_stop().await;
        }
        let printed = std::fs::read_to_string(&errors).unwrap_or_default();
        if seen != [SERVED, SERVED]
            || reason != StopReason::Exited(ExitStatus::Code(0))
            || printed.contains("ws://")
        {
            lies.push(format!(
                "node {entry}: stopped at lines {seen:?}, then {reason:?}; stderr:\n{printed}"
            ));
        }
        scenario.shutdown().await;
    }
    assert!(lies.is_empty(), "{}", lies.join("\n"));
}
