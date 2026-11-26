use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const VERSION: u32 = 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub enum Role {
    Control,

    View,
}

#[derive(Debug, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct Envelope {
    pub id: u64,

    #[serde(flatten)]
    pub request: Request,
}

mod syscalls;
pub use syscalls::*;

#[derive(Debug, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(tag = "method", content = "params", rename_all = "camelCase")]
pub enum Request {
    SetName(SetName),

    Share(Share),

    CompletePath(CompletePath),

    Processes,

    Launch(Launch),

    Attach(Attach),

    OpenCore(OpenCore),

    End,

    Continue(Continue),

    Pause,

    Kill,

    Restart,

    SelectMember(MemberRef),

    DetachMember(MemberRef),

    Step(Step),

    StepTargets(ThreadAt),

    Jump(Jump),

    Advance(Advance),

    SetFocus(SetFocus),

    Backtrace(ThreadAt),

    Sources,

    Source(SourcePath),

    AddBreakpoint(AddBreakpoint),

    EditBreakpoint(EditBreakpoint),
    RemoveBreakpoint(BreakpointRef),

    Input(Input),

    Scopes(FrameAt),

    Children(ChildrenOf),

    Evaluate(Evaluate),

    SetValue(SetValue),

    Complete(Complete),

    Console(ConsoleLine),

    Disassemble(Disassemble),

    ReadMemory(ReadMemory),

    WriteMemory(WriteMemory),

    Registers(FrameAt),

    AddWatchpoint(AddWatchpoint),

    EditWatchpoint(EditWatchpoint),
    RemoveWatchpoint(WatchpointRef),

    Signals,

    SetSignal(SignalPolicy),

    Exceptions,

    SetException(ExceptionChoice),

    Modules,

    Functions(FunctionQuery),

    Resolve(SymbolName),

    Tasks(StopAt),

    Draw(Draw),

    Renderer(RendererRef),

    Renderers,

    ReloadViews,

    Profile(ProfileRef),

    ProfileInspect(ProfileRef),

    ProfileTree(ProfileQuery),

    ProfileFunctions(ProfileQuery),

    ProfileTimeline(ProfileTimelineQuery),

    ProfileZones(ProfileZonesQuery),

    ProfileZoneStats(ProfileZoneStatsQuery),

    ProfileLines(ProfileFunctionQuery),

    ProfileCode(ProfileFunctionQuery),

    RecordProfile(RecordProfile),

    StopRecording(ProfileRef),

    SyscallSummary(SyscallSummaryQuery),

    SyscallHeatmap(SyscallHeatmapQuery),

    SyscallDetail(SyscallDetailQuery),

    SyscallLog(SyscallLogQuery),

    SyscallCall(SyscallCallQuery),

    SyscallFiles(SyscallQuery),

    SyscallNetwork(SyscallQuery),

    SyscallProcesses(SyscallQuery),

    SyscallInFlight(SyscallInFlightQuery),

    CloudStatus,

    CloudLoginStart(CloudLoginStart),

    CloudLoginPoll(CloudLoginPoll),

    ShareProfile(ShareProfile),
}

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

    pub to: String,
}

#[derive(Debug, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct CompletePath {
    pub text: String,
}

#[derive(Debug, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct Launch {
    pub program: String,

    #[serde(default)]
    pub arguments: Vec<String>,

    #[serde(default)]
    pub cwd: Option<String>,

    #[serde(default)]
    pub environment: Vec<(String, String)>,

    #[serde(default)]
    pub stop_at_entry: bool,

    #[serde(default)]
    pub run: bool,

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

    #[serde(default)]
    pub executable: Option<String>,

    #[serde(default)]
    pub replace: bool,
}

#[derive(Debug, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct Continue {
    #[serde(default)]
    pub stop: Option<u64>,
}

#[derive(Debug, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct Step {
    pub stop: u64,
    pub thread: u64,

    #[serde(default)]
    #[cfg_attr(test, ts(optional = nullable))]
    pub task: Option<TaskKey>,

    #[serde(default)]
    #[cfg_attr(test, ts(optional = nullable))]
    pub frame: u32,
    pub kind: StepKind,

    #[serde(default)]
    #[cfg_attr(test, ts(optional = nullable))]
    pub call: Option<String>,
}

#[derive(Debug, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct Jump {
    pub stop: u64,
    pub thread: u64,

    #[serde(default)]
    #[cfg_attr(test, ts(optional = nullable))]
    pub task: Option<TaskKey>,

    pub location: String,
}

