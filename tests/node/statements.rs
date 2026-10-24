//! JavaScript `debugger` statements stop where V8 pauses at them under a
//! plain CDP client (`oracle.rs`), with the frame's locals, unless their
//! script is skipped.

use microscope::{ExitStatus, StepSkips, StopReason};

use crate::oracle::{self, Pause, Skipping};
use crate::session::{MATRIX, NodeSession};

const FIXTURE: &str = "statements.js";

/// Runs the fixture on one build, skipping `directories`, and returns
/// where microscope stopped, where V8 paused, and `doubled` at the first
/// stop.
async fn run(entry: &str, directories: &[&str]) -> (Vec<Pause>, Vec<Pause>, String) {
    let skipping = Skipping {
        directories,
        enter: false,
    };
    let expected = oracle::pauses(entry, FIXTURE, &[], (&[], skipping), "none");
    let session = NodeSession::unlaunched(entry, FIXTURE);
    let globs: Vec<_> = directories
        .iter()
        .map(|directory| format!("**{directory}**"))
        .collect();
    let skips = StepSkips::parse(&globs).expect("skip patterns");
    session
        .scenario
        .operation("skip", session.scenario.handle().set_step_skips(skips))
        .await;
    let (mut session, mut reason, _events) = session.run_to_stop(Vec::new()).await;
    let context = session.context();
    let (mut seen, mut doubled) = (Vec::new(), String::new());
    loop {
        match &reason {
            StopReason::ProgramBreakpoint { address: None } => {
                seen.push(oracle::selected(&session.scenario).await);
                if seen.len() == 1 {
                    doubled = local(&session, "doubled").await;
                }
            }
            StopReason::Exited(ExitStatus::Code(0)) => break,
            other => panic!("{context}: {other:?}\n{}", session.output()),
        }
        assert!(seen.len() < 16, "{context}: too many stops");
        reason = session.scenario.resume_to_stop().await;
    }
    assert!(session.output().contains("DONE"), "{context}");
    session.shutdown().await;
    (seen, expected, doubled)
}

/// The summary of the selected frame's variable `name`.
async fn local(session: &NodeSession, name: &str) -> String {
    let variables = session
        .scenario
        .operation("variables", session.scenario.handle().variables())
        .await;
    variables
        .variables
        .iter()
        .find(|variable| &*variable.name == name)
        .map_or_else(
            || "absent".to_owned(),
            |variable| match &variable.state {
                microscope::VariableState::Available {
                    presentation: Some(presentation),
                    ..
                } => presentation.summary.to_string(),
                other => format!("{other:?}"),
            },
        )
}

/// On every build, each `debugger` statement the program runs stops where
/// V8 pauses at it, once each time it runs, except in a skipped script.
#[tokio::test]
async fn debugger_statements_stop_where_v8_pauses_at_them() {
    let mut lies = Vec::new();
    for entry in MATRIX {
        for directories in [&[][..], &["/vendor/"][..]] {
            let (seen, expected, doubled) = run(entry, directories).await;
            // first, halt unless skipped, and the loop's two.
            let count = if directories.is_empty() { 4 } else { 3 };
            if seen != expected || seen.len() != count {
                let show = |pauses: &[Pause]| {
                    pauses
                        .iter()
                        .map(ToString::to_string)
                        .collect::<Vec<_>>()
                        .join(", ")
                };
                lies.push(format!(
                    "node {entry} {directories:?}:\n  microscope {}\n  V8         {}",
                    show(&seen),
                    show(&expected)
                ));
            }
            if doubled != "6" {
                lies.push(format!("node {entry}: doubled is {doubled}"));
            }
        }
    }
    assert!(lies.is_empty(), "{}", lies.join("\n"));
}
