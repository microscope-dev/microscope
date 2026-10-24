//! JavaScript breakpoints through Node's inspector: lines, conditions, hit
//! counts, and log messages over the frame's stack locals, which V8 names
//! only at its own pauses (G5), and a file that never loads.
//!
//! One run carries every breakpoint, so each stop also proves the others
//! were told apart, and the program prints what it computed, which
//! stopping must not change.

use std::sync::Arc;

use microscope::{
    BreakpointId, BreakpointOptions, BreakpointSpec, Condition, DebuggerEvent, Evaluation,
    ExitStatus, Expression, HitComparison, HitCondition, LineNumber, LogMessage, LogPart,
    SourceLanguage, StopReason, VariableState,
};
use tokio::sync::broadcast;

use crate::session::{MATRIX, NodeSession};
use crate::support::node;

const FIXTURE: &str = "breakpoints.js";

/// The line of the fixture that carries `marker`.
fn line_of(marker: &str) -> LineNumber {
    let text = std::fs::read_to_string(node::fixture(FIXTURE)).expect("read the fixture");
    let index = text
        .lines()
        .position(|line| line.contains(marker))
        .expect("a marked line");
    LineNumber::new(index as u64 + 1).expect("a one-based line")
}

/// The breakpoints of the run, in the order they are added.
#[derive(Debug, Clone, Copy)]
enum Planted {
    /// A message on `AREA_LINE` naming both parameters.
    Log,
    /// Every second hit of `RETURN_LINE`.
    EverySecond,
    /// `index == 3` on `LOOP_LINE`, a block's variable in the top-level
    /// function.
    Condition,
    /// A line of a file the program never loads.
    Elsewhere,
}

const PLANTED: [Planted; 4] = [
    Planted::Log,
    Planted::EverySecond,
    Planted::Condition,
    Planted::Elsewhere,
];

impl Planted {
    fn request(self) -> (BreakpointSpec, BreakpointOptions) {
        let at = |marker| BreakpointSpec::Source {
            path: node::fixture(FIXTURE),
            line: line_of(marker),
        };
        let pending = BreakpointOptions {
            pending: true,
            ..BreakpointOptions::default()
        };
        match self {
            Self::Log => (
                at("AREA_LINE"),
                BreakpointOptions {
                    log_message: Some(LogMessage::parse("{width}x{height}").expect("a message")),
                    ..pending
                },
            ),
            Self::EverySecond => (
                at("RETURN_LINE"),
                BreakpointOptions {
                    hit_condition: Some(
                        HitCondition::new(HitComparison::Multiple, 2).expect("a count"),
                    ),
                    ..pending
                },
            ),
            Self::Condition => (
                at("LOOP_LINE"),
                BreakpointOptions {
                    condition: Some(Condition::parse("index == 3").expect("a condition")),
                    ..pending
                },
            ),
            Self::Elsewhere => (
                BreakpointSpec::Source {
                    path: "never_loaded.js".into(),
                    line: LineNumber::new(1).expect("a line"),
                },
                pending,
            ),
        }
    }
}

struct Run {
    session: NodeSession,
    context: String,
    ids: Vec<BreakpointId>,
    events: broadcast::Receiver<DebuggerEvent>,
    logged: Vec<String>,
}

impl Run {
    async fn launch(entry: &str) -> (Self, StopReason) {
        let requests = PLANTED.iter().map(|planted| planted.request()).collect();
        let (session, reason, events) = NodeSession::launch_to_stop(entry, FIXTURE, requests).await;
        let snapshot = session
            .scenario
            .operation("snapshot", session.scenario.handle().snapshot())
            .await;
        let mut ids: Vec<_> = snapshot
            .breakpoints
            .iter()
            .map(|breakpoint| breakpoint.id)
            .collect();
        ids.sort_unstable();
        let mut run = Self {
            context: session.context(),
            session,
            ids,
            events,
            logged: Vec::new(),
        };
        run.drain();
        (run, reason)
    }

    fn id(&self, planted: Planted) -> BreakpointId {
        self.ids[planted as usize]
    }

    fn output(&self) -> String {
        self.session.output()
    }

    /// Keeps the log messages reported so far, as text.
    fn drain(&mut self) {
        while let Ok(event) = self.events.try_recv() {
            if let DebuggerEvent::LogMessage {
                breakpoint, parts, ..
            } = event
            {
                assert_eq!(breakpoint, self.id(Planted::Log), "{}", self.context);
                self.logged.push(text(&parts));
            }
        }
    }

    async fn resume(&mut self) -> StopReason {
        let reason = self.session.scenario.resume_to_stop().await;
        self.drain();
        reason
    }

    /// Checks `reason` is a stop at exactly the hit `(planted, count)`, in
    /// the selected JavaScript frame of `function` at `marker`'s line.
    async fn assert_stop(
        &self,
        reason: &StopReason,
        planted: Planted,
        count: u64,
        at: (&str, &str),
    ) {
        let context = &self.context;
        let StopReason::Breakpoint {
            address: None,
            hits,
        } = reason
        else {
            panic!(
                "{context}: not an inspector stop: {reason:?}\n{}",
                self.output()
            );
        };
        let hits: Vec<_> = hits
            .iter()
            .map(|hit| (hit.breakpoint, hit.hit_count))
            .collect();
        assert_eq!(hits, [(self.id(planted), count)], "{context}: {reason:?}");

        let snapshot = self
            .session
            .scenario
            .operation("snapshot", self.session.scenario.handle().snapshot())
            .await;
        let selected = snapshot.selected_frame.expect("a selected frame");
        let trace = self
            .session
            .scenario
            .operation("backtrace", self.session.scenario.handle().backtrace())
            .await;
        let frame = trace
            .frames
            .iter()
            .find(|frame| frame.id == selected)
            .expect("the selected frame is in the backtrace");
        let recorded = frame.recorded.as_ref().unwrap_or_else(|| {
            panic!("{context}: the selected frame is not JavaScript: {frame:?}")
        });
        let (function, marker) = at;
        assert_eq!(&*recorded.function, function, "{context}: {frame:?}");
        assert_eq!(recorded.language, SourceLanguage::JavaScript, "{context}");
        assert_eq!(
            recorded.place.as_ref().map(|place| u64::from(place.line)),
            Some(line_of(marker).get()),
            "{context}: {frame:?}"
        );
    }

