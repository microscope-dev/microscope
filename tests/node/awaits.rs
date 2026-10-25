//! The async functions suspended in await below a thread's stack, and the
//! promise combinators between them, held to the async frames V8's own call
//! sites reported at the same checkpoint (`isAsync()`, which V8's
//! `CaptureAsyncStackTrace` finds), and each placed after the function the
//! thread resumed.

use microscope::{Backtrace, DrivenFrames, StackSegment, ThreadId};

use crate::frames::{awaiting_recorded, observed};
use crate::session::{MATRIX, NodeSession};
use crate::support::Scenario;
use crate::truth::{Frame, Thread, check_threads, threads};

/// Each checkpoint of `awaits.js`, in order, with the function its printer
/// resumed, and whether V8's chain ends where several reactions wait.
pub const CHECKPOINTS: [(&str, &str, bool); 8] = [
    ("chain", "inner", false),
    ("resumed", "middle", false),
    ("allSettled", "settledLeaf", false),
    ("any", "anyLeaf", false),
    ("then", "thenLeaf", false),
    ("resolver", "resolverLeaf", false),
    ("generator", "produce", false),
    ("shared", "sharedLeaf", true),
];

/// The function the worker resumed, at every checkpoint.
const WORKER_RESUMED: &str = "workerLeaf";

/// Each way the await chain of `trace`, a backtrace of the thread
/// `reported`, which resumed `resumed`, is misplaced or partial: its
/// frames must follow the resumed function's frame, all together, each
/// with its place known; and the chain may end early only where V8's ends
/// at a promise several reactions wait for, which a note says.
pub fn placement(trace: &Backtrace, resumed: &str, ends_early: bool) -> Vec<String> {
    let mut lies = Vec::new();
    let frames = &trace.frames;
    let notes: Vec<_> = trace
        .unfollowed
        .iter()
        .filter(|note| note.driven == DrivenFrames::Awaiting)
        .collect();
    match (ends_early, &notes[..]) {
        (false, []) => {}
        (true, [note]) if note.reason.contains("several reactions") => {}
        _ => lies.push(format!("the chain's notes are {notes:?}")),
    }
    for (frame, problem) in awaiting_recorded(trace) {
        if let Some(problem) = problem {
            lies.push(format!("{} has a problem: {problem}", frame.function));
        }
    }

    let Some(first) = frames
        .iter()
        .position(|frame| frame.segment == StackSegment::Awaiting)
    else {
        return lies;
    };
    let run = frames[first..]
        .iter()
        .take_while(|frame| frame.segment == StackSegment::Awaiting)
        .count();
    if frames[first + run..]
        .iter()
        .any(|frame| frame.segment == StackSegment::Awaiting)
    {
        lies.push("the await chain is split".to_owned());
    }
    let before = first
        .checked_sub(1)
        .and_then(|at| frames[at].recorded.as_ref())
        .map(|recorded| recorded.function.to_string());
    if before.as_deref() != Some(resumed) {
        lies.push(format!(
            "the await chain follows {before:?}, not {resumed}'s frame"
        ));
    }
    lies
}

/// On every build, at every checkpoint, each thread's await chain is the
/// one V8 reported, after the function the thread resumed, and its stack
/// is the one V8 reported above it.
#[tokio::test]
async fn each_await_chain_is_the_one_v8_reported() {
    let mut lies = Vec::new();
    for entry in MATRIX {
        let mut session = NodeSession::launch(entry, "awaits.js", &[], &[]).await;
        for (index, (checkpoint, resumed, ends_early)) in CHECKPOINTS.into_iter().enumerate() {
            if index > 0 {
                session.proceed().await;
            }
            let reported = threads(&session.stop_at(checkpoint).await);
            let context = format!("{} at {checkpoint}", session.context());
            lies.extend(check(&session.scenario, &reported, resumed, ends_early, &context).await);
        }
        session.shutdown().await;
    }
    assert!(lies.is_empty(), "{lies:#?}");
}

/// Each lie the backtraces of the threads `reported` tell at a stop of
/// `awaits.js` whose printer resumed `resumed`.
pub async fn check(
    scenario: &Scenario,
    reported: &[Thread],
    resumed: &str,
    ends_early: bool,
    context: &str,
) -> Vec<String> {
    let mut lies = Vec::new();
    assert!(
        reported.iter().any(|thread| !thread.awaiting.is_empty()),
        "{context}: V8 reported no await chain: {reported:#?}"
    );
    let seen = observed(scenario, reported).await;
    if let Err(lie) = check_threads(&seen, reported) {
        lies.push(format!("{context}: {lie}"));
    }
    for thread in reported {
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
        let (resumed, ends_early) = if thread.printer {
            (resumed, ends_early)
        } else {
            (WORKER_RESUMED, false)
        };
        lies.extend(
            placement(&trace, resumed, ends_early)
                .into_iter()
                .map(|lie| format!("{context} thread {}: {lie}", thread.name)),
        );
    }
    lies
}

/// The comparisons fail on a dropped async frame, a wrong line, a wrong
/// combinator index, a chain that is not there, and one where V8 has none,
/// and hold for the truth itself.
#[tokio::test]
async fn the_await_truth_fails_on_the_lies_it_looks_for() {
    let mut session = NodeSession::launch("26", "awaits.js", &[], &[]).await;
    let truth = threads(&session.stop_at("chain").await);
    session.shutdown().await;

    check_threads(&truth, &truth).expect("the truth holds");
    let chain = |thread: &Thread| -> Vec<Frame> { thread.awaiting.clone() };
    assert!(chain(&truth[0]).len() >= 4, "{truth:#?}");

    let mut dropped = truth.clone();
    dropped[0].awaiting.remove(0);
    assert!(check_threads(&dropped, &truth).is_err(), "a dropped frame");

    let mut moved = truth.clone();
    *moved[0].awaiting[0].line.as_mut().expect("a line") += 1;
    assert!(check_threads(&moved, &truth).is_err(), "a wrong line");

    let mut reindexed = truth.clone();
    let combinator = reindexed[0]
        .awaiting
        .iter_mut()
        .find(|frame| frame.function.starts_with("Promise.all"))
        .expect("a combinator");
    combinator.function = "Promise.all (index 0)".to_owned();
    assert!(check_threads(&reindexed, &truth).is_err(), "a wrong index");

    let mut missing = truth.clone();
    missing[1].awaiting.clear();
    assert!(check_threads(&missing, &truth).is_err(), "a missing chain");

    let mut invented = truth.clone();
    let mut none = truth.clone();
    none[0].awaiting.clear();
    invented[0].awaiting.truncate(1);
    assert!(
        check_threads(&invented, &none).is_err(),
        "an invented chain"
    );
}
