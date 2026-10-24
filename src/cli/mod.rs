//! The interactive and scripted command-line client.
//!
//! The CLI is one client of [`DebuggerHandle`]: it parses commands, issues
//! requests, and renders replies. Debugger semantics live in the library.

pub mod commands;
mod complete;
pub mod config;
pub mod format;
mod group;
pub mod help;
mod highlight;
pub mod profile;
mod repl;
mod saved;
pub mod session;
mod stops;
mod suggest;
pub mod syscalls;
pub mod terminal;
pub mod value;

use std::ffi::OsString;
use std::fmt::Display;
use std::fs;
use std::io::{self, IsTerminal as _, Write as _};
use std::path::PathBuf;

use anyhow::{Context as _, Result, anyhow, bail};
use clap::ValueEnum;
use microscope::{AssemblySyntax, DebuggerHandle, Error, LaunchOptions, StopReason};
use tokio::io::{AsyncBufReadExt as _, BufReader};

use config::{Settings, Toggle};
use session::Session;
use terminal::{
    ColorChoice, ColorEnvironment, Look, Palette, Renderer, Role, color_enabled,
    terminal_control_enabled,
};

/// Renderers for each output stream.
#[derive(Clone, Copy)]
pub struct Renderers {
    pub stdout: Renderer,
    pub stderr: Renderer,

    /// Whether stdout accepts cursor-control sequences.
    pub stdout_control: bool,
}

impl Renderers {
    /// Detects color and terminal support from the streams and environment.
    pub fn detect(choice: ColorChoice, batch: bool) -> Self {
        let environment = ColorEnvironment::current();
        let stdout_is_terminal = io::stdout().is_terminal();
        let stderr_is_terminal = io::stderr().is_terminal();

        Self {
            stdout: Renderer::new(color_enabled(
                choice,
                &environment,
                stdout_is_terminal,
                batch,
            )),
            stderr: Renderer::new(color_enabled(
                choice,
                &environment,
                stderr_is_terminal,
                batch,
            )),
            stdout_control: terminal_control_enabled(&environment, stdout_is_terminal),
        }
    }

    /// Renderers for a debug adapter's console: paths relative to the
    /// project root, as the CLI shows them, and Unicode, which every editor
    /// shows.
    pub fn console(color: bool, root: PathBuf) -> Self {
        // One look per session, which lives as long as the adapter.
        let look: &'static Look = Box::leak(Box::new(Look {
            palette: Palette::new(
                terminal::ThemeName::Default,
                &terminal::ThemeOverrides::default(),
            ),
            paths: terminal::PathStyle::Relative,
            root: Some(root),
            unicode: true,
            hyperlinks: false,
        }));

        Self {
            stdout: Renderer::with_look(color, look),
            stderr: Renderer::with_look(color, look),
            stdout_control: false,
        }
    }

    /// Renderers in the look the settings describe, colored as they and
    /// the streams allow.
    pub fn configured(settings: &Settings, batch: bool) -> Self {
        let ui = &settings.config.ui;
        let detected = Self::detect(ui.color, batch);
        let stdout_is_terminal = io::stdout().is_terminal();
        let look: &'static Look = Box::leak(Box::new(Look {
            palette: Palette::new(ui.theme, &settings.config.theme),
            paths: ui.paths,
            root: Some(settings.root.clone()),
            unicode: match ui.unicode {
                Toggle::Always => true,
                Toggle::Never => false,
                Toggle::Auto => locale_is_utf8(),
            },
            hyperlinks: match ui.hyperlinks {
                Toggle::Always => true,
                Toggle::Never => false,
                Toggle::Auto => !batch && stdout_is_terminal && terminal_has_hyperlinks(),
            },
        }));

        Self {
            stdout: Renderer::with_look(detected.stdout.is_colored(), look),
            stderr: Renderer::with_look(detected.stderr.is_colored(), look),
            stdout_control: detected.stdout_control,
        }
    }
}

/// Whether the locale's character set is UTF-8, by the first of `LC_ALL`,
/// `LC_CTYPE`, and `LANG` that is set.
fn locale_is_utf8() -> bool {
    ["LC_ALL", "LC_CTYPE", "LANG"]
        .iter()
        .find_map(|name| std::env::var(name).ok().filter(|value| !value.is_empty()))
        .is_some_and(|locale| {
            let locale = locale.to_ascii_lowercase();
            locale.contains("utf-8") || locale.contains("utf8")
        })
}

