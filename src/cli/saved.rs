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

/// The `break` command that recreates a saved breakpoint.
pub fn command(saved: &Saved) -> String {
    let mut command = format!("break {}", saved.location);
    if let Some(hits) = &saved.hits {
        command.push_str(" hits ");
        command.push_str(hits);
    }

    if let Some(condition) = &saved.condition {
        command.push_str(" if ");
        command.push_str(condition);
    }

    if let Some(log) = &saved.log {
        command.push_str(" log \"");
        command.push_str(&log.replace('\\', "\\\\").replace('"', "\\\""));
        command.push('"');
    }

    if !saved.enabled {
        command.push_str(" disabled");
    }

    command
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
/// address ones included, as `-c` reads them.
pub fn commands(breakpoints: &[Breakpoint], root: &Path) -> String {
    let mut text = String::new();
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
        let line = command(&saved);

        if breakpoint.temporary {
            text.push('t');
        }

        text.push_str(&line);
        text.push('\n');
    }

    text
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
