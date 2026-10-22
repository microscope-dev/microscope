//! The CLI's settings: TOML files layered over the defaults.
//!
//! The user's file applies to every project; a project's
//! `.microscope/config.toml`, usually committed, and
//! `.microscope/config.local.toml`, one person's, override it there. Each file
//! is checked strictly on its own, so an error names its file, line, and
//! column, and the valid files are then merged key by key. Startup commands
//! accumulate instead, and launch configurations merge by name. A project's
//! startup commands, aliases, and launch configurations act, so they apply only
//! once the user trusts them; see [`TrustStore`].

use std::collections::BTreeMap;
use std::fmt;
use std::io::Read as _;
use std::ops::Range;
use std::path::{Path, PathBuf};

use serde::de::{self, Deserializer};
use serde::{Deserialize, Serialize};

use super::DisassemblySyntax;
use super::suggest;
use super::terminal::{ColorChoice, PathStyle, ThemeName, ThemeOverrides};

/// The defaults, as the file `microscope config init` writes commented out.
pub const DEFAULTS: &str = include_str!("default-config.toml");

/// The largest settings or state file read.
const MAX_FILE_BYTES: u64 = 1024 * 1024;

/// Every setting, with its default.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields, rename_all = "kebab-case")]
pub struct Config {
    pub ui: Ui,
    pub theme: ThemeOverrides,
    pub source: Source,
    pub stop: Stop,
    pub print: Print,
    pub disassembly: Disassembly,
    pub step: Stepping,
    pub backtrace: Backtraces,
    pub breakpoints: Breakpoints,
    pub debug_info: DebugInfo,
    pub history: History,
    pub follow: Follow,

    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub signals: BTreeMap<String, SignalActions>,

    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub source_map: Vec<SourceMapRule>,

    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub aliases: BTreeMap<String, String>,
    pub startup: Startup,
    pub projects: Projects,

    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub launch: Vec<Launch>,
}

/// Auto-detected, or forced on or off.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Toggle {
    #[default]
    Auto,
    Always,
    Never,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields, rename_all = "kebab-case")]
pub struct Ui {
    pub color: ColorChoice,
    pub theme: ThemeName,
    pub unicode: Toggle,
    pub hyperlinks: Toggle,
    pub paths: PathStyle,

    /// `auto`, `never`, or a command.
    pub pager: String,
    pub editor: String,
    pub prompt: String,
    pub confirm_quit: bool,
}

impl Default for Ui {
    fn default() -> Self {
        Self {
            color: ColorChoice::Auto,
            theme: ThemeName::Default,
            unicode: Toggle::Auto,
            hyperlinks: Toggle::Auto,
            paths: PathStyle::Relative,
            pager: "auto".to_owned(),
            editor: String::new(),
            prompt: "(microscope) ".to_owned(),
            confirm_quit: true,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields, rename_all = "kebab-case")]
pub struct Source {
    /// Lines shown before and after the current line.
    #[serde(deserialize_with = "context")]
    pub context: [u32; 2],
    pub highlight: bool,

    #[serde(deserialize_with = "bounded::<_, u8, 1, 16>")]
    pub tab_width: u8,
}

impl Default for Source {
    fn default() -> Self {
        Self {
            context: [3, 3],
            highlight: true,
            tab_width: 8,
        }
    }
}

/// What a stop prints after its header.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Section {
    Source,
    Locals,
    Displays,
    Registers,
    Disassembly,
    Backtrace,
    Threads,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields, rename_all = "kebab-case")]
pub struct Stop {
    #[serde(deserialize_with = "sections")]
    pub show: Vec<Section>,

    #[serde(deserialize_with = "bounded::<_, u32, 1, 1000>")]
    pub backtrace_frames: u32,

    #[serde(deserialize_with = "bounded::<_, u32, 1, 64>")]
    pub disassembly_instructions: u32,
    pub highlight_changes: bool,
    pub elapsed: bool,
}

impl Default for Stop {
    fn default() -> Self {
        Self {
            show: vec![Section::Source],
            backtrace_frames: 5,
            disassembly_instructions: 6,
            highlight_changes: true,
            elapsed: true,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum PrintStyle {
    #[default]
    Compact,
    Pretty,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Radix {
    #[default]
    Decimal,
    Hexadecimal,
}

/// The width pretty printing fits values to.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum Width {
    /// The terminal's, or 80 when output is not a terminal.
    #[default]
    Terminal,
    Columns(u16),
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields, rename_all = "kebab-case")]
pub struct Print {
    pub style: PrintStyle,
    pub radix: Radix,
    pub width: Width,

    #[serde(deserialize_with = "bounded::<_, u8, 0, 16>")]
    pub indent: u8,

    #[serde(deserialize_with = "bounded::<_, u64, 1, 64>")]
    pub max_depth: u64,

    #[serde(deserialize_with = "bounded::<_, u64, 1, 512>")]
    pub max_elements: u64,
}

impl Default for Print {
    fn default() -> Self {
        Self {
            style: PrintStyle::Compact,
            radix: Radix::Decimal,
            width: Width::Terminal,
            indent: 2,
            max_depth: 64,
            max_elements: 256,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields, rename_all = "kebab-case")]
pub struct Disassembly {
    pub syntax: DisassemblySyntax,
    pub show_bytes: bool,
}

impl Default for Disassembly {
    fn default() -> Self {
        Self {
            syntax: DisassemblySyntax::Intel,
            show_bytes: true,
        }
    }
}

/// How steps treat a language runtime's own code.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields, rename_all = "kebab-case")]
pub struct Stepping {
    pub runtime: StepRuntime,
}

/// Whether steps pass over a runtime's code to the program's code it
/// calls, or stop in it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum StepRuntime {
    #[default]
    Skip,
    Enter,
}

/// How backtraces show a language runtime's and an interpreter's own
/// frames.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields, rename_all = "kebab-case")]
pub struct Backtraces {
    pub runtime: RuntimeFrames,
}

/// Whether a backtrace folds each run of a runtime's frames into a line,
/// or shows every frame.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum RuntimeFrames {
    #[default]
    Fold,
    Show,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields, rename_all = "kebab-case")]
pub struct Breakpoints {
    pub save: bool,
}

impl Default for Breakpoints {
    fn default() -> Self {
        Self { save: true }
    }
}

/// Where separate debug files are found.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields, rename_all = "kebab-case")]
pub struct DebugInfo {
    /// Debug directories searched before the system's, relative to the
    /// project root.
    pub directories: Vec<PathBuf>,

    /// Download debug files no directory holds from debuginfod servers.
    pub debuginfod: bool,

    /// How many threads load debug information, unless `MICROSCOPE_JOBS` says;
    /// by default, one per CPU up to 16.
    pub jobs: Option<std::num::NonZeroUsize>,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields, rename_all = "kebab-case")]
pub struct History {
    #[serde(deserialize_with = "bounded::<_, u32, 0, 1_000_000>")]
    pub size: u32,
}

impl Default for History {
    fn default() -> Self {
        Self { size: 10_000 }
    }
}

