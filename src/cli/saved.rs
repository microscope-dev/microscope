//! Breakpoints an interactive session keeps for the next one, in the
//! project's `.microscope/state/breakpoints.toml`.

use std::io::{self, Write as _};
use std::path::{Path, PathBuf};

use microscope::{Breakpoint, BreakpointSpec};
use serde::{Deserialize, Serialize};

/// The version of the file's layout this build reads and writes.
const VERSION: u32 = 1;

/// One breakpoint as `break` would recreate it.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct Saved {
    /// The location as the user wrote it, with a path inside the project
    /// relative to its root.
    pub location: String,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub condition: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hits: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub log: Option<String>,

    #[serde(default = "enabled", skip_serializing_if = "is_enabled")]
    pub enabled: bool,

    /// A source breakpoint's line as it read when saved.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line_text: Option<String>,
}

const fn enabled() -> bool {
    true
}

#[expect(
    clippy::trivially_copy_pass_by_ref,
    reason = "serde passes a reference"
)]
const fn is_enabled(enabled: &bool) -> bool {
    *enabled
}

/// One display as `display` would recreate it.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct SavedDisplay {
    pub expression: String,

    /// The format letters, as `display/` takes them.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub format: String,
}

/// What a project keeps for its next session.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Contents {
    pub breakpoints: Vec<Saved>,
    pub displays: Vec<SavedDisplay>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct File {
    version: u32,

    #[serde(default, rename = "breakpoint", skip_serializing_if = "Vec::is_empty")]
    breakpoints: Vec<Saved>,

    #[serde(default, rename = "display", skip_serializing_if = "Vec::is_empty")]
    displays: Vec<SavedDisplay>,
}

/// Where a project keeps its saved state.
pub fn state_directory(root: &Path) -> PathBuf {
    root.join(".microscope/state")
}

/// Where a project keeps its breakpoints and displays.
pub fn path(root: &Path) -> PathBuf {
    state_directory(root).join("breakpoints.toml")
}

/// What is saved at `path`, nothing when there is no file, or why the
/// file cannot be read.
pub fn read(path: &Path) -> Result<Contents, String> {
    let Some(text) = super::config::read_text(path)? else {
        return Ok(Contents::default());
    };

    let file: File = toml::from_str(&text).map_err(|error| error.to_string().trim().to_owned())?;
    if file.version != VERSION {
        return Err(format!(
            "version {} is not one this microscope reads, which is {VERSION}",
            file.version
        ));
    }

    Ok(Contents {
        breakpoints: file.breakpoints,
        displays: file.displays,
    })
}

/// Writes `contents` to `path` through a temporary file renamed into
/// place, creating the state directory with a `.gitignore` that keeps it
/// out of the project's history.
pub fn write(path: &Path, contents: &Contents) -> io::Result<()> {
    let directory = path
        .parent()
        .ok_or_else(|| io::Error::other("a state file has a directory"))?;
    if !directory.is_dir() {
        std::fs::create_dir_all(directory)?;
        std::fs::write(directory.join(".gitignore"), "*\n")?;
    }

    let text = toml::to_string(&File {
        version: VERSION,
        breakpoints: contents.breakpoints.clone(),
        displays: contents.displays.clone(),
    })
    .map_err(io::Error::other)?;
    let temporary = path.with_extension("toml.tmp");
    let mut file = std::fs::File::create(&temporary)?;
    file.write_all(text.as_bytes())?;
    file.sync_all()?;
    std::fs::rename(&temporary, path)
}

/// A breakpoint as it is kept, or `None` for one that is not: an address,
/// which does not survive a rebuild, or a temporary breakpoint, which
/// belongs to one stop.
pub fn saved(breakpoint: &Breakpoint, root: &Path, line_text: Option<String>) -> Option<Saved> {
    if breakpoint.temporary {
        return None;
    }

    Some(Saved {
        location: location(&breakpoint.spec, root)?,
        condition: breakpoint.condition.as_ref().map(ToString::to_string),
        hits: breakpoint.hit_condition.map(|hits| hits.to_string()),
        log: breakpoint.log_message.as_ref().map(ToString::to_string),
        enabled: breakpoint.enabled,
        line_text,
    })
}

