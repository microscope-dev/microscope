//! The one debugging session a `microscope web` server holds, shared by every
//! connected tab.
//!
//! The session owns at most one [`Debugger`] at a time. Choosing something
//! else to debug ends the current one first: a launched program is killed
//! and an attached one detached, as [`Debugger::shutdown`] does. Each
//! debugger gets a new session id, so links to an earlier one say it ended
//! instead of opening something else.
//!
//! State is pushed: a task per debugger turns its events into whole
//! [`State`]s published through a watch channel, so a burst of events sends
//! only the newest state, and a lagged event stream costs nothing but a
//! fresh snapshot.

use std::collections::{BTreeMap, VecDeque};
use std::ffi::OsString;
use std::fmt::Write as _;
use std::os::fd::OwnedFd;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use microscope::{
    BreakpointOptions, CoreDumpOptions, Debugger, DebuggerEvent, DebuggerHandle, EvaluationMode,
    ExceptionDisposition, Expression, InferiorState, InspectionLimits, LaunchOptions, ProcessId,
    ResumeScope, StopId,
};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::unix::pipe;
use tokio::sync::{broadcast, watch};
use tokio::task::JoinHandle;

use super::auth::{Tokens, random_hex};
use super::describe::{Cause, Describer, Images};
use super::protocol::{
    self, Added, ConsoleResult, ErrorBody, ErrorKind, FrameAt, Inferior, Output, PathCompletions,
    Person, Presence, Processes, Request, Role, ServerMessage, ShareLink, State, StepKind, Stream,
    TargetKind,
};

use super::{draw, inspect, lowlevel, picker, values};
use crate::cli::format;
use crate::cli::terminal::Renderer;
use crate::cli::{Cli, LaunchSettings, Renderers};
use crate::dap::output;
use crate::present::{self, Code};

/// The most program output kept for tabs that join later.
const OUTPUT_HISTORY: usize = 256 * 1024;

/// The most bytes one `output` message carries.
const OUTPUT_CHUNK: usize = 32 * 1024;

/// How long writing the program's input may wait for it to read.
const INPUT_WAIT: Duration = Duration::from_secs(5);

/// How a launched program starts.
#[derive(Debug, Clone, Default)]
pub struct LaunchSpec {
    pub program: PathBuf,
    pub arguments: Vec<OsString>,
    pub environment: Vec<(OsString, OsString)>,
    pub cwd: Option<PathBuf>,
    pub stop_at_entry: bool,
}

/// What to debug.
#[derive(Debug, Clone)]
pub enum Start {
    Launch { spec: LaunchSpec, run: bool },
    Attach(ProcessId),
    Core(CoreDumpOptions),
}

/// A request's failure, as the page receives it.
pub struct Failure(ErrorBody);

impl Failure {
    pub fn new(kind: ErrorKind, message: impl Into<String>) -> Self {
        Self(ErrorBody {
            kind,
            message: message.into(),
        })
    }

    pub fn body(self) -> ErrorBody {
        self.0
    }
}

impl From<microscope::Error> for Failure {
    fn from(error: microscope::Error) -> Self {
        use microscope::Error;
        let kind = match error {
            Error::StaleStop => ErrorKind::StaleStop,
            Error::NotStopped
            | Error::NotRunning
            | Error::AlreadyRunning
            | Error::AlreadyStopped => ErrorKind::NotStopped,
            // A frame the interpreter or a suspended task holds has no
            // machine state, which is how such a frame is, not a failure.
            Error::PostMortemTarget
            | Error::FrameInterpreted
            | Error::FrameSuspended
            | Error::UnsupportedWatchAccess(..)
            | Error::DisassemblyUnsupported(..)
            | Error::HardwareWatchpointsUnavailable(..)
            | Error::WatchTargetUnsupported(_) => ErrorKind::Unsupported,
            Error::InvalidHitCondition(_)
            | Error::Expression(_)
            | Error::InvalidCondition(_)
            | Error::UnknownSignal(_)
            | Error::AddressOverflow
            | Error::MemoryReadTooLarge { .. }
            | Error::MemoryWriteTooLarge { .. }
            | Error::MemoryNotWritable(_)
            | Error::WatchpointNotFound(_)
            | Error::InvalidWatchRange { .. }
            | Error::WatchpointCapacity { .. }
            | Error::WatchTargetNotInMemory(_)
            | Error::WatchTargetUnavailable(_)
            | Error::InvalidDisassemblyWindow { .. }
            | Error::NoFunctionContainsAddress(_) => ErrorKind::Invalid,
            _ => ErrorKind::Failed,
        };

        Self::new(kind, error.to_string())
    }
}

type Answer = Result<Value, Failure>;

/// What a picker request produced: an answer, or a change to announce.
enum Chosen {
    Answer(Value),
    Notice(String),
}

/// The session every connection shares.
pub struct Session {
    cwd: PathBuf,
    home: Option<PathBuf>,

    /// The view files given for every program the session debugs, before
    /// the project's and the user's.
    views: Vec<PathBuf>,

    /// Where every program's separate debug files are found.
    debug_files: microscope::DebugFileOptions,

    /// What links begin with, such as `http://127.0.0.1:7341/`, or the
    /// public URL a proxy serves the page at.
    link_base: String,
    tokens: Tokens,
    default_name: String,

    /// The current debugger; locked across a change of what is debugged, so
    /// changes happen one at a time.
    target: tokio::sync::Mutex<Option<Target>>,
    state: Arc<watch::Sender<Arc<State>>>,

    /// Output, presence, and notices, already serialized.
    messages: broadcast::Sender<Arc<str>>,

    /// Recent output; its lock also orders publishing output, so a joining
    /// tab sees each piece exactly once.
    history: Arc<Mutex<History>>,
    people: Mutex<BTreeMap<u32, Person>>,
    next_connection: AtomicU32,

    /// Who last ran the program, for the stop the run ends at.
    cause: Arc<Mutex<Option<Cause>>>,

    /// Each connection's handles to values it was shown.
    handles: values::Handles,

    /// Whether a process attached to runs on at once rather than staying
    /// stopped at the attach.
    resume_attached: AtomicBool,
    /// Whether `share` makes links; see [`Session::stop_sharing`].
    share_links: AtomicBool,
}

struct Target {
    /// The session id, which a new target replaces.
    id: String,
    debugger: Option<Debugger>,
    handle: DebuggerHandle,
    images: Arc<Images>,
    launch: Option<LaunchSpec>,

    /// The launched program's standard input, until closed.
    input: Arc<tokio::sync::Mutex<Option<pipe::Sender>>>,
    pump: JoinHandle<()>,
    readers: Vec<JoinHandle<()>>,
    name: String,

    /// Runs the console's commands, one at a time: each selects the tab's
    /// frame first, so the selection never changes under another's.
    console: Arc<tokio::sync::Mutex<Cli>>,

    /// Counts the changes tabs make to the program's values.
    writes: Arc<AtomicU64>,

    /// Counts changes to settings no event announces, such as signal
    /// policies.
    settings: Arc<AtomicU64>,
}

#[derive(Default)]
struct History {
    pieces: VecDeque<Arc<str>>,
    bytes: usize,
}

/// What a joining connection receives before anything else.
pub struct Joined {
    pub connection: u32,
    pub name: String,
    pub history: Vec<Arc<str>>,
    pub messages: broadcast::Receiver<Arc<str>>,
    pub state: watch::Receiver<Arc<State>>,
}

impl Session {
    pub fn new(
        cwd: PathBuf,
        link_base: String,
        tokens: Tokens,
        debug_files: microscope::DebugFileOptions,
        views: Vec<PathBuf>,
    ) -> Arc<Self> {
        let (state, _) = watch::channel(Arc::new(State::idle(None)));
        let (messages, _) = broadcast::channel(1024);
        let default_name = std::env::var("USER")
            .ok()
            .filter(|name| !name.is_empty())
            .unwrap_or_else(|| "you".to_owned());

        Arc::new(Self {
            cwd,
            home: std::env::var_os("HOME").map(PathBuf::from),
            views,
            debug_files,
            link_base,
            tokens,
            default_name,
            target: tokio::sync::Mutex::new(None),
            state: Arc::new(state),
            messages,
            history: Arc::default(),
            people: Mutex::default(),
            next_connection: AtomicU32::new(1),
            cause: Arc::default(),
            handles: values::Handles::default(),
            resume_attached: AtomicBool::new(false),
            share_links: AtomicBool::new(true),
        })
    }

