//! One client's debug session.
//!
//! The session handles the client's requests one at a time, in the order
//! they arrive, and forwards the debugger's events between them. Because a
//! request that resumes the inferior is answered before the session reads
//! the events it causes, a response always precedes the stop it leads to.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use std::os::fd::OwnedFd;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use microscope::{
    Backtrace, BreakpointSpec, Debugger, DebuggerEvent, DebuggerHandle, Error,
    ExceptionDisposition, ExecutionContext, ExitStatus, HeldProcess, InferiorState, LaunchOptions,
    LineNumber, ModuleId, ModuleImage, ProcessId, ResumeScope, SignalPolicy, StackFrameId,
    StepKind, StopContext, StopId, StopReason, ThreadId, VariableSnapshot, VirtualAddress,
};
use serde_json::{Value, json};
use tokio::sync::{broadcast, mpsc, oneshot};
use tokio::task::JoinHandle;

use super::breakpoints::{Breakpoints, Change, Entry, Group, Key, Placement, Slot, State, Want};
use super::config::{self, Configuration, Start};
use super::handles::References;
use super::output;
use super::protocol::{
    self, ErrorBody, GotoArguments, Outgoing, SetBreakpointsArguments,
    SetExceptionBreakpointsArguments, SetFunctionBreakpointsArguments, ThreadArguments,
};

use super::signals::Selection;
use super::sources::source_json;
use super::threads::ThreadHandles;
use crate::cli::{Cli, LaunchSettings, Renderers};

/// How long the session waits for a program's output to drain after it
/// exits, in case a process it started still holds the output open.
const OUTPUT_DRAIN: std::time::Duration = std::time::Duration::from_secs(1);

/// Sends messages to the client through the connection's writer.
#[derive(Clone)]
pub struct Client {
    outgoing: mpsc::Sender<Outgoing>,
    responses: Arc<Mutex<Responses>>,
}

/// The client's answer to a reverse request: its body or error message.
pub type Response = Result<Value, String>;

/// Reverse requests awaiting the client's responses.
#[derive(Default)]
pub struct Responses {
    next_ticket: u64,

    /// Callers by ticket, until the writer numbers their request.
    unsent: HashMap<u64, oneshot::Sender<Response>>,

    /// Callers by the `seq` of their request.
    sent: HashMap<u64, oneshot::Sender<Response>>,
}

impl Responses {
    /// Records the `seq` the writer gave a request.
    pub fn sent(&mut self, ticket: u64, seq: u64) {
        if let Some(caller) = self.unsent.remove(&ticket) {
            self.sent.insert(seq, caller);
        }
    }

    /// Answers the caller of the request with this `seq`.
    pub fn settle(&mut self, seq: u64, response: Response) {
        if let Some(caller) = self.sent.remove(&seq) {
            let _ = caller.send(response);
        }
    }

    /// Fails every caller once the connection is gone.
    pub fn close(&mut self) {
        self.unsent.clear();
        self.sent.clear();
    }
}

/// The connection to the client is gone.
#[derive(Debug, Clone, Copy)]
pub struct Closed;

impl From<Closed> for ErrorBody {
    fn from(Closed: Closed) -> Self {
        Self::new("the connection to the client closed")
    }
}

impl Client {
    pub fn new(outgoing: mpsc::Sender<Outgoing>) -> Self {
        Self {
            outgoing,
            responses: Arc::default(),
        }
    }

    /// The reverse requests the connection's reader and writer settle.
    pub fn responses(&self) -> Arc<Mutex<Responses>> {
        Arc::clone(&self.responses)
    }

    /// Sends a reverse request and waits for the client's response.
    pub(super) async fn request(
        &self,
        command: &'static str,
        arguments: Value,
    ) -> Result<Response, Closed> {
        let (caller, response) = oneshot::channel();
        let ticket = {
            let mut responses = self.responses.lock().map_err(|_| Closed)?;
            responses.next_ticket += 1;
            let ticket = responses.next_ticket;
            responses.unsent.insert(ticket, caller);
            ticket
        };

        self.outgoing
            .send(Outgoing::Request {
                command,
                arguments,
                ticket,
            })
            .await
            .map_err(|_| Closed)?;
        response.await.map_err(|_| Closed)
    }

    pub async fn event(&self, event: &'static str, body: Value) -> Result<(), Closed> {
        self.outgoing
            .send(Outgoing::Event { event, body })
            .await
            .map_err(|_| Closed)
    }

    async fn respond(
        &self,
        request: &Header,
        result: Result<Value, ErrorBody>,
    ) -> Result<(), Closed> {
        self.outgoing
            .send(Outgoing::Response {
                request_seq: request.seq.clone(),
                command: request.command.clone(),
                result,
            })
            .await
            .map_err(|_| Closed)
    }

    pub(super) async fn important(&self, text: impl Into<String>) -> Result<(), Closed> {
        let mut text = text.into();
        text.push('\n');
        self.event("output", json!({"category": "important", "output": text}))
            .await
    }

    async fn thread(&self, reason: &str, id: i64) -> Result<(), Closed> {
        self.event("thread", json!({"reason": reason, "threadId": id}))
            .await
    }

    pub(super) async fn console(&self, text: impl Into<String>) -> Result<(), Closed> {
        let mut text = text.into();
        text.push('\n');
        self.event("output", json!({"category": "console", "output": text}))
            .await
    }
}

/// The `seq` of each request the client cancelled, as text, shared by the
/// reader that sees the cancellation and the session that honors it.
pub type Cancelled = Arc<Mutex<HashSet<String>>>;

/// What the reader hands the session.
#[derive(Debug)]
pub enum Inbound {
    /// A request the client cancelled before the session reached it.
    Cancelled { seq: Value, command: String },
    Request {
        seq: Value,
        command: String,
        arguments: Value,
    },

    /// A request that could not be parsed but whose `seq` was recovered.
    Malformed {
        seq: Value,
        command: String,
        message: String,
    },
}

/// Identifies a request to answer.
#[derive(Debug, Clone)]
struct Header {
    seq: Value,
    command: String,
}

/// What the session does after answering a request.
enum After {
    /// Tell the client it may configure the session.
    Initialized,

    /// Start the target, as launch and configuration both arrived.
    Start,

    /// End the session.
    End,
}

/// What the client said it supports.
#[derive(Debug, Clone, Copy)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "each is an independent capability the client reports"
)]
pub(super) struct ClientSupport {
    pub lines_start_at1: bool,
    pub columns_start_at1: bool,
    pub progress: bool,
    pub variable_type: bool,
    pub memory_references: bool,
    pub ansi: bool,
    pub invalidated: bool,
    pub memory_events: bool,
    pub run_in_terminal: bool,
    pub start_debugging: bool,
}

impl ClientSupport {
    /// What a client that declared nothing supports, as the protocol says:
    /// lines and columns start at one.
    const UNDECLARED: Self = Self {
        lines_start_at1: true,
        columns_start_at1: true,
        progress: false,
        variable_type: false,
        memory_references: false,
        ansi: false,
        invalidated: false,
        memory_events: false,
        run_in_terminal: false,
        start_debugging: false,
    };
}

/// The program being debugged.
struct Target {
    debugger: Option<Debugger>,
    handle: DebuggerHandle,
    start: Start,
    stop_on_entry: bool,
    source_paths: microscope::SourcePathMap,
    signals: Vec<(u64, Vec<String>)>,

    /// The policy each signal had before the session changed it.
    default_policies: HashMap<u64, SignalPolicy>,
    applied_policies: HashMap<u64, SignalPolicy>,
    applied_exceptions: microscope::ExceptionStops,

    /// The catchpoint the system-call filter made, and the calls it
    /// catches.
    applied_syscalls: Option<(
        Arc<[microscope::syscalls::SyscallId]>,
        microscope::CatchpointId,
    )>,
    console: Cli,
    syntax: microscope::AssemblySyntax,
    images: HashMap<ModuleId, Arc<ModuleImage>>,

    /// Recorded source paths by the canonical local path they map to.
    recorded_paths: Option<HashMap<PathBuf, PathBuf>>,
    process: Option<ProcessId>,
    pumps: Vec<JoinHandle<()>>,

    /// The settings child sessions carry over.
    inherited: serde_json::Map<String, Value>,
    threads: config::ThreadListing,
}

/// The stop the client was last told about.
#[derive(Debug, Clone)]
pub(super) struct Stop {
    pub id: StopId,
    pub thread: ThreadId,

    /// What the client knows as the thread that stopped: the task the
    /// thread runs, when the client's threads are tasks, or the thread.
    pub context: ExecutionContext,
    pub reason: StopReason,

    /// The reason the client was told, as its stopped event named it.
    pub told: &'static str,

    /// The frame of the stopped context the debugger selected, as at an
    /// exception the frame that raised it; the innermost otherwise.
    pub selected: StackFrameId,
}

impl Stop {
    /// The innermost frame of the stopped thread.
    pub const fn innermost(&self) -> StopContext {
        StopContext {
            stop: self.id,
            execution: ExecutionContext::Thread(self.thread),
            frame: StackFrameId::INNERMOST,
        }
    }
}

pub struct Session {
    pub(super) client: Client,
    support: Option<ClientSupport>,
    after: Option<After>,
    configured: bool,

    /// The deferred `launch` or `attach` response.
    starting: Option<Header>,
    target: Option<Target>,
    events: Option<broadcast::Receiver<DebuggerEvent>>,

    /// The processes the program forked, held for child sessions.
    held: Option<microscope::HeldChildren>,

    /// The requests for child sessions still waiting for the client.
    follows: Vec<JoinHandle<()>>,
    pub(super) breakpoints: Breakpoints,
    pub(super) data: super::watch::Data,
    exceptions: Selection,
    pub(super) references: References,
    pub(super) stop: Option<Stop>,
    backtraces: HashMap<ExecutionContext, Arc<Backtrace>>,
    variables: HashMap<(ExecutionContext, StackFrameId), Arc<VariableSnapshot>>,

    /// The client's ids for threads and tasks.
    pub(super) thread_ids: ThreadHandles,

    /// The threads the client was told about, with their client ids.
    threads: BTreeMap<ThreadId, i64>,

    /// The tasks the client was told about, with their client ids.
    pub(super) tasks: BTreeMap<microscope::TaskId, i64>,

    /// The modules the client was told are loaded.
    pub(super) modules: BTreeMap<ModuleId, microscope::LoadedModuleRecord>,

    /// The execution the client last started, whose resume it already knows.
    resumed: Option<microscope::ExecutionId>,

    /// Whether the program is being restarted, so its end does not end the
    /// session.
    restarting: bool,

    /// How values show when a request does not say.
    pub(super) display: super::values::Display,

    /// The source files the client was told are loaded, and the modules
    /// that have each, once it asked.
    pub(super) loaded_sources: Option<BTreeMap<PathBuf, BTreeSet<ModuleId>>>,

    /// Requests read ahead of the one being handled.
    queue: VecDeque<Inbound>,

    /// The `seq` of each request the client cancelled, as text.
    cancelled: Cancelled,
    ended: bool,
}

impl Session {
    pub fn new(client: Client, cancelled: Cancelled) -> Self {
        Self {
            cancelled,
            client,
            support: None,
            after: None,
            configured: false,
            starting: None,
            target: None,
            events: None,
            held: None,
            follows: Vec::new(),
            breakpoints: Breakpoints::default(),
            data: super::watch::Data::default(),
            exceptions: Selection::default(),
            references: References::default(),
            stop: None,
            backtraces: HashMap::new(),
            variables: HashMap::new(),
            thread_ids: ThreadHandles::default(),
            threads: BTreeMap::new(),
            tasks: BTreeMap::new(),
            modules: BTreeMap::new(),
            resumed: None,
            restarting: false,
            display: super::values::Display::default(),
            loaded_sources: None,
            queue: VecDeque::new(),
            ended: false,
        }
    }

