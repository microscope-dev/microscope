//! The comparisons the suite rests on, each shown to fail on the lies it
//! exists to catch, and the attach path held to the same truth as launch.

use microscope::{FrameKind, ThreadId};

use crate::locals::{PARKED, Stop, assert_locals, check_locals, reported_locals};
use crate::session::PythonSession;
use crate::stacks::{CodeLines, check_frames, reported_threads};

/// The primary pair, which the plan runs the client and path scenarios
/// on.
const PRIMARY: [&str; 2] = ["3.14", "3.14t"];

/// The comparisons fail on a dropped frame, a wrong line, a swapped
/// thread, a wrong value, and a missing local.
#[tokio::test]
async fn the_truth_fails_on_the_lies_it_looks_for() {
    let mut session = PythonSession::launch("3.14", "stacks.py", &[]).await;
    let truth = session.stop_at("parked").await;
    let lines = CodeLines::of(&session.report());
    let parked: Vec<_> = reported_threads(&truth)
        .into_iter()
        .filter(|(_, printer, _)| !printer)
        .collect();
    let [(deep, _, reported), (shallow, _, _)] = &parked[..] else {
        panic!("two parked threads: {parked:?}");
    };
    let trace = session.backtrace(ThreadId::new(*deep)).await;
    check_frames(&trace, reported, 0).expect("the truth holds");
    lines.check(&trace).expect("each line is its code's");

    // A frame of the fixture's own code, which its code table covers.
    let python = trace
        .frames
        .iter()
        .position(|frame| {
            matches!(frame.kind, FrameKind::Interpreted { .. })
                && frame
                    .recorded
                    .as_ref()
                    .is_some_and(|recorded| &*recorded.function == "leaf")
        })
        .expect("the frame of leaf");
    let mut dropped = trace.clone();
    let mut frames = trace.frames.to_vec();
    frames.remove(python);
    dropped.frames = frames.into();
    assert!(
        check_frames(&dropped, reported, 0).is_err(),
        "a dropped frame"
    );

    let mut moved = trace.clone();
    let mut frames = trace.frames.to_vec();
    let place = frames[python]
        .recorded
        .as_mut()
        .and_then(|recorded| recorded.place.as_mut())
        .expect("a recorded place");
    place.line += 1000;
    moved.frames = frames.into();
    assert!(check_frames(&moved, reported, 0).is_err(), "a wrong line");
    assert!(lines.check(&moved).is_err(), "a line its code has not");

    let other = session.backtrace(ThreadId::new(*shallow)).await;
    assert!(
        check_frames(&other, reported, 0).is_err(),
        "a swapped thread"
    );
    session.shutdown().await;

    let mut session = PythonSession::launch("3.14", "locals.py", &[]).await;
    let truth = session.stop_at("parked").await;
    let (function, parameters) = PARKED[0];
    let (thread, expected) = reported_locals(&truth)
        .into_iter()
        .find_map(|((thread, name), locals)| (name == function).then_some((thread, locals)))
        .expect("CPython reported the function");
    let stop = Stop {
        id: session.stop().await,
        scenario: &session.scenario,
        context: session.context(),
    };
    let variables = stop.variables(thread, function).await;
    check_locals(function, parameters, &variables, &expected).expect("the truth holds");

    let mut wrong = expected.clone();
    let small = wrong
        .iter_mut()
        .find(|(name, _)| name == "small")
        .expect("the local small");
    small.1 = "8".to_owned();
    assert!(
        check_locals(function, parameters, &variables, &wrong).is_err(),
        "a wrong value"
    );
    let mut missing = expected.clone();
    missing.remove(1);
    assert!(
        check_locals(function, parameters, &variables, &missing).is_err(),
        "a missing local"
    );
    session.shutdown().await;
}

/// A process the debugger attaches to, parked at its checkpoint, shows
/// the frames and locals CPython reports, as a launched one does.
#[tokio::test]
async fn an_attached_process_shows_what_cpython_reports() {
    for interpreter in PRIMARY {
        let (mut session, truth) =
            PythonSession::attach_at(interpreter, "locals.py", "parked").await;
        let lines = CodeLines::of(&session.report());
        for (thread, printer, reported) in reported_threads(&truth) {
            let trace = session.backtrace(ThreadId::new(thread)).await;
            let context = format!("{} thread {thread}: {trace:#?}", session.context());

            // The printer reported its frames from its checkpoint's
            // caller, and parks in two frames beneath it.
            let beneath = if printer { 2 } else { 0 };
            check_frames(&trace, &reported, beneath)
                .unwrap_or_else(|lie| panic!("{context}\n{lie}"));
            lines
                .check(&trace)
                .unwrap_or_else(|lie| panic!("{context}\n{lie}"));
        }

        let stop = Stop {
            id: session.stop().await,
            scenario: &session.scenario,
            context: session.context(),
        };
        assert_locals(&stop, &truth).await;
        session.shutdown().await;
    }
}