    /// Stops making links that share the session, as behind a trusted
    /// proxy, whose links are its own.
    pub fn stop_sharing(&self) {
        self.share_links.store(false, Ordering::Relaxed);
    }

    pub fn shares(&self) -> bool {
        self.share_links.load(Ordering::Relaxed)
    }

    /// Lets every process attached to from now on run on at once: a
    /// service someone inspects shouldn't wait for a tab to continue it.
    pub fn resume_attached(&self, resume: bool) {
        self.resume_attached.store(resume, Ordering::Relaxed);
    }

    pub const fn tokens(&self) -> &Tokens {
        &self.tokens
    }

    pub fn cwd(&self) -> &Path {
        &self.cwd
    }

    /// The link that joins this server with `role`.
    pub fn join_link(&self, role: Role, to: &str) -> String {
        let to = if to == "/" || to.is_empty() {
            String::new()
        } else {
            format!("?to={}", escape_query(to))
        };

        format!("{}join{to}#{}", self.link_base, self.tokens.token(role))
    }

    /// Adds a connection to the presence list.
    pub fn join(&self, role: Role) -> Joined {
        let connection = self.next_connection.fetch_add(1, Ordering::Relaxed);
        let name = match role {
            Role::Control => self.default_name.clone(),
            Role::View => format!("guest-{connection}"),
        };

        let (history, messages) = {
            let history = self.history.lock().unwrap_or_else(PoisonError::into_inner);
            (
                history.pieces.iter().cloned().collect(),
                self.messages.subscribe(),
            )
        };

        let state = self.state.subscribe();

        self.people
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(
                connection,
                Person {
                    connection,
                    name: name.clone(),
                    role,
                    focus: None,
                },
            );
        self.announce_presence();

        Joined {
            connection,
            name,
            history,
            messages,
            state,
        }
    }