    /// Serves the session until the client disconnects, the connection
    /// closes, or `shutdown` completes, which counts as a disconnect.
    pub async fn run(
        mut self,
        mut inbox: mpsc::Receiver<Inbound>,
        shutdown: impl std::future::Future<Output = ()>,
    ) {
        tokio::pin!(shutdown);

        loop {
            // Requests set aside while cancelling come first, in order.
            while let Ok(message) = inbox.try_recv() {
                self.queue.push_back(message);
            }

            let input = if let Some(message) = self.queue.pop_front() {
                Some(Input::Message(message))
            } else {
                tokio::select! {
                    biased;
                    () = &mut shutdown => None,
                    message = inbox.recv() => message.map(Input::Message),
                    child = next_held(&mut self.held) => Some(Input::Held(child)),
                    event = next_event(&mut self.events) => Some(Input::Event(event)),
                }
            };

            let result = match input {
                None => break,
                Some(Input::Message(message)) => self.message(message).await,
                Some(Input::Held(Some(child))) => self.follow(child).await,
                Some(Input::Held(None)) => {
                    self.held = None;
                    Ok(())
                }
                Some(Input::Event(Ok(event))) => self.event(event).await,
                Some(Input::Event(Err(broadcast::error::RecvError::Lagged(_)))) => {
                    self.resync().await
                }
                Some(Input::Event(Err(broadcast::error::RecvError::Closed))) => {
                    self.events = None;
                    Ok(())
                }
            };

            if result.is_err() || self.ended {
                break;
            }
        }

        if !self.ended {
            let _ = self.end(None).await;
        }

        self.release().await;
    }

    async fn message(&mut self, message: Inbound) -> Result<(), Closed> {
        let (header, arguments) = match message {
            Inbound::Request { seq, command, .. } if self.take_cancellation(&seq) => {
                return self
                    .client
                    .respond(&Header { seq, command }, Err(ErrorBody::cancelled()))
                    .await;
            }

            Inbound::Request {
                seq,
                command,
                arguments,
            } => (Header { seq, command }, arguments),
            Inbound::Cancelled { seq, command } => {
                return self
                    .client
                    .respond(&Header { seq, command }, Err(ErrorBody::cancelled()))
                    .await;
            }

            Inbound::Malformed {
                seq,
                command,
                message,
            } => {
                return self
                    .client
                    .respond(&Header { seq, command }, Err(ErrorBody::new(message)))
                    .await;
            }
        };

        match self.dispatch(&header, arguments).await {
            Ok(Some(body)) => self.client.respond(&header, Ok(body)).await?,
            Ok(None) => {}
            Err(error) => self.client.respond(&header, Err(error)).await?,
        }

        match self.after.take() {
            Some(After::Initialized) => self.client.event("initialized", Value::Null).await,
            Some(After::Start) => self.start().await,
            Some(After::End) => {
                self.ended = true;
                Ok(())
            }
            None => Ok(()),
        }
    }

    /// Handles one request. `Ok(None)` defers the response.
    async fn dispatch(
        &mut self,
        header: &Header,
        arguments: Value,
    ) -> Result<Option<Value>, ErrorBody> {
        let command = header.command.as_str();
        if self.support.is_none() && command != "initialize" {
            return Err(ErrorBody::new(format!(
                "the client must send initialize before {command}"
            )));
        }

        let body = match command {
            "initialize" => self.initialize(arguments)?,
            "launch" => return self.launch(header, arguments).await,
            "attach" => return self.attach(header, arguments).await,
            "configurationDone" => self.configuration_done()?,
            "disconnect" => self.disconnect(arguments).await?,
            "terminate" => self.terminate().await?,
            "restart" => self.restart(arguments).await?,
            "cancel" => json!({}),
            "threads" => self.threads().await?,
            "stackTrace" => self.stack_trace(arguments).await?,
            "scopes" => self.scopes(arguments).await?,
            "variables" => self.variables_request(arguments).await?,
            "evaluate" => self.evaluate(arguments).await?,
            "exceptionInfo" => self.exception_info(arguments)?,
            "continue" => self.continue_execution(arguments).await?,
            "next" => {
                self.step(arguments, StepKind::OverSource, StepKind::OverInstruction)
                    .await?
            }
            "stepIn" => {
                self.step(arguments, StepKind::IntoSource, StepKind::Instruction)
                    .await?
            }
            "stepOut" => self.step(arguments, StepKind::Out, StepKind::Out).await?,
            "stepInTargets" => self.step_in_targets(arguments).await?,
            "pause" => self.pause().await?,
            "setBreakpoints" => self.set_breakpoints(arguments).await?,
            "setFunctionBreakpoints" => self.set_function_breakpoints(arguments).await?,
            "setExceptionBreakpoints" => self.set_exception_breakpoints(arguments).await?,
            "setInstructionBreakpoints" => self.set_instruction_breakpoints(arguments).await?,
            "readMemory" => self.read_memory(arguments).await?,
            "writeMemory" => self.write_memory(arguments).await?,
            "setVariable" => self.set_variable(arguments).await?,
            "setExpression" => self.set_expression(arguments).await?,
            "dataBreakpointInfo" => self.data_breakpoint_info(arguments).await?,
            "modules" => self.modules(arguments).await?,
            "loadedSources" => self.loaded_sources().await?,
            "breakpointLocations" => self.breakpoint_locations(arguments)?,
            "gotoTargets" => self.goto_targets(arguments)?,
            "goto" => self.goto(arguments).await?,
            "completions" => self.completions(arguments).await?,
            "setDataBreakpoints" => self.set_data_breakpoints(arguments).await?,
            "disassemble" => self.disassemble(arguments).await?,
            "locations" => self.locations(arguments)?,
            "microscope/setValueFormat" => self.set_value_format(arguments).await?,
            "source" => {
                return Err(ErrorBody::new(
                    "source contents are not available from the debugger; open the file locally",
                ));
            }
            _ => {
                return Err(ErrorBody::new(format!(
                    "the '{command}' request is not supported"
                )));
            }
        };

        Ok(Some(body))
    }

    fn initialize(&mut self, arguments: Value) -> Result<Value, ErrorBody> {
        if self.support.is_some() {
            return Err(ErrorBody::new("the session is already initialized"));
        }

        let arguments = parse::<protocol::InitializeArguments>(arguments, "initialize arguments")?;
        self.support = Some(ClientSupport {
            lines_start_at1: arguments.lines_start_at1.unwrap_or(true),
            columns_start_at1: arguments.columns_start_at1.unwrap_or(true),
            progress: arguments.supports_progress_reporting.unwrap_or(false),
            variable_type: arguments.supports_variable_type.unwrap_or(false),
            memory_references: arguments.supports_memory_references.unwrap_or(false),
            ansi: arguments.supports_ansi_styling.unwrap_or(false),
            invalidated: arguments.supports_invalidated_event.unwrap_or(false),
            memory_events: arguments.supports_memory_event.unwrap_or(false),
            run_in_terminal: arguments.supports_run_in_terminal_request.unwrap_or(false),
            start_debugging: arguments.supports_start_debugging_request.unwrap_or(false),
        });
        self.after = Some(After::Initialized);

        Ok(capabilities())
    }

    /// What the client supports. Requests wait for `initialize`, so only a
    /// request that somehow came before it sees the protocol defaults.
    pub(super) const fn support(&self) -> ClientSupport {
        match self.support {
            Some(support) => support,
            None => ClientSupport::UNDECLARED,
        }
    }

    async fn launch(
        &mut self,
        header: &Header,
        arguments: Value,
    ) -> Result<Option<Value>, ErrorBody> {
        self.check_unstarted()?;
        let configuration = config::launch(arguments).map_err(ErrorBody::shown)?;
        let Start::Launch(launch) = &configuration.start else {
            return Err(ErrorBody::new(
                "internal: a launch configuration does not launch",
            ));
        };

        let program = launch.program.clone();
        let title = format!("Loading {}", launch.program.display());
        let debug_files = configuration.debug_files.clone();
        let debugger = self
            .with_progress(
                title,
                tokio::task::spawn_blocking(move || Debugger::new_with(&program, &debug_files)),
            )
            .await
            .map_err(|error| ErrorBody::shown(error.to_string()))?
            .map_err(|error| {
                ErrorBody::shown(format!(
                    "failed to load {}: {error}",
                    launch.program.display()
                ))
            })?;

        self.adopt(debugger, configuration, header).await
    }

    async fn attach(
        &mut self,
        header: &Header,
        arguments: Value,
    ) -> Result<Option<Value>, ErrorBody> {
        self.check_unstarted()?;
        let configuration = config::attach(arguments).map_err(ErrorBody::shown)?;
        let debugger = match &configuration.start {
            Start::Attach {
                process,
                executable,
                held,
            } => {
                let process = *process;
                let held = held.map(|start_time| HeldProcess {
                    process_id: process,
                    start_time,
                });
                let debug_files = &configuration.debug_files;
                let attached = async {
                    match held {
                        Some(held) => {
                            Debugger::attach_held_with(held, executable.as_deref(), debug_files)
                                .await
                        }
                        None => {
                            Debugger::attach_with(process, executable.as_deref(), debug_files).await
                        }
                    }
                };

                let attached = self
                    .with_progress(format!("Attaching to process {process}"), attached)
                    .await;

                attached.map_err(|error| {
                    // A held process this session cannot take runs on.
                    let released = held.is_some_and(|held| {
                        microscope::release_held(&held).is_ok_and(|released| released)
                    });
                    let released = if released { "; it runs on its own" } else { "" };
                    ErrorBody::shown(format!(
                        "failed to attach to process {process}: {error}{released}"
                    ))
                })?
            }
            Start::Core(options) => {
                let core = options.core.display().to_string();
                let options = options.clone();
                self.with_progress(
                    format!("Opening {core}"),
                    tokio::task::spawn_blocking(move || Debugger::open_core(&options)),
                )
                .await
                .map_err(|error| ErrorBody::shown(error.to_string()))?
                .map_err(|error| {
                    ErrorBody::shown(format!("failed to open the core dump {core}: {error}"))
                })?
            }
            Start::Launch(_) => {
                return Err(ErrorBody::new(
                    "internal: an attach configuration does not attach",
                ));
            }
        };

        self.adopt(debugger, configuration, header).await
    }

    /// Reports slow work, such as loading debug information, to a client
    /// that shows progress.
    async fn with_progress<T>(
        &self,
        title: String,
        work: impl std::future::Future<Output = T>,
    ) -> T {
        if !self.support().progress {
            return work.await;
        }

        let _ = self
            .client
            .event(
                "progressStart",
                json!({"progressId": "load", "title": title, "cancellable": false}),
            )
            .await;
        let result = work.await;
        let _ = self
            .client
            .event("progressEnd", json!({"progressId": "load"}))
            .await;

        result
    }

    fn check_unstarted(&self) -> Result<(), ErrorBody> {
        if self.target.is_some() || self.starting.is_some() {
            return Err(ErrorBody::new(
                "the session already has a program; start a new session for another",
            ));
        }

        Ok(())
    }

