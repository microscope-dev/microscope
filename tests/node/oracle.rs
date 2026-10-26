//! Compares microscope stops with V8's `Debugger.paused` reports from
//! `cdp_oracle.js`, an independent CDP client. Checks every JavaScript frame's
//! function, plus source position, parameter and local names, and primitive
//! values in the program's frames. Node frames are compared by function only:
//! the oracle's `--inspect-brk` takes a different module-loader branch.

use std::collections::BTreeMap;
use std::process::{Command as Process, Stdio};

use microscope::{SourceLanguage, StepKind, VariableKind, VariableState};

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

    /// For a pause at an exception, whether nothing catches it.
    pub uncaught: Option<bool>,

    /// Every JavaScript frame, innermost first.
    pub stack: Vec<Frame>,
}

/// A JavaScript frame of a pause.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
pub struct Frame {
    pub function: String,
    pub line: u32,
    pub column: u32,

    /// Whether the frame runs the program's own code, not Node's.
    pub own: bool,

    /// In the program's own frames, each parameter and local by name,
    /// with a primitive's value as util.inspect writes it; `None` for an
    /// object, whose text is not compared.
    pub locals: BTreeMap<String, Option<String>>,

    /// The parameters and locals microscope could not read, with why.
    #[serde(default)]
    pub missing: BTreeMap<String, String>,
}

/// Why microscope shows a variable V8 does not list: a `let` or `const`
/// not yet initialized, which V8's scope iterator skips, whether V8 marks
/// its binding so or its slot still holds the hole.
const UNINITIALIZED: [&str; 2] = [
    "the variable is not initialized yet",
    "the variable is unbound: not yet assigned, or deleted",
];

/// Why microscope cannot read an arrow function's `this`, which V8's
/// debugger reports as undefined.
const UNKEPT_THIS: &str = "V8 did not keep `this` because the arrow function does not use it";

impl std::fmt::Display for Pause {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{} {}:{}", self.function, self.line, self.column)?;
        match self.uncaught {
            Some(true) => write!(formatter, " uncaught"),
            Some(false) => write!(formatter, " caught"),
            None => Ok(()),
        }
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

/// Runs `fixture` under `entry` with the given breakpoints and exception filter
/// (`none`, `caught`, or `uncaught`). Applies `commands` at successive pauses,
/// then resumes; steps continue through scripts selected by `skipping`.
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
    let mut lines = text.lines();
    let mut found = Vec::new();
    while let Some(line) = lines.next() {
        let Some(line) = line.strip_prefix("PAUSE ") else {
            continue;
        };
        let fields: Vec<_> = line.split(' ').collect();
        let [_reason, function, line, column, ..] = fields[..] else {
            panic!("a malformed pause: {line}");
        };
        let stack = lines
            .next()
            .and_then(|line| line.strip_prefix("STACK "))
            .unwrap_or_else(|| panic!("a pause with no stack: {text}"));
        found.push(Pause {
            function: function.to_owned(),
            line: line.parse().expect("a line"),
            column: column.parse().expect("a column"),
            uncaught: fields.get(4).map(|caught| *caught == "uncaught"),
            stack: serde_json::from_str(stack).expect("a stack"),
        });
    }
    assert!(!found.is_empty(), "node {entry}: V8 never paused: {text}");
    found
}

