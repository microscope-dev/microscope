//! asyncio's tasks, which CPython lists from 3.14: each is a task of the
//! Python runtime, named, in the state asyncio gives it, with what it
//! awaits and who awaits it; one no thread runs has its coroutines' frames
//! as its stack, whose locals and expressions read as a thread's frames'
//! do, and the one a thread runs is on that thread.

use std::collections::BTreeMap;

use microscope::{
    ExecutionContext, FrameKind, StackFrameId, StopContext, TaskSnapshot, TaskState,
    ThreadActivity, ThreadId, UnwindTermination,
};

use crate::expressions::{Outcome, outcome};
use crate::locals::{Stop, presented};
use crate::session::PythonSession;

/// The interpreters whose asyncio lists its tasks.
const ASYNCIO: [&str; 4] = ["3.14", "3.14t", "3.15", "3.15t"];

/// One task as CPython reported it: its state, detail, and awaiters, and
/// each frame of its stack with its locals.
#[derive(Debug, Default)]
struct Reported<'t> {
    state: &'t str,
    detail: &'t str,
    awaited_by: &'t str,
    frames: Vec<(&'t str, &'t str, &'t str)>,
    locals: BTreeMap<usize, Vec<(&'t str, &'t str)>>,
}

/// Each task CPython reported, by its name.
fn reported(truth: &[Vec<String>]) -> BTreeMap<&str, Reported<'_>> {
    let mut tasks: BTreeMap<&str, Reported<'_>> = BTreeMap::new();
    for fields in truth {
        match fields[0].as_str() {
            "task" => {
                let task = tasks.entry(fields[1].as_str()).or_default();
                task.state = &fields[2];
                task.detail = &fields[3];
                task.awaited_by = &fields[4];
            }
            "task-frame" => {
                let task = tasks.entry(fields[1].as_str()).or_default();
                assert_eq!(fields[2], task.frames.len().to_string(), "frames in order");
                task.frames.push((&fields[3], &fields[4], &fields[5]));
            }
            "task-local" => {
                let depth = fields[2].parse().expect("a depth");
                tasks
                    .entry(fields[1].as_str())
                    .or_default()
                    .locals
                    .entry(depth)
                    .or_default()
                    .push((&fields[3], &fields[4]));
            }
            _ => {}
        }
    }

    tasks
}

/// The value of a task's label `key`, if it has one.
fn label<'t>(task: &'t TaskSnapshot, key: &str) -> Option<&'t str> {
    task.labels
        .iter()
        .find(|(name, _)| &**name == key)
        .map(|(_, value)| &**value)
}

const fn state_name(state: &TaskState) -> &'static str {
    match state {
        TaskState::Running => "running",
        TaskState::Runnable => "runnable",
        TaskState::Blocked => "blocked",
        TaskState::Exited => "exited",
        TaskState::Unknown(_) => "unknown",
    }
}