/// A location as `break` takes it, with a path inside `root` relative to it.
pub fn location(spec: &BreakpointSpec, root: &Path) -> Option<String> {
    let relative = |path: &Path| {
        path.strip_prefix(root)
            .ok()
            .filter(|_| root.parent().is_some())
            .unwrap_or(path)
            .display()
            .to_string()
    };

    Some(match spec {
        BreakpointSpec::Address(_) => return None,
        BreakpointSpec::Function(name) => name.clone(),
        BreakpointSpec::Source { path, line } => format!("{}:{line}", relative(path)),
        BreakpointSpec::FileFunction { path, function } => {
            format!("{}:{function}", relative(path))
        }
    })
}

/// The `break` command that recreates a saved breakpoint, or why there is
/// none. The location is written in quotes and the condition in
/// parentheses only where the plain line would read back as something else.
pub fn command(saved: &Saved) -> Result<String, &'static str> {
    let texts = [&saved.condition, &saved.hits, &saved.log];
    if saved.location.contains(['\n', '\r'])
        || texts
            .into_iter()
            .flatten()
            .any(|text| text.contains(['\n', '\r']))
    {
        return Err("it has a line break, but a command must be one line");
    }

    let quoted = |text: &str| format!("\"{}\"", text.replace('\\', "\\\\").replace('"', "\\\""));
    let spec = super::commands::command_named("break").ok_or("there is no break command")?;

    for location in [saved.location.clone(), quoted(&saved.location)] {
        let conditions = saved.condition.as_ref().map_or_else(
            || vec![None],
            |condition| vec![Some(condition.clone()), Some(format!("({condition})"))],
        );
        for condition in conditions {
            let mut options = String::new();
            if let Some(hits) = &saved.hits {
                options.push_str(" hits ");
                options.push_str(hits);
            }

            if let Some(condition) = &condition {
                options.push_str(" if ");
                options.push_str(condition);
            }

            if let Some(log) = &saved.log {
                options.push_str(" log ");
                options.push_str(&quoted(log));
            }

            if !saved.enabled {
                options.push_str(" disabled");
            }

            let line = format!("{location}{options}");
            let reads_back = super::commands::parse_break(&line, spec).is_ok_and(|parsed| {
                parsed.location.as_deref() == Some(saved.location.as_str())
                    && parsed.condition == condition.as_deref()
                    && parsed.hits == saved.hits.as_deref()
                    && parsed.log == saved.log
                    && parsed.disabled != saved.enabled
            });
            if reads_back {
                return Ok(format!("break {line}"));
            }
        }
    }

    Err("no command line reads back as it")
}

/// What a session that keeps its breakpoints knows of the file.
#[derive(Debug, Default)]
pub struct Kept {
    pub path: PathBuf,

    /// Whether the file could not be read, so that nothing overwrites it.
    pub blocked: bool,

    /// What the file holds now.
    pub written: Contents,

    /// Breakpoints the file holds that this session could not restore,
    /// which it keeps writing back.
    pub unrestored: Vec<Saved>,

    /// The line text of each source location, as first read or as saved.
    pub line_texts: std::collections::BTreeMap<String, String>,

    /// Where the session finds source files, which line texts are read from.
    pub source_paths: microscope::SourcePathMap,
}