/// Reads the selected JavaScript frame and its JavaScript callers.
pub async fn selected(scenario: &Scenario) -> Pause {
    let snapshot = scenario
        .operation("snapshot", scenario.handle().snapshot())
        .await;
    let selected = snapshot.selected_frame.expect("a selected frame");
    let trace = scenario
        .operation("backtrace", scenario.handle().backtrace())
        .await;
    let at = trace
        .frames
        .iter()
        .position(|frame| frame.id == selected)
        .expect("the selected frame is in the backtrace");
    let frame = &trace.frames[at];
    let recorded = frame
        .recorded
        .as_ref()
        .filter(|recorded| recorded.language == SourceLanguage::JavaScript)
        .unwrap_or_else(|| panic!("the selected frame is not JavaScript: {frame:?}"));
    let place = recorded
        .place
        .as_ref()
        .unwrap_or_else(|| panic!("the selected frame has no place: {frame:?}"));

    let mut stack = Vec::new();
    for frame in &trace.frames[at..] {
        let Some(recorded) = frame.recorded.as_ref().filter(|recorded| {
            matches!(
                recorded.language,
                SourceLanguage::JavaScript | SourceLanguage::TypeScript
            )
        }) else {
            continue;
        };
        // V8 reports the script's own place, which a source map moves. A
        // builtin such as Array.prototype.map has no script, and CDP no
        // frame for it.
        let Some(place) = recorded.generated.as_ref().or(recorded.place.as_ref()) else {
            continue;
        };
        let own = !place.path.starts_with("node:");
        let (locals, missing) = if own {
            locals(scenario, frame.id).await
        } else {
            Default::default()
        };
        stack.push(Frame {
            function: name(&recorded.function),
            line: place.line,
            column: place.column,
            own,
            locals,
            missing,
        });
    }
    scenario
        .operation("select", scenario.handle().select_frame(selected))
        .await;

    Pause {
        function: name(&recorded.function),
        line: place.line,
        column: place.column,
        uncaught: None,
        stack,
    }
}

fn name(function: &str) -> String {
    if function.is_empty() {
        "-".to_owned()
    } else {
        function.to_owned()
    }
}

/// Reads formatted parameters and locals, retaining unreadable-value reasons.
async fn locals(
    scenario: &Scenario,
    frame: microscope::StackFrameId,
) -> (BTreeMap<String, Option<String>>, BTreeMap<String, String>) {
    scenario
        .operation("select", scenario.handle().select_frame(frame))
        .await;
    let variables = scenario
        .operation("variables", scenario.handle().variables())
        .await;
    let (mut read, mut missing) = (BTreeMap::new(), BTreeMap::new());
    for variable in variables
        .variables
        .iter()
        .filter(|variable| matches!(variable.kind, VariableKind::Parameter | VariableKind::Local))
    {
        let name = variable.name.to_string();
        match &variable.state {
            VariableState::Available { presentation, .. } => {
                let text = presentation
                    .as_ref()
                    .map(|presentation| presentation.summary.to_string());
                read.insert(name, text);
            }
            VariableState::Unavailable(reason) => {
                missing.insert(name, reason.to_string());
            }
            other => {
                missing.insert(name, format!("{other:?}"));
            }
        }
    }
    (read, missing)
}

/// How microscope's pauses `seen` differ from V8's `expected`, pause by
/// pause and frame by frame, or nothing when they agree.
pub fn differences(seen: &[Pause], expected: &[Pause]) -> Vec<String> {
    let mut found = Vec::new();
    if seen.len() != expected.len() {
        found.push(format!(
            "{} pauses, where V8 made {}",
            seen.len(),
            expected.len()
        ));
    }
    for (index, (seen, expected)) in seen.iter().zip(expected).enumerate() {
        if (&seen.function, seen.line, seen.column, seen.uncaught)
            != (
                &expected.function,
                expected.line,
                expected.column,
                expected.uncaught,
            )
        {
            found.push(format!(
                "pause {index}: at {seen}, where V8 is at {expected}"
            ));
        }
        if seen.stack.len() != expected.stack.len() {
            found.push(format!(
                "pause {index}: {} frames, where V8 has {}",
                seen.stack.len(),
                expected.stack.len()
            ));
        }
        for (level, (mine, theirs)) in seen.stack.iter().zip(&expected.stack).enumerate() {
            let place = |frame: &Frame| {
                if frame.own {
                    format!("{} {}:{}", frame.function, frame.line, frame.column)
                } else {
                    format!("{} in Node", frame.function)
                }
            };
            if place(mine) != place(theirs) {
                found.push(format!(
                    "pause {index} frame {level}: {}, where V8 has {}",
                    place(mine),
                    place(theirs)
                ));
            }
            for problem in local_differences(mine, theirs) {
                found.push(format!(
                    "pause {index} frame {level} {}: {problem}",
                    theirs.function
                ));
            }
        }
    }
    found
}