    pub fn leave(&self, connection: u32) {
        self.handles.forget(connection);
        self.people
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&connection);
        self.announce_presence();
    }

    fn announce_presence(&self) {
        let people = self
            .people
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .values()
            .cloned()
            .collect();
        self.broadcast(&ServerMessage::Presence(Presence { people }));
    }

    fn broadcast(&self, message: &ServerMessage) {
        let text = encode(message);
        let _ = self.messages.send(text.into());
    }

    fn name_of(&self, connection: u32) -> String {
        self.people
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&connection)
            .map(|person| person.name.clone())
            .unwrap_or_default()
    }

    fn notice(&self, connection: u32, text: impl Into<String>) {
        self.broadcast(&ServerMessage::Notice(protocol::Notice {
            connection,
            name: self.name_of(connection),
            text: text.into(),
        }));
    }

    /// The inputs of a drawing, and the bytes their numbers and bytes lie
    /// in, which go to the tab before the answer.
    pub async fn draw(&self, request: &protocol::Draw) -> Result<(Value, Vec<u8>), Failure> {
        let handle = self.current_handle().await?;
        let drawn = draw::draw(&handle, request).await?;
        Ok((to_value(&drawn.drawing), drawn.bytes))
    }

    /// Answers a request about the renderers drawings name.
    async fn renderers(&self, request: Request) -> Answer {
        match request {
            Request::Renderer(protocol::RendererRef { digest }) => Ok(to_value(
                &draw::renderer(&self.current_handle().await?, &digest).await?,
            )),
            Request::Renderers => Ok(to_value(
                &draw::renderers(&self.current_handle().await?).await?,
            )),
            _ => Err(Failure::new(
                ErrorKind::Failed,
                "internal: only reads are read",
            )),
        }
    }

    /// Reads the session's view files, and the renderers beside them,
    /// again, and has every tab read its values again.
    async fn reload_views(&self, connection: u32) -> Result<(), Failure> {
        let console = self
            .target
            .lock()
            .await
            .as_ref()
            .map(|target| Arc::clone(&target.console))
            .ok_or_else(|| Failure::new(ErrorKind::NotStopped, "nothing is being debugged"))?;
        let warnings = console
            .lock()
            .await
            .load_view_sources(&self.cwd, &self.views)
            .await;

        self.count(
            |target| &target.settings,
            |state, count| state.settings = count,
        )
        .await;
        self.notice(
            connection,
            if warnings.is_empty() {
                "reloaded the views".to_owned()
            } else {
                format!("reloaded the views: {}", warnings.join("; "))
            },
        );
        Ok(())
    }

    /// Serves one request from `connection`.
    pub async fn handle(&self, connection: u32, role: Role, request: Request) -> Answer {
        let reads = matches!(
            request,
            Request::SetName(_)
                | Request::Share(_)
                | Request::SetFocus(_)
                | Request::Backtrace(_)
                | Request::Sources
                | Request::Source(_)
                | Request::Scopes(_)
                | Request::Children(_)
                | Request::Evaluate(_)
                | Request::Complete(_)
                | Request::Console(_)
                | Request::Disassemble(_)
                | Request::ReadMemory(_)
                | Request::Registers(_)
                | Request::StepTargets(_)
                | Request::Signals
                | Request::Exceptions
                | Request::Modules
                | Request::Functions(_)
                | Request::Tasks(_)
                | Request::Renderer(_)
                | Request::Renderers
        );
        if !reads && role != Role::Control {
            return Err(Failure::new(
                ErrorKind::Forbidden,
                "this link can only view the session; ask for a control link",
            ));
        }

        if reads {
            return self.read(connection, role, request).await;
        }

        let notice = match request {
            Request::CompletePath(_)
            | Request::Processes
            | Request::Launch(_)
            | Request::Attach(_)
            | Request::OpenCore(_)
            | Request::End => match self.choose(connection, request).await? {
                Chosen::Answer(answer) => return Ok(answer),
                Chosen::Notice(text) => Some(text),
            },
            Request::AddBreakpoint(_)
            | Request::EditBreakpoint(_)
            | Request::RemoveBreakpoint(_) => {
                return self.breakpoints(connection, request).await;
            }

            Request::WriteMemory(_)
            | Request::AddWatchpoint(_)
            | Request::EditWatchpoint(_)
            | Request::RemoveWatchpoint(_)
            | Request::SetSignal(_) => return self.low_level(connection, request).await,
            Request::SetException(choice) => Some(self.set_exception(&choice).await?),
            Request::ReloadViews => {
                self.reload_views(connection).await?;
                return Ok(Value::Null);
            }
            Request::SetValue(set) => {
                let row = self
                    .reader(connection)
                    .await?
                    .set_value(set.at, &set.path, &set.value)
                    .await?;
                self.wrote().await;
                self.notice(
                    connection,
                    format!("set {} = {}", set.path, set.value.trim()),
                );

                return Ok(to_value(&row));
            }
            request => self.control(connection, request).await?,
        };

        if let Some(text) = notice {
            self.notice(connection, text);
        }

        Ok(Value::Null)
    }

    /// Answers a request that changes nothing in the program.
    async fn read(&self, connection: u32, role: Role, request: Request) -> Answer {
        match request {
            Request::SetName(protocol::SetName { name }) => {
                self.rename(connection, &name)?;
                Ok(Value::Null)
            }
            Request::SetFocus(protocol::SetFocus { focus }) => {
                self.refocus(connection, focus)?;
                Ok(Value::Null)
            }
            Request::Share(protocol::Share { role: wanted, to }) => self.share(role, wanted, &to),
            Request::Backtrace(at) => {
                let (handle, images) = self.current_images().await?;
                let selected = self.stop_frame(&at);
                Ok(to_value(
                    &inspect::backtrace(&handle, &images, at, selected).await?,
                ))
            }
            Request::Tasks(at) => {
                let (handle, images) = self.current_images().await?;
                Ok(to_value(&inspect::tasks(&handle, &images, at).await?))
            }
            Request::Sources => {
                let (_, images) = self.current_images().await?;
                let python = self.python_project().await;
                Ok(to_value(&inspect::sources(&images, python).await))
            }
            Request::Functions(query) => {
                let (_, images) = self.current_images().await?;
                Ok(to_value(&inspect::functions(&images, &query).await?))
            }
            Request::Source(protocol::SourcePath { path }) => {
                let (handle, images) = self.current_images().await?;
                let python = self.python_project().await;
                Ok(to_value(
                    &inspect::source(&handle, &images, python.as_ref(), &path).await?,
                ))
            }
            Request::Scopes(at) => Ok(to_value(&self.reader(connection).await?.scopes(at).await?)),
            Request::Children(of) => Ok(to_value(&protocol::Rows {
                rows: self.reader(connection).await?.children(&of).await?,
            })),
            Request::Evaluate(evaluate) => Ok(to_value(
                &self
                    .reader(connection)
                    .await?
                    .evaluate(evaluate.at, &evaluate.expression, EvaluationMode::Read)
                    .await?,
            )),
            Request::Complete(complete) => {
                let at = frame_at(
                    complete.stop,
                    complete.thread,
                    complete.task,
                    complete.frame,
                );
                Ok(to_value(
                    &self
                        .reader(connection)
                        .await?
                        .complete(&complete.text, at)
                        .await?,
                ))
            }
            Request::Console(line) => Ok(to_value(&self.console(connection, role, line).await?)),
            Request::Disassemble(disassemble) => {
                let (handle, images) = self.current_images().await?;
                Ok(to_value(
                    &lowlevel::disassemble(&handle, &images, &disassemble).await?,
                ))
            }
            Request::ReadMemory(read) => Ok(to_value(
                &lowlevel::read_memory(&self.current_handle().await?, &read).await?,
            )),
            Request::StepTargets(at) => Ok(to_value(&self.step_targets(&at).await?)),
            Request::Registers(at) => Ok(to_value(&protocol::Registers {
                registers: lowlevel::registers(&self.current_handle().await?, at).await?,
            })),
            Request::Signals => Ok(to_value(&protocol::Signals {
                signals: lowlevel::signals(&self.current_handle().await?).await?,
            })),
            Request::Exceptions => Ok(to_value(&self.exceptions().await?)),
            Request::Modules => {
                let (handle, images) = self.current_images().await?;
                Ok(to_value(&protocol::Modules {
                    modules: lowlevel::modules(&handle, &images).await?,
                }))
            }
            request => self.renderers(request).await,
        }
    }

    /// Serves the picker: what there is to debug, and choosing it.
    async fn choose(&self, connection: u32, request: Request) -> Result<Chosen, Failure> {
        Ok(Chosen::Notice(match request {
            Request::CompletePath(protocol::CompletePath { text }) => {
                let entries = picker::complete(&text, &self.cwd, self.home.as_deref());
                return Ok(Chosen::Answer(to_value(&PathCompletions { entries })));
            }
            Request::Processes => {
                return Ok(Chosen::Answer(to_value(&Processes {
                    processes: picker::processes(Path::new("/proc")),
                    ptrace_scope: picker::ptrace_scope(),
                })));
            }
            Request::Launch(launch) => {
                let (replace, run) = (launch.replace, launch.run);
                let spec = self.launch_spec(launch);
                let name = spec.program.display().to_string();
                self.choose_target(connection, Start::Launch { spec, run }, replace)
                    .await?;
                format!("launched {name}")
            }
            Request::Attach(protocol::Attach { pid, replace }) => {
                self.choose_target(connection, Start::Attach(ProcessId::new(pid)), replace)
                    .await?;
                format!("attached to process {pid}")
            }

            Request::OpenCore(protocol::OpenCore {
                core,
                executable,
                replace,
            }) => {
                let resolve = |text: &str| picker::resolve(text, &self.cwd, self.home.as_deref());
                let mut options = CoreDumpOptions::new(resolve(&core));
                options.executable = executable
                    .filter(|path| !path.is_empty())
                    .map(|path| resolve(&path));
                self.choose_target(connection, Start::Core(options), replace)
                    .await?;
                format!("opened {core}")
            }
            Request::End => self.end_current().await?,
            _ => {
                return Err(Failure::new(
                    ErrorKind::Failed,
                    "internal: only the picker's requests choose",
                ));
            }
        }))
    }

    /// Starts what the picker chose. Tabs on a session that a failed
    /// replacement ended learn why it ended.
    async fn choose_target(
        &self,
        connection: u32,
        start: Start,
        replace: bool,
    ) -> Result<(), Failure> {
        self.replace(start, replace)
            .await
            .map_err(|(failure, ended)| {
                if let Some(name) = ended {
                    self.notice(
                        connection,
                        format!("ended the {name} session, but {}", failure.0.message),
                    );
                }

                failure
            })
    }

    /// Runs, stops, or feeds the program, returning what to tell others.
    async fn control(&self, connection: u32, request: Request) -> Result<Option<String>, Failure> {
        Ok(Some(match request {
            Request::Continue(protocol::Continue { stop }) => self.resume(connection, stop).await?,
            Request::Step(step) => self.step(connection, step).await?,
            Request::Jump(jump) => self.jump(connection, jump).await?,
            Request::Pause => {
                let handle = self.current_handle().await?;
                self.caused(connection, "paused");
                handle.pause().await?;
                "paused".to_owned()
            }
            Request::Kill => {
                self.current_handle().await?.kill().await?;
                "killed the program".to_owned()
            }
            Request::Restart => {
                self.restart(connection).await?;
                "restarted the program".to_owned()
            }
            Request::Input(input) => {
                self.input(input).await?;
                return Ok(None);
            }
            _ => {
                return Err(Failure::new(
                    ErrorKind::Failed,
                    "internal: only run control controls",
                ));
            }
        }))
    }

    async fn breakpoints(&self, connection: u32, request: Request) -> Answer {
        let handle = self.current_handle().await?;

        match request {
            Request::AddBreakpoint(add) => {
                let spec = crate::cli::commands::parse_breakpoint_location(add.location.trim())
                    .map_err(|error| Failure::new(ErrorKind::Invalid, format!("{error:#}")))?
                    .ok_or_else(|| {
                        Failure::new(
                            ErrorKind::Invalid,
                            "a breakpoint goes at a function, FILE:LINE, FILE:FUNCTION, or 0xADDRESS",
                        )
                    })?;
                let options = BreakpointOptions {
                    pending: crate::cli::commands::is_python(&spec),
                    ..options(add.condition, add.hit_condition, add.log_message)?
                };

                let breakpoint =
                    handle
                        .add_breakpoint_with(spec, options)
                        .await
                        .map_err(|error| {
                            let hint = match &error {
                                microscope::Error::FunctionNotFound(name) => {
                                    crate::cli::commands::python_name_hint(
                                        name,
                                        handle.module_image(),
                                    )
                                }
                                _ => None,
                            };
                            match hint {
                                Some(hint) => {
                                    Failure::new(ErrorKind::Invalid, format!("{error}; {hint}"))
                                }
                                None => Failure::from(error),
                            }
                        })?;

                self.notice(connection, format!("set breakpoint {}", breakpoint.id));
                return Ok(to_value(&Added {
                    id: breakpoint.id.get(),
                }));
            }
            Request::EditBreakpoint(edit) => edit_breakpoint(&handle, edit).await?,
            Request::RemoveBreakpoint(protocol::BreakpointRef { id }) => {
                handle
                    .remove_breakpoint(microscope::BreakpointId::new(id))
                    .await?;
                self.notice(connection, format!("removed breakpoint {id}"));
            }
            _ => {
                return Err(Failure::new(
                    ErrorKind::Failed,
                    "internal: only breakpoint requests change breakpoints",
                ));
            }
        }

        Ok(Value::Null)
    }

    /// Changes memory, watchpoints, or signal policies, and says so.
    async fn low_level(&self, connection: u32, request: Request) -> Answer {
        let handle = self.current_handle().await?;

        let notice = match request {
            Request::WriteMemory(write) => {
                let written = lowlevel::write_memory(&handle, &write).await?;
                self.wrote().await;
                format!("wrote {written} bytes at {}", write.address)
            }
            Request::AddWatchpoint(add) => {
                let id = lowlevel::add_watchpoint(&handle, add).await?;
                self.notice(connection, format!("set watchpoint {id}"));
                return Ok(to_value(&Added { id }));
            }
            Request::EditWatchpoint(edit) => {
                let id = edit.id;
                lowlevel::edit_watchpoint(&handle, edit).await?;
                format!("changed watchpoint {id}")
            }
            Request::RemoveWatchpoint(protocol::WatchpointRef { id }) => {
                handle
                    .remove_watchpoint(microscope::WatchpointId::new(id))
                    .await?;
                format!("removed watchpoint {id}")
            }
            Request::SetSignal(policy) => {
                let name = lowlevel::set_signal(&handle, &policy).await?;
                self.count(
                    |target| &target.settings,
                    |state, count| state.settings = count,
                )
                .await;
                format!("changed how {name} is handled")
            }
            _ => {
                return Err(Failure::new(
                    ErrorKind::Failed,
                    "internal: only low-level requests change the program below its source",
                ));
            }
        };

        self.notice(connection, notice);
        Ok(Value::Null)
    }

    /// Records who runs the program next, and how.
    fn caused(&self, connection: u32, action: &str) {
        *self.cause.lock().unwrap_or_else(PoisonError::into_inner) = Some(Cause {
            name: self.name_of(connection),
            action: action.to_owned(),
        });
    }

    async fn step(&self, connection: u32, step: protocol::Step) -> Result<String, Failure> {
        let handle = self.current_handle().await?;
        let InferiorState::Stopped { process_id, .. } = handle.snapshot().await?.inferior else {
            return Err(Failure::new(
                ErrorKind::NotStopped,
                "the program is not stopped",
            ));
        };

        let (kind, action) = match step.kind {
            StepKind::Over => (microscope::StepKind::OverSource, "stepped over"),
            StepKind::Into => (microscope::StepKind::IntoSource, "stepped into"),
            StepKind::Out => (microscope::StepKind::Out, "stepped out"),
            StepKind::Instruction => (microscope::StepKind::Instruction, "stepped an instruction"),
            StepKind::OverInstruction => (
                microscope::StepKind::OverInstruction,
                "stepped over an instruction",
            ),
        };

        let frame = if step.kind == StepKind::Out {
            step.frame
        } else {
            0
        };

        let context = inspect::context(
            &handle,
            step.stop,
            protocol::execution(step.thread, step.task),
            frame,
        )
        .await?;
        let scope = ResumeScope::Process(process_id);

        if let Some(call) = step.call.as_deref().filter(|_| step.kind == StepKind::Into) {
            let call = lowlevel::address(call)?;
            let action = format!("stepped into the call at {call}");
            self.caused(connection, &action);
            handle
                .start_step_into(
                    context.stop,
                    context.execution,
                    call,
                    scope,
                    ExceptionDisposition::Pass,
                )
                .await?;
            return Ok(action);
        }

        self.caused(connection, action);
        handle
            .start_step(
                context.stop,
                context.execution,
                context.frame,
                kind,
                scope,
                ExceptionDisposition::Pass,
            )
            .await?;
        Ok(action.to_owned())
    }

    async fn jump(&self, connection: u32, jump: protocol::Jump) -> Result<String, Failure> {
        let handle = self.current_handle().await?;
        let location = jump.location.trim();
        let spec = crate::cli::commands::parse_breakpoint_location(location)
            .map_err(|error| Failure::new(ErrorKind::Invalid, format!("{error:#}")))?
            .ok_or_else(|| {
                Failure::new(
                    ErrorKind::Invalid,
                    "a thread moves to FILE:LINE, FILE:FUNCTION, or 0xADDRESS",
                )
            })?;
        let context = inspect::context(
            &handle,
            jump.stop,
            protocol::execution(jump.thread, jump.task),
            0,
        )
        .await?;
        let action = format!("moved thread {} to {location}", jump.thread);
        self.caused(connection, &action);

        handle
            .start_jump(context.stop, context.execution, spec)
            .await?;

        Ok(action)
    }

    async fn input(&self, input: protocol::Input) -> Result<(), Failure> {
        let writer = self
            .target
            .lock()
            .await
            .as_ref()
            .map(|target| Arc::clone(&target.input))
            .ok_or_else(|| Failure::new(ErrorKind::Invalid, "nothing is being debugged"))?;
        let mut open = writer.lock().await;
        let Some(pipe) = open.as_mut() else {
            return Err(Failure::new(
                ErrorKind::Invalid,
                "the program's input is closed; it reads input only while it runs",
            ));
        };

        let written = tokio::time::timeout(INPUT_WAIT, pipe.write_all(input.text.as_bytes()))
            .await
            .map_err(|_| Failure::new(ErrorKind::Failed, "the program is not reading its input"))?;
        if let Err(error) = written {
            *open = None;
            return Err(Failure::new(
                ErrorKind::Failed,
                format!("the program's input closed: {error}"),
            ));
        }

        if input.eof {
            *open = None;
        }

        drop(open);
        Ok(())
    }

    fn rename(&self, connection: u32, name: &str) -> Result<(), Failure> {
        let name = name.trim();
        if name.is_empty() || name.chars().count() > 40 {
            return Err(Failure::new(
                ErrorKind::Invalid,
                "a name has 1 to 40 characters",
            ));
        }

        if let Some(person) = self
            .people
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get_mut(&connection)
        {
            name.clone_into(&mut person.name);
        }

        self.announce_presence();
        Ok(())
    }

    fn refocus(&self, connection: u32, focus: Option<protocol::Focus>) -> Result<(), Failure> {
        if focus.as_ref().is_some_and(|focus| {
            !is_page_path(&focus.url) || focus.url.len() > 4096 || focus.label.len() > 200
        }) {
            return Err(Failure::new(
                ErrorKind::Invalid,
                "a focus is a page path and a short label",
            ));
        }

        let changed = self
            .people
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get_mut(&connection)
            .is_some_and(|person| {
                let changed = person.focus != focus;
                person.focus = focus;
                changed
            });
        if changed {
            self.announce_presence();
        }

        Ok(())
    }

    fn share(&self, role: Role, wanted: Role, to: &str) -> Answer {
        if !self.shares() {
            return Err(Failure::new(
                ErrorKind::Forbidden,
                "this server's pages are shared by the proxy in front of it",
            ));
        }

        if wanted == Role::Control && role != Role::Control {
            return Err(Failure::new(
                ErrorKind::Forbidden,
                "only control access can share control",
            ));
        }

        if !is_page_path(to) {
            return Err(Failure::new(ErrorKind::Invalid, "a link opens a page path"));
        }

        Ok(to_value(&ShareLink {
            url: self.join_link(wanted, to),
        }))
    }

    fn launch_spec(&self, launch: protocol::Launch) -> LaunchSpec {
        let resolve = |text: &str| picker::resolve(text, &self.cwd, self.home.as_deref());
        LaunchSpec {
            program: resolve(&launch.program),
            arguments: launch.arguments.into_iter().map(OsString::from).collect(),
            environment: launch
                .environment
                .into_iter()
                .map(|(name, value)| (name.into(), value.into()))
                .collect(),
            cwd: launch
                .cwd
                .filter(|cwd| !cwd.is_empty())
                .map(|cwd| resolve(&cwd)),
            stop_at_entry: launch.stop_at_entry,
        }
    }

    async fn end_current(&self) -> Result<String, Failure> {
        let current = self.target.lock().await.take();
        let Some(current) = current else {
            return Err(Failure::new(
                ErrorKind::Invalid,
                "nothing is being debugged",
            ));
        };

        let name = current.name.clone();
        let ended = self.end(current).await;
        self.state.send_replace(Arc::new(State::idle(None)));
        ended?;
        Ok(format!("ended the {name} session"))
    }

    /// Continues the stop named, or starts a launched program that is not
    /// running.
    async fn resume(&self, connection: u32, stop: Option<u64>) -> Result<String, Failure> {
        let (handle, launch, started) = self.current_launch().await?;
        if let Some(stop) = stop {
            let InferiorState::Stopped { process_id, .. } = handle.snapshot().await?.inferior
            else {
                return Err(Failure::new(
                    ErrorKind::NotStopped,
                    "the program is not stopped",
                ));
            };

            self.caused(connection, "continued");
            handle
                .continue_execution(
                    StopId::new(stop),
                    ResumeScope::Process(process_id),
                    ExceptionDisposition::Pass,
                )
                .await?;
            return Ok("continued".to_owned());
        }

        let Some(spec) = launch else {
            return Err(Failure::new(
                ErrorKind::Invalid,
                "name the stop to continue",
            ));
        };

        if started {
            return Err(Failure::new(
                ErrorKind::NotStopped,
                "the program has already started; name the stop to continue",
            ));
        }

        self.caused(connection, "started the program");
        self.run(&handle, &spec).await?;
        Ok("started the program".to_owned())
    }

    async fn restart(&self, connection: u32) -> Result<(), Failure> {
        let (handle, launch, _) = self.current_launch().await?;
        let Some(spec) = launch else {
            return Err(Failure::new(
                ErrorKind::Unsupported,
                "only a launched program can restart",
            ));
        };

        match handle.kill().await {
            Ok(()) | Err(microscope::Error::NotRunning) => {}
            Err(error) => return Err(error.into()),
        }

        self.caused(connection, "restarted the program");
        self.run(&handle, &spec).await
    }

    async fn current_images(&self) -> Result<(DebuggerHandle, Arc<Images>), Failure> {
        self.target
            .lock()
            .await
            .as_ref()
            .map(|target| (target.handle.clone(), Arc::clone(&target.images)))
            .ok_or_else(|| Failure::new(ErrorKind::Invalid, "nothing is being debugged"))
    }

    /// Where a launched Python program's own files are, as its interpreter
    /// begins `sys.path`; none for any other program, or one attached to.
    async fn python_project(&self) -> Option<crate::present::python_files::Project> {
        let (handle, spec) = self
            .target
            .lock()
            .await
            .as_ref()
            .and_then(|target| Some((target.handle.clone(), target.launch.clone()?)))?;
        if !crate::present::debug_information::interprets_python(handle.module_image()) {
            return None;
        }

        let cwd = match spec.cwd {
            Some(cwd) => cwd,
            None => std::env::current_dir().ok()?,
        };
        // The program's environment is this one, with the launch's changes.
        let pythonpath = spec
            .environment
            .iter()
            .rev()
            .find(|(name, _)| name == "PYTHONPATH")
            .map(|(_, value)| value.clone())
            .or_else(|| std::env::var_os("PYTHONPATH"));

        Some(crate::present::python_files::Project::of(
            &spec.arguments,
            &cwd,
            pythonpath.as_deref(),
        ))
    }

    async fn current_handle(&self) -> Result<DebuggerHandle, Failure> {
        self.target
            .lock()
            .await
            .as_ref()
            .map(|target| target.handle.clone())
            .ok_or_else(|| Failure::new(ErrorKind::Invalid, "nothing is being debugged"))
    }

    /// The calls on the line a thread stopped at, which a step can go into.
    async fn step_targets(
        &self,
        at: &protocol::ThreadAt,
    ) -> Result<protocol::StepTargets, Failure> {
        let handle = self.current_handle().await?;
        let context = inspect::context(&handle, at.stop, at.execution(), 0).await?;
        let targets = handle.at(context).step_targets().await?;
        let hex = |address: microscope::VirtualAddress| format!("{:#x}", address.get());

        Ok(protocol::StepTargets {
            calls: targets
                .iter()
                .map(|target| protocol::StepCall {
                    call: hex(target.call),
                    callee: target.callee.as_deref().map(str::to_owned),
                    target: target.target.map(hex),
                })
                .collect(),
        })
    }

    /// The frame the stop `at` names selected on its thread, which the
    /// thread's frames fold around; a task's stack selects none of its own.
    fn stop_frame(&self, at: &protocol::ThreadAt) -> u32 {
        if at.task.is_some() {
            return 0;
        }

        self.state
            .borrow()
            .stops
            .iter()
            .find(|entry| entry.stop == at.stop && entry.thread == at.thread)
            .map_or(0, |entry| entry.frame)
    }

    /// The exceptions runtimes report, and whether each stops, as this
    /// session's console keeps them.
    async fn exceptions(&self) -> Result<protocol::Exceptions, Failure> {
        let console = self.shared_console().await?;
        let stops = console.lock().await.exception_stops();

        Ok(protocol::Exceptions {
            filters: microscope::ExceptionStops::filters()
                .iter()
                .map(|filter| protocol::ExceptionFilter {
                    id: filter.id.to_owned(),
                    language: filter.language.to_owned(),
                    label: filter.label.to_owned(),
                    description: filter.description.to_owned(),
                    on: stops.stops(filter.id),
                })
                .collect(),
        })
    }

    /// Chooses whether one filter's exceptions stop, as the console's
    /// `catch` does, and says so.
    async fn set_exception(&self, choice: &protocol::ExceptionChoice) -> Result<String, Failure> {
        let console = self.shared_console().await?;
        console
            .lock()
            .await
            .set_exception_stop(&choice.id, choice.on)
            .await
            .map_err(|error| Failure::new(ErrorKind::Invalid, format!("{error:#}")))?;
        self.count(
            |target| &target.settings,
            |state, count| state.settings = count,
        )
        .await;

        Ok(format!(
            "{} stopping at {} exceptions",
            if choice.on { "started" } else { "stopped" },
            choice.id
        ))
    }

    /// The session's console, which keeps what `catch` chose.
    async fn shared_console(&self) -> Result<Arc<tokio::sync::Mutex<Cli>>, Failure> {
        self.target
            .lock()
            .await
            .as_ref()
            .map(|target| Arc::clone(&target.console))
            .ok_or_else(|| Failure::new(ErrorKind::Invalid, "nothing is being debugged"))
    }

    /// Reads values for `connection` from the current session.
    async fn reader(&self, connection: u32) -> Result<values::Reader<'_>, Failure> {
        let (handle, images, session) = self
            .target
            .lock()
            .await
            .as_ref()
            .map(|target| {
                (
                    target.handle.clone(),
                    Arc::clone(&target.images),
                    target.id.clone(),
                )
            })
            .ok_or_else(|| Failure::new(ErrorKind::Invalid, "nothing is being debugged"))?;
        let code = Code::new(images.loaded().await);

        Ok(values::Reader {
            handle,
            images,
            code,
            handles: &self.handles,
            connection,
            session,
        })
    }

    /// Tells every tab that values it read at this stop may have changed.
    async fn wrote(&self) {
        self.count(|target| &target.writes, |state, count| state.writes = count)
            .await;
    }

    /// Bumps one of the target's counters into the state, so tabs that read
    /// what it counts read again.
    async fn count(&self, counter: fn(&Target) -> &Arc<AtomicU64>, set: fn(&mut State, u64)) {
        let Some(counter) = self
            .target
            .lock()
            .await
            .as_ref()
            .map(|target| Arc::clone(counter(target)))
        else {
            return;
        };

        counter.fetch_add(1, Ordering::Relaxed);

        self.state.send_modify(|state| {
            let mut next = State::clone(state);

            // Read under the state's lock, so that a count bumped after
            // this one and published first is not published back down.
            set(&mut next, counter.load(Ordering::Relaxed));
            *state = Arc::new(next);
        });
    }

    /// Runs a console line: an expression, unless it is a command the
    /// frame does not know as a name, so that `x`, `n`, or `list` read as
    /// themselves. Viewers evaluate without assigning, and run no commands.
    async fn console(
        &self,
        connection: u32,
        role: Role,
        line: protocol::ConsoleLine,
    ) -> Result<ConsoleResult, Failure> {
        let text = line.line.trim();
        let at = frame_at(line.stop, line.thread, line.task, line.frame);
        let command = crate::cli::commands::line_command(text);
        let expression = match (Expression::parse(text), command) {
            (Ok(expression), _) => expression,
            (Err(_), Some(_)) => return self.command(role, text, at).await,
            (Err(failure), None) => {
                return Err(Failure::new(
                    ErrorKind::Invalid,
                    crate::cli::format::expression_error(text, &failure),
                ));
            }
        };

        let Some(at) = at else {
            if command.is_some() {
                return self.command(role, text, at).await;
            }

            return Err(Failure::new(
                ErrorKind::NotStopped,
                "expressions need a stopped program",
            ));
        };

        let assigns = expression.assignment_target().is_some();
        if assigns && role != Role::Control {
            return Err(Failure::new(
                ErrorKind::Forbidden,
                "this link can only view the session; it cannot change values",
            ));
        }

        let reader = self.reader(connection).await?;
        let context = inspect::context(&reader.handle, at.stop, at.execution(), at.frame).await?;
        let mode = if role == Role::Control {
            EvaluationMode::Assign
        } else {
            EvaluationMode::Read
        };

        let evaluation = match reader
            .handle
            .at(context)
            .evaluate_with(&expression, mode, InspectionLimits::default())
            .await
        {
            Ok(evaluation) => evaluation,
            // The frame does not know the command's name, so it is the command.
            Err(microscope::Error::Expression(failure))
                if command.is_some_and(|(_, name)| present::names_only(&failure, name)) =>
            {
                drop(reader);
                return self.command(role, text, Some(at)).await;
            }
            Err(microscope::Error::Expression(failure)) => {
                return Err(Failure::new(
                    ErrorKind::Invalid,
                    crate::cli::format::expression_error(text, &failure),
                ));
            }
            Err(other) => return Err(other.into()),
        };

        let row = reader.present(context, text, expression, evaluation)?;
        if assigns {
            self.wrote().await;
            self.notice(connection, format!("ran {text}"));
        }

        Ok(ConsoleResult {
            output: None,
            row: Some(row),
        })
    }

    /// Runs one of microscope's commands in the frame `at`.
    async fn command(
        &self,
        role: Role,
        text: &str,
        at: Option<FrameAt>,
    ) -> Result<ConsoleResult, Failure> {
        if role != Role::Control {
            return Err(Failure::new(
                ErrorKind::Forbidden,
                "this link can only view the session; it evaluates, but runs no commands",
            ));
        }

        let (handle, console) = self
            .target
            .lock()
            .await
            .as_ref()
            .map(|target| (target.handle.clone(), Arc::clone(&target.console)))
            .ok_or_else(|| Failure::new(ErrorKind::Invalid, "nothing is being debugged"))?;
        let console = console.lock().await;

        if let Some(at) = at {
            let context = inspect::context(&handle, at.stop, at.execution(), at.frame).await?;
            handle.select_context(context.execution).await?;
            handle.select_frame(context.frame).await?;
        }

        let output = console
            .console(text)
            .await
            .map_err(|error| Failure::new(ErrorKind::Invalid, format!("{error:#}")))?
            .ok_or_else(|| {
                Failure::new(ErrorKind::Invalid, format!("'{text}' is not a command"))
            })?;
        drop(console);

        // `set` changes values the tabs show, and `catch` which exceptions
        // stop.
        match crate::cli::commands::line_command(text).map(|(spec, _)| spec.command) {
            Some(crate::cli::commands::Command::Set) => self.wrote().await,
            Some(crate::cli::commands::Command::Catch) => {
                self.count(
                    |target| &target.settings,
                    |state, count| state.settings = count,
                )
                .await;
            }
            _ => {}
        }

        Ok(ConsoleResult {
            output: Some(output),
            row: None,
        })
    }

    /// The handle, the launch spec of a launched program, and whether the
    /// program is running or stopped, rather than not started or ended.
    async fn current_launch(&self) -> Result<(DebuggerHandle, Option<LaunchSpec>, bool), Failure> {
        let current = self
            .target
            .lock()
            .await
            .as_ref()
            .map(|target| (target.handle.clone(), target.launch.clone()));
        let (handle, launch) =
            current.ok_or_else(|| Failure::new(ErrorKind::Invalid, "nothing is being debugged"))?;

        let started = matches!(
            self.state.borrow().inferior,
            Inferior::Running { .. } | Inferior::Stopped { .. }
        );
        Ok((handle, launch, started))
    }

    /// Says that `start` is under way, before anything is debugged.
    pub fn loading(&self, start: &Start) {
        let busy = match start {
            Start::Launch { spec, .. } => format!("Loading {}", spec.program.display()),
            Start::Attach(process) => format!("Attaching to process {process}"),
            Start::Core(options) => format!("Opening {}", options.core.display()),
        };

        self.state.send_replace(Arc::new(State::idle(Some(busy))));
    }

    /// Makes `start` the session's target, ending a current one when
    /// `replace` allows.
    pub async fn start(&self, start: Start, replace: bool) -> Result<(), Failure> {
        self.replace(start, replace)
            .await
            .map_err(|(failure, _)| failure)
    }

    /// Starts debugging `start`, ending the current target first when
    /// `replace` allows it. A failure names the session it ended, if any.
    async fn replace(&self, start: Start, replace: bool) -> Result<(), (Failure, Option<String>)> {
        let mut target = self.target.lock().await;
        if let Some(current) = target.as_ref()
            && !replace
        {
            return Err((
                Failure::new(
                    ErrorKind::Busy,
                    format!("{} is being debugged; replace it to continue", current.name),
                ),
                None,
            ));
        }

        let ended = match target.take() {
            Some(current) => {
                let name = current.name.clone();
                let _ = self.end(current).await;
                Some(name)
            }
            None => None,
        };

        let opened = self.open_target(&mut target, start).await;
        drop(target);
        opened.map_err(|failure| (failure, ended))
    }

    async fn open_target(&self, target: &mut Option<Target>, start: Start) -> Result<(), Failure> {
        self.history
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clear();
        self.loading(&start);
        let opened = open(&start, &self.debug_files).await;
        let debugger = match opened {
            Ok(debugger) => debugger,
            Err(message) => {
                self.state.send_replace(Arc::new(State::idle(None)));
                return Err(Failure::new(ErrorKind::Failed, message));
            }
        };

        let handle = debugger.handle();
        let attached = matches!(start, Start::Attach(_));
        let (info, launch, run) = describe_target(start, &handle);
        let id =
            random_hex(4).map_err(|error| Failure::new(ErrorKind::Failed, error.to_string()))?;
        let name = Path::new(&info.program).file_name().map_or_else(
            || info.program.clone(),
            |name| name.to_string_lossy().into_owned(),
        );

        // The new session's state goes out before the request is answered,
        // so a page can open it; the pump takes every later change.
        let events = handle.subscribe();
        let snapshot = handle.snapshot().await?;
        self.cause
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();

        let mut describer = Describer::new(id, info, handle.clone(), Arc::clone(&self.cause));
        self.state
            .send_replace(Arc::new(describer.describe(&snapshot).await));
        let images = Arc::clone(&describer.images);
        let writes = Arc::clone(&describer.writes);
        let settings = Arc::clone(&describer.settings);
        let outlet = Outlet {
            history: Arc::clone(&self.history),
            messages: self.messages.clone(),
        };

        // The console's commands read no settings files, as the debug
        // adapter's do not.
        let root = crate::cli::config::project_root(&self.cwd);
        let console = Cli::new(
            handle.clone(),
            Renderers::console(false, root.clone()),
            crate::cli::config::Settings::defaults(root),
            LaunchSettings::default(),
        );
        for warning in console.debug_information_warnings().await {
            outlet.publish(Stream::Log, &format!("warning: {warning}\n"));
        }

        // The session's, the project's, and the user's views apply, as in
        // the terminal.
        for warning in console.load_view_sources(&self.cwd, &self.views).await {
            outlet.publish(Stream::Log, &format!("views: {warning}\n"));
        }

        let session = describer.id.clone();
        let pump = tokio::spawn(pump(Arc::clone(&self.state), describer, events, outlet));

        *target = Some(Target {
            id: session,
            console: Arc::new(tokio::sync::Mutex::new(console)),
            writes,
            settings,
            debugger: Some(debugger),
            handle: handle.clone(),
            images,
            launch: launch.clone(),
            input: Arc::default(),
            pump,
            readers: Vec::new(),
            name,
        });

        if attached
            && self.resume_attached.load(Ordering::Relaxed)
            && let InferiorState::Stopped {
                process_id,
                stop_id,
                ..
            } = handle.snapshot().await?.inferior
        {
            handle
                .continue_execution(
                    stop_id,
                    ResumeScope::Process(process_id),
                    ExceptionDisposition::Pass,
                )
                .await?;
        }

        if run && let Some(spec) = launch {
            let (readers, input) = self.launch(&handle, &spec).await?;
            if let Some(target) = target.as_mut() {
                target.readers = readers;
                *target.input.lock().await = Some(input);
            }
        }

        Ok(())
    }

    /// Starts a launched program, recording its output readers and input.
    async fn run(&self, handle: &DebuggerHandle, spec: &LaunchSpec) -> Result<(), Failure> {
        let (readers, input) = self.launch(handle, spec).await?;

        if let Some(target) = self.target.lock().await.as_mut() {
            target.readers.extend(readers);
            *target.input.lock().await = Some(input);
        }

        Ok(())
    }

    async fn launch(
        &self,
        handle: &DebuggerHandle,
        spec: &LaunchSpec,
    ) -> Result<(Vec<JoinHandle<()>>, pipe::Sender), Failure> {
        let failed = |error: std::io::Error| Failure::new(ErrorKind::Failed, error.to_string());
        let (stdin_read, stdin_write) = output::pipe().map_err(failed)?;
        let (stdout_read, stdout_write) = output::pipe().map_err(failed)?;
        let (stderr_read, stderr_write) = output::pipe().map_err(failed)?;
        let input = pipe::Sender::from_owned_fd(stdin_write).map_err(failed)?;
        let options = LaunchOptions {
            arguments: spec.arguments.clone(),
            environment: spec
                .environment
                .iter()
                .map(|(name, value)| (name.clone(), Some(value.clone())))
                .collect(),
            working_directory: spec.cwd.clone(),
            stdin: Some(stdin_read.into()),
            stdout: Some(stdout_write.into()),
            stderr: Some(stderr_write.into()),
            stop_at_entry: spec.stop_at_entry,
        };

        handle.launch_with(options).await.map_err(|error| {
            Failure::new(
                ErrorKind::Failed,
                format!("failed to launch {}: {error}", spec.program.display()),
            )
        })?;

        let readers = [(stdout_read, Stream::Stdout), (stderr_read, Stream::Stderr)]
            .into_iter()
            .filter_map(|(read, stream)| self.read_output(read, stream).ok())
            .collect();

        Ok((readers, input))
    }

    /// Forwards a program stream to every tab until its writers close it.
    fn read_output(&self, read: OwnedFd, stream: Stream) -> std::io::Result<JoinHandle<()>> {
        let mut receiver = pipe::Receiver::from_owned_fd(read)?;
        let outlet = Outlet {
            history: Arc::clone(&self.history),
            messages: self.messages.clone(),
        };

        Ok(tokio::spawn(async move {
            let mut pending = Vec::with_capacity(OUTPUT_CHUNK);
            let mut buffer = vec![0; OUTPUT_CHUNK];

            loop {
                let read = receiver.read(&mut buffer).await.unwrap_or(0);
                pending.extend_from_slice(&buffer[..read]);
                outlet.publish(stream, &output::decode(&mut pending, read == 0));
                if read == 0 {
                    return;
                }
            }
        }))
    }

    /// Ends a target: a launched program is killed, an attached one
    /// detached.
    async fn end(&self, mut target: Target) -> Result<(), Failure> {
        target.pump.abort();
        let _ = (&mut target.pump).await;
        let shutdown = match target.debugger.take() {
            Some(debugger) => debugger.shutdown().await,
            None => Ok(()),
        };

        // The program's end closes its streams; a reader still waiting is
        // on a stream a forked child kept open.
        for reader in &target.readers {
            reader.abort();
        }

        shutdown.map_err(Failure::from)
    }

    /// Ends whatever is being debugged, as the server exits.
    pub async fn shutdown(&self) {
        let current = self.target.lock().await.take();
        if let Some(target) = current {
            let _ = self.end(target).await;
        }
    }
}

