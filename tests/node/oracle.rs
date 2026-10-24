//! V8's own answer to where JavaScript run control stops: the fixture run
//! under Node's inspector by `cdp_oracle.js`, a plain CDP client that
//! shares no code with microscope, and the same place read from a stop.

use std::process::{Command as Process, Stdio};

use microscope::{SourceLanguage, StepKind};

use crate::support::{Scenario, node};

/// A breakpoint both sides set: a line, and a condition V8 evaluates in
/// the oracle and microscope at its own stops.
#[derive(Debug, Clone, Copy)]
pub struct Planted<'a> {
    pub line: u64,
    pub condition: Option<&'a str>,
}

/// What to do at a pause.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    Into,
    Over,
    Out,
}

impl Command {
    const fn method(self) -> &'static str {
        match self {
            Self::Into => "Debugger.stepInto",
            Self::Over => "Debugger.stepOver",
            Self::Out => "Debugger.stepOut",
        }
    }

    pub const fn kind(self) -> StepKind {
        match self {
            Self::Into => StepKind::IntoSource,
            Self::Over => StepKind::OverSource,
            Self::Out => StepKind::Out,
        }
    }
}

/// Where a pause was: the innermost JavaScript function, and its one-based
/// line and column.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pause {
    pub function: String,
    pub line: u32,
    pub column: u32,
}

impl std::fmt::Display for Pause {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{} {}:{}", self.function, self.line, self.column)
    }
}

/// How steps treat code that is not the program's own.
#[derive(Debug, Clone, Copy, Default)]
pub struct Skipping<'a> {
    /// Directories whose scripts steps pass through, as `/vendor/`.
    pub directories: &'a [&'a str],

    /// Whether steps stop in Node's own scripts.
    pub enter: bool,
}

/// Where V8 pauses running `fixture` under the build `entry` with
/// `breakpoints`, doing `commands` at the first pauses and resuming at the
/// rest, stepping on through code `skipping` names, and pausing at
/// `exceptions` (`none`, `caught`, or `uncaught`).
pub fn pauses(
    entry: &str,
    fixture: &str,
    breakpoints: &[Planted<'_>],
    (commands, skipping): (&[Command], Skipping<'_>),
    exceptions: &str,
) -> Vec<Pause> {
    let plan = serde_json::json!({
        "breakpoints": breakpoints
            .iter()
            .map(|planted| serde_json::json!({
                "line": planted.line,
                "condition": planted.condition.unwrap_or_default(),
            }))
            .collect::<Vec<_>>(),
        "commands": commands.iter().map(|command| command.method()).collect::<Vec<_>>(),
        "exceptions": exceptions,
        "skip": skipping.directories,
        "enter": skipping.enter,
    });
    let output = Process::new(node::node(entry))
        .arg(node::fixture("cdp_oracle.js"))
        .arg(plan.to_string())
        .arg(node::fixture(fixture))
        .env_clear()
        .stdin(Stdio::null())
        .stderr(Stdio::inherit())
        .output()
        .expect("run the CDP oracle");
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "node {entry} cdp_oracle.js {fixture}: {text}"
    );
    let found: Vec<_> = text
        .lines()
        .filter_map(|line| line.strip_prefix("PAUSE "))
        .map(|line| {
            let fields: Vec<_> = line.split(' ').collect();
            let [_reason, function, line, column] = fields[..] else {
                panic!("a malformed pause: {line}");
            };
            Pause {
                function: function.to_owned(),
                line: line.parse().expect("a line"),
                column: column.parse().expect("a column"),
            }
        })
        .collect();
    assert!(!found.is_empty(), "node {entry}: V8 never paused: {text}");
    found
}

/// Where the stop `scenario` is at selects: its JavaScript frame.
pub async fn selected(scenario: &Scenario) -> Pause {
    let snapshot = scenario
        .operation("snapshot", scenario.handle().snapshot())
        .await;
    let selected = snapshot.selected_frame.expect("a selected frame");
    let trace = scenario
        .operation("backtrace", scenario.handle().backtrace())
        .await;
    let frame = trace
        .frames
        .iter()
        .find(|frame| frame.id == selected)
        .expect("the selected frame is in the backtrace");
    let recorded = frame
        .recorded
        .as_ref()
        .filter(|recorded| recorded.language == SourceLanguage::JavaScript)
        .unwrap_or_else(|| panic!("the selected frame is not JavaScript: {frame:?}"));
    let place = recorded.place.as_ref().expect("the frame's place");
    Pause {
        function: if recorded.function.is_empty() {
            "-".to_owned()
        } else {
            recorded.function.to_string()
        },
        line: place.line,
        column: place.column,
    }
}
