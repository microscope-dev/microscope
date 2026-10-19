//! What holds at every stop of a tokio program, whatever a test is about.
//! A scenario from [`checked`] runs [`check_tokio_stop`] after each stop,
//! so a bug surfaces in whichever test reaches it.
//!
//! - Every thread is accounted for: what it does for the runtime is known.
//! - The tasks are listed whole, each once. Only a list the program was
//!   changing at the stop may be stale, and the page says so, as it says
//!   when tokio's version is not the one verified.
//! - A thread runs a task exactly when the list puts the task on it, but
//!   for a worker's own launch, which the list puts on its worker.
//! - A thread running a task has tokio's dispatch among its frames.
//! - A suspended task's backtrace is its chain of awaits: it ends where the
//!   chain does, or says why it cannot go on; each async frame is a future
//!   of its own, and names its function.

use std::collections::BTreeSet;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use microscope::{
    Backtrace, CodeRole, DebuggerHandle, ExecutionContext, FrameKind, InferiorState, StackFrameId,
    StackSegment, StopContext, TaskSnapshot, TaskState, ThreadActivity, ThreadId,
    UnwindTermination,
};

use crate::support::Scenario;

/// The most tasks a stop's check reads.
const MAX_TASKS: usize = 4096;

/// A scenario of `fixture` that checks every stop.
pub fn checked(fixture: &str) -> Scenario {
    Scenario::launch(fixture).checking_stops(check_tokio_stop)
}

/// Checks the invariants of the stop the debugger is at.
pub fn check_tokio_stop(
    handle: DebuggerHandle,
) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send>> {
    Box::pin(async move { check(&handle).await })
}

/// A stop as the checks see it: each thread with what it does and its
/// backtrace, and the tasks with the gaps of their pages.
#[derive(Clone)]
struct Stop {
    threads: Vec<(ThreadId, ThreadActivity, Backtrace)>,
    tasks: Vec<TaskSnapshot>,
    gaps: Vec<String>,

    /// Each suspended task's backtrace.
    suspended: Vec<(u64, Backtrace)>,
}

async fn check(handle: &DebuggerHandle) -> Result<(), String> {
    read(handle).await?.map_or(Ok(()), |stop| check_stop(&stop))
}

/// The stop the debugger is at, or `None` when it is at none.
async fn read(handle: &DebuggerHandle) -> Result<Option<Stop>, String> {
    let failed = |what: &str, error: microscope::Error| format!("{what}: {error}");

    let snapshot = handle
        .snapshot()
        .await
        .map_err(|error| failed("snapshot", error))?;

    let InferiorState::Stopped { stop_id, .. } = snapshot.inferior else {
        return Ok(None);
    };

    let mut threads = Vec::new();
    for thread in snapshot.threads.iter() {
        let activity = thread
            .activity
            .clone()
            .ok_or_else(|| format!("thread {} has no activity", thread.id))?;

        let trace = handle
            .at(StopContext {
                stop: stop_id,
                execution: ExecutionContext::Thread(thread.id),
                frame: StackFrameId::INNERMOST,
            })
            .backtrace()
            .await
            .map_err(|error| failed(&format!("thread {}'s backtrace", thread.id), error))?;
        threads.push((thread.id, activity, trace));
    }

    let mut tasks = Vec::new();
    let mut gaps = Vec::new();
    let mut from = None;
    loop {
        let page = handle
            .tasks(from, 256)
            .await
            .map_err(|error| failed("tasks", error))?;
        tasks.extend(page.tasks.iter().cloned());
        gaps.extend(page.gaps.iter().map(ToString::to_string));
        match page.next {
            Some(_) if tasks.len() >= MAX_TASKS => {
                return Err(format!(
                    "the program has more than {MAX_TASKS} tasks to check"
                ));
            }
            Some(next) => from = Some(next),
            None => break,
        }
    }

    let mut suspended = Vec::new();
    for task in tasks.iter().filter(|task| {
        matches!(task.state, TaskState::Blocked | TaskState::Runnable) && task.thread.is_none()
    }) {
        // A blocking closure no thread runs yet has no future of its own.
        if task.detail.as_deref() == Some("queued in the blocking pool") {
            continue;
        }

        let trace = handle
            .at(StopContext {
                stop: stop_id,
                execution: ExecutionContext::Task(task.id),
                frame: StackFrameId::INNERMOST,
            })
            .backtrace()
            .await
            .map_err(|error| failed(&format!("task {}'s backtrace", task.id.number), error))?;
        suspended.push((task.id.number, trace));
    }

    Ok(Some(Stop {
        threads,
        tasks,
        gaps,
        suspended,
    }))
}