impl History {
    fn push(&mut self, piece: Arc<str>) {
        self.bytes += piece.len();
        self.pieces.push_back(piece);
        while self.bytes > OUTPUT_HISTORY
            && let Some(oldest) = self.pieces.pop_front()
        {
            self.bytes -= oldest.len();
        }
    }

    fn clear(&mut self) {
        self.pieces.clear();
        self.bytes = 0;
    }
}

async fn edit_breakpoint(
    handle: &DebuggerHandle,
    edit: protocol::EditBreakpoint,
) -> Result<(), Failure> {
    let id = microscope::BreakpointId::new(edit.id);
    let wanted = options(edit.condition, edit.hit_condition, edit.log_message)?;
    let current = handle
        .snapshot()
        .await?
        .breakpoints
        .iter()
        .find(|breakpoint| breakpoint.id == id)
        .cloned()
        .ok_or_else(|| {
            Failure::new(
                ErrorKind::Invalid,
                format!("there is no breakpoint {}", edit.id),
            )
        })?;

    if current.log_message != wanted.log_message {
        handle
            .set_breakpoint_log_message(id, wanted.log_message)
            .await?;
    }

    if current.hit_condition != wanted.hit_condition {
        handle
            .set_breakpoint_hit_condition(id, wanted.hit_condition)
            .await?;
    }

    if current.condition != wanted.condition {
        handle
            .set_breakpoint_condition(id, wanted.condition)
            .await?;
    }

    Ok(())
}

