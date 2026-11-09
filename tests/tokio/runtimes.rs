//! Two runtimes, two `LocalSet`s, and a thread of the program's own in one
//! process: every task of each is listed, whether its set is running or
//! waits in the frame that drives it, each named by the runtime or set
//! that holds it; a thread runs a task exactly when its set runs it. A
//! set whose driving future an optimized build loses is said to be
//! missing, never silently left out.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
use std::process::Stdio;

use microscope::{
    BreakpointSpec, CoreDumpOptions, Evaluation, EvaluationMode, Expression, InferiorState,
    InspectionLimits, LaunchOptions, RegisterRole, StackFrameId, StopContext, StopReason,
    TaskSnapshot, TaskState, ThreadActivity, ThreadId,
};

use crate::invariants::checked;
use crate::support::truth::Report;
use crate::support::{Scenario, ScratchDir, register_u64};
use crate::workers::{activities, tasks};

const BUILDS: [&str; 4] = [
    "tokio-runtimes-o0",
    "tokio-runtimes-o3",
    "tokio-runtimes-1.52-o0",
    "tokio-runtimes-1.52-o3",
];

/// What the program reported: each task's runtime, and its threads.
#[derive(Debug, Default)]
struct Truth {
    /// Each task's id and the runtime or set the program says holds it.
    tasks: BTreeMap<u64, String>,

    /// Each of the program's named threads.
    threads: BTreeMap<String, ThreadId>,
}

impl Truth {
    fn parse(text: &str) -> Self {
        let mut truth = Self::default();
        for fields in Report::parse(text).lines() {
            match fields.iter().map(String::as_str).collect::<Vec<_>>()[..] {
                ["value", id, "runtime", runtime] => {
                    truth
                        .tasks
                        .insert(id.parse().expect("an id"), runtime.to_owned());
                }
                ["thread", name, tid] => {
                    truth.threads.insert(
                        name.to_owned(),
                        ThreadId::new(tid.parse().expect("a thread id")),
                    );
                }
                _ => {}
            }
        }

        truth
    }

    /// Fails unless the list holds each of the program's tasks once, with
    /// one runtime label for each runtime or set and different labels for
    /// different ones, and in the state `state` gives it.
    ///
    /// An optimized build may lose the future the thread `shared` drives,
    /// and with it the set that future runs: then the list says so, and
    /// leaves out exactly that set's tasks.
    fn check_tasks(
        &self,
        tasks: &[TaskSnapshot],
        gaps: &[String],
        state: impl Fn(u64) -> TaskState,
        context: &str,
    ) {
        let lost = match gaps {
            [] => BTreeSet::new(),
            [gap]
                if context.contains("o3")
                    && gap.starts_with(&format!(
                        "thread {} drives a future that may run a local set",
                        self.threads["shared"]
                    )) =>
            {
                self.tasks
                    .iter()
                    .filter(|(_, runtime)| *runtime == "shared")
                    .map(|(id, _)| *id)
                    .collect()
            }
            _ => panic!("{context}: {gaps:?}"),
        };

        let listed = tasks
            .iter()
            .map(|task| task.id.number)
            .collect::<BTreeSet<_>>();

        assert_eq!(listed.len(), tasks.len(), "{context}: {tasks:#?}");
        assert_eq!(
            listed,
            self.tasks
                .keys()
                .copied()
                .filter(|id| !lost.contains(id))
                .collect(),
            "{context}: {tasks:#?}"
        );

        let mut labels = BTreeMap::new();
        for task in tasks {
            let number = task.id.number;
            assert_eq!(task.state, state(number), "{context}: task {number}");
            let label = task
                .labels
                .iter()
                .find(|(key, _)| &**key == "runtime")
                .map_or_else(
                    || panic!("{context}: task {number} names no runtime"),
                    |(_, value)| value.to_string(),
                );
            labels.insert(number, label);
        }

        let runtimes = |by: &dyn Fn(u64) -> String| {
            listed
                .iter()
                .map(|&id| (by(id), id))
                .fold(
                    BTreeMap::<String, BTreeSet<u64>>::new(),
                    |mut groups, (key, id)| {
                        groups.entry(key).or_default().insert(id);
                        groups
                    },
                )
                .into_values()
                .collect::<BTreeSet<_>>()
        };

        assert_eq!(
            runtimes(&|id| labels[&id].clone()),
            runtimes(&|id| self.tasks[&id].clone()),
            "{context}: {labels:?}"
        );
    }
}

