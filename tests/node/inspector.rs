//! The inspector microscope's preload opens in a launched node: microscope
//! reads its address from the process and hides it (F29), so Node neither
//! prints it nor lists it, and the program's own variables are put back
//! (F31).

use std::process::Stdio;

use microscope::{ExitStatus, LaunchOptions, StopReason};

use crate::session::MATRIX;
use crate::support::{Scenario, ScratchDir, node};

/// What a user set, which the program must see as it was.
const USER_OPTIONS: &str = "--no-warnings";

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
        !text.contains("ws://") && !text.contains("Debugger listening"),
        "{entry}: the address is printed:\n{text}"
    );
}

#[tokio::test]
async fn a_launched_nodes_inspector_is_opened_and_shown_to_no_one() {
    for entry in MATRIX {
        check_hidden(entry).await;
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
