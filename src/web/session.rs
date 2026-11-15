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
    ExceptionDisposition, Expression, FollowPolicy, GroupHandle, InferiorState, InspectionLimits,
    LaunchOptions, MemberNumber, ProcessGroup, ProcessId, ResumeScope, StopId,
};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::unix::pipe;
use tokio::sync::{broadcast, mpsc, oneshot, watch};
use tokio::task::JoinHandle;

use super::auth::{Tokens, random_hex};
use super::describe::{Cause, Credits, Describer, Images, Run};
use super::group::{Pump, Selection, View};
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

const OUTPUT_HISTORY: usize = 256 * 1024;

const OUTPUT_CHUNK: usize = 32 * 1024;

const INPUT_WAIT: Duration = Duration::from_secs(5);

const INPUT_QUEUE: usize = 16;

#[expect(
    clippy::disallowed_types,
    reason = "no holder is cancelled: each runs on a request task of its own"
)]
type Turns<T> = tokio::sync::Mutex<T>;

#[derive(Debug, Clone, Default)]
pub struct LaunchSpec {
    pub program: PathBuf,
    pub arguments: Vec<OsString>,
    pub environment: Vec<(OsString, OsString)>,
    pub cwd: Option<PathBuf>,
    pub stop_at_entry: bool,
}