/// What is being debugged, and how a launched program starts.
fn describe_target(
    start: Start,
    handle: &DebuggerHandle,
) -> (protocol::Target, Option<LaunchSpec>, bool) {
    let program = handle.executable().display().to_string();

    match start {
        Start::Launch { spec, run } => (
            protocol::Target {
                kind: TargetKind::Launch,
                program,
                arguments: spec
                    .arguments
                    .iter()
                    .map(|argument| argument.to_string_lossy().into_owned())
                    .collect(),
                pid: None,
            },
            Some(spec),
            run,
        ),
        Start::Attach(process) => (
            protocol::Target {
                kind: TargetKind::Attach,
                program,
                arguments: Vec::new(),
                pid: Some(process.get()),
            },
            None,
            false,
        ),
        Start::Core(_) => (
            protocol::Target {
                kind: TargetKind::Core,
                program,
                arguments: Vec::new(),
                pid: handle.core_dump().map(|core| core.process_id.get()),
            },
            None,
            false,
        ),
    }
}

/// Breakpoint options from the page's text, where empty text means none.
fn options(
    condition: Option<String>,
    hit_condition: Option<String>,
    log_message: Option<String>,
) -> Result<BreakpointOptions, Failure> {
    let given = |text: Option<String>| text.filter(|text| !text.trim().is_empty());
    let invalid = |error: microscope::Error| Failure::new(ErrorKind::Invalid, error.to_string());

    Ok(BreakpointOptions {
        condition: given(condition)
            .map(|text| microscope::Condition::parse(&text))
            .transpose()
            .map_err(invalid)?,
        hit_condition: given(hit_condition)
            .map(|text| text.parse())
            .transpose()
            .map_err(invalid)?,
        log_message: given(log_message)
            .map(|text| microscope::LogMessage::parse(&text))
            .transpose()
            .map_err(invalid)?,
        pending: false,
        enabled: true,
        temporary: false,
    })
}

