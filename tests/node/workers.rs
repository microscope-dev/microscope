//! Workers through Node's inspector: a breakpoint in a worker's script
//! stops its thread, with its locals, and steps there are its isolate's;
//! two isolates pausing at once are two stops, each on its own thread;
//! and an exception thrown in a worker stops on the worker's thread.

use microscope::{
    BreakpointOptions, BreakpointSpec, ExceptionStops, ExitStatus, LineNumber, StepKind,
    StopReason, ThreadId,
};

use crate::oracle;
use crate::session::{MATRIX, NodeSession};
use crate::support::node;

const FIXTURE: &str = "workers.js";
const CRUNCH: u32 = 9;
const AFTER: u32 = 10;
const THROWN: u32 = 15;

fn at_crunch() -> (BreakpointSpec, BreakpointOptions) {
    (
        BreakpointSpec::Source {
            path: node::fixture(FIXTURE),
            line: LineNumber::new(u64::from(CRUNCH)).expect("a line"),
        },
        BreakpointOptions {
            pending: true,
            ..BreakpointOptions::default()
        },
    )
}

/// The stopped thread and the summary of its selected frame's local
/// `name`.
async fn where_stopped(session: &NodeSession, name: &str) -> (ThreadId, String) {
    let snapshot = session
        .scenario
        .operation("snapshot", session.scenario.handle().snapshot())
        .await;
    let thread = match snapshot.selected {
        Some(microscope::ExecutionContext::Thread(thread)) => thread,
        other => panic!("{}: selected {other:?}", session.context()),
    };
    let variables = session
        .scenario
        .operation("variables", session.scenario.handle().variables())
        .await;
    let value = variables
        .variables
        .iter()
        .find(|variable| &*variable.name == name)
        .map_or_else(
            || "absent".to_owned(),
            |variable| match &variable.state {
                microscope::VariableState::Available {
                    presentation: Some(presentation),
                    ..
                } => presentation.summary.to_string(),
                other => format!("{other:?}"),
            },
        );
    (thread, value)
}

/// The process's main thread, whose id is the process's.
async fn main_thread(session: &NodeSession) -> ThreadId {
    let snapshot = session
        .scenario
        .operation("snapshot", session.scenario.handle().snapshot())
        .await;
    match snapshot.inferior {
        microscope::InferiorState::Stopped { process_id, .. } => ThreadId::new(process_id.get()),
        other => panic!("{}: not stopped: {other:?}", session.context()),
    }
}

/// What, on the build `entry`, a worker's breakpoint and step get wrong.
pub async fn worker_lies(entry: &str) -> Vec<String> {
    let mut lies = Vec::new();
    let (mut session, reason, _events) =
        NodeSession::launch_to_stop(entry, FIXTURE, vec![at_crunch()]).await;
    let context = session.context();
    let main = main_thread(&session).await;
    let pause = oracle::selected(&session.scenario).await;
    let (thread, n) = where_stopped(&session, "n").await;
    if !matches!(reason, StopReason::Breakpoint { address: None, .. })
        || pause.line != CRUNCH
        || thread == main
        || n != "6"
    {
        lies.push(format!(
            "{context}: {reason:?} at {pause} on {thread:?} (main {main:?}), n = {n}"
        ));
    }
    let reason = session.scenario.step_to_stop(StepKind::OverSource).await;
    let pause = oracle::selected(&session.scenario).await;
    let (stepped, squared) = where_stopped(&session, "squared").await;
    if !matches!(reason, StopReason::Step { .. })
        || pause.line != AFTER
        || stepped != thread
        || squared != "36"
    {
        lies.push(format!(
            "{context}: the step ended {reason:?} at {pause} on {stepped:?}, squared = {squared}"
        ));
    }
    let reason = session.scenario.resume_to_stop().await;
    if reason != StopReason::Exited(ExitStatus::Code(0)) || !session.output().contains("RESULT 37")
    {
        lies.push(format!("{context}: ended {reason:?}\n{}", session.output()));
    }
    session.shutdown().await;
    lies
}

/// On every build, a breakpoint in a worker stops the worker's thread at
/// its line with its locals, and a step over there ends at the worker's
/// next line, not in the main thread.
#[tokio::test]
async fn a_worker_stops_and_steps_on_its_own_thread() {
    let mut lies = Vec::new();
    for entry in MATRIX {
        lies.extend(worker_lies(entry).await);
    }
    assert!(lies.is_empty(), "{}", lies.join("\n"));
}