    /// The summary of `text`'s value in the selected frame.
    async fn value(&self, text: &str) -> String {
        let expression = Expression::parse(text).expect("an expression");
        let evaluation = self
            .session
            .scenario
            .operation(text, self.session.scenario.handle().evaluate(&expression))
            .await;
        let Evaluation::Value { value, .. } = evaluation else {
            panic!("{}: `{text}` has no value", self.context);
        };
        summary(&value.state)
            .unwrap_or_else(|| panic!("{}: `{text}` is {:?}", self.context, value.state))
    }
}

/// The summary a value presents, if it is available.
fn summary(state: &VariableState) -> Option<String> {
    match state {
        VariableState::Available {
            presentation: Some(presentation),
            ..
        } => Some(presentation.summary.to_string()),
        _ => None,
    }
}

/// A log message as text.
fn text(parts: &Arc<[LogPart]>) -> String {
    parts
        .iter()
        .map(|part| match part {
            LogPart::Text(text) => text.to_string(),
            LogPart::Value { state, .. } => summary(state).unwrap_or_else(|| format!("{state:?}")),
            other @ LogPart::Error { .. } => format!("{other:?}"),
        })
        .collect()
}

/// Every kind of breakpoint through one run, on one build:
///
/// - a log message reports each hit with the parameters' values by name,
///   and never stops;
/// - a hit count counts every hit and stops at every second, where the
///   function's `const` reads by name;
/// - a condition over a block's `let` in the top-level function stops once,
///   where the block's and the function's locals read by name;
/// - a file that never loads stays pending, saying why.
async fn every_kind_of_breakpoint(entry: &str) {
    let (mut run, reason) = Run::launch(entry).await;
    let context = run.context.clone();

    let snapshot = run.session.scenario.snapshot().await;
    let ready = |planted: Planted| {
        let breakpoint = snapshot
            .breakpoints
            .iter()
            .find(|breakpoint| breakpoint.id == run.id(planted))
            .expect("a planted breakpoint");
        (
            breakpoint.locations.len(),
            breakpoint.pending_locations.len(),
        )
    };
    assert_eq!(ready(Planted::EverySecond), (1, 0), "{context}");
    let elsewhere = snapshot
        .breakpoints
        .iter()
        .find(|breakpoint| breakpoint.id == run.id(Planted::Elsewhere))
        .expect("a planted breakpoint");
    assert!(
        elsewhere
            .pending_locations
            .iter()
            .any(|pending| pending.reason.contains("no script")),
        "{context}: {elsewhere:?}"
    );

    run.assert_stop(&reason, Planted::EverySecond, 2, ("area", "RETURN_LINE"))
        .await;
    for (name, value) in [("width", "1"), ("height", "2"), ("product", "2")] {
        assert_eq!(run.value(name).await, value, "{context}: {name}");
    }

    let reason = run.resume().await;
    run.assert_stop(&reason, Planted::EverySecond, 4, ("area", "RETURN_LINE"))
        .await;
    assert_eq!(run.value("product").await, "6", "{context}");

    let reason = run.resume().await;
    run.assert_stop(&reason, Planted::Condition, 4, ("", "LOOP_LINE"))
        .await;
    for (name, value) in [
        ("index", "3"),
        ("step", "6"),
        ("total", "6"),
        ("index * step", "18"),
    ] {
        assert_eq!(run.value(name).await, value, "{context}: {name}");
    }

    let reason = run.resume().await;
    run.assert_stop(&reason, Planted::EverySecond, 6, ("area", "RETURN_LINE"))
        .await;
    assert_eq!(run.value("width * height").await, "10", "{context}");

    let reason = run.resume().await;
    let output = run.output();
    assert_eq!(
        reason,
        StopReason::Exited(ExitStatus::Code(0)),
        "{context}: {output}"
    );
    assert!(output.contains("total 30\n"), "{context}: {output}");
    let expected: Vec<_> = (0..6).map(|index| format!("{index}x2")).collect();
    assert_eq!(run.logged, expected, "{context}");

    let snapshot = run.session.scenario.snapshot().await;
    for (planted, count) in [
        (Planted::Log, 6),
        (Planted::EverySecond, 6),
        (Planted::Condition, 6),
        (Planted::Elsewhere, 0),
    ] {
        let breakpoint = snapshot
            .breakpoints
            .iter()
            .find(|breakpoint| breakpoint.id == run.id(planted))
            .expect("a planted breakpoint");
        assert_eq!(breakpoint.hit_count, count, "{context}: {planted:?}");
    }
    run.session.shutdown().await;
}

#[tokio::test]
async fn every_kind_of_breakpoint_stops_logs_and_waits_as_asked() {
    for entry in MATRIX {
        every_kind_of_breakpoint(entry).await;
    }
}
