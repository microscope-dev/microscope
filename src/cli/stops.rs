//! What a stop prints: a header that says where, then the sections
//! `[stop] show` names, with the values that changed since the last stop
//! marked.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::time::Duration;

use anyhow::Result;
use microscope::{
    ExecutionContext, InferiorState, StackFrameId, StateSnapshot, StopContext, StopId, StopReason,
    ThreadActivity, ThreadState, VirtualAddress,
};

use super::commands::{Command, CommandSpec};
use super::config::Section;
use super::format;
use super::terminal::{Role, plain};
use super::{Cli, value};

/// An expression printed at every stop.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Display {
    pub id: u64,

    /// The format letters written after `display/`, as `print` takes them.
    pub format: String,
    pub expression: String,
}

impl Display {
    /// The display as `display` takes it.
    pub fn command(&self) -> String {
        if self.format.is_empty() {
            self.expression.clone()
        } else {
            format!("/{} {}", self.format, self.expression)
        }
    }
}

/// The session's displays, numbered in the order they were added.
#[derive(Debug, Default)]
pub struct Displays {
    added: u64,
    pub list: Vec<Display>,
}

impl Displays {
    pub fn add(&mut self, format: &str, expression: &str) -> Display {
        self.added += 1;
        let display = Display {
            id: self.added,
            format: format.to_owned(),
            expression: expression.to_owned(),
        };

        self.list.push(display.clone());
        display
    }

    /// Removes the displays numbered `ids`, or every display for `None`,
    /// returning how many it removed.
    fn remove(&mut self, ids: Option<&[u64]>) -> Result<usize> {
        let Some(ids) = ids else {
            return Ok(std::mem::take(&mut self.list).len());
        };

        if let Some(missing) = ids
            .iter()
            .find(|id| !self.list.iter().any(|display| display.id == **id))
        {
            anyhow::bail!("no display {missing}");
        }

        self.list.retain(|display| !ids.contains(&display.id));
        Ok(ids.len())
    }
}

/// One activation of a function at one stop, which the values a stop
/// shows are compared within.
#[derive(Clone, Debug)]
pub struct Activation {
    stop: StopId,
    context: ExecutionContext,
    function: String,
    frame: VirtualAddress,
}

type Key = (ExecutionContext, String, VirtualAddress, String);

/// The values the last stop showed and those this one has, as text, keyed
/// by activation and name.
#[derive(Debug, Default)]
pub struct Changes {
    stop: Option<StopId>,
    before: BTreeMap<Key, String>,
    shown: BTreeMap<Key, String>,
}

impl Changes {
    /// Records that `name` reads `text` in `activation`, and returns
    /// whether the last stop showed it reading otherwise.
    pub fn changed(&mut self, activation: &Activation, name: &str, text: &str) -> bool {
        if self.stop != Some(activation.stop) {
            self.before = std::mem::take(&mut self.shown);
            self.stop = Some(activation.stop);
        }

        let key = (
            activation.context,
            activation.function.clone(),
            activation.frame,
            name.to_owned(),
        );
        let changed = self.before.get(&key).is_some_and(|before| before != text);
        self.shown.insert(key, text.to_owned());
        changed
    }
}

/// Whether a stop is in the program's code, so that it says where and
/// prints its sections.
const fn in_code(reason: &StopReason) -> bool {
    matches!(
        reason,
        StopReason::Breakpoint { .. }
            | StopReason::Step { .. }
            | StopReason::StepIncomplete { .. }
            | StopReason::TaskEnded { .. }
            | StopReason::FutureDropped { .. }
            | StopReason::Watchpoint { .. }
            | StopReason::Exception(_)
            | StopReason::LanguageException(_)
            | StopReason::RuntimeEvent { .. }
            | StopReason::ProgramBreakpoint { .. }
            | StopReason::Pause
            | StopReason::Jump
            | StopReason::Assignment
    )
}

const fn section_name(section: Section) -> &'static str {
    match section {
        Section::Source => "source",
        Section::Locals => "locals",
        Section::Displays => "displays",
        Section::Registers => "registers",
        Section::Disassembly => "disassembly",
        Section::Backtrace => "backtrace",
        Section::Threads => "threads",
    }
}

