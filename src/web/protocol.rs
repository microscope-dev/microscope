//! The messages the web UI and `microscope web` exchange over the WebSocket.
//!
//! Every message is one JSON text frame. A tab sends [`Envelope`]s; the
//! server answers each with a [`ServerMessage::Result`] or
//! [`ServerMessage::Error`] carrying its `id`, and pushes the rest. The
//! TypeScript side, `web/src/protocol.gen.ts`, is generated from these types
//! by a test that fails when the checked-in file is stale.

use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Changes whenever a message changes incompatibly; the page refuses a
/// server that speaks another version.
pub const VERSION: u32 = 1;

/// What a connection may do.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub enum Role {
    /// Run, stop, and choose what to debug.
    Control,

    /// Watch, and move one's own focus.
    View,
}

/// One request from a tab.
#[derive(Debug, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct Envelope {
    /// Chosen by the tab; the answer carries it back.
    pub id: u64,

    #[serde(flatten)]
    pub request: Request,
}

mod syscalls;
pub use syscalls::*;

/// What a tab can ask for.
#[derive(Debug, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(tag = "method", content = "params", rename_all = "camelCase")]
pub enum Request {
    /// Names this connection for everyone else's presence list.
    SetName(SetName),

    /// A link that joins this session with the given access.
    Share(Share),

    /// Paths that complete a partly typed one, for the picker.
    CompletePath(CompletePath),

    /// This user's processes, for attaching.
    Processes,

    /// Loads a program to launch, ending any current session.
    Launch(Launch),

    /// Attaches to a process, ending any current session.
    Attach(Attach),

    /// Opens a core dump, ending any current session.
    OpenCore(OpenCore),

    /// Ends the current session: a launched program is killed, an attached
    /// one detached.
    End,

    /// Starts a launched program that has not started, or continues a
    /// stopped one.
    Continue(Continue),

    /// Stops a running program.
    Pause,

    /// Kills the program, which can then be started again.
    Kill,

    /// Kills the program and starts it again.
    Restart,

    /// Steps one thread of a stop.
    Step(Step),

    /// The calls of a thread's line at a stop that a step into can go
    /// into.
    StepTargets(ThreadAt),

    /// Moves one thread of a stop, without running it, to resume at a
    /// location in its function.
    Jump(Jump),

    /// Says where this tab is looking, for everyone's presence list.
    SetFocus(SetFocus),

    /// A thread's stack at a stop.
    Backtrace(ThreadAt),

    /// Every source file the debug information names.
    Sources,

    /// One source file's text, and the lines a breakpoint can stop at.
    Source(SourcePath),

    /// Adds a breakpoint.
    AddBreakpoint(AddBreakpoint),

    /// Replaces a breakpoint's condition, hit condition, and log message.
    EditBreakpoint(EditBreakpoint),
    RemoveBreakpoint(BreakpointRef),

    /// Writes to the program's standard input, or closes it.
    Input(Input),

    /// A frame's arguments and locals, and the statics of its source file.
    Scopes(FrameAt),

    /// A window of what a row expands to.
    Children(ChildrenOf),

    /// Evaluates an expression in a frame without changing the program.
    Evaluate(Evaluate),

    /// Assigns a value to what a path names.
    SetValue(SetValue),

    /// Completes a console line.
    Complete(Complete),

    /// Runs a console line: an expression, which may assign, or one of
    /// microscope's commands, in the tab's frame.
    Console(ConsoleLine),

    /// The instructions of the function holding an address, or around it.
    Disassemble(Disassemble),

    /// Bytes of the program's memory at a stop.
    ReadMemory(ReadMemory),

    /// Writes bytes to the program's memory at a stop.
    WriteMemory(WriteMemory),

    /// A frame's registers.
    Registers(FrameAt),

    /// Stops when memory an expression names, or an address range, changes
    /// or is accessed.
    AddWatchpoint(AddWatchpoint),

    /// Replaces a watchpoint's condition and hit condition.
    EditWatchpoint(EditWatchpoint),
    RemoveWatchpoint(WatchpointRef),

    /// What the debugger does with each signal.
    Signals,

    /// Changes what the debugger does with one signal.
    SetSignal(SignalPolicy),

    /// The exceptions language runtimes report, and whether each stops.
    Exceptions,

    /// Chooses whether the exceptions one filter passes stop.
    SetException(ExceptionChoice),

    /// The modules the program has loaded.
    Modules,

    /// Functions whose names hold the query, best matches first.
    Functions(FunctionQuery),

    /// The tasks of the program's runtimes at a stop.
    Tasks(StopAt),

    /// The inputs of a drawing of a value, which the page's renderer draws.
    Draw(Draw),

    /// A renderer's JavaScript, by its digest.
    Renderer(RendererRef),

    /// Every renderer a drawing may name, for drawing a value with any.
    Renderers,

    /// Reads the session's view files, and the renderers beside them,
    /// again.
    ReloadViews,

    /// What a profile holds: its threads, span, and accounts.
    Profile(ProfileRef),

    /// A profile's call tree under a query; its columns go in the binary
    /// frame before the answer.
    ProfileTree(ProfileQuery),

    /// What each function costs under a query.
    ProfileFunctions(ProfileQuery),

    /// How a query's samples fall over time, per thread.
    ProfileTimeline(ProfileTimelineQuery),

    /// Each thread's zones in a range, folded to a width; their columns
    /// go in the binary frame before the answer.
    ProfileZones(ProfileZonesQuery),

    /// The spread of one site's zone lengths.
    ProfileZoneStats(ProfileZoneStatsQuery),

    /// A function's samples on its source lines, with the lines' text.
    ProfileLines(ProfileFunctionQuery),

    /// A function's samples on its instructions.
    ProfileCode(ProfileFunctionQuery),

    /// Records a program, publishing its profile as it goes.
    RecordProfile(RecordProfile),

    /// Ends a recording, killing its program.
    StopRecording(ProfileRef),

    /// What a profile's system calls cost, grouped.
    SyscallSummary(SyscallSummaryQuery),

    /// Calls per slice of time, stacked.
    SyscallRate(SyscallRateQuery),

    /// Calls by time and latency.
    SyscallHeatmap(SyscallHeatmapQuery),

    /// One call's latency, errors, and callers.
    SyscallDetail(SyscallDetailQuery),

    /// A page of the trace as strace prints it.
    SyscallLog(SyscallLogQuery),

    /// One call of the trace, expanded.
    SyscallCall(SyscallCallQuery),

    /// Each thread's time in calls.
    SyscallLanes(SyscallLanesQuery),

    /// What the calls did with files.
    SyscallFiles(SyscallQuery),

    /// What the calls did over sockets.
    SyscallNetwork(SyscallQuery),

    /// The processes the trace saw.
    SyscallProcesses(SyscallQuery),

    /// What a live trace's threads are blocked in.
    SyscallInFlight(SyscallInFlightQuery),
}

/// A stop, which must be current.
#[derive(Debug, Clone, Copy, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct StopAt {
    pub stop: u64,
}

#[derive(Debug, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct SetName {
    pub name: String,
}

#[derive(Debug, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct Share {
    pub role: Role,

    /// The page path the link opens, such as `/s/k7q2`.
    pub to: String,
}

#[derive(Debug, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct CompletePath {
    /// The path typed so far, relative to the server's directory or `~`.
    pub text: String,
}

#[derive(Debug, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct Launch {
    pub program: String,

    #[serde(default)]
    pub arguments: Vec<String>,

    /// The program's working directory, instead of the server's.
    #[serde(default)]
    pub cwd: Option<String>,

    /// Variables set in the program's environment, in order.
    #[serde(default)]
    pub environment: Vec<(String, String)>,

    /// Stop at the program's first instruction when it starts.
    #[serde(default)]
    pub stop_at_entry: bool,

    /// Start the program at once instead of waiting for a continue.
    #[serde(default)]
    pub run: bool,

    /// End a current session to make way. Without it, a current session
    /// fails the request with `busy`.
    #[serde(default)]
    pub replace: bool,
}