/// Which processes a program starts the session debugs too.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields, rename_all = "kebab-case")]
pub struct Follow {
    pub mode: FollowMode,

    /// Programs debugged besides those `mode` chooses, by file name, where
    /// `*` matches any run of characters and `?` any one.
    pub programs: Vec<String>,

    /// What a stop in one process stops.
    pub stop: FollowStop,
}

impl Follow {
    /// The policy a group follows, with `programs` named too.
    pub fn policy(&self, programs: &[String]) -> microscope::FollowPolicy {
        microscope::FollowPolicy {
            mode: match self.mode {
                FollowMode::Auto => microscope::FollowMode::Auto,
                FollowMode::All => microscope::FollowMode::All,
                FollowMode::None => microscope::FollowMode::None,
            },
            programs: self.programs.iter().chain(programs).cloned().collect(),
            stop: match self.stop {
                FollowStop::Group => microscope::StopScope::Group,
                FollowStop::Process => microscope::StopScope::Process,
            },
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum FollowMode {
    /// The program, programs of a runtime microscope models, and those
    /// named; others are watched until they execute one of those.
    #[default]
    Auto,

    /// Every process the program starts.
    All,

    /// Only the program's own process.
    None,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum FollowStop {
    /// Every process stops with the one that stopped.
    #[default]
    Group,

    /// Only the process that stopped.
    Process,
}

/// A signal's actions as `handle` takes them, such as `nostop noprint pass`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SignalActions(String);

impl SignalActions {
    pub fn actions(&self) -> impl Iterator<Item = &str> {
        self.0.split_whitespace()
    }
}

impl<'de> Deserialize<'de> for SignalActions {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        let mut policy = microscope::SignalPolicy {
            stop: true,
            print: true,
            pass: true,
        };

        for action in text.split_whitespace() {
            super::commands::apply_signal_action(&mut policy, action).map_err(de::Error::custom)?;
        }

        if text.split_whitespace().next().is_none() {
            return Err(de::Error::custom(
                "expected actions such as \"nostop noprint pass\"",
            ));
        }

        Ok(Self(text))
    }
}

impl Serialize for SignalActions {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.0)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct SourceMapRule {
    pub from: PathBuf,
    pub to: PathBuf,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields, rename_all = "kebab-case")]
pub struct Startup {
    pub commands: Vec<String>,
}

/// Whether a project's acting settings apply.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Trust {
    /// Ask once, and again whenever they change.
    #[default]
    Ask,
    Always,
    Never,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields, rename_all = "kebab-case")]
pub struct Projects {
    pub trust: Trust,
}

/// A process to attach to: its id, or its name.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AttachTarget {
    Pid(u64),
    Name(String),
}

impl<'de> Deserialize<'de> for AttachTarget {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Visitor;

        impl de::Visitor<'_> for Visitor {
            type Value = AttachTarget;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a process id or a process name")
            }

            fn visit_i64<E: de::Error>(self, value: i64) -> Result<AttachTarget, E> {
                u64::try_from(value)
                    .ok()
                    .filter(|pid| *pid > 0)
                    .map(AttachTarget::Pid)
                    .ok_or_else(|| E::custom(format!("{value} is not a process id")))
            }

            fn visit_u64<E: de::Error>(self, value: u64) -> Result<AttachTarget, E> {
                self.visit_i64(i64::try_from(value).unwrap_or(-1))
            }

            fn visit_str<E: de::Error>(self, value: &str) -> Result<AttachTarget, E> {
                if value.is_empty() {
                    return Err(E::custom("a process name cannot be empty"));
                }

                Ok(value
                    .parse()
                    .map_or_else(|_| AttachTarget::Name(value.to_owned()), AttachTarget::Pid))
            }
        }

        deserializer.deserialize_any(Visitor)
    }
}

impl Serialize for AttachTarget {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Pid(pid) => serializer.serialize_u64(*pid),
            Self::Name(name) => serializer.serialize_str(name),
        }
    }
}

/// How a project's program is debugged, as `launch.json` describes it.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct Launch {
    pub name: String,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub program: Option<PathBuf>,

    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<String>,

    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<PathBuf>,

    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub startup: Vec<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attach: Option<AttachTarget>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub core: Option<PathBuf>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sysroot: Option<PathBuf>,

    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub module_path: Vec<PathBuf>,

    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub allow_module_mismatch: bool,

    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub views: Vec<PathBuf>,
}

/// What a launch configuration starts, with its paths resolved against the
/// project root.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LaunchTarget {
    Program(PathBuf),
    Attach(AttachTarget, Option<PathBuf>),
    Core(PathBuf, Option<PathBuf>),
}

impl Launch {
    /// Checks the keys make one coherent target, after merging.
    fn check(&self) -> Result<(), String> {
        let name = &self.name;
        match (&self.program, &self.attach, &self.core) {
            (_, Some(_), Some(_)) => {
                return Err(format!(
                    "launch configuration '{name}' sets both attach and core; choose one"
                ));
            }
            (None, None, None) => {
                return Err(format!(
                    "launch configuration '{name}' sets none of program, attach, and core"
                ));
            }
            _ => {}
        }

        let launches = self.attach.is_none() && self.core.is_none();
        if !launches
            && let Some(key) = [
                (!self.args.is_empty(), "args"),
                (!self.env.is_empty(), "env"),
                (self.cwd.is_some(), "cwd"),
            ]
            .into_iter()
            .find_map(|(set, key)| set.then_some(key))
        {
            return Err(format!(
                "launch configuration '{name}' sets {key}, which only a launched program takes"
            ));
        }

        if self.core.is_none()
            && let Some(key) = [
                (self.sysroot.is_some(), "sysroot"),
                (!self.module_path.is_empty(), "module-path"),
                (self.allow_module_mismatch, "allow-module-mismatch"),
            ]
            .into_iter()
            .find_map(|(set, key)| set.then_some(key))
        {
            return Err(format!(
                "launch configuration '{name}' sets {key}, which only a core dump takes"
            ));
        }

        if let Some(variable) = self
            .env
            .keys()
            .find(|name| name.is_empty() || name.contains('='))
        {
            return Err(format!(
                "launch configuration '{name}' sets the environment variable '{variable}', which is not a name"
            ));
        }

        Ok(())
    }

    /// What the configuration starts.
    pub fn target(&self, root: &Path) -> Result<LaunchTarget, ConfigError> {
        let program = self.program.as_ref().map(|path| root.join(path));
        Ok(match (&self.attach, &self.core) {
            (Some(attach), _) => LaunchTarget::Attach(attach.clone(), program),
            (None, Some(core)) => LaunchTarget::Core(root.join(core), program),
            (None, None) => LaunchTarget::Program(program.ok_or_else(|| {
                ConfigError::new(
                    None,
                    format!(
                        "launch configuration '{}' names no program, process, or core",
                        self.name
                    ),
                )
            })?),
        })
    }
}

