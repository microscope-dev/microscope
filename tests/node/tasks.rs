//! Node's event loop's work as tasks (`tests/fixtures/node/tasks.js`): each
//! handle, request, and timer of the main thread's and a worker's
//! `Environment`, numbered by the async id `async_hooks` saw Node give it,
//! of the type `async_hooks` named, triggered by the resource `async_hooks`
//! said, ref'd as `hasRef()` says, with the fixture's callback at its line;
//! and each busy thread of libuv's pool, as `/proc` names them.

use std::collections::BTreeSet;

use microscope::{CoreDumpOptions, TaskSnapshot, TaskState};

use crate::session::{CORES, MATRIX, NodeSession};
use crate::support::truth::Report;
use crate::support::{Scenario, ScratchDir};
use crate::truth::threads;

const FIXTURE: &str = "tasks.js";

/// A live resource as `async_hooks` saw it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resource {
    pub id: u64,
    pub kind: String,
    pub trigger: u64,
    pub refed: Option<bool>,
    pub callback: Option<(String, u32)>,
}

/// What the fixture said of itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Truth {
    pub resources: Vec<Resource>,
    pub pool: BTreeSet<u64>,

    /// The worker's thread and its timer's async id.
    pub worker: (u64, u64),
}

fn number<T: std::str::FromStr>(field: &str) -> T {
    field
        .parse()
        .unwrap_or_else(|_| panic!("a number, not {field:?}"))
}

/// The truth `report` holds, whose checkpoint is `lines`.
pub fn truth(report: &Report, lines: &[Vec<String>]) -> Truth {
    let mut resources = Vec::new();
    let mut pool = BTreeSet::new();
    for fields in report.lines() {
        match fields[0].as_str() {
            "task" => resources.push(Resource {
                id: number(&fields[1]),
                kind: fields[2].clone(),
                trigger: number(&fields[3]),
                refed: match fields[4].as_str() {
                    "-" => None,
                    refed => Some(refed == "1"),
                },
                callback: (fields[5] != "-").then(|| (fields[5].clone(), number(&fields[6]))),
            }),
            "pool" => {
                pool.insert(number(&fields[1]));
            }
            _ => {}
        }
    }
    let worker = threads(lines)
        .into_iter()
        .find(|thread| thread.name == "ticker")
        .expect("the worker's report");
    let timer = worker
        .values
        .iter()
        .find(|value| value.name == "timer")
        .expect("the worker's timer");
    Truth {
        resources,
        pool,
        worker: (worker.native, number(&timer.text)),
    }
}

/// One task as microscope shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Seen {
    pub number: u64,
    pub stackless: bool,
    pub thread: Option<u64>,
    pub state: TaskState,
    pub detail: String,
    pub kind: Option<String>,
    pub callback: Option<(String, Option<u32>)>,
    pub parent: Option<u64>,
}

fn seen(task: &TaskSnapshot) -> Seen {
    Seen {
        number: task.id.number,
        stackless: task.stackless,
        thread: task.thread.map(microscope::ThreadId::get),
        state: task.state.clone(),
        detail: task.detail.as_deref().unwrap_or_default().to_owned(),
        kind: task
            .labels
            .iter()
            .find(|(key, _)| &**key == "type")
            .map(|(_, value)| value.to_string()),
        callback: task.resume.as_ref().and_then(|resume| {
            Some((
                resume.function.as_deref()?.to_owned(),
                resume.recorded.as_ref().map(|place| place.line),
            ))
        }),
        parent: task.parent.map(|parent| parent.number),
    }
}

/// The tasks a stopped scenario lists, and its gaps.
async fn listed(scenario: &Scenario) -> (Vec<Seen>, Vec<String>) {
    let mut tasks = Vec::new();
    let mut gaps = Vec::new();
    let mut from = None;
    loop {
        let page = scenario
            .operation("tasks", scenario.handle().tasks(from, 64))
            .await;
        tasks.extend(page.tasks.iter().map(seen));
        gaps.extend(page.gaps.iter().map(ToString::to_string));
        match page.next {
            Some(next) => from = Some(next),
            None => return (tasks, gaps),
        }
    }
}