/// The program stopped at `function`, with its truth so far.
async fn stopped_at(fixture: &str, function: &str) -> (Scenario, Truth, ScratchDir) {
    let scratch = ScratchDir::new("runtimes");
    let output = scratch.path().join("stdout");
    let mut scenario = checked(fixture);

    scenario
        .add_breakpoint_spec(BreakpointSpec::Function(function.into()))
        .await;

    let reason = scenario
        .run_with_to_stop(LaunchOptions {
            stdout: Some(Stdio::from(File::create(&output).expect("an output file"))),
            ..LaunchOptions::default()
        })
        .await;

    assert!(
        matches!(reason, StopReason::Breakpoint { .. }),
        "{fixture}: {reason:?}"
    );

    let truth = Truth::parse(&std::fs::read_to_string(&output).expect("the output"));

    (scenario, truth, scratch)
}

/// Fails unless no thread runs a task, the program's threads are its own,
/// and the multi-thread runtime's workers are idle.
fn check_parked_threads(
    activities: &BTreeMap<ThreadId, ThreadActivity>,
    truth: &Truth,
    context: &str,
) {
    for (name, thread) in &truth.threads {
        assert_eq!(
            activities.get(thread),
            Some(&ThreadActivity::Outside),
            "{context}: thread {name}"
        );
    }

    assert!(
        activities
            .values()
            .all(|activity| !matches!(activity, ThreadActivity::Task { .. })),
        "{context}: {activities:?}"
    );
}

/// At the checkpoint neither set runs, and their tasks are found from the
/// futures their threads drive.
#[tokio::test]
async fn every_runtimes_and_sets_tasks_are_listed() {
    for fixture in BUILDS {
        let (mut scenario, truth, _scratch) = stopped_at(fixture, "truth_reached").await;
        assert_eq!(truth.tasks.len(), 8, "{fixture}: {truth:?}");

        let (tasks, gaps) = tasks(&scenario, 3).await;
        truth.check_tasks(&tasks, &gaps, |_| TaskState::Blocked, fixture);

        check_parked_threads(&activities(&mut scenario).await, &truth, fixture);
        scenario.shutdown().await;
    }
}

/// A core of the checkpoint lists them all as a live program does.
#[tokio::test]
async fn a_cores_runtimes_and_sets_are_listed() {
    for fixture in BUILDS {
        let core = format!("{fixture}.core");
        let log = std::fs::read_to_string(Scenario::fixture(&format!("{core}.log")))
            .expect("the core's log");

        let truth = Truth::parse(&log);
        let mut scenario =
            Scenario::open_core(&core, &CoreDumpOptions::new(Scenario::fixture(&core)));

        let (tasks, gaps) = tasks(&scenario, 4096).await;
        truth.check_tasks(&tasks, &gaps, |_| TaskState::Blocked, &core);

        check_parked_threads(&activities(&mut scenario).await, &truth, &core);
        scenario.shutdown().await;
    }
}

/// A set's task stopped at a breakpoint runs on the thread that runs the
/// set, which blocks on the multi-thread runtime; it runs no blocking
/// closure, and the set's other task and every other task are listed too.
#[tokio::test]
async fn a_running_sets_task_runs_on_its_thread() {
    for fixture in BUILDS {
        let (mut scenario, truth, _scratch) = stopped_at(fixture, "task_reached").await;
        let shared = truth.threads["shared"];
        let (tasks, gaps) = tasks(&scenario, 4096).await;

        // The set runs, so its thread names it, whatever its frames lose.
        assert!(gaps.is_empty(), "{fixture}: {gaps:?}");

        let running = tasks
            .iter()
            .find(|task| task.thread == Some(shared))
            .unwrap_or_else(|| panic!("{fixture}: no task on the set's thread: {tasks:#?}"));

        assert_eq!(running.detail.as_deref(), Some("running"), "{fixture}");
        let go = running.id.number;

        truth.check_tasks(
            &tasks,
            &gaps,
            |id| {
                if id == go {
                    TaskState::Running
                } else {
                    TaskState::Blocked
                }
            },
            fixture,
        );

        let activities = activities(&mut scenario).await;
        assert!(
            matches!(activities[&shared], ThreadActivity::Task { task, .. } if task.number == go),
            "{fixture}: {activities:?}"
        );

        scenario.shutdown().await;
    }
}