fn check_stop(stop: &Stop) -> Result<(), String> {
    // A list may be changing, tokio's version may be one the debugger was
    // not checked against, and an optimized build may lose the future
    // that runs a local set; nothing else is missing.
    if let Some(gap) = stop.gaps.iter().find(|gap| {
        !gap.contains("was being changed at the stop")
            && !gap.contains("its runtime is read as tokio")
            && !gap.contains("drives a future that may run a local set")
    }) {
        return Err(format!("the tasks are incomplete: {gap}"));
    }

    let mut ids = BTreeSet::new();
    if let Some(task) = stop.tasks.iter().find(|task| !ids.insert(task.id)) {
        return Err(format!("task {} is listed twice", task.id.number));
    }

    for (thread, activity, trace) in &stop.threads {
        match activity {
            ThreadActivity::Unknown(reason) => {
                return Err(format!("thread {thread} runs something unknown: {reason}"));
            }
            ThreadActivity::Task { task, .. } => {
                let listed = stop.tasks.iter().find(|listed| listed.id == *task);
                if listed.and_then(|listed| listed.thread) != Some(*thread) {
                    return Err(format!(
                        "thread {thread} runs task {}, which the list does not put on it: \
                         {listed:#?}; gaps {:?}; {trace:#?}",
                        task.number, stop.gaps
                    ));
                }

                // Its frames reach tokio's dispatch, unless the thread is in
                // glibc's clone3, starting another, which ends its unwind
                // information before the syscall.
                let in_clone3 = match trace.termination {
                    UnwindTermination::NoUnwindInfo { address } => {
                        trace.frames.last().is_some_and(|frame| {
                            frame.instruction == Some(address)
                                && frame.symbol.as_ref().is_some_and(|symbol| {
                                    ["clone3", "__clone3", "__GI___clone3"].contains(&&*symbol.name)
                                })
                        })
                    }
                    _ => false,
                };

                if !in_clone3
                    && !trace
                        .frames
                        .iter()
                        .any(|frame| frame.role == CodeRole::Dispatch)
                {
                    return Err(format!(
                        "thread {thread} runs task {} without tokio's dispatch: {trace:#?}",
                        task.number
                    ));
                }
            }
            ThreadActivity::Idle | ThreadActivity::Outside => {}
        }

        check_driven(trace).map_err(|problem| format!("thread {thread}: {problem}: {trace:#?}"))?;
    }

    for task in stop.tasks.iter().filter(|task| !task.internal) {
        let Some(thread) = task.thread else {
            continue;
        };

        let runs = stop.threads.iter().any(|(id, activity, _)| {
            *id == thread
                && matches!(activity, ThreadActivity::Task { task: on, .. } if *on == task.id)
        });

        if !runs {
            return Err(format!(
                "task {} is listed on thread {thread}, which does not run it",
                task.id.number
            ));
        }
    }

    for (task, trace) in &stop.suspended {
        check_awaits(trace).map_err(|problem| format!("task {task}: {problem}: {trace:#?}"))?;
    }

    Ok(())
}