/// Encodes a message. One that cannot encode goes out as an error.
pub(super) fn encode(message: &impl serde::Serialize) -> String {
    serde_json::to_string(message)
        .unwrap_or_else(|error| json!({ "error": error.to_string() }).to_string())
}

fn to_value<T: serde::Serialize>(value: &T) -> Value {
    serde_json::to_value(value).unwrap_or_else(|error| json!({ "error": error.to_string() }))
}

/// Opens the debugger `start` describes, off the async threads where it
/// loads debug information.
async fn open(
    start: &Start,
    debug_files: &microscope::DebugFileOptions,
) -> Result<Debugger, String> {
    match start {
        Start::Launch { spec, .. } => {
            let program = spec.program.clone();
            let debug_files = debug_files.clone();
            tokio::task::spawn_blocking(move || Debugger::new_with(&program, &debug_files))
                .await
                .map_err(|error| error.to_string())?
                .map_err(|error| format!("failed to load {}: {error}", spec.program.display()))
        }
        Start::Attach(process) => Debugger::attach_with(*process, None, debug_files)
            .await
            .map_err(|error| format!("failed to attach to process {process}: {error}")),
        Start::Core(options) => {
            let options = microscope::CoreDumpOptions {
                debug_files: debug_files.clone(),
                ..options.clone()
            };

            let core = options.core.display().to_string();
            tokio::task::spawn_blocking(move || Debugger::open_core(&options))
                .await
                .map_err(|error| error.to_string())?
                .map_err(|error| format!("failed to open the core dump {core}: {error}"))
        }
    }
}

