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

/// What, on the build `entry`, an attached node's breakpoints get wrong.
pub async fn attach_lies(entry: &str) -> Vec<String> {
    attach_with_inspector(entry, false).await
}

async fn attach_with_inspector(entry: &str, existing: bool) -> Vec<String> {
    let mut lies = Vec::new();
    let scratch = ScratchDir::new("node-attach");
    let errors = scratch.path().join("stderr");
    let mut command = Command::new(node::node(entry));
    command.arg("--inspect-port=0");
    if existing {
        command.arg("--inspect=127.0.0.1:0");
    }
    let mut child =
        ExternalProcess::spawn_command(command.arg(node::fixture(FIXTURE)).env_clear().stderr(
            Stdio::from(std::fs::File::create(&errors).expect("the fixture's errors")),
        ));
    let before = std::fs::read_to_string(&errors).expect("the initial stderr");
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
    let mut running = scenario.start_resuming().await;
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
    nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(i32::try_from(child.process_id().get()).expect("native PID")),
        nix::sys::signal::Signal::SIGUSR1,
    )
    .expect("send an application signal after inspector activation");
    assert!(matches!(
        tokio::time::timeout(std::time::Duration::from_secs(5), running)
            .await.expect("the external SIGUSR1 must stop").expect("the run task").expect("the run"),
        StopReason::Exception(info) if info.code == 10
    ));
    running = scenario.start_resuming().await;
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
        || printed
            .strip_prefix(&before)
            .unwrap_or(&printed)
            .contains("ws://")
    {
        lies.push(format!(
            "node {entry}: stopped at lines {seen:?}, then {reason:?}; stderr:\n{printed}"
        ));
    }
    scenario.shutdown().await;
    lies
}

/// On every build, an attached node stops at a JavaScript breakpoint on
/// its main thread and then in a worker it starts after the attach, and
/// prints no inspector address.
#[tokio::test]
async fn an_attached_node_stops_in_javascript_and_in_a_new_worker() {
    let mut lies = Vec::new();
    for entry in MATRIX {
        lies.extend(attach_lies(entry).await);
    }
    assert!(lies.is_empty(), "{}", lies.join("\n"));
}

#[tokio::test]
async fn attaching_finds_an_existing_inspector_beside_an_application_listener() {
    let mut lies = Vec::new();
    for entry in MATRIX {
        lies.extend(attach_with_inspector(entry, true).await);
    }
    assert!(lies.is_empty(), "{}", lies.join("\n"));
}

#[tokio::test]
async fn attaching_does_not_invoke_a_javascript_sigusr1_handler() {
    for entry in MATRIX {
        let scratch = ScratchDir::new("node-attach-signal");
        let report = scratch.path().join("signals");
        let mut child = ExternalProcess::spawn_command(
            Command::new(node::node(entry))
                .arg("--inspect-port=0")
                .arg(node::fixture("attach-signal.js"))
                .arg(&report)
                .env_clear(),
        );
        let debugger = child.attach().await;
        let mut scenario = Scenario::attached(format!("node {entry} custom SIGUSR1"), debugger);
        let running = scenario.start_resuming().await;
        child.release();
        assert_eq!(
            running.await.expect("the run task").expect("the run"),
            StopReason::Exited(ExitStatus::Code(0)),
            "node {entry} exits normally"
        );
        scenario.shutdown().await;
        assert_eq!(
            std::fs::read_to_string(&report).expect("the signal report"),
            "",
            "node {entry}: attach invoked the application's SIGUSR1 listener"
        );
    }
}

