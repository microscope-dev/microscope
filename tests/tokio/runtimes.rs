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
    BreakpointSpec, CoreDumpOptions, LaunchOptions, StopReason, TaskSnapshot, TaskState,
    ThreadActivity, ThreadId,
};

use crate::invariants::checked;
use crate::support::truth::Report;
use crate::support::{Scenario, ScratchDir};
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
