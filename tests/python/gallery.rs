use microscope::{
    BreakpointOptions, BreakpointSpec, Condition, ConditionOwner, DebuggerEvent, ExecutionContext,
    FrameKind, InferiorState, StackFrameId, StopContext, StopReason,
};

use crate::expressions::{Outcome, outcome};
use crate::extension::{INTERPRETERS, extension_directory};
use crate::locals::Stop;
use crate::session::PythonSession;

const CHECKPOINTS: [&str; 5] = ["scalars", "text", "containers", "objects", "library"];

fn reached(condition: &str) -> Vec<(BreakpointSpec, BreakpointOptions)> {
    vec![(
        BreakpointSpec::Function("reached".to_owned()),
        BreakpointOptions {
            condition: Some(Condition::parse(condition).expect("a condition")),
            pending: true,
            ..BreakpointOptions::default()
        },
    )]
}

async fn stopped(session: &mut PythonSession) -> (Stop<'_>, StopContext, StopContext) {
    let snapshot = session.scenario.snapshot().await;
    let InferiorState::Stopped {
        stop_id, thread_id, ..
    } = snapshot.inferior
    else {
        panic!("{}: not stopped", session.context());
    };

    let at = |frame| StopContext {
        stop: stop_id,
        execution: ExecutionContext::Thread(thread_id),
        frame,
    };
    let trace = session
        .scenario
        .operation(
            "backtrace",
            session
                .scenario
                .handle()
                .at(at(StackFrameId::INNERMOST))
                .backtrace(),
        )
        .await;
    let caller = trace
        .frames
        .iter()
        .find(|frame| matches!(frame.kind, FrameKind::Interpreted { .. }))
        .unwrap_or_else(|| panic!("{}: no Python frame: {trace:#?}", session.context()));
    let caller = at(caller.id);
    let stop = Stop {
        id: stop_id,
        scenario: &session.scenario,
        context: session.context(),
    };

    (stop, at(StackFrameId::INNERMOST), caller)
}

#[tokio::test]
async fn each_value_is_what_the_gallery_says_where_a_python_condition_stops_it() {
    for interpreter in INTERPRETERS {
        let directory = extension_directory(interpreter);
        let (mut session, mut reason, _) = PythonSession::launch_to_stop(
            interpreter,
            "values.py",
            &[&directory],
            reached(r#"(py)checkpoint != "skipped""#),
        )
        .await;
        let mut lies = Vec::new();
        for checkpoint in CHECKPOINTS {
            assert!(
                matches!(reason, StopReason::Breakpoint { .. }),
                "{}: {checkpoint}: {reason:?}",
                session.context()
            );
            let report = session.report();
            let (stop, innermost, caller) = stopped(&mut session).await;
            let quoted = format!("'{checkpoint}'");
            match outcome(&stop, innermost, "checkpoint").await {
                Outcome::Value(value) if value == quoted => {}
                other => {
                    lies.push(format!(
                        "stopped where checkpoint is {other:?}, not {quoted}"
                    ));
                }
            }

            let truths: Vec<_> = report
                .lines()
                .filter(|fields| fields.len() == 4 && fields[0] == checkpoint)
                .collect();
            assert!(
                !truths.is_empty(),
                "{}: {checkpoint} said nothing in {:?}",
                stop.context,
                report.lines().collect::<Vec<_>>()
            );
            for fields in truths {
                let (path, value) = (&fields[1], &fields[3]);
                match outcome(&stop, caller, path).await {
                    Outcome::Value(shown) if shown == *value => {}
                    other => lies.push(format!("{checkpoint}: {path} is {other:?}, not {value}")),
                }
            }

            reason = session.scenario.resume_to_stop().await;
        }

        assert!(
            matches!(reason, StopReason::Exited { .. }),
            "{}: after the last checkpoint: {reason:?}",
            session.context()
        );
        assert!(
            lies.is_empty(),
            "{}:\n{}",
            session.context(),
            lies.join("\n")
        );
        session.shutdown().await;
    }
}

#[tokio::test]
async fn a_python_condition_that_cannot_be_judged_stops_and_says_why() {
    let interpreter = INTERPRETERS[1];
    let directory = extension_directory(interpreter);
    let (mut session, reason, mut events) = PythonSession::launch_to_stop(
        interpreter,
        "values.py",
        &[&directory],
        reached("(py)checkpoint < 3"),
    )
    .await;
    let StopReason::Breakpoint { hits, .. } = &reason else {
        panic!("{}: {reason:?}", session.context());
    };

    let failures: Vec<_> = std::iter::from_fn(|| events.try_recv().ok())
        .filter_map(|event| match event {
            DebuggerEvent::ConditionFailed { owner, error, .. } => Some((owner, error)),
            _ => None,
        })
        .collect();
    let [(owner, error)] = failures.as_slice() else {
        panic!("{}: one failure: {failures:?}", session.context());
    };

    assert_eq!(*owner, ConditionOwner::Breakpoint(hits[0].breakpoint));
    assert!(
        error.contains("str"),
        "{}: the failure names the operand's type: {error}",
        session.context()
    );

    let (stop, innermost, _) = stopped(&mut session).await;
    match outcome(&stop, innermost, "checkpoint").await {
        Outcome::Value(value) if value == "'skipped'" => {}
        other => panic!("{}: stopped at {other:?}", stop.context),
    }

    session.shutdown().await;
}