/// Whether the terminal is one known to show OSC 8 hyperlinks.
fn terminal_has_hyperlinks() -> bool {
    let variable = |name| std::env::var(name).unwrap_or_default();
    let program = variable("TERM_PROGRAM");
    let term = variable("TERM");
    matches!(
        program.as_str(),
        "iTerm.app" | "WezTerm" | "vscode" | "ghostty" | "Hyper" | "rio"
    ) || ["kitty", "foot", "alacritty", "ghostty", "wezterm"]
        .iter()
        .any(|name| term.contains(name))
        || std::env::var_os("WT_SESSION").is_some()
        || std::env::var_os("KITTY_WINDOW_ID").is_some()
        || variable("VTE_VERSION")
            .parse::<u32>()
            .is_ok_and(|version| version >= 5000)
}

/// The assembly syntax `disassemble` renders.
#[derive(
    Clone, Copy, Debug, Default, Eq, PartialEq, ValueEnum, serde::Deserialize, serde::Serialize,
)]
#[serde(rename_all = "kebab-case")]
pub enum DisassemblySyntax {
    /// Intel syntax: destination first, `qword ptr [rbp-0x8]`.
    #[default]
    Intel,

    /// AT&T syntax as GNU tools print it: source first, `-0x8(%rbp)`.
    Att,
}

impl From<DisassemblySyntax> for AssemblySyntax {
    fn from(syntax: DisassemblySyntax) -> Self {
        match syntax {
            DisassemblySyntax::Intel => Self::Intel,
            DisassemblySyntax::Att => Self::Att,
        }
    }
}

impl From<AssemblySyntax> for DisassemblySyntax {
    fn from(syntax: AssemblySyntax) -> Self {
        match syntax {
            AssemblySyntax::Intel => Self::Intel,
            AssemblySyntax::Att => Self::Att,
        }
    }
}

/// What the input loop does after a command.
enum Control {
    /// Print the output, if any, and read the next command.
    Continue(String),
    ClearScreen,
    Quit,
}

/// How a script reacts to a failed command.
#[derive(Clone, Copy)]
enum OnError {
    /// Stop the script and fail the session.
    Abort,

    /// Report the error and read the next command.
    Report,
}

/// How `run` starts the inferior.
#[derive(Clone, Debug, Default)]
pub struct LaunchSettings {
    pub arguments: Vec<OsString>,
    pub environment: Vec<(OsString, Option<OsString>)>,
    pub working_directory: Option<PathBuf>,
}

impl LaunchSettings {
    /// The options for one launch, sharing the debugger's standard streams.
    fn options(&self) -> LaunchOptions {
        LaunchOptions {
            arguments: self.arguments.clone(),
            environment: self.environment.clone(),
            working_directory: self.working_directory.clone(),
            ..LaunchOptions::default()
        }
    }
}

pub struct Cli {
    /// The debugger commands act on: the session's, or in a group, the
    /// selected member's.
    selected: std::sync::Mutex<Selected>,

    /// The process group the session debugs, when it follows the
    /// processes its program starts.
    group: Option<group::Group>,
    renderers: Renderers,
    settings: Settings,
    syntax: AssemblySyntax,
    launch: LaunchSettings,
    views: std::sync::Mutex<ViewSources>,

    /// The breakpoints an interactive session keeps for the next one.
    kept: std::sync::OnceLock<std::sync::Mutex<saved::Kept>>,

    /// The terminal's width and height as the line editor last measured
    /// them, or 0.
    columns: std::sync::atomic::AtomicUsize,
    rows: std::sync::atomic::AtomicUsize,

    /// The exceptions that stop, as `catch` last chose them.
    exceptions: std::sync::Mutex<microscope::ExceptionStops>,

    /// Whether backtraces show every runtime frame rather than folding
    /// them, as `set runtime-frames` last chose.
    runtime_frames: std::sync::atomic::AtomicBool,

    /// The expressions every stop prints.
    displays: std::sync::Mutex<stops::Displays>,

    /// The values the last stops showed, which a stop marks changes from.
    changes: std::sync::Mutex<stops::Changes>,