#[derive(Debug, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct Advance {
    pub stop: u64,
    pub thread: u64,

    #[serde(default)]
    #[cfg_attr(test, ts(optional = nullable))]
    pub task: Option<TaskKey>,

    #[serde(default)]
    #[cfg_attr(test, ts(optional = nullable))]
    pub frame: u32,

    pub location: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub enum StepKind {
    Over,

    Into,

    Out,

    Instruction,

    OverInstruction,
}

#[derive(Debug, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct SetFocus {
    pub focus: Option<Focus>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct Focus {
    pub url: String,

    pub label: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct TaskKey {
    pub runtime: u32,

    #[serde(with = "decimal")]
    #[cfg_attr(test, ts(type = "string"))]
    pub number: u64,
}

mod decimal {
    use serde::{Deserialize, Deserializer, Serializer, de::Error};

    #[expect(
        clippy::trivially_copy_pass_by_ref,
        reason = "serde's `with` passes the field by reference"
    )]
    pub fn serialize<S: Serializer>(value: &u64, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(value)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<u64, D::Error> {
        let text = <std::borrow::Cow<'de, str>>::deserialize(deserializer)?;

        if text.is_empty() || !text.bytes().all(|byte| byte.is_ascii_digit()) {
            return Err(D::Error::custom(format!("{text:?} is no decimal number")));
        }
        text.parse()
            .map_err(|_| D::Error::custom(format!("{text} does not fit in 64 bits")))
    }
}

impl TaskKey {
    pub const fn id(self) -> microscope::TaskId {
        microscope::TaskId {
            runtime: microscope::RuntimeId::new(self.runtime),
            number: self.number,
        }
    }
}

pub const fn execution(thread: u64, task: Option<TaskKey>) -> microscope::ExecutionContext {
    match task {
        Some(task) => microscope::ExecutionContext::Task(task.id()),
        None => microscope::ExecutionContext::Thread(microscope::ThreadId::new(thread)),
    }
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct ThreadAt {
    pub stop: u64,
    pub thread: u64,

    #[serde(default)]
    #[cfg_attr(test, ts(optional = nullable))]
    pub task: Option<TaskKey>,
}

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

    pub path: String,

    pub value: String,
}

#[derive(Debug, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct Complete {
    pub text: String,

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

    #[serde(default)]
    #[cfg_attr(test, ts(optional = nullable))]
    pub address: Option<String>,

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
    pub stop: u64,

    pub address: String,
    pub count: u64,
}

#[derive(Debug, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct WriteMemory {
    pub stop: u64,
    pub address: String,

    pub bytes: String,
}

#[derive(Debug, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct AddWatchpoint {
    pub target: String,
    pub access: WatchAccess,

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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub enum WatchAccess {
    Change,

    Write,

    ReadWrite,
    Read,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct ExceptionChoice {
    pub id: String,
    pub on: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct SignalPolicy {
    pub signal: u64,

    #[serde(default)]
    pub name: String,
    pub stop: bool,
    pub print: bool,
    pub pass: bool,
}

#[derive(Debug, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct SymbolName {
    pub name: String,
}

#[derive(Debug, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct FunctionQuery {
    pub query: String,

    #[serde(default)]
    #[cfg_attr(test, ts(optional = nullable))]
    pub limit: Option<u32>,
}

#[derive(Debug, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct SourcePath {
    pub path: String,
}

#[derive(Debug, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct AddBreakpoint {
    pub location: String,

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
#[serde(rename_all = "camelCase")]
pub struct EditBreakpoint {
    pub id: u64,

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
pub struct MemberRef {
    pub member: u32,
}

#[derive(Debug, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct Input {
    pub text: String,

    #[serde(default)]
    pub eof: bool,
}