/// Whether `observed`, with `gaps`, is what `truth` says, for a build that
/// links libuv's pool where `pool`. An error says every lie.
pub fn check(observed: &[Seen], gaps: &[String], truth: &Truth, pool: bool) -> Result<(), String> {
    let mut lies = Vec::new();
    let (worker, timer) = truth.worker;
    let worker_number = (worker << 32) | timer;

    for task in observed.iter().filter(|task| task.stackless) {
        if task.number == worker_number {
            if task.kind.as_deref() != Some("Timeout")
                || task.callback.as_ref().map(|(name, _)| name.as_str()) != Some("workerTimer")
            {
                lies.push(format!("the worker's timer is {task:?}"));
            }
            continue;
        }
        if task.number >> 32 != 0 {
            // Another of the worker's own, as its message port.
            if task.number >> 32 != worker {
                lies.push(format!(
                    "{task:?} is numbered for no thread that runs JavaScript"
                ));
            }
            continue;
        }
        let Some(real) = truth.resources.iter().find(|real| real.id == task.number) else {
            lies.push(format!("{task:?} is no live resource"));
            continue;
        };
        if task.kind.as_deref() != Some(real.kind.as_str()) {
            lies.push(format!(
                "{} is a {}, not {:?}",
                real.id, real.kind, task.kind
            ));
        }
        if task.parent != Some(real.trigger) {
            lies.push(format!(
                "{} was triggered by {}, not {:?}",
                real.id, real.trigger, task.parent
            ));
        }
        if let Some(refed) = real.refed
            && refed == task.detail.contains("unref'd")
        {
            lies.push(format!(
                "{} is ref'd: {refed}, but shows {:?}",
                real.id, task.detail
            ));
        }
        if let Some((name, line)) = &real.callback
            && task.callback != Some((name.clone(), Some(*line)))
        {
            lies.push(format!(
                "{}'s callback is {name} at line {line}, not {:?}",
                real.id, task.callback
            ));
        }
        if real.kind == "TCPSERVERWRAP" && !task.detail.contains("listening") {
            lies.push(format!("the server shows {:?}", task.detail));
        }
        // async_hooks destroys a message port only after it closed, which
        // the fixture's own ports, transferred, are doing.
        let closing = real.kind == "MESSAGEPORT"
            && task.state == TaskState::Exited
            && task.detail.contains("closing");
        if task.state != TaskState::Blocked && !closing {
            lies.push(format!("{} is {:?}: {}", real.id, task.state, task.detail));
        }
    }
    // A message port async_hooks has not yet seen destroyed may be closed.
    for real in truth
        .resources
        .iter()
        .filter(|real| real.kind != "MESSAGEPORT")
    {
        if !observed
            .iter()
            .any(|task| task.stackless && task.number == real.id)
        {
            lies.push(format!("no task for {real:?}"));
        }
    }
    if !observed.iter().any(|task| task.number == worker_number) {
        lies.push(format!(
            "no task for the worker's timer {timer} on thread {worker}"
        ));
    }

    check_pool(observed, gaps, truth, pool, &mut lies);
    if lies.is_empty() {
        Ok(())
    } else {
        Err(lies.join("\n"))
    }
}

/// Whether the pool jobs in `observed`, and `gaps`, are what `truth` says
/// for a build that links libuv's pool where `pool`; each lie is added to
/// `lies`.
fn check_pool(
    observed: &[Seen],
    gaps: &[String],
    truth: &Truth,
    pool: bool,
    lies: &mut Vec<String>,
) {
    let pool_job = 1 << 62;
    let jobs: BTreeSet<u64> = observed
        .iter()
        .filter(|task| !task.stackless)
        .map(|task| {
            if task.number != pool_job | task.thread.unwrap_or(0)
                || task.state != TaskState::Running
                || task.callback.is_some()
            {
                lies.push(format!("{task:?} is no thread-pool job"));
            }
            task.thread.unwrap_or(0)
        })
        .collect();
    let pool_gap = gaps
        .iter()
        .any(|gap| gap.contains("thread-pool jobs are not listed"));
    if pool {
        if jobs != truth.pool {
            lies.push(format!(
                "the pool runs jobs on {:?}, not {jobs:?}",
                truth.pool
            ));
        }
    } else if !jobs.is_empty() || !pool_gap {
        lies.push(format!(
            "a build without libuv's pool lists {jobs:?}, with gaps {gaps:?}"
        ));
    }
    let unexpected: Vec<_> = gaps
        .iter()
        .filter(|gap| pool || !gap.contains("thread-pool jobs are not listed"))
        .collect();
    if !unexpected.is_empty() {
        lies.push(format!("gaps: {unexpected:?}"));
    }
}

/// Whether the build `entry` links libuv, and so its pool, into its node.
fn links_pool(entry: &str) -> bool {
    !entry.contains('-')
}

/// A scratch directory holding a FIFO no one writes, for the fixture.
pub fn fifo() -> (ScratchDir, String) {
    let scratch = ScratchDir::new("node-tasks");
    let path = scratch.path().join("fifo");
    nix::unistd::mkfifo(
        &path,
        nix::sys::stat::Mode::S_IRUSR | nix::sys::stat::Mode::S_IWUSR,
    )
    .expect("make the FIFO");
    let path = path.to_str().expect("a UTF-8 path").to_owned();
    (scratch, path)
}

