//! A worker pool, and goroutines parked in every way a program parks one.

use std::collections::BTreeSet;

use microscope::{
    Backtrace, BreakpointOptions, BreakpointSpec, Condition, Evaluation, ExecutionContext,
    Expression, InferiorState, ScalarValue, StackFrameId, StackSegment, StopContext, StopReason,
    TaskSnapshot, TaskState, ThreadActivity, ThreadId, VariableState, VariableValue,
};

use crate::truth::GoSession;

const BUILDS: [&str; 2] = ["workers-go-o0", "workers-go-o2"];

#[tokio::test]
async fn goroutines_are_listed_as_the_runtime_lists_them() {
    for fixture in BUILDS {
        let mut session = GoSession::launch(fixture).await;
        let truth = session.checkpoint("parked");

        // Small pages cross from one page to the next mid-list.
        let (tasks, gaps) = session.tasks(3).await;
        assert!(gaps.is_empty(), "{fixture}: {gaps:?}");

        truth
            .check_tasks(&tasks)
            .unwrap_or_else(|problem| panic!("{fixture}: {problem}: {tasks:#?}"));
        assert!(
            tasks.iter().any(|task| task.internal),
            "{fixture}: the runtime's own goroutines are listed too"
        );

        // The check fails on a goroutine left out, or in another state.
        let mut sabotaged = tasks.clone();
        let dropped = sabotaged
            .iter()
            .position(|task| !task.internal && task.id.number != truth.main.0)
            .expect("a goroutine besides main");
        sabotaged.remove(dropped);
        assert!(truth.check_tasks(&sabotaged).is_err(), "{fixture}");

        let mut sabotaged = tasks.clone();
        sabotaged[dropped].detail = Some("running".into());
        assert!(truth.check_tasks(&sabotaged).is_err(), "{fixture}");

        for task in &tasks {
            let context = format!("{fixture}: {task:#?}");
            let entry = task
                .entry
                .as_ref()
                .and_then(|entry| entry.function.as_deref())
                .expect(&context);
            assert_eq!(
                task.internal,
                entry.starts_with("runtime.") && entry != "runtime.main",
                "{context}"
            );
            let Some(dumped) = truth.tasks.get(&task.id.number) else {
                continue;
            };

            // Only a goroutine on a thread has one, and a parked one says
            // where it resumes.
            assert_eq!(
                task.thread.is_some(),
                task.state == TaskState::Running,
                "{context}"
            );
            assert_eq!(task.resume.is_some(), task.thread.is_none(), "{context}");
            let created_by = task
                .creation
                .as_ref()
                .and_then(|creation| creation.function.as_deref());

            if task.id.number == truth.main.0 {
                assert_eq!(entry, "runtime.main", "{context}");
                assert_eq!(task.thread, Some(ThreadId::new(truth.main.1)), "{context}");
            } else {
                assert_eq!(created_by, Some("main.main"), "{context}");

                // `go worker(...)` begins in a wrapper the compiler writes
                // to pass the arguments; a closure begins in itself.
                let outermost = dumped.frames.last().map(|frame| frame.0.as_str());
                let started = if outermost == Some("main.worker") {
                    "main.main.gowrap1"
                } else {
                    outermost.expect(&context)
                };

                assert_eq!(entry, started, "{context}");
            }
        }

        check_threads(&mut session, &tasks, truth.main.0).await;
        session.scenario.shutdown().await;
    }
}