    /// Makes a loaded debugger the session's target and defers the
    /// `launch` or `attach` response until configuration is done.
    async fn adopt(
        &mut self,
        debugger: Debugger,
        configuration: Configuration,
        header: &Header,
    ) -> Result<Option<Value>, ErrorBody> {
        let Configuration {
            start,
            stop_on_entry,
            source_paths,
            syntax,
            signals,
            view_files,
            working_directory,
            follow_forks,
            inherited,
            threads,
            debug_files: _,
            step_into_runtime,
        } = configuration;
        let launched = matches!(start, Start::Launch(_));
        let core = matches!(start, Start::Core(_));
        let handle = debugger.handle().with_source_paths(source_paths.clone());
        self.events = Some(handle.subscribe());

        // The console's commands read no settings files: an editor's
        // sessions take their settings from its launch arguments.
        let root = crate::cli::config::project_root(
            &working_directory
                .clone()
                .or_else(|| std::env::current_dir().ok())
                .unwrap_or_default(),
        );
        let renderers = Renderers::console(self.support().ansi, root.clone());
        let mut settings = crate::cli::config::Settings::defaults(root);
        settings.config.disassembly.syntax = syntax.into();
        let console = Cli::new(
            handle.clone(),
            renderers,
            settings,
            LaunchSettings::default(),
        );
        let mut default_policies = HashMap::new();

        for code in microscope::signal_codes() {
            default_policies.insert(code, handle.signal_policy(code).await.map_err(error)?);
        }

        if step_into_runtime {
            handle.set_step_into_runtime(true).await.map_err(error)?;
        }

        self.target = Some(Target {
            debugger: Some(debugger),
            handle,
            start,
            stop_on_entry,
            source_paths,
            signals,
            applied_policies: default_policies.clone(),
            default_policies,
            applied_exceptions: microscope::ExceptionStops::default(),
            applied_syscalls: None,
            console,
            syntax,
            images: HashMap::new(),
            recorded_paths: None,
            process: None,
            pumps: Vec::new(),
            inherited,
            threads,
        });
        if follow_forks && !core {
            self.hold_forks().await?;
        }

        self.warn_debug_information().await?;
        self.load_views(working_directory, &view_files).await?;
        self.apply_signal_policies().await?;

        if !launched {
            // A client restarts a core dump by opening it again, and an
            // attached process by detaching and attaching to it again.
            self.client
                .event(
                    "capabilities",
                    json!({"capabilities": {"supportsRestartRequest": false}}),
                )
                .await?;
        }

        if core {
            self.warn_core_modules().await?;
        }

        self.resolve_unresolved().await?;
        self.starting = Some(header.clone());
        if self.configured {
            self.after = Some(After::Start);
        }

        Ok(None)
    }

    /// Holds the processes the program forks for child sessions, if the
    /// client can start them; otherwise says that they run on their own.
    async fn hold_forks(&mut self) -> Result<(), ErrorBody> {
        if !self.support().start_debugging {
            self.client
                .important(
                    "followForks: this client cannot start child sessions, so processes the \
                     program forks run on their own",
                )
                .await?;
            return Ok(());
        }

        let handle = self.target_handle()?;
        self.held = Some(handle.hold_forks().await.map_err(error)?);
        Ok(())
    }

    /// Asks the client to debug a forked process in a session of its own.
    async fn follow(&mut self, child: microscope::HeldChild) -> Result<(), Closed> {
        let Some(target) = self.target.as_ref() else {
            return Ok(());
        };

        let program = target.handle.executable().file_name().map_or_else(
            || target.handle.executable().display().to_string(),
            |name| name.to_string_lossy().into_owned(),
        );
        let arguments = super::forks::start_arguments(&target.inherited, &program, &child);
        self.client
            .console(format!(
                "process {} forked process {}, which is debugged in a session of its own",
                child.parent(),
                child.process().process_id
            ))
            .await?;
        self.follows.retain(|follow| !follow.is_finished());
        self.follows.push(tokio::spawn(super::forks::follow(
            self.client.clone(),
            arguments,
            child,
        )));

        Ok(())
    }

    /// Presents values with the configuration's view files and the
    /// project's and user's, and says what kept any of them, or the
    /// program's own, out.
    async fn load_views(
        &self,
        working_directory: Option<PathBuf>,
        view_files: &[PathBuf],
    ) -> Result<(), Closed> {
        let Some(target) = self.target.as_ref() else {
            return Ok(());
        };

        let working_directory = working_directory
            .or_else(|| std::env::current_dir().ok())
            .unwrap_or_default();
        let warnings = target
            .console
            .load_view_sources(
                &crate::cli::config::project_root(&working_directory),
                view_files,
            )
            .await;

        for warning in warnings {
            self.client.important(format!("views: {warning}")).await?;
        }

        Ok(())
    }

    /// Says what could not be used of the program's debug information; a
    /// library's is said as it is announced.
    async fn warn_debug_information(&self) -> Result<(), Closed> {
        let Some(handle) = self.handle() else {
            return Ok(());
        };

        for warning in crate::present::debug_information::warnings(&handle.module_image(), true) {
            self.client.important(warning).await?;
        }

        Ok(())
    }

    async fn warn_core_modules(&self) -> Result<(), Closed> {
        let Some(core) = self.handle().and_then(DebuggerHandle::core_dump) else {
            return Ok(());
        };

        for warning in crate::cli::format::core_module_warnings(core) {
            self.client.important(format!("warning: {warning}")).await?;
        }

        Ok(())
    }

    fn configuration_done(&mut self) -> Result<Value, ErrorBody> {
        if self.configured {
            return Err(ErrorBody::new("configuration is already done"));
        }

        self.configured = true;
        if self.starting.is_some() {
            self.after = Some(After::Start);
        }

        Ok(json!({}))
    }

    /// Starts the target once both its configuration and `launch` or
    /// `attach` arrived: answers the deferred request, announces the
    /// process, and lets it run or reports its first stop.
    async fn start(&mut self) -> Result<(), Closed> {
        let Some(header) = self.starting.take() else {
            return Ok(());
        };

        let (method, pipes) = match self.begin().await {
            Ok(begun) => begun,
            Err(error) => {
                self.client.respond(&header, Err(error)).await?;
                self.ended_target().await?;
                return Ok(());
            }
        };

        self.client.respond(&header, Ok(json!({}))).await?;
        let Some(snapshot) = self.announce_process(method, pipes).await? else {
            return Ok(());
        };

        let InferiorState::Stopped {
            stop_id,
            thread_id,
            reason,
            ..
        } = snapshot.inferior
        else {
            return Ok(());
        };

        let Some(target) = self.target.as_ref() else {
            self.client
                .important("internal: the started target is gone")
                .await?;
            return Ok(());
        };

        match &target.start {
            Start::Core(_) => self.stopped(stop_id, thread_id, reason).await?,
            Start::Attach { .. } if target.stop_on_entry => {
                self.stopped(stop_id, thread_id, StopReason::Entry).await?;
            }
            Start::Attach { .. } => {
                let Some(process) = target.process else {
                    self.client
                        .important("internal: the attached process is unknown")
                        .await?;
                    self.stopped(stop_id, thread_id, StopReason::Attach).await?;
                    return Ok(());
                };

                let resumed = target
                    .handle
                    .continue_execution(
                        stop_id,
                        ResumeScope::Process(process),
                        ExceptionDisposition::Pass,
                    )
                    .await;
                match resumed {
                    Ok(execution) => self.resumed = Some(execution),
                    Err(error) => {
                        self.client
                            .important(format!("cannot resume the process: {error}"))
                            .await?;
                        self.stopped(stop_id, thread_id, StopReason::Attach).await?;
                    }
                }
            }
            Start::Launch(_) => {}
        }

        Ok(())
    }

    /// Announces a started process: the `process` event, its output, its
    /// modules, and its threads. Returns the state it started in.
    async fn announce_process(
        &mut self,
        method: &'static str,
        pipes: Vec<(OwnedFd, &'static str)>,
    ) -> Result<Option<microscope::StateSnapshot>, Closed> {
        let Some(target) = self.target.as_mut() else {
            self.client
                .important("internal: the started target is gone")
                .await?;
            return Ok(None);
        };

        let snapshot = target.handle.snapshot().await.ok();
        let process = snapshot
            .as_ref()
            .and_then(|snapshot| match snapshot.inferior {
                InferiorState::Running { process_id, .. }
                | InferiorState::Stopped { process_id, .. } => Some(process_id),
                InferiorState::NotRunning => None,
            });
        let process = process.or_else(|| target.handle.core_dump().map(|core| core.process_id));
        target.process = process;
        let mut body = json!({
            "name": target.handle.executable().display().to_string(),
            "isLocalProcess": true,
            "startMethod": method,
            "pointerSize": 64,
        });

        // A core dump's process is not one on this machine.
        if let Some(process) = process.filter(|_| target.handle.core_dump().is_none()) {
            body["systemProcessId"] = process.get().into();
        }

        self.client.event("process", body).await?;

        for (read, category) in pipes {
            match output::spawn(read, category, self.client.clone()) {
                Ok(pump) => target.pumps.push(pump),
                Err(error) => {
                    self.client
                        .important(format!("cannot forward the program's {category}: {error}"))
                        .await?;
                }
            }
        }

        self.announce_modules().await?;
        if let Some(snapshot) = &snapshot {
            self.announce_threads(snapshot).await?;
        }

        Ok(snapshot)
    }

    /// Kills a launched program and launches it again, keeping the session
    /// and its breakpoints. The new arguments may change how the program is
    /// started, but not which program.
    async fn restart(&mut self, arguments: Value) -> Result<Value, ErrorBody> {
        let target = self
            .target
            .as_mut()
            .ok_or_else(|| ErrorBody::new("no program is loaded"))?;
        let Start::Launch(launch) = &mut target.start else {
            return Err(ErrorBody::new(
                "only a launched program can be restarted; start a new session instead",
            ));
        };

        if let Some(configuration) = arguments.get("arguments").filter(|value| value.is_object()) {
            let configuration = config::launch(configuration.clone()).map_err(ErrorBody::shown)?;
            let Start::Launch(new) = configuration.start else {
                return Err(ErrorBody::new(
                    "internal: a launch configuration does not launch",
                ));
            };

            if new.program != launch.program {
                return Err(ErrorBody::shown(
                    "a restart cannot change the program; start a new session for another",
                ));
            }
            *launch = new;
            target.stop_on_entry = configuration.stop_on_entry;
            target.threads = configuration.threads;
        }

        let handle = target.handle.clone();
        self.restarting = true;
        let killed = match handle.kill().await {
            Ok(()) | Err(Error::NotRunning) => Ok(()),
            Err(error) => Err(self::error(error)),
        };

        // Report the old process's end, without ending the session.
        let _ = self.drain_events().await;
        self.restarting = false;
        killed?;
        let (method, pipes) = self.begin().await?;
        self.announce_process(method, pipes).await?;
        Ok(json!({}))
    }

    /// Launches the program, or readies an attached process or core dump,
    /// returning the `process` event's start method and output pipes.
    async fn begin(&mut self) -> Result<(&'static str, Vec<(OwnedFd, &'static str)>), ErrorBody> {
        let supported = self.support().run_in_terminal;
        let Some(target) = self.target.as_mut() else {
            return Err(ErrorBody::new("internal: no target is ready to start"));
        };

        let Start::Launch(launch) = &target.start else {
            return Ok(("attach", Vec::new()));
        };

        if launch.console != config::Console::Internal {
            // The program's streams belong to the terminal.
            let execution = super::terminal::launch(
                &self.client,
                supported,
                &target.handle,
                launch,
                target.stop_on_entry,
            )
            .await?;
            self.resumed = Some(execution);
            return Ok(("launch", Vec::new()));
        }

        let (stdout_read, stdout_write) =
            output::pipe().map_err(|error| ErrorBody::shown(error.to_string()))?;
        let (stderr_read, stderr_write) =
            output::pipe().map_err(|error| ErrorBody::shown(error.to_string()))?;
        let options = LaunchOptions {
            arguments: launch.arguments.clone(),
            environment: launch.environment.clone(),
            working_directory: launch.working_directory.clone(),
            stdin: Some(std::process::Stdio::null()),
            stdout: Some(stdout_write.into()),
            stderr: Some(stderr_write.into()),
            stop_at_entry: target.stop_on_entry,
        };

        let execution = target.handle.launch_with(options).await.map_err(|error| {
            ErrorBody::shown(format!(
                "failed to launch {}: {error}",
                launch.program.display()
            ))
        })?;
        self.resumed = Some(execution);
        Ok((
            "launch",
            vec![(stdout_read, "stdout"), (stderr_read, "stderr")],
        ))
    }