/// Reads an integer and refuses one outside `MIN..=MAX`.
fn bounded<'de, D, T, const MIN: u64, const MAX: u64>(deserializer: D) -> Result<T, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de> + Copy + Into<u64>,
{
    let value = T::deserialize(deserializer)?;
    let number: u64 = value.into();
    if (MIN..=MAX).contains(&number) {
        Ok(value)
    } else {
        Err(de::Error::custom(format!(
            "{number} is out of range; expected {MIN} to {MAX}"
        )))
    }
}

fn context<'de, D: Deserializer<'de>>(deserializer: D) -> Result<[u32; 2], D::Error> {
    const MAX: u32 = 100;
    let lines = <[u32; 2]>::deserialize(deserializer)?;
    if lines.iter().any(|count| *count > MAX) {
        return Err(de::Error::custom(format!(
            "context lines are out of range; expected 0 to {MAX} before and after"
        )));
    }

    Ok(lines)
}

fn sections<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Vec<Section>, D::Error> {
    let sections = Vec::<Section>::deserialize(deserializer)?;
    for (index, section) in sections.iter().enumerate() {
        if sections[..index].contains(section) {
            return Err(de::Error::custom(format!(
                "{} is listed twice",
                serde_name(section)
            )));
        }
    }

    Ok(sections)
}

/// A unit variant's name as settings files spell it.
fn serde_name(value: &impl Serialize) -> String {
    toml::Value::try_from(value)
        .ok()
        .and_then(|value| value.as_str().map(str::to_owned))
        .unwrap_or_default()
}

impl<'de> Deserialize<'de> for Width {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Visitor;

        impl de::Visitor<'_> for Visitor {
            type Value = Width;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("\"terminal\" or a column count")
            }

            fn visit_i64<E: de::Error>(self, value: i64) -> Result<Width, E> {
                u16::try_from(value)
                    .ok()
                    .filter(|columns| (20..=1000).contains(columns))
                    .map(Width::Columns)
                    .ok_or_else(|| {
                        E::custom(format!(
                            "{value} is out of range; expected 20 to 1000 columns"
                        ))
                    })
            }

            fn visit_u64<E: de::Error>(self, value: u64) -> Result<Width, E> {
                self.visit_i64(i64::try_from(value).unwrap_or(i64::MAX))
            }

            fn visit_str<E: de::Error>(self, value: &str) -> Result<Width, E> {
                if value == "terminal" {
                    Ok(Width::Terminal)
                } else {
                    Err(E::custom(format!(
                        "unknown value '{value}'; expected \"terminal\" or a column count"
                    )))
                }
            }
        }

        deserializer.deserialize_any(Visitor)
    }
}

impl Serialize for Width {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Terminal => serializer.serialize_str("terminal"),
            Self::Columns(columns) => serializer.serialize_u16(*columns),
        }
    }
}

/// Which of the three files a setting was read from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum FileKind {
    User,
    Project,
    Local,
}

impl FileKind {
    pub const fn name(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Project => "project",
            Self::Local => "local",
        }
    }
}

/// Where a setting in effect came from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Origin {
    Default,
    File(FileKind, PathBuf),
    Flag(&'static str),
    Environment(&'static str),
}

impl fmt::Display for Origin {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Default => formatter.write_str("default"),
            Self::File(kind, path) => write!(formatter, "{} {}", kind.name(), path.display()),
            Self::Flag(flag) => write!(formatter, "flag {flag}"),
            Self::Environment(variable) => write!(formatter, "environment {variable}"),
        }
    }
}

/// A settings file that could not be used, and where.
#[derive(Debug)]
pub struct ConfigError {
    pub file: Option<PathBuf>,

    /// The one-based line and column.
    pub position: Option<(usize, usize)>,
    pub message: String,
}

impl ConfigError {
    fn new(file: Option<&Path>, message: impl Into<String>) -> Self {
        Self {
            file: file.map(Path::to_path_buf),
            position: None,
            message: message.into(),
        }
    }

    fn at(file: &Path, text: &str, span: Option<Range<usize>>, message: impl Into<String>) -> Self {
        Self {
            file: Some(file.to_path_buf()),
            position: span.map(|span| position(text, span.start)),
            message: message.into(),
        }
    }
}

impl fmt::Display for ConfigError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match (&self.file, self.position) {
            (Some(file), Some((line, column))) => {
                write!(formatter, "{}:{line}:{column}: ", file.display())?;
            }
            (Some(file), None) => write!(formatter, "{}: ", file.display())?,
            _ => {}
        }

        formatter.write_str(&self.message)
    }
}

impl std::error::Error for ConfigError {}

/// The one-based line and column of a byte offset.
fn position(text: &str, offset: usize) -> (usize, usize) {
    let offset = offset.min(text.len());
    let before = &text[..text.floor_char_boundary(offset)];
    let line = before.matches('\n').count() + 1;
    let column = before
        .rsplit_once('\n')
        .map_or(before, |(_, line)| line)
        .chars()
        .count()
        + 1;
    (line, column)
}

/// Rewrites serde's message for a settings file's reader: `unknown field`
/// becomes `unknown key` with a suggestion, and a value's error names its
/// key.
fn explain(message: &str, path: &str) -> String {
    let quoted = |text: &str| {
        text.split('`')
            .skip(1)
            .step_by(2)
            .map(str::to_owned)
            .collect::<Vec<_>>()
    };

    if let Some(rest) = message.strip_prefix("unknown field ") {
        let names = quoted(rest);
        let Some((key, expected)) = names.split_first() else {
            return message.to_owned();
        };

        // The path ends at the table holding the unknown key, or at the
        // key itself.
        let table = if path == key {
            ""
        } else {
            path.strip_suffix(&format!(".{key}")).unwrap_or(path)
        };

        let place = if table.is_empty() {
            format!("unknown table or key '{key}'")
        } else {
            format!("unknown key '{key}' in [{table}]")
        };

        return match suggest::did_you_mean(key, expected.iter().map(String::as_str)) {
            Some(suggestion) => format!("{place}; {suggestion}"),
            None if expected.is_empty() => format!("{place}; it takes no keys"),
            None => format!("{place}; expected one of {}", expected.join(", ")),
        };
    }

    if let Some(rest) = message.strip_prefix("unknown variant ") {
        let names = quoted(rest);
        if let Some((value, expected)) = names.split_first() {
            let suggestion = suggest::did_you_mean(value, expected.iter().map(String::as_str))
                .map(|suggestion| format!("; {suggestion}"))
                .unwrap_or_default();
            return format!(
                "unknown value '{value}' for {path}{suggestion} (expected {})",
                expected.join(", ")
            );
        }
    }

    if message.starts_with("missing field ") {
        let names = quoted(message);
        if let Some(key) = names.first() {
            let table = if path.is_empty() { "the file" } else { path };
            return format!("{table} needs the key '{key}'");
        }
    }

    if path.is_empty() {
        message.to_owned()
    } else {
        format!("{path}: {message}")
    }
}

/// One key of a path into a TOML document.
#[derive(Clone, Copy)]
enum Step<'a> {
    Key(&'a str),
    Index(usize),
}

