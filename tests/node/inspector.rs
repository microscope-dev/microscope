//! The inspector microscope's preload opens in a launched node: microscope
//! reads its address from the process and hides it (F29), so Node neither
//! prints it nor lists it, and the program's own variables are put back
//! (F31).

use std::process::Stdio;
use std::time::Duration;

use microscope::{ExitStatus, LaunchOptions, StopReason};

use crate::session::MATRIX;
use crate::support::{Scenario, ScratchDir, node};

/// What a user set, which the program must see as it was.
const USER_OPTIONS: &str = "--no-warnings";

async fn inspector_lifetime(entry: &str, race: bool) -> (String, String) {
    let scratch = ScratchDir::new("node-inspector-lifetime");
    let output = scratch.path().join("output");
    let report = scratch.path().join("listing");
    let file = std::fs::File::create(&output).expect("output");
    let errors = file.try_clone().expect("stderr");
    let addon = Scenario::fixture("node-inspector-lifetime.node");
    let mut environment = vec![
        ("LD_PRELOAD".into(), Some(addon.clone().into_os_string())),
        ("LIFETIME_ADDON".into(), Some(addon.into_os_string())),
    ];
    if race {
        environment.push((
            "MICROSCOPE_LISTING_REPORT".into(),
            Some(report.clone().into_os_string()),
        ));
    }
    let mut scenario = Scenario::new(
        format!("node {entry} inspector lifetime"),
        node::node(entry),
    );
    let mut running = scenario
        .start_running_with(LaunchOptions {
            arguments: vec![node::fixture("inspector-lifetime.js").into_os_string()],
            environment,
            stdout: Some(Stdio::from(file)),
            stderr: Some(Stdio::from(errors)),
            ..LaunchOptions::default()
        })
        .await;
    let completed = tokio::time::timeout(Duration::from_secs(20), async {
        if race {
            loop {
                let text = std::fs::read_to_string(&report).unwrap_or_default();
                if text.split_once("\r\n\r\n").is_some_and(|(_, body)| {
                    serde_json::from_str::<serde_json::Value>(body).is_ok()
                }) {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        }
        let disclosed = std::fs::read_to_string(&report)
            .unwrap_or_default()
            .contains("webSocketDebuggerUrl");
        if disclosed {
            None
        } else {
            Some((&mut running).await)
        }
    })
    .await;
    scenario.shutdown().await;
    if let Some(ended) = completed.expect("inspector lifetime deadline") {
        assert_eq!(
            ended.expect("run task").expect("run"),
            StopReason::Exited(ExitStatus::Code(0)),
            "{entry}: {}",
            std::fs::read_to_string(&output).unwrap_or_default()
        );
    }
    (
        std::fs::read_to_string(output).expect("output"),
        std::fs::read_to_string(report).unwrap_or_default(),
    )
}

#[tokio::test]
async fn hidden_listings_release_their_native_uuid_allocations() {
    let mut failures = Vec::new();
    for entry in MATRIX {
        let (output, _) = inspector_lifetime(entry, false).await;
        if !output.lines().any(|line| line == "COPIES 0") {
            failures.push(format!("{entry}: {output}"));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[tokio::test]
async fn a_listing_queued_before_the_debuggers_upgrade_never_discloses_the_target() {
    let mut failures = Vec::new();
    for entry in MATRIX {
        let (_, listing) = inspector_lifetime(entry, true).await;
        let body = listing.split_once("\r\n\r\n").expect("HTTP response").1;
        if serde_json::from_str::<serde_json::Value>(body).expect("JSON listing")
            != serde_json::json!([])
        {
            failures.push(format!("{entry}: {body}"));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

/// Runs `inspector.js` under the build `entry` and checks that it ran to
/// its end, its inspector open, its address shown to no one, and its
/// `NODE_OPTIONS` as the user set it.
pub async fn check_hidden(entry: &str) {
    let scratch = ScratchDir::new("node-inspector");
    let output = scratch.path().join("output");
    let file = std::fs::File::create(&output).expect("create the fixture's output");
    let errors = file.try_clone().expect("share the fixture's output");
    let mut environment: Vec<_> = std::env::vars_os().map(|(name, _)| (name, None)).collect();
    environment.push(("NODE_OPTIONS".into(), Some(USER_OPTIONS.into())));

    let mut scenario = Scenario::new(format!("node {entry} inspector.js"), node::node(entry));
    let running = scenario
        .start_running_with(LaunchOptions {
            arguments: vec![node::fixture("inspector.js").into_os_string()],
            environment,
            stdin: Some(Stdio::null()),
            stdout: Some(Stdio::from(file)),
            stderr: Some(Stdio::from(errors)),
            ..LaunchOptions::default()
        })
        .await;
    let ended = running.await.expect("the run's task").expect("the run");
    scenario.shutdown().await;
    let text = std::fs::read_to_string(&output).expect("the fixture's output");

    assert_eq!(
        ended,
        StopReason::Exited(ExitStatus::Code(0)),
        "{entry}:\n{text}"
    );
    assert!(
        text.contains(&format!("ENV \"{USER_OPTIONS}\" null\n")),
        "{entry}: the program sees microscope's variables:\n{text}"
    );
    let target = text
        .lines()
        .find_map(|line| line.strip_prefix("TARGET "))
        .unwrap_or_else(|| panic!("{entry}: the preload opened no inspector:\n{text}"));
    assert!(
        target.len() == 36
            && target
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() || byte == b'-'),
        "{entry}: the target is not a UUID: {target:?}"
    );
    assert!(
        text.contains("LIST []\n"),
        "{entry}: the inspector lists its target:\n{text}"
    );
    assert_eq!(
        text.matches(target).count(),
        1,
        "{entry}: the address is printed:\n{text}"
    );
    assert!(
        !text.contains("ws://")
            && !text.contains("Debugger listening")
            && !text.contains("microscope ownership"),
        "{entry}: the address is printed:\n{text}"
    );
}

#[tokio::test]
async fn a_launched_nodes_inspector_is_opened_and_shown_to_no_one() {
    for entry in MATRIX {
        check_hidden(entry).await;
    }
}

async fn releasing_a_launched_child_closes_only_its_preloads_inspector(entry: &str) {
    use microscope::GroupEvent;

    crate::support::flight_recordings::watch_scenarios();
    for (borrowed, restricted) in [(true, false), (false, false), (false, true), (true, true)] {
        // Node 22 has no --allow-inspector permission flag.
        if restricted && crate::session::major(entry) == 22 {
            continue;
        }
        let scratch = ScratchDir::new("node-launch-release");
        let script = scratch.path().join("child.js");
        let report = scratch.path().join("report");
        let gate = scratch.path().join("gate");
        let options = release_child_options(entry, borrowed, restricted, &script, &report, &gate);
        let group = release_child_group(script).await;
        let mut events = group.subscribe();
        group
            .root()
            .launch_with(options)
            .await
            .expect("launch shell");
        let member = tokio::time::timeout(Duration::from_secs(30), async {
                loop {
                    match events.recv().await.expect("group event") {
                        GroupEvent::GroupStopped { member, reason } => {
                            assert!(matches!(reason, StopReason::Breakpoint { .. }), "{entry}: {reason:?}");
                            break member;
                        }
                        GroupEvent::GroupExited => panic!("{entry}: exited before breakpoint, borrowed={borrowed} restricted={restricted}"),
                        _ => {}
                    }
                }
            })
            .await
            .expect("child breakpoint deadline");
        assert_ne!(member, 1, "the launched child stopped");
        let before: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&report).expect("before report"))
                .expect("JSON report");
        let address: std::net::SocketAddr = before["before"]
            .as_str()
            .expect("open inspector")
            .strip_prefix("ws://")
            .expect("URL")
            .split('/')
            .next()
            .expect("authority")
            .parse()
            .expect("address");
        let released = group.detach(member).await;
        let endpoint_open =
            std::net::TcpStream::connect_timeout(&address, Duration::from_secs(1)).is_ok();
        std::fs::write(&gate, "go").expect("release child gate");
        group
            .continue_all()
            .await
            .expect("resume parent to reap child");
        let after = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let value: serde_json::Value =
                    serde_json::from_slice(&std::fs::read(&report).expect("report"))
                        .unwrap_or_default();
                if value.get("after").is_some() {
                    break value;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await;
        let reaped = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if matches!(
                    events.recv().await.expect("group event"),
                    GroupEvent::GroupExited
                ) {
                    break;
                }
            }
        })
        .await;
        group.shutdown().await.expect("shutdown group");
        released.unwrap_or_else(|error| {
            panic!("{entry}: release borrowed={borrowed} restricted={restricted}: {error}")
        });
        reaped.expect("parent reaps the released child and exits");
        let after = after.expect("released child progresses");
        assert_eq!(
            endpoint_open, borrowed,
            "{entry}: listener borrowed={borrowed}"
        );
        assert_eq!(
            after["after"],
            if borrowed {
                before["before"].clone()
            } else {
                serde_json::Value::Null
            },
            "{entry}: inspector borrowed={borrowed}"
        );
    }
}

// Keep the build matrix out of a single test's stress deadline.
const _: () = assert!(MATRIX.len() == 6, "a release test for each build");

#[tokio::test]
async fn releasing_a_launched_child_closes_only_its_preloads_inspector_on_build_0() {
    releasing_a_launched_child_closes_only_its_preloads_inspector(MATRIX[0]).await;
}

#[tokio::test]
async fn releasing_a_launched_child_closes_only_its_preloads_inspector_on_build_1() {
    releasing_a_launched_child_closes_only_its_preloads_inspector(MATRIX[1]).await;
}

#[tokio::test]
async fn releasing_a_launched_child_closes_only_its_preloads_inspector_on_build_2() {
    releasing_a_launched_child_closes_only_its_preloads_inspector(MATRIX[2]).await;
}

#[tokio::test]
async fn releasing_a_launched_child_closes_only_its_preloads_inspector_on_build_3() {
    releasing_a_launched_child_closes_only_its_preloads_inspector(MATRIX[3]).await;
}

#[tokio::test]
async fn releasing_a_launched_child_closes_only_its_preloads_inspector_on_build_4() {
    releasing_a_launched_child_closes_only_its_preloads_inspector(MATRIX[4]).await;
}

#[tokio::test]
async fn releasing_a_launched_child_closes_only_its_preloads_inspector_on_build_5() {
    releasing_a_launched_child_closes_only_its_preloads_inspector(MATRIX[5]).await;
}

async fn release_child_group(script: std::path::PathBuf) -> microscope::ProcessGroup {
    use microscope::{
        BreakpointOptions, BreakpointSpec, FollowPolicy, LineNumber, ProcessGroup, Program,
    };

    let program =
        Program::load("/bin/sh", &microscope::DebugFileOptions::default()).expect("load shell");
    let group = ProcessGroup::new(program, FollowPolicy::default()).expect("start group");
    group
        .root()
        .add_breakpoint_with(
            BreakpointSpec::Source {
                path: script,
                line: LineNumber::new(9).expect("line"),
            },
            BreakpointOptions {
                pending: true,
                ..BreakpointOptions::default()
            },
        )
        .await
        .expect("pending JavaScript breakpoint");
    group
}

fn release_child_options(
    entry: &str,
    borrowed: bool,
    restricted: bool,
    script: &std::path::Path,
    report: &std::path::Path,
    gate: &std::path::Path,
) -> LaunchOptions {
    std::fs::write(
        script,
        r"'use strict';
const fs = require('fs');
const inspector = require('inspector');
const url = inspector.url;
const [report, gate] = process.argv.slice(2);
const before = url() ?? null;
fs.writeFileSync(report, JSON.stringify({before: before}));
function reached(n) {
  return n + 1;
}
Object.defineProperty(inspector, 'url', {get() { throw Error('user getter'); }});
Object.defineProperty(inspector, 'close', {get() { throw Error('user getter'); }});
reached(41);
const timer = setInterval(() => {
  if (!fs.existsSync(gate)) return;
  clearInterval(timer);
  fs.writeFileSync(report, JSON.stringify({before, after: url() ?? null}));
  process.exit(0);
}, 10);
",
    )
    .expect("write child");
    let mut arguments = vec![
        "-c".into(),
        "\"$@\" & wait".into(),
        "shell".into(),
        node::node(entry).into_os_string(),
    ];
    if borrowed {
        arguments.push("--inspect=127.0.0.1:0".into());
    }
    if restricted {
        arguments.extend([
            "--permission".into(),
            "--allow-inspector".into(),
            "--allow-fs-read=*".into(),
            format!("--allow-fs-write={}", report.display()).into(),
        ]);
    }
    arguments.extend([
        script.as_os_str().to_owned(),
        report.as_os_str().to_owned(),
        gate.as_os_str().to_owned(),
    ]);
    LaunchOptions {
        arguments,
        stdin: Some(Stdio::null()),
        stdout: Some(Stdio::null()),
        stderr: Some(Stdio::null()),
        ..LaunchOptions::default()
    }
}

#[tokio::test]
async fn user_preloads_wait_for_the_inspector_and_workers_keep_their_options() {
    for entry in MATRIX {
        let scratch = ScratchDir::new("node-preload-options");
        let preload = scratch.path().join("user \"quoted\\preload.cjs");
        std::fs::write(
            &preload,
            r"
if (require('worker_threads').isMainThread) {
  console.log('USER_PRELOAD ' + Boolean(require('inspector').url()));
}
",
        )
        .expect("write user preload");
        let quoted = preload
            .to_str()
            .expect("preload path")
            .replace('\\', "\\\\")
            .replace('"', "\\\"");
        let options = format!("--no-warnings --require \"{quoted}\"");
        let output = scratch.path().join("output");
        let file = std::fs::File::create(&output).expect("create output");
        let mut environment: Vec<_> = std::env::vars_os().map(|(name, _)| (name, None)).collect();
        environment.push(("NODE_OPTIONS".into(), Some(options.clone().into())));
        let script = r"
const {Worker} = require('worker_threads');
console.log('MAIN ' + JSON.stringify(process.env.NODE_OPTIONS));
const code = `require('worker_threads').parentPort.postMessage(process.env.NODE_OPTIONS ?? null)`;
new Worker(code, { eval: true }).on('message', value => console.log('INHERITED ' + JSON.stringify(value)));
new Worker(code, {eval:true, env:{...process.env, NODE_OPTIONS:'--no-deprecation'}})
  .on('message', value => console.log('EXPLICIT ' + JSON.stringify(value)));
";
        let mut scenario =
            Scenario::new(format!("node {entry} preload options"), node::node(entry));
        let running = scenario
            .start_running_with(LaunchOptions {
                arguments: vec!["-e".into(), script.into()],
                environment,
                stdin: Some(Stdio::null()),
                stderr: Some(Stdio::from(file.try_clone().expect("clone output"))),
                stdout: Some(Stdio::from(file)),
                ..LaunchOptions::default()
            })
            .await;
        let ended = running.await.expect("run task").expect("run");
        scenario.shutdown().await;
        let text = std::fs::read_to_string(&output).expect("read output");
        let expected = serde_json::to_string(&options).expect("JSON options");
        assert_eq!(
            ended,
            StopReason::Exited(ExitStatus::Code(0)),
            "{entry}: {text}"
        );
        assert!(text.contains("USER_PRELOAD true\n"), "{entry}: {text}");
        assert!(!text.contains("USER_PRELOAD false\n"), "{entry}: {text}");
        assert!(
            text.contains(&format!("MAIN {expected}\n")),
            "{entry}: {text}"
        );
        assert!(
            text.contains(&format!("INHERITED {expected}\n")),
            "{entry}: {text}"
        );
        assert!(
            text.contains("EXPLICIT \"--no-deprecation\"\n"),
            "{entry}: {text}"
        );
    }
}

/// A node whose permission model refuses the inspector runs on, and its
/// JavaScript breakpoints wait, saying which flag would let it open.
#[tokio::test]
async fn a_refused_inspector_names_the_flag_that_allows_it() {
    use microscope::{BreakpointOptions, BreakpointSpec, LineNumber};

    for entry in MATRIX {
        let mut session = crate::session::NodeSession::launch(
            entry,
            "functions.js",
            &["--permission", "--allow-fs-read=*"],
            &[],
        )
        .await;
        session.wrote("FIRST 2 5 2 6 4").await;
        session.pause().await;
        let added = session
            .scenario
            .handle()
            .add_breakpoint_with(
                BreakpointSpec::Source {
                    path: node::fixture("functions.js"),
                    line: LineNumber::new(10).expect("a line"),
                },
                BreakpointOptions {
                    pending: true,
                    ..BreakpointOptions::default()
                },
            )
            .await
            .expect("a pending breakpoint");
        let reasons = added
            .pending_locations
            .iter()
            .map(|pending| pending.reason.to_string())
            .collect::<Vec<_>>();
        assert_eq!(
            reasons,
            [
                "Node's inspector has not started; this node runs with --permission, under \
                 which Node opens no inspector without --allow-inspector"
            ],
            "{}",
            session.context()
        );
        session.shutdown().await;
    }
}