    async fn disconnect(&mut self, arguments: Value) -> Result<Value, ErrorBody> {
        let arguments = parse::<protocol::DisconnectArguments>(arguments, "disconnect arguments")?;

        // A restart attaches to the process again, so it must survive the
        // disconnect that begins the restart, whatever else that asks.
        let attached = matches!(
            self.target.as_ref().map(|target| &target.start),
            Some(Start::Attach { .. })
        );
        let terminate = if attached && arguments.restart == Some(true) {
            Some(false)
        } else {
            arguments.terminate_debuggee
        };

        self.end(terminate).await?;
        self.after = Some(After::End);
        Ok(json!({}))
    }

    /// Ends the session: kills a launched program, or one attached to when
    /// asked, detaches from the rest, and reports the program's end.
    async fn end(&mut self, terminate: Option<bool>) -> Result<(), Closed> {
        self.ended = true;
        let Some(target) = self.target.as_mut() else {
            return Ok(());
        };

        let launched = matches!(target.start, Start::Launch(_));
        if terminate.unwrap_or(launched)
            && target.process.is_some()
            && target.handle.core_dump().is_none()
            && let Err(error) = target.handle.kill().await
            && !matches!(error, Error::NotRunning)
        {
            self.client
                .important(format!("cannot kill the program: {error}"))
                .await?;
        }

        let shutdown = match target.debugger.take() {
            Some(debugger) => debugger.shutdown().await,
            None => Ok(()),
        };

        if let Err(error) = shutdown {
            self.client
                .important(format!("the debugger did not shut down cleanly: {error}"))
                .await?;
        }

        // Report the exit the kill caused, then the session's end.
        self.drain_events().await?;
        if self
            .target
            .as_ref()
            .is_some_and(|target| target.process.is_some())
        {
            self.ended_target().await?;
        }

        Ok(())
    }

    /// Reports the debugger events already received.
    async fn drain_events(&mut self) -> Result<(), Closed> {
        while let Some(event) = self
            .events
            .as_mut()
            .and_then(|events| events.try_recv().ok())
        {
            self.event(event).await?;
        }

        Ok(())
    }

    async fn terminate(&self) -> Result<Value, ErrorBody> {
        let Some(handle) = self.handle() else {
            return Ok(json!({}));
        };

        match handle.terminate().await {
            Ok(()) | Err(Error::NotRunning) => Ok(json!({})),
            Err(error) => Err(self::error(error)),
        }
    }

    /// Releases what the session holds once it is over. Children held but
    /// not yet offered to the client are released; those offered wait for
    /// its answer.
    async fn release(&mut self) {
        if let Some(target) = self.target.as_mut() {
            if let Some(debugger) = target.debugger.take() {
                let _ = debugger.shutdown().await;
            }

            for pump in target.pumps.drain(..) {
                pump.abort();
            }
        }

        self.held = None;
        for follow in self.follows.drain(..) {
            let _ = follow.await;
        }
    }

    fn handle(&self) -> Option<&DebuggerHandle> {
        self.target.as_ref().map(|target| &target.handle)
    }

    /// The target's debugger, for requests that need a program.
    pub(super) fn target_handle(&self) -> Result<DebuggerHandle, ErrorBody> {
        self.handle()
            .cloned()
            .ok_or_else(|| ErrorBody::new("no program is loaded"))
    }

    /// The stop the client was told about, for requests that need one.
    pub(super) fn current_stop(&self) -> Result<Stop, ErrorBody> {
        self.stop.clone().ok_or_else(|| {
            if self
                .target
                .as_ref()
                .is_some_and(|target| target.process.is_some())
            {
                ErrorBody::not_stopped()
            } else {
                ErrorBody::new("the program is not running")
            }
        })
    }

    /// Whether the client cancelled the request with this `seq`.
    fn take_cancellation(&self, seq: &Value) -> bool {
        self.cancelled
            .lock()
            .is_ok_and(|mut cancelled| cancelled.remove(&seq.to_string()))
    }

    /// Cancels queued requests that inspect the stop a resume leaves, which
    /// the client no longer needs. A console command is kept, since it may
    /// do more than inspect.
    fn cancel_inspections(&mut self) {
        for message in &mut self.queue {
            if let Inbound::Request {
                seq,
                command,
                arguments,
            } = message
                && matches!(
                    command.as_str(),
                    "stackTrace"
                        | "scopes"
                        | "variables"
                        | "evaluate"
                        | "disassemble"
                        | "readMemory"
                )
                && arguments["context"] != "repl"
            {
                *message = Inbound::Cancelled {
                    seq: seq.clone(),
                    command: std::mem::take(command),
                };
            }
        }
    }

    async fn continue_execution(&mut self, arguments: Value) -> Result<Value, ErrorBody> {
        let arguments = parse::<ThreadArguments>(arguments, "continue arguments")?;
        let stop = self.current_stop()?;
        let single = arguments.single_thread.unwrap_or(false);
        let scope = self.resume_scope(single, &arguments)?;
        let handle = self.target_handle()?;
        let execution = handle
            .continue_execution(stop.id, scope, ExceptionDisposition::Pass)
            .await
            .map_err(error)?;
        self.resumed = Some(execution);
        self.leave_stop();
        self.cancel_inspections();

        Ok(json!({"allThreadsContinued": !single}))
    }

    fn resume_scope(
        &self,
        single: bool,
        arguments: &ThreadArguments,
    ) -> Result<ResumeScope, ErrorBody> {
        if single {
            let context = self.thread_ids.context(arguments.thread_id)?;
            return context
                .as_thread()
                .map(ResumeScope::Thread)
                .ok_or_else(|| ErrorBody::new(format!("{context} cannot run alone")));
        }

        self.target
            .as_ref()
            .and_then(|target| target.process)
            .map(ResumeScope::Process)
            .ok_or_else(|| ErrorBody::new("the program is not running"))
    }

    async fn step(
        &mut self,
        arguments: Value,
        source: StepKind,
        instruction: StepKind,
    ) -> Result<Value, ErrorBody> {
        let arguments = parse::<ThreadArguments>(arguments, "step arguments")?;
        let stop = self.current_stop()?;
        let context = self.thread_ids.context(arguments.thread_id)?;
        let single = arguments.single_thread.unwrap_or(false);
        let scope = self.resume_scope(single, &arguments)?;
        let kind = if arguments.granularity.as_deref() == Some("instruction") {
            instruction
        } else {
            source
        };

        let handle = self.target_handle()?;

        // A step in may go into one target of `stepInTargets`.
        let call = match arguments.target_id {
            Some(target) if kind == StepKind::IntoSource => {
                Some(self.references.call_of(target).ok_or_else(|| {
                    ErrorBody::new("the step-in target belongs to an earlier stop")
                })?)
            }
            _ => None,
        };

        let execution = match call {
            Some(call) => {
                handle
                    .start_step_into(stop.id, context, call, scope, ExceptionDisposition::Pass)
                    .await
            }
            None => {
                handle
                    .start_step(
                        stop.id,
                        context,
                        StackFrameId::INNERMOST,
                        kind,
                        scope,
                        ExceptionDisposition::Pass,
                    )
                    .await
            }
        }
        .map_err(error)?;
        self.resumed = Some(execution);
        self.leave_stop();
        self.cancel_inspections();

        // Without this, clients assume only the stepping thread runs.
        self.client
            .event(
                "continued",
                json!({"threadId": arguments.thread_id, "allThreadsContinued": !single}),
            )
            .await?;

        Ok(json!({}))
    }

    /// The calls of a frame's line that a step in can go into, which only
    /// the innermost frame has.
    async fn step_in_targets(&mut self, arguments: Value) -> Result<Value, ErrorBody> {
        let arguments = parse::<protocol::ScopesArguments>(arguments, "stepInTargets arguments")?;
        self.current_stop()?;
        let context = self
            .references
            .frame_context(arguments.frame_id)
            .ok_or_else(|| ErrorBody::new("the frame belongs to an earlier stop"))?;
        if context.frame != StackFrameId::INNERMOST {
            return Ok(json!({"targets": []}));
        }

        let handle = self.target_handle()?;
        let listed = handle.at(context).step_targets().await.map_err(error)?;
        let mut targets = Vec::with_capacity(listed.len());

        for target in listed.iter() {
            let id = self.references.step_target(target.call)?;
            let label = match (&target.callee, target.target) {
                (Some(callee), _) => callee.to_string(),
                (None, Some(address)) => format!("call to {address}"),
                (None, None) => format!("indirect call at {}", target.call),
            };

            targets.push(json!({"id": id, "label": label}));
        }

        Ok(json!({"targets": targets}))
    }

    /// Moves a thread to a target `gotoTargets` named, without running it;
    /// the stop it publishes again follows as a `goto` stop.
    async fn goto(&self, arguments: Value) -> Result<Value, ErrorBody> {
        let arguments = parse::<GotoArguments>(arguments, "goto arguments")?;
        let stop = self.current_stop()?;
        let context = self.thread_ids.context(arguments.thread_id)?;
        let target = self
            .references
            .target_of(arguments.target_id)
            .cloned()
            .ok_or_else(|| ErrorBody::new("the goto target belongs to an earlier stop"))?;
        let handle = self.target_handle()?;
        handle
            .start_jump(stop.id, context, target)
            .await
            .map_err(error)?;

        Ok(json!({}))
    }

    async fn pause(&self) -> Result<Value, ErrorBody> {
        let handle = self.target_handle()?;
        match handle.pause().await {
            Ok(_) | Err(Error::AlreadyStopped) => Ok(json!({})),
            Err(Error::NotRunning) => Err(ErrorBody::new("the program is not running")),
            Err(error) => Err(self::error(error)),
        }
    }

    /// Says the process executed another program, whose recorded source
    /// paths replace the old program's.
    async fn program_changed(
        &mut self,
        process: microscope::ProcessId,
        program: &Path,
    ) -> Result<(), Closed> {
        if let Some(target) = self.target.as_mut() {
            target.recorded_paths = None;
        }

        self.client
            .console(crate::cli::format::program_changed(
                process,
                program,
                crate::cli::terminal::Renderer::new(false),
            ))
            .await
    }

    /// Forgets the current stop and everything that referred to it.
    fn leave_stop(&mut self) {
        self.stop = None;
        self.references.clear();
        self.backtraces.clear();
        self.variables.clear();
    }

