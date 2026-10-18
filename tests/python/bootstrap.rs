//! CPython 3.12 and 3.13, which no debugger can ask to run a script, take
//! the agent through the bootstrap microscope gives a launch. The program
//! and its children see their own environment and path, and the program's
//! own `sitecustomize` still runs. A breakpoint set as the program runs
//! reaches the agent through the bootstrap's hidden thread.

use microscope::{BreakpointSpec, LineNumber, StopReason};

use crate::session::PythonSession;
use crate::support::python;

const FIXTURE: &str = "bootstrap.py";

#[tokio::test]
async fn a_launch_bootstrap_is_invisible_and_takes_breakpoints_as_the_program_runs() {
    let site = python::fixture("site");
    let site = site.to_str().expect("a UTF-8 fixture path");
    let mut session =
        PythonSession::launch_with("3.12", FIXTURE, &[], &[("PYTHONPATH", site)]).await;
    session.wait_for_output("READY").await;
    let context = session.context();
    let output = session.output();
    for expected in [
        "SITE 1".to_owned(),
        format!("SEES {site} [] False"),
        "THREADS 1".to_owned(),
        format!("CHILD {site} [] False"),
    ] {
        assert!(
            output.lines().any(|line| line == expected),
            "{context}: no {expected:?} in:\n{output}"
        );
    }

    let text = std::fs::read_to_string(python::fixture(FIXTURE)).expect("read the fixture");
    let line = text
        .lines()
        .position(|line| line.ends_with("# LOOP_LINE"))
        .expect("a marked line");
    session
        .scenario
        .operation(
            "break in the loop",
            session
                .scenario
                .handle()
                .add_breakpoint(BreakpointSpec::Source {
                    path: python::fixture(FIXTURE),
                    line: LineNumber::new(line as u64 + 1).expect("a one-based line"),
                }),
        )
        .await;
    let reason = session.finish_run().await;
    assert!(
        matches!(reason, StopReason::Breakpoint { address: None, .. }),
        "{context}: {reason:?}: {}",
        session.output()
    );
    assert!(session.integer("count").await >= 0, "{context}");
    session.shutdown().await;
}