/// The span of a key, or of an array element, at `path` in a document.
fn locate(text: &str, path: &[Step<'_>]) -> Option<Range<usize>> {
    let document = toml::de::DeTable::parse(text).ok()?;
    let mut table = document.get_ref();
    let mut value: Option<&toml::Spanned<toml::de::DeValue<'_>>> = None;
    let mut span = None;

    for step in path {
        if let Some(found) = value {
            if let toml::de::DeValue::Table(inner) = found.get_ref() {
                table = inner;
            } else if let (toml::de::DeValue::Array(array), Step::Index(index)) =
                (found.get_ref(), step)
            {
                let element = array.get(*index)?;
                span = Some(element.span());
                value = Some(element);
                continue;
            } else {
                return span;
            }
        }

        let Step::Key(name) = step else {
            return span;
        };

        let (key, found) = table.iter().find(|(key, _)| key.get_ref() == name)?;
        span = Some(key.span());
        value = Some(found);
    }

    span
}

/// One settings file that was read and is valid on its own.
#[derive(Clone, Debug)]
pub struct File {
    pub kind: FileKind,
    pub path: PathBuf,
    pub text: String,
    table: toml::Table,
}

/// The files a session's settings come from.
#[derive(Clone, Debug)]
pub struct Files {
    pub root: PathBuf,

    /// The user's file, if any is to be read.
    pub user: Option<PathBuf>,
    pub project: PathBuf,
    pub local: PathBuf,

    /// Whether any file is read at all.
    pub enabled: bool,

    /// The files that exist, from the lowest precedence to the highest.
    pub read: Vec<File>,
}

/// Which user file a session reads.
#[derive(Clone, Debug)]
pub enum UserFile {
    /// None, and no project file either.
    Disabled,

    /// The standard one, if it exists.
    Standard,

    /// A file named by `--config` or `MICROSCOPE_CONFIG`, which must exist.
    Named(PathBuf),
}

impl UserFile {
    /// `--no-config` and `--config`, then `MICROSCOPE_CONFIG`, which is empty
    /// to read no files.
    pub fn choose(no_config: bool, config: Option<&Path>) -> Self {
        if no_config {
            return Self::Disabled;
        }

        if let Some(path) = config {
            return Self::Named(path.to_path_buf());
        }

        match ENVIRONMENT.get().cloned().flatten() {
            Some(path) if path.is_empty() => Self::Disabled,
            Some(path) => Self::Named(PathBuf::from(path)),
            None => Self::Standard,
        }
    }
}

/// `MICROSCOPE_CONFIG`, as the process started with it.
static ENVIRONMENT: std::sync::OnceLock<Option<std::ffi::OsString>> = std::sync::OnceLock::new();

/// Takes `MICROSCOPE_CONFIG` out of the environment, keeping its value for
/// [`UserFile::choose`], so that programs the debugger launches see the
/// same environment, and so the same stack layout, whatever it was set to.
///
/// # Safety
///
/// No other thread may read or write the environment meanwhile. `main`
/// calls this before microscope starts any other thread.
#[allow(unsafe_code, reason = "the environment can only be edited unsafely")]
pub unsafe fn take_environment() {
    const VARIABLE: &str = "MICROSCOPE_CONFIG";
    let _ = ENVIRONMENT.set(std::env::var_os(VARIABLE));

    // SAFETY: the caller guarantees no other thread uses the environment.
    unsafe { std::env::remove_var(VARIABLE) };
}

/// The user's configuration directory, `$XDG_CONFIG_HOME/microscope` or
/// `~/.config/microscope`.
pub fn user_directory() -> Option<PathBuf> {
    let config = std::env::var_os("XDG_CONFIG_HOME")
        .filter(|path| !path.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))?;
    Some(config.join("microscope"))
}

/// The user's state directory, `$XDG_STATE_HOME/microscope` or
/// `~/.local/state/microscope`. A relative `XDG_STATE_HOME` is ignored, as the
/// XDG specification says.
pub fn state_directory() -> Option<PathBuf> {
    std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/state")))
        .map(|state| state.join("microscope"))
}

/// The project a directory belongs to: the nearest directory, from `start`
/// upwards, holding `.microscope/` or `.git`, else `start` itself. A
/// repository's root ends the search, so a repository inside another
/// project, as a git worktree often is, never takes the outer one's
/// settings or saved state.
pub fn project_root(start: &Path) -> PathBuf {
    start
        .ancestors()
        .find(|directory| directory.join(".microscope").is_dir() || directory.join(".git").exists())
        .unwrap_or(start)
        .to_path_buf()
}

/// Reads a file of at most [`MAX_FILE_BYTES`], or `None` if it does not
/// exist.
pub fn read_text(path: &Path) -> Result<Option<String>, String> {
    let file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("{}: {error}", path.display())),
    };

    let mut text = String::new();
    file.take(MAX_FILE_BYTES + 1)
        .read_to_string(&mut text)
        .map_err(|error| format!("{}: {error}", path.display()))?;

    if text.len() as u64 > MAX_FILE_BYTES {
        return Err(format!(
            "{} is larger than {MAX_FILE_BYTES} bytes",
            path.display()
        ));
    }

    Ok(Some(text))
}

impl Files {
    /// Finds and checks the files a session started in `start` reads.
    pub fn read(start: &Path, user: &UserFile) -> Result<Self, ConfigError> {
        let root = project_root(start);
        let mut files = Self {
            project: root.join(".microscope/config.toml"),
            local: root.join(".microscope/config.local.toml"),
            root,
            user: match user {
                UserFile::Disabled => None,
                UserFile::Standard => {
                    user_directory().map(|directory| directory.join("config.toml"))
                }
                UserFile::Named(path) => Some(path.clone()),
            },
            enabled: !matches!(user, UserFile::Disabled),
            read: Vec::new(),
        };

        if !files.enabled {
            return Ok(files);
        }

        let required = matches!(user, UserFile::Named(_));
        let candidates = [
            (FileKind::User, files.user.clone(), required),
            (FileKind::Project, Some(files.project.clone()), false),
            (FileKind::Local, Some(files.local.clone()), false),
        ];

        for (kind, path, required) in candidates {
            let Some(path) = path else { continue };
            match read_text(&path).map_err(|error| ConfigError::new(None, error))? {
                Some(text) => files.read.push(File::parse(kind, path, text)?),
                None if required => {
                    return Err(ConfigError::new(Some(&path), "the file does not exist"));
                }
                None => {}
            }
        }

        Ok(files)
    }

    /// The acting settings of the project's files, as TOML, or `None` when
    /// they set none.
    pub fn acting(&self) -> Option<String> {
        let mut acting = toml::Table::new();

        for file in &self.read {
            if file.kind == FileKind::User {
                continue;
            }

            let subset = file
                .table
                .iter()
                .filter(|(key, _)| ACTING.contains(&key.as_str()))
                .map(|(key, value)| (key.clone(), value.clone()))
                .collect::<toml::Table>();
            if !subset.is_empty() {
                acting.insert(file.kind.name().to_owned(), toml::Value::Table(subset));
            }
        }

        (!acting.is_empty()).then(|| {
            toml::to_string(&acting)
                .unwrap_or_else(|error| format!("the acting settings cannot be shown: {error}"))
        })
    }

