//! Native backtraces of Node threads: through V8's builtins, which no
//! call-frame information describes, and through the code it compiles for
//! JavaScript, to where the thread began.

use microscope::{Backtrace, CoreDumpOptions, LoadedModuleSnapshot, ThreadId, UnwindTermination};

use crate::session::{CORES, MATRIX, NodeSession};
use crate::support::{Scenario, frame_modules};
use crate::truth::threads;

/// Holds one thread's native backtrace to a complete walk in which every
/// frame lies in a module, and returns how many frames ran V8's compiled
/// code and how many its interpreter.
pub fn check_thread(
    context: &str,
    main: bool,
    trace: &Backtrace,
    modules: &LoadedModuleSnapshot,
) -> (usize, usize) {
    let frames = frame_modules(trace, modules);
    assert_eq!(
        trace.termination,
        UnwindTermination::Complete,
        "{context}: {frames:#?}"
    );
    if main {
        let start = trace.frames.last().expect("frames");
        assert_eq!(
            start.symbol.as_ref().map(|symbol| &*symbol.name),
            Some("_start"),
            "{context}: {frames:#?}"
        );
    }
    let compiled = frames
        .iter()
        .filter(|(module, _)| module.starts_with("[V8 code "))
        .count();
    // Builtins have symbols, not debug information, where the build keeps
    // them; elsewhere an interpreted frame is one outside V8's compiled
    // code that runs a fixture's function.
    let interpreted = trace
        .frames
        .iter()
        .zip(&frames)
        .filter(|(frame, (module, _))| {
            frame.symbol.as_ref().map(|symbol| &*symbol.name)
                == Some("Builtins_InterpreterEntryTrampoline")
                || (!module.starts_with("[V8 code ")
                    && frame.recorded.as_ref().is_some_and(|recorded| {
                        recorded
                            .place
                            .as_ref()
                            .is_some_and(|place| place.path.contains("/tests/fixtures/node/"))
                    }))
        })
        .count();
    (compiled, interpreted)
}

/// On every build, each thread `stacks.js` parks unwinds natively to where
/// it began, through the interpreter's frames.
#[tokio::test]
async fn each_parked_thread_unwinds_through_v8_to_where_it_began() {
    for entry in MATRIX {
        let mut session = NodeSession::launch(entry, "stacks.js", &[], &[]).await;
        let reported = threads(&session.stop_at("parked").await);
        let scenario = &session.scenario;
        let modules = scenario
            .operation("modules", scenario.handle().loaded_modules())
            .await;
        for thread in &reported {
            let context = format!("{} thread {}", session.context(), thread.name);
            scenario
                .operation(
                    "select",
                    scenario
                        .handle()
                        .select_context(ThreadId::new(thread.native)),
                )
                .await;
            let trace = scenario
                .operation("backtrace", scenario.handle().backtrace())
                .await;
            let (_, interpreted) = check_thread(&context, thread.printer, &trace, &modules);
            assert!(interpreted > 0, "{context}: no interpreted frame");
        }
        session.shutdown().await;
    }
}

/// On every build, the stack of `tiers.js` unwinds through the code each
/// compiler made, which no module's file holds, to `_start`.
#[tokio::test]
async fn a_stack_of_every_tier_unwinds_through_compiled_code() {
    for entry in MATRIX {
        let mut session =
            NodeSession::launch(entry, "tiers.js", &["--allow-natives-syntax"], &[]).await;
        let reported = threads(&session.stop_at("tiers").await);
        let scenario = &session.scenario;
        let modules = scenario
            .operation("modules", scenario.handle().loaded_modules())
            .await;
        let context = session.context();
        scenario
            .operation(
                "select",
                scenario
                    .handle()
                    .select_context(ThreadId::new(reported[0].native)),
            )
            .await;
        let trace = scenario
            .operation("backtrace", scenario.handle().backtrace())
            .await;
        // Sparkplug's and Turbofan's frames run compiled code on every
        // build; Maglev's and OSR's too, where the build runs them.
        let (compiled, _) = check_thread(&context, true, &trace, &modules);
        assert!(compiled >= 2, "{context}: {compiled} compiled frames");
        session.shutdown().await;
    }
}

/// A core's main thread unwinds as the live process's does: the dump saved
/// the builtins' copies and the compiled code.
#[tokio::test]
async fn a_cores_main_thread_unwinds_through_v8_to_start() {
    for entry in CORES {
        for fixture in ["stacks", "tiers"] {
            let name = format!("node-{fixture}-{entry}.core");
            let mut scenario =
                Scenario::open_core(&name, &CoreDumpOptions::new(Scenario::fixture(&name)));
            let snapshot = scenario.snapshot().await;
            let microscope::InferiorState::Stopped { process_id, .. } = snapshot.inferior else {
                panic!("{name}: the core is not stopped");
            };
            scenario
                .operation(
                    "select",
                    scenario
                        .handle()
                        .select_context(ThreadId::new(process_id.get())),
                )
                .await;
            let modules = scenario
                .operation("modules", scenario.handle().loaded_modules())
                .await;
            let trace = scenario
                .operation("backtrace", scenario.handle().backtrace())
                .await;
            let (compiled, interpreted) = check_thread(&name, true, &trace, &modules);
            assert!(interpreted > 0, "{name}: no interpreted frame");
            if fixture == "tiers" {
                assert!(compiled >= 2, "{name}: {compiled} compiled frames");
            }
            scenario.shutdown().await;
        }
    }
}