#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum ServerMessage {
    /// The first message on every connection.
    Hello(Hello),

    /// The whole debugger state, sent on connecting and at every change.
    State(Arc<State>),

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

    /// they change.
    Profiles(ProfileList),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct ProfileList {
    pub profiles: Vec<ProfileInfo>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct ProfileInfo {
    pub id: String,

    pub name: String,

    pub recording: bool,

    pub revision: u32,

    pub ended: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct ProfileRef {
    pub profile: String,

    #[serde(default)]
    #[cfg_attr(test, ts(optional = nullable))]
    pub revision: u32,
}

#[derive(Debug, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct RecordProfile {
    pub program: String,

    #[serde(default)]
    pub arguments: Vec<String>,

    #[serde(default)]
    #[cfg_attr(test, ts(optional = nullable))]
    pub cwd: Option<String>,

    #[serde(default)]
    pub environment: Vec<(String, String)>,

    #[serde(default)]
    pub preset: RecordPreset,

    #[serde(default)]
    #[cfg_attr(test, ts(optional = nullable))]
    pub function: Option<String>,

    #[serde(default)]
    #[cfg_attr(test, ts(optional = nullable))]
    pub window: Option<f64>,

    #[serde(default)]
    #[cfg_attr(test, ts(optional = nullable))]
    pub syscalls: Option<SyscallsLevel>,

    #[serde(default)]
    #[cfg_attr(test, ts(optional = nullable))]
    pub syscall_engine: Option<SyscallsEngine>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub enum SyscallsEngine {
    Bpf,

    Ptrace,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub enum SyscallsLevel {
    Off,

    Counts,

    Calls,

    Full,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub enum RecordPreset {
    Production,

    #[default]
    Development,

    Maximum,
}

#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct RecordStarted {
    pub profile: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct ProfileQuery {
    pub profile: String,

    #[serde(default)]
    #[cfg_attr(test, ts(optional = nullable))]
    pub revision: u32,

    #[serde(default)]
    #[cfg_attr(test, ts(optional = nullable))]
    pub range: Option<[i64; 2]>,

    #[serde(default)]
    #[cfg_attr(test, ts(optional = nullable))]
    pub threads: Option<Vec<u32>>,

    #[serde(default)]
    pub source: ProfileSource,

    #[serde(default)]
    #[cfg_attr(test, ts(optional = nullable))]
    pub tasks: Option<Vec<Option<u32>>>,

    #[serde(default)]
    pub by_task: bool,

    #[serde(default)]
    pub labels: Vec<String>,

    #[serde(default)]
    #[cfg_attr(test, ts(optional = nullable))]
    pub by_label: Option<String>,

    #[serde(default)]
    pub transforms: Vec<ProfileTransform>,

    #[serde(default)]
    pub invert: bool,

    #[serde(default)]
    pub look: ProfileLook,

    #[serde(default)]
    pub granularity: ProfileGranularity,

    #[serde(default)]
    #[cfg_attr(test, ts(optional = nullable))]
    pub zones: Option<ProfileZoneScope>,

    #[serde(default)]
    #[cfg_attr(test, ts(optional = nullable))]
    pub base: Option<ProfileBase>,

    #[serde(default)]
    #[cfg_attr(test, ts(optional = nullable))]
    pub during: Option<ProfileDuring>,

    #[serde(default)]
    pub syscall_leaves: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct ProfileDuring {
    pub filter: String,

    #[serde(default)]
    #[cfg_attr(test, ts(optional = nullable))]
    pub latency: Option<u32>,

    #[serde(default)]
    #[cfg_attr(test, ts(optional = nullable))]
    pub row: Option<SyscallRowKey>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct ProfileZoneScope {
    pub name: String,

    #[serde(default)]
    #[cfg_attr(test, ts(optional = nullable))]
    pub band: Option<[u64; 2]>,

    #[serde(default)]
    pub outside: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct ProfileBase {
    pub profile: String,

    #[serde(default)]
    #[cfg_attr(test, ts(optional = nullable))]
    pub range: Option<[i64; 2]>,

    #[serde(default)]
    #[cfg_attr(test, ts(optional = nullable))]
    pub zones: Option<ProfileZoneScope>,
}

#[cfg(test)]
#[derive(Debug, Clone, Copy, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct ProfileBaseWeight {
    pub weight: u64,
    pub samples: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub enum ProfileSource {
    #[default]
    Samples,

    Waits,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct ProfileTransform {
    pub op: TransformOp,

    #[serde(default)]
    pub name: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub enum TransformOp {
    Focus,

    Merge,

    Drop,

    CollapseCycles,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub enum ProfileLook {
    #[default]
    Code,

    Tasks,

    Native,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub enum ProfileGranularity {
    #[default]
    Function,

    Physical,

    Line,

    Address,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct ProfileFunctionQuery {
    #[serde(flatten)]
    pub query: ProfileQuery,
    pub function: String,
}

#[cfg(test)]
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct ProfileLines {
    pub weight: u64,
    pub files: Vec<ProfileSourceFile>,
}

#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct ProfileSourceFile {
    pub path: Option<String>,

    pub lines: Vec<[u64; 3]>,

    pub first: u32,
    pub text: Option<Vec<String>>,
    pub unread: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct ProfileCode {
    pub weight: u64,
    pub module: Option<String>,
    pub blocks: Vec<ProfileCodeBlock>,

    pub unplaced: Vec<[u64; 3]>,
    pub unavailable: Option<String>,

    pub functions: Vec<String>,

    pub files: Vec<ProfileSourceFile>,

    pub unlined: Option<String>,

    pub inlined_into: Option<String>,
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

    pub tokens: Vec<ProfileToken>,

    pub target: Option<String>,
    pub own: u64,
    pub total: u64,

    pub lines: Vec<ProfileCodeLine>,
}

#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct ProfileToken(pub String, pub String);

#[derive(Debug, Clone, Copy, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct ProfileCodeLine(pub u32, pub Option<u32>, pub u32);

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct ProfileTimelineQuery {
    #[serde(flatten)]
    pub query: ProfileQuery,

    pub buckets: u32,

    #[serde(default)]
    pub task_lanes: bool,
}

#[cfg(test)]
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct ProfileSummary {
    pub id: String,
    pub name: String,

    pub program: Option<String>,

    pub command: Option<ProfileCommand>,

    pub unit: String,

    pub source: Option<String>,
    pub samples: u64,
    pub weight: u64,

    pub syscalls: u64,

    pub syscalls_lost: SyscallLosses,

    pub duration: i64,
    pub threads: Vec<ProfileThread>,

    pub lost: u64,

    pub throttles: u64,

    pub truncations: Vec<ProfileCount>,

    pub settings: Vec<ProfileSetting>,

    pub recorder_cpu: u64,
    pub recorder_loading: u64,

    pub behind: Vec<[i64; 2]>,

    pub budget_changes: Vec<ProfileMarker>,

    pub tasks: Vec<ProfileTask>,

    pub interpreted: bool,

    pub snapshots: u64,
    pub snapshot_pause: u64,
    pub task_samples: u64,

    pub snapshot_problems: Vec<ProfileMarker>,

    pub waited: u64,

    pub probes: ProfileProbes,

    pub zone_sites: Vec<ProfileZoneSite>,

    pub label_keys: Vec<String>,

    pub probe_problems: Vec<ProfileMarker>,
}

#[cfg(test)]
#[derive(Debug, Clone, Copy, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct ProfileProbes {
    pub events: u64,

    pub overwritten: u64,

    pub no_ring: u64,

    pub interrupted: u64,

    pub malformed: u64,

    pub plotted: u64,
}

#[cfg(test)]
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct ProfileZoneSite {
    pub name: String,

    pub label: bool,
    pub count: u64,

    pub total: u64,
    pub longest: u64,

    pub flagged: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct ProfileZonesQuery {
    pub profile: String,

    #[serde(default)]
    #[cfg_attr(test, ts(optional = nullable))]
    pub revision: u32,

    #[serde(default)]
    #[cfg_attr(test, ts(optional = nullable))]
    pub range: Option<[i64; 2]>,

    #[serde(default)]
    #[cfg_attr(test, ts(optional = nullable))]
    pub threads: Option<Vec<u32>>,

    pub width: u32,

    #[serde(default)]
    #[cfg_attr(test, ts(optional = nullable))]
    pub site: Option<String>,

    #[serde(default)]
    #[cfg_attr(test, ts(optional = nullable))]
    pub band: Option<[u64; 2]>,
}

#[cfg(test)]
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct ProfileZones {
    pub range: [i64; 2],
    pub blocks: u32,

    pub off_cpu: bool,
    pub names: Vec<String>,

    pub marks: Vec<ProfileMark>,

    pub plots: Vec<ProfilePlot>,
}

#[cfg(test)]
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct ProfilePlot {
    pub name: String,

    pub values: u64,
    pub unshown: u64,

    pub bins: Vec<[f64; 5]>,

    pub held: Option<f64>,

    pub continues: bool,
}

#[cfg(test)]
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct ProfileMark {
    pub thread: u32,

    pub time: i64,
    pub kind: ProfileMarkKind,

    pub name: String,
    pub text: Option<String>,
}

#[cfg(test)]
#[expect(
    dead_code,
    reason = "serialized by microscope_data::view and exported via ts-rs"
)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub enum ProfileMarkKind {
    Message,
    Frame,
    Trigger,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct ProfileZoneStatsQuery {
    pub profile: String,

    #[serde(default)]
    #[cfg_attr(test, ts(optional = nullable))]
    pub revision: u32,

    pub name: String,

    #[serde(default)]
    #[cfg_attr(test, ts(optional = nullable))]
    pub range: Option<[i64; 2]>,

    #[serde(default)]
    #[cfg_attr(test, ts(optional = nullable))]
    pub threads: Option<Vec<u32>>,

    #[serde(default)]
    #[cfg_attr(test, ts(optional = nullable))]
    pub band: Option<[u64; 2]>,

    #[serde(default)]
    #[cfg_attr(test, ts(optional = nullable))]
    pub rank_of: Option<u64>,
}

#[cfg(test)]
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

    pub histogram: Vec<[u64; 2]>,

    pub longest: Vec<ProfileZoneAt>,

    pub banded: [u64; 2],

    pub shorter: u64,

    pub values: Vec<ProfileLabelValue>,
}

#[cfg(test)]
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct ProfileZoneAt {
    pub thread: u32,

    pub start: i64,
    pub end: i64,
    pub value: Option<String>,
}

#[cfg(test)]
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct ProfileLabelValue {
    pub value: String,
    pub total: u64,
    pub count: u64,
}

#[cfg(test)]
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct ProfileTask {
    pub row: u32,

    pub name: String,

    pub weight: u64,
    pub waited: u64,
}

#[cfg(test)]
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct ProfileMarker {
    pub time: i64,
    pub text: String,
}

#[cfg(test)]
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct ProfileInspection {
    pub processes: Vec<ProfileProcess>,
    pub modules: Vec<ProfileModule>,

    pub depths: Vec<u64>,

    pub over_time: Vec<u64>,
    pub distinct_stacks: u64,
    pub frames: u64,
    pub frames_with_lines: u64,

    pub counted: u64,

    pub stack_bytes: u64,

    pub shortened: u64,

    pub recorder_setup: u64,
    pub recorder_finishing: u64,

    pub tables: Vec<ProfileTable>,

    pub file: Option<ProfileFile>,
}

#[cfg(test)]
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct ProfileCommand {
    pub arguments: Option<Vec<String>>,
    pub text: String,
}

#[cfg(test)]
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct ProfileProcess {
    pub pid: u32,
    pub name: String,
    pub command: Option<ProfileCommand>,
}

#[cfg(test)]
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct ProfileModule {
    pub path: String,
    pub size: u64,
    pub build_id: Option<String>,

    pub frames: u64,
}

#[cfg(test)]
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct ProfileTable {
    pub name: String,
    pub bytes: u64,
}

#[cfg(test)]
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct ProfileFile {
    pub bytes: Option<u64>,
    pub chunks: u64,

    pub truncated: bool,
}

#[cfg(test)]
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct ProfileThread {
    pub row: u32,
    pub pid: u32,
    pub tid: u32,
    pub name: Option<String>,
    pub samples: u64,
    pub weight: u64,

    pub zones: u64,

    pub blocked: u64,
    pub preempted: u64,
}

#[cfg(test)]
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct ProfileCount {
    pub what: String,
    pub count: u64,
}

#[cfg(test)]
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct ProfileSetting {
    pub name: String,
    pub value: String,
}

#[cfg(test)]
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct ProfileTree {
    pub nodes: u32,

    pub weight: u64,
    pub samples: u64,
    pub keys: Vec<ProfileKey>,

    pub base: Option<ProfileBaseWeight>,
}

#[cfg(test)]
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct ProfileKey {
    pub name: String,
    pub kind: ProfileKeyKind,

    pub module: Option<String>,

    pub file: Option<String>,
    pub line: Option<u32>,
}

#[cfg(test)]
#[expect(
    dead_code,
    reason = "serialized by microscope_data::view and exported via ts-rs"
)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub enum ProfileKeyKind {
    Function,
    Line,

    Address,

    Truncation,

    Task,

    Group,

    Label,

    Syscall,
}

#[cfg(test)]
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct ProfileFunctions {
    pub weight: u64,

    pub rows: Vec<ProfileCost>,
    pub keys: Vec<ProfileKey>,

    pub base: Option<ProfileBaseWeight>,
}

#[cfg(test)]
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct ProfileCost {
    pub key: u32,
    pub own: u64,
    pub total: u64,

    pub base_own: u64,
    pub base_total: u64,
}

#[cfg(test)]
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct ProfileTimeline {
    pub range: [i64; 2],
    pub lanes: Vec<ProfileLane>,
}

#[cfg(test)]
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct ProfileLane {
    pub thread: Option<u32>,
    pub task: Option<u32>,
    pub weights: Vec<u64>,
    pub blocked: Vec<u64>,
    pub preempted: Vec<u64>,

    pub syscalls: Vec<u32>,
    pub failed: Vec<u32>,
    pub waits: Vec<ProfileWait>,
}

#[cfg(test)]
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct ProfileWait {
    pub call: String,
    pub start: i64,
    pub end: i64,
}

#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct Hello {
    pub version: u32,

    pub connection: u32,
    pub role: Role,

    pub name: String,

    pub cwd: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct State {
    pub session: Option<String>,

    pub target: Option<Target>,

    pub busy: Option<String>,

    pub revision: u64,
    pub inferior: Inferior,
    pub threads: Vec<Thread>,
    pub breakpoints: Vec<Breakpoint>,

    pub stops: Vec<StopEntry>,

    pub writes: u64,
    pub watchpoints: Vec<Watchpoint>,

    pub settings: u64,

    pub group: Option<Group>,
}

impl State {
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
            group: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct Group {
    pub members: Vec<Member>,

    pub selected: u32,

    pub stopped: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct Member {
    pub number: u32,
    pub pid: Option<u64>,

    pub parent: Option<u32>,

    pub command: String,
    pub joined: Joined,

    pub program: Option<String>,
    pub state: MemberState,

    pub description: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub enum Joined {
    Launched,

    Attached,

    Forked,

    Executed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub enum MemberState {
    NotStarted,
    Running,
    Stopped,
    Exited,

    Watched,

    Detached,

    Lost,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct Target {
    pub kind: TargetKind,

    pub program: String,

    pub arguments: Vec<String>,

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

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(tag = "state", rename_all = "camelCase")]
pub enum Inferior {
    NotStarted,
    Running {
        pid: u64,
    },
    Stopped {
        pid: u64,

        stop: u64,

        thread: u64,
        reason: StopReason,

        frame: u32,

        place: Option<Place>,
    },

    Exited {
        description: String,
    },

    Detached {
        pid: u64,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct StopReason {
    pub kind: String,

    pub description: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct Place {
    pub address: String,
    pub function: Option<String>,

    pub path: Option<String>,
    pub line: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct StopEntry {
    pub stop: u64,
    pub thread: u64,
    pub reason: StopReason,

    pub frame: u32,
    pub place: Option<Place>,

    pub by: Option<String>,

    pub action: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct Breakpoint {
    pub id: u64,

    pub location: String,
    pub condition: Option<String>,
    pub hit_condition: Option<String>,
    pub log_message: Option<String>,

    pub hits: u64,

    pub places: Vec<Place>,

    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(test, ts(optional))]
    pub pending: Option<PendingBreakpoint>,
}

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

    pub labels: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct ErrorBody {
    pub kind: ErrorKind,
    pub message: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub enum ErrorKind {
    StaleStop,

    NotStopped,

    Forbidden,

    Busy,

    Invalid,

    Unsupported,

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
    pub connection: u32,
    pub name: String,

    pub text: String,
}

#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct PathCompletions {
    pub entries: Vec<PathEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct PathEntry {
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

#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct Processes {
    pub processes: Vec<Process>,

    pub ptrace_scope: Option<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct Process {
    pub pid: u64,

    pub command: String,
}

#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct Backtrace {
    pub frames: Vec<Frame>,

    pub folds: Vec<FrameRun>,

    pub incomplete: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct FrameRun {
    pub first: u32,
    pub count: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct FrameLanguage {
    pub mark: String,
    pub name: String,
}

#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct Frame {
    pub index: u32,

    pub name: String,
    pub kind: FrameKind,

    pub address: Option<String>,

    pub module: Option<String>,
    pub source: Option<SourceLine>,

    pub language: Option<FrameLanguage>,

    pub unfollowed: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub enum FrameKind {
    Physical,

    Inline,

    Signal,

    TailCall,

    Async,

    Awaited,

    Interpreted,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct SourceLine {
    pub path: String,
    pub line: u64,
    pub column: Option<u64>,

    pub first: Option<u64>,
}

#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct SourceFiles {
    pub files: Vec<String>,

    pub entry: Option<SourceLine>,

    pub note: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct Resolved {
    pub address: String,
}

#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct Functions {
    pub functions: Vec<FunctionMatch>,

    pub more: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct FunctionMatch {
    pub name: String,

    pub path: Option<String>,
    pub line: Option<u64>,
}

#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct SourceText {
    pub path: String,

    pub read: String,
    pub text: String,

    pub breakable: Vec<u64>,
}

#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct Added {
    pub id: u64,
}

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

    pub problem: Option<String>,

    pub note: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub enum ScopeKey {
    Args,
    Locals,
    Closure,
    Module,
    Global,
    Unnamed,
    Statics,
}

#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct Row {
    pub name: String,

    pub text: String,

    pub detail: Option<String>,

    #[serde(rename = "type")]
    pub type_name: Option<String>,

    pub path: Option<String>,
    pub children: Option<Children>,

    pub editable: bool,

    pub memory: Option<String>,

    pub memory_bytes: Option<u64>,

    pub stored: bool,

    pub text_bytes: Option<u64>,

    pub truncated: bool,

    pub drawings: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct Children {
    pub handle: u64,

    pub indexed: Option<u64>,

    pub named: Option<u64>,
}

#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct Rows {
    pub rows: Vec<Row>,
}

#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct Completions {
    pub start: u64,
    pub items: Vec<Completion>,
}

#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct Completion {
    pub label: String,

    pub kind: String,
}

#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct ConsoleResult {
    pub output: Option<String>,
    pub row: Option<Row>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct Watchpoint {
    pub id: u64,
    pub access: WatchAccess,

    pub expression: Option<String>,
    pub address: String,
    pub bytes: u64,

    pub scope: String,
    pub condition: Option<String>,
    pub hit_condition: Option<String>,
    pub hits: u64,
}

#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct Disassembled {
    pub function: Option<String>,

    pub marked: Option<String>,
    pub instructions: Vec<Instruction>,

    pub notes: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct Instruction {
    pub address: String,

    pub bytes: String,

    pub tokens: Vec<Token>,

    pub invalid: Option<String>,

    pub target: Option<BranchTarget>,

    pub comment: Option<String>,

    pub symbol: Option<String>,

    pub source: Option<SourceLine>,
}

#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct Token {
    pub kind: String,
    pub text: String,
}

#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct BranchTarget {
    pub address: String,
    pub name: Option<String>,
}

#[derive(Debug, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct Draw {
    #[serde(flatten)]
    pub at: FrameAt,

    pub path: String,

    pub renderer: String,
}

#[derive(Debug, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct RendererRef {
    pub digest: String,
}

#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct RendererInfo {
    pub name: String,

    pub origin: String,
    pub digest: String,
}

#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct RendererSource {
    #[serde(flatten)]
    pub info: RendererInfo,
    pub source: String,
}

#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct RendererList {
    pub renderers: Vec<RendererInfo>,
}

#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct Drawing {
    pub renderer: RendererInfo,

    pub offered: bool,

    pub inputs: Option<Vec<DrawInput>>,

    pub problem: Option<String>,

    pub bytes: u64,
}

#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct DrawInput {
    pub name: String,
    pub value: Datum,

    pub path: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(tag = "t", rename_all = "camelCase")]
pub enum Datum {
    Bool {
        b: bool,
    },

    Int {
        i: i64,
    },

    Big {
        big: String,
    },

    Float {
        f: String,
    },
    Text {
        s: String,
    },

    Enum {
        name: Option<String>,
        value: Box<Self>,
    },

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

    Numbers {
        kind: NumberKind,
        offset: u64,
        count: u64,
    },

    Bytes {
        offset: u64,
        length: u64,
    },

    Entries {
        entries: Vec<(Self, Self)>,
    },
    Null,
}

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

#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct Memory {
    pub address: String,

    pub bytes: String,

    pub unreadable: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct StepTargets {
    pub calls: Vec<StepCall>,
}

#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct StepCall {
    pub call: String,

    pub callee: Option<String>,

    pub target: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct TaskList {
    pub noun: Option<String>,
    pub tasks: Vec<Task>,

    pub more: bool,

    pub gaps: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct Task {
    pub key: TaskKey,
    pub state: TaskState,

    pub place: String,

    pub frame: Option<Frame>,

    pub detail: Option<String>,

    pub labels: Option<String>,

    pub thread: Option<u64>,

    pub stackless: bool,
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

    pub value: Option<String>,
    pub bits: u16,

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

#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct ExceptionFilter {
    pub id: String,
    pub language: String,

    pub label: String,
    pub description: String,

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

    pub start: Option<String>,
    pub end: Option<String>,

    pub symbols: String,

    pub debug_file: Option<String>,

    pub debug_file_problem: Option<String>,

    pub debug_information_problem: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct ShareLink {
    pub url: String,
}

#[derive(Debug, Clone, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct CloudLoginStart {
    #[serde(default)]
    #[cfg_attr(test, ts(optional = nullable))]
    pub server: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct CloudLoginStarted {
    pub server: String,
    pub verify_url: String,
    pub user_code: String,
    pub poll_token: String,
    pub expires_in_secs: u64,
    pub poll_interval_ms: u64,
}

#[derive(Debug, Clone, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct CloudLoginPoll {
    pub server: String,
    pub poll_token: String,
}

#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct CloudLoginStatus {
    pub status: String,
    pub username: Option<String>,
    pub email: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct CloudProject {
    pub id: i64,
    pub name: String,
    pub org_id: i64,
    pub org_name: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct CloudStatus {
    pub logged_in: bool,
    pub server: Option<String>,
    pub default_server: String,
    pub username: Option<String>,
    pub email: Option<String>,
    pub projects: Vec<CloudProject>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct ShareProfile {
    pub profile: String,
    pub project_id: i64,
    #[serde(default)]
    #[cfg_attr(test, ts(optional = nullable))]
    pub name: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct SharedProfile {
    pub url: String,
    pub session_id: i64,
    pub capture_id: i64,
    pub project_name: String,
    pub byte_len: i64,
    pub redacted_count: u64,
}

#[cfg(test)]
mod tests {
    use ts_rs::{Config, TS};

    use super::*;

    const GENERATED: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/web/src/protocol.gen.ts");

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
        Advance::decl,
        StepKind::decl,
        SetFocus::decl,
        Focus::decl,
        ThreadAt::decl,
        SourcePath::decl,
        AddBreakpoint::decl,
        EditBreakpoint::decl,
        BreakpointRef::decl,
        MemberRef::decl,
        Input::decl,
        ServerMessage::decl,
        Hello::decl,
        State::decl,
        Group::decl,
        Member::decl,
        Joined::decl,
        MemberState::decl,
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
        SymbolName::decl,
        Resolved::decl,
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
        ProfileDuring::decl,
        ProfileTransform::decl,
        TransformOp::decl,
        ProfileGranularity::decl,
        ProfileLook::decl,
        ProfileTimelineQuery::decl,
        ProfileSummary::decl,
        ProfileInspection::decl,
        ProfileCommand::decl,
        ProfileProcess::decl,
        ProfileModule::decl,
        ProfileTable::decl,
        ProfileFile::decl,
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
        ProfileWait::decl,
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
        ProfileCodeLine::decl,
        ProfileToken::decl,
        ProfileInstruction::decl,
        ProfileBase::decl,
        ProfileZoneScope::decl,
        ProfileBaseWeight::decl,
        ProfilePlot::decl,
        ProfileMarkKind::decl,
        ProfileZoneStatsQuery::decl,
        ProfileZoneStats::decl,
        SyscallQuery::decl,
        SyscallGroup::decl,
        SyscallRowKey::decl,
        SyscallSummaryQuery::decl,
        SyscallSummary::decl,
        SyscallLosses::decl,
        SyscallRow::decl,
        SyscallCount::decl,
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
        SyscallFiles::decl,
        SyscallFile::decl,
        SyscallNetwork::decl,
        SyscallEndpoint::decl,
        SyscallProcesses::decl,
        SyscallsLevel::decl,
        SyscallsEngine::decl,
        SyscallInFlightQuery::decl,
        SyscallInFlight::decl,
        SyscallBlocked::decl,
        SyscallProcess::decl,
        ProfileZoneAt::decl,
        ProfileLabelValue::decl,
        RecordProfile::decl,
        RecordPreset::decl,
        RecordStarted::decl,
        CloudLoginStart::decl,
        CloudLoginStarted::decl,
        CloudLoginPoll::decl,
        CloudLoginStatus::decl,
        CloudProject::decl,
        CloudStatus::decl,
        ShareProfile::decl,
        SharedProfile::decl,
    ];

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

    #[test]
    fn task_numbers_round_trip_exactly_above_two_to_the_fifty_third() {
        let key = TaskKey {
            runtime: 3,
            number: (1 << 62) + 1_884_211,
        };
        let text = serde_json::to_string(&key).expect("serialize");
        assert_eq!(text, r#"{"runtime":3,"number":"4611686018429272115"}"#);
        assert_eq!(serde_json::from_str::<TaskKey>(&text).expect("parse"), key);
        let neighbour = TaskKey {
            number: key.number + 1,
            ..key
        };
        assert_ne!(serde_json::to_string(&neighbour).expect("serialize"), text);
        for wrong in [
            r#"{"runtime":3,"number":4611686018429272115}"#,
            r#"{"runtime":3,"number":"+1"}"#,
            r#"{"runtime":3,"number":""}"#,
            r#"{"runtime":3,"number":"18446744073709551616"}"#,
        ] {
            assert!(serde_json::from_str::<TaskKey>(wrong).is_err(), "{wrong}");
        }
    }

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
}