#[tokio::test]
async fn detach_closes_only_the_inspector_server_created_by_attach() {
    for entry in MATRIX {
        for borrowed in [false, true] {
            let scratch = ScratchDir::new("node-detach");
            let report = scratch.path().join("report");
            let mut command = Command::new(node::node(entry));
            command.arg("--inspect-port=0");
            if borrowed {
                command.arg("--inspect=127.0.0.1:0");
            }
            let mut child = ExternalProcess::spawn_command(
                command
                    .arg(node::fixture("attach-detach.js"))
                    .arg(&report)
                    .env_clear(),
            );
            let debugger = child.attach().await;
            let mut scenario =
                Scenario::attached(format!("node {entry} detach borrowed={borrowed}"), debugger);
            scenario
                .operation(
                    "add breakpoint",
                    scenario.handle().add_breakpoint_with(
                        BreakpointSpec::Source {
                            path: node::fixture("attach-detach.js"),
                            line: LineNumber::new(9).expect("a line"),
                        },
                        BreakpointOptions {
                            pending: true,
                            ..BreakpointOptions::default()
                        },
                    ),
                )
                .await;
            let running = scenario.start_resuming().await;
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
            loop {
                let snapshot = scenario.snapshot().await;
                if !snapshot.breakpoints[0].locations.is_empty()
                    && snapshot.breakpoints[0].pending_locations.is_empty()
                {
                    break;
                }
                assert!(
                    std::time::Instant::now() < deadline,
                    "node {entry}: breakpoint never arms"
                );
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
            child.release();
            assert!(matches!(
                tokio::time::timeout(std::time::Duration::from_secs(20), running)
                    .await
                    .expect("breakpoint deadline")
                    .expect("run task")
                    .expect("run"),
                StopReason::Breakpoint { .. }
            ));
            let before: serde_json::Value =
                serde_json::from_slice(&std::fs::read(&report).expect("before report"))
                    .expect("JSON");
            let old = before["before"]
                .as_str()
                .expect("inspector open at breakpoint");
            let address: std::net::SocketAddr = old
                .strip_prefix("ws://")
                .expect("URL")
                .split('/')
                .next()
                .expect("authority")
                .parse()
                .expect("address");
            scenario.shutdown().await;
            assert_eq!(
                std::net::TcpStream::connect_timeout(&address, std::time::Duration::from_secs(1))
                    .is_ok(),
                borrowed,
                "node {entry}: old listener borrowed={borrowed}"
            );
            child.release();
            let after = detached_report(&report, entry, borrowed).await;
            assert!(
                child.wait().success(),
                "node {entry}: detached process survives"
            );
            assert_eq!(
                after["after"],
                if borrowed {
                    before["before"].clone()
                } else {
                    serde_json::Value::Null
                },
                "node {entry}: inspector state after detach"
            );
        }
    }
}

#[tokio::test]
async fn shutdown_before_first_resume_does_not_leave_an_inspector() {
    for entry in MATRIX {
        let scratch = ScratchDir::new("node-early-detach");
        let report = scratch.path().join("report");
        let mut child = ExternalProcess::spawn_command(
            Command::new(node::node(entry))
                .arg("--inspect-port=0")
                .arg(node::fixture("attach-detach.js"))
                .arg(&report)
                .env_clear(),
        );
        let debugger = child.attach().await;
        let scenario = Scenario::attached(format!("node {entry} early detach"), debugger);
        scenario
            .operation(
                "add breakpoint",
                scenario.handle().add_breakpoint_with(
                    BreakpointSpec::Source {
                        path: node::fixture("attach-detach.js"),
                        line: LineNumber::new(9).expect("a line"),
                    },
                    BreakpointOptions {
                        pending: true,
                        ..BreakpointOptions::default()
                    },
                ),
            )
            .await;
        scenario.shutdown().await;
        child.release();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !report.exists() {
            assert!(
                std::time::Instant::now() < deadline,
                "node {entry}: no first report"
            );
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        child.release();
        let after = detached_report(&report, entry, false).await;
        assert!(child.wait().success(), "node {entry}: process survives");
        assert_eq!(after, serde_json::json!({"before": null, "after": null}));
    }
}

#[tokio::test]
async fn shutdown_finishes_an_inspector_start_already_requested() {
    for entry in MATRIX {
        let scratch = ScratchDir::new("node-starting-detach");
        let report = scratch.path().join("report");
        let mut child = ExternalProcess::spawn_command(
            Command::new(node::node(entry))
                .arg("--inspect-port=0")
                .arg(node::fixture("attach-detach.js"))
                .arg(&report)
                .env_clear(),
        );
        let debugger = child.attach().await;
        let mut scenario = Scenario::attached(format!("node {entry} starting detach"), debugger);
        scenario
            .operation(
                "stop the inspector watchdog before it requests startup",
                scenario
                    .handle()
                    .add_breakpoint(BreakpointSpec::Function("uv_async_send".into())),
            )
            .await;
        assert!(matches!(
            scenario.resume_to_stop().await,
            StopReason::Breakpoint {
                address: Some(_),
                ..
            }
        ));
        scenario.shutdown().await;
        child.release();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !report.exists() {
            assert!(
                std::time::Instant::now() < deadline,
                "node {entry}: no report"
            );
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        child.release();
        let after = detached_report(&report, entry, false).await;
        assert!(child.wait().success(), "node {entry}: process survives");
        assert_eq!(after, serde_json::json!({"before": null, "after": null}));
    }
}

async fn detached_report(
    report: &std::path::Path,
    entry: &str,
    borrowed: bool,
) -> serde_json::Value {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        let report: serde_json::Value =
            serde_json::from_slice(&std::fs::read(report).expect("live report"))
                .unwrap_or(serde_json::Value::Null);
        if report.get("after").is_some() {
            return report;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "node {entry}: detached fixture cannot progress, borrowed={borrowed}"
        );
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
}