impl Cli {
    /// Formats a stop: its header, saying where and after how long, then,
    /// for a stop in the program's code, its sections.
    pub(super) async fn stop_report(&self, reason: &StopReason, elapsed: Duration) -> String {
        if matches!(reason, StopReason::Exec { followed: false }) {
            self.unfollowed_exec
                .store(true, std::sync::atomic::Ordering::Relaxed);
        }

        let renderer = self.renderers.stdout;
        let snapshot = self.debugger().snapshot().await.ok();
        let located = in_code(reason) || matches!(reason, StopReason::Attach | StopReason::Entry);
        let mut suffix = String::new();

        if located {
            if let Ok(place) = self.describe_location(false).await {
                let _ = write!(suffix, " in {place}");
            }

            if let Some(snapshot) = &snapshot
                && snapshot.threads.len() > 1
                && let Some(ExecutionContext::Thread(thread)) = snapshot.selected
            {
                let _ = write!(
                    suffix,
                    " [thread {} of {}]",
                    renderer.paint(Role::Metadata, thread),
                    snapshot.threads.len()
                );
            }

            // The task the stopped thread runs, as the thread list shows it.
            if let Some(snapshot) = &snapshot
                && let Some(ExecutionContext::Thread(thread)) = snapshot.selected
                && let Some(ThreadActivity::Task { task, .. }) = snapshot
                    .threads
                    .iter()
                    .find(|listed| listed.id == thread)
                    .and_then(|listed| listed.activity.as_ref())
            {
                let _ = write!(
                    suffix,
                    " {}",
                    renderer.paint(Role::Metadata, format_args!("[{}]", task.number))
                );
            }
        }

        if self.settings.config.stop.elapsed && elapsed >= Duration::from_secs(1) {
            let _ = write!(
                suffix,
                " {}",
                renderer.paint(
                    Role::Metadata,
                    format!("(ran {:.2}s)", elapsed.as_secs_f64())
                )
            );
        }

        let mut output = match reason {
            StopReason::Watchpoint { hits } => format::watchpoint_hits(
                hits,
                snapshot
                    .as_ref()
                    .map_or(&[][..], |snapshot| &snapshot.watchpoints[..]),
                Some(&self.debugger().module_image()),
                &suffix,
                renderer,
            ),
            StopReason::LanguageException(raised) => {
                format::language_exception(raised, &suffix, renderer)
            }
            _ => format!("{}{suffix}", format::stop(reason, renderer)),
        };

        if let Some(snapshot) = &snapshot
            && in_code(reason)
        {
            for line in self.co_hits(snapshot).await {
                output.push('\n');
                output.push_str(&line);
            }
        }

        if let (StopReason::Breakpoint { hits, .. }, Some(snapshot)) = (reason, &snapshot)
            && let Some(deleted) = self.deleted_temporaries(hits, snapshot)
        {
            output.push('\n');
            output.push_str(&deleted);
        }

        if in_code(reason) {
            let sections = self.sections(true).await;
            if !sections.is_empty() {
                output.push('\n');
                output.push_str(&sections);
            }
        }

        output
    }

    /// A line for each thread besides the stop's that the stop found at a
    /// breakpoint or watchpoint of its own, with the task it runs.
    async fn co_hits(&self, snapshot: &StateSnapshot) -> Vec<String> {
        let renderer = self.renderers.stdout;
        let (InferiorState::Stopped { thread_id, .. }, Some(stop)) =
            (&snapshot.inferior, snapshot.stop_id)
        else {
            return Vec::new();
        };

        let mut lines = Vec::new();

        for thread in snapshot
            .threads
            .iter()
            .filter(|thread| thread.id != *thread_id)
        {
            let ThreadState::Stopped {
                reason:
                    Some(reason @ (StopReason::Breakpoint { .. } | StopReason::Watchpoint { .. })),
            } = &thread.state
            else {
                continue;
            };

            let mut line = format!("thread {}", renderer.paint(Role::Metadata, thread.id));

            if let Some(ThreadActivity::Task { task, .. }) = &thread.activity {
                let _ = write!(
                    line,
                    " {}",
                    renderer.paint(Role::Metadata, format_args!("[{}]", task.number))
                );
            }

            let _ = write!(line, " also {}", format::stop(reason, renderer));
            let context = StopContext {
                stop,
                execution: ExecutionContext::Thread(thread.id),
                frame: StackFrameId::INNERMOST,
            };

            if let Ok(location) = self.debugger().at(context).location().await
                && let Ok(place) = self.describe(&location, false).await
            {
                let _ = write!(line, " in {place}");
            }

            lines.push(line);
        }

        lines
    }