    /// The policy for a project's acting settings, which only the user's
    /// file sets.
    pub fn trust_policy(&self) -> Trust {
        self.read
            .iter()
            .find(|file| file.kind == FileKind::User)
            .and_then(|file| file.table.get("projects"))
            .and_then(|projects| projects.get("trust"))
            .and_then(|trust| trust.clone().try_into().ok())
            .unwrap_or_default()
    }

    /// Merges the files over the defaults. Unless `trusted`, a project's
    /// acting settings are left out.
    pub fn settings(&self, trusted: bool) -> Result<Settings, ConfigError> {
        let mut merged = toml::Table::new();
        let mut origins = BTreeMap::new();
        let mut startup = Vec::new();

        for file in &self.read {
            let origin = Origin::File(file.kind, file.path.clone());
            let mut table = file.table.clone();
            if file.kind != FileKind::User && !trusted {
                table.retain(|key, _| !ACTING.contains(&key));
            }

            if let Some(commands) = table
                .get_mut("startup")
                .and_then(toml::Value::as_table_mut)
                .and_then(|startup| startup.remove("commands"))
            {
                let commands: Vec<String> =
                    commands.try_into().map_err(|error: toml::de::Error| {
                        ConfigError::new(Some(file.path.as_path()), error.message())
                    })?;
                for (index, text) in commands.into_iter().enumerate() {
                    startup.push(StartupCommand {
                        label: format!("{} [startup] command {}", file.path.display(), index + 1),
                        origin: origin.clone(),
                        text,
                    });
                }
            }

            if let Some(toml::Value::Array(launches)) = table.remove("launch") {
                merge_launches(&mut merged, launches, &origin, &mut origins)?;
            }

            merge_table(&mut merged, &table, "", &origin, &mut origins);
        }

        let config: Config = toml::Value::Table(merged)
            .try_into()
            .map_err(|error: toml::de::Error| ConfigError::new(None, error.message()))?;

        for launch in &config.launch {
            launch.check().map_err(|message| {
                let file = origins
                    .get(&format!("launch.{}", launch.name))
                    .and_then(|origin| match origin {
                        Origin::File(_, path) => Some(path.as_path()),
                        _ => None,
                    });
                ConfigError::new(file, message)
            })?;
        }

        Ok(Settings {
            config,
            root: self.root.clone(),
            startup,
            origins,
        })
    }
}

/// The settings that act rather than present, which a project's files set
/// only once trusted.
const ACTING: [&str; 3] = ["startup", "aliases", "launch"];

/// Merges `source` into `target` key by key, recording where each setting
/// came from. A value that is not a table replaces the one before.
fn merge_table(
    target: &mut toml::Table,
    source: &toml::Table,
    prefix: &str,
    origin: &Origin,
    origins: &mut BTreeMap<String, Origin>,
) {
    for (key, value) in source {
        let path = if prefix.is_empty() {
            key.clone()
        } else {
            format!("{prefix}.{key}")
        };

        if let (Some(toml::Value::Table(existing)), toml::Value::Table(incoming)) =
            (target.get_mut(key), value)
        {
            merge_table(existing, incoming, &path, origin, origins);
            continue;
        }

        target.insert(key.clone(), value.clone());
        origins
            .retain(|recorded, _| recorded != &path && !recorded.starts_with(&format!("{path}.")));
        record_leaves(value, &path, origin, origins);
    }
}

fn record_leaves(
    value: &toml::Value,
    path: &str,
    origin: &Origin,
    origins: &mut BTreeMap<String, Origin>,
) {
    match value {
        toml::Value::Table(table) if !table.is_empty() => {
            for (key, value) in table {
                record_leaves(value, &format!("{path}.{key}"), origin, origins);
            }
        }
        _ => {
            origins.insert(path.to_owned(), origin.clone());
        }
    }
}

/// Merges launch configurations by name: a later file's keys override an
/// earlier configuration of the same name, and new names are added.
fn merge_launches(
    merged: &mut toml::Table,
    launches: Vec<toml::Value>,
    origin: &Origin,
    origins: &mut BTreeMap<String, Origin>,
) -> Result<(), ConfigError> {
    let Some(existing) = merged
        .entry("launch")
        .or_insert_with(|| toml::Value::Array(Vec::new()))
        .as_array_mut()
    else {
        return Err(ConfigError::new(
            None,
            "launch must be an array of tables, as in [[launch]]",
        ));
    };

    for launch in launches {
        let toml::Value::Table(launch) = launch else {
            continue;
        };

        let name = launch
            .get("name")
            .and_then(toml::Value::as_str)
            .unwrap_or_default()
            .to_owned();
        let prefix = format!("launch.{name}");
        origins
            .entry(prefix.clone())
            .or_insert_with(|| origin.clone());
        let mut scratch = BTreeMap::new();

        if let Some(toml::Value::Table(same)) = existing
            .iter_mut()
            .find(|entry| entry.get("name").and_then(toml::Value::as_str) == Some(&name))
        {
            merge_table(same, &launch, &prefix, origin, &mut scratch);
        } else {
            merge_table(
                &mut toml::Table::new(),
                &launch,
                &prefix,
                origin,
                &mut scratch,
            );
            existing.push(toml::Value::Table(launch));
        }

        origins.extend(scratch);
    }

    Ok(())
}

impl File {
    /// Checks one file on its own.
    fn parse(kind: FileKind, path: PathBuf, text: String) -> Result<Self, ConfigError> {
        let document = toml::Deserializer::parse(&text).map_err(|error| {
            ConfigError::at(&path, &text, error.span(), error.message().trim_end())
        })?;

        if let Err(error) = serde_path_to_error::deserialize::<_, Config>(document) {
            let path_text = error.path().to_string();
            let path_text = if path_text == "." {
                String::new()
            } else {
                path_text
            };

            let inner = error.into_inner();
            return Err(ConfigError::at(
                &path,
                &text,
                inner.span(),
                explain(inner.message(), &path_text),
            ));
        }

        let table = text
            .parse::<toml::Table>()
            .map_err(|error| ConfigError::at(&path, &text, error.span(), error.message()))?;
        let file = Self {
            kind,
            path,
            text,
            table,
        };

        file.check()?;
        Ok(file)
    }