    /// What completion knows of the loaded code and the selected frame.
    completion_code: std::sync::Mutex<Option<complete::Code>>,
    completion_names: std::sync::Mutex<Option<complete::Names>>,

    /// Source files lexed for highlighting, by path, with when each was
    /// modified.
    highlights: std::sync::Mutex<Highlights>,

    /// The program's profile, recording or recorded, boxed as a profile is
    /// large.
    profiling: std::sync::Mutex<Box<profile::Profiling>>,

    /// Whether the inferior replaced its program with one microscope does
    /// not follow, which fails a batch run.
    unfollowed_exec: std::sync::atomic::AtomicBool,

    /// Whether a process the group's policy would debug executed a program
    /// microscope could not load, which fails a batch run too.
    unfollowed_process: std::sync::atomic::AtomicBool,
}

/// The member commands act on, and its handle.
struct Selected {
    member: microscope::MemberNumber,
    handle: DebuggerHandle,
}

/// A source file's lines, each with its highlighted spans.
type Lexed = Vec<(String, Vec<highlight::Span>)>;

/// Lexed source files by path, with when each was modified.
type Highlights =
    std::collections::BTreeMap<PathBuf, (Option<std::time::SystemTime>, std::sync::Arc<Lexed>)>;

/// The view files a session loads: those it was given or loaded, most
/// recent first, then the project's and the user's.
#[derive(Default)]
struct ViewSources {
    session: Vec<microscope::view_files::ViewFile>,
    discovered: Vec<microscope::view_files::ViewFile>,
}

impl Cli {
    pub fn new(
        debugger: DebuggerHandle,
        renderers: Renderers,
        settings: Settings,
        launch: LaunchSettings,
    ) -> Self {
        let runtime_frames = settings.config.backtrace.runtime == config::RuntimeFrames::Show;

        Self {
            selected: std::sync::Mutex::new(Selected {
                member: 1,
                handle: debugger,
            }),
            group: None,
            renderers,
            syntax: settings.config.disassembly.syntax.into(),
            settings,
            launch,
            views: std::sync::Mutex::new(ViewSources {
                session: Vec::new(),
                discovered: Vec::new(),
            }),
            kept: std::sync::OnceLock::new(),
            columns: std::sync::atomic::AtomicUsize::new(0),
            rows: std::sync::atomic::AtomicUsize::new(0),
            exceptions: std::sync::Mutex::default(),
            runtime_frames: std::sync::atomic::AtomicBool::new(runtime_frames),
            displays: std::sync::Mutex::default(),
            changes: std::sync::Mutex::default(),
            highlights: std::sync::Mutex::default(),
            completion_code: std::sync::Mutex::default(),
            completion_names: std::sync::Mutex::default(),
            profiling: std::sync::Mutex::default(),
            unfollowed_exec: std::sync::atomic::AtomicBool::new(false),
            unfollowed_process: std::sync::atomic::AtomicBool::new(false),
        }
    }

    /// Makes the session one of a process group, whose root is the
    /// debugger it was made with, and whose members' handles read source
    /// files through `source_paths`.
    #[must_use]
    pub fn with_group(
        mut self,
        group: microscope::GroupHandle,
        source_paths: microscope::SourcePathMap,
    ) -> Self {
        self.group = Some(group::Group::new(group, source_paths));
        self
    }

    /// The debugger commands act on.
    pub(super) fn debugger(&self) -> DebuggerHandle {
        self.selected
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .handle
            .clone()
    }

    /// The debugger breakpoints belong to: a group's root, whose
    /// breakpoints every member has, or the session's.
    pub(super) fn group_debugger(&self) -> DebuggerHandle {
        self.group
            .as_ref()
            .map_or_else(|| self.debugger(), |group| group.handle.root())
    }

    /// The member commands act on.
    pub(super) fn selected_member(&self) -> microscope::MemberNumber {
        self.selected
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .member
    }

    /// The process the session launched, while it is alive.
    pub async fn live_process(&self) -> Option<microscope::ProcessId> {
        match self.debugger().snapshot().await.ok()?.inferior {
            microscope::InferiorState::Running { process_id, .. }
            | microscope::InferiorState::Stopped { process_id, .. } => Some(process_id),
            microscope::InferiorState::NotRunning => None,
        }
    }