#[derive(Debug, Clone)]
pub enum Start {
    Launch { spec: LaunchSpec, run: bool },
    Attach(ProcessId),
    Core(CoreDumpOptions),
}

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
        let run = match error {
            Error::NotStopped => Some("the program is not stopped"),
            Error::NotRunning => Some("the program is not running"),
            Error::AlreadyRunning => Some("the program is already running"),
            Error::AlreadyStopped => Some("the program is already stopped"),
            _ => None,
        };
        if let Some(message) = run {
            return Self::new(ErrorKind::NotStopped, message);
        }

        let kind = match error {
            Error::StaleStop => ErrorKind::StaleStop,
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

enum Chosen {
    Answer(Value),
    Notice(String),
}

pub struct Session {
    cwd: PathBuf,
    home: Option<PathBuf>,

    views: Vec<PathBuf>,

    debug_files: microscope::DebugFileOptions,

    link_base: String,
    tokens: Tokens,
    default_name: String,

    target: Turns<Option<Target>>,
    state: Arc<watch::Sender<Arc<State>>>,

    messages: broadcast::Sender<Arc<str>>,

    history: Arc<Mutex<History>>,
    people: Mutex<BTreeMap<u32, Person>>,
    next_connection: AtomicU32,

    credits: Arc<Credits>,

    handles: values::Handles,

    resume_attached: AtomicBool,

    follow: Mutex<FollowPolicy>,

    stopping: watch::Sender<bool>,
}

struct Target {
    id: String,

    debugger: Option<Debugger>,

    group: Option<ProcessGroup>,

    root: DebuggerHandle,

    selection: Arc<Selection>,

    console_member: Arc<AtomicU32>,
    launch: Option<LaunchSpec>,

    input: Option<Input>,
    pump: JoinHandle<()>,
    readers: Vec<output::Forwarder>,
    name: String,

    console: Arc<Turns<Cli>>,

    writes: Arc<AtomicU64>,

    settings: Arc<AtomicU64>,
}

struct Input {
    queue: mpsc::Sender<protocol::Input>,

    _open: oneshot::Sender<()>,
}

impl Input {
    fn new(mut pipe: pipe::Sender, outlet: Outlet) -> Self {
        let (queue, mut queued) = mpsc::channel::<protocol::Input>(INPUT_QUEUE);
        let (open, closed) = oneshot::channel::<()>();
        tokio::spawn(async move {
            let write = async {
                while let Some(input) = queued.recv().await {
                    if let Err(error) = pipe.write_all(input.text.as_bytes()).await {
                        outlet.publish(
                            Stream::Log,
                            &format!("the program's input closed: {error}\n"),
                        );
                        return;
                    }

                    if input.eof {
                        return;
                    }
                }
            };

            tokio::select! {
                () = write => {}
                _ = closed => {}
            }
        });

        Self { queue, _open: open }
    }
}

#[derive(Default)]
struct History {
    pieces: VecDeque<Arc<str>>,
    bytes: usize,
}

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
            target: Turns::new(None),
            state: Arc::new(state),
            messages,
            history: Arc::default(),
            people: Mutex::default(),
            next_connection: AtomicU32::new(1),
            credits: Arc::default(),
            handles: values::Handles::default(),
            resume_attached: AtomicBool::new(false),
            follow: Mutex::default(),
            stopping: watch::Sender::new(false),
        })
    }

    pub fn stop_loading(&self) {
        self.stopping.send_replace(true);
    }

    pub fn follow(&self, policy: FollowPolicy) {
        *self.follow.lock().unwrap_or_else(PoisonError::into_inner) = policy;
    }

    pub fn resume_attached(&self, resume: bool) {
        self.resume_attached.store(resume, Ordering::Relaxed);
    }

    pub const fn tokens(&self) -> &Tokens {
        &self.tokens
    }

    pub fn cwd(&self) -> &Path {
        &self.cwd
    }

    pub fn join_link(&self, role: Role, to: &str) -> String {
        let to = if to == "/" || to.is_empty() {
            String::new()
        } else {
            format!("?to={}", escape_query(to))
        };

        format!("{}join{to}#{}", self.link_base, self.tokens.token(role))
    }

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

    pub async fn draw(&self, request: &protocol::Draw) -> Result<(Value, Vec<u8>), Failure> {
        let handle = self.current_handle().await?;
        let drawn = draw::draw(&handle, request).await?;
        Ok((to_value(&drawn.drawing), drawn.bytes))
    }

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
                | Request::Resolve(_)
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
            Request::SelectMember(protocol::MemberRef { member }) => {
                Some(self.select_member(member).await?)
            }
            Request::DetachMember(protocol::MemberRef { member }) => {
                Some(self.detach_member(member).await?)
            }
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
            Request::Resolve(protocol::SymbolName { name }) => {
                let address = self.current_handle().await?.runtime_address(&name).await?;
                Ok(to_value(&protocol::Resolved {
                    address: super::describe::hex(address.get()),
                }))
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

    async fn control(&self, connection: u32, request: Request) -> Result<Option<String>, Failure> {
        Ok(Some(match request {
            Request::Continue(protocol::Continue { stop }) => self.resume(connection, stop).await?,
            Request::Step(step) => self.step(connection, step).await?,
            Request::Jump(jump) => self.jump(connection, jump).await?,
            Request::Advance(advance) => self.advance(connection, advance).await?,
            Request::Pause => {
                self.pause(connection).await?;
                "paused".to_owned()
            }
            Request::Kill => {
                self.kill().await?;
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
        let handle = self.root_handle().await?;

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
                    pending: crate::cli::commands::is_python(&spec)
                        || crate::cli::commands::is_javascript(&spec),
                    ..options(add.condition, add.hit_condition, add.log_message)?
                };

                let (breakpoint, waits) = add_breakpoint(&handle, spec, options).await?;
                let notice = if waits {
                    format!(
                        "set breakpoint {}, pending: nothing loaded has it, and the program \
                         may execute one that does",
                        breakpoint.id
                    )
                } else {
                    format!("set breakpoint {}", breakpoint.id)
                };
                self.notice(connection, notice);
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

    fn credit(&self, connection: u32, run: Run, action: &str) {
        self.credits.started(run, self.cause(connection, action));
    }

    fn cause(&self, connection: u32, action: &str) -> Cause {
        Cause {
            name: self.name_of(connection),
            action: action.to_owned(),
        }
    }

    async fn step(&self, connection: u32, step: protocol::Step) -> Result<String, Failure> {
        let View { member, handle, .. } = self.current_view().await?;
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
            let execution = handle
                .start_step_into(
                    context.stop,
                    context.execution,
                    call,
                    scope,
                    ExceptionDisposition::Pass,
                )
                .await?;
            self.credit(connection, Run::Member(member, execution), &action);
            return Ok(action);
        }

        let execution = handle
            .start_step(
                context.stop,
                context.execution,
                context.frame,
                kind,
                scope,
                ExceptionDisposition::Pass,
            )
            .await?;
        self.credit(connection, Run::Member(member, execution), action);
        Ok(action.to_owned())
    }

    async fn jump(&self, connection: u32, jump: protocol::Jump) -> Result<String, Failure> {
        let View { member, handle, .. } = self.current_view().await?;
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
        let execution = handle
            .start_jump(context.stop, context.execution, spec)
            .await?;
        self.credit(connection, Run::Member(member, execution), &action);

        Ok(action)
    }

    async fn advance(
        &self,
        connection: u32,
        advance: protocol::Advance,
    ) -> Result<String, Failure> {
        let View { member, handle, .. } = self.current_view().await?;
        let InferiorState::Stopped { process_id, .. } = handle.snapshot().await?.inferior else {
            return Err(Failure::new(
                ErrorKind::NotStopped,
                "the program is not stopped",
            ));
        };

        let location = advance.location.trim();
        let spec = crate::cli::commands::parse_breakpoint_location(location)
            .map_err(|error| Failure::new(ErrorKind::Invalid, format!("{error:#}")))?
            .filter(|_| !location.is_empty())
            .ok_or_else(|| {
                Failure::new(
                    ErrorKind::Invalid,
                    "a thread advances to FILE:LINE, FILE:FUNCTION, a function, or 0xADDRESS",
                )
            })?;
        let context = inspect::context(
            &handle,
            advance.stop,
            protocol::execution(advance.thread, advance.task),
            advance.frame,
        )
        .await?;
        let action = format!("advanced thread {} to {location}", advance.thread);
        let execution = handle
            .start_advance(
                context.stop,
                context.execution,
                context.frame,
                spec,
                ResumeScope::Process(process_id),
                ExceptionDisposition::Pass,
            )
            .await?;
        self.credit(connection, Run::Member(member, execution), &action);

        Ok(action)
    }

    async fn input(&self, input: protocol::Input) -> Result<(), Failure> {
        let queue = self
            .target
            .lock()
            .await
            .as_ref()
            .ok_or_else(|| Failure::new(ErrorKind::Invalid, "nothing is being debugged"))?
            .input
            .as_ref()
            .map(|input| input.queue.clone());
        let closed = || {
            Failure::new(
                ErrorKind::Invalid,
                "the program's input is closed; it reads input only while it runs",
            )
        };
        let queue = queue.ok_or_else(closed)?;

        let permit = tokio::time::timeout(INPUT_WAIT, queue.reserve())
            .await
            .map_err(|_| Failure::new(ErrorKind::Failed, "the program is not reading its input"))?
            .map_err(|_| closed())?;
        permit.send(input);
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

    async fn resume(&self, connection: u32, stop: Option<u64>) -> Result<String, Failure> {
        let (handle, launch, started) = self.current_launch().await?;
        if let Some(stop) = stop {
            let (group, view) = self
                .target
                .lock()
                .await
                .as_ref()
                .map(|target| {
                    (
                        target.group.as_ref().map(ProcessGroup::handle),
                        target.selection.view(),
                    )
                })
                .ok_or_else(|| Failure::new(ErrorKind::Invalid, "nothing is being debugged"))?;

            if let Some(group) = group {
                let execution = group
                    .continue_from((view.member, StopId::new(stop)))
                    .await?;
                self.credit(connection, Run::Group(execution), "continued");
                return Ok("continued".to_owned());
            }

            let InferiorState::Stopped { process_id, .. } = view.handle.snapshot().await?.inferior
            else {
                return Err(Failure::new(
                    ErrorKind::NotStopped,
                    "the program is not stopped",
                ));
            };

            let execution = view
                .handle
                .continue_execution(
                    StopId::new(stop),
                    ResumeScope::Process(process_id),
                    ExceptionDisposition::Pass,
                )
                .await?;
            self.credit(connection, Run::Member(view.member, execution), "continued");
            return Ok("continued".to_owned());
        }

        let Some(spec) = launch else {
            return Err(Failure::new(
                ErrorKind::Invalid,
                "pick a stop to continue from",
            ));
        };

        if started {
            return Err(Failure::new(
                ErrorKind::NotStopped,
                "the program has already started; pick a stop to continue from",
            ));
        }

        let execution = self.run(&handle, &spec).await?;
        self.credit(connection, Run::Member(1, execution), "started the program");
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

        let group = self.group_handle().await;
        match kill_program(group.as_ref(), &handle).await {
            Ok(()) | Err(microscope::Error::NotRunning) => {}
            Err(error) => return Err(error.into()),
        }
        if let Some(group) = group {
            for _ in 0..group.members().len() {
                if group.continue_all().await.is_err() {
                    break;
                }
            }
        }

        let execution = self.run(&handle, &spec).await?;
        self.credit(
            connection,
            Run::Member(1, execution),
            "restarted the program",
        );
        Ok(())
    }

    async fn pause(&self, connection: u32) -> Result<(), Failure> {
        let Some(group) = self.group_handle().await else {
            let View { member, handle, .. } = self.current_view().await?;
            let running = match handle.snapshot().await?.inferior {
                InferiorState::Running { execution_id, .. } => execution_id,
                _ => None,
            };
            handle.pause().await?;
            if let Some(execution) = running {
                let run = Run::Member(member, execution);
                self.credits.paused(run, self.cause(connection, "paused"));
            }
            return Ok(());
        };

        if !group
            .members()
            .iter()
            .any(|member| member.state == microscope::MemberState::Running)
        {
            return Err(microscope::Error::NotRunning.into());
        }

        if let Some(execution) = group.pause_all().await? {
            let run = Run::Group(execution);
            self.credits.paused(run, self.cause(connection, "paused"));
        }
        Ok(())
    }

    async fn kill(&self) -> Result<(), Failure> {
        let group = self.group_handle().await;
        let handle = self.root_handle().await?;
        kill_program(group.as_ref(), &handle)
            .await
            .map_err(Failure::from)
    }

    async fn select_member(&self, number: MemberNumber) -> Result<String, Failure> {
        let (group, selection) = self.group_selection().await?;
        let handle = group.member(number).ok_or_else(|| {
            Failure::new(
                ErrorKind::Invalid,
                format!("there is no process [{number}]"),
            )
        })?;
        if handle.snapshot().await.is_err() {
            return Err(Failure::new(
                ErrorKind::Invalid,
                format!("[{number}] is no longer being debugged"),
            ));
        }

        selection.select(number, handle);
        Ok(group
            .members()
            .iter()
            .find(|member| member.number == number)
            .map_or_else(
                || format!("selected [{number}]"),
                |member| format!("selected {}", format::member(member, Renderer::new(false))),
            ))
    }

    async fn detach_member(&self, number: MemberNumber) -> Result<String, Failure> {
        let (group, _) = self.group_selection().await?;
        if number == 1 {
            return Err(Failure::new(
                ErrorKind::Invalid,
                "the program's own process leaves only with the session; end it instead",
            ));
        }

        let member = group
            .members()
            .into_iter()
            .find(|member| member.number == number)
            .ok_or_else(|| {
                Failure::new(
                    ErrorKind::Invalid,
                    format!("there is no process [{number}]"),
                )
            })?;
        group.detach(number).await?;
        Ok(format!(
            "detached {}",
            format::member(&member, Renderer::new(false))
        ))
    }

    async fn group_selection(&self) -> Result<(GroupHandle, Arc<Selection>), Failure> {
        let found = self.target.lock().await.as_ref().map(|target| {
            (
                target.group.as_ref().map(ProcessGroup::handle),
                Arc::clone(&target.selection),
            )
        });
        let (group, selection) =
            found.ok_or_else(|| Failure::new(ErrorKind::Invalid, "nothing is being debugged"))?;
        let group = group
            .ok_or_else(|| Failure::new(ErrorKind::Unsupported, "a core dump has one process"))?;
        Ok((group, selection))
    }

    async fn group_handle(&self) -> Option<GroupHandle> {
        self.target
            .lock()
            .await
            .as_ref()
            .and_then(|target| target.group.as_ref().map(ProcessGroup::handle))
    }

    async fn root_handle(&self) -> Result<DebuggerHandle, Failure> {
        self.target
            .lock()
            .await
            .as_ref()
            .map(|target| target.root.clone())
            .ok_or_else(|| Failure::new(ErrorKind::Invalid, "nothing is being debugged"))
    }

    async fn current_images(&self) -> Result<(DebuggerHandle, Arc<Images>), Failure> {
        self.target
            .lock()
            .await
            .as_ref()
            .map(|target| {
                let view = target.selection.view();
                (view.handle, view.images)
            })
            .ok_or_else(|| Failure::new(ErrorKind::Invalid, "nothing is being debugged"))
    }

    async fn python_project(&self) -> Option<crate::present::python_files::Project> {
        let (handle, spec) =
            self.target.lock().await.as_ref().and_then(|target| {
                Some((target.selection.view().handle, target.launch.clone()?))
            })?;
        if !crate::present::debug_information::interprets_python(&handle.module_image()) {
            return None;
        }

        let cwd = match spec.cwd {
            Some(cwd) => cwd,
            None => std::env::current_dir().ok()?,
        };
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
        self.current_view().await.map(|view| view.handle)
    }

    async fn current_view(&self) -> Result<View, Failure> {
        self.target
            .lock()
            .await
            .as_ref()
            .map(|target| target.selection.view())
            .ok_or_else(|| Failure::new(ErrorKind::Invalid, "nothing is being debugged"))
    }

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

    async fn shared_console(&self) -> Result<Arc<Turns<Cli>>, Failure> {
        self.target
            .lock()
            .await
            .as_ref()
            .map(|target| Arc::clone(&target.console))
            .ok_or_else(|| Failure::new(ErrorKind::Invalid, "nothing is being debugged"))
    }

    async fn reader(&self, connection: u32) -> Result<values::Reader<'_>, Failure> {
        let (handle, images, session) = self
            .target
            .lock()
            .await
            .as_ref()
            .map(|target| {
                let view = target.selection.view();
                (view.handle, view.images, target.id.clone())
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

    async fn wrote(&self) {
        self.count(|target| &target.writes, |state, count| state.writes = count)
            .await;
    }

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

            set(&mut next, counter.load(Ordering::Relaxed));
            *state = Arc::new(next);
        });
    }

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

    async fn command(
        &self,
        role: Role,
        text: &str,
        at: Option<FrameAt>,
    ) -> Result<ConsoleResult, Failure> {
        if role != Role::Control {
            return Err(Failure::new(
                ErrorKind::Forbidden,
                "this link can only view the session. It can evaluate expressions but not run commands.",
            ));
        }

        let (view, console, synced, selection, group) = self
            .target
            .lock()
            .await
            .as_ref()
            .map(|target| {
                (
                    target.selection.view(),
                    Arc::clone(&target.console),
                    Arc::clone(&target.console_member),
                    Arc::clone(&target.selection),
                    target.group.as_ref().map(ProcessGroup::handle),
                )
            })
            .ok_or_else(|| Failure::new(ErrorKind::Invalid, "nothing is being debugged"))?;
        let handle = view.handle;
        let console = console.lock().await;

        if group.is_some() && synced.load(Ordering::Relaxed) != view.member {
            console
                .console(&format!("process {}", view.member))
                .await
                .map_err(|error| Failure::new(ErrorKind::Failed, format!("{error:#}")))?;
            synced.store(view.member, Ordering::Relaxed);
        }

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

        match crate::cli::commands::line_command(text).map(|(spec, _)| spec.command) {
            Some(crate::cli::commands::Command::Set) => self.wrote().await,
            Some(crate::cli::commands::Command::Process) => {
                let number = text
                    .split_whitespace()
                    .nth(1)
                    .map(|word| word.trim_start_matches('[').trim_end_matches(']'))
                    .and_then(|word| word.parse::<MemberNumber>().ok());
                if let Some((number, handle)) =
                    number.and_then(|number| Some((number, group.as_ref()?.member(number)?)))
                {
                    synced.store(number, Ordering::Relaxed);
                    selection.select(number, handle);
                }
            }
            Some(crate::cli::commands::Command::Detach) => synced.store(0, Ordering::Relaxed),
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

    async fn current_launch(&self) -> Result<(DebuggerHandle, Option<LaunchSpec>, bool), Failure> {
        let current = self.target.lock().await.as_ref().map(|target| {
            (
                target.root.clone(),
                target.launch.clone(),
                target.group.is_some(),
            )
        });
        let (handle, launch, group) =
            current.ok_or_else(|| Failure::new(ErrorKind::Invalid, "nothing is being debugged"))?;

        let started = if group {
            handle.snapshot().await?.inferior.live_process().is_some()
        } else {
            matches!(
                self.state.borrow().inferior,
                Inferior::Running { .. } | Inferior::Stopped { .. }
            )
        };
        Ok((handle, launch, started))
    }

    pub fn loading(&self, start: &Start) {
        let busy = match start {
            Start::Launch { spec, .. } => format!("Loading {}", spec.program.display()),
            Start::Attach(process) => format!("Attaching to process {process}"),
            Start::Core(options) => format!("Opening {}", options.core.display()),
        };

        self.state.send_replace(Arc::new(State::idle(Some(busy))));
    }

    pub async fn start(&self, start: Start, replace: bool) -> Result<(), Failure> {
        self.replace(start, replace)
            .await
            .map_err(|(failure, _)| failure)
    }

    async fn replace(&self, start: Start, replace: bool) -> Result<(), (Failure, Option<String>)> {
        let mut target = self.target.lock().await;

        if *self.stopping.borrow() {
            return Err((
                Failure::new(ErrorKind::Busy, "the server is stopping"),
                None,
            ));
        }

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

        let program = if target.is_some() {
            match prepare(&start, &self.debug_files, self.stopping.subscribe()).await {
                Ok(program) => program,
                Err(message) => return Err((Failure::new(ErrorKind::Failed, message), None)),
            }
        } else {
            None
        };

        let ended = match target.take() {
            Some(current) => {
                let name = current.name.clone();
                let _ = self.end(current).await;
                Some(name)
            }
            None => None,
        };

        let opened = self.open_target(&mut target, start, program).await;
        drop(target);
        opened.map_err(|failure| (failure, ended))
    }

    async fn open_target(
        &self,
        target: &mut Option<Target>,
        start: Start,
        program: Option<microscope::Program>,
    ) -> Result<(), Failure> {
        self.history
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clear();
        let id =
            random_hex(4).map_err(|error| Failure::new(ErrorKind::Failed, error.to_string()))?;
        self.loading(&start);
        let policy = self
            .follow
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        let stopping = self.stopping.subscribe();
        let opened = match open(&start, program, &self.debug_files, policy, stopping).await {
            Ok(opened) => opened,
            Err(message) => {
                self.state.send_replace(Arc::new(State::idle(None)));
                return Err(Failure::new(ErrorKind::Failed, message));
            }
        };

        let handle = opened.handle();
        let attached = matches!(start, Start::Attach(_));
        let (info, launch, run) = describe_target(start, &handle);
        let name = Path::new(&info.program).file_name().map_or_else(
            || info.program.clone(),
            |name| name.to_string_lossy().into_owned(),
        );
        let (debugger, group) = match opened {
            Opened::Single(debugger) => (Some(debugger), None),
            Opened::Group(group) => (None, Some(group)),
        };
        let members = group.as_ref().map(ProcessGroup::handle);

        let events = handle.subscribe();
        self.credits.clear();

        let describer = Describer::new(id, info, handle.clone(), Arc::clone(&self.credits));
        let selection = Arc::new(Selection::new(
            handle.clone(),
            Arc::clone(&describer.images),
        ));
        let writes = Arc::clone(&describer.writes);
        let settings = Arc::clone(&describer.settings);
        let session = describer.id.clone();
        let outlet = self.outlet();
        let mut pump = Pump::new(
            Arc::clone(&self.state),
            outlet.clone(),
            members.clone(),
            Arc::clone(&selection),
            describer,
            if attached {
                protocol::Joined::Attached
            } else {
                protocol::Joined::Launched
            },
        );
        let Some(first) = pump.first_state().await else {
            let _ = shut_down(debugger, group).await;
            self.state.send_replace(Arc::new(State::idle(None)));
            return Err(Failure::new(
                ErrorKind::Failed,
                "the debugger ended as it started",
            ));
        };
        self.state.send_replace(Arc::new(first));
        let console = self.console_for(&handle, members.as_ref(), &outlet).await;
        let group_events = members.as_ref().map(GroupHandle::subscribe);
        let pump = tokio::spawn(pump.run(events, group_events));

        *target = Some(Target {
            id: session,
            console: Arc::new(Turns::new(console)),
            writes,
            settings,
            debugger,
            group,
            root: handle.clone(),
            selection,
            console_member: Arc::new(AtomicU32::new(1)),
            launch: launch.clone(),
            input: None,
            pump,
            readers: Vec::new(),
            name,
        });

        if attached && self.resume_attached.load(Ordering::Relaxed) {
            resume_attached(members.as_ref(), &handle).await?;
        }

        if run && let Some(spec) = launch {
            let (readers, input, _) = self.launch(&handle, &spec).await?;
            if let Some(target) = target.as_mut() {
                target.readers = readers;
                target.input = Some(input);
            }
        }

        Ok(())
    }

    async fn console_for(
        &self,
        root: &DebuggerHandle,
        group: Option<&GroupHandle>,
        outlet: &Outlet,
    ) -> Cli {
        let project = crate::cli::config::project_root(&self.cwd);
        let mut console = Cli::new(
            root.clone(),
            Renderers::console(false, project.clone()),
            crate::cli::config::Settings::defaults(project),
            LaunchSettings::default(),
        );
        if let Some(group) = group {
            console = console.with_group(group.clone(), microscope::SourcePathMap::default());
        }
        for warning in console.debug_information_warnings().await {
            outlet.publish(Stream::Log, &format!("warning: {warning}\n"));
        }

        for warning in console.load_view_sources(&self.cwd, &self.views).await {
            outlet.publish(Stream::Log, &format!("views: {warning}\n"));
        }

        console
    }

    async fn run(
        &self,
        handle: &DebuggerHandle,
        spec: &LaunchSpec,
    ) -> Result<microscope::ExecutionId, Failure> {
        let (readers, input, execution) = self.launch(handle, spec).await?;

        if let Some(target) = self.target.lock().await.as_mut() {
            target.readers.extend(readers);
            target.input = Some(input);
        }

        Ok(execution)
    }

    async fn launch(
        &self,
        handle: &DebuggerHandle,
        spec: &LaunchSpec,
    ) -> Result<(Vec<output::Forwarder>, Input, microscope::ExecutionId), Failure> {
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

        let execution = handle.launch_with(options).await.map_err(|error| {
            Failure::new(
                ErrorKind::Failed,
                format!("failed to launch {}: {error}", spec.program.display()),
            )
        })?;

        let readers = [(stdout_read, Stream::Stdout), (stderr_read, Stream::Stderr)]
            .into_iter()
            .filter_map(|(read, stream)| self.read_output(read, stream).ok())
            .collect();

        Ok((readers, Input::new(input, self.outlet()), execution))
    }

    fn outlet(&self) -> Outlet {
        Outlet {
            history: Arc::clone(&self.history),
            messages: self.messages.clone(),
        }
    }

    fn read_output(&self, read: OwnedFd, stream: Stream) -> std::io::Result<output::Forwarder> {
        let mut receiver = pipe::Receiver::from_owned_fd(read)?;
        let outlet = self.outlet();

        Ok(output::Forwarder::spawn(async move {
            let mut pending = Vec::with_capacity(OUTPUT_CHUNK);
            let mut buffer = vec![0; OUTPUT_CHUNK];

            loop {
                let read = receiver.read(&mut buffer).await.unwrap_or(0);
                #[expect(clippy::indexing_slicing, reason = "a read fills at most its buffer")]
                let filled = &buffer[..read];
                pending.extend_from_slice(filled);
                outlet.publish(stream, &output::decode(&mut pending, read == 0));
                if read == 0 {
                    return;
                }
            }
        }))
    }

    async fn end(&self, mut target: Target) -> Result<(), Failure> {
        #[expect(
            clippy::disallowed_methods,
            reason = "the pump changes nothing a cancellation leaves half done"
        )]
        target.pump.abort();
        let _ = (&mut target.pump).await;
        let shutdown = shut_down(target.debugger.take(), target.group.take()).await;

        for reader in &target.readers {
            reader.stop();
        }

        shutdown.map_err(Failure::from)
    }

    pub async fn shutdown(&self) {
        self.stop_loading();
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

async fn kill_program(
    group: Option<&GroupHandle>,
    root: &DebuggerHandle,
) -> microscope::Result<()> {
    let Some(group) = group else {
        return root.kill().await;
    };

    let (mut killed, mut failed) = (false, None);
    for member in group.members().iter().rev() {
        let Some(handle) = group.member(member.number) else {
            continue;
        };
        if !matches!(
            member.state,
            microscope::MemberState::Running | microscope::MemberState::Stopped(_)
        ) {
            continue;
        }

        match handle.kill().await {
            Ok(()) => killed = true,
            Err(microscope::Error::NotRunning) => {}
            Err(error) => failed = failed.or(Some(error)),
        }
    }

    match failed {
        Some(error) => Err(error),
        None if killed => Ok(()),
        None => Err(microscope::Error::NotRunning),
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

pub(super) fn encode(message: &impl serde::Serialize) -> String {
    serde_json::to_string(message)
        .unwrap_or_else(|error| json!({ "error": error.to_string() }).to_string())
}

fn to_value<T: serde::Serialize>(value: &T) -> Value {
    serde_json::to_value(value).unwrap_or_else(|error| json!({ "error": error.to_string() }))
}

async fn resume_attached(
    group: Option<&GroupHandle>,
    handle: &DebuggerHandle,
) -> Result<(), Failure> {
    if let Some(group) = group {
        group.continue_all().await?;
        return Ok(());
    }

    if let InferiorState::Stopped {
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

    Ok(())
}

enum Opened {
    Group(ProcessGroup),
    Single(Debugger),
}

impl Opened {
    fn handle(&self) -> DebuggerHandle {
        match self {
            Self::Group(group) => group.root(),
            Self::Single(debugger) => debugger.handle(),
        }
    }
}

async fn prepare(
    start: &Start,
    debug_files: &microscope::DebugFileOptions,
    mut stopping: watch::Receiver<bool>,
) -> Result<Option<microscope::Program>, String> {
    match start {
        Start::Launch { spec, .. } => load(spec, debug_files, &mut stopping).await.map(Some),
        Start::Attach(process) => {
            if Path::new("/proc").join(process.to_string()).exists() {
                Ok(None)
            } else {
                Err(format!(
                    "failed to attach to process {process}: no such process"
                ))
            }
        }
        Start::Core(options) => std::fs::File::open(&options.core)
            .map(|_| None)
            .map_err(|error| {
                format!(
                    "failed to open the core dump {}: {error}",
                    options.core.display()
                )
            }),
    }
}

async fn load(
    spec: &LaunchSpec,
    debug_files: &microscope::DebugFileOptions,
    stopping: &mut watch::Receiver<bool>,
) -> Result<microscope::Program, String> {
    let program = spec.program.clone();
    let debug_files = debug_files.clone();
    loaded(
        stopping,
        tokio::task::spawn_blocking(move || microscope::Program::load(&program, &debug_files)),
    )
    .await?
    .map_err(|error| format!("failed to load {}: {error}", spec.program.display()))
}

async fn open(
    start: &Start,
    program: Option<microscope::Program>,
    debug_files: &microscope::DebugFileOptions,
    policy: FollowPolicy,
    mut stopping: watch::Receiver<bool>,
) -> Result<Opened, String> {
    match start {
        Start::Launch { spec, .. } => {
            let program = match program {
                Some(program) => program,
                None => load(spec, debug_files, &mut stopping).await?,
            };
            ProcessGroup::new(program, policy)
                .map(Opened::Group)
                .map_err(|error| format!("failed to load {}: {error}", spec.program.display()))
        }
        Start::Attach(process) => ProcessGroup::attach(*process, None, policy, debug_files)
            .await
            .map(Opened::Group)
            .map_err(|error| format!("failed to attach to process {process}: {error}")),
        Start::Core(options) => {
            let options = microscope::CoreDumpOptions {
                debug_files: debug_files.clone(),
                ..options.clone()
            };

            let core = options.core.display().to_string();
            loaded(
                &mut stopping,
                tokio::task::spawn_blocking(move || Debugger::open_core(&options)),
            )
            .await?
            .map(Opened::Single)
            .map_err(|error| format!("failed to open the core dump {core}: {error}"))
        }
    }
}

async fn shut_down(
    debugger: Option<Debugger>,
    group: Option<ProcessGroup>,
) -> microscope::Result<()> {
    match (debugger, group) {
        (Some(debugger), _) => debugger.shutdown().await,
        (None, Some(group)) => group.shutdown().await,
        (None, None) => Ok(()),
    }
}

async fn loaded<T>(
    stopping: &mut watch::Receiver<bool>,
    load: tokio::task::JoinHandle<T>,
) -> Result<T, String> {
    tokio::select! {
        loaded = load => loaded.map_err(|error| error.to_string()),
        _ = stopping.wait_for(|stopping| *stopping) => {
            Err("the server stopped while the program loaded".to_owned())
        }
    }
}

#[derive(Clone)]
pub(super) struct Outlet {
    history: Arc<Mutex<History>>,
    messages: broadcast::Sender<Arc<str>>,
}

#[cfg(test)]
impl Outlet {
    pub(super) fn unread() -> Self {
        Self {
            history: Arc::new(Mutex::new(History {
                pieces: VecDeque::new(),
                bytes: 0,
            })),
            messages: broadcast::channel(16).0,
        }
    }
}

impl Outlet {
    pub(super) fn publish(&self, stream: Stream, text: &str) {
        for piece in output::pieces(text, OUTPUT_CHUNK) {
            let message = ServerMessage::Output(Output {
                stream,
                text: piece.to_owned(),
            });
            let text: Arc<str> = encode(&message).into();

            let mut history = self.history.lock().unwrap_or_else(PoisonError::into_inner);
            history.push(Arc::clone(&text));
            let _ = self.messages.send(text);
            drop(history);
        }
    }
}

pub(super) fn publish(
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

async fn add_breakpoint(
    handle: &DebuggerHandle,
    spec: microscope::BreakpointSpec,
    options: BreakpointOptions,
) -> Result<(microscope::Breakpoint, bool), Failure> {
    let error = match handle
        .add_breakpoint_with(spec.clone(), options.clone())
        .await
    {
        Ok(breakpoint) => return Ok((breakpoint, false)),
        Err(error) => error,
    };

    let hint = match &error {
        microscope::Error::FunctionNotFound(name) => {
            crate::cli::commands::python_name_hint(name, &handle.module_image())
        }
        _ => None,
    };
    if let Some(hint) = hint {
        return Err(Failure::new(ErrorKind::Invalid, format!("{error}; {hint}")));
    }

    let unresolved = matches!(
        error,
        microscope::Error::FunctionNotFound(_) | microscope::Error::SourceFileNotFound(_)
    );
    if !unresolved
        || !crate::cli::commands::executes_programs(
            &crate::cli::commands::loaded_images(handle).await,
        )
    {
        return Err(Failure::from(error));
    }

    let options = BreakpointOptions {
        pending: true,
        ..options
    };
    Ok((handle.add_breakpoint_with(spec, options).await?, true))
}

pub(super) fn observe(describer: &Describer, outlet: &Outlet, event: &DebuggerEvent, tag: &str) {
    let log = |text: String| outlet.publish(Stream::Log, &format!("{tag}{text}\n"));
    let renderer = Renderer::new(false);

    match event {
        DebuggerEvent::InferiorLaunched { .. }
        | DebuggerEvent::InferiorAttached { .. }
        | DebuggerEvent::InferiorExited { .. } => describer.images.clear(),
        DebuggerEvent::LogMessage { parts, .. } => log(format::log_message(parts)),
        DebuggerEvent::ConditionFailed { owner, error, .. } => {
            let owner = match owner {
                microscope::ConditionOwner::Breakpoint(id) => format!("breakpoint {id}"),
                microscope::ConditionOwner::Watchpoint(id) => format!("watchpoint {id}"),
                microscope::ConditionOwner::Catchpoint(id) => format!("catchpoint {id}"),
            };

            log(format!(
                "the condition of {owner} failed, so it stopped: {error}"
            ));
        }

        DebuggerEvent::SignalReceived {
            thread_id,
            exception,
            ..
        } => log(format::signal_received(*thread_id, exception, renderer)),

        DebuggerEvent::ChildReleased {
            process_id, child, ..
        } => log(format::child_released(*process_id, *child, renderer)),

        DebuggerEvent::ProgramChanged {
            process_id,
            program,
            ..
        } => {
            describer.images.clear();
            if let Some(process_id) = process_id {
                log(format::program_changed(
                    *process_id,
                    &program.path,
                    renderer,
                ));
            }
        }
        _ => {}
    }
}

fn is_page_path(text: &str) -> bool {
    text.starts_with('/')
        && !text[1..].starts_with(['/', '\\'])
        && !text.contains(|c: char| c.is_control())
}

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

    #[tokio::test]
    async fn nothing_starts_once_the_session_is_shut_down() {
        struct Killed(std::process::Child);
        impl Drop for Killed {
            fn drop(&mut self) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }

        let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("build/test-programs/spin");
        let child = Killed(
            std::process::Command::new(fixture)
                .spawn()
                .expect("start the program"),
        );
        let session = Session::new(
            std::env::temp_dir(),
            String::new(),
            Tokens::mint().expect("tokens"),
            microscope::DebugFileOptions::default(),
            Vec::new(),
        );
        session.shutdown().await;

        let process = ProcessId::new(u64::from(child.0.id()));
        let started = session.start(Start::Attach(process), true).await;
        let attached = session.target.lock().await.take();
        if let Some(target) = attached {
            let _ = session.end(target).await;
            panic!("a program was attached to after the session shut down");
        }
        assert!(started.is_err(), "the attach fails");
    }

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