/// Every task is listed once, as CPython reported it, and each one no
/// thread runs has the frames, locals, and expressions CPython gives its
/// coroutines; the one the printer runs is on its thread.
#[tokio::test]
async fn each_task_is_listed_with_its_coroutines_as_its_stack() {
    for interpreter in ASYNCIO {
        let mut session = PythonSession::launch(interpreter, "tasks.py", &[]).await;
        let truth = session.stop_at("parked").await;
        let printer: u64 = truth
            .iter()
            .find(|fields| fields[0] == "thread" && fields[3] == "printer")
            .and_then(|fields| fields[1].parse().ok())
            .expect("the printer's thread");

        let snapshot = session.scenario.snapshot().await;
        let stop = Stop {
            id: session.stop().await,
            scenario: &session.scenario,
            context: session.context(),
        };
        let mut lies = Vec::new();

        let page = stop
            .scenario
            .operation("tasks", stop.scenario.handle().tasks(None, 256))
            .await;
        if !page.gaps.is_empty() {
            lies.push(format!("the tasks have gaps: {:?}", page.gaps));
        }

        let reported = reported(&truth);
        let mut listed = BTreeMap::new();
        for task in page.tasks.iter() {
            let Some(name) = label(task, "name") else {
                lies.push(format!("a task has no name: {task:?}"));
                continue;
            };

            if listed.insert(name, task).is_some() {
                lies.push(format!("{name} is listed twice"));
            }
        }

        let names: Vec<&str> = listed.keys().copied().collect();
        let expected: Vec<&str> = reported.keys().copied().collect();
        if names != expected {
            lies.push(format!("tasks {names:?} where CPython listed {expected:?}"));
        }

        for (name, truth) in &reported {
            let Some(task) = listed.get(name) else {
                continue;
            };

            let detail = task.detail.as_deref().unwrap_or("-");
            let awaited_by = label(task, "awaited_by").unwrap_or("-");
            let shown = (state_name(&task.state), detail, awaited_by);
            if shown != (truth.state, truth.detail, truth.awaited_by) {
                lies.push(format!(
                    "{name} is {shown:?} where CPython reported {:?}",
                    (truth.state, truth.detail, truth.awaited_by)
                ));
            }

            if truth.state == "running" {
                if task.thread != Some(ThreadId::new(printer)) {
                    lies.push(format!("{name} runs on {:?}, not the printer", task.thread));
                }

                let activity = snapshot
                    .threads
                    .iter()
                    .find(|thread| thread.id == ThreadId::new(printer))
                    .and_then(|thread| thread.activity.clone());
                if !matches!(&activity, Some(ThreadActivity::Task { task: id, .. }) if *id == task.id)
                {
                    lies.push(format!(
                        "the printer's activity is {activity:?}, not {name}"
                    ));
                }

                continue;
            }

            task_stack(&stop, task, name, truth, &mut lies).await;
        }

        // A task's frame evaluates its variables as a thread's would.
        if let Some(chained) = listed.get("chained") {
            let mut at = StopContext {
                stop: stop.id,
                execution: ExecutionContext::Task(chained.id),
                frame: StackFrameId::INNERMOST,
            };

            let trace = stop
                .scenario
                .operation("task backtrace", stop.scenario.handle().at(at).backtrace())
                .await;

            match trace.frames.get(1) {
                Some(leaf) => at.frame = leaf.id,
                None => lies.push(format!("chained has no frame of leaf: {trace:#?}")),
            }

            match outcome(&stop, at, "deadline + depth").await {
                Outcome::Error(kind) => {
                    if kind != microscope::ExpressionErrorKind::UnknownName {
                        lies.push(format!("leaf's `deadline + depth` fails as {kind:?}"));
                    }
                }
                other => lies.push(format!("leaf sees middle's `depth`: {other:?}")),
            }

            match outcome(&stop, at, "deadline + 1").await {
                Outcome::Value(value) if value == "3601" => {}
                other => lies.push(format!("leaf's `deadline + 1` is {other:?}")),
            }
        }

        assert!(lies.is_empty(), "{}:\n{}", stop.context, lies.join("\n"));
        session.shutdown().await;
    }
}

/// The task's backtrace is its coroutines' frames, and each frame's locals
/// are those CPython reported.
async fn task_stack(
    stop: &Stop<'_>,
    task: &TaskSnapshot,
    name: &str,
    truth: &Reported<'_>,
    lies: &mut Vec<String>,
) {
    let at = |frame| StopContext {
        stop: stop.id,
        execution: ExecutionContext::Task(task.id),
        frame,
    };

    let trace = stop
        .scenario
        .operation(
            "task backtrace",
            stop.scenario
                .handle()
                .at(at(StackFrameId::INNERMOST))
                .backtrace(),
        )
        .await;

    let frames: Vec<(String, String, String)> = trace
        .frames
        .iter()
        .map(|frame| {
            if !matches!(frame.kind, FrameKind::Interpreted { .. }) {
                return (format!("{:?}", frame.kind), String::new(), String::new());
            }

            frame.recorded.as_ref().map_or_else(
                || ("<none>".to_owned(), String::new(), String::new()),
                |recorded| {
                    let (path, line) = recorded.place.as_ref().map_or_else(
                        || (String::new(), "-".to_owned()),
                        |place| (place.path.to_string(), place.line.to_string()),
                    );
                    (recorded.function.to_string(), path, line)
                },
            )
        })
        .collect();

    let expected: Vec<(String, String, String)> = truth
        .frames
        .iter()
        .map(|(function, path, line)| {
            (
                (*function).to_owned(),
                (*path).to_owned(),
                (*line).to_owned(),
            )
        })
        .collect();

    if frames != expected {
        lies.push(format!(
            "{name}'s frames are {frames:#?} where CPython gave {expected:#?}"
        ));
    }

    if trace.termination != UnwindTermination::Complete {
        lies.push(format!("{name}'s frames end {:?}", trace.termination));
    }

    for (depth, locals) in &truth.locals {
        let Some(frame) = trace.frames.get(*depth) else {
            continue;
        };

        let variables = stop
            .scenario
            .operation(
                "task frame variables",
                stop.scenario.handle().at(at(frame.id)).variables(),
            )
            .await;

        let shown: Vec<(String, String)> = variables
            .variables
            .iter()
            .map(|variable| {
                (
                    variable.name.to_string(),
                    presented(&variable.state)
                        .map_or_else(|| format!("{:?}", variable.state), str::to_owned),
                )
            })
            .collect();

        let expected: Vec<(String, String)> = locals
            .iter()
            .map(|(local, value)| ((*local).to_owned(), (*value).to_owned()))
            .collect();

        if shown != expected {
            lies.push(format!(
                "{name}'s frame {depth} has {shown:?} where CPython gave {expected:?}"
            ));
        }
    }
}