    /// Records the terminal's size, as the line editor measured it.
    pub fn set_dimensions(&self, columns: usize, rows: usize) {
        self.columns
            .store(columns, std::sync::atomic::Ordering::Relaxed);
        self.rows.store(rows, std::sync::atomic::Ordering::Relaxed);
    }

    /// Prints a command's output, through the pager when it is taller
    /// than the terminal of an interactive session.
    async fn show(&self, output: &str) -> Result<()> {
        let rows = self.rows.load(std::sync::atomic::Ordering::Relaxed);
        let columns = self.columns().max(1);

        let pager = match self.settings.config.ui.pager.as_str() {
            "never" => None,
            "auto" => Some(
                std::env::var("PAGER")
                    .ok()
                    .filter(|pager| !pager.trim().is_empty())
                    .unwrap_or_else(|| "less -FRX".to_owned()),
            ),
            command => Some(command.to_owned()),
        };

        let tall = rows != 0
            && output
                .split('\n')
                .map(|line| {
                    terminal::plain(line)
                        .chars()
                        .count()
                        .div_ceil(columns)
                        .max(1)
                })
                .sum::<usize>()
                >= rows;

        let Some(pager) = pager.filter(|_| tall) else {
            return Ok(emit(output)?);
        };

        let text = format!("{output}\n");
        let shown = tokio::task::spawn_blocking(move || page(&pager, &text)).await?;
        if matches!(shown, Ok(true)) {
            return Ok(());
        }

        self.warn("the pager could not run; set [ui] pager, or PAGER");
        Ok(emit(output)?)
    }

    /// The width a value is laid out to: the terminal's, or 80 when output
    /// is not one, so that piped and batch output is the same everywhere.
    pub fn columns(&self) -> usize {
        match self.columns.load(std::sync::atomic::Ordering::Relaxed) {
            0 => 80,
            columns => columns,
        }
    }

    /// Loads the project's view files, under `project_root`, and the
    /// user's, and the session files at `paths`, and returns a warning for
    /// each file or view it could not use, the program's own included.
    pub async fn load_view_sources(
        &self,
        project_root: &std::path::Path,
        paths: &[PathBuf],
    ) -> Vec<String> {
        let (discovered, mut warnings) = microscope::view_files::discover(project_root);
        let mut session = Vec::new();

        for path in paths {
            match microscope::view_files::read(path) {
                Ok(file) => session.insert(0, file),
                Err(error) => warnings.push(error),
            }
        }

        {
            let mut views = self
                .views
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            views.discovered = discovered;
            views.session = session;
        }

        warnings.extend(self.reload_views().await);

        warnings.extend(
            self.debugger()
                .module_image()
                .view_errors()
                .iter()
                .map(ToString::to_string),
        );

        warnings
    }

    /// What could not be used of the debug information of the program and of
    /// the libraries already loaded, as an attached process or a core dump
    /// has them.
    pub async fn debug_information_warnings(&self) -> Vec<String> {
        let program = self.debugger().module_image();
        let mut warnings = crate::present::debug_information::warnings(&program, true);

        // A program yet to run has loaded nothing else.
        let Ok(loaded) = self.debugger().loaded_modules().await else {
            return warnings;
        };

        for record in loaded.modules.iter() {
            if let Ok(image) = self.debugger().loaded_module_image(record.module.id).await
                && image.id() != program.id()
            {
                warnings.extend(crate::present::debug_information::warnings(&image, false));
            }
        }

        warnings
    }

    /// Loads view files as [`Self::load_view_sources`] does and warns about
    /// each one that cannot be used. Returns whether every one could be used.
    pub async fn load_views(&self, project_root: &std::path::Path, paths: &[PathBuf]) -> bool {
        let warnings = self.load_view_sources(project_root, paths).await;
        for warning in &warnings {
            self.warn(&format!("views: {warning}"));
        }

        warnings.is_empty()
    }