    async fn event(&mut self, event: DebuggerEvent) -> Result<(), Closed> {
        match event {
            DebuggerEvent::InferiorLaunched { process_id, .. } => {
                if let Some(target) = self.target.as_mut() {
                    target.process = Some(process_id);
                }

                // The first thread's id is the process id.
                self.announce_thread(ThreadId::new(process_id.get()))
                    .await?;
            }
            DebuggerEvent::InferiorAttached { process_id, .. } => {
                if let Some(target) = self.target.as_mut() {
                    target.process = Some(process_id);
                }
            }
            DebuggerEvent::InferiorContinued { execution_id, .. } => {
                if self.resumed != Some(execution_id) {
                    self.resumed_elsewhere().await?;
                }
            }

            DebuggerEvent::InferiorStopped {
                stop_id,
                thread_id,
                reason,
                ..
            } => {
                // Catching up after missed events may have reported it.
                if self.stop.as_ref().map(|stop| stop.id) != Some(stop_id) {
                    self.stopped(stop_id, thread_id, reason).await?;
                }
            }
            DebuggerEvent::ThreadStarted { thread_id, .. } => {
                self.announce_thread(thread_id).await?;
            }
            DebuggerEvent::ThreadExited { thread_id, .. } => {
                if let Some(id) = self.threads.remove(&thread_id) {
                    self.client.thread("exited", id).await?;
                }
            }
            DebuggerEvent::ModuleLoaded { module, .. } => self.announce_module(&module).await?,
            DebuggerEvent::ModuleUnloaded { module, .. } => {
                self.forget_module(module.module.id).await?;
            }
            DebuggerEvent::InferiorExited { status, .. } => self.exited(&status).await?,
            DebuggerEvent::InferiorDetached { .. } => {
                if let Some(target) = self.target.as_mut() {
                    target.process = None;
                }

                self.leave_stop();
                self.client.event("terminated", json!({})).await?;
            }

            DebuggerEvent::SignalReceived {
                thread_id,
                exception,
                ..
            } => {
                self.client
                    .console(crate::cli::format::signal_received(
                        thread_id,
                        &exception,
                        crate::cli::terminal::Renderer::new(false),
                    ))
                    .await?;
            }
            DebuggerEvent::ChildReleased {
                process_id, child, ..
            } => {
                self.client
                    .console(crate::cli::format::child_released(
                        process_id,
                        child,
                        crate::cli::terminal::Renderer::new(false),
                    ))
                    .await?;
            }
            DebuggerEvent::ProgramChanged {
                process_id,
                program,
                ..
            } => self.program_changed(process_id, &program.path).await?,
            DebuggerEvent::BreakpointsChanged { .. } => self.sync_breakpoints().await?,
            DebuggerEvent::LogMessage { parts, .. } => {
                self.client
                    .console(crate::cli::format::log_message(&parts))
                    .await?;
            }
            DebuggerEvent::ConditionFailed { owner, error, .. } => {
                self.condition_failed(owner, &error).await?;
            }
            DebuggerEvent::WatchpointsInvalidated { invalidated, .. } => {
                self.data_invalidated(&invalidated).await?;
            }
            DebuggerEvent::WatchpointsChanged { .. } => self.sync_data().await?,
            DebuggerEvent::CatchpointsChanged { .. } | DebuggerEvent::StateChanged { .. } => {}
        }

        Ok(())
    }

    /// Tells the client the program resumed without its asking, such as
    /// to receive a termination signal.
    async fn resumed_elsewhere(&mut self) -> Result<(), Closed> {
        let Some(stop) = self.stop.take() else {
            return Ok(());
        };

        self.leave_stop();
        let Some(id) = self.client_id(stop.context) else {
            return Ok(());
        };

        self.client
            .event(
                "continued",
                json!({"threadId": id, "allThreadsContinued": true}),
            )
            .await
    }

    /// Explains a stop that a breakpoint's or data breakpoint's unevaluable
    /// condition caused, naming it by the client's id where it has one.
    async fn condition_failed(
        &self,
        owner: microscope::ConditionOwner,
        error: &str,
    ) -> Result<(), Closed> {
        let subject = match owner {
            microscope::ConditionOwner::Breakpoint(breakpoint) => {
                let id = self
                    .breakpoints
                    .entries()
                    .find(|(_, entry)| entry.breakpoint() == Some(breakpoint))
                    .map_or_else(|| breakpoint.to_string(), |(_, entry)| entry.id.to_string());
                format!("breakpoint {id}")
            }
            microscope::ConditionOwner::Watchpoint(watchpoint) => self
                .data
                .entries
                .iter()
                .find(|entry| entry.watchpoint == Ok(watchpoint))
                .map_or_else(
                    || format!("watchpoint {watchpoint}"),
                    |entry| format!("data breakpoint {}", entry.id),
                ),
            microscope::ConditionOwner::Catchpoint(catchpoint) => {
                format!("system-call catchpoint {catchpoint}")
            }
        };

        self.client
            .important(format!(
                "{subject} stopped because its condition could not be evaluated: {error}"
            ))
            .await
    }

    /// Reports a stop to the client.
    async fn stopped(
        &mut self,
        stop: StopId,
        thread: ThreadId,
        reason: StopReason,
    ) -> Result<(), Closed> {
        self.leave_stop();
        let snapshot = match self.target_handle() {
            Ok(handle) => handle.snapshot().await.ok(),
            Err(_) => None,
        };

        let context = self.stopped_context(snapshot.as_ref(), thread);
        let selected = snapshot
            .as_ref()
            .filter(|snapshot| snapshot.selected == Some(ExecutionContext::Thread(thread)))
            .and_then(|snapshot| snapshot.selected_frame)
            .unwrap_or(StackFrameId::INNERMOST);
        self.announce(context).await?;
        let mut body = json!({
            "allThreadsStopped": true,
            "preserveFocusHint": false,
        });
        if let Some(id) = self.client_id(context) {
            body["threadId"] = id.into();
        }

        let (kind, description, text) = match &reason {
            StopReason::Breakpoint { hits, .. } => {
                let (ids, kind) = self.breakpoints.hit(hits);
                body["hitBreakpointIds"] = ids.into();
                (kind, None, None)
            }
            StopReason::Watchpoint { hits } => {
                body["hitBreakpointIds"] = self.data.hit(hits).into();
                (
                    "data breakpoint",
                    Some(self.watch_description(hits).await),
                    None,
                )
            }
            StopReason::WatchpointInvalidated { invalidated } => {
                self.data_invalidated(invalidated).await?;
                (
                    "data breakpoint",
                    Some(
                        invalidated
                            .iter()
                            .map(|entry| {
                                format!(
                                    "the watch on {} ended: {}",
                                    crate::cli::format::watch_subject(&entry.watchpoint),
                                    crate::cli::format::invalidation_text(entry.reason)
                                )
                            })
                            .collect::<Vec<_>>()
                            .join("; "),
                    ),
                    None,
                )
            }
            StopReason::Exited(_) => return Ok(()),
            other => describe_stop(other),
        };

        body["reason"] = kind.into();
        let told = kind;
        if let Some(description) = description {
            body["description"] = description.into();
        }

        if let Some(text) = text {
            body["text"] = text.into();
        }

        self.stop = Some(Stop {
            id: stop,
            thread,
            context,
            reason,
            told,
            selected,
        });

        self.client.event("stopped", body).await
    }

    async fn exited(&mut self, status: &ExitStatus) -> Result<(), Closed> {
        if let Some(target) = self.target.as_mut() {
            // The program's last output comes before its exit.
            for mut pump in target.pumps.drain(..) {
                if tokio::time::timeout(OUTPUT_DRAIN, &mut pump).await.is_err() {
                    pump.abort();
                }
            }
        }

        self.leave_stop();
        if let ExitStatus::Terminated(info) = status {
            self.client
                .important(format!(
                    "the program was terminated by {} ({})",
                    signal_text(info.code),
                    info.description
                ))
                .await?;
        }

        self.client
            .event("exited", json!({"exitCode": exit_code(status)}))
            .await?;

        self.ended_target().await
    }

    /// Reports that the program is gone.
    async fn ended_target(&mut self) -> Result<(), Closed> {
        if let Some(target) = self.target.as_mut() {
            target.process = None;
        }

        // The program's threads end with it, also for a client that keeps
        // the session for a restart.
        let tasks = std::mem::take(&mut self.tasks).into_values();

        for id in std::mem::take(&mut self.threads).into_values().chain(tasks) {
            self.client.thread("exited", id).await?;
        }

        if self.restarting {
            return Ok(());
        }

        self.client.event("terminated", json!({})).await
    }

    /// Tells the client which tasks it was told about have ended.
    pub(super) async fn forget_tasks(
        &mut self,
        live: &[microscope::TaskSnapshot],
    ) -> Result<(), Closed> {
        let live = live.iter().map(|task| task.id).collect::<BTreeSet<_>>();
        let gone = self
            .tasks
            .keys()
            .filter(|task| !live.contains(task))
            .copied()
            .collect::<Vec<_>>();

        for task in gone {
            if let Some(id) = self.tasks.remove(&task) {
                self.client.thread("exited", id).await?;
            }
        }

        Ok(())
    }

    /// What the client's threads are, once a program is loaded.
    pub(super) fn thread_listing(&self) -> Option<config::ThreadListing> {
        self.target.as_ref().map(|target| target.threads)
    }

    /// What the client knows as a stopped thread: the task it runs, when
    /// the client's threads are tasks and it runs one, or the thread.
    fn stopped_context(
        &self,
        snapshot: Option<&microscope::StateSnapshot>,
        thread: ThreadId,
    ) -> ExecutionContext {
        let lists_tasks = self
            .target
            .as_ref()
            .is_some_and(|target| target.threads.tasks);
        snapshot
            .filter(|_| lists_tasks)
            .and_then(|snapshot| {
                snapshot
                    .threads
                    .iter()
                    .find(|listed| listed.id == thread)
                    .and_then(|listed| match listed.activity {
                        Some(microscope::ThreadActivity::Task { task, .. }) => {
                            Some(ExecutionContext::Task(task))
                        }
                        _ => None,
                    })
            })
            .unwrap_or(ExecutionContext::Thread(thread))
    }

    /// The client's id for a thread or task it was told about.
    fn client_id(&self, context: ExecutionContext) -> Option<i64> {
        match context {
            ExecutionContext::Thread(thread) => self.threads.get(&thread).copied(),
            ExecutionContext::Task(task) => self.tasks.get(&task).copied(),
        }
    }

    /// Tells the client of a thread or task it does not know yet.
    async fn announce(&mut self, context: ExecutionContext) -> Result<(), Closed> {
        let task = match context {
            ExecutionContext::Thread(thread) => return self.announce_thread(thread).await,
            ExecutionContext::Task(task) => task,
        };

        if self.tasks.contains_key(&task) {
            return Ok(());
        }

        match self.thread_ids.id(context) {
            Ok(id) => {
                self.tasks.insert(task, id);
                self.client.thread("started", id).await
            }
            Err(error) => self.client.important(error.short).await,
        }
    }

    async fn announce_thread(&mut self, thread: ThreadId) -> Result<(), Closed> {
        if self.threads.contains_key(&thread) {
            return Ok(());
        }

        match self.thread_ids.id(ExecutionContext::Thread(thread)) {
            Ok(id) => {
                self.threads.insert(thread, id);
                self.client.thread("started", id).await
            }
            Err(error) => self.client.important(error.short).await,
        }
    }

    /// Announces the snapshot's threads not yet announced, and the exit of
    /// announced threads it no longer has.
    async fn announce_threads(
        &mut self,
        snapshot: &microscope::StateSnapshot,
    ) -> Result<(), Closed> {
        let live = snapshot
            .threads
            .iter()
            .map(|thread| thread.id)
            .collect::<BTreeSet<_>>();
        let gone = self
            .threads
            .keys()
            .filter(|thread| !live.contains(thread))
            .copied()
            .collect::<Vec<_>>();

        for thread in gone {
            if let Some(id) = self.threads.remove(&thread) {
                self.client.thread("exited", id).await?;
            }
        }

        for thread in live {
            self.announce_thread(thread).await?;
        }

        Ok(())
    }

    /// Tells the client a module is gone.
    pub(super) async fn forget_module(&mut self, id: ModuleId) -> Result<(), Closed> {
        if let Some(target) = self.target.as_mut() {
            target.images.remove(&id);
        }

        let Some(record) = self.modules.remove(&id) else {
            return Ok(());
        };

        self.client
            .event(
                "module",
                json!({"reason": "removed", "module": super::sources::module_json(&record, None)}),
            )
            .await?;

        self.remove_loaded_sources(id).await
    }