/// The program stopped at its checkpoint with nothing read at the stop,
/// which a checked scenario reads in full as the stop arrives.
async fn stopped_unread(fixture: &str) -> Scenario {
    let mut scenario = Scenario::launch(fixture);
    scenario
        .add_breakpoint_spec(BreakpointSpec::Function("truth_reached".into()))
        .await;
    let reason = scenario
        .run_with_to_stop(LaunchOptions {
            stdout: Some(Stdio::null()),
            ..LaunchOptions::default()
        })
        .await;
    assert!(
        matches!(reason, StopReason::Breakpoint { .. }),
        "{fixture}: {reason:?}"
    );
    scenario
}

/// Moves the stack and frame pointers of the thread that runs the second
/// set down into its unused stack, where unwinding it no longer finds the
/// frame that drives the set. Returns the thread.
async fn move_shared_stack(scenario: &mut Scenario) -> ThreadId {
    let snapshot = scenario.snapshot().await;
    let InferiorState::Stopped { stop_id, .. } = snapshot.inferior else {
        panic!("not stopped");
    };
    let shared = snapshot
        .threads
        .iter()
        .find(|thread| thread.name.as_deref() == Some("shared"))
        .expect("the thread that runs the second set")
        .id;
    let view = scenario.handle().at(StopContext {
        stop: stop_id,
        execution: shared.into(),
        frame: StackFrameId::INNERMOST,
    });
    let registers = scenario.operation("registers", view.registers()).await;
    let moved = register_u64(&registers, RegisterRole::StackPointer) - 0x8000;

    for register in ["$rsp", "$rbp"] {
        let text = format!("{register} = {moved:#x}");
        let expression = Expression::parse(&text).expect("an expression");
        let assigned = scenario
            .operation(
                &text,
                view.evaluate_with(
                    &expression,
                    EvaluationMode::Assign,
                    InspectionLimits::default(),
                ),
            )
            .await;
        assert!(
            matches!(assigned, Evaluation::Value { .. }),
            "{text}: {assigned:?}"
        );
    }

    shared
}

/// The task numbers listed in order, and the gaps.
async fn listing(scenario: &Scenario) -> (Vec<u64>, Vec<String>) {
    let (tasks, gaps) = tasks(scenario, 4096).await;
    let mut numbers = tasks.iter().map(|task| task.id.number).collect::<Vec<_>>();
    numbers.sort_unstable();
    (numbers, gaps)
}

/// A set's tasks are found from the stack of the thread that drives it, so
/// moving that stack changes the list at the same stop. Listed before the
/// move or not, the list is the one read only after it, which says that
/// the thread's stack ends before the frame that drives the set.
#[tokio::test]
async fn a_register_write_changes_the_tasks_found_on_a_threads_stack() {
    let fixture = "tokio-runtimes-o0";
    let says_lost = |(_, gaps): &(Vec<u64>, Vec<String>), shared: ThreadId| {
        let named = format!("thread {shared}'s stack");
        matches!(&gaps[..], [gap] if gap.starts_with(&named))
    };

    let mut scenario = stopped_unread(fixture).await;
    let shared = move_shared_stack(&mut scenario).await;
    let moved = listing(&scenario).await;
    assert!(says_lost(&moved, shared), "{fixture}: {moved:?}");
    scenario.shutdown().await;

    let mut scenario = stopped_unread(fixture).await;
    let before = listing(&scenario).await;
    assert!(before.1.is_empty(), "{fixture}: {before:?}");
    assert_ne!(before.0, moved.0, "{fixture}: the move loses the set");
    let shared = move_shared_stack(&mut scenario).await;
    let after = listing(&scenario).await;
    assert_eq!(after.0, moved.0, "{fixture}");
    assert!(says_lost(&after, shared), "{fixture}: {after:?}");
    scenario.shutdown().await;
}