    /// Presents values with the session's view files and the kernels beside
    /// them, and returns what kept parts of them out.
    async fn reload_views(&self) -> Vec<String> {
        let sources = {
            let views = self
                .views
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            views
                .session
                .iter()
                .chain(&views.discovered)
                .cloned()
                .collect::<Vec<_>>()
        };

        let mut kernels = Vec::<microscope::view_files::KernelFile>::new();

        // Files in one directory share the kernels beside them.
        for kernel in sources.iter().flat_map(|file| &file.kernels) {
            if !kernels.iter().any(|loaded| loaded.path == kernel.path) {
                kernels.push(kernel.clone());
            }
        }

        let files = sources
            .iter()
            .map(|file| (file.name.as_str(), file.text.as_str()))
            .collect::<Vec<_>>();
        let mut renderers = Vec::<microscope::view_files::RendererFile>::new();

        for renderer in sources.iter().flat_map(|file| &file.renderers) {
            if !renderers.iter().any(|loaded| loaded.path == renderer.path) {
                renderers.push(renderer.clone());
            }
        }

        match self
            .debugger()
            .load_views(&files, &kernels, &renderers)
            .await
        {
            Ok(errors) => errors.iter().map(ToString::to_string).collect(),
            Err(error) => vec![error.to_string()],
        }
    }

    /// Runs the session's scripts and then its REPL, pausing the inferior on
    /// Ctrl-C.
    pub async fn run(&self, session: &Session) -> Result<()> {
        let session_batch = session.batch;
        let session = self.run_inputs(session);
        tokio::pin!(session);
        loop {
            tokio::select! {
                result = &mut session => {
                    result?;

                    // A script that ran to its end still failed when the
                    // program it debugged was replaced by one it did not.
                    if session_batch
                        && self.unfollowed_exec.load(std::sync::atomic::Ordering::Relaxed)
                    {
                        bail!(
                            "the inferior replaced its program with another, which microscope \
                             does not follow; launch that program itself"
                        );
                    }

                    if session_batch
                        && self.unfollowed_process.load(std::sync::atomic::Ordering::Relaxed)
                    {
                        bail!(
                            "a process the program started executed a program microscope \
                             cannot load; launch that program itself"
                        );
                    }

                    return Ok(());
                }
                signal = tokio::signal::ctrl_c() => {
                    signal.context("failed to listen for Ctrl-C")?;
                    self.interrupt().await;
                }
            }
        }
    }

    /// Pauses a running inferior. Ctrl-C never ends the session: with nothing
    /// running there is nothing to interrupt, and `quit` or end-of-input exits.
    async fn interrupt(&self) {
        // A group stops as a whole once its selected member stops, or at
        // once when that member has no process left.
        let paused = match &self.group {
            Some(group) => match self.debugger().pause().await {
                Err(Error::NotRunning | Error::AlreadyStopped) => {
                    group.handle.pause_all().await.map(|()| StopReason::Pause)
                }
                paused => paused,
            },
            None => self.debugger().pause().await,
        };
        match paused {
            Ok(_) | Err(Error::NotRunning | Error::AlreadyStopped | Error::PostMortemTarget) => {}
            Err(error) => self.report_error(&anyhow!(error).context("failed to pause inferior")),
        }
    }

    async fn run_inputs(&self, args: &Session) -> Result<()> {
        // The program is loaded and the session can take its first command.
        microscope::timings::mark("ready");
        self.announce(args)?;

        for warning in self.debug_information_warnings().await {
            self.warn(&warning);
        }

        self.load_views(&self.settings.root, &args.views).await;
        self.apply_signal_settings().await?;

        if self.settings.config.step.runtime == config::StepRuntime::Enter {
            self.debugger().set_step_into_runtime(true).await?;
        }
        if !self.settings.config.step.skip.is_empty() {
            let skips = microscope::StepSkips::parse(&self.settings.config.step.skip)?;
            self.debugger().set_step_skips(skips).await?;
        }

        // Scripts behave the same in every checkout, so only a session at
        // a terminal keeps its breakpoints.
        if !args.batch
            && io::stdin().is_terminal()
            && io::stdout().is_terminal()
            && self.debugger().core_dump().is_none()
            && self.settings.config.breakpoints.save
        {
            self.restore_breakpoints(&args.source_paths).await;
        }

        for command in &args.startup {
            if !self
                .run_scripted(&command.text, &command.label, OnError::Abort)
                .await?
            {
                return Ok(());
            }
        }

        for path in &args.command_files {
            let contents = fs::read_to_string(path)
                .with_context(|| format!("failed to read command file {}", path.display()))?;
            for (index, line) in contents.lines().enumerate() {
                let label = format!("{}:{}", path.display(), index + 1);
                if !self.run_scripted(line, label, OnError::Abort).await? {
                    return Ok(());
                }
            }
        }

        for (index, command) in args.commands.iter().enumerate() {
            let label = format!("--eval #{}", index + 1);
            if !self.run_scripted(command, label, OnError::Abort).await? {
                return Ok(());
            }
        }

        if !args.batch {
            if io::stdin().is_terminal() && io::stdout().is_terminal() {
                // Quitting kills a launched program, but detaches an
                // attached one and leaves a core as it was.
                let confirm = self.settings.config.ui.confirm_quit
                    && args.attached().is_none()
                    && self.debugger().core_dump().is_none();
                return repl::run(self, confirm).await;
            }

            return self.run_stdin(OnError::Report).await;
        }

        if args.command_files.is_empty() && args.commands.is_empty() {
            return self.run_stdin(OnError::Abort).await;
        }

        Ok(())
    }