#[derive(Debug, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct Attach {
    pub pid: u64,

    #[serde(default)]
    pub replace: bool,
}

#[derive(Debug, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct OpenCore {
    pub core: String,

    /// The executable that wrote the dump, when the recorded one is wrong.
    #[serde(default)]
    pub executable: Option<String>,

    #[serde(default)]
    pub replace: bool,
}

#[derive(Debug, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct Continue {
    /// The stop being continued, which must still be current. Absent to
    /// start a program that has not started.
    #[serde(default)]
    pub stop: Option<u64>,
}

#[derive(Debug, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct Step {
    /// The stop being stepped from, which must still be current.
    pub stop: u64,
    pub thread: u64,

    /// The task stepped, which runs on the thread.
    #[serde(default)]
    #[cfg_attr(test, ts(optional = nullable))]
    pub task: Option<TaskKey>,

    /// The frame a step out leaves; every other step starts from the
    /// innermost frame.
    #[serde(default)]
    #[cfg_attr(test, ts(optional = nullable))]
    pub frame: u32,
    pub kind: StepKind,

    /// For a step into, the one call of the line to go into, in
    /// hexadecimal, as `stepTargets` lists it; the line's other calls run
    /// to their returns.
    #[serde(default)]
    #[cfg_attr(test, ts(optional = nullable))]
    pub call: Option<String>,
}

#[derive(Debug, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct Jump {
    /// The stop the thread is moved at, which must still be current.
    pub stop: u64,
    pub thread: u64,

    #[serde(default)]
    #[cfg_attr(test, ts(optional = nullable))]
    pub task: Option<TaskKey>,

    /// `FILE:LINE`, or another location a breakpoint takes.
    pub location: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub enum StepKind {
    /// To the next source line, running calls through.
    Over,

    /// To the next source line, entering calls.
    Into,

    /// Until the frame returns.
    Out,

    /// One instruction, entering calls.
    Instruction,

    /// One instruction, running calls through.
    OverInstruction,
}

#[derive(Debug, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct SetFocus {
    /// Absent when the tab looks at nothing in particular.
    pub focus: Option<Focus>,
}

/// Where a person is looking: a page address and a few words for it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct Focus {
    /// The page path and query, such as `/s/k7q2/stop/12/t/41872/f/1`.
    pub url: String,

    /// Such as `frame 1, serve_conn`.
    pub label: String,
}

/// A task of one of the program's runtimes, as the debugger numbers it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct TaskKey {
    pub runtime: u32,
    pub number: u64,
}

impl TaskKey {
    pub const fn id(self) -> microscope::TaskId {
        microscope::TaskId {
            runtime: microscope::RuntimeId::new(self.runtime),
            number: self.number,
        }
    }
}

/// What runs code: a task when one is named, else the thread.
pub const fn execution(thread: u64, task: Option<TaskKey>) -> microscope::ExecutionContext {
    match task {
        Some(task) => microscope::ExecutionContext::Task(task.id()),
        None => microscope::ExecutionContext::Thread(microscope::ThreadId::new(thread)),
    }
}

/// A thread at a stop, or a task, when one is named, whose stack is shown
/// instead.
#[derive(Debug, Clone, Copy, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct ThreadAt {
    pub stop: u64,
    pub thread: u64,

    #[serde(default)]
    #[cfg_attr(test, ts(optional = nullable))]
    pub task: Option<TaskKey>,
}

/// A frame of a thread or task at a stop, counting from the innermost.
#[derive(Debug, Clone, Copy, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct FrameAt {
    pub stop: u64,
    pub thread: u64,

    #[serde(default)]
    #[cfg_attr(test, ts(optional = nullable))]
    pub task: Option<TaskKey>,
    pub frame: u32,
}

impl ThreadAt {
    pub const fn execution(self) -> microscope::ExecutionContext {
        execution(self.thread, self.task)
    }
}

impl FrameAt {
    pub const fn execution(self) -> microscope::ExecutionContext {
        execution(self.thread, self.task)
    }
}

#[derive(Debug, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct ChildrenOf {
    /// A row's `children.handle`, which belongs to this connection and the
    /// row's stop.
    pub handle: u64,
    pub start: u64,
    pub count: u64,
}

#[derive(Debug, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct Evaluate {
    #[serde(flatten)]
    pub at: FrameAt,
    pub expression: String,
}

#[derive(Debug, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct SetValue {
    #[serde(flatten)]
    pub at: FrameAt,

    /// The expression that names what changes, a row's `path`.
    pub path: String,

    /// An expression for the new value.
    pub value: String,
}

#[derive(Debug, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct Complete {
    /// The line up to the cursor.
    pub text: String,

    /// The frame whose names complete; absent when nothing is stopped.
    #[serde(default)]
    #[cfg_attr(test, ts(optional = nullable))]
    pub stop: Option<u64>,

    #[serde(default)]
    #[cfg_attr(test, ts(optional = nullable))]
    pub thread: Option<u64>,

    #[serde(default)]
    #[cfg_attr(test, ts(optional = nullable))]
    pub task: Option<TaskKey>,

    #[serde(default)]
    #[cfg_attr(test, ts(optional = nullable))]
    pub frame: Option<u32>,
}

#[derive(Debug, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct ConsoleLine {
    pub line: String,

    /// The frame the line runs in; absent when nothing is stopped.
    #[serde(default)]
    #[cfg_attr(test, ts(optional = nullable))]
    pub stop: Option<u64>,

    #[serde(default)]
    #[cfg_attr(test, ts(optional = nullable))]
    pub thread: Option<u64>,

    #[serde(default)]
    #[cfg_attr(test, ts(optional = nullable))]
    pub task: Option<TaskKey>,

    #[serde(default)]
    #[cfg_attr(test, ts(optional = nullable))]
    pub frame: Option<u32>,
}

#[derive(Debug, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct Disassemble {
    #[serde(flatten)]
    pub at: FrameAt,

    /// The address to show, as hexadecimal; the frame's own code when
    /// absent.
    #[serde(default)]
    #[cfg_attr(test, ts(optional = nullable))]
    pub address: Option<String>,

    /// Intel unless AT&T is asked for.
    #[serde(default)]
    #[cfg_attr(test, ts(optional = nullable))]
    pub syntax: Option<Syntax>,
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub enum Syntax {
    Intel,
    Att,
}

#[derive(Debug, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct ReadMemory {
    /// The stop the bytes are read at, which must be current.
    pub stop: u64,

    /// As hexadecimal, such as `0x7ffff7a3e010`.
    pub address: String,
    pub count: u64,
}

#[derive(Debug, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct WriteMemory {
    pub stop: u64,
    pub address: String,

    /// The bytes, as pairs of hexadecimal digits.
    pub bytes: String,
}

#[derive(Debug, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct AddWatchpoint {
    /// An expression in the frame, or `0xADDRESS:BYTES`.
    pub target: String,
    pub access: WatchAccess,

    /// The frame an expression is resolved in; an address range needs none.
    #[serde(default)]
    #[cfg_attr(test, ts(optional = nullable))]
    pub stop: Option<u64>,

    #[serde(default)]
    #[cfg_attr(test, ts(optional = nullable))]
    pub thread: Option<u64>,

    #[serde(default)]
    #[cfg_attr(test, ts(optional = nullable))]
    pub task: Option<TaskKey>,

    #[serde(default)]
    #[cfg_attr(test, ts(optional = nullable))]
    pub frame: Option<u32>,

    #[serde(default)]
    #[cfg_attr(test, ts(optional = nullable))]
    pub condition: Option<String>,

    #[serde(default)]
    #[cfg_attr(test, ts(optional = nullable))]
    pub hit_condition: Option<String>,
}

#[derive(Debug, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct EditWatchpoint {
    pub id: u64,

    #[serde(default)]
    #[cfg_attr(test, ts(optional = nullable))]
    pub condition: Option<String>,

    #[serde(default)]
    #[cfg_attr(test, ts(optional = nullable))]
    pub hit_condition: Option<String>,
}