/// A suspended task's backtrace ends where its chain of awaits does, or
/// says why it cannot go on, and each async frame is a future of its own
/// that names its function. A future may begin where the one it awaits
/// does, which its function tells apart.
fn check_awaits(trace: &Backtrace) -> Result<(), String> {
    if !matches!(
        trace.termination,
        UnwindTermination::Complete | UnwindTermination::BrokenAwaitChain { .. }
    ) {
        return Err(format!("its awaits end at {}", trace.termination));
    }

    let mut objects = BTreeSet::new();
    for frame in trace.frames.iter() {
        let FrameKind::Async { object } = frame.kind else {
            continue;
        };

        let Some(function) = &frame.function else {
            return Err(format!("frame {} names no function", frame.level));
        };

        if !objects.insert((object, function.id)) {
            return Err(format!(
                "frame {} repeats the future at {object}",
                frame.level
            ));
        }
    }

    Ok(())
}

/// A thread's frames of a future it drives are a chain of awaits that
/// names its functions, just before a frame of tokio's that drives it, and
/// each future the trace does not show is noted at such a frame.
fn check_driven(trace: &Backtrace) -> Result<(), String> {
    let driver = |frame: Option<&microscope::StackFrame>| {
        frame.is_some_and(|frame| {
            frame.role == CodeRole::RuntimeInternal && frame.segment != StackSegment::Future
        })
    };

    let mut objects = BTreeSet::new();
    for (index, frame) in trace.frames.iter().enumerate() {
        if frame.segment != StackSegment::Future {
            continue;
        }

        match frame.kind {
            FrameKind::Async { object } => {
                let Some(function) = &frame.function else {
                    return Err(format!("frame {index} names no function"));
                };

                if !objects.insert((object, function.id)) {
                    return Err(format!("frame {index} repeats the future at {object}"));
                }
            }
            FrameKind::Awaited { .. } => {}
            kind => return Err(format!("frame {index} of a future is {kind:?}")),
        }

        let next = trace.frames.get(index + 1);
        if next.is_none_or(|next| next.segment != StackSegment::Future) && !driver(next) {
            return Err(format!(
                "no frame of tokio's drives the future of frame {index}"
            ));
        }
    }

    for future in trace.unfollowed.iter() {
        let at = usize::try_from(future.driver.get()).expect("a level fits usize");
        if !driver(trace.frames.get(at)) {
            return Err(format!(
                "no frame of tokio's at {at} drives a future: {future:?}"
            ));
        }
    }

    Ok(())
}