/// On every build, the main thread and a worker pausing at one line are
/// two stops, each on its own thread with its own `n`.
#[tokio::test]
async fn two_isolates_pausing_at_once_stop_each_on_its_thread() {
    let mut lies = Vec::new();
    for entry in MATRIX {
        let session = NodeSession::unlaunched_with(entry, FIXTURE, &["both"]);
        let (mut session, mut reason, _events) = session.run_to_stop(vec![at_crunch()]).await;
        let context = session.context();
        let main = main_thread(&session).await;
        let mut seen = Vec::new();
        while let StopReason::Breakpoint { address: None, .. } = reason {
            let (thread, n) = where_stopped(&session, "n").await;
            seen.push((thread == main, n));
            assert!(seen.len() < 4, "{context}: too many stops");
            reason = session.scenario.resume_to_stop().await;
        }
        seen.sort();
        let expected = vec![(false, "6".to_owned()), (true, "2".to_owned())];
        if seen != expected || reason != StopReason::Exited(ExitStatus::Code(0)) {
            lies.push(format!("{context}: (main, n) {seen:?}, then {reason:?}"));
        }
        session.shutdown().await;
    }
    assert!(lies.is_empty(), "{}", lies.join("\n"));
}

/// On every build, an exception thrown in a worker stops on its thread.
#[tokio::test]
async fn an_exception_in_a_worker_stops_on_its_thread() {
    let mut lies = Vec::new();
    for entry in MATRIX {
        let session = NodeSession::unlaunched(entry, FIXTURE);
        let stops = ExceptionStops::NONE
            .with("javascript-thrown", true)
            .expect("a filter");
        session
            .scenario
            .operation(
                "choose exceptions",
                session.scenario.handle().set_exception_stops(stops),
            )
            .await;
        let (session, reason, _events) = session.run_to_stop(Vec::new()).await;
        let context = session.context();
        let main = main_thread(&session).await;
        // Node's own startup throws nothing a filter stops in user code.
        let pause = oracle::selected(&session.scenario).await;
        let (thread, _) = where_stopped(&session, "n").await;
        if !matches!(reason, StopReason::LanguageException(_))
            || pause.function != "risky"
            || pause.line != THROWN
            || thread == main
        {
            lies.push(format!("{context}: {reason:?} at {pause} on {thread:?}"));
        }
        session.shutdown().await;
    }
    assert!(lies.is_empty(), "{}", lies.join("\n"));
}

#[tokio::test]
async fn a_worker_pause_is_not_assigned_to_another_script_at_the_same_position() {
    let scratch = crate::support::ScratchDir::new("node-worker-identity");
    let worker = |operation: &str| {
        format!(
            r"
const {{ workerData, parentPort }} = require('worker_threads');
function crunch(n) {{
  const squared = {operation};
  return squared + 1;
}}
parentPort.postMessage(require('fs').readlinkSync('/proc/thread-self').split('/').pop());
parentPort.once('message', () => {{
  const result = crunch(workerData.n);
  parentPort.postMessage(result);
}});
"
        )
    };
    std::fs::write(
        scratch.path().join("parked.js"),
        worker("Atomics.wait(workerData.flags, 0, 0)"),
    )
    .expect("write the parked worker");
    let active = scratch.path().join("active.js");
    std::fs::write(&active, worker("        n * n")).expect("write the active worker");
    let main = scratch.path().join("main.js");
    std::fs::write(&main, r"
const { Worker } = require('worker_threads');
const fs = require('fs');
const flags = new Int32Array(new SharedArrayBuffer(4));
const parked = new Worker(require('path').join(__dirname, 'parked.js'), { workerData: { n: 6, flags } });
parked.once('message', (tid) => {
  parked.postMessage('go');
  const deadline = Date.now() + 5000;
  while (!fs.readFileSync(`/proc/self/task/${tid}/syscall`, 'utf8').startsWith('202 ')) {
    if (Date.now() > deadline) throw new Error('worker did not park');
  }
  const active = new Worker(require('path').join(__dirname, 'active.js'), { workerData: { n: 9, flags } });
  active.once('message', (tid) => {
    console.log('ACTIVE', tid);
    active.postMessage('go');
  });
  active.on('message', (value) => {
    if (typeof value !== 'number') return;
    Atomics.store(flags, 0, 1);
    Atomics.notify(flags, 0);
  });
});
").expect("write the main script");
    for entry in MATRIX {
        let pending = NodeSession::unlaunched_with_node(entry, &main.display().to_string(), &[]);
        let breakpoint = (
            BreakpointSpec::Source {
                path: active.clone(),
                line: LineNumber::new(4).expect("a line"),
            },
            BreakpointOptions {
                pending: true,
                ..BreakpointOptions::default()
            },
        );
        let (mut session, reason, _) = pending.run_to_stop(vec![breakpoint]).await;
        let (thread, n) = where_stopped(&session, "n").await;
        let output = session.output();
        let tid = output
            .lines()
            .find_map(|line| line.strip_prefix("ACTIVE "))
            .and_then(|tid| tid.parse::<u64>().ok())
            .expect("the active worker's native id");
        assert!(
            matches!(reason, StopReason::Breakpoint { .. }),
            "{entry}: {reason:?}"
        );
        assert_eq!((thread, n), (ThreadId::new(tid), "9".to_owned()), "{entry}");
        assert_eq!(
            session.scenario.resume_to_stop().await,
            StopReason::Exited(ExitStatus::Code(0))
        );
        session.shutdown().await;
    }
}