#[derive(Debug, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct WatchpointRef {
    pub id: u64,
}

/// What stops a watchpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub enum WatchAccess {
    /// A store that changes the value.
    Change,

    /// Every store, even of the same value.
    Write,

    /// Every load or store.
    ReadWrite,
    Read,
}

/// Whether the exceptions a filter passes stop the program.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct ExceptionChoice {
    /// The filter, such as `python-uncaught`.
    pub id: String,
    pub on: bool,
}

/// What the debugger does with one signal.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct SignalPolicy {
    pub signal: u64,

    /// Such as `SIGUSR1`; filled in by the server.
    #[serde(default)]
    pub name: String,
    pub stop: bool,
    pub print: bool,
    pub pass: bool,
}

#[derive(Debug, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct FunctionQuery {
    pub query: String,

    /// How many to return; 50 when absent, and never more than 200.
    #[serde(default)]
    #[cfg_attr(test, ts(optional = nullable))]
    pub limit: Option<u32>,
}

#[derive(Debug, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct SourcePath {
    /// A path as the debug information records it.
    pub path: String,
}

#[derive(Debug, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct AddBreakpoint {
    /// A function, `FILE:LINE`, `FILE:FUNCTION`, or `0xADDRESS`.
    pub location: String,

    #[serde(default)]
    #[cfg_attr(test, ts(optional = nullable))]
    pub condition: Option<String>,

    /// Which hits stop, such as `>=5` or `%10`.
    #[serde(default)]
    #[cfg_attr(test, ts(optional = nullable))]
    pub hit_condition: Option<String>,

    /// A message to log instead of stopping, with expressions in braces.
    #[serde(default)]
    #[cfg_attr(test, ts(optional = nullable))]
    pub log_message: Option<String>,
}

#[derive(Debug, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct EditBreakpoint {
    pub id: u64,

    /// Absent to remove.
    #[serde(default)]
    #[cfg_attr(test, ts(optional = nullable))]
    pub condition: Option<String>,

    #[serde(default)]
    #[cfg_attr(test, ts(optional = nullable))]
    pub hit_condition: Option<String>,

    #[serde(default)]
    #[cfg_attr(test, ts(optional = nullable))]
    pub log_message: Option<String>,
}

#[derive(Debug, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct BreakpointRef {
    pub id: u64,
}

#[derive(Debug, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct Input {
    pub text: String,

    /// Close the input after the text, so the program reads its end.
    #[serde(default)]
    pub eof: bool,
}

/// Everything the server sends.
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum ServerMessage {
    /// The first message on every connection.
    Hello(Hello),

    /// The whole debugger state, sent on connecting and at every change.
    State(Arc<State>),

    /// A request succeeded.
    Result {
        id: u64,

        #[cfg_attr(test, ts(type = "unknown"))]
        result: Value,
    },

    /// A request failed.
    Error { id: u64, error: ErrorBody },

    /// Bytes the program wrote.
    Output(Output),

    /// Who is connected.
    Presence(Presence),

    /// Something another participant did.
    Notice(Notice),

    /// The profiles the server serves, sent on connecting and whenever
    /// they change.
    Profiles(ProfileList),
}

/// The profiles a server serves, which belong to no debugging session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct ProfileList {
    pub profiles: Vec<ProfileInfo>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct ProfileInfo {
    /// Derived from a file's bytes, so a link names the same profile
    /// whenever a server serves it; a recording's is its own.
    pub id: String,

    /// The file it was read from, or the program recorded.
    pub name: String,

    /// Whether it is being recorded, and grows.
    pub recording: bool,

    /// Counts the profiles published under this id: each answer about it
    /// is of the newest.
    pub revision: u32,

    /// How a recorded program ended, once it has.
    pub ended: Option<String>,
}

#[derive(Debug, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct ProfileRef {
    pub profile: String,

    /// The revision the tab has seen, which keeps its cached answers
    /// apart; the server answers from the newest.
    #[serde(default)]
    #[cfg_attr(test, ts(optional = nullable))]
    #[expect(dead_code, reason = "only the tab's cache reads it")]
    pub revision: u32,
}

/// A program to record.
#[derive(Debug, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct RecordProfile {
    pub program: String,

    #[serde(default)]
    pub arguments: Vec<String>,

    /// Where it runs: the server's directory when absent.
    #[serde(default)]
    #[cfg_attr(test, ts(optional = nullable))]
    pub cwd: Option<String>,

    /// Variables added to its environment, as `[name, value]` pairs.
    #[serde(default)]
    pub environment: Vec<(String, String)>,

    #[serde(default)]
    pub preset: RecordPreset,

    /// Samples each call of the function this names instead of CPU time.
    #[serde(default)]
    #[cfg_attr(test, ts(optional = nullable))]
    pub function: Option<String>,

    /// Keeps only the last this many seconds, which the page then shows
    /// and saves.
    #[serde(default)]
    #[cfg_attr(test, ts(optional = nullable))]
    pub window: Option<f64>,

    /// Traces system calls instead of sampling.
    #[serde(default)]
    #[cfg_attr(test, ts(optional = nullable))]
    pub syscalls: Option<RecordSyscalls>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub enum RecordPreset {
    /// 49 samples a second: safe in production.
    Production,

    /// 999 samples a second, with a deep stack copy.
    #[default]
    Development,

    /// 9999 samples a second.
    Maximum,
}

#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct RecordStarted {
    /// The profile's id, which stays its own once the recording ends.
    pub profile: String,
}

/// Which of a profile's samples, seen how.
#[derive(Debug, Clone, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct ProfileQuery {
    pub profile: String,

    /// The revision the tab has seen, which keeps its cached answers
    /// apart; the server answers from the newest.
    #[serde(default)]
    #[cfg_attr(test, ts(optional = nullable))]
    #[expect(dead_code, reason = "only the tab's cache reads it")]
    pub revision: u32,

    /// Samples taken in `[start, end)`, in nanoseconds from the profile's
    /// start, or all.
    #[serde(default)]
    #[cfg_attr(test, ts(optional = nullable))]
    pub range: Option<[i64; 2]>,

    /// Samples of these threads, by row, or of all.
    #[serde(default)]
    #[cfg_attr(test, ts(optional = nullable))]
    pub threads: Option<Vec<u32>>,

    /// What the query aggregates.
    #[serde(default)]
    pub source: ProfileSource,

    /// Samples or waits of these tasks, by row, or of any; `null` in the
    /// list is what no snapshot named.
    #[serde(default)]
    #[cfg_attr(test, ts(optional = nullable))]
    pub tasks: Option<Vec<Option<u32>>>,

    /// Roots each path at its task.
    #[serde(default)]
    pub by_task: bool,

    /// Samples or waits that carry each of these labels, `key=value`.
    #[serde(default)]
    pub labels: Vec<String>,

    /// Roots each path, outside any task, at its label of this key.
    #[serde(default)]
    #[cfg_attr(test, ts(optional = nullable))]
    pub by_label: Option<String>,

    /// Applied in order, to keys by the names views show.
    #[serde(default)]
    pub transforms: Vec<ProfileTransform>,

    /// Paths from the innermost frame out: a tree of callers.
    #[serde(default)]
    pub invert: bool,

    /// How each stack's frames are seen.
    #[serde(default)]
    pub look: ProfileLook,

    #[serde(default)]
    pub granularity: ProfileGranularity,

    /// What a tree or functions answer compares itself with.
    #[serde(default)]
    #[cfg_attr(test, ts(optional = nullable))]
    pub base: Option<ProfileBase>,
}

/// What a diff compares a query with: the same query of another range of
/// its profile, or of another profile.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct ProfileBase {
    pub profile: String,

    /// Nanoseconds from that profile's start, or all of it.
    #[serde(default)]
    #[cfg_attr(test, ts(optional = nullable))]
    pub range: Option<[i64; 2]>,
}

/// What a diff's base weighs.
#[derive(Debug, Clone, Copy, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct ProfileBaseWeight {
    pub weight: u64,
    pub samples: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub enum ProfileSource {
    /// The samples.
    #[default]
    Samples,

    /// Where task snapshots found tasks waiting.
    Waits,
}