    /// Applies the settings' `[signals]`, as `handle` would.
    async fn apply_signal_settings(&self) -> Result<()> {
        for (name, actions) in &self.settings.config.signals {
            let Some(code) = microscope::signal_named(name) else {
                bail!("unknown signal '{name}' in [signals]");
            };

            let mut policy = self.debugger().signal_policy(code).await?;
            for action in actions.actions() {
                commands::apply_signal_action(&mut policy, action)?;
            }

            self.debugger().set_signal_policy(code, policy).await?;
        }

        Ok(())
    }

    fn announce(&self, args: &Session) -> Result<()> {
        let stdout = self.renderers.stdout;

        if let Some(core) = self.debugger().core_dump() {
            if !args.batch {
                emit(&format!(
                    "{} {} of {} (process {})\n{}",
                    stdout.paint(Role::Success, "opened core dump"),
                    stdout.paint(Role::Metadata, core.path.display()),
                    stdout.paint(Role::Name, &core.process_name),
                    core.process_id,
                    format::stop(
                        &StopReason::CoreDump {
                            exception: core.exception.clone()
                        },
                        stdout
                    )
                ))?;
            }

            // Modules not proven to match the dump are always reported.
            for warning in format::core_module_warnings(core) {
                self.warn(&warning);
            }
        } else if !args.batch {
            let action = if args.attached().is_some() {
                "attached to"
            } else {
                "debugging"
            };

            emit(&format!(
                "{} {}{}",
                stdout.paint(Role::Success, action),
                stdout.paint(Role::Metadata, self.debugger().executable().display()),
                args.attached()
                    .map(|pid| format!(" (process {pid})"))
                    .unwrap_or_default()
            ))?;
        }

        Ok(())
    }

    async fn run_stdin(&self, on_error: OnError) -> Result<()> {
        let mut lines = BufReader::new(tokio::io::stdin()).lines();
        let mut number = 0_u64;

        while let Some(line) = lines.next_line().await? {
            number += 1;
            if !self
                .run_scripted(&line, format!("stdin:{number}"), on_error)
                .await?
            {
                break;
            }
        }

        Ok(())
    }

    /// Runs one line of a script and returns whether to keep reading.
    async fn run_scripted(
        &self,
        line: &str,
        label: impl Display,
        on_error: OnError,
    ) -> Result<bool> {
        match self.run_line(line).await {
            Ok(keep_running) => Ok(keep_running),
            Err(error) => {
                let error = error.context(label.to_string());
                match on_error {
                    OnError::Abort => Err(error),
                    OnError::Report if is_broken_pipe(&error) => Err(error),
                    OnError::Report => {
                        self.report_error(&error);
                        Ok(true)
                    }
                }
            }
        }
    }

    /// Executes one command line, keeps the breakpoints it leaves, and
    /// returns whether to keep reading.
    async fn run_line(&self, line: &str) -> Result<bool> {
        let result = self.run_unsaved_line(line).await;
        self.save_breakpoints().await;
        result
    }

    async fn run_unsaved_line(&self, line: &str) -> Result<bool> {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            return Ok(true);
        }