/// glibc's `clone3` has no call-frame information after its system call.
/// The thread that runs the first set starts a thread from it. Stopped as
/// that call returns, its stack unwinds whole, through the frame that
/// drives the set, and the list holds the set's tasks with no gap.
#[tokio::test]
async fn a_thread_stopped_in_clone_still_shows_the_set_it_runs() {
    use microscope::CatchPhase;
    use microscope::syscalls::{Abi, SyscallId};

    for fixture in ["tokio-runtimes-o0", "tokio-runtimes-1.52-o0"] {
        let mut scenario = Scenario::launch(fixture);
        let clone3 = SyscallId::named(Abi::X86_64, "clone3").expect("x86-64 names clone3");
        scenario
            .operation(
                "catch clone3",
                scenario
                    .handle()
                    .add_catchpoint([clone3].into(), CatchPhase::Exit, None),
            )
            .await;
        let mut reason = scenario
            .run_with_to_stop(LaunchOptions {
                arguments: vec!["clone".into()],
                stdout: Some(Stdio::null()),
                ..LaunchOptions::default()
            })
            .await;

        // The clone the thread `local` makes.
        let local = loop {
            assert!(
                matches!(reason, StopReason::Syscall(_)),
                "{fixture}: {reason:?}"
            );
            let snapshot = scenario.snapshot().await;
            let InferiorState::Stopped { thread_id, .. } = snapshot.inferior else {
                panic!("{fixture}: not stopped");
            };
            let name = snapshot
                .threads
                .iter()
                .find(|thread| thread.id == thread_id)
                .and_then(|thread| thread.name.as_deref());
            if name == Some("local") {
                break thread_id;
            }
            reason = scenario.resume_to_stop().await;
        };

        let (tasks, gaps) = tasks(&scenario, 4096).await;
        assert!(gaps.is_empty(), "{fixture}: {gaps:?}");
        let in_set = tasks
            .iter()
            .filter(|task| {
                task.labels
                    .iter()
                    .any(|(key, value)| &**key == "runtime" && value.to_string().contains("local"))
            })
            .count();
        assert_eq!(in_set, 2, "{fixture}: thread {local}: {tasks:#?}");
        let trace = scenario
            .operation("backtrace", scenario.handle().backtrace())
            .await;
        assert_eq!(
            trace.termination,
            microscope::UnwindTermination::Complete,
            "{fixture}: {trace:#?}"
        );
        assert!(
            trace.frames.iter().any(|frame| frame
                .function
                .as_ref()
                .is_some_and(|function| function.name.starts_with("block_on"))),
            "{fixture}: {trace:#?}"
        );
        scenario.shutdown().await;
    }
}

/// A set run in one branch of a `join!` is idle while the other branch
/// runs, so no thread's context names it. It is found in the future the
/// thread drives, where the `join!` holds it.
#[tokio::test]
async fn a_set_a_join_holds_is_listed_while_its_sibling_runs() {
    for fixture in BUILDS {
        let mut scenario = Scenario::launch(fixture);
        scenario
            .add_breakpoint_spec(BreakpointSpec::Function("join_sibling".into()))
            .await;
        let reason = scenario
            .run_with_to_stop(LaunchOptions {
                arguments: vec!["join".into()],
                stdout: Some(Stdio::null()),
                ..LaunchOptions::default()
            })
            .await;
        assert!(
            matches!(reason, StopReason::Breakpoint { .. }),
            "{fixture}: {reason:?}"
        );

        let snapshot = scenario.snapshot().await;
        let InferiorState::Stopped { thread_id, .. } = snapshot.inferior else {
            panic!("{fixture}: not stopped");
        };
        assert_eq!(
            snapshot
                .threads
                .iter()
                .find(|thread| thread.id == thread_id)
                .and_then(|thread| thread.name.as_deref()),
            Some("local"),
            "{fixture}"
        );

        let (tasks, gaps) = tasks(&scenario, 4096).await;
        assert!(gaps.is_empty(), "{fixture}: thread {thread_id}: {gaps:?}");
        let in_set = tasks
            .iter()
            .filter(|task| {
                task.labels
                    .iter()
                    .any(|(key, value)| &**key == "runtime" && value.to_string().contains("local"))
            })
            .count();
        assert_eq!(in_set, 2, "{fixture}: {tasks:#?}");

        // The set runs none of its tasks while the other branch runs.
        let activities = activities(&mut scenario).await;
        assert!(
            activities
                .values()
                .all(|activity| !matches!(activity, ThreadActivity::Task { .. })),
            "{fixture}: {activities:?}"
        );
        scenario.shutdown().await;
    }
}