#[derive(Debug, Clone, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct ProfileTransform {
    pub op: TransformOp,
    pub name: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub enum TransformOp {
    /// Keeps samples through the function, rooted at it.
    Focus,

    /// Removes the function, giving its cost to its callers.
    Merge,

    /// Removes the samples through the function.
    Drop,

    /// Merges the function's calls of itself.
    Collapse,
}

/// How a query sees each stack's frames.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub enum ProfileLook {
    /// Each async task's frames as a stack of their own, an interpreter's
    /// frames among the frames that run them.
    #[default]
    Logical,

    /// The frames threads ran, which an interpreter's frames are not.
    Native,

    /// As the logical look, from each stack's outermost interpreted frame
    /// in, without the interpreter's own frames.
    Python,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub enum ProfileGranularity {
    /// Each function, inlined ones too.
    #[default]
    Function,

    /// Each function as compiled, its inlined functions in it.
    Physical,

    /// Each source line of each function.
    Line,

    /// Each instruction.
    Address,
}

#[derive(Debug, Clone, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct ProfileFunctionQuery {
    /// Whose samples count; its transforms do not apply, as they name
    /// what views show rather than lines or instructions.
    #[serde(flatten)]
    pub query: ProfileQuery,
    pub function: String,
}

/// A function's samples on its source lines, by file.
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct ProfileLines {
    /// The weight of every sample selected.
    pub weight: u64,
    pub files: Vec<ProfileSourceFile>,
}

#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct ProfileSourceFile {
    pub path: Option<String>,

    /// Each line with samples: its number, own weight, and total weight.
    pub lines: Vec<[u64; 3]>,

    /// The file's text from line `first`, around those lines, as the file
    /// reads now; `None`, with why, when it cannot be read.
    pub first: u32,
    pub text: Option<Vec<String>>,
    pub unread: Option<String>,
}

/// A function's samples on its instructions, or why its code is unknown.
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct ProfileCode {
    pub weight: u64,
    pub module: Option<String>,
    pub blocks: Vec<ProfileCodeBlock>,

    /// Sampled addresses no decoded instruction holds: address, own, and
    /// total.
    pub unplaced: Vec<[u64; 3]>,
    pub unavailable: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct ProfileCodeBlock {
    pub symbol: String,
    pub instructions: Vec<ProfileInstruction>,
    pub stopped: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct ProfileInstruction {
    pub address: u64,
    pub text: String,

    /// The function a direct jump or call goes to.
    pub target: Option<String>,
    pub own: u64,
    pub total: u64,
}

#[derive(Debug, Clone, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct ProfileTimelineQuery {
    #[serde(flatten)]
    pub query: ProfileQuery,

    /// Slices of the query's range, or of the whole profile.
    pub buckets: u32,

    /// Lanes of tasks rather than threads; waits are always of tasks.
    #[serde(default)]
    pub task_lanes: bool,
}

#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct ProfileSummary {
    pub id: String,
    pub name: String,

    /// The program recorded.
    pub program: Option<String>,

    /// What a sample weighs: `nanoseconds` of CPU time, `executions`,
    /// `bytes`, or what an imported profile counts.
    pub unit: String,

    /// What made the samples, such as a clock or an imported pprof file.
    pub source: Option<String>,

    /// Whether the samples carry their times and threads, which an
    /// imported pprof or folded profile's do not.
    pub timed: bool,
    pub samples: u64,
    pub weight: u64,

    /// System calls its trace holds, which the Syscalls view shows.
    pub syscalls: u64,

    /// Records of calls the trace's engine lost.
    pub syscalls_lost: u64,

    /// Nanoseconds from the first sample to the last.
    pub duration: i64,
    pub threads: Vec<ProfileThread>,

    /// Samples the kernel dropped.
    pub lost: u64,

    /// Times the kernel throttled sampling.
    pub throttles: u64,

    /// Stacks that ended short of their outermost frame, by reason.
    pub truncations: Vec<ProfileCount>,

    /// How the profile was recorded, by setting.
    pub settings: Vec<ProfileSetting>,

    /// Nanoseconds of CPU time microscope spent recording, and the part of it
    /// spent reading debug information.
    pub recorder_cpu: u64,
    pub recorder_loading: u64,

    /// Spans not sampled, in nanoseconds from the start.
    pub gaps: Vec<[i64; 2]>,

    /// Each change the budget made, from the start.
    pub budget_changes: Vec<ProfileMarker>,

    /// The tasks task snapshots found, most weight first.
    pub tasks: Vec<ProfileTask>,

    /// Whether its stacks hold frames an interpreter runs, which a query's
    /// `python` look shows as their code sees them.
    pub interpreted: bool,

    /// Task snapshots taken, the nanoseconds they held the program
    /// stopped, and the samples they named the task of.
    pub snapshots: u64,
    pub snapshot_pause: u64,
    pub task_samples: u64,

    /// Each time a snapshot failed or missed tasks, from the start.
    pub snapshot_problems: Vec<ProfileMarker>,

    /// The weight of every wait the snapshots found.
    pub waited: u64,

    /// What the recording read of the program's instrumentation, and lost.
    pub probes: ProfileProbes,

    /// Each zone site name's and label key's zones, most time first.
    pub zone_sites: Vec<ProfileZoneSite>,

    /// Every key a sample's or task's label may have.
    pub label_keys: Vec<String>,

    /// Why modules' instrumentation could not be read.
    pub probe_problems: Vec<ProfileMarker>,
}

#[derive(Debug, Clone, Copy, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct ProfileProbes {
    pub events: u64,

    /// Records threads overwrote before microscope read them.
    pub overwritten: u64,

    /// Events no ring could take.
    pub no_ring: u64,

    /// Events a signal handler dropped mid-write.
    pub interrupted: u64,

    /// Records that broke the probes ABI.
    pub malformed: u64,

    /// Values plotted.
    pub plotted: u64,
}

#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct ProfileZoneSite {
    pub name: String,

    /// Whether its zones are a label's spans.
    pub label: bool,
    pub count: u64,

    /// Nanoseconds, all of them and the longest.
    pub total: u64,
    pub longest: u64,

    /// Zones of it whose begin or end was not seen, or did not nest.
    pub flagged: u64,
}

#[derive(Debug, Clone, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct ProfileZonesQuery {
    pub profile: String,

    #[serde(default)]
    #[cfg_attr(test, ts(optional = nullable))]
    #[expect(dead_code, reason = "only the tab's cache reads it")]
    pub revision: u32,

    /// Nanoseconds from the profile's start, or all of it.
    #[serde(default)]
    #[cfg_attr(test, ts(optional = nullable))]
    pub range: Option<[i64; 2]>,

    /// Of these threads, by row, or of all.
    #[serde(default)]
    #[cfg_attr(test, ts(optional = nullable))]
    pub threads: Option<Vec<u32>>,

    /// The pixels the range is drawn across: zones under three pixels wide
    /// fold together.
    pub width: u32,
}

/// Zones as a view draws them: the binary frame before it holds, for each
/// block, its start and end as `f64` nanoseconds from the profile's start,
/// then its thread, depth, name (an index into `names`), zone count, and
/// flags as `u32`s, column by column.
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct ProfileZones {
    pub range: [i64; 2],
    pub blocks: u32,
    pub names: Vec<String>,

    /// The program's messages, frames, and triggers in the range.
    pub marks: Vec<ProfileMark>,

    /// Each plotted series in the range, by name.
    pub plots: Vec<ProfilePlot>,
}

#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct ProfilePlot {
    pub name: String,

    /// How many values it drew, and how many were not finite.
    pub values: u64,
    pub unshown: u64,

    /// A bin a pixel wide each: the time from the start of its first
    /// value, its first, last, lowest, and highest values.
    pub bins: Vec<[f64; 5]>,

    /// The value before the range, which holds into it.
    pub held: Option<f64>,

    /// Whether a value follows the range, so the last holds to its end.
    pub continues: bool,
}