        match self.execute(line).await? {
            Control::Continue(output) => {
                if !output.is_empty() {
                    self.show(&output).await?;
                }

                Ok(true)
            }
            Control::ClearScreen => {
                if !self.renderers.stdout_control {
                    bail!("cannot clear screen: stdout is not an ANSI terminal");
                }

                let mut stdout = io::stdout().lock();
                stdout.write_all(b"\x1b[2J\x1b[H")?;
                stdout.flush()?;
                Ok(true)
            }
            Control::Quit => Ok(false),
        }
    }

    /// Restores the breakpoints the project's last interactive session
    /// kept, each pending until code for it loads, and starts keeping this
    /// session's.
    async fn restore_breakpoints(&self, source_paths: &microscope::SourcePathMap) {
        let path = saved::path(&self.settings.root);
        let shown = self.renderers.stderr.path(&path);
        let mut kept = saved::Kept {
            path: path.clone(),
            source_paths: source_paths.clone(),
            ..saved::Kept::default()
        };

        match saved::read(&path) {
            Err(message) => {
                self.warn(&format!(
                    "cannot restore breakpoints from {shown}: {message}; none are saved until it is fixed or deleted"
                ));
                kept.blocked = true;
            }
            Ok(contents) => {
                let mut restored = 0;

                for entry in &contents.breakpoints {
                    match self.restore_breakpoint(entry, source_paths).await {
                        Ok(changed) => {
                            restored += 1;
                            if let Some(text) = &entry.line_text {
                                kept.line_texts.insert(entry.location.clone(), text.clone());
                            }

                            if changed {
                                self.warn(&format!(
                                    "{} changed since it was saved",
                                    entry.location
                                ));
                            }
                        }
                        Err(error) => {
                            self.warn(&format!(
                                "cannot restore the breakpoint at {}: {error:#}",
                                entry.location
                            ));
                            kept.unrestored.push(entry.clone());
                        }
                    }
                }

                let mut displays = 0;

                for display in &contents.displays {
                    match self.layout(commands::Command::Print, &display.format) {
                        Ok(_) => {
                            self.displays
                                .lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner)
                                .add(&display.format, &display.expression);
                            displays += 1;
                        }
                        Err(error) => self.warn(&format!(
                            "cannot restore the display of {}: {error:#}",
                            display.expression
                        )),
                    }
                }

                kept.written = contents;

                let restored = [(restored, "breakpoint"), (displays, "display")]
                    .into_iter()
                    .filter(|(count, _)| *count > 0)
                    .map(|(count, noun)| format::plural(count, noun))
                    .collect::<Vec<_>>();
                if !restored.is_empty() {
                    let _ = emit(&format!(
                        "{} {}",
                        self.renderers.stdout.paint(Role::Success, "restored"),
                        restored.join(" and ")
                    ));
                }
            }
        }

        let _ = self.kept.set(std::sync::Mutex::new(kept));
    }

    /// Restores one saved breakpoint, returning whether its line reads
    /// differently than when it was saved.
    async fn restore_breakpoint(
        &self,
        entry: &saved::Saved,
        source_paths: &microscope::SourcePathMap,
    ) -> Result<bool> {
        let spec = commands::parse_breakpoint_location(&entry.location)?
            .ok_or_else(|| anyhow!("'{}' is no location", entry.location))?;

        let options = microscope::BreakpointOptions {
            hit_condition: entry.hits.as_deref().map(str::parse).transpose()?,
            condition: entry
                .condition
                .as_deref()
                .map(microscope::Condition::parse)
                .transpose()?,
            log_message: entry
                .log
                .as_deref()
                .map(microscope::LogMessage::parse)
                .transpose()?,
            enabled: entry.enabled,
            pending: true,
            ..microscope::BreakpointOptions::default()
        };

        let breakpoint = self.debugger().add_breakpoint_with(spec, options).await?;
        let Some(text) = &entry.line_text else {
            return Ok(false);
        };

        Ok(self
            .current_line_text(&breakpoint, source_paths)
            .await
            .is_some_and(|current| current.trim() != text.trim()))
    }

    /// The line a source breakpoint's first location is at, as it reads now.
    async fn current_line_text(
        &self,
        breakpoint: &microscope::Breakpoint,
        source_paths: &microscope::SourcePathMap,
    ) -> Option<String> {
        if !matches!(breakpoint.spec, microscope::BreakpointSpec::Source { .. }) {
            return None;
        }

        let placed = self.placed(breakpoint).await;
        let (path, line) = placed.first()?.source.clone()?;
        saved::line_text(source_paths, &path, line.get())
    }

    /// Writes the session's breakpoints to the project's file when they
    /// changed since it was last written.
    async fn save_breakpoints(&self) {
        let Some(kept) = self.kept.get() else {
            return;
        };

        if kept
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .blocked
        {
            return;
        }

        let Ok(snapshot) = self.debugger().snapshot().await else {
            return;
        };

        let root = &self.settings.root;
        let source_paths = kept
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .source_paths
            .clone();

        let mut entries = Vec::new();

        for breakpoint in snapshot.breakpoints.iter() {
            let Some(location) = saved::location(&breakpoint.spec, root) else {
                continue;
            };

            let known = kept
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .line_texts
                .get(&location)
                .cloned();

            let line_text = if known.is_some() {
                known
            } else {
                let text = self.current_line_text(breakpoint, &source_paths).await;
                if let Some(text) = &text {
                    kept.lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .line_texts
                        .insert(location, text.clone());
                }

                text
            };

            entries.extend(saved::saved(breakpoint, root, line_text));
        }

        let (path, error) = {
            let mut kept = kept
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            entries.extend(kept.unrestored.iter().cloned());
            let contents = saved::Contents {
                breakpoints: entries,
                displays: self
                    .displays
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .list
                    .iter()
                    .map(|display| saved::SavedDisplay {
                        expression: display.expression.clone(),
                        format: display.format.clone(),
                    })
                    .collect(),
            };

            if contents == kept.written {
                return;
            }

            match saved::write(&kept.path, &contents) {
                Ok(()) => {
                    kept.written = contents;
                    return;
                }
                Err(error) => {
                    kept.blocked = true;
                    (kept.path.clone(), error)
                }
            }
        };

        self.warn(&format!(
            "cannot save breakpoints to {}: {error}; none are saved for the rest of the session",
            self.renderers.stderr.path(&path)
        ));
    }

    /// Whether this session keeps its breakpoints for the next one.
    fn keeps_breakpoints(&self) -> bool {
        self.kept.get().is_some_and(|kept| {
            !kept
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .blocked
        })
    }

    fn report_error(&self, error: &anyhow::Error) {
        eprintln!(
            "{}: {error:#}",
            self.renderers.stderr.paint(Role::Error, "error")
        );
    }

    fn warn(&self, message: &str) {
        eprintln!(
            "{}: {message}",
            self.renderers.stderr.paint(Role::Warning, "warning")
        );
    }
}