/// How the variables microscope read in a frame differ from V8's. Every
/// name V8 lists must be read, with a primitive's value V8's own. Beyond
/// V8's, microscope may show only extra arguments and uninitialized
/// bindings, and may leave unread only an arrow's unkept `this`.
fn local_differences(mine: &Frame, theirs: &Frame) -> Vec<String> {
    let mut found = Vec::new();
    for (name, value) in &theirs.locals {
        match (mine.locals.get(name), mine.missing.get(name)) {
            (Some(text), _) if value.is_some() && text != value => {
                found.push(format!("{name} = {text:?}, where V8 has {value:?}"));
            }
            (Some(_), _) => {}
            (None, Some(reason)) if name == "this" && reason.ends_with(UNKEPT_THIS) => {}
            (None, Some(reason)) => found.push(format!("{name} is unread: {reason}")),
            (None, None) => found.push(format!(
                "{name}, which V8 lists, is absent from {:?} {:?}",
                mine.locals, mine.missing
            )),
        }
    }
    for name in mine.locals.keys().chain(mine.missing.keys()) {
        let extra = name.starts_with("arguments[")
            || mine
                .missing
                .get(name)
                .is_some_and(|reason| UNINITIALIZED.iter().any(|why| reason.ends_with(why)));
        if !theirs.locals.contains_key(name) && !extra {
            found.push(format!(
                "{name}, which V8 does not list, is shown: {:?} {:?}",
                mine.locals.get(name),
                mine.missing.get(name)
            ));
        }
    }
    found
}

/// A named change to a pause, for the differential's self-test.
#[cfg(test)]
type Change<'a> = (&'a str, &'a dyn Fn(&mut Pause));

#[test]
fn the_differential_fails_on_every_kind_of_lie() {
    let frame = |function: &str, line, locals: &[(&str, Option<&str>)]| Frame {
        function: function.to_owned(),
        line,
        column: 3,
        own: true,
        locals: locals
            .iter()
            .map(|(name, value)| ((*name).to_owned(), value.map(str::to_owned)))
            .collect(),
        missing: BTreeMap::new(),
    };
    let pause = |stack: Vec<Frame>| Pause {
        function: stack[0].function.clone(),
        line: stack[0].line,
        column: stack[0].column,
        uncaught: None,
        stack,
    };
    let truth = pause(vec![
        frame("leaf", 10, &[("x", Some("5")), ("this", Some("undefined"))]),
        frame("calls", 15, &[("n", Some("5")), ("o", None)]),
    ]);
    let lie = |change: &dyn Fn(&mut Pause)| {
        let mut seen = truth.clone();
        change(&mut seen);
        differences(std::slice::from_ref(&seen), std::slice::from_ref(&truth))
    };
    assert!(lie(&|_| {}).is_empty());

    let lies: [Change<'_>; 7] = [
        ("dropped frame", &|seen| {
            seen.stack.pop();
        }),
        ("wrong line", &|seen| seen.stack[1].line = 16),
        ("wrong function", &|seen| {
            seen.stack[1].function = "other".into();
        }),
        ("wrong value", &|seen| {
            seen.stack[0].locals.insert("x".into(), Some("6".into()));
        }),
        ("missing local", &|seen| {
            seen.stack[1].locals.remove("o");
        }),
        ("extra local", &|seen| {
            seen.stack[0].locals.insert("y".into(), Some("1".into()));
        }),
        ("unread local", &|seen| {
            seen.stack[0].locals.remove("x");
            seen.stack[0]
                .missing
                .insert("x".into(), "unreadable".into());
        }),
    ];
    for (name, change) in lies {
        assert!(!lie(change).is_empty(), "{name} passed");
    }

    let allowed: [Change<'_>; 4] = [
        ("an object's text", &|seen| {
            seen.stack[1].locals.insert("o".into(), Some("{}".into()));
        }),
        ("an extra argument", &|seen| {
            seen.stack[0]
                .locals
                .insert("arguments[1]".into(), Some("0".into()));
        }),
        ("an uninitialized binding", &|seen| {
            seen.stack[0]
                .missing
                .insert("later".into(), format!("why: {}", UNINITIALIZED[1]));
        }),
        ("an arrow's unkept this", &|seen| {
            seen.stack[0].locals.remove("this");
            seen.stack[0]
                .missing
                .insert("this".into(), format!("why: {UNKEPT_THIS}"));
        }),
    ];
    for (name, change) in allowed {
        assert!(lie(change).is_empty(), "{name} failed: {:?}", lie(change));
    }
}