#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct ProfileMark {
    pub thread: u32,

    /// Nanoseconds from the start.
    pub time: i64,
    pub kind: ProfileMarkKind,

    /// Its site's name.
    pub name: String,
    pub text: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub enum ProfileMarkKind {
    Message,
    Frame,
    Trigger,
}

#[derive(Debug, Clone, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct ProfileZoneStatsQuery {
    pub profile: String,

    #[serde(default)]
    #[cfg_attr(test, ts(optional = nullable))]
    #[expect(dead_code, reason = "only the tab's cache reads it")]
    pub revision: u32,

    /// A zone site's name, or a label's key.
    pub name: String,

    #[serde(default)]
    #[cfg_attr(test, ts(optional = nullable))]
    pub range: Option<[i64; 2]>,

    #[serde(default)]
    #[cfg_attr(test, ts(optional = nullable))]
    pub threads: Option<Vec<u32>>,
}

/// The spread of one site's zone lengths, in nanoseconds.
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct ProfileZoneStats {
    pub count: u64,
    pub total: u64,
    pub flagged: u64,
    pub min: u64,
    pub median: u64,
    pub p90: u64,
    pub p99: u64,
    pub max: u64,

    /// Buckets of equal ratio: each its lowest length and how many.
    pub histogram: Vec<[u64; 2]>,

    /// The longest, longest first.
    pub longest: Vec<ProfileZoneAt>,

    /// For a label: each value's time and count, most time first.
    pub values: Vec<ProfileLabelValue>,
}

#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct ProfileZoneAt {
    pub thread: u32,

    /// Nanoseconds from the start.
    pub start: i64,
    pub end: i64,
    pub value: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct ProfileLabelValue {
    pub value: String,
    pub total: u64,
    pub count: u64,
}

#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct ProfileTask {
    pub row: u32,

    /// As views name it: its runtime's noun, number, and labels.
    pub name: String,

    /// The weight of the samples that ran it, and the nanoseconds of
    /// waiting snapshots found it in.
    pub weight: u64,
    pub waited: u64,
}

#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct ProfileMarker {
    /// Nanoseconds from the start.
    pub time: i64,
    pub text: String,
}

#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct ProfileThread {
    pub row: u32,
    pub pid: u32,
    pub tid: u32,
    pub name: Option<String>,
    pub samples: u64,
    pub weight: u64,

    /// Nanoseconds off its CPU, blocked and preempted.
    pub blocked: u64,
    pub preempted: u64,
}

#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct ProfileCount {
    pub what: String,
    pub count: u64,
}

#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct ProfileSetting {
    pub name: String,
    pub value: String,
}

/// A call tree: the binary frame before it holds, for each node in
/// depth-first order, its own weight and total weight as `f64`s, then its
/// key, an index into `keys`, and depth as `u32`s, column by column.
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct ProfileTree {
    pub nodes: u32,

    /// The weight of every sample the query kept.
    pub weight: u64,
    pub samples: u64,
    pub keys: Vec<ProfileKey>,

    /// For a diff, its base's weight, whose own and total weights by node
    /// follow the columns as two more of `f64`s.
    pub base: Option<ProfileBaseWeight>,
}

/// What a node or row stands for.
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct ProfileKey {
    /// How views name it, which transforms take.
    pub name: String,
    pub kind: ProfileKeyKind,

    /// The module's file name.
    pub module: Option<String>,

    /// Where it is in the source.
    pub file: Option<String>,
    pub line: Option<u32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub enum ProfileKeyKind {
    Function,
    Line,

    /// Code no symbols describe.
    Address,

    /// Where a stack ended early.
    Truncation,

    /// An async task's root.
    Task,

    /// A task a query by task roots paths at.
    Group,

    /// A label value a query by label roots paths at.
    Label,
}

#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct ProfileFunctions {
    pub weight: u64,

    /// Most own weight first, or, for a diff, most changed share first.
    pub rows: Vec<ProfileCost>,
    pub keys: Vec<ProfileKey>,

    /// For a diff, its base's weight.
    pub base: Option<ProfileBaseWeight>,
}

#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct ProfileCost {
    /// An index into `keys`.
    pub key: u32,
    pub own: u64,
    pub total: u64,

    /// For a diff, the key's weights in its base.
    pub base_own: u64,
    pub base_total: u64,
}

#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct ProfileTimeline {
    /// The range sliced, in nanoseconds from the profile's start.
    pub range: [i64; 2],
    pub lanes: Vec<ProfileLane>,
}

/// One thread's or task's weight in each slice, and the nanoseconds of
/// each a thread spent off its CPU: each list empty when it has none. A
/// lane of neither is what no task snapshot named.
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct ProfileLane {
    pub thread: Option<u32>,
    pub task: Option<u32>,
    pub weights: Vec<u64>,
    pub blocked: Vec<u64>,
    pub preempted: Vec<u64>,
}

#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct Hello {
    pub version: u32,

    /// This connection's number, unique while the server runs.
    pub connection: u32,
    pub role: Role,

    /// The name others see until the tab chooses one.
    pub name: String,

    /// The server's working directory, which relative paths start from.
    pub cwd: String,
}

/// The debugger's state at one revision.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct State {
    /// Identifies what is being debugged; a new program, process, or core
    /// dump gets a new one. Absent when nothing is.
    pub session: Option<String>,

    /// What is being debugged.
    pub target: Option<Target>,

    /// Slow work under way, such as loading debug information.
    pub busy: Option<String>,

    /// The debugger revision this state reflects.
    pub revision: u64,
    pub inferior: Inferior,
    pub threads: Vec<Thread>,
    pub breakpoints: Vec<Breakpoint>,

    /// The latest stops of this session, oldest first.
    pub stops: Vec<StopEntry>,

    /// Counts changes made to the program's values, which make values read
    /// earlier at the same stop out of date.
    pub writes: u64,
    pub watchpoints: Vec<Watchpoint>,

    /// Counts changes to settings that publish nothing else, such as
    /// signal policies.
    pub settings: u64,
}