    /// Catches up after missing events: announces threads and modules, and
    /// reports the current stop, resume, or exit the client has not heard
    /// about.
    async fn resync(&mut self) -> Result<(), Closed> {
        let Some(handle) = self.handle().cloned() else {
            return Ok(());
        };

        let Ok(snapshot) = handle.snapshot().await else {
            return Ok(());
        };

        self.announce_threads(&snapshot).await?;
        self.announce_modules().await?;
        match snapshot.inferior {
            InferiorState::Stopped {
                stop_id,
                thread_id,
                reason,
                ..
            } => {
                if self.stop.as_ref().map(|stop| stop.id) != Some(stop_id) {
                    self.stopped(stop_id, thread_id, reason).await?;
                }
            }
            InferiorState::Running { .. } => self.resumed_elsewhere().await?,
            InferiorState::NotRunning => {
                if self
                    .target
                    .as_ref()
                    .is_some_and(|target| target.process.is_some())
                {
                    self.leave_stop();
                    self.client
                        .important("the program ended, but its exit status was missed")
                        .await?;
                    self.ended_target().await?;
                }
            }
        }

        Ok(())
    }

    // Breakpoints.

    async fn set_breakpoints(&mut self, arguments: Value) -> Result<Value, ErrorBody> {
        let arguments = parse::<SetBreakpointsArguments>(arguments, "setBreakpoints arguments")?;
        let path = arguments.source.path.ok_or_else(|| {
            ErrorBody::new(
                "breakpoints need a source with a path; source references are not supported",
            )
        })?;
        let lines_start_at1 = self.support().lines_start_at1;
        let to_line = |line: i64| -> Key {
            let from_one = if lines_start_at1 {
                line
            } else {
                line.saturating_add(1)
            };

            match u64::try_from(from_one) {
                Ok(from_one) if from_one > 0 => Key::Line(from_one),
                _ => Key::Invalid(format!("line {line} does not exist")),
            }
        };

        let wants = match (arguments.breakpoints, arguments.lines) {
            (Some(breakpoints), _) => breakpoints
                .into_iter()
                .map(|breakpoint| {
                    Want::new(
                        to_line(breakpoint.line),
                        breakpoint.condition,
                        breakpoint.hit_condition,
                        breakpoint.log_message,
                    )
                })
                .collect(),
            (None, Some(lines)) => lines
                .into_iter()
                .map(|line| Want::at(to_line(line)))
                .collect(),
            (None, None) => Vec::new(),
        };

        self.replace_group(Group::Source(PathBuf::from(path)), wants)
            .await
    }

    async fn set_function_breakpoints(&mut self, arguments: Value) -> Result<Value, ErrorBody> {
        let arguments = parse::<SetFunctionBreakpointsArguments>(
            arguments,
            "setFunctionBreakpoints arguments",
        )?;
        let wants = arguments
            .breakpoints
            .into_iter()
            .map(|breakpoint| {
                Want::new(
                    Key::Function(breakpoint.name.trim().to_owned()),
                    breakpoint.condition,
                    breakpoint.hit_condition,
                    None,
                )
            })
            .collect();
        self.replace_group(Group::Functions, wants).await
    }

    async fn set_instruction_breakpoints(&mut self, arguments: Value) -> Result<Value, ErrorBody> {
        let arguments = parse::<protocol::SetInstructionBreakpointsArguments>(
            arguments,
            "setInstructionBreakpoints arguments",
        )?;
        let wants = arguments
            .breakpoints
            .into_iter()
            .map(|breakpoint| {
                let key = protocol::address(&breakpoint.instruction_reference)
                    .and_then(|address| protocol::offset(address, breakpoint.offset))
                    .map_or_else(
                        || {
                            Key::Invalid(format!(
                                "invalid instruction reference '{}' with offset {}",
                                breakpoint.instruction_reference,
                                breakpoint.offset.unwrap_or(0)
                            ))
                        },
                        Key::Instruction,
                    );
                Want::new(key, breakpoint.condition, breakpoint.hit_condition, None)
            })
            .collect();
        self.replace_group(Group::Instructions, wants).await
    }

    /// Replaces a group of breakpoints and answers with its new entries in
    /// request order.
    async fn replace_group(&mut self, group: Group, wants: Vec<Want>) -> Result<Value, ErrorBody> {
        let plan = self.breakpoints.plan(&group, wants);
        let mut release = plan.release.into_iter();
        while let Some(entry) = release.next() {
            if let Err(error) = self.release_breakpoint(&entry).await {
                self.breakpoints
                    .abandon(group, plan.slots, std::iter::once(entry).chain(release));
                return Err(error);
            }
        }

        let mut entries = Vec::with_capacity(plan.slots.len());

        for slot in plan.slots {
            entries.push(match slot {
                Slot::Keep(entry) => entry,
                Slot::Resolve { id, want } => {
                    let state = self.resolve(&group, &want).await;
                    Entry { id, want, state }
                }
            });
        }

        let body = entries
            .iter()
            .map(|entry| self.breakpoint_json(&group, entry))
            .collect::<Vec<_>>();
        self.breakpoints.install(group, entries);

        Ok(json!({"breakpoints": body}))
    }

    async fn release_breakpoint(&mut self, entry: &Entry) -> Result<(), ErrorBody> {
        let Some(breakpoint) = entry.breakpoint() else {
            return Ok(());
        };

        if self.breakpoints.release(breakpoint)
            && let Some(handle) = self.handle()
        {
            match handle.remove_breakpoint(breakpoint).await {
                Ok(_) | Err(Error::BreakpointNotFound(_)) => {}
                Err(error) => {
                    // It may still be installed, and its entry still owns it.
                    self.breakpoints.acquire(breakpoint);
                    return Err(self::error(error));
                }
            }
        }

        Ok(())
    }

    /// Installs one breakpoint, or explains why it is not installed.
    async fn resolve(&mut self, group: &Group, want: &Want) -> State {
        let failed = |message: String| State::Unresolved {
            message,
            pending: false,
        };

        let Some(handle) = self.handle().cloned() else {
            return State::Unresolved {
                message: "the program is not loaded yet".to_owned(),
                pending: true,
            };
        };

        if let Key::Invalid(message) = &want.key {
            return failed(message.clone());
        }

        let options = match breakpoint_options(want) {
            Ok(options) => options,
            Err(error) => return failed(error.to_string()),
        };

        let spec = match (group, &want.key) {
            (Group::Source(path), Key::Line(line)) => {
                let Some(line) = LineNumber::new(*line) else {
                    return failed("line 0 does not exist".to_owned());
                };

                BreakpointSpec::Source {
                    path: self.recorded_path(path),
                    line,
                }
            }
            (_, Key::Function(name)) => match crate::cli::commands::parse_breakpoint_location(name)
            {
                Ok(Some(spec)) => spec,
                Ok(None) => {
                    return failed(format!(
                        "'{name}' is not a function, 0xaddress, file:line, or file:function"
                    ));
                }
                Err(error) => return failed(format!("{error:#}")),
            },
            (_, Key::Instruction(address)) => {
                BreakpointSpec::Address(VirtualAddress::new(*address))
            }
            _ => return failed("this breakpoint cannot be placed".to_owned()),
        };

        let added = handle.add_breakpoint_with(spec, options).await;
        match added {
            Ok(breakpoint) => {
                self.breakpoints.acquire(breakpoint.id);
                self.state_of(&breakpoint).await
            }
            Err(error) => failed(error.to_string()),
        }
    }

    /// Resolves breakpoints that were waiting for the program and tells
    /// the client which now are.
    async fn resolve_unresolved(&mut self) -> Result<(), Closed> {
        for (group, entry) in self.breakpoints.unresolved() {
            let state = self.resolve(&group, &entry.want).await;
            let entry = Entry { state, ..entry };
            self.breakpoints.replace(&group, entry.clone());
            self.client
                .event(
                    "breakpoint",
                    json!({"reason": "changed", "breakpoint": self.breakpoint_json(&group, &entry)}),
                )
                .await?;
        }

        Ok(())
    }

    /// Adopts breakpoints made or deleted from the console.
    async fn sync_breakpoints(&mut self) -> Result<(), Closed> {
        let Some(handle) = self.handle().cloned() else {
            return Ok(());
        };

        let Ok(snapshot) = handle.snapshot().await else {
            return Ok(());
        };

        for change in self.breakpoints.sync(&snapshot.breakpoints) {
            let (reason, mut entry) = match change {
                Change::New(entry) => ("new", entry),
                Change::Removed(entry) => ("removed", entry),
            };

            if reason == "new"
                && let Some(core) = snapshot
                    .breakpoints
                    .iter()
                    .find(|core| Some(core.id) == entry.breakpoint())
            {
                entry.state = self.state_of(core).await;
                self.breakpoints.replace(&Group::Console, entry.clone());
            }

            self.client
                .event(
                    "breakpoint",
                    json!({"reason": reason, "breakpoint": self.breakpoint_json(&Group::Console, &entry)}),
                )
                .await?;
        }

        // Libraries loading and unloading move breakpoints in and out of
        // pending.
        for (group, entry) in self.breakpoints.owned_entries() {
            let Some(core) = snapshot
                .breakpoints
                .iter()
                .find(|core| Some(core.id) == entry.breakpoint())
            else {
                continue;
            };

            let state = self.state_of(core).await;
            if state == entry.state {
                continue;
            }

            let entry = Entry { state, ..entry };
            self.breakpoints.replace(&group, entry.clone());
            self.client
                .event(
                    "breakpoint",
                    json!({"reason": "changed", "breakpoint": self.breakpoint_json(&group, &entry)}),
                )
                .await?;
        }

        Ok(())
    }

    /// The state a debugger breakpoint gives the client's breakpoint: where
    /// it resolved, or that it waits for a module with code for it.
    async fn state_of(&mut self, breakpoint: &microscope::Breakpoint) -> State {
        if !breakpoint.enabled {
            return State::Disabled {
                breakpoint: breakpoint.id,
            };
        }

        if breakpoint.ready_locations().next().is_none() {
            return State::Pending {
                breakpoint: breakpoint.id,
                message: crate::present::breakpoints::pending_message(breakpoint).unwrap_or_else(|| format!(
                    "no loaded module has code for {}; the breakpoint resolves when one that does loads",
                    breakpoint.spec
                )),
            };
        }

        State::Resolved {
            breakpoint: breakpoint.id,
            placement: self.placement(breakpoint).await,
            message: crate::present::breakpoints::pending_message(breakpoint),
        }
    }

