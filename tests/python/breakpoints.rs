//! Python breakpoints through the agent: lines, functions, conditions, hit
//! counts, log messages and pending files, each with the ways it can fail.
//!
//! One run of the fixture carries every breakpoint at once, beside ones that
//! must never match, so each stop also proves the agent told them apart. The
//! fixture prints what it computed, so stopping must change nothing.

use microscope::{
    BreakpointId, BreakpointOptions, BreakpointSpec, CodeRole, Condition, ConditionOwner,
    DebuggerEvent, ExitStatus, HitComparison, HitCondition, LineNumber, LogMessage, LogPart,
    StackFrame, StopReason, VariableState, VariableValue,
};
use tokio::sync::broadcast;

use crate::session::{PythonSession, as_integer};
use crate::support::python;

/// The line of `fixture` that carries `marker`.
fn line_of(fixture: &str, marker: &str) -> LineNumber {
    let text = std::fs::read_to_string(python::fixture(fixture)).expect("read the fixture");
    let index = text
        .lines()
        .position(|line| line.contains(marker))
        .expect("a marked line");
    LineNumber::new(index as u64 + 1).expect("a one-based line")
}

fn pending() -> BreakpointOptions {
    BreakpointOptions {
        pending: true,
        ..BreakpointOptions::default()
    }
}

/// The breakpoints of one run, in the order they are added.
#[derive(Debug, Clone, Copy)]
enum Planted {
    /// `index == 3` on `LOOP_LINE`.
    Condition,
    /// A message on `LOG_LINE` with a name the frame does not have.
    Log,
    /// Every second hit of `TICK_LINE`.
    EverySecond,
    /// `missing > 1` on `AREA_LINE`, which cannot be evaluated.
    FailingCondition,
    /// `py:Shape.area`.
    Method,
    /// `py:area`, the last part of two qualified names.
    LastPart,
    /// `py:rea`, which only ends a name.
    PartOfName,
    /// The later file by its absolute path.
    LaterAbsolute,
    /// The later file by its name.
    LaterName,
    /// The later file by its directory and name.
    LaterDirectory,
    /// `later.py`, which only ends the later file's name.
    LaterPartOfName,
    /// `s_later.py`, likewise.
    LaterOtherName,
    /// The later file's name in a directory it is not in.
    LaterElsewhere,
}

const PLANTED: [Planted; 13] = [
    Planted::Condition,
    Planted::Log,
    Planted::EverySecond,
    Planted::FailingCondition,
    Planted::Method,
    Planted::LastPart,
    Planted::PartOfName,
    Planted::LaterAbsolute,
    Planted::LaterName,
    Planted::LaterDirectory,
    Planted::LaterPartOfName,
    Planted::LaterOtherName,
    Planted::LaterElsewhere,
];

impl Planted {
    fn request(self) -> (BreakpointSpec, BreakpointOptions) {
        let main = |marker| BreakpointSpec::Source {
            path: python::fixture("breakpoints.py"),
            line: line_of("breakpoints.py", marker),
        };
        let later = |path: std::path::PathBuf| BreakpointSpec::Source {
            path,
            line: line_of("breakpoints_later.py", "LATER_LINE"),
        };
        let function = |name: &str| BreakpointSpec::Function(format!("py:{name}"));
        let condition = |text| BreakpointOptions {
            condition: Some(Condition::parse(text).expect("a condition")),
            ..pending()
        };
        match self {
            Self::Condition => (main("LOOP_LINE"), condition("index == 3")),
            Self::Log => (
                main("LOG_LINE"),
                BreakpointOptions {
                    log_message: Some(
                        LogMessage::parse("index={index} total={total} lost={missing}")
                            .expect("a message"),
                    ),
                    ..pending()
                },
            ),
            Self::EverySecond => (
                main("TICK_LINE"),
                BreakpointOptions {
                    hit_condition: Some(
                        HitCondition::new(HitComparison::Multiple, 2).expect("a count"),
                    ),
                    ..pending()
                },
            ),
            Self::FailingCondition => (main("AREA_LINE"), condition("missing > 1")),
            Self::Method => (function("Shape.area"), pending()),
            Self::LastPart => (function("area"), pending()),
            Self::PartOfName => (function("rea"), pending()),
            Self::LaterAbsolute => (later(python::fixture("breakpoints_later.py")), pending()),
            Self::LaterName => (later("breakpoints_later.py".into()), pending()),
            Self::LaterDirectory => (later("python/breakpoints_later.py".into()), pending()),
            Self::LaterPartOfName => (later("later.py".into()), pending()),
            Self::LaterOtherName => (later("s_later.py".into()), pending()),
            Self::LaterElsewhere => (later("/elsewhere/breakpoints_later.py".into()), pending()),
        }
    }
}