/// Shows `text` through `pager`, a shell command, returning false when the
/// shell found no such command.
fn page(pager: &str, text: &str) -> io::Result<bool> {
    let mut child = std::process::Command::new("sh")
        .args(["-c", pager])
        .stdin(std::process::Stdio::piped())
        .spawn()?;
    if let Some(mut input) = child.stdin.take() {
        // A pager that quits early closes its input, which is no error.
        let _ = input.write_all(text.as_bytes());
    }

    Ok(wait_for_child(&mut child)?.is_none_or(|status| status.code() != Some(127)))
}

/// Waits for a process the CLI started, returning its status when it can
/// still be read. While a program is traced the debugger collects the exit
/// of every child of this process, so a child already collected has ended,
/// with a status no one can read now.
pub fn wait_for_child(
    child: &mut std::process::Child,
) -> io::Result<Option<std::process::ExitStatus>> {
    match child.wait() {
        Ok(status) => Ok(Some(status)),
        Err(error) if error.raw_os_error() == Some(nix::libc::ECHILD) => Ok(None),
        Err(error) => Err(error),
    }
}

/// Writes one line to stdout. Unlike `println!`, a closed pipe is an error
/// rather than a panic.
fn emit(text: &str) -> io::Result<()> {
    let mut stdout = io::stdout().lock();
    writeln!(stdout, "{text}")?;
    stdout.flush()
}

pub fn is_broken_pipe(error: &anyhow::Error) -> bool {
    error
        .chain()
        .filter_map(|cause| cause.downcast_ref::<io::Error>())
        .any(|cause| cause.kind() == io::ErrorKind::BrokenPipe)
}
