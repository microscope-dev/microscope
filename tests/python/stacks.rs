//! Python programs' stacks at native stops.

use microscope::CodeRole;

use crate::session::PythonSession;

/// The interpreters with debug files, whose call machinery DWARF names.
const DESCRIBED: [&str; 3] = ["3.12", "3.13", "3.14"];

/// The evaluation loop and the calls around it are the interpreter's, in
/// the frames of every thread; the C functions Python calls are not.
#[tokio::test]
async fn the_interpreters_loop_and_calls_are_its_own() {
    for interpreter in DESCRIBED {
        let mut session = PythonSession::launch(interpreter, "stacks.py", &[]).await;
        let truth = session.stop_at("parked").await;
        let threads: Vec<u64> = truth
            .iter()
            .filter(|fields| fields[0] == "thread")
            .map(|fields| fields[1].parse().expect("a native thread id"))
            .collect();
        assert_eq!(threads.len(), 3, "{}: {truth:?}", session.context());
        for thread in threads {
            let trace = session.backtrace(microscope::ThreadId::new(thread)).await;
            let context = format!("{} thread {thread}: {trace:#?}", session.context());
            let named = |name: &str| {
                trace.frames.iter().find(|frame| {
                    frame
                        .function
                        .as_ref()
                        .is_some_and(|function| &*function.name == name)
                })
            };
            let evaluating = named("_PyEval_EvalFrameDefault").expect(&context);
            assert_eq!(evaluating.role, CodeRole::Interpreter, "{context}");
            if let Some(vectorcall) = named("_PyFunction_Vectorcall") {
                assert_eq!(vectorcall.role, CodeRole::Interpreter, "{context}");
            }
        }

        session.shutdown().await;
    }
}