/// Every thread runs a goroutine or is idle, and a goroutine on a thread is
/// the one that thread runs. Another thread may be running the runtime's
/// code for its goroutine on the system stack, but the goroutine that hit
/// the breakpoint is on its own.
async fn check_threads(session: &mut GoSession, tasks: &[TaskSnapshot], main: u64) {
    let fixture = session.fixture.clone();
    {
        let snapshot = session.scenario.snapshot().await;
        let mut running = BTreeSet::new();

        for thread in snapshot.threads.iter() {
            let context = format!("{fixture}: {thread:#?}");

            match thread.activity.as_ref().expect(&context) {
                ThreadActivity::Task { task, stack } => {
                    if task.number == main {
                        assert_eq!(*stack, StackSegment::Task, "{context}");
                    }

                    let listed = tasks.iter().find(|listed| listed.id == *task);
                    assert_eq!(
                        listed.and_then(|listed| listed.thread),
                        Some(thread.id),
                        "{context}"
                    );
                    running.insert(task.number);
                }
                ThreadActivity::Idle => {}
                ThreadActivity::Unknown(reason) => panic!("{context}: {reason}"),
                ThreadActivity::Outside => panic!("{context}: outside the runtime"),
            }
        }

        let on_threads = tasks
            .iter()
            .filter(|task| task.thread.is_some())
            .map(|task| task.id.number)
            .collect::<BTreeSet<_>>();
        assert_eq!(running, on_threads, "{fixture}");
        assert!(running.contains(&main), "{fixture}");
    }
}

#[tokio::test]
async fn parked_goroutines_show_the_frames_the_runtime_dumps() {
    for fixture in BUILDS {
        let mut session = GoSession::launch(fixture).await;
        let truth = session.checkpoint("parked");
        let (tasks, _) = session.tasks(64).await;
        let InferiorState::Stopped { stop_id, .. } = session.scenario.snapshot().await.inferior
        else {
            panic!("{fixture}: not stopped");
        };

        let image = session.scenario.handle().module_image();
        let mut workers = 0;
        let mut compared = 0;

        for task in tasks.iter().filter(|task| task.thread.is_none()) {
            let Some(dumped) = truth.tasks.get(&task.id.number) else {
                continue;
            };

            let context = ExecutionContext::Task(task.id);
            let view = |frame| StopContext {
                stop: stop_id,
                execution: context,
                frame,
            };

            let trace = session
                .scenario
                .operation(
                    "task backtrace",
                    session
                        .scenario
                        .handle()
                        .at(view(StackFrameId::INNERMOST))
                        .backtrace(),
                )
                .await;
            assert_eq!(trace.context, context, "{fixture}");
            check_saved_registers(&session, view(StackFrameId::INNERMOST), &trace).await;

            let shown = trace
                .frames
                .iter()
                .filter_map(|frame| {
                    let name = frame.function.as_ref()?.name.to_string();
                    let source = frame.source.as_ref()?;
                    let file = image.source_file(source.file)?.path.display().to_string();
                    (!runtime_hides(&name)).then(|| (name, format!("{file}:{}", source.line)))
                })
                .collect::<Vec<_>>();

            dumped
                .check_frames(&shown)
                .unwrap_or_else(|problem| panic!("{fixture}: {problem}: {task:#?}\n{trace:#?}"));

            // The check fails on a frame left out.
            assert!(dumped.check_frames(&shown[1..]).is_err(), "{fixture}");
            compared += 1;

            // A parked worker's arguments are on its own stack.
            let worker = trace.frames.iter().find(|frame| {
                frame
                    .function
                    .as_ref()
                    .is_some_and(|function| function.name.as_ref() == "main.worker")
            });

            if fixture.ends_with("o0")
                && let Some(frame) = worker
            {
                check_worker_arguments(&session, view(frame.id)).await;
                workers += 1;
            }
        }

        let parked = truth
            .tasks
            .values()
            .filter(|task| !matches!(task.status.as_str(), "running" | "syscall"))
            .count();
        assert_eq!(compared, parked, "{fixture}");
        if fixture.ends_with("o0") {
            assert_eq!(workers, 4, "{fixture}");
        }

        session.scenario.shutdown().await;
    }
}