/// The checks fail on the faults they look for: a task listed twice or
/// on a thread that does not run it, a thread running a task the list
/// leaves off it or without tokio's dispatch, an unknown thread, a gap
/// other than a list changing, and an await chain that repeats a future
/// or names no function.
#[tokio::test]
async fn the_checks_fail_on_the_faults_they_look_for() {
    let mut scenario = checked("tokio-workers-o0");
    scenario.add_breakpoint("task_reached").await;
    scenario.run_to_stop().await;

    // The first task to arrive may stop every thread before any other task
    // has suspended, so stop at each task's arrival until one has: the
    // eight tasks all arrive before the program goes on.
    let suspended_in_async = |stop: &Stop| {
        stop.suspended.iter().any(|(_, trace)| {
            trace
                .frames
                .iter()
                .any(|frame| matches!(frame.kind, FrameKind::Async { .. }))
        })
    };

    let mut stop = read(scenario.handle())
        .await
        .expect("the stop")
        .expect("stopped");
    for _ in 0..8 {
        if suspended_in_async(&stop) {
            break;
        }

        scenario.resume_to_stop().await;
        stop = read(scenario.handle())
            .await
            .expect("the stop")
            .expect("stopped");
    }

    check_stop(&stop).expect("the stop holds");

    let (index, task) = stop
        .threads
        .iter()
        .enumerate()
        .find_map(|(index, (_, activity, _))| match activity {
            ThreadActivity::Task { task, .. } => Some((index, *task)),
            _ => None,
        })
        .expect("a thread runs a task");

    let position = stop
        .tasks
        .iter()
        .position(|listed| listed.id == task)
        .expect("the task is listed");

    let mut twice = stop.clone();
    twice.tasks.push(stop.tasks[position].clone());
    let mut moved = stop.clone();
    moved.tasks[position].thread = None;

    let mut undispatched = stop.clone();
    let frames = undispatched.threads[index]
        .2
        .frames
        .iter()
        .filter(|frame| frame.role != CodeRole::Dispatch)
        .cloned()
        .collect::<Vec<_>>();
    undispatched.threads[index].2.frames = frames.into();

    let mut idle = stop.clone();
    idle.threads[index].1 = ThreadActivity::Idle;
    let mut unknown = stop.clone();
    unknown.threads[index].1 = ThreadActivity::Unknown("unreadable".into());
    let mut gap = stop.clone();
    gap.gaps.push("shard 0 is broken".into());

    let (_, trace) = stop
        .suspended
        .iter()
        .find(|(_, trace)| {
            trace
                .frames
                .iter()
                .any(|frame| matches!(frame.kind, FrameKind::Async { .. }))
        })
        .expect("a task is suspended in an async function");

    let at = trace
        .frames
        .iter()
        .position(|frame| matches!(frame.kind, FrameKind::Async { .. }))
        .expect("an async frame");

    let sabotage = |change: &dyn Fn(&mut Vec<microscope::StackFrame>)| {
        let mut frames = trace.frames.to_vec();
        change(&mut frames);
        let mut sabotaged = stop.clone();
        let trace = Backtrace {
            frames: frames.into(),
            ..trace.clone()
        };

        sabotaged.suspended.push((0, trace));
        sabotaged
    };

    let repeated = sabotage(&|frames| frames.push(frames[at].clone()));
    let unnamed = sabotage(&|frames| frames[at].function = None);

    for (fault, sabotaged) in [
        ("twice", twice),
        ("moved", moved),
        ("undispatched", undispatched),
        ("idle", idle),
        ("unknown", unknown),
        ("gap", gap),
        ("repeated", repeated),
        ("unnamed", unnamed),
    ] {
        assert!(check_stop(&sabotaged).is_err(), "{fault}");
    }

    scenario.shutdown().await;
}

/// The checks fail on a thread's future with no frame of tokio's to
/// drive it, and on a future noted at a frame that drives none.
#[tokio::test]
async fn the_checks_fail_on_a_future_nothing_drives() {
    let mut scenario = checked("tokio-drivers-o0");
    scenario.add_breakpoint("truth_reached").await;
    scenario.run_to_stop().await;

    let stop = read(scenario.handle())
        .await
        .expect("the stop")
        .expect("stopped");

    check_stop(&stop).expect("the stop holds");

    // The thread that blocks on the runtime drives the future it was given.
    let (driving, last) = stop
        .threads
        .iter()
        .enumerate()
        .find_map(|(index, (_, _, trace))| {
            let last = trace
                .frames
                .iter()
                .rposition(|frame| frame.segment == StackSegment::Future)?;
            Some((index, last))
        })
        .expect("a thread drives a future");

    let thread_sabotage = |change: &dyn Fn(&mut Backtrace)| {
        let mut sabotaged = stop.clone();
        change(&mut sabotaged.threads[driving].2);
        sabotaged
    };

    let undriven = thread_sabotage(&|trace| {
        let mut frames = trace.frames.to_vec();
        frames[last + 1].role = CodeRole::Ordinary;
        trace.frames = frames.into();
    });

    let misplaced = thread_sabotage(&|trace| {
        trace.unfollowed = Arc::from([microscope::Unfollowed {
            driver: StackFrameId::INNERMOST,
            driven: microscope::DrivenFrames::Future,
            reason: "unread".into(),
        }]);
    });

    for (fault, sabotaged) in [("undriven", undriven), ("misplaced", misplaced)] {
        assert!(check_stop(&sabotaged).is_err(), "{fault}");
    }

    scenario.shutdown().await;
}
