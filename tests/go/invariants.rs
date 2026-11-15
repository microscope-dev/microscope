use std::future::Future;
use std::pin::Pin;

use microscope::{
    Backtrace, CodeRole, DebuggerHandle, ExecutionContext, FrameKind, InferiorState, ModuleId,
    StackFrameId, StackSegment, StopContext, StopId, ThreadActivity, UnwindTermination,
};

use crate::support::Scenario;

const MAX_TASKS: usize = 4096;

pub fn checked(fixture: &str) -> Scenario {
    Scenario::launch(fixture).checking_stops(check_go_stop)
}

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

pub fn check_backtrace(trace: &Backtrace, main: ModuleId) -> Result<(), String> {
    let role = |index: usize| Some(trace.frames[index].role);

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