#[tokio::test]
async fn a_parked_goroutine_can_be_selected() {
    let fixture = "workers-go-o0";
    let mut session = GoSession::launch(fixture).await;
    let truth = session.checkpoint("parked");
    let (tasks, _) = session.tasks(64).await;
    let worker = tasks
        .iter()
        .find(|task| {
            task.thread.is_none()
                && truth.tasks.get(&task.id.number).is_some_and(|dumped| {
                    dumped.frames.iter().any(|frame| frame.0 == "main.worker")
                })
        })
        .expect("a parked worker")
        .id;
    let handle = session.scenario.handle().clone();
    let stopped = session.scenario.snapshot().await;
    let thread = stopped.selected.expect("a selected thread");

    session
        .scenario
        .operation("select task", handle.select_context(worker))
        .await;

    let snapshot = session.scenario.snapshot().await;
    assert_eq!(snapshot.selected, Some(ExecutionContext::Task(worker)));
    assert_eq!(snapshot.selected_frame, Some(StackFrameId::INNERMOST));
    assert_eq!(snapshot.presentation, None, "a parked task has no stop");

    // Implicit inspection follows the selected task.
    let trace = session
        .scenario
        .operation("backtrace", handle.backtrace())
        .await;
    assert_eq!(trace.context, ExecutionContext::Task(worker));
    let id = trace
        .frames
        .iter()
        .find(|frame| {
            frame
                .function
                .as_ref()
                .is_some_and(|function| function.name.as_ref() == "main.worker")
        })
        .expect("the worker's frame")
        .id;

    let selected = session
        .scenario
        .operation("select frame", handle.select_frame(id))
        .await;
    assert_eq!(selected.id, id);
    assert_eq!(session.scenario.snapshot().await.selected_frame, Some(id));

    let location = session
        .scenario
        .operation("location", handle.current_location())
        .await;
    assert_eq!(
        location
            .image
            .function
            .map(|function| function.name.to_string()),
        Some("main.worker".to_owned())
    );

    let jobs = session
        .scenario
        .operation("variable", handle.variable("jobs"))
        .await;
    assert!(
        matches!(jobs.state, VariableState::Available { .. }),
        "{jobs:#?}"
    );

    // Registers are the selected frame's, and memory is the process's
    // whichever context is selected.
    let registers = session
        .scenario
        .operation("registers", handle.registers())
        .await;
    assert_eq!(registers.context, ExecutionContext::Task(worker));
    let stack = registers
        .registers
        .iter()
        .find(|register| register.register.name.as_ref() == "rsp")
        .and_then(|register| register.bytes.as_deref())
        .map(|bytes| u64::from_le_bytes(bytes.try_into().expect("a word")))
        .expect("the frame's stack pointer");

    session
        .scenario
        .operation(
            "memory",
            handle.read_word(microscope::VirtualAddress::new(stack)),
        )
        .await;

    // Each context keeps its own frame.
    session
        .scenario
        .operation("select thread", handle.select_context(thread))
        .await;
    let snapshot = session.scenario.snapshot().await;
    assert_eq!(snapshot.selected, Some(thread));
    assert_eq!(snapshot.selected_frame, Some(StackFrameId::INNERMOST));
    assert!(snapshot.presentation.is_some());
    session
        .scenario
        .operation("reselect task", handle.select_context(worker))
        .await;
    assert_eq!(session.scenario.snapshot().await.selected_frame, Some(id));
    session.scenario.shutdown().await;
}

