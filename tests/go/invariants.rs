//! What holds at every stop of a Go program, whatever a test is about.
//! A scenario from [`checked`] runs [`check_go_stop`] after each stop, so
//! a bug surfaces in whichever test reaches it.
//!
//! - Every thread is accounted for: it runs a goroutine, the runtime's
//!   code for one, or nothing; its state is never unreadable.
//! - Every backtrace, of every thread and every parked goroutine, ends
//!   properly: complete at an outermost frame, or with a typed reason. It
//!   names every frame of the image carrying the runtime, the program's or
//!   a library's, and changes stacks only where the runtime switches them.

use std::future::Future;
use std::pin::Pin;

use microscope::{
    Backtrace, CodeRole, DebuggerHandle, ExecutionContext, FrameKind, InferiorState, ModuleId,
    StackFrameId, StackSegment, StopContext, StopId, ThreadActivity, UnwindTermination,
};

use crate::support::Scenario;

/// The most tasks a stop's check reads.
const MAX_TASKS: usize = 4096;

/// A scenario of `fixture` that checks every stop.
pub fn checked(fixture: &str) -> Scenario {
    Scenario::launch(fixture).checking_stops(check_go_stop)
}

/// Checks the invariants of the stop the debugger is at.
pub fn check_go_stop(
    handle: DebuggerHandle,
) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send>> {
    Box::pin(async move { check(&handle).await })
}

async fn check(handle: &DebuggerHandle) -> Result<(), String> {
    let failed = |what: &str, error: microscope::Error| format!("{what}: {error}");
    let snapshot = handle
        .snapshot()
        .await
        .map_err(|error| failed("snapshot", error))?;
    let InferiorState::Stopped { stop_id, .. } = snapshot.inferior else {
        return Ok(());
    };

    // Before a library carrying the runtime loads, no image is checked.
    let Some(main) = go_module(handle).await? else {
        return Ok(());
    };

    for thread in snapshot.threads.iter() {
        if let Some(ThreadActivity::Unknown(reason)) = &thread.activity {
            return Err(format!(
                "thread {} runs something unknown: {reason}",
                thread.id
            ));
        }

        let context = ExecutionContext::Thread(thread.id);
        let trace = backtrace(handle, stop_id, context).await?;
        check_backtrace(&trace, main).map_err(|problem| format!("{context:?}: {problem}"))?;
    }

    // A goroutine on a thread was checked as its thread's.
    let mut from = None;
    let mut read = 0;

    loop {
        let page = handle
            .tasks(from, 256)
            .await
            .map_err(|error| failed("tasks", error))?;
        if let Some(gap) = page.gaps.first() {
            return Err(format!("the tasks are incomplete: {gap}"));
        }

        for task in page.tasks.iter().filter(|task| task.thread.is_none()) {
            let context = ExecutionContext::Task(task.id);
            let trace = backtrace(handle, stop_id, context).await?;
            check_backtrace(&trace, main).map_err(|problem| format!("{context:?}: {problem}"))?;
        }

        read += page.tasks.len();
        match page.next {
            Some(next) if read < MAX_TASKS => from = Some(next),
            _ => return Ok(()),
        }
    }
}

/// The loaded module whose image carries Go's runtime.
async fn go_module(handle: &DebuggerHandle) -> Result<Option<ModuleId>, String> {
    let modules = handle
        .loaded_modules()
        .await
        .map_err(|error| format!("modules: {error}"))?;

    for module in modules.modules.iter() {
        let image = handle
            .loaded_module_image(module.module.id)
            .await
            .map_err(|error| format!("module {:?}: {error}", module.module.id))?;
        if image.functions_named("runtime.goexit").next().is_some() {
            return Ok(Some(module.module.id));
        }
    }

    Ok(None)
}