/// The commands that recreate `breakpoints`, one per line, temporary and
/// address ones included, as `-c` reads them, and the breakpoints no
/// command recreates, with why.
pub fn commands(
    breakpoints: &[Breakpoint],
    root: &Path,
) -> (String, Vec<(microscope::BreakpointId, &'static str)>) {
    let mut text = String::new();
    let mut refused = Vec::new();
    for breakpoint in breakpoints {
        let mut saved = saved(
            &Breakpoint {
                temporary: false,
                ..breakpoint.clone()
            },
            root,
            None,
        )
        .unwrap_or_else(|| Saved {
            location: breakpoint.spec.to_string(),
            condition: breakpoint.condition.as_ref().map(ToString::to_string),
            hits: breakpoint.hit_condition.map(|hits| hits.to_string()),
            log: breakpoint.log_message.as_ref().map(ToString::to_string),
            enabled: breakpoint.enabled,
            line_text: None,
        });
        saved.line_text = None;
        let line = match command(&saved) {
            Ok(line) => line,
            Err(reason) => {
                refused.push((breakpoint.id, reason));
                continue;
            }
        };

        if breakpoint.temporary {
            text.push('t');
        }

        text.push_str(&line);
        text.push('\n');
    }

    (text, refused)
}

/// The text of line `line` of the source file recorded at `path`, read
/// where `source_paths` finds it, without its line ending.
pub fn line_text(
    source_paths: &microscope::SourcePathMap,
    path: &Path,
    line: u64,
) -> Option<String> {
    let text = source_paths
        .candidates(path)
        .iter()
        .find_map(|candidate| std::fs::read_to_string(candidate).ok())?;
    let index = usize::try_from(line.checked_sub(1)?).ok()?;
    text.lines().nth(index).map(ToOwned::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    use crate::cli::commands::{command_named, parse_break};

    /// A function name as debug information or a demangler spells it.
    fn function() -> impl Strategy<Value = String> {
        let part = prop::sample::select(vec![
            "f",
            "ns",
            "log",
            "hits",
            "disabled",
            "<impl A for B>",
            "<T as Trait>",
            "{closure#0}",
            "operator()",
            "Foo<'a>",
            "\"q\"",
        ]);
        let arguments = prop::sample::select(vec!["", "(int, char)", "()", "(char const*)"]);
        (prop::collection::vec(part, 1..4), arguments)
            .prop_map(|(parts, arguments)| format!("{}{arguments}", parts.join("::")))
    }

    fn path() -> impl Strategy<Value = String> {
        let part = prop::sample::select(vec!["src", "a b", "é", "if", "q\"x", "it's"]);
        (
            prop::collection::vec(part, 0..3),
            prop::sample::select(vec!["a.c", "m n.rs", "log"]),
        )
            .prop_map(|(mut parts, file)| {
                parts.push(file);
                parts.join("/")
            })
    }

    fn location() -> impl Strategy<Value = String> {
        prop_oneof![
            function(),
            (path(), 1..100_u32).prop_map(|(path, line)| format!("{path}:{line}")),
            (path(), function()).prop_map(|(path, function)| format!("{path}:{function}")),
        ]
    }

    /// A condition as `Condition` prints it: an expression with names that
    /// are option words, character literals of quotes and brackets, and
    /// strings and backticked names holding option words.
    fn condition() -> impl Strategy<Value = String> {
        let leaf = prop::sample::select(vec![
            "a",
            "log",
            "hits",
            "disabled",
            "b.c",
            "'('",
            "'\"'",
            "'\\''",
            "'['",
            "\"s ) log \\\" x\"",
            "`a if b`",
            "3",
        ])
        .prop_map(str::to_owned);
        leaf.prop_recursive(4, 16, 2, |inner| {
            prop_oneof![
                (
                    inner.clone(),
                    prop::sample::select(vec!["+", "&&", "||", "*"]),
                    inner.clone()
                )
                    .prop_map(|(left, op, right)| format!("{left} {op} {right}")),
                (
                    inner.clone(),
                    prop::sample::select(vec!["==", "<", ">"]),
                    inner.clone()
                )
                    .prop_map(|(left, op, right)| format!("({left} {op} {right})")),
                inner.clone().prop_map(|operand| format!("!({operand})")),
                (inner.clone(), inner).prop_map(|(base, index)| format!("({base})[{index}]")),
            ]
        })
    }

    fn log() -> impl Strategy<Value = String> {
        let piece = prop::sample::select(vec![
            "x",
            " ",
            "\"",
            "\\",
            "{a}",
            "{{",
            "}}",
            " log ",
            " disabled",
            "é",
            "\n",
            "\r",
            "\\\"",
        ]);
        prop::collection::vec(piece, 0..8).prop_map(|pieces| pieces.concat())
    }

    fn hits() -> impl Strategy<Value = String> {
        (
            prop::sample::select(vec!["==", "!=", "<", "<=", ">", ">=", "%"]),
            prop_oneof![Just(1_u64), Just(u64::MAX), 1..1000_u64],
        )
            .prop_map(|(operator, count)| format!("{operator}{count}"))
    }

    fn saved() -> impl Strategy<Value = Saved> {
        (
            location(),
            proptest::option::of(condition()),
            proptest::option::of(hits()),
            proptest::option::of(log()),
            any::<bool>(),
        )
            .prop_map(|(location, condition, hits, log, enabled)| Saved {
                location,
                condition,
                hits,
                log,
                enabled,
                line_text: None,
            })
    }

    /// A saved breakpoint's command parses back to the same breakpoint,
    /// with its condition perhaps in parentheses, or saving refuses it
    /// because one of its texts holds a line break, which a file of one
    /// command per line cannot hold.
    fn reads_back(saved: &Saved) -> Result<(), TestCaseError> {
        if let Some(condition) = &saved.condition {
            prop_assert!(
                microscope::Condition::parse(condition).is_ok(),
                "{condition}"
            );
        }

        let breaks_lines = [
            Some(&saved.location),
            saved.condition.as_ref(),
            saved.log.as_ref(),
        ]
        .into_iter()
        .flatten()
        .any(|text| text.contains(['\n', '\r']));
        let line = command(saved);
        prop_assert_eq!(line.is_err(), breaks_lines, "{:?}", line);

        if let Ok(line) = line {
            let spec = command_named("break").expect("break is a command");
            let rest = line.strip_prefix("break ").expect("a break command");
            let parsed = parse_break(rest, spec);
            prop_assert!(parsed.is_ok(), "{line:?}: {parsed:?}");
            let parsed = parsed.expect("checked");

            prop_assert_eq!(
                parsed.location.as_deref(),
                Some(saved.location.as_str()),
                "{}",
                line
            );
            prop_assert_eq!(parsed.hits, saved.hits.as_deref(), "{}", line);
            prop_assert_eq!(parsed.log, saved.log.clone(), "{}", line);
            prop_assert_eq!(parsed.disabled, !saved.enabled, "{}", line);
            prop_assert!(
                parsed.condition.map(str::to_owned) == saved.condition
                    || parsed.condition.map(str::to_owned)
                        == saved
                            .condition
                            .as_ref()
                            .map(|condition| format!("({condition})")),
                "{line}"
            );
        }

        Ok(())
    }

    proptest! {
        #[test]
        fn saved_commands_parse_back(saved in saved()) {
            reads_back(&saved)?;
        }
    }

    /// The shrunk inputs of the property above, and the cases the review
    /// suspected, each of which once saved a line that read back as
    /// something else or not at all.
    #[test]
    fn saved_commands_quote_what_would_not_read_back() {
        let saved = |location: &str, condition: Option<&str>, log: Option<&str>, enabled| Saved {
            location: location.to_owned(),
            condition: condition.map(str::to_owned),
            hits: None,
            log: log.map(str::to_owned),
            enabled,
            line_text: None,
        };

        for breakpoint in [
            saved("<impl A for B>", None, None, false),
            saved("a b/m.c:3", None, None, true),
            saved("log", None, None, true),
            saved("f", Some("c == '('"), Some("x"), true),
            saved("f", Some("c == '\"'"), None, false),
            saved("f", Some("log > 3"), None, true),
            saved("f", Some("`a if b`"), None, true),
            saved("Foo<'a>::f", Some("x"), None, true),
            saved("f", None, Some("a\nb"), true),
        ] {
            reads_back(&breakpoint).unwrap_or_else(|error| panic!("{breakpoint:?}: {error}"));
        }
    }
}