#[tokio::test]
async fn a_condition_on_the_task_stops_only_in_that_goroutine() {
    let fixture = "workers-go-o0";
    let mut session = GoSession::launch(fixture).await;
    let truth = session.checkpoint("parked");
    let (tasks, _) = session.tasks(64).await;
    let handle = session.scenario.handle().clone();

    // The stopped thread runs main.
    let main = truth.main.0;
    assert_eq!(
        evaluate(&session, &format!("$task == {main}")).await,
        Some(true)
    );

    // A selected task is its own, and a thread between tasks has none.
    let worker = tasks
        .iter()
        .find(|task| {
            task.thread.is_none()
                && truth.tasks.get(&task.id.number).is_some_and(|dumped| {
                    dumped.frames.iter().any(|frame| frame.0 == "main.worker")
                })
        })
        .expect("a parked worker")
        .id;
    session
        .scenario
        .operation("select task", handle.select_context(worker))
        .await;

    let condition = format!("$task == {}", worker.number);
    assert_eq!(evaluate(&session, &condition).await, Some(true));

    let idle = session
        .scenario
        .snapshot()
        .await
        .threads
        .iter()
        .find(|thread| thread.activity == Some(ThreadActivity::Idle))
        .expect("an idle thread")
        .id;

    session
        .scenario
        .operation("select idle", handle.select_context(idle))
        .await;
    assert_eq!(evaluate(&session, "$task").await, None);

    // Every worker calls Done as it finishes; only the chosen one stops.
    session
        .scenario
        .operation(
            "conditional breakpoint",
            handle.add_breakpoint_with(
                BreakpointSpec::Function("sync.(*WaitGroup).Done".into()),
                BreakpointOptions {
                    condition: Some(Condition::parse(&condition).expect("a condition")),
                    ..BreakpointOptions::default()
                },
            ),
        )
        .await;

    let reason = session.scenario.resume_to_stop().await;
    assert!(
        matches!(reason, StopReason::Breakpoint { .. }),
        "{fixture}: {reason:?}"
    );
    assert_eq!(evaluate(&session, &condition).await, Some(true));

    let InferiorState::Stopped { thread_id, .. } = session.scenario.snapshot().await.inferior
    else {
        panic!("{fixture}: not stopped");
    };

    let activity = session
        .scenario
        .snapshot()
        .await
        .threads
        .iter()
        .find(|thread| thread.id == thread_id)
        .and_then(|thread| thread.activity.clone());
    assert!(
        matches!(activity, Some(ThreadActivity::Task { task, .. }) if task == worker),
        "{fixture}: {activity:?}"
    );
    session.scenario.shutdown().await;
}

/// A boolean or integer expression's value in the selected frame, or
/// `None` where it is unavailable.
async fn evaluate(session: &GoSession, text: &str) -> Option<bool> {
    let expression = Expression::parse(text).expect("an expression");
    let Evaluation::Value { value, .. } = session
        .scenario
        .operation(text, session.scenario.handle().evaluate(&expression))
        .await
    else {
        panic!("{text}: not a value");
    };

    match value.state {
        VariableState::Available {
            value: VariableValue::Scalar(ScalarValue::Boolean(holds)),
            ..
        } => Some(holds),
        VariableState::Unavailable(_) => None,
        other => panic!("{text}: {other:?}"),
    }
}

/// A parked task has the registers its runtime saved, and none a thread
/// would give it.
async fn check_saved_registers(session: &GoSession, view: StopContext, trace: &Backtrace) {
    let fixture = &session.fixture;
    let registers = session
        .scenario
        .operation(
            "task registers",
            session.scenario.handle().at(view).registers(),
        )
        .await;
    assert_eq!(registers.context, view.execution, "{fixture}");
    let register = |name: &str| {
        registers
            .registers
            .iter()
            .find(|register| register.register.name.as_ref() == name)
            .and_then(|register| register.bytes.as_deref())
            .map(|bytes| u64::from_le_bytes(bytes.try_into().expect("a word")))
    };

    assert_eq!(
        register("rip"),
        Some(
            trace.frames[0]
                .instruction
                .expect("a thread's frame has an instruction")
                .get()
        ),
        "{fixture}: {registers:#?}"
    );
    assert!(register("rsp").is_some(), "{fixture}: {registers:#?}");

    for name in ["rax", "fs_base", "orig_rax"] {
        assert_eq!(register(name), None, "{fixture}: {registers:#?}");
    }
}

async fn check_worker_arguments(session: &GoSession, view: StopContext) {
    let fixture = &session.fixture;
    let variables = session
        .scenario
        .operation(
            "task variables",
            session.scenario.handle().at(view).variables(),
        )
        .await;
    assert_eq!(variables.context, view.execution, "{fixture}");
    let names = variables
        .variables
        .iter()
        .filter(|variable| matches!(variable.state, VariableState::Available { .. }))
        .map(|variable| variable.name.as_ref())
        .collect::<BTreeSet<_>>();

    for name in ["jobs", "results", "group"] {
        assert!(names.contains(name), "{fixture}: {variables:#?}");
    }
}

/// Whether the runtime leaves a function out of a goroutine dump: its own
/// code, and the wrappers the compiler writes.
fn runtime_hides(function: &str) -> bool {
    function.starts_with("runtime.")
        || function.starts_with("internal/runtime/")
        || function.contains(".gowrap")
}
