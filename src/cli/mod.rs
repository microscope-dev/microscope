pub mod auth;
pub mod cloud;
pub mod commands;
mod complete;
pub mod config;
pub mod format;
mod group;
pub mod help;
mod highlight;
pub mod profile;
pub mod query;
mod repl;
mod saved;
pub mod session;
mod stops;
pub mod symbols;
pub mod terminal;
pub mod upload;
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
use tokio::signal::unix::SignalKind;

use config::{Settings, Toggle};
use session::Session;
use terminal::{
    ColorChoice, ColorEnvironment, Look, Palette, Renderer, Role, color_enabled,
    terminal_control_enabled,
};

#[derive(Clone, Copy)]
pub struct Renderers {
    pub stdout: Renderer,
    pub stderr: Renderer,

    pub stdout_control: bool,
}

impl Renderers {
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

    pub fn console(color: bool, root: PathBuf) -> Self {
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

fn locale_is_utf8() -> bool {
    ["LC_ALL", "LC_CTYPE", "LANG"]
        .iter()
        .find_map(|name| std::env::var(name).ok().filter(|value| !value.is_empty()))
        .is_some_and(|locale| {
            let locale = locale.to_ascii_lowercase();
            locale.contains("utf-8") || locale.contains("utf8")
        })
}

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

enum Control {
    Continue(String),
    ClearScreen,
    Quit,
}

#[derive(Clone, Copy)]
enum OnError {
    Abort,

    Report,
}

#[derive(Clone, Debug, Default)]
pub struct LaunchSettings {
    pub arguments: Vec<OsString>,
    pub environment: Vec<(OsString, Option<OsString>)>,
    pub working_directory: Option<PathBuf>,
}

impl LaunchSettings {
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
    selected: std::sync::Mutex<Selected>,

    group: Option<group::Group>,
    renderers: Renderers,
    settings: Settings,
    syntax: AssemblySyntax,
    launch: LaunchSettings,
    views: std::sync::Mutex<ViewSources>,

    kept: std::sync::OnceLock<std::sync::Mutex<saved::Kept>>,

    columns: std::sync::atomic::AtomicUsize,
    rows: std::sync::atomic::AtomicUsize,

    exceptions: std::sync::Mutex<microscope::ExceptionStops>,

    runtime_frames: std::sync::atomic::AtomicBool,

    displays: std::sync::Mutex<stops::Displays>,

    changes: std::sync::Mutex<stops::Changes>,

    completion_code: std::sync::Mutex<Option<complete::Code>>,
    completion_names: std::sync::Mutex<Option<complete::Names>>,

    highlights: std::sync::Mutex<Highlights>,

    profiling: std::sync::Mutex<Box<profile::Profiling>>,

    unfollowed_exec: std::sync::atomic::AtomicBool,
}

struct Selected {
    member: microscope::MemberNumber,
    handle: DebuggerHandle,
}

type Lexed = Vec<(String, Vec<highlight::Span>)>;

type Highlights =
    std::collections::BTreeMap<PathBuf, (Option<std::time::SystemTime>, std::sync::Arc<Lexed>)>;

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
        }
    }

    #[must_use]
    pub fn with_group(
        mut self,
        group: microscope::GroupHandle,
        source_paths: microscope::SourcePathMap,
    ) -> Self {
        self.group = Some(group::Group::new(group, source_paths));
        self
    }

    pub(super) fn debugger(&self) -> DebuggerHandle {
        self.selected
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .handle
            .clone()
    }

    pub(super) fn group_debugger(&self) -> DebuggerHandle {
        self.group
            .as_ref()
            .map_or_else(|| self.debugger(), |group| group.handle.root())
    }

    pub(super) fn selected_member(&self) -> microscope::MemberNumber {
        self.selected
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .member
    }

    pub async fn live_process(&self) -> Option<microscope::ProcessId> {
        self.debugger()
            .snapshot()
            .await
            .ok()?
            .inferior
            .live_process()
    }

    pub fn set_dimensions(&self, columns: usize, rows: usize) {
        self.columns
            .store(columns, std::sync::atomic::Ordering::Relaxed);
        self.rows.store(rows, std::sync::atomic::Ordering::Relaxed);
    }

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

    pub fn columns(&self) -> usize {
        match self.columns.load(std::sync::atomic::Ordering::Relaxed) {
            0 => 80,
            columns => columns,
        }
    }

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

    pub async fn debug_information_warnings(&self) -> Vec<String> {
        let program = self.debugger().module_image();
        let mut warnings = crate::present::debug_information::warnings(&program, true);
        let mut images = vec![std::sync::Arc::clone(&program)];

        if let Ok(loaded) = self.debugger().loaded_modules().await {
            for record in loaded.modules.iter() {
                if let Ok(image) = self.debugger().loaded_module_image(record.module.id).await
                    && image.id() != program.id()
                {
                    warnings.extend(crate::present::debug_information::warnings(&image, false));
                    images.push(image);
                }
            }
        }

        warnings.extend(crate::present::debug_information::unkept_images(&images));
        warnings
    }

    pub async fn load_views(&self, project_root: &std::path::Path, paths: &[PathBuf]) -> bool {
        let warnings = self.load_view_sources(project_root, paths).await;
        for warning in &warnings {
            self.warn(&format!("views: {warning}"));
        }

        warnings.is_empty()
    }

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

    pub async fn run(
        &self,
        session: &Session,
        terminated: impl Future<Output = SignalKind>,
    ) -> Result<Option<SignalKind>> {
        let session_batch = session.batch;

        let mut interrupts =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())
                .context("failed to listen for Ctrl-C")?;
        let session = self.run_inputs(session);
        tokio::pin!(session, terminated);
        loop {
            tokio::select! {
                result = &mut session => {
                    result?;

                    if session_batch
                        && self.unfollowed_exec.load(std::sync::atomic::Ordering::Relaxed)
                    {
                        bail!(
                            "the inferior replaced its program with another, which microscope \
                             does not follow; launch that program itself"
                        );
                    }

                    let group = self.group.as_ref().map(|group| &group.handle);
                    if session_batch && group.is_some_and(microscope::GroupHandle::missed_a_process)
                    {
                        bail!(
                            "a process the program started executed a program microscope \
                             cannot load; launch that program itself"
                        );
                    }

                    if session_batch
                        && group.is_some_and(microscope::GroupHandle::missed_a_breakpoint_change)
                    {
                        bail!(
                            "a process the program started did not take every change to its \
                             breakpoints, so its stops may be missing"
                        );
                    }

                    return Ok(None);
                }
                Some(()) = interrupts.recv() => self.interrupt().await,
                signal = &mut terminated => return Ok(Some(signal)),
            }
        }
    }

    async fn interrupt(&self) {
        let paused = match &self.group {
            Some(group) => match self.debugger().pause().await {
                Err(Error::NotRunning | Error::AlreadyStopped) => {
                    group.handle.pause_all().await.map(|_| StopReason::Pause)
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

    async fn restore_breakpoint(
        &self,
        entry: &saved::Saved,
        source_paths: &microscope::SourcePathMap,
    ) -> Result<bool> {
        let spec = commands::parse_breakpoint_location(&entry.location)?
            .ok_or_else(|| anyhow!("'{}' is not a location", entry.location))?;

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

fn page(pager: &str, text: &str) -> io::Result<bool> {
    let mut child = std::process::Command::new("sh")
        .args(["-c", pager])
        .stdin(std::process::Stdio::piped())
        .spawn()?;
    if let Some(mut input) = child.stdin.take() {
        let _ = input.write_all(text.as_bytes());
    }

    Ok(wait_for_child(&mut child)?.is_none_or(|status| status.code() != Some(127)))
}

pub fn wait_for_child(
    child: &mut std::process::Child,
) -> io::Result<Option<std::process::ExitStatus>> {
    match child.wait() {
        Ok(status) => Ok(Some(status)),
        Err(error) if error.raw_os_error() == Some(nix::libc::ECHILD) => Ok(None),
        Err(error) => Err(error),
    }
}

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