impl State {
    /// The state with nothing being debugged.
    pub const fn idle(busy: Option<String>) -> Self {
        Self {
            session: None,
            target: None,
            busy,
            revision: 0,
            inferior: Inferior::NotStarted,
            threads: Vec::new(),
            breakpoints: Vec::new(),
            stops: Vec::new(),
            writes: 0,
            watchpoints: Vec::new(),
            settings: 0,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct Target {
    pub kind: TargetKind,

    /// The executable's path.
    pub program: String,

    /// A launched program's arguments.
    pub arguments: Vec<String>,

    /// An attached process, or the process a core dump recorded.
    pub pid: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub enum TargetKind {
    Launch,
    Attach,
    Core,
}

/// The program's execution state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(tag = "state", rename_all = "camelCase")]
pub enum Inferior {
    /// A launched program that has not started, or nothing at all.
    NotStarted,
    Running {
        pid: u64,
    },
    Stopped {
        pid: u64,

        /// The stop's number, which requests about it carry.
        stop: u64,

        /// The thread whose event caused the stop.
        thread: u64,
        reason: StopReason,

        /// The thread's frame the stop selects, which a tab opens on: the
        /// innermost, or the program's frame where a language runtime's
        /// agent stopped it.
        frame: u32,

        /// Where that thread stopped.
        place: Option<Place>,
    },

    /// The program ended; it can be started again.
    Exited {
        description: String,
    },

    /// The debugger let an attached process go.
    Detached {
        pid: u64,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct StopReason {
    /// A short machine-readable kind, such as `breakpoint` or `pause`.
    pub kind: String,

    /// The reason as the CLI says it.
    pub description: String,
}

/// A place in the program's code.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct Place {
    /// The instruction's address, in hexadecimal.
    pub address: String,
    pub function: Option<String>,

    /// The source file, as the debug information records it.
    pub path: Option<String>,
    pub line: Option<u64>,
}

/// One stop in the session's history.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct StopEntry {
    pub stop: u64,
    pub thread: u64,
    pub reason: StopReason,

    /// The thread's frame the stop selects, and where that frame is.
    pub frame: u32,
    pub place: Option<Place>,

    /// Who ran the program to it, when someone did.
    pub by: Option<String>,

    /// What they did, such as `stepped over`.
    pub action: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct Breakpoint {
    pub id: u64,

    /// What it was set at, as it was asked for.
    pub location: String,
    pub condition: Option<String>,
    pub hit_condition: Option<String>,
    pub log_message: Option<String>,

    /// Hits in the current process, including those that did not stop.
    pub hits: u64,

    /// Ready locations; runtime arming problems are described in `pending`.
    pub places: Vec<Place>,

    // Additive and omitted for native breakpoints; old tabs still see
    // only ready places and therefore render unarmed breakpoints pending.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(test, ts(optional))]
    pub pending: Option<PendingBreakpoint>,
}

/// Runtime locations whose desired arming has not been accepted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct PendingBreakpoint {
    pub message: String,
    pub places: Vec<PendingBreakpointPlace>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct PendingBreakpointPlace {
    pub place: Place,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct Thread {
    pub id: u64,
    pub name: Option<String>,
    pub stopped: bool,
}

#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct ErrorBody {
    pub kind: ErrorKind,
    pub message: String,
}

/// Failures the page handles by kind; the message says the rest.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub enum ErrorKind {
    /// The stop a request named has passed.
    StaleStop,

    /// The request needs a stopped program.
    NotStopped,

    /// This connection's role does not allow the request.
    Forbidden,

    /// Something is already being debugged, and the request did not ask to
    /// replace it.
    Busy,

    /// The request was malformed.
    Invalid,

    /// The request cannot work for this session, such as running a core
    /// dump.
    Unsupported,

    /// The debugger tried and failed.
    Failed,
}

#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct Output {
    pub stream: Stream,
    pub text: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub enum Stream {
    Stdout,
    Stderr,

    /// Messages logpoints wrote.
    Log,
}

#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct Presence {
    pub people: Vec<Person>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct Person {
    pub connection: u32,
    pub name: String,
    pub role: Role,
    pub focus: Option<Focus>,
}

#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct Notice {
    /// Who did it.
    pub connection: u32,
    pub name: String,

    /// What they did, such as `continued`.
    pub text: String,
}

/// The answer to `completePath`.
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct PathCompletions {
    pub entries: Vec<PathEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct PathEntry {
    /// The completed text, ending in `/` for a directory.
    pub text: String,
    pub kind: PathKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub enum PathKind {
    Directory,
    Executable,
    File,
}

/// The answer to `processes`.
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct Processes {
    pub processes: Vec<Process>,

    /// Yama's `ptrace_scope`, when the kernel has it: at 1 or more,
    /// attaching to a process that is not a child may be refused.
    pub ptrace_scope: Option<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct Process {
    pub pid: u64,

    /// The command line, or the process's name when it has none.
    pub command: String,
}

/// The answer to `backtrace`.
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct Backtrace {
    pub frames: Vec<Frame>,

    /// The runs of frames of a runtime's or an interpreter's machinery
    /// the stack folds into a line each, as the CLI's `bt` does, with the
    /// innermost frame shown.
    pub folds: Vec<FrameRun>,

    /// Why the stack ends early, when the unwinder could not finish it.
    pub incomplete: Option<String>,
}

/// Frames in a row, by their indices in [`Backtrace::frames`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct FrameRun {
    pub first: u32,
    pub count: u32,
}

/// The language of the code a frame runs: a short mark, and its name.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct FrameLanguage {
    pub mark: String,
    pub name: String,
}

#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct Frame {
    /// Its number, counting from the innermost frame.
    pub index: u32,

    /// The function, symbol, or address.
    pub name: String,
    pub kind: FrameKind,

    /// The instruction or return address, in hexadecimal; for a suspended
    /// task's frame, where it resumes, if that is known.
    pub address: Option<String>,

    /// The file name of the module the code is in.
    pub module: Option<String>,
    pub source: Option<SourceLine>,

    /// The language of its code, where its function or interpreter says.
    pub language: Option<FrameLanguage>,

    /// For a frame that drives frames the stack does not show in full,
    /// the note that says so and why.
    pub unfollowed: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub enum FrameKind {
    Physical,

    /// A call the compiler inlined into the frame below it.
    Inline,

    /// A signal handler's trampoline.
    Signal,

    /// A function that left by a tail call to the frame below it.
    TailCall,

    /// An async function of a suspended task, which its future holds.
    Async,

    /// The future a suspended task's innermost async function awaits.
    Awaited,

    /// A frame of an interpreted language, whose code is in no module.
    Interpreted,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct SourceLine {
    pub path: String,
    pub line: u64,
    pub column: Option<u64>,

    /// The line of the same file the frame's function begins at, where its
    /// runtime or debug information says.
    pub first: Option<u64>,
}

/// The answer to `sources`.
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct SourceFiles {
    /// Paths as the debug information records them, sorted.
    pub files: Vec<String>,

    /// Where the program's main function is declared, when it has one.
    pub entry: Option<SourceLine>,
}

/// The answer to `functions`.
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct Functions {
    pub functions: Vec<FunctionMatch>,

    /// Whether more functions match than were returned.
    pub more: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct FunctionMatch {
    pub name: String,

    /// Where it is declared, when the debug information says.
    pub path: Option<String>,
    pub line: Option<u64>,
}

/// The answer to `source`.
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct SourceText {
    pub path: String,

    /// The file read, which a source map may have moved.
    pub read: String,
    pub text: String,

    /// The lines a breakpoint can stop at, in order.
    pub breakable: Vec<u64>,
}

/// The answer to `addBreakpoint` and `addWatchpoint`: what was added.
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct Added {
    pub id: u64,
}

/// The answer to `scopes`.
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct Scopes {
    pub scopes: Vec<Scope>,
}

#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct Scope {
    pub key: ScopeKey,
    pub name: String,
    pub rows: Vec<Row>,

    /// Why the scope's rows could not be read, when they could not.
    pub problem: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub enum ScopeKey {
    Args,
    Locals,
    Statics,
}

/// One value, as the value tree, watches, and the console show it.
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct Row {
    pub name: String,

    /// The value's summary, or why there is none.
    pub text: String,

    #[serde(rename = "type")]
    pub type_name: Option<String>,

    /// An expression that evaluates the value again.
    pub path: Option<String>,
    pub children: Option<Children>,

    /// Whether `setValue` can change it.
    pub editable: bool,

    /// The address of its natural memory, as hexadecimal.
    pub memory: Option<String>,

    /// How many bytes the value occupies there, when it is stored there.
    pub memory_bytes: Option<u64>,

    /// A row that only says where reading stopped short.
    pub truncated: bool,

    /// The renderers of the drawings the value's view offers, each once.
    pub drawings: Vec<String>,
}

/// What a row expands to.
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct Children {
    pub handle: u64,

    /// How many children are elements, when it is known.
    pub indexed: Option<u64>,

    /// How many are named, when it is known.
    pub named: Option<u64>,
}

/// The answer to `children`.
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct Rows {
    pub rows: Vec<Row>,
}

/// The answer to `complete`.
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct Completions {
    /// Where the completed part starts, in characters.
    pub start: u64,
    pub items: Vec<Completion>,
}

#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct Completion {
    pub label: String,

    /// `keyword`, `value`, `variable`, or `field`.
    pub kind: String,
}

/// The answer to `console`: a command's output, or an expression's value.
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct ConsoleResult {
    pub output: Option<String>,
    pub row: Option<Row>,
}

/// A watchpoint, as every tab shows it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct Watchpoint {
    pub id: u64,
    pub access: WatchAccess,

    /// The expression it watches, when it was given one.
    pub expression: Option<String>,
    pub address: String,
    pub bytes: u64,

    /// Such as `thread 41872's frame at 0x7ffe…, until it returns` for a
    /// local, which ends with its frame; empty for a global.
    pub scope: String,
    pub condition: Option<String>,
    pub hit_condition: Option<String>,
    pub hits: u64,
}

/// The answer to `disassemble`.
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct Disassembled {
    /// The function shown, when the code shown is one.
    pub function: Option<String>,

    /// The frame's instruction: its program counter, or, in a caller, the
    /// call it returns to after.
    pub marked: Option<String>,
    pub instructions: Vec<Instruction>,

    /// What the code shown leaves out, such as unreadable memory.
    pub notes: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct Instruction {
    pub address: String,

    /// Its bytes, as hexadecimal pairs separated by spaces.
    pub bytes: String,

    /// Its text in pieces; empty when the bytes decode to nothing.
    pub tokens: Vec<Token>,

    /// Why there is no instruction, when there is none.
    pub invalid: Option<String>,

    /// Where a branch goes, which a page can follow.
    pub target: Option<BranchTarget>,

    /// What its operands name, such as a function or a global.
    pub comment: Option<String>,

    /// The symbol it is in, with its offset, such as `main+12`.
    pub symbol: Option<String>,

    /// Its source line, when it starts one.
    pub source: Option<SourceLine>,
}

#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct Token {
    /// `mnemonic`, `prefix`, `keyword`, `register`, `number`, `address`,
    /// `punctuation`, or `text`.
    pub kind: String,
    pub text: String,
}

#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct BranchTarget {
    pub address: String,
    pub name: Option<String>,
}

/// A drawing of a value at a frame.
#[derive(Debug, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct Draw {
    #[serde(flatten)]
    pub at: FrameAt,

    /// The value's `path`, which evaluates it.
    pub path: String,

    /// The renderer: one whose drawing the value's view offers, or else any
    /// renderer, which then draws the value itself as its `values`.
    pub renderer: String,
}

#[derive(Debug, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct RendererRef {
    pub digest: String,
}

/// A renderer, by name, where it was loaded from, and the digest of its
/// JavaScript.
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct RendererInfo {
    pub name: String,

    /// The file or module record it was loaded from, or `built-in`.
    pub origin: String,
    pub digest: String,
}

/// The answer to `renderer`.
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct RendererSource {
    #[serde(flatten)]
    pub info: RendererInfo,
    pub source: String,
}

/// The answer to `renderers`: each name once, the one a drawing would use.
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct RendererList {
    pub renderers: Vec<RendererInfo>,
}

/// The answer to `draw`.
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct Drawing {
    pub renderer: RendererInfo,

    /// Whether the value's view offers the drawing; otherwise the renderer
    /// draws the value itself as its `values`.
    pub offered: bool,

    /// Each input, by name, in order; none when a problem kept any part of
    /// them from being read.
    pub inputs: Option<Vec<DrawInput>>,

    /// Why the inputs could not be read whole: the first part that could
    /// not, and why.
    pub problem: Option<String>,

    /// How many bytes the binary frame sent just before this answer holds:
    /// the bytes and numbers the inputs' `bytes` and `numbers` lie in.
    pub bytes: u64,
}

#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct DrawInput {
    pub name: String,
    pub value: Datum,