/// Where program output and logged messages go: every tab, and the history
/// a tab that joins later receives.
#[derive(Clone)]
struct Outlet {
    history: Arc<Mutex<History>>,
    messages: broadcast::Sender<Arc<str>>,
}

impl Outlet {
    fn publish(&self, stream: Stream, text: &str) {
        for piece in output::pieces(text, OUTPUT_CHUNK) {
            let message = ServerMessage::Output(Output {
                stream,
                text: piece.to_owned(),
            });
            let text: Arc<str> = encode(&message).into();

            // Publishing under the lock keeps joining tabs from missing or
            // repeating a piece.
            let mut history = self.history.lock().unwrap_or_else(PoisonError::into_inner);
            history.push(Arc::clone(&text));
            let _ = self.messages.send(text);
            drop(history);
        }
    }
}

/// Publishes the debugger's state after every burst of its events, and
/// what its logpoints and conditions say.
async fn pump(
    state: Arc<watch::Sender<Arc<State>>>,
    mut describer: Describer,
    mut events: broadcast::Receiver<DebuggerEvent>,
    outlet: Outlet,
) {
    loop {
        let Ok(snapshot) = describer.handle.snapshot().await else {
            return;
        };

        let next = describer.describe(&snapshot).await;
        publish(&state, next, &describer.writes, &describer.settings);

        match events.recv().await {
            Ok(event) => observe(&mut describer, &outlet, &event),
            Err(broadcast::error::RecvError::Lagged(_)) => {}
            Err(broadcast::error::RecvError::Closed) => return,
        }

        // Take every event already queued, so the burst publishes once.
        loop {
            match events.try_recv() {
                Ok(event) => observe(&mut describer, &outlet, &event),
                Err(broadcast::error::TryRecvError::Lagged(_)) => {}
                Err(_) => break,
            }
        }
    }
}

