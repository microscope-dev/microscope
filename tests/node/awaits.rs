//! The async functions suspended in await below a thread's stack, and the
//! promise combinators between them, held to the async frames V8's own call
//! sites reported at the same checkpoint (`isAsync()`, which V8's
//! `CaptureAsyncStackTrace` finds), and each placed after the function the
//! thread resumed.

use microscope::{
    Backtrace, CoreDumpOptions, DrivenFrames, SourceLanguage, StackSegment, ThreadId, VariableKind,
    VariableState,
};

use crate::frames::{awaiting_recorded, observed};
use crate::session::{CORES, MATRIX, NodeSession};
use crate::support::Scenario;
use crate::support::truth::Report;
use crate::truth::{Frame, Thread, check_threads, threads};
use crate::values::presented;

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

/// In each core dump, taken where `awaits.js` aborted at its first
/// checkpoint, every thread's await chain is the one V8 reported just
/// before, after the function the thread resumed.
#[tokio::test]
async fn a_cores_await_chains_are_the_ones_v8_reported() {
    let (checkpoint, resumed, ends_early) = CHECKPOINTS[0];
    let mut lies = Vec::new();
    for entry in CORES {
        let name = format!("node-awaits-{entry}.core");
        let core = Scenario::fixture(&name);
        let log = std::fs::read_to_string(core.with_extension("core.log"))
            .expect("the core's log; run `just build-test-programs`");
        let reported = threads(
            Report::parse(&log)
                .checkpoint(checkpoint)
                .expect("the checkpoint the core was taken at"),
        );
        let scenario = Scenario::open_core(&name, &CoreDumpOptions::new(core));
        lies.extend(check(&scenario, &reported, resumed, ends_early, &name).await);
        scenario.shutdown().await;
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

/// On every build, the variables of `middle`, suspended in await, are the
/// ones its generator keeps: its argument, `captured` from its context, by
/// name and by expression, `this`, which its strict call left undefined,
/// and the string `plain` in the register its frame on the stack holds it
/// in once the await resumed it.
#[tokio::test]
async fn a_suspended_functions_variables_are_its_generators() {
    let mut lies = Vec::new();
    for entry in MATRIX {
        let mut session = NodeSession::launch(entry, "awaits.js", &[], &[]).await;
        let reported = threads(&session.stop_at("chain").await);
        let context = session.context();
        let suspended =
            middle_variables(&session, reported[0].native, StackSegment::Awaiting).await;
        for (name, kind, text) in [
            ("this", VariableKind::Parameter, "undefined"),
            ("arguments[0]", VariableKind::Parameter, "20"),
            ("captured", VariableKind::Local, "40"),
        ] {
            if !suspended
                .iter()
                .any(|(n, k, t)| n == name && *k == kind && t == text)
            {
                lies.push(format!(
                    "{context}: no {name} {kind:?} {text}: {suspended:#?}"
                ));
            }
        }
        let (summary, _) = presented(&session, "captured").await;
        if summary != "40" {
            lies.push(format!("{context}: captured evaluates to {summary}"));
        }

        session.proceed().await;
        let reported = threads(&session.stop_at("resumed").await);
        let running = middle_variables(&session, reported[0].native, StackSegment::Thread).await;
        let plain = |variables: &[(String, VariableKind, String)]| -> Option<String> {
            variables
                .iter()
                .find(|(_, kind, text)| *kind == VariableKind::Unnamed && text == "'plain-10'")
                .map(|(name, ..)| name.clone())
        };
        if plain(&suspended).is_none() || plain(&suspended) != plain(&running) {
            lies.push(format!(
                "{context}: plain is in {:?} suspended, {:?} on the stack",
                plain(&suspended),
                plain(&running)
            ));
        }
        session.shutdown().await;
    }
    assert!(lies.is_empty(), "{lies:#?}");
}

/// The name, kind, and summary of each variable of the frame of `middle`
/// on the stack `segment` of the thread `native`, which it selects.
async fn middle_variables(
    session: &NodeSession,
    native: u64,
    segment: StackSegment,
) -> Vec<(String, VariableKind, String)> {
    let handle = session.scenario.handle();
    session
        .scenario
        .operation("select", handle.select_context(ThreadId::new(native)))
        .await;
    let trace = session
        .scenario
        .operation("backtrace", handle.backtrace())
        .await;
    let middle = trace
        .frames
        .iter()
        .find(|frame| {
            frame.segment == segment
                && frame.recorded.as_ref().is_some_and(|recorded| {
                    recorded.language == SourceLanguage::JavaScript
                        && &*recorded.function == "middle"
                })
        })
        .unwrap_or_else(|| panic!("{}: no frame of middle on {segment:?}", session.context()));
    session
        .scenario
        .operation("select frame", handle.select_frame(middle.id))
        .await;
    let variables = session
        .scenario
        .operation("variables", handle.variables())
        .await;
    variables
        .variables
        .iter()
        .map(|variable| {
            let text = match &variable.state {
                VariableState::Available {
                    presentation: Some(presentation),
                    ..
                } => presentation.summary.to_string(),
                state => format!("<{state:?}>"),
            };
            (variable.name.to_string(), variable.kind, text)
        })
        .collect()
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