    /// The part of the drawn value the input is, when it is one.
    pub path: Option<String>,
}

/// A value as a renderer receives it, decided by its type alone.
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(tag = "t", rename_all = "camelCase")]
pub enum Datum {
    Bool {
        b: bool,
    },

    /// An integer of at most 32 bits, or one the view writes, such as
    /// `64`, that a double holds exactly.
    Int {
        i: i64,
    },

    /// An integer of 64 or 128 bits, a pointer, or an address, in decimal.
    Big {
        big: String,
    },

    /// A float, as the shortest text that reads back as its nearest
    /// double: `NaN`, `Infinity`, and `-Infinity` included.
    Float {
        f: String,
    },
    Text {
        s: String,
    },

    /// An enumeration's value and the enumerator it equals.
    Enum {
        name: Option<String>,
        value: Box<Self>,
    },

    /// The variant a sum type holds, and its payload.
    Sum {
        variant: String,
        value: Option<Box<Self>>,
    },
    Record {
        members: Vec<(String, Self)>,
    },
    List {
        items: Vec<Self>,
    },

    /// Numbers of one kind, `count` of them at `offset` in the drawing's
    /// bytes, little-endian.
    Numbers {
        kind: NumberKind,
        offset: u64,
        count: u64,
    },

    /// `length` bytes at `offset` in the drawing's bytes.
    Bytes {
        offset: u64,
        length: u64,
    },

    /// A map's entries, each a key and a value, in its view's order.
    Entries {
        entries: Vec<(Self, Self)>,
    },
    Null,
}

/// The kind of number each of a sequence's elements is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub enum NumberKind {
    I8,
    U8,
    I16,
    U16,
    I32,
    U32,
    I64,
    U64,
    F32,
    F64,
}

/// The answer to `readMemory`.
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct Memory {
    pub address: String,

    /// The bytes read, as hexadecimal pairs with no separator.
    pub bytes: String,

    /// The first address that could not be read, when the read stopped
    /// short.
    pub unreadable: Option<String>,
}

/// The calls a step into can go into, in address order.
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct StepTargets {
    pub calls: Vec<StepCall>,
}

/// One call of a line.
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct StepCall {
    /// The call instruction's address, in hexadecimal, which names it to a
    /// step.
    pub call: String,

    /// The function it calls, when something names it.
    pub callee: Option<String>,

    /// The address a direct call calls, in hexadecimal; none for an
    /// indirect call.
    pub target: Option<String>,
}

/// The tasks of the program's runtimes at a stop.
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct TaskList {
    /// What the runtimes call a task, such as `task` or `goroutine`.
    pub noun: Option<String>,
    pub tasks: Vec<Task>,

    /// Whether there are more tasks than those listed, which the list
    /// leaves out to stay small.
    pub more: bool,

    /// Why the list may be missing tasks, or describe some wrongly.
    pub gaps: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct Task {
    pub key: TaskKey,
    pub state: TaskState,

    /// The code the program wrote that it is in, as the CLI says it.
    pub place: String,

    /// The frame of that code in the task's stack, which choosing the task
    /// shows; absent for a task that runs only its runtime's code.
    pub frame: Option<Frame>,

    /// The runtime's own words for what it does or waits for.
    pub detail: Option<String>,

    /// Labels the program or runtime gave it, such as `{runtime: "local
    /// set 3"}`.
    pub labels: Option<String>,

    /// The thread running it.
    pub thread: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub enum TaskState {
    Running,
    Runnable,
    Blocked,
    Exited,
    Unknown,
}

#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct Registers {
    pub registers: Vec<Register>,
}

#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct Register {
    pub name: String,

    /// As hexadecimal; absent where a caller's frame did not save it.
    pub value: Option<String>,
    pub bits: u16,

    /// `pc`, `sp`, or `fp`.
    pub role: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct Signals {
    pub signals: Vec<SignalPolicy>,
}

#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct Exceptions {
    pub filters: Vec<ExceptionFilter>,
}

/// The exceptions one filter of a language runtime passes.
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct ExceptionFilter {
    /// The name it is chosen by, such as `python-uncaught`.
    pub id: String,
    pub language: String,

    /// A few words naming what stops, and a sentence saying where.
    pub label: String,
    pub description: String,

    /// Whether they stop the program.
    pub on: bool,
}

#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct Modules {
    pub modules: Vec<Module>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct Module {
    pub id: u64,
    pub name: String,
    pub path: String,

    /// Where it is loaded, as hexadecimal.
    pub start: Option<String>,
    pub end: Option<String>,

    /// `debug` with debug information, `symbols` with only a symbol table,
    /// or `none`.
    pub symbols: String,

    /// The separate file its debug information came from, when its own
    /// file was stripped of it.
    pub debug_file: Option<String>,

    /// Why the separate debug file found for it could not be used.
    pub debug_file_problem: Option<String>,

    /// Why not all of its debug information could be read.
    pub debug_information_problem: Option<String>,
}

/// The answer to `share`.
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct ShareLink {
    pub url: String,
}