/// A run stopped at a breakpoint, with the ids of what it planted.
struct Run {
    session: PythonSession,
    ids: Vec<BreakpointId>,
    events: broadcast::Receiver<DebuggerEvent>,
    logged: Vec<std::sync::Arc<[LogPart]>>,
    failed: Vec<(ConditionOwner, String)>,
}

impl Run {
    async fn launch(build: &str) -> (Self, StopReason) {
        let requests = PLANTED.iter().map(|planted| planted.request()).collect();
        let (mut session, reason, events) =
            PythonSession::launch_to_stop(build, "breakpoints.py", &[], requests).await;
        let snapshot = session.scenario.snapshot().await;
        let mut ids: Vec<_> = snapshot
            .breakpoints
            .iter()
            .map(|breakpoint| breakpoint.id)
            .collect();
        ids.sort_unstable();
        assert_eq!(ids.len(), PLANTED.len(), "{}", session.context());
        let mut run = Self {
            session,
            ids,
            events,
            logged: Vec::new(),
            failed: Vec::new(),
        };
        run.drain();
        (run, reason)
    }

    fn id(&self, planted: Planted) -> BreakpointId {
        self.ids[planted as usize]
    }

    /// Keeps the log messages and failed conditions reported so far.
    fn drain(&mut self) {
        while let Ok(event) = self.events.try_recv() {
            match event {
                DebuggerEvent::LogMessage {
                    breakpoint, parts, ..
                } => {
                    assert_eq!(
                        breakpoint,
                        self.id(Planted::Log),
                        "{}",
                        self.session.context()
                    );
                    self.logged.push(parts);
                }
                DebuggerEvent::ConditionFailed { owner, error, .. } => {
                    self.failed.push((owner, error.to_string()));
                }
                _ => {}
            }
        }
    }

    /// Checks `reason` is a stop at exactly these hits, each `(planted, hit
    /// count)`.
    fn assert_hits(&self, reason: &StopReason, expected: &[(Planted, u64)]) {
        let context = self.session.context();
        let StopReason::Breakpoint {
            address: None,
            hits,
        } = reason
        else {
            panic!(
                "{context}: not a Python breakpoint stop: {reason:?}: {}",
                self.session.output()
            );
        };
        let mut actual: Vec<_> = hits
            .iter()
            .map(|hit| (hit.breakpoint, hit.hit_count))
            .collect();
        let mut expected: Vec<_> = expected
            .iter()
            .map(|&(planted, count)| (self.id(planted), count))
            .collect();
        actual.sort_unstable();
        expected.sort_unstable();
        assert_eq!(actual, expected, "{context}: {reason:?}");
    }

    async fn resume(&mut self) -> StopReason {
        let reason = self.session.scenario.resume_to_stop().await;
        self.drain();
        reason
    }

    /// The selected frame of the stop and every frame inner to it.
    async fn selected(&mut self) -> (StackFrame, Vec<StackFrame>) {
        let scenario = &mut self.session.scenario;
        let snapshot = scenario.snapshot().await;
        let id = snapshot.selected_frame.expect("a selected frame");
        let trace = scenario
            .operation("backtrace", scenario.handle().backtrace())
            .await;
        let index = trace
            .frames
            .iter()
            .position(|frame| frame.id == id)
            .expect("the selected frame is in the backtrace");
        (trace.frames[index].clone(), trace.frames[..index].to_vec())
    }