/// What, on the build `entry`, the tasks get wrong.
pub async fn task_lies(entry: &str) -> Result<(), String> {
    let (_scratch, fifo) = fifo();
    let mut session = NodeSession::launch(entry, FIXTURE, &[], &[&fifo]).await;
    let lines = session.stop_at("tasks").await;
    let truth = truth(&session.report(), &lines);
    let (observed, gaps) = listed(&session.scenario).await;
    let context = session.context();
    session.shutdown().await;
    check(&observed, &gaps, &truth, links_pool(entry))
        .map_err(|lies| format!("{context}:\n{lies}\nlisted: {observed:#?}"))
}

/// On every build, the tasks are the resources Node said it waits on.
#[tokio::test]
async fn nodes_handles_requests_timers_and_pool_jobs_are_the_ones_it_reported() {
    let mut lies = Vec::new();
    for entry in MATRIX {
        if let Err(lie) = task_lies(entry).await {
            lies.push(lie);
        }
    }
    assert!(lies.is_empty(), "{}", lies.join("\n\n"));
}

/// Each core holds the tasks its fixture reported just before it aborted,
/// with Node's `nodedbg_` variables read from the core's memory.
#[tokio::test]
async fn a_cores_tasks_are_the_ones_node_reported() {
    let mut wrong = Vec::new();
    for entry in CORES {
        let name = format!("node-tasks-{entry}.core");
        let core = Scenario::fixture(&name);
        let log = std::fs::read_to_string(core.with_extension("core.log"))
            .expect("the core's log; run `just build-test-programs`");
        let report = Report::parse(&log);
        let lines = report
            .checkpoint("tasks")
            .expect("the checkpoint the core was taken at")
            .to_vec();
        let truth = truth(&report, &lines);
        let scenario = Scenario::open_core(&name, &CoreDumpOptions::new(core));
        let (observed, gaps) = listed(&scenario).await;
        scenario.shutdown().await;
        if let Err(lie) = check(&observed, &gaps, &truth, links_pool(entry)) {
            wrong.push(format!("{name}:\n{lie}\nlisted: {observed:#?}"));
        }
    }
    assert!(wrong.is_empty(), "{}", wrong.join("\n\n"));
}

/// The comparison fails on a dropped task, a wrong callback line, a wrong
/// trigger, a wrong ref, a worker's timer numbered as the main thread's, a
/// missing pool job, and a pool gap where a pool is linked; and holds for
/// a faithful list.
#[tokio::test]
async fn the_tasks_comparison_fails_on_the_lies_it_looks_for() {
    let (_scratch, fifo) = fifo();
    let mut session = NodeSession::launch("26", FIXTURE, &[], &[&fifo]).await;
    let lines = session.stop_at("tasks").await;
    let truth = truth(&session.report(), &lines);
    let (observed, gaps) = listed(&session.scenario).await;
    session.shutdown().await;
    check(&observed, &gaps, &truth, true).expect("the list holds");

    let lied = |change: &dyn Fn(&mut Vec<Seen>)| {
        let mut changed = observed.clone();
        change(&mut changed);
        check(&changed, &gaps, &truth, true).is_err()
    };
    let main = |task: &Seen| task.stackless && task.number >> 32 == 0;
    let labelled = |task: &Seen| {
        truth
            .resources
            .iter()
            .any(|real| real.id == task.number && real.callback.is_some())
    };
    assert!(
        lied(&|tasks| {
            let at = tasks.iter().position(&main).expect("a task");
            tasks.remove(at);
        }),
        "a dropped task"
    );
    assert!(
        lied(&|tasks| {
            let task = tasks.iter_mut().find(|t| labelled(t)).expect("a callback");
            if let Some((_, Some(line))) = &mut task.callback {
                *line += 1;
            }
        }),
        "a wrong callback line"
    );
    assert!(
        lied(&|tasks| {
            let task = tasks.iter_mut().find(|t| main(t)).expect("a task");
            task.parent = task.parent.map(|parent| parent + 1);
        }),
        "a wrong trigger"
    );
    assert!(
        lied(&|tasks| {
            let task = tasks
                .iter_mut()
                .find(|t| t.detail.contains("unref'd"))
                .expect("an unref'd timer");
            task.detail = task.detail.replace(", unref'd", "");
        }),
        "a wrong ref"
    );
    assert!(
        lied(&|tasks| {
            let task = tasks
                .iter_mut()
                .find(|t| t.number >> 32 == truth.worker.0)
                .expect("the worker's timer");
            task.number &= u64::from(u32::MAX);
        }),
        "a worker's timer numbered as main's"
    );
    assert!(
        lied(&|tasks| tasks.retain(|t| t.stackless)),
        "a missing pool job"
    );
    let mut gapped = gaps.clone();
    gapped.push("libuv's thread-pool jobs are not listed: planted".to_owned());
    assert!(
        check(&observed, &gapped, &truth, true).is_err(),
        "a pool gap where the pool is linked"
    );
}