    fn error_at(&self, steps: &[Step<'_>], message: impl Into<String>) -> ConfigError {
        ConfigError::at(&self.path, &self.text, locate(&self.text, steps), message)
    }

    /// The checks that need more than one value's type.
    fn check(&self) -> Result<(), ConfigError> {
        let config: Config = self
            .table
            .clone()
            .try_into()
            .map_err(|error: toml::de::Error| {
                ConfigError::new(Some(&self.path), error.message())
            })?;

        if self.kind != FileKind::User && self.table.contains_key("projects") {
            return Err(self.error_at(
                &[Step::Key("projects")],
                "[projects] may be set only in the user's file, so a project cannot trust itself",
            ));
        }

        for name in config.signals.keys() {
            if microscope::signal_named(name).is_none() {
                let names = microscope::signal_codes()
                    .filter_map(microscope::signal_name)
                    .collect::<Vec<_>>();
                let suggestion = suggest::did_you_mean(name, names.iter().map(String::as_str))
                    .map(|suggestion| format!("; {suggestion}"))
                    .unwrap_or_default();
                return Err(self.error_at(
                    &[Step::Key("signals"), Step::Key(name)],
                    format!("unknown signal '{name}' in [signals]{suggestion}"),
                ));
            }
        }

        for (alias, expansion) in &config.aliases {
            if let Err(message) = check_alias(alias, expansion) {
                return Err(self.error_at(&[Step::Key("aliases"), Step::Key(alias)], message));
            }
        }

        for (index, launch) in config.launch.iter().enumerate() {
            if launch.name.trim().is_empty() || launch.name.contains('.') {
                return Err(self.error_at(
                    &[Step::Key("launch"), Step::Index(index)],
                    "a launch configuration needs a name, without dots",
                ));
            }

            if config.launch[..index]
                .iter()
                .any(|earlier| earlier.name == launch.name)
            {
                return Err(self.error_at(
                    &[Step::Key("launch"), Step::Index(index)],
                    format!("two launch configurations are named '{}'", launch.name),
                ));
            }
        }

        Ok(())
    }
}

/// Checks an alias is one new word standing for a command line.
fn check_alias(alias: &str, expansion: &str) -> Result<(), String> {
    use super::commands::{COMMANDS, command_named};
    if alias.is_empty() || alias.contains(char::is_whitespace) || alias.contains('/') {
        return Err(format!("the alias '{alias}' must be one word"));
    }

    if let Some(spec) = command_named(alias) {
        return Err(format!(
            "the alias '{alias}' would hide the command {}",
            spec.name
        ));
    }

    let first = expansion.split_whitespace().next().unwrap_or_default();
    let command = first.split_once('/').map_or(first, |(name, _)| name);
    if command_named(command).is_none() {
        let names = COMMANDS
            .iter()
            .flat_map(|spec| std::iter::once(spec.name).chain(spec.aliases.iter().copied()));
        let suggestion = suggest::did_you_mean(command, names)
            .map(|suggestion| format!("; {suggestion}"))
            .unwrap_or_default();
        return Err(format!(
            "the alias '{alias}' stands for '{expansion}', which does not start with a command{suggestion}"
        ));
    }

    Ok(())
}

/// One startup command, and where it came from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StartupCommand {
    /// Names the command in an error, as `FILE [startup] command 2`.
    pub label: String,
    pub origin: Origin,
    pub text: String,
}

/// The settings in effect.
#[derive(Clone, Debug)]
pub struct Settings {
    pub config: Config,
    pub root: PathBuf,

    /// The startup commands of every file, in order.
    pub startup: Vec<StartupCommand>,

    /// Where each setting a file set came from, by dotted key; every other
    /// setting is a default.
    pub origins: BTreeMap<String, Origin>,
}

impl Settings {
    /// The defaults, for clients that read no files.
    pub fn defaults(root: PathBuf) -> Self {
        Self {
            config: Config::default(),
            root,
            startup: Vec::new(),
            origins: BTreeMap::new(),
        }
    }

    /// Where a dotted key's value came from.
    pub fn origin(&self, key: &str) -> Origin {
        let mut key = key;

        loop {
            if let Some(origin) = self.origins.get(key) {
                return origin.clone();
            }

            match key.rsplit_once('.') {
                Some((parent, _)) => key = parent,
                None => return Origin::Default,
            }
        }
    }

    /// Sets the color choice from a flag or the environment, which outrank
    /// every file.
    pub fn override_color(&mut self, choice: ColorChoice, origin: Origin) {
        self.config.ui.color = choice;
        self.origins.insert("ui.color".to_owned(), origin);
    }

    /// Sets which processes are followed from `--follow`: a mode, or
    /// programs to debug besides those the mode chooses.
    pub fn override_follow(&mut self, follow: &[String]) {
        if follow.is_empty() {
            return;
        }

        let mut programs = Vec::new();
        for word in follow {
            match word.as_str() {
                "auto" => self.config.follow.mode = FollowMode::Auto,
                "all" => self.config.follow.mode = FollowMode::All,
                "none" => self.config.follow.mode = FollowMode::None,
                program => programs.push(program.to_owned()),
            }
        }

        self.origins
            .insert("follow.mode".to_owned(), Origin::Flag("--follow"));
        if !programs.is_empty() {
            self.config.follow.programs = programs;
            self.origins
                .insert("follow.programs".to_owned(), Origin::Flag("--follow"));
        }
    }

    /// Sets the disassembly syntax from `--disassembly-syntax`.
    pub fn override_syntax(&mut self, syntax: DisassemblySyntax) {
        self.config.disassembly.syntax = syntax;
        self.origins.insert(
            "disassembly.syntax".to_owned(),
            Origin::Flag("--disassembly-syntax"),
        );
    }

    /// The launch configuration named `name`.
    pub fn launch(&self, name: &str) -> Result<&Launch, String> {
        self.config
            .launch
            .iter()
            .find(|launch| launch.name == name)
            .ok_or_else(|| {
                let names = self.config.launch.iter().map(|launch| launch.name.as_str());
                let mut message = format!("no launch configuration is named '{name}'");
                if let Some(suggestion) = suggest::did_you_mean(name, names.clone()) {
                    message = format!("{message}; {suggestion}");
                } else if self.config.launch.is_empty() {
                    message.push_str("; the project has none");
                } else {
                    message = format!(
                        "{message}; the project has {}",
                        names.collect::<Vec<_>>().join(", ")
                    );
                }

                message
            })
    }