#[cfg(test)]
mod tests {
    use ts_rs::{Config, TS};

    use super::*;

    const GENERATED: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/web/src/protocol.gen.ts");

    /// Every message type, in the order the TypeScript declares them.
    const DECLARATIONS: &[fn(&Config) -> String] = &[
        Role::decl,
        Envelope::decl,
        Request::decl,
        SetName::decl,
        Share::decl,
        CompletePath::decl,
        Launch::decl,
        Attach::decl,
        OpenCore::decl,
        Continue::decl,
        Step::decl,
        Jump::decl,
        StepKind::decl,
        SetFocus::decl,
        Focus::decl,
        ThreadAt::decl,
        SourcePath::decl,
        AddBreakpoint::decl,
        EditBreakpoint::decl,
        BreakpointRef::decl,
        Input::decl,
        ServerMessage::decl,
        Hello::decl,
        State::decl,
        Target::decl,
        TargetKind::decl,
        Inferior::decl,
        StopReason::decl,
        Place::decl,
        StopEntry::decl,
        Breakpoint::decl,
        PendingBreakpoint::decl,
        PendingBreakpointPlace::decl,
        Thread::decl,
        ErrorBody::decl,
        ErrorKind::decl,
        Output::decl,
        Stream::decl,
        Presence::decl,
        Person::decl,
        Notice::decl,
        PathCompletions::decl,
        PathEntry::decl,
        PathKind::decl,
        Processes::decl,
        Process::decl,
        ShareLink::decl,
        Backtrace::decl,
        FrameRun::decl,
        Frame::decl,
        FrameKind::decl,
        FrameLanguage::decl,
        SourceLine::decl,
        SourceFiles::decl,
        FunctionQuery::decl,
        Functions::decl,
        FunctionMatch::decl,
        SourceText::decl,
        Added::decl,
        FrameAt::decl,
        ChildrenOf::decl,
        Evaluate::decl,
        SetValue::decl,
        Complete::decl,
        ConsoleLine::decl,
        Scopes::decl,
        Scope::decl,
        ScopeKey::decl,
        Row::decl,
        Children::decl,
        Rows::decl,
        Completions::decl,
        Completion::decl,
        ConsoleResult::decl,
        Disassemble::decl,
        ReadMemory::decl,
        WriteMemory::decl,
        AddWatchpoint::decl,
        EditWatchpoint::decl,
        WatchpointRef::decl,
        WatchAccess::decl,
        Syntax::decl,
        SignalPolicy::decl,
        Watchpoint::decl,
        Disassembled::decl,
        Instruction::decl,
        Token::decl,
        BranchTarget::decl,
        Memory::decl,
        StepTargets::decl,
        StepCall::decl,
        Registers::decl,
        Register::decl,
        Signals::decl,
        Exceptions::decl,
        ExceptionFilter::decl,
        ExceptionChoice::decl,
        Modules::decl,
        Module::decl,
        StopAt::decl,
        TaskKey::decl,
        TaskList::decl,
        Task::decl,
        TaskState::decl,
        Draw::decl,
        RendererRef::decl,
        RendererInfo::decl,
        RendererSource::decl,
        RendererList::decl,
        Drawing::decl,
        DrawInput::decl,
        Datum::decl,
        NumberKind::decl,
        ProfileList::decl,
        ProfileInfo::decl,
        ProfileRef::decl,
        ProfileQuery::decl,
        ProfileTransform::decl,
        TransformOp::decl,
        ProfileGranularity::decl,
        ProfileLook::decl,
        ProfileTimelineQuery::decl,
        ProfileSummary::decl,
        ProfileThread::decl,
        ProfileCount::decl,
        ProfileSetting::decl,
        ProfileTree::decl,
        ProfileKey::decl,
        ProfileKeyKind::decl,
        ProfileTask::decl,
        ProfileSource::decl,
        ProfileFunctions::decl,
        ProfileCost::decl,
        ProfileTimeline::decl,
        ProfileLane::decl,
        ProfileMarker::decl,
        ProfileProbes::decl,
        ProfileZoneSite::decl,
        ProfileZonesQuery::decl,
        ProfileZones::decl,
        ProfileMark::decl,
        ProfileFunctionQuery::decl,
        ProfileLines::decl,
        ProfileSourceFile::decl,
        ProfileCode::decl,
        ProfileCodeBlock::decl,
        ProfileInstruction::decl,
        ProfileBase::decl,
        ProfileBaseWeight::decl,
        ProfilePlot::decl,
        ProfileMarkKind::decl,
        ProfileZoneStatsQuery::decl,
        ProfileZoneStats::decl,
        SyscallQuery::decl,
        SyscallGroup::decl,
        SyscallSummaryQuery::decl,
        SyscallSummary::decl,
        SyscallRow::decl,
        SyscallCount::decl,
        SyscallRateBy::decl,
        SyscallRateQuery::decl,
        SyscallRate::decl,
        SyscallSeries::decl,
        SyscallHeatmapQuery::decl,
        SyscallHeatmap::decl,
        SyscallDetailQuery::decl,
        SyscallDetail::decl,
        SyscallCaller::decl,
        SyscallLogQuery::decl,
        SyscallLog::decl,
        SyscallLine::decl,
        SyscallToken::decl,
        SyscallCallQuery::decl,
        SyscallCall::decl,
        SyscallArgument::decl,
        SyscallSite::decl,
        SyscallLanesQuery::decl,
        SyscallLanes::decl,
        SyscallLane::decl,
        SyscallFiles::decl,
        SyscallFile::decl,
        SyscallNetwork::decl,
        SyscallEndpoint::decl,
        SyscallProcesses::decl,
        RecordSyscalls::decl,
        SyscallInFlightQuery::decl,
        SyscallInFlight::decl,
        SyscallBlocked::decl,
        SyscallProcess::decl,
        ProfileZoneAt::decl,
        ProfileLabelValue::decl,
        RecordProfile::decl,
        RecordPreset::decl,
        RecordStarted::decl,
    ];

    /// Writes every message type as TypeScript.
    fn typescript() -> String {
        let config = Config::new().with_large_int("number");
        let mut text = format!(
            "// Generated from src/web/protocol.rs by `cargo test`; do not edit.\n\n\
             export const PROTOCOL_VERSION = {VERSION};\n"
        );
        for declare in DECLARATIONS {
            text.push('\n');
            text.push_str("export ");
            text.push_str(&declare(&config));
            text.push('\n');
        }

        text
    }

    /// Fails when the checked-in TypeScript is stale. Set
    /// `MICROSCOPE_UPDATE_PROTOCOL=1` to rewrite it.
    #[test]
    fn typescript_matches_the_protocol() {
        let expected = typescript();
        if std::env::var_os("MICROSCOPE_UPDATE_PROTOCOL").is_some() {
            std::fs::write(GENERATED, &expected).expect("write the generated protocol");
            return;
        }

        let actual = std::fs::read_to_string(GENERATED).unwrap_or_default();
        assert!(
            actual == expected,
            "web/src/protocol.gen.ts is stale; rerun with MICROSCOPE_UPDATE_PROTOCOL=1"
        );
    }

    #[test]
    fn requests_parse_with_and_without_parameters() {
        let parse = |text: &str| serde_json::from_str::<Envelope>(text).expect(text);
        assert!(matches!(
            parse(r#"{"id":1,"method":"pause"}"#).request,
            Request::Pause
        ));
        assert!(matches!(
            parse(r#"{"id":2,"method":"continue","params":{"stop":7}}"#).request,
            Request::Continue(Continue { stop: Some(7) })
        ));
        let Request::Launch(launch) = parse(
            r#"{"id":3,"method":"launch","params":{"program":"./a","environment":[["A","1"]]}}"#,
        )
        .request
        else {
            panic!("a launch");
        };

        assert_eq!(launch.environment, [("A".to_owned(), "1".to_owned())]);
        assert!(!launch.replace && !launch.run);
        assert!(
            serde_json::from_str::<Envelope>(r#"{"id":4,"method":"format the disk"}"#).is_err()
        );
    }
}