    /// Where a debugger breakpoint resolved: its first location's source
    /// line and address.
    async fn placement(&mut self, breakpoint: &microscope::Breakpoint) -> Placement {
        let Some(handle) = self.handle().cloned() else {
            return Placement::default();
        };

        let Some(first) = breakpoint.ready_locations().next() else {
            return Placement::default();
        };

        // A location's image, and its address there.
        let (image, address, virtual_address) = match (&first.location, first.library) {
            (microscope::BreakpointLocation::Image(address), _) => {
                (handle.module_image(), *address, None)
            }

            // A runtime's location has no address: it is where the runtime
            // says it is.
            (microscope::BreakpointLocation::Managed(managed), _) => {
                return Placement {
                    source: match &managed.place {
                        microscope::ManagedPlace::Line { path, line } => {
                            Some((self.local_path(Path::new(&**path)), u64::from(*line)))
                        }
                        microscope::ManagedPlace::Function { .. } => None,
                    },
                    address: None,
                };
            }
            (microscope::BreakpointLocation::Virtual(address), Some(library)) => {
                let bias = handle.loaded_modules().await.ok().and_then(|snapshot| {
                    snapshot
                        .modules
                        .iter()
                        .find(|record| record.module.id == library)
                        .map(|record| record.module.load_bias)
                });
                match (self.image(library).await, bias) {
                    (Some(image), Some(bias)) => {
                        (image, bias.wrapping_unload(*address), Some(*address))
                    }
                    _ => {
                        return Placement {
                            source: None,
                            address: Some(address.get()),
                        };
                    }
                }
            }
            (microscope::BreakpointLocation::Virtual(address), None) => {
                return Placement {
                    source: None,
                    address: Some(address.get()),
                };
            }
        };

        let source = image.source_location(address).and_then(|location| {
            let file = image.source_file(location.file)?;
            let line = match &breakpoint.spec {
                // A line keeps the file the client named.
                BreakpointSpec::Source { path, line } => image
                    .source_file_matching(path)
                    .ok()
                    .and_then(|file| image.breakpoint_line(file.id, *line))
                    .unwrap_or(location.line),
                _ => location.line,
            };

            Some((self.local_path(&file.path), line.get()))
        });

        Placement {
            source,
            address: virtual_address.map(microscope::VirtualAddress::get),
        }
    }

    fn breakpoint_json(&self, group: &Group, entry: &Entry) -> Value {
        let mut body = json!({"id": entry.id});
        let (message, reason) = match &entry.state {
            State::Resolved {
                placement, message, ..
            } => {
                body["verified"] = true.into();
                if let Some(message) = message {
                    body["message"] = message.clone().into();
                }
                if let Some((path, line)) = &placement.source {
                    body["line"] = self.line_to_client(*line).into();
                    body["source"] = match group {
                        Group::Source(client) => source_json(client),
                        _ => source_json(path),
                    };
                }

                if let Some(address) = placement.address {
                    body["instructionReference"] = format!("{address:#x}").into();
                }

                return body;
            }
            State::Pending { message, .. } => (message.clone(), Some("pending")),
            State::Disabled { breakpoint } => (
                format!("disabled; enable {breakpoint} in the debug console"),
                None,
            ),
            State::Unresolved { message, pending } => (
                message.clone(),
                Some(if *pending { "pending" } else { "failed" }),
            ),
        };

        body["verified"] = false.into();
        body["message"] = message.into();
        if let Some(reason) = reason {
            body["reason"] = reason.into();
        }

        if let (Group::Source(client), Key::Line(line)) = (group, &entry.want.key) {
            body["line"] = self.line_to_client(*line).into();
            body["source"] = source_json(client);
        }

        body
    }

    async fn set_exception_breakpoints(&mut self, arguments: Value) -> Result<Value, ErrorBody> {
        let arguments = parse::<SetExceptionBreakpointsArguments>(
            arguments,
            "setExceptionBreakpoints arguments",
        )?;
        let (selection, breakpoints) = Selection::parse(&arguments);
        self.exceptions = selection;
        self.apply_signal_policies().await?;
        Ok(json!({"breakpoints": breakpoints}))
    }

    /// Makes the debugger stop on the signals and exceptions the exception
    /// filters select, with the configuration's per-signal handling
    /// applied over them.
    async fn apply_signal_policies(&mut self) -> Result<(), ErrorBody> {
        let Some(target) = self.target.as_mut() else {
            return Ok(());
        };

        let exceptions = self.exceptions.exceptions();
        if target.applied_exceptions != exceptions {
            target
                .handle
                .set_exception_stops(exceptions)
                .await
                .map_err(error)?;
            target.applied_exceptions = exceptions;
        }

        let syscalls = self.exceptions.syscalls();
        if target.applied_syscalls.as_ref().map(|(calls, _)| calls) != syscalls {
            if let Some((_, id)) = target.applied_syscalls.take() {
                target.handle.remove_catchpoint(id).await.map_err(error)?;
            }

            if let Some(calls) = syscalls {
                let catchpoint = target
                    .handle
                    .add_catchpoint(Arc::clone(calls), microscope::CatchPhase::Both, None)
                    .await
                    .map_err(error)?;
                target.applied_syscalls = Some((Arc::clone(calls), catchpoint.id));
            }
        }

        for code in microscope::signal_codes() {
            let mut policy = target.default_policies[&code];
            policy.stop = self.exceptions.stops(code);
            policy.print |= policy.stop;

            for (_, actions) in target.signals.iter().filter(|(signal, _)| *signal == code) {
                for action in actions {
                    crate::cli::commands::apply_signal_action(&mut policy, action)
                        .map_err(|error| ErrorBody::new(error.to_string()))?;
                }
            }

            if target.applied_policies.get(&code) != Some(&policy) {
                target
                    .handle
                    .set_signal_policy(code, policy)
                    .await
                    .map_err(error)?;
                target.applied_policies.insert(code, policy);
            }
        }

        Ok(())
    }

    // Paths.

    /// Where a recorded source file is on this machine: the first place
    /// the source map finds it, or its recorded path.
    pub(super) fn local_path(&self, recorded: &Path) -> PathBuf {
        let Some(target) = &self.target else {
            return recorded.to_owned();
        };

        target
            .source_paths
            .candidates(recorded)
            .into_iter()
            .find(|candidate| candidate.exists())
            .unwrap_or_else(|| recorded.to_owned())
    }

    /// The recorded path of the source file a client path names: the one
    /// whose local copy it is, or else the only one ending with the most of
    /// its trailing components.
    pub(super) fn recorded_path(&mut self, client: &Path) -> PathBuf {
        let Some(target) = self.target.as_mut() else {
            return client.to_owned();
        };

        let image = target.handle.module_image();
        let index = target.recorded_paths.get_or_insert_with(|| {
            let mut index = HashMap::new();
            for file in image.source_files() {
                for candidate in target.source_paths.candidates(&file.path) {
                    if let Ok(canonical) = std::fs::canonicalize(&candidate) {
                        index
                            .entry(canonical)
                            .or_insert_with(|| file.path.as_ref().clone());
                    }
                }
            }

            index
        });
        let canonical = std::fs::canonicalize(client).unwrap_or_else(|_| client.to_owned());
        if let Some(recorded) = index.get(&canonical) {
            return recorded.clone();
        }

        let components = client.components().collect::<Vec<_>>();

        for kept in (1..=components.len()).rev() {
            let suffix = components[components.len() - kept..]
                .iter()
                .collect::<PathBuf>();
            let mut matches = image
                .source_files()
                .iter()
                .filter(|file| file.path.ends_with(&suffix));
            if let (Some(only), None) = (matches.next(), matches.next()) {
                return only.path.as_ref().clone();
            }
        }

        client.to_owned()
    }

    pub(super) async fn image(&mut self, module: ModuleId) -> Option<Arc<ModuleImage>> {
        let target = self.target.as_mut()?;
        if let Some(image) = target.images.get(&module) {
            return Some(Arc::clone(image));
        }

        let image = target.handle.loaded_module_image(module).await.ok()?;
        target.images.insert(module, Arc::clone(&image));
        Some(image)
    }

    pub(super) async fn backtrace(
        &mut self,
        stop: &Stop,
        context: ExecutionContext,
    ) -> Result<Arc<Backtrace>, ErrorBody> {
        if let Some(trace) = self.backtraces.get(&context) {
            return Ok(Arc::clone(trace));
        }

        let handle = self.target_handle()?;
        let trace = Arc::new(
            handle
                .at(StopContext {
                    stop: stop.id,
                    execution: context,
                    frame: StackFrameId::INNERMOST,
                })
                .backtrace()
                .await
                .map_err(error)?,
        );
        self.backtraces.insert(context, Arc::clone(&trace));
        Ok(trace)
    }

    /// Chooses how values show when a request does not say, and has the
    /// client read them again when that changes.
    async fn set_value_format(&mut self, arguments: Value) -> Result<Value, ErrorBody> {
        let arguments = parse::<protocol::SetValueFormatArguments>(
            arguments,
            "microscope/setValueFormat arguments",
        )?;
        if self.display.hex != arguments.hex {
            self.display.hex = arguments.hex;
            if self.stop.is_some() {
                self.invalidate_values().await;
            }
        }

        Ok(json!({}))
    }

    /// Tells a client that shows values to read them again, after a write
    /// may have changed any of them.
    pub(super) async fn invalidate_values(&self) {
        if self.support().invalidated {
            let _ = self
                .client
                .event("invalidated", json!({"areas": ["variables"]}))
                .await;
        }
    }

    /// Forgets what was read at the current stop, after a write changed
    /// it; references stay valid, since the stop is the same.
    pub(super) fn forget_reads(&mut self) {
        self.backtraces.clear();
        self.variables.clear();
    }

    pub(super) async fn frame_variables(
        &mut self,
        context: StopContext,
    ) -> Result<Arc<VariableSnapshot>, ErrorBody> {
        if let Some(snapshot) = self.variables.get(&(context.execution, context.frame)) {
            return Ok(Arc::clone(snapshot));
        }

        let handle = self.target_handle()?;
        let snapshot = Arc::new(handle.at(context).variables().await.map_err(error)?);
        self.variables
            .insert((context.execution, context.frame), Arc::clone(&snapshot));
        Ok(snapshot)
    }

    /// The image of a module the client was told is loaded.
    pub(super) fn loaded_image(&self, module: ModuleId) -> Option<Arc<ModuleImage>> {
        self.target.as_ref()?.images.get(&module).cloned()
    }

    /// The code of the modules the client was told are loaded.
    pub(super) fn code(&self) -> crate::present::Code {
        let Some(target) = &self.target else {
            return crate::present::Code::default();
        };

        crate::present::Code::new(
            self.modules
                .values()
                .filter_map(|record| {
                    let image = target.images.get(&record.module.id)?;
                    Some((record.module.load_bias, Arc::clone(image)))
                })
                .collect(),
        )
    }

    /// The assembly syntax the configuration chose.
    pub(super) fn syntax(&self) -> microscope::AssemblySyntax {
        self.target
            .as_ref()
            .map_or_else(microscope::AssemblySyntax::default, |target| target.syntax)
    }

    pub(super) fn console(&self) -> Result<&Cli, ErrorBody> {
        self.target
            .as_ref()
            .map(|target| &target.console)
            .ok_or_else(|| ErrorBody::new("no program is loaded"))
    }

    /// Converts a client's line to a one-based line, if it is one.
    pub(super) fn line_from_client(&self, line: i64) -> Option<u64> {
        let line = u64::try_from(line).ok()?;
        if self.support().lines_start_at1 {
            Some(line)
        } else {
            line.checked_add(1)
        }
    }

    /// Converts a one-based line to the client's numbering.
    pub(super) const fn line_to_client(&self, line: u64) -> u64 {
        if self.support().lines_start_at1 {
            line
        } else {
            line.saturating_sub(1)
        }
    }

    /// Converts a one-based column to the client's numbering.
    pub(super) const fn column_to_client(&self, column: u64) -> u64 {
        if self.support().columns_start_at1 {
            column
        } else {
            column.saturating_sub(1)
        }
    }
}