    /// The sections `[stop] show` names, in order, each of which that
    /// fails giving its error, and the displays after them when the list
    /// leaves them out. At a stop, the header has said where.
    pub(super) async fn sections(&self, at_stop: bool) -> String {
        let renderer = self.renderers.stdout;
        let stop = &self.settings.config.stop;
        let variables = if stop.highlight_changes || stop.show.contains(&Section::Locals) {
            Some(self.debugger().variables().await)
        } else {
            None
        };

        let activation = match &variables {
            Some(Ok(variables)) if stop.highlight_changes => {
                self.activation(
                    variables.stop_id,
                    variables.context,
                    variables.frame_address,
                )
                .await
            }
            _ => None,
        };

        let activation = activation.as_ref();
        let mut parts = Vec::new();
        let mut sections = stop.show.clone();
        if !sections.contains(&Section::Displays) {
            sections.push(Section::Displays);
        }

        for section in sections {
            let part = match section {
                Section::Source => self.source_section(at_stop).await,
                Section::Locals => match &variables {
                    Some(Ok(variables)) => Ok(self.locals_section(variables, activation)),
                    Some(Err(error)) => Err(anyhow::anyhow!("{error}")),
                    None => Ok(String::new()),
                },
                Section::Displays => Ok(self.displays_section(activation).await),
                Section::Registers => self.registers_section(activation).await,
                Section::Disassembly => self.disassembly_section().await,
                Section::Backtrace => {
                    self.backtrace(
                        Some(stop.backtrace_frames as usize),
                        self.shows_runtime_frames(),
                    )
                    .await
                }
                Section::Threads => self
                    .debugger()
                    .snapshot()
                    .await
                    .map(|snapshot| format::threads(&snapshot, renderer))
                    .map_err(Into::into),
            };

            match part {
                Ok(text) if text.is_empty() => {}
                Ok(text) => parts.push(text),
                Err(error) => parts.push(format!(
                    "{}: {error:#}",
                    renderer.paint(
                        Role::Warning,
                        format!("{} unavailable", section_name(section))
                    )
                )),
            }
        }

        parts.join("\n")
    }

    /// The activation the selected frame is, when its function and frame
    /// address identify it.
    async fn activation(
        &self,
        stop: StopId,
        context: ExecutionContext,
        frame: Option<VirtualAddress>,
    ) -> Option<Activation> {
        let location = self.debugger().current_location().await.ok()?;
        Some(Activation {
            stop,
            context,
            function: format::code_name(
                location.image.function.as_ref(),
                location.image.symbol.as_ref(),
            ),
            frame: frame?,
        })
    }

    /// Whether `name`, shown as `line`, changed since the last stop.
    fn changed(&self, activation: Option<&Activation>, name: &str, line: &str) -> bool {
        activation.is_some_and(|activation| {
            self.changes
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .changed(activation, name, &plain(line))
        })
    }

    /// The source around the selected frame's line; at a stop, without the
    /// location the header gave, and nothing where no line is known.
    async fn source_section(&self, at_stop: bool) -> Result<String> {
        match self.source_context().await {
            Ok(context) => Ok(self.source_listing(&context, !at_stop).await),
            Err(microscope::Error::SourceLocationUnavailable) if at_stop => Ok(String::new()),
            Err(error) => Err(error.into()),
        }
    }

    fn locals_section(
        &self,
        variables: &microscope::VariableSnapshot,
        activation: Option<&Activation>,
    ) -> String {
        value::variables_marked(variables, self.renderers.stdout, &mut |name, line| {
            self.changed(activation, name, line)
        })
    }

    async fn displays_section(&self, activation: Option<&Activation>) -> String {
        let displays = self
            .displays
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .list
            .clone();

        let mut lines = Vec::with_capacity(displays.len());
        for display in &displays {
            lines.push(self.display_line(display, activation).await);
        }

        lines.join("\n")
    }