/// Publishes `next` unless it changes nothing, with the counts as they are
/// now. Read under the state's lock, they are never lower than what a
/// count published before.
fn publish(
    state: &watch::Sender<Arc<State>>,
    mut next: State,
    writes: &AtomicU64,
    settings: &AtomicU64,
) {
    state.send_if_modified(|current| {
        next.writes = writes.load(Ordering::Relaxed);
        next.settings = settings.load(Ordering::Relaxed);
        let changed = **current != next;
        if changed {
            *current = Arc::new(next);
        }

        changed
    });
}

fn observe(describer: &mut Describer, outlet: &Outlet, event: &DebuggerEvent) {
    describer.ended.observe(event);

    match event {
        DebuggerEvent::InferiorLaunched { .. }
        | DebuggerEvent::InferiorAttached { .. }
        | DebuggerEvent::InferiorExited { .. } => describer.images.clear(),
        DebuggerEvent::LogMessage { parts, .. } => {
            outlet.publish(Stream::Log, &(format::log_message(parts) + "\n"));
        }
        DebuggerEvent::ConditionFailed { owner, error, .. } => {
            let owner = match owner {
                microscope::ConditionOwner::Breakpoint(id) => format!("breakpoint {id}"),
                microscope::ConditionOwner::Watchpoint(id) => format!("watchpoint {id}"),
                microscope::ConditionOwner::Catchpoint(id) => format!("catchpoint {id}"),
            };

            outlet.publish(
                Stream::Log,
                &format!("the condition of {owner} failed, so it stopped: {error}\n"),
            );
        }

        DebuggerEvent::SignalReceived {
            thread_id,
            exception,
            ..
        } => {
            outlet.publish(
                Stream::Log,
                &(format::signal_received(*thread_id, exception, Renderer::new(false)) + "\n"),
            );
        }

        DebuggerEvent::ChildReleased {
            process_id, child, ..
        } => {
            outlet.publish(
                Stream::Log,
                &(format::child_released(*process_id, *child, Renderer::new(false)) + "\n"),
            );
        }
        _ => {}
    }
}

/// Whether `text` names a path on this page, never another host: browsers
/// read `//host` and `/\\host` as one.
fn is_page_path(text: &str) -> bool {
    text.starts_with('/')
        && !text[1..].starts_with(['/', '\\'])
        && !text.contains(|c: char| c.is_control())
}

/// Escapes a query value: a page path's own query holds `&`, `#`, and `+`.
/// `/` and `:` stay readable, as the page writes them.
fn escape_query(text: &str) -> String {
    let mut escaped = String::with_capacity(text.len());
    for byte in text.bytes() {
        if byte.is_ascii_alphanumeric() || b"-._~/:".contains(&byte) {
            escaped.push(char::from(byte));
        } else {
            let _ = write!(escaped, "%{byte:02X}");
        }
    }

    escaped
}

/// A frame named by a request's optional parts, when all are there.
fn frame_at(
    stop: Option<u64>,
    thread: Option<u64>,
    task: Option<protocol::TaskKey>,
    frame: Option<u32>,
) -> Option<FrameAt> {
    Some(FrameAt {
        stop: stop?,
        thread: thread?,
        task,
        frame: frame.unwrap_or(0),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A tab can count a change while the pump builds a state from the
    /// counts before it. Publishing that state keeps the newer count, so
    /// tabs never see a count go back.
    #[test]
    fn a_state_built_before_a_count_publishes_the_count() {
        let (writes, settings) = (AtomicU64::new(1), AtomicU64::new(1));
        let built = State {
            writes: writes.load(Ordering::Relaxed),
            settings: settings.load(Ordering::Relaxed),
            revision: 1,
            ..State::idle(None)
        };

        let (state, receiver) = watch::channel(Arc::new(built.clone()));
        writes.fetch_add(1, Ordering::Relaxed);
        settings.fetch_add(1, Ordering::Relaxed);
        state.send_modify(|state| {
            *state = Arc::new(State {
                writes: 2,
                settings: 2,
                ..State::clone(state)
            });
        });

        publish(
            &state,
            State {
                revision: 2,
                ..built
            },
            &writes,
            &settings,
        );

        let published = State::clone(&receiver.borrow());
        assert_eq!(published.revision, 2, "the pump's state is published");
        assert_eq!((published.writes, published.settings), (2, 2));
    }
}