    /// Checks the stop selects the program's frame running `function`, at
    /// `line` when given, with only the debugger's agent and the
    /// interpreter's machinery inner to it.
    async fn assert_frame(&mut self, function: &str, line: Option<LineNumber>) {
        let context = self.session.context();
        let (frame, inner) = self.selected().await;
        let recorded = frame
            .recorded
            .as_ref()
            .unwrap_or_else(|| panic!("{context}: the selected frame is not Python's: {frame:?}"));
        assert_eq!(&*recorded.function, function, "{context}: {frame:?}");
        if let Some(line) = line {
            assert_eq!(
                recorded.place.as_ref().map(|place| u64::from(place.line)),
                Some(line.get()),
                "{context}: {frame:?}"
            );
        }
        assert_eq!(frame.role, CodeRole::Ordinary, "{context}");
        assert!(
            inner.iter().any(|hidden| hidden
                .recorded
                .as_ref()
                .and_then(|recorded| recorded.place.as_ref())
                .is_some_and(|place| crate::agent::agent_source::is_agent_filename(&place.path))),
            "{context}: the agent's frames that reported the hit are in the stack: {inner:?}"
        );
        for hidden in inner {
            assert_eq!(
                hidden.role,
                CodeRole::Interpreter,
                "{context}: machinery inner to the program's frame is hidden: {hidden:?}"
            );
        }
    }

    /// The integer an expression has in the selected frame.
    async fn integer(&self, text: &str) -> i128 {
        self.session.integer(text).await
    }

    async fn disable(&self, planted: Planted) {
        let scenario = &self.session.scenario;
        scenario
            .operation(
                "disable",
                scenario
                    .handle()
                    .set_breakpoint_enabled(self.id(planted), false),
            )
            .await;
    }
}

/// The integers a log message reported, in order.
fn logged_integers(parts: &[LogPart]) -> Vec<i128> {
    parts
        .iter()
        .filter_map(|part| match part {
            LogPart::Value {
                state:
                    VariableState::Available {
                        value: VariableValue::Scalar(scalar),
                        ..
                    },
                ..
            } => as_integer(scalar),
            _ => None,
        })
        .collect()
}