    /// Every setting in effect, one per line, with where it came from, as
    /// `microscope config show` prints it.
    pub fn show(&self) -> String {
        let mut table = match toml::Value::try_from(&self.config) {
            Ok(toml::Value::Table(table)) => table,
            Ok(_) => return "the settings are not a table".to_owned(),
            Err(error) => return format!("the settings cannot be shown: {error}"),
        };

        // Startup commands are listed one by one, each from its own file.
        table.remove("startup");
        let mut lines = Vec::new();
        flatten(&toml::Value::Table(table), "", &mut lines);

        for (index, command) in self.startup.iter().enumerate() {
            lines.push((
                format!("startup.commands[{index}]"),
                toml::Value::String(command.text.clone()).to_string(),
                Some(command.origin.clone()),
            ));
        }

        let width = lines
            .iter()
            .map(|(key, value, _)| key.len() + value.len() + 3)
            .max()
            .unwrap_or(0)
            .min(60);

        lines
            .into_iter()
            .map(|(key, value, origin)| {
                let origin = origin.unwrap_or_else(|| self.origin(&key));
                let setting = format!("{key} = {value}");
                format!("{setting:<width$}  # {origin}")
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
}

/// Lists a value's settings as dotted keys, naming launch configurations by
/// their names.
fn flatten(value: &toml::Value, path: &str, lines: &mut Vec<(String, String, Option<Origin>)>) {
    let join = |key: &str| {
        if path.is_empty() {
            key.to_owned()
        } else {
            format!("{path}.{key}")
        }
    };

    match value {
        toml::Value::Table(table) if !table.is_empty() => {
            for (key, value) in table {
                flatten(value, &join(key), lines);
            }
        }
        toml::Value::Array(launches) if path == "launch" => {
            for launch in launches {
                let name = launch
                    .get("name")
                    .and_then(toml::Value::as_str)
                    .unwrap_or_default();
                let mut launch = launch.as_table().cloned().unwrap_or_default();
                launch.remove("name");
                flatten(&toml::Value::Table(launch), &join(name), lines);
            }
        }
        _ => lines.push((path.to_owned(), value.to_string(), None)),
    }
}

/// The projects whose acting settings the user trusts, kept in
/// `$XDG_STATE_HOME/microscope/trust.toml`. Each holds the settings it trusted,
/// not a hash, so the file can be read to see what was allowed, and a
/// change to them asks again.
#[derive(Clone, Debug, Default)]
pub struct TrustStore {
    path: Option<PathBuf>,
    record: TrustRecord,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
struct TrustRecord {
    project: Vec<TrustedProject>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct TrustedProject {
    root: PathBuf,
    settings: String,
}

impl TrustStore {
    /// Reads the record; a missing one trusts nothing.
    pub fn load() -> Result<Self, ConfigError> {
        let path = state_directory().map(|directory| directory.join("trust.toml"));
        let Some(path) = path else {
            return Ok(Self::default());
        };

        let record = match read_text(&path).map_err(|error| ConfigError::new(None, error))? {
            Some(text) => toml::from_str(&text).map_err(|error| {
                ConfigError::at(&path, &text, error.span(), error.message().trim_end())
            })?,
            None => TrustRecord::default(),
        };

        Ok(Self {
            path: Some(path),
            record,
        })
    }

    /// Whether `root`'s acting settings are trusted as they are now.
    pub fn trusts(&self, root: &Path, acting: &str) -> bool {
        self.record
            .project
            .iter()
            .any(|project| project.root == root && project.settings == acting)
    }

    /// Trusts `root`'s acting settings as they are now.
    pub fn trust(&mut self, root: &Path, acting: &str) -> Result<(), String> {
        self.record.project.retain(|project| project.root != root);
        self.record.project.push(TrustedProject {
            root: root.to_path_buf(),
            settings: acting.to_owned(),
        });

        self.save()
    }

    /// Forgets `root`, returning whether it was trusted.
    pub fn untrust(&mut self, root: &Path) -> Result<bool, String> {
        let before = self.record.project.len();
        self.record.project.retain(|project| project.root != root);
        let removed = self.record.project.len() != before;
        if removed {
            self.save()?;
        }

        Ok(removed)
    }

    /// The trusted project roots.
    pub fn roots(&self) -> impl Iterator<Item = &Path> {
        self.record
            .project
            .iter()
            .map(|project| project.root.as_path())
    }

    fn save(&self) -> Result<(), String> {
        let path = self
            .path
            .as_deref()
            .ok_or("no state directory: set XDG_STATE_HOME or HOME")?;
        let text = format!(
            "# Projects whose startup commands, aliases, and launch configurations\n\
             # microscope runs, with the settings trusted. `microscope config untrust` forgets one.\n\n{}",
            toml::to_string(&self.record).map_err(|error| error.to_string())?
        );

        write_atomically(path, &text)
    }
}

/// Writes a file through a temporary file renamed into place, so no reader
/// sees half of it.
pub fn write_atomically(path: &Path, text: &str) -> Result<(), String> {
    let describe = |error: std::io::Error| format!("cannot write {}: {error}", path.display());

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(describe)?;
    }

    let temporary = path.with_extension(format!("tmp.{}", std::process::id()));
    std::fs::write(&temporary, text).map_err(describe)?;

    std::fs::rename(&temporary, path).map_err(|error| {
        let _ = std::fs::remove_file(&temporary);
        describe(error)
    })
}

/// The answer to whether to trust a project.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Answer {
    /// Trust it until its acting settings change.
    Yes,

    /// Trust it for this session.
    Once,
    No,
}

/// Shows a project's acting settings and asks whether to trust them,
/// until the answer is one of yes, once, or no. End of input is no.
pub fn ask_trust(
    root: &Path,
    acting: &str,
    input: &mut impl std::io::BufRead,
    output: &mut impl std::io::Write,
) -> std::io::Result<Answer> {
    writeln!(
        output,
        "The project at {} has settings that run commands:\n",
        root.display()
    )?;

    for line in acting.lines() {
        writeln!(output, "    {line}")?;
    }

    writeln!(output)?;

    loop {
        write!(
            output,
            "Trust them? yes, once (this session only), or no [y/o/N]: "
        )?;
        output.flush()?;
        let mut line = String::new();
        if input.read_line(&mut line)? == 0 {
            writeln!(output)?;
            return Ok(Answer::No);
        }

        match line.trim().to_ascii_lowercase().as_str() {
            "y" | "yes" => return Ok(Answer::Yes),
            "o" | "once" => return Ok(Answer::Once),
            "" | "n" | "no" => return Ok(Answer::No),
            _ => writeln!(output, "Answer yes, once, or no.")?,
        }
    }
}

/// The user's file with every setting commented out at its default, as
/// `microscope config init` writes it.
pub fn commented_defaults() -> String {
    DEFAULTS
        .lines()
        .map(|line| {
            if line.trim().is_empty() || line.trim_start().starts_with('#') {
                line.to_owned()
            } else {
                format!("# {line}")
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
        + "\n"
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(kind: FileKind, text: &str) -> Result<File, ConfigError> {
        File::parse(kind, PathBuf::from("/p/config.toml"), text.to_owned())
    }

    fn error(kind: FileKind, text: &str) -> String {
        parse(kind, text).expect_err("an invalid file").to_string()
    }

    #[test]
    fn the_defaults_file_documents_exactly_the_defaults() {
        let parsed = parse(FileKind::User, DEFAULTS).expect("the defaults parse");
        let config: Config = parsed.table.try_into().expect("the defaults deserialize");
        assert_eq!(config, Config::default());

        // Commented out, it sets nothing.
        let commented = parse(FileKind::User, &commented_defaults()).expect("comments parse");
        assert!(commented.table.is_empty(), "{:?}", commented.table);
    }

    #[test]
    fn errors_name_their_position_and_suggest_the_nearest_key_or_value() {
        assert_eq!(
            error(
                FileKind::User,
                "[ui]\nprompt = \"> \"\ncolour = \"never\"\n"
            ),
            "/p/config.toml:3:1: unknown key 'colour' in [ui]; did you mean 'color'?"
        );
        assert_eq!(
            error(FileKind::User, "[uii]\n"),
            "/p/config.toml:1:2: unknown table or key 'uii'; did you mean 'ui'?"
        );
        assert_eq!(
            error(FileKind::User, "[ui]\ncolor = \"alwyas\"\n"),
            "/p/config.toml:2:9: unknown value 'alwyas' for ui.color; did you mean 'always'? \
             (expected auto, always, never)"
        );
        assert_eq!(
            error(FileKind::User, "[source]\ntab-width = 0\n"),
            "/p/config.toml:2:13: source.tab-width: 0 is out of range; expected 1 to 16"
        );
        assert_eq!(
            error(FileKind::User, "[print]\nwidth = \"wide\"\n"),
            "/p/config.toml:2:9: print.width: unknown value 'wide'; expected \"terminal\" or a column count"
        );
        assert_eq!(
            error(FileKind::User, "[theme]\nvalue = \"blod\"\n"),
            "/p/config.toml:2:9: theme.value: unknown style word 'blod'; use a color (red, \
             bright-red, 0-255, #rrggbb), bold, dim, italic, underline, reverse, or on COLOR"
        );
        assert_eq!(
            error(FileKind::User, "[signals]\nSIGUSR = \"nostop\"\n"),
            "/p/config.toml:2:1: unknown signal 'SIGUSR' in [signals]; did you mean 'SIGUSR1' or 'SIGUSR2'?"
        );
        assert_eq!(
            error(FileKind::User, "[aliases]\np = \"print\"\n"),
            "/p/config.toml:2:1: the alias 'p' would hide the command print"
        );
        assert_eq!(
            error(FileKind::Project, "[projects]\ntrust = \"always\"\n"),
            "/p/config.toml:1:2: [projects] may be set only in the user's file, so a project cannot trust itself"
        );
        assert_eq!(
            error(FileKind::User, "[ui\n"),
            "/p/config.toml:1:4: unclosed table, expected `]`"
        );
    }

    fn file(kind: FileKind, text: &str) -> File {
        let mut file = parse(kind, text).expect("a valid file");
        file.path = PathBuf::from(format!("/{}.toml", kind.name()));
        file
    }

    fn files(read: Vec<File>) -> Files {
        Files {
            root: PathBuf::from("/p"),
            user: None,
            project: PathBuf::from("/p/.microscope/config.toml"),
            local: PathBuf::from("/p/.microscope/config.local.toml"),
            enabled: true,
            read,
        }
    }

    #[test]
    fn layers_merge_key_by_key_startup_accumulates_and_launches_merge_by_name() {
        let files = files(vec![
            file(
                FileKind::User,
                "[print]\nstyle = \"pretty\"\nmax-depth = 8\n[startup]\ncommands = [\"handle SIGPIPE nostop\"]\n",
            ),
            file(
                FileKind::Project,
                "[print]\nstyle = \"compact\"\n[stop]\nshow = [\"source\", \"locals\"]\n\
                 [startup]\ncommands = [\"break main\"]\n\
                 [[launch]]\nname = \"server\"\nprogram = \"build/server\"\nargs = [\"--port\", \"80\"]\n\
                 [[launch]]\nname = \"other\"\nprogram = \"build/other\"\n",
            ),
            file(
                FileKind::Local,
                "[stop]\nshow = [\"locals\"]\n[[launch]]\nname = \"server\"\nargs = [\"--port\", \"8080\"]\n",
            ),
        ]);
        let settings = files.settings(true).expect("valid settings");
        let config = &settings.config;
        assert_eq!(config.print.style, PrintStyle::Compact);
        assert_eq!(config.print.max_depth, 8);
        assert_eq!(config.stop.show, [Section::Locals]);
        assert_eq!(
            settings
                .startup
                .iter()
                .map(|command| command.text.as_str())
                .collect::<Vec<_>>(),
            ["handle SIGPIPE nostop", "break main"]
        );
        let server = settings.launch("server").expect("merged launch");
        assert_eq!(server.program.as_deref(), Some(Path::new("build/server")));
        assert_eq!(server.args, ["--port", "8080"]);
        assert!(settings.launch("other").is_ok());
        assert_eq!(
            settings.launch("sever").expect_err("a misspelling"),
            "no launch configuration is named 'sever'; did you mean 'server'?"
        );
        assert_eq!(
            settings.origin("print.max-depth").to_string(),
            "user /user.toml"
        );
        assert_eq!(
            settings.origin("launch.server.args").to_string(),
            "local /local.toml"
        );
        assert_eq!(
            settings.origin("launch.server.program").to_string(),
            "project /project.toml"
        );
        assert_eq!(settings.origin("ui.color"), Origin::Default);

        // Untrusted, the project's acting settings are left out.
        let untrusted = files.settings(false).expect("valid settings");
        assert_eq!(untrusted.startup.len(), 1);
        assert!(untrusted.config.launch.is_empty());
        assert_eq!(untrusted.config.stop.show, [Section::Locals]);
        assert!(
            files
                .acting()
                .is_some_and(|acting| acting.contains("break main"))
        );
    }

    #[test]
    fn launch_configurations_must_name_one_coherent_target() {
        let refused = |text: &str| {
            files(vec![file(FileKind::Project, text)])
                .settings(true)
                .expect_err("an incoherent launch")
                .to_string()
        };

        assert_eq!(
            refused("[[launch]]\nname = \"a\"\nattach = 4\ncore = \"core\"\n"),
            "/project.toml: launch configuration 'a' sets both attach and core; choose one"
        );
        assert_eq!(
            refused("[[launch]]\nname = \"a\"\nattach = \"server\"\nargs = [\"x\"]\n"),
            "/project.toml: launch configuration 'a' sets args, which only a launched program takes"
        );
        assert_eq!(
            refused("[[launch]]\nname = \"a\"\n"),
            "/project.toml: launch configuration 'a' sets none of program, attach, and core"
        );
        assert_eq!(
            error(
                FileKind::Project,
                "[[launch]]\nname = \"a\"\nprogram = \"x\"\n[[launch]]\nname = \"a\"\nprogram = \"y\"\n"
            ),
            "/p/config.toml:4:1: two launch configurations are named 'a'"
        );
    }

    #[test]
    fn trust_asks_until_answered_and_end_of_input_declines() {
        let ask = |typed: &str| {
            let mut output = Vec::new();
            let answer = ask_trust(
                Path::new("/p"),
                "[project.startup]\ncommands = [\"run\"]\n",
                &mut typed.as_bytes(),
                &mut output,
            )
            .expect("in-memory streams");
            (answer, String::from_utf8(output).expect("UTF-8"))
        };

        let (answer, shown) = ask("maybe\nYes\n");
        assert_eq!(answer, Answer::Yes);
        assert!(shown.contains("    commands = [\"run\"]"), "{shown}");
        assert!(shown.contains("Answer yes, once, or no."), "{shown}");
        assert_eq!(ask("o\n").0, Answer::Once);
        assert_eq!(ask("\n").0, Answer::No);
        assert_eq!(ask("").0, Answer::No);
    }
}