/// The adapter's capabilities, as `initialize` reports them.
fn capabilities() -> Value {
    json!({
        "supportsConfigurationDoneRequest": true,
        "supportsFunctionBreakpoints": true,
        "supportsHitConditionalBreakpoints": true,
        "supportsConditionalBreakpoints": true,
        "supportsLogPoints": true,
        "supportsEvaluateForHovers": true,
        "supportsClipboardContext": true,
        "supportsExceptionInfoRequest": true,
        "supportsExceptionFilterOptions": true,
        "exceptionBreakpointFilters": super::signals::filters(),
        "supportTerminateDebuggee": true,
        "supportsTerminateRequest": true,
        "supportsInstructionBreakpoints": true,
        "supportsRestartRequest": true,
        "supportsWriteMemoryRequest": true,
        "supportsSetVariable": true,
        "supportsSetExpression": true,
        "supportsCancelRequest": true,
        "supportsDisassembleRequest": true,
        "supportsReadMemoryRequest": true,
        "supportsSteppingGranularity": true,
        "supportsSingleThreadExecutionRequests": true,
        "supportsDataBreakpoints": true,
        "supportsDataBreakpointBytes": true,
        "breakpointModes": super::watch::modes(),
        "supportsModulesRequest": true,
        "supportsLoadedSourcesRequest": true,
        "supportsBreakpointLocationsRequest": true,
        "supportsGotoTargetsRequest": true,
        "supportsStepInTargetsRequest": true,
        "supportsValueFormattingOptions": true,
        "supportsCompletionsRequest": true,
        "completionTriggerCharacters": [" ", ".", ">", "$"],
        "supportsANSIStyling": true,
        "supportsDelayedStackTraceLoading": true,
    })
}

enum Input {
    Message(Inbound),
    Event(Result<DebuggerEvent, broadcast::error::RecvError>),

    /// A child the program forked, or none once the debugger is gone.
    Held(Option<microscope::HeldChild>),
}

async fn next_held(held: &mut Option<microscope::HeldChildren>) -> Option<microscope::HeldChild> {
    match held {
        Some(held) => held.recv().await,
        None => std::future::pending().await,
    }
}

async fn next_event(
    events: &mut Option<broadcast::Receiver<DebuggerEvent>>,
) -> Result<DebuggerEvent, broadcast::error::RecvError> {
    match events {
        Some(events) => events.recv().await,
        None => std::future::pending().await,
    }
}

/// The debugger's options for a client breakpoint, or why it has none.
fn breakpoint_options(want: &Want) -> microscope::Result<microscope::BreakpointOptions> {
    Ok(microscope::BreakpointOptions {
        hit_condition: want.hit_condition.as_deref().map(str::parse).transpose()?,
        condition: want
            .condition
            .as_deref()
            .map(microscope::Condition::parse)
            .transpose()?,
        log_message: want
            .log_message
            .as_deref()
            .map(microscope::LogMessage::parse)
            .transpose()?,
        // A library loaded later may have code for it.
        pending: true,
        ..microscope::BreakpointOptions::default()
    })
}

/// Parses request arguments into their type.
pub(super) fn parse<T: serde::de::DeserializeOwned>(
    arguments: Value,
    what: &str,
) -> Result<T, ErrorBody> {
    config::parse(arguments, what).map_err(ErrorBody::new)
}

/// Describes a debugger error to the client.
pub(super) fn error(error: Error) -> ErrorBody {
    match error {
        Error::NotStopped => ErrorBody::not_stopped(),
        error => ErrorBody::new(error.to_string()),
    }
}

/// The exit code a client is told: the program's own, or, as a shell
/// reports it, 128 plus the signal that killed it.
fn exit_code(status: &ExitStatus) -> i64 {
    match status {
        ExitStatus::Code(code) => *code,
        ExitStatus::Terminated(info) => 128 + i64::try_from(info.code).unwrap_or(0),
    }
}

/// The description of a stop the session describes itself, which only
/// reaches `describe_stop` by a bug.
const DESCRIBED_ELSEWHERE: &str = "internal: the session describes this stop itself";

/// The DAP reason for a stop.
pub(super) fn stop_kind(reason: &StopReason) -> &'static str {
    describe_stop(reason).0
}

/// The `stopped` event's reason, description, and text for a stop that
/// needs nothing from the session to describe.
fn describe_stop(reason: &StopReason) -> (&'static str, Option<String>, Option<String>) {
    match reason {
        StopReason::Step { .. }
        | StopReason::StepIncomplete { .. }
        | StopReason::TaskEnded { .. }
        | StopReason::FutureDropped { .. } => describe_step_stop(reason),
        StopReason::Pause => ("pause", None, None),
        StopReason::Jump => ("goto", None, None),
        StopReason::Assignment => ("step", None, Some("the variable was assigned".to_owned())),
        StopReason::Entry | StopReason::Attach => ("entry", None, None),
        StopReason::LanguageException(exception) => (
            "exception",
            Some(exception.message.to_string()),
            Some(language_exception_text(exception.kind).to_owned()),
        ),
        StopReason::RuntimeEvent { description } => (
            "exception",
            Some(format!(
                "the runtime reported an event that could not be read: {description}"
            )),
            Some("unreadable runtime event".to_owned()),
        ),
        StopReason::ProgramBreakpoint { address } => (
            "exception",
            Some(format!(
                "the program executed a breakpoint instruction at {address}"
            )),
            Some("program breakpoint".to_owned()),
        ),
        StopReason::Exception(info)
        | StopReason::CoreDump {
            exception: Some(info),
        } => (
            "exception",
            Some(info.description.to_string()),
            Some(signal_text(info.code)),
        ),
        StopReason::CoreDump { exception: None } => (
            "exception",
            Some("the core dump records no signal".to_owned()),
            Some("core dump".to_owned()),
        ),
        StopReason::ThreadExited { thread_id, .. } => (
            "step",
            Some(format!("thread {thread_id} exited during the step")),
            None,
        ),
        StopReason::Exec { followed: true } => (
            "entry",
            Some("the process executed its program again".to_owned()),
            None,
        ),
        StopReason::Exec { followed: false } => (
            "exception",
            Some(
                "the process replaced its executable image (exec), which is not followed"
                    .to_owned(),
            ),
            Some("exec".to_owned()),
        ),
        StopReason::Unclassifiable { description } => (
            "exception",
            Some(format!(
                "stopped for an unclassifiable reason: {description}"
            )),
            Some("unclassifiable stop".to_owned()),
        ),
        StopReason::WatchpointArmFailed { description, .. } => (
            "exception",
            Some(format!("watchpoints could not be armed: {description}")),
            Some("watchpoint failure".to_owned()),
        ),
        StopReason::Syscall(stop) => (
            "exception",
            Some(crate::present::syscalls::caught(stop)),
            Some(
                if stop.returned.is_some() {
                    "system call returned"
                } else {
                    "system call"
                }
                .to_owned(),
            ),
        ),
        StopReason::Breakpoint { .. }
        | StopReason::Watchpoint { .. }
        | StopReason::WatchpointInvalidated { .. }
        | StopReason::Exited(_) => ("stopped", Some(DESCRIBED_ELSEWHERE.to_owned()), None),
    }
}

/// The `stopped` event's reason, description, and text for a step's end.
fn describe_step_stop(reason: &StopReason) -> (&'static str, Option<String>, Option<String>) {
    match reason {
        StopReason::StepIncomplete { description, .. } => (
            "step",
            Some(format!(
                "the step stopped before it completed: {description}"
            )),
            Some("step incomplete".to_owned()),
        ),
        StopReason::TaskEnded { task, ending, .. } => {
            let ending = match ending {
                microscope::TaskEnding::Finished => "finished",
                microscope::TaskEnding::Cancelled => "was cancelled",
            };

            (
                "step",
                Some(format!("the step's task {task} {ending}")),
                Some(format!("task {task} {ending}")),
            )
        }
        StopReason::FutureDropped { .. } => (
            "step",
            Some("the future the step waited for was dropped".to_owned()),
            Some("future dropped".to_owned()),
        ),
        _ => ("step", None, None),
    }
}

/// What a client shows for each kind of exception a runtime reports.
pub(super) const fn language_exception_text(
    kind: microscope::LanguageExceptionKind,
) -> &'static str {
    match kind {
        microscope::LanguageExceptionKind::Raised => "exception raised",
        microscope::LanguageExceptionKind::Unhandled => "unhandled exception",
        microscope::LanguageExceptionKind::Fatal => "fatal error",
    }
}

/// A signal's name, or its number when it has none.
pub(super) fn signal_text(code: u64) -> String {
    microscope::signal_name(code).unwrap_or_else(|| format!("signal {code}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(seq: u64, command: &str, arguments: Value) -> Inbound {
        Inbound::Request {
            seq: seq.into(),
            command: command.to_owned(),
            arguments,
        }
    }

    #[tokio::test]
    async fn a_resolved_runtime_location_is_pending_until_arming_recovers() {
        let (outgoing, _messages) = mpsc::channel(8);
        let mut session = Session::new(Client::new(outgoing), Cancelled::default());
        let location = microscope::BreakpointLocation::Managed(microscope::ManagedLocation {
            runtime: microscope::RuntimeId::new(1),
            place: microscope::ManagedPlace::Line {
                path: "app.scripted".into(),
                line: 3,
            },
        });
        let mut breakpoint = microscope::Breakpoint {
            id: microscope::BreakpointId::new(1),
            spec: microscope::BreakpointSpec::Function("managed".into()),
            locations: Arc::from([microscope::ResolvedBreakpointLocation {
                location: location.clone(),
                code_instances: Arc::from([]),
                library: None,
            }]),
            pending_locations: Arc::from([microscope::PendingBreakpointLocation {
                location,
                reason: "waiting for a runtime acknowledgement".into(),
            }]),
            hit_condition: None,
            condition: None,
            log_message: None,
            hit_count: 0,
            enabled: true,
            temporary: false,
        };
        assert!(matches!(session.state_of(&breakpoint).await,
            State::Pending { message, .. } if message.contains("runtime acknowledgement")));
        breakpoint.enabled = false;
        assert!(matches!(
            session.state_of(&breakpoint).await,
            State::Disabled { .. }
        ));
        breakpoint.enabled = true;
        let mut locations = breakpoint.locations.to_vec();
        locations.push(microscope::ResolvedBreakpointLocation {
            location: microscope::BreakpointLocation::Virtual(VirtualAddress::new(0x1234)),
            code_instances: Arc::from([]),
            library: None,
        });
        breakpoint.locations = locations.into();
        assert!(matches!(
            session.state_of(&breakpoint).await,
            State::Resolved { message: Some(message), .. } if message.contains("some locations pending") && message.contains("runtime acknowledgement")
        ));
        breakpoint.pending_locations = Arc::from([]);
        assert!(matches!(
            session.state_of(&breakpoint).await,
            State::Resolved { .. }
        ));
    }

    #[test]
    fn resuming_cancels_queued_inspections_but_not_console_commands() {
        let (outgoing, _messages) = mpsc::channel(8);
        let mut session = Session::new(Client::new(outgoing), Cancelled::default());
        session.queue.extend([
            request(2, "stackTrace", json!({"threadId": 1})),
            request(
                3,
                "evaluate",
                json!({"expression": "break f", "context": "repl"}),
            ),
            request(
                4,
                "evaluate",
                json!({"expression": "x", "context": "hover"}),
            ),
            request(5, "setBreakpoints", json!({})),
            request(6, "variables", json!({"variablesReference": 7})),
        ]);
        session.cancel_inspections();
        let kinds = session
            .queue
            .iter()
            .map(|message| match message {
                Inbound::Request { seq, .. } => format!("keep {seq}"),
                Inbound::Cancelled { seq, command } => format!("cancel {seq} {command}"),
                Inbound::Malformed { .. } => "malformed".to_owned(),
            })
            .collect::<Vec<_>>();
        assert_eq!(
            kinds,
            [
                "cancel 2 stackTrace",
                "keep 3",
                "cancel 4 evaluate",
                "keep 5",
                "cancel 6 variables",
            ]
        );

        // A cancellation is honored once, by the request's own seq.
        session
            .cancelled
            .lock()
            .expect("lock")
            .insert("9".to_owned());
        assert!(!session.take_cancellation(&json!("9")));
        assert!(session.take_cancellation(&json!(9)));
        assert!(!session.take_cancellation(&json!(9)));
    }
}