/// Every kind of Python breakpoint through one run of the fixture.
///
/// - A condition that holds stops once, in the program's own frame, whose
///   locals read as Python has them; one that cannot be evaluated stops
///   and says why, rather than hiding what the user asked to see.
/// - A log message reports each hit with the frame's values, and a value it
///   cannot read in its place, and never stops.
/// - A hit count counts every hit and stops only those it names, and a
///   disabled breakpoint stops no more.
/// - A function breakpoint matches a qualified name or its last part, as a
///   method or nested function is named; a name that only ends one never
///   stops.
/// - A file imported later is armed as it loads, whether named by its whole
///   path or by the end of one; a name that only ends its name, or a
///   directory it is not in, never stops.
async fn every_kind_of_breakpoint(build: &str) {
    let main = |marker| Some(line_of("breakpoints.py", marker));
    let (mut run, reason) = Run::launch(build).await;
    let context = run.session.context();

    run.assert_hits(&reason, &[(Planted::Condition, 4)]);
    run.assert_frame("accumulate", main("LOOP_LINE")).await;
    assert_eq!(run.integer("index").await, 3, "{context}");
    assert_eq!(run.integer("total").await, 3, "{context}");
    assert_eq!(run.integer("limit").await, 5, "{context}");

    let reason = run.resume().await;
    run.assert_hits(&reason, &[(Planted::EverySecond, 2)]);
    run.assert_frame("count", main("TICK_LINE")).await;
    assert_eq!(run.integer("tick").await, 1, "{context}");
    let reason = run.resume().await;
    run.assert_hits(&reason, &[(Planted::EverySecond, 4)]);
    assert_eq!(run.integer("tick").await, 3, "{context}");
    run.disable(Planted::EverySecond).await;

    let reason = run.resume().await;
    run.assert_hits(&reason, &[(Planted::Method, 1), (Planted::LastPart, 1)]);
    run.assert_frame("Shape.area", None).await;
    assert_eq!(run.integer("scale").await, 3, "{context}");

    let reason = run.resume().await;
    run.assert_hits(&reason, &[(Planted::FailingCondition, 1)]);
    run.assert_frame("Shape.area", main("AREA_LINE")).await;
    let failing = ConditionOwner::Breakpoint(run.id(Planted::FailingCondition));
    assert!(
        matches!(&run.failed[..], [(breakpoint, error)] if *breakpoint == failing && error.contains("missing")),
        "{context}: the failed condition is reported once: {:?}",
        run.failed
    );

    let reason = run.resume().await;
    run.assert_hits(&reason, &[(Planted::LastPart, 2)]);
    run.assert_frame("outer.<locals>.area", None).await;
    assert_eq!(run.integer("side").await, 4, "{context}");

    let reason = run.resume().await;
    run.assert_hits(
        &reason,
        &[
            (Planted::LaterAbsolute, 1),
            (Planted::LaterName, 1),
            (Planted::LaterDirectory, 1),
        ],
    );
    run.assert_frame("later", Some(line_of("breakpoints_later.py", "LATER_LINE")))
        .await;
    assert_eq!(run.integer("value").await, 7, "{context}");

    // A forked child is released with its traps removed and its agent
    // disarmed: it runs the breakpoint's code to its end, unmonitored,
    // while the parent stops there again.
    let reason = run.resume().await;
    run.assert_hits(&reason, &[(Planted::LastPart, 3)]);
    run.assert_frame("outer.<locals>.area", None).await;
    let output = run.session.output();
    for line in ["CHILD TOOLS [] NESTED 16", "CHILD EXIT 0"] {
        assert!(output.contains(line), "{context}: no {line:?} in {output}");
    }

    let reason = run.resume().await;
    let output = run.session.output();
    assert_eq!(
        reason,
        StopReason::Exited(ExitStatus::Code(0)),
        "{context}: {output}"
    );
    for line in [
        "ACCUMULATED 10",
        "COUNTED 6",
        "AREA 6",
        "NESTED 16",
        "LATER 15",
        "PARENT NESTED 16",
        "BREAKPOINTS DONE",
    ] {
        assert!(output.contains(line), "{context}: no {line:?} in {output}");
    }

    assert_eq!(run.logged.len(), 5, "{context}: {:?}", run.logged);
    for (index, parts) in (0..).zip(&run.logged) {
        let total = (0..=index).sum::<i128>();
        assert_eq!(
            logged_integers(parts),
            [index, total],
            "{context}: {parts:?}"
        );
        assert!(
            parts
                .iter()
                .any(|part| matches!(part, LogPart::Error { .. })),
            "{context}: the unreadable value is reported: {parts:?}"
        );
    }
    assert_eq!(run.failed.len(), 1, "{context}: {:?}", run.failed);
    run.session.shutdown().await;
}

// One test for each build PEP 768 injects the agent into, and for each the
// launch bootstrap gives it to. A process holds one debugger session, and
// the test runner runs each test in its own process, so the builds run at
// once.

#[tokio::test]
async fn every_kind_of_breakpoint_on_3_12() {
    every_kind_of_breakpoint("3.12").await;
}

#[tokio::test]
async fn every_kind_of_breakpoint_on_3_13() {
    every_kind_of_breakpoint("3.13").await;
}

#[tokio::test]
async fn every_kind_of_breakpoint_on_3_14() {
    every_kind_of_breakpoint("3.14").await;
}

#[tokio::test]
async fn every_kind_of_breakpoint_on_3_14t() {
    every_kind_of_breakpoint("3.14t").await;
}

#[tokio::test]
async fn every_kind_of_breakpoint_on_3_15() {
    every_kind_of_breakpoint("3.15").await;
}

#[tokio::test]
async fn every_kind_of_breakpoint_on_3_15t() {
    every_kind_of_breakpoint("3.15t").await;
}

#[tokio::test]
async fn every_kind_of_breakpoint_on_standalone_3_14() {
    every_kind_of_breakpoint("standalone-3.14").await;
}