    /// A display's value, as `print` shows it after the display's number,
    /// or its error, dimmed.
    pub(super) async fn display_line(
        &self,
        display: &Display,
        activation: Option<&Activation>,
    ) -> String {
        let renderer = self.renderers.stdout;
        let id = renderer.paint(Role::Metadata, format!("{}:", display.id));

        let shown = match self.layout(Command::Print, &display.format) {
            Ok(layout) => self.print(&display.expression, layout, renderer).await,
            Err(error) => Err(error),
        };

        let line = match shown {
            Ok(line) => line,
            Err(error) => {
                return format!(
                    "{id} {}",
                    renderer.paint(Role::Muted, format!("{}: {error:#}", display.expression))
                );
            }
        };

        let name = format!("display {}", display.id);
        if !self.changed(activation, &name, &line) {
            return format!("{id} {line}");
        }

        if renderer.is_colored()
            && let Ok(layout) = self.layout(Command::Print, &display.format)
            && let Ok(changed) = self
                .print(&display.expression, layout, renderer.changed())
                .await
        {
            return format!("{id} {changed}");
        }

        format!("{id} {}*", plain(&line))
    }

    async fn registers_section(&self, activation: Option<&Activation>) -> Result<String> {
        let renderer = self.renderers.stdout;
        let registers = self.debugger().registers().await?;
        let text = format::registers(&registers, renderer);
        let mut changed_lines: Option<Vec<String>> = None;
        let lines = text
            .split('\n')
            .zip(registers.registers.iter())
            .enumerate()
            .map(|(index, (line, register))| {
                if register.bytes.is_none()
                    || !self.changed(activation, &register.register.name, line)
                {
                    return line.to_owned();
                }

                if !renderer.is_colored() {
                    return format!("{line}*");
                }

                changed_lines
                    .get_or_insert_with(|| {
                        format::registers(&registers, renderer.changed())
                            .split('\n')
                            .map(ToOwned::to_owned)
                            .collect()
                    })
                    .get(index)
                    .cloned()
                    .unwrap_or_else(|| line.to_owned())
            })
            .collect::<Vec<_>>();
        Ok(lines.join("\n"))
    }

    /// The instructions around the selected frame's, as many as
    /// `[stop] disassembly-instructions` says.
    async fn disassembly_section(&self) -> Result<String> {
        let (_, marked) = self.selected_code().await?;
        let count = self.settings.config.stop.disassembly_instructions;
        let before = count / 2;
        let disassembly = self
            .disassemble_query(microscope::DisassemblyRange::Window {
                address: marked,
                before,
                after: count - before,
            })
            .await?;

        self.render_disassembly(&disassembly, marked).await
    }

    /// Prints the stop's sections again, in the selected frame.
    pub(super) async fn context(&self) -> Result<String> {
        if self.debugger().snapshot().await?.stop_id.is_none() {
            anyhow::bail!("the program is not stopped");
        }

        Ok(self.sections(false).await)
    }

    /// Adds a display, showing its value when the program is stopped.
    pub(super) async fn add_display(&self, format: &str, expression: &str) -> Result<String> {
        // A display's format is checked once, when it is added.
        self.layout(Command::Print, format)?;

        if expression.is_empty() {
            anyhow::bail!("display takes an expression; `display` alone lists them");
        }

        microscope::Expression::parse(expression)
            .map_err(|error| anyhow::anyhow!(format::expression_error(expression, &error)))?;
        let display = self
            .displays
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .add(format, expression);

        let stopped = self
            .debugger()
            .snapshot()
            .await
            .is_ok_and(|snapshot| snapshot.stop_id.is_some());
        if stopped {
            return Ok(self.display_line(&display, None).await);
        }

        Ok(format!(
            "{} {}: {}",
            self.renderers.stdout.paint(Role::Success, "display"),
            self.renderers.stdout.paint(Role::Metadata, display.id),
            display.command()
        ))
    }

    /// Lists the displays as `display` takes them.
    pub(super) fn list_displays(&self) -> String {
        let renderer = self.renderers.stdout;
        let displays = self
            .displays
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

        if displays.list.is_empty() {
            return "no displays".to_owned();
        }

        displays
            .list
            .iter()
            .map(|display| {
                format!(
                    "{} {}",
                    renderer.paint(Role::Metadata, format!("{}:", display.id)),
                    display.command()
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Removes the displays `words` number, or every display for `all`.
    pub(super) fn undisplay(&self, words: &[&str], spec: &CommandSpec) -> Result<String> {
        let mut displays = self
            .displays
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let ids = super::commands::parse_display_ids(words, displays.list.len(), spec)?;
        let removed = displays.remove(ids.as_deref())?;
        drop(displays);

        Ok(format!(
            "{} {}",
            self.renderers.stdout.paint(Role::Success, "removed"),
            format::plural(removed as u64, "display")
        ))
    }
}