async fn backtrace(
    handle: &DebuggerHandle,
    stop: StopId,
    execution: ExecutionContext,
) -> Result<Backtrace, String> {
    handle
        .at(StopContext {
            stop,
            execution,
            frame: StackFrameId::INNERMOST,
        })
        .backtrace()
        .await
        .map_err(|error| format!("{execution:?}'s backtrace: {error}"))
}

/// Whether a backtrace ends properly, names every frame of the image
/// carrying the runtime, loaded as `main`, and changes stacks only where
/// the runtime switches them.
pub fn check_backtrace(trace: &Backtrace, main: ModuleId) -> Result<(), String> {
    let role = |index: usize| Some(trace.frames[index].role);

    // A runtime's stack may also end where it switched from a task that
    // has since gone elsewhere, as an idle thread's does at `mcall`, and a
    // thread the kernel just started, before its runtime gives it a stack,
    // has only the frame it began in. Code outside the runtime's image,
    // such as the C library's that starts a thread, ends a stack where its
    // call-frame information says it does.
    if trace.termination == UnwindTermination::Complete {
        let last = trace.frames.len().checked_sub(1).ok_or("no frames")?;
        let switched_from = role(last) == Some(CodeRole::StackSwitch)
            && matches!(
                trace.frames[last].segment,
                StackSegment::System | StackSegment::Signal
            );
        let starting = last == 0 && trace.frames[0].segment == StackSegment::Thread;
        let foreign = trace.frames[last]
            .module
            .is_some_and(|module| module != main);

        if role(last) != Some(CodeRole::Outermost) && !switched_from && !starting && !foreign {
            return Err(format!(
                "it ends complete at a frame that begins no stack: {trace:#?}"
            ));
        }
    }

    for (index, frame) in trace.frames.iter().enumerate() {
        if frame.module == Some(main) && frame.function.is_none() {
            return Err(format!(
                "frame {index} in the program is unnamed: {trace:#?}"
            ));
        }
    }

    for (index, pair) in trace.frames.windows(2).enumerate() {
        let [inner, outer] = pair else {
            unreachable!("windows of two");
        };

        let switched = role(index) == Some(CodeRole::StackSwitch)
            || inner.kind == FrameKind::Signal
            || outer.kind == FrameKind::Signal;
        if inner.segment != outer.segment && !switched {
            return Err(format!(
                "it changes stacks after frame {index}, where nothing switches them: {trace:#?}"
            ));
        }
    }

    Ok(())
}

/// The checks fail on backtraces with the faults they look for: a frame
/// dropped where the stack switched, or at its end, and an unnamed frame.
#[tokio::test]
async fn the_checks_fail_on_the_faults_they_look_for() {
    let mut scenario = checked("stacks-go-o0");
    scenario.add_breakpoint("runtime.readmemstats_m").await;
    scenario.run_to_stop().await;
    let trace = scenario
        .operation("backtrace", scenario.handle().backtrace())
        .await;
    let main = scenario
        .operation("modules", scenario.handle().loaded_modules())
        .await
        .modules[0]
        .module
        .id;
    check_backtrace(&trace, main).expect("the stop's own backtrace holds");

    let without = |index: usize| {
        let mut frames = trace.frames.to_vec();
        frames.remove(index);
        Backtrace {
            frames: frames.into(),
            ..trace.clone()
        }
    };

    let switch = trace
        .frames
        .iter()
        .position(|frame| {
            frame
                .function
                .as_ref()
                .is_some_and(|function| function.role == CodeRole::StackSwitch)
        })
        .expect("the trace switches stacks");
    assert!(check_backtrace(&without(switch), main).is_err());
    assert!(check_backtrace(&without(trace.frames.len() - 1), main).is_err());

    let mut unnamed = trace.frames.to_vec();
    let program = unnamed
        .iter()
        .position(|frame| frame.module == Some(main))
        .expect("a frame in the program");
    unnamed[program].function = None;
    let unnamed = Backtrace {
        frames: unnamed.into(),
        ..trace.clone()
    };

    assert!(check_backtrace(&unnamed, main).is_err());
    scenario.shutdown().await;
}
