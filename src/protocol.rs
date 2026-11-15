use std::{ffi::OsString, fmt, path::PathBuf, process::Stdio, sync::Arc};

use tokio::sync::oneshot;

use crate::model::numeric_id;

use crate::{
    AddressDescription, Backtrace, BreakpointLocation, CodeInstanceId, DereferenceReference,
    DereferencedValue, ExecutionContext, ExecutionLocation, GlobalVariablePage,
    GlobalVariableReference, LineNumber, LoadedModule, LoadedModuleSnapshot, RegisterSnapshot,
    Result, StackFrame, StackFrameId, ThreadId, ValueChildPage, ValueChildrenReference,
    VariableSnapshot, VirtualAddress,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VariableQuery {
    All,

    Name(String),

    Global(GlobalVariableReference),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GlobalVariableQuery {
    pub filter: Option<String>,

    pub offset: u64,

    pub limit: u32,
}

impl Default for GlobalVariableQuery {
    fn default() -> Self {
        Self {
            filter: None,
            offset: 0,
            limit: 100,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValueChildQuery {
    pub offset: u64,

    pub limit: u32,
}

impl Default for ValueChildQuery {
    fn default() -> Self {
        Self {
            offset: 0,
            limit: 32,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ViewCandidate {
    pub view: Arc<crate::ViewName>,
    pub rejection: Option<Arc<str>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TypeViews {
    pub type_info: crate::TypeInfo,

    pub module: Arc<std::path::Path>,
    pub candidates: Arc<[ViewCandidate]>,

    pub visualizers: Arc<[VisualizerBinding]>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VisualizerBinding {
    pub view: Arc<crate::ViewName>,

    pub line: u32,

    pub name: Arc<str>,

    pub origin: Option<Arc<str>>,

    pub inputs: Arc<[Arc<str>]>,

    pub rejection: Option<Arc<str>>,
}

impl TypeViews {
    #[must_use]
    pub fn presented_by(&self) -> Option<&Arc<crate::ViewName>> {
        self.candidates
            .iter()
            .find(|candidate| candidate.rejection.is_none() && !candidate.view.extend)
            .map(|candidate| &candidate.view)
    }

    #[must_use]
    pub fn extended_by(&self) -> Option<&Arc<crate::ViewName>> {
        if self.presented_by().is_some() {
            return None;
        }

        self.candidates
            .iter()
            .find(|candidate| candidate.rejection.is_none())
            .map(|candidate| &candidate.view)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ViewCheck {
    pub types: Arc<[TypeViews]>,
    pub unused: Arc<[Arc<crate::ViewName>]>,

    pub kernels: Arc<[KernelSource]>,

    pub renderers: Arc<[Arc<Renderer>]>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KernelSource {
    pub name: Arc<str>,

    pub origin: Arc<str>,

    pub source: Arc<str>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Renderer {
    pub name: Arc<str>,

    pub origin: Arc<str>,

    pub source: Arc<str>,

    pub digest: Arc<str>,
}

impl Renderer {
    #[must_use]
    pub fn new(name: &str, origin: &str, source: &str) -> Self {
        let digest = twox_hash::XxHash3_128::oneshot(source.as_bytes());
        Self {
            name: name.into(),
            origin: origin.into(),
            source: source.into(),
            digest: format!("{digest:032x}").into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VisualizerInputs {
    pub stop_id: StopId,
    pub inputs: Arc<[VisualizerInput]>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VisualizerInput {
    pub name: Arc<str>,
    pub value: VisualizerValue,

    pub path: Option<Arc<str>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VisualizerValue {
    Value(Box<crate::InspectedValue>),

    Bytes(Arc<[u8]>),

    Text(Arc<str>),

    Problem(Arc<str>),
}

pub const MAX_DRAWING_BYTES: u64 = 64 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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

impl NumberKind {
    #[must_use]
    pub const fn size(self) -> u64 {
        match self {
            Self::I8 | Self::U8 => 1,
            Self::I16 | Self::U16 => 2,
            Self::I32 | Self::U32 | Self::F32 => 4,
            Self::I64 | Self::U64 | Self::F64 => 8,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Numbers {
    pub stop_id: StopId,
    pub kind: NumberKind,
    pub count: u64,
    pub bytes: Arc<[u8]>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ViewExplanation {
    pub type_info: Option<crate::TypeInfo>,

    pub enabled: bool,

    pub candidates: Arc<[ViewCandidate]>,

    pub visualizers: Arc<[VisualizerBinding]>,

    pub presentation: Option<Arc<crate::Presentation>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum BreakpointSpec {
    Function(String),

    Source { path: PathBuf, line: LineNumber },

    FileFunction { path: PathBuf, function: String },

    Address(VirtualAddress),
}

impl fmt::Display for BreakpointSpec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Function(function) => f.write_str(function),
            Self::Source { path, line } => write!(f, "{}:{line}", path.display()),
            Self::FileFunction { path, function } => write!(f, "{}:{function}", path.display()),
            Self::Address(address) => address.fmt(f),
        }
    }
}

numeric_id!(
    BreakpointId,
    "Identifies one logical user breakpoint within a debug session."
);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedBreakpointLocation {
    pub location: BreakpointLocation,

    pub code_instances: Arc<[CodeInstanceId]>,

    pub library: Option<crate::ModuleId>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum HitComparison {
    Equal,

    NotEqual,

    Less,

    LessOrEqual,

    Greater,

    GreaterOrEqual,

    Multiple,
}

impl HitComparison {
    const fn operator(self) -> &'static str {
        match self {
            Self::Equal => "==",
            Self::NotEqual => "!=",
            Self::Less => "<",
            Self::LessOrEqual => "<=",
            Self::Greater => ">",
            Self::GreaterOrEqual => ">=",
            Self::Multiple => "%",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct HitCondition {
    comparison: HitComparison,
    count: u64,
}

impl HitCondition {
    pub fn new(comparison: HitComparison, count: u64) -> Result<Self> {
        let condition = Self { comparison, count };
        let satisfiable = match comparison {
            HitComparison::Equal | HitComparison::LessOrEqual | HitComparison::Multiple => {
                count != 0
            }
            HitComparison::Less => count > 1,
            HitComparison::Greater => count != u64::MAX,
            HitComparison::NotEqual | HitComparison::GreaterOrEqual => true,
        };

        if satisfiable {
            Ok(condition)
        } else {
            Err(crate::Error::InvalidHitCondition(format!(
                "no hit can satisfy {condition}"
            )))
        }
    }

    #[must_use]
    pub const fn comparison(self) -> HitComparison {
        self.comparison
    }

    #[must_use]
    pub const fn count(self) -> u64 {
        self.count
    }

    #[must_use]
    pub const fn is_met(self, hit: u64) -> bool {
        match self.comparison {
            HitComparison::Equal => hit == self.count,
            HitComparison::NotEqual => hit != self.count,
            HitComparison::Less => hit < self.count,
            HitComparison::LessOrEqual => hit <= self.count,
            HitComparison::Greater => hit > self.count,
            HitComparison::GreaterOrEqual => hit >= self.count,
            HitComparison::Multiple => hit.is_multiple_of(self.count),
        }
    }

    #[must_use]
    pub const fn may_stop_after(self, hits: u64) -> bool {
        match self.comparison {
            HitComparison::Equal | HitComparison::LessOrEqual => hits < self.count,
            HitComparison::Less => hits.saturating_add(1) < self.count,
            HitComparison::Greater | HitComparison::GreaterOrEqual => hits < u64::MAX,
            HitComparison::NotEqual | HitComparison::Multiple => true,
        }
    }
}

impl fmt::Display for HitCondition {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}{}", self.comparison.operator(), self.count)
    }
}

impl std::str::FromStr for HitCondition {
    type Err = crate::Error;

    fn from_str(text: &str) -> Result<Self> {
        let invalid = || {
            let trimmed = text.trim();
            crate::Error::InvalidHitCondition(
                if !trimmed.is_empty() && trimmed.bytes().all(|byte| byte.is_ascii_digit()) {
                    format!(
                        "a bare count is ambiguous; write =={trimmed} to stop only at that hit \
                         or >={trimmed} to stop at it and every later hit"
                    )
                } else {
                    format!(
                        "'{text}' is not an operator (==, !=, <, <=, >, >=, %) followed by a \
                         decimal count"
                    )
                },
            )
        };

        let (comparison, digits) = [
            HitComparison::Equal,
            HitComparison::NotEqual,
            HitComparison::LessOrEqual,
            HitComparison::GreaterOrEqual,
            HitComparison::Less,
            HitComparison::Greater,
            HitComparison::Multiple,
        ]
        .into_iter()
        .find_map(|comparison| {
            text.trim()
                .strip_prefix(comparison.operator())
                .map(|digits| (comparison, digits.trim_start()))
        })
        .ok_or_else(invalid)?;
        if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
            return Err(invalid());
        }

        let count = digits.parse().map_err(|_| invalid())?;
        Self::new(comparison, count)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Breakpoint {
    pub id: BreakpointId,

    pub spec: BreakpointSpec,

    pub locations: Arc<[ResolvedBreakpointLocation]>,

    pub pending_locations: Arc<[PendingBreakpointLocation]>,

    pub hit_condition: Option<HitCondition>,

    pub condition: Option<crate::Condition>,

    pub log_message: Option<crate::LogMessage>,

    pub hit_count: u64,

    pub enabled: bool,

    pub temporary: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingBreakpointLocation {
    pub location: BreakpointLocation,
    pub reason: Arc<str>,
}

impl Breakpoint {
    pub fn ready_locations(&self) -> impl Iterator<Item = &ResolvedBreakpointLocation> {
        self.locations.iter().filter(move |resolved| {
            !self
                .pending_locations
                .iter()
                .any(|pending| pending.location == resolved.location)
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BreakpointOptions {
    pub hit_condition: Option<HitCondition>,

    pub condition: Option<crate::Condition>,

    pub log_message: Option<crate::LogMessage>,

    pub pending: bool,

    pub enabled: bool,

    pub temporary: bool,
}

impl Default for BreakpointOptions {
    fn default() -> Self {
        Self {
            hit_condition: None,
            condition: None,
            log_message: None,
            pending: false,
            enabled: true,
            temporary: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(
    clippy::large_enum_variant,
    reason = "a message has few parts, each made once per logged hit"
)]
pub enum LogPart {
    Text(Arc<str>),

    Value {
        expression: crate::Expression,

        type_info: Option<crate::TypeInfo>,

        state: crate::VariableState,
    },

    Error {
        expression: crate::Expression,

        error: Arc<str>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BreakpointHit {
    pub breakpoint: BreakpointId,

    pub hit_count: u64,
}

numeric_id!(
    WatchpointId,
    "Identifies one watchpoint within a debug session."
);

numeric_id!(
    CatchpointId,
    "Identifies one catchpoint within a debug session."
);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConditionOwner {
    Breakpoint(BreakpointId),
    Watchpoint(WatchpointId),
    Catchpoint(CatchpointId),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CatchPhase {
    Entry,

    Exit,

    #[default]
    Both,
}

impl CatchPhase {
    #[must_use]
    pub const fn stops(self, exit: bool) -> bool {
        match self {
            Self::Entry => !exit,
            Self::Exit => exit,
            Self::Both => true,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Catchpoint {
    pub id: CatchpointId,

    pub calls: Arc<[crate::syscalls::SyscallId]>,
    pub phase: CatchPhase,

    pub condition: Option<crate::Condition>,

    pub hit_count: u64,
}

impl Catchpoint {
    #[must_use]
    pub fn catches(&self, call: crate::syscalls::SyscallId, exit: bool) -> bool {
        self.phase.stops(exit) && (self.calls.is_empty() || self.calls.contains(&call))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyscallStop {
    pub call: crate::syscalls::SyscallId,

    pub args: [u64; 6],

    pub returned: Option<i64>,

    pub path: Option<Arc<str>>,

    pub captured: Arc<[crate::syscalls::Capture]>,

    pub catchpoints: Arc<[CatchpointId]>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum WatchAccess {
    Change,

    Write,

    Read,

    ReadWrite,
}

impl fmt::Display for WatchAccess {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Change => "change",
            Self::Write => "write",
            Self::Read => "read",
            Self::ReadWrite => "read/write",
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WatchpointCapabilities {
    pub slots: u32,

    pub max_slot_bytes: u64,

    pub access: Arc<[WatchAccess]>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WatchScope {
    Location,

    Static {
        module: crate::ModuleId,
    },

    ThreadLocal {
        thread: ThreadId,
    },

    Frame {
        thread: ThreadId,

        activation: VirtualAddress,
    },

    Task {
        task: crate::TaskId,

        activation: u64,
    },

    Interpreted {
        thread: ThreadId,

        runtime: crate::RuntimeId,

        frame: VirtualAddress,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FrameScopeEvidence {
    pub module: crate::ModuleId,
    pub image: crate::ModuleImageId,
    pub function: CodeInstanceId,
    pub ranges: Arc<[crate::AddressRange<crate::ImageAddress>]>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WatchTarget {
    pub(crate) stop_id: StopId,
    pub(crate) expression: crate::Expression,
    pub(crate) address: VirtualAddress,
    pub(crate) byte_size: u64,
    pub(crate) type_info: Option<crate::TypeInfo>,
    pub(crate) scope: WatchScope,
    pub(crate) frame: Option<FrameScopeEvidence>,

    pub(crate) code: Option<VirtualAddress>,
}

impl WatchTarget {
    #[must_use]
    pub const fn expression(&self) -> &crate::Expression {
        &self.expression
    }

    #[must_use]
    pub const fn address(&self) -> VirtualAddress {
        self.address
    }

    #[must_use]
    pub const fn byte_size(&self) -> u64 {
        self.byte_size
    }

    #[must_use]
    pub const fn scope(&self) -> &WatchScope {
        &self.scope
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WatchpointSpec {
    Target(Box<WatchTarget>),

    Location {
        address: VirtualAddress,

        byte_size: u64,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Watchpoint {
    pub id: WatchpointId,

    pub access: WatchAccess,

    pub expression: Option<crate::Expression>,

    pub address: VirtualAddress,

    pub byte_size: u64,

    pub type_info: Option<crate::TypeInfo>,

    pub scope: WatchScope,

    pub coverage: Arc<[crate::AddressRange<VirtualAddress>]>,

    pub hit_condition: Option<HitCondition>,

    pub condition: Option<crate::Condition>,

    pub hit_count: u64,

    pub enabled: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WatchpointOptions {
    pub hit_condition: Option<HitCondition>,

    pub condition: Option<crate::Condition>,

    pub enabled: bool,
}

impl Default for WatchpointOptions {
    fn default() -> Self {
        Self {
            hit_condition: None,
            condition: None,
            enabled: true,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WatchpointHit {
    pub watchpoint: WatchpointId,

    pub thread: ThreadId,

    pub hit_count: u64,

    pub previous: Option<Arc<[u8]>>,

    pub current: Option<Arc<[u8]>>,

    pub previous_value: Option<Arc<str>>,
    pub current_value: Option<Arc<str>>,
}

impl WatchpointHit {
    #[must_use]
    pub fn changed(&self) -> bool {
        self.previous != self.current
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WatchpointInvalidation {
    ScopeExited,

    OwnerThreadExited,

    ModuleUnloaded,

    StackMoved,

    ObjectMoved,

    FrameReplaced,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvalidatedWatchpoint {
    pub watchpoint: Watchpoint,

    pub reason: WatchpointInvalidation,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PresentedFrame {
    Physical,

    Inline(CodeInstanceId),

    Ambiguous(Arc<[CodeInstanceId]>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FramePresentation {
    pub instruction: VirtualAddress,

    pub frame: PresentedFrame,

    pub hidden_inline_frames: u32,
}

numeric_id!(
    ProcessId,
    "Identifies an inferior process; local attach accepts an operating-system process ID."
);

#[derive(Debug, Default)]
pub struct LaunchOptions {
    pub arguments: Vec<OsString>,

    pub environment: Vec<(OsString, Option<OsString>)>,

    pub working_directory: Option<PathBuf>,

    pub stdin: Option<Stdio>,

    pub stdout: Option<Stdio>,

    pub stderr: Option<Stdio>,

    pub stop_at_entry: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FollowMode {
    None,

    #[default]
    Auto,

    All,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct FollowPolicy {
    pub mode: FollowMode,

    pub programs: Vec<String>,

    pub stop: StopScope,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum StopScope {
    #[default]
    Group,

    Process,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DebugFileOptions {
    pub directories: Vec<PathBuf>,

    pub debuginfod: bool,

    pub debuginfod_urls: Option<Vec<String>>,

    pub debuginfod_cache: Option<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CoreDumpOptions {
    pub core: PathBuf,

    pub executable: Option<PathBuf>,

    pub sysroot: Option<PathBuf>,

    pub module_paths: Vec<PathBuf>,

    pub allow_module_mismatch: bool,

    pub debug_files: DebugFileOptions,
}

impl CoreDumpOptions {
    pub fn new(core: impl Into<PathBuf>) -> Self {
        Self {
            core: core.into(),
            executable: None,
            sysroot: None,
            module_paths: Vec::new(),
            allow_module_mismatch: false,
            debug_files: DebugFileOptions::default(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModuleIdentity {
    BuildId,

    SavedContent {
        /// The number of saved bytes compared.
        compared_bytes: u64,
    },

    Mismatched {
        detail: Arc<str>,
    },

    Unverified,

    DumpedMemory,
}

impl ModuleIdentity {
    #[must_use]
    pub const fn is_verified(&self) -> bool {
        matches!(
            self,
            Self::BuildId | Self::SavedContent { .. } | Self::DumpedMemory
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CoreModuleState {
    Loaded {
        module: crate::LoadedModuleRecord,

        identity: ModuleIdentity,
    },

    Missing,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CoreModule {
    pub recorded_path: Arc<PathBuf>,

    pub start: VirtualAddress,

    pub build_id: Option<Arc<[u8]>>,

    pub state: CoreModuleState,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CoreDumpInfo {
    pub path: Arc<PathBuf>,

    pub process_id: ProcessId,

    pub process_name: Arc<str>,

    pub arguments: Arc<str>,

    pub exception: Option<ExceptionInfo>,

    pub modules: Arc<[CoreModule]>,
}

numeric_id!(
    StopId,
    "Identifies an externally observable stopped snapshot."
);

numeric_id!(
    ExecutionId,
    "Identifies one accepted execution-control operation."
);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResumeScope {
    Process(ProcessId),

    Thread(ThreadId),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StepKind {
    Instruction,

    OverInstruction,

    IntoSource,

    OverSource,

    Out,

    Advance,

    IntoNewTask,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExceptionDisposition {
    Pass,

    Suppress,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SignalPolicy {
    pub stop: bool,

    pub print: bool,

    pub pass: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExceptionFilter {
    pub id: &'static str,

    pub language: &'static str,
    pub kind: LanguageExceptionKind,

    pub label: &'static str,

    pub description: &'static str,

    pub default: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub struct ExceptionStops {
    stopping: u64,
}

const _: () = assert!(crate::runtime_model::EXCEPTION_FILTERS.len() <= 64);

impl Default for ExceptionStops {
    fn default() -> Self {
        Self::filters()
            .iter()
            .enumerate()
            .filter(|(_, filter)| filter.default)
            .fold(Self::NONE, |stops, (index, _)| Self {
                stopping: stops.stopping | Self::bit_at(index),
            })
    }
}

impl fmt::Debug for ExceptionStops {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_set()
            .entries(
                Self::filters()
                    .iter()
                    .filter(|filter| self.stops(filter.id))
                    .map(|filter| filter.id),
            )
            .finish()
    }
}

impl ExceptionStops {
    pub const NONE: Self = Self { stopping: 0 };

    #[must_use]
    pub const fn filters() -> &'static [ExceptionFilter] {
        &crate::runtime_model::EXCEPTION_FILTERS
    }

    fn bit(filter: &str) -> Option<u64> {
        Self::filters()
            .iter()
            .position(|known| known.id == filter)
            .map(Self::bit_at)
    }

    const fn bit_at(index: usize) -> u64 {
        1 << index
    }

    #[must_use]
    pub fn stops(self, filter: &str) -> bool {
        Self::bit(filter).is_some_and(|bit| self.stopping & bit != 0)
    }

    #[must_use]
    pub fn with(self, filter: &str, stops: bool) -> Option<Self> {
        let bit = Self::bit(filter)?;
        Some(Self {
            stopping: if stops {
                self.stopping | bit
            } else {
                self.stopping & !bit
            },
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum LanguageExceptionKind {
    Raised,

    Unhandled,

    Fatal,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LanguageException {
    pub kind: LanguageExceptionKind,

    pub message: Arc<str>,

    pub value: Option<Arc<str>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExceptionInfo {
    pub code: u64,

    pub description: Arc<str>,
}

impl ExceptionInfo {
    pub fn new(code: u64, description: impl Into<Arc<str>>) -> Self {
        Self {
            code,
            description: description.into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExitStatus {
    Code(i64),

    Terminated(ExceptionInfo),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StepTarget {
    pub call: VirtualAddress,

    pub target: Option<VirtualAddress>,

    pub callee: Option<Arc<str>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskEnding {
    Finished,

    Cancelled,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StopReason {
    /// The debugger established an initial coherent stop after attaching.
    Attach,

    /// A launch requested with [`LaunchOptions::stop_at_entry`] stopped at
    Entry,

    Breakpoint {
        address: Option<VirtualAddress>,

        hits: Arc<[BreakpointHit]>,
    },

    Watchpoint {
        /// Every watchpoint this thread's access reported.
        hits: Arc<[WatchpointHit]>,
    },

    WatchpointInvalidated {
        invalidated: Arc<[InvalidatedWatchpoint]>,
    },

    WatchpointArmFailed {
        thread_id: ThreadId,

        description: Arc<str>,
    },

    /// A stepping operation completed.
    Step { kind: StepKind },

    StepIncomplete {
        kind: StepKind,

        description: Arc<str>,
    },

    TaskEnded {
        kind: StepKind,
        task: crate::TaskId,
        ending: TaskEnding,
    },

    /// timeout drops a future it no longer awaits, and the step went on in
    FutureDropped { kind: StepKind },

    /// Execution stopped at the user's request.
    Pause,

    /// A thread was moved to resume elsewhere, by a jump or by assigning
    Jump,

    /// as its thread, the only one, ran to make it.
    Assignment,

    /// Execution stopped because of an exception.
    Exception(ExceptionInfo),

    /// a fatal error. The frame the runtime blames is selected.
    LanguageException(LanguageException),

    RuntimeEvent {
        /// Why the event could not be read.
        description: Arc<str>,
    },

    ProgramBreakpoint {
        /// whose frame is selected.
        address: Option<VirtualAddress>,
    },

    /// catches.
    Syscall(Box<SyscallStop>),

    Exec {
        /// cannot be inspected or resumed.
        followed: bool,
    },

    ThreadExited {
        thread_id: ThreadId,

        status: ExitStatus,
    },

    Unclassifiable {
        /// Diagnostic details retained by the platform edge.
        description: Arc<str>,
    },

    /// The inferior exited.
    Exited(ExitStatus),

    CoreDump {
        /// The signal that terminated the process, when the dump recorded one.
        exception: Option<ExceptionInfo>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ThreadState {
    Running,

    Stopped {
        /// The thread's own stop reason, when it produced an interesting event.
        reason: Option<StopReason>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThreadSnapshot {
    pub id: ThreadId,

    pub state: ThreadState,

    pub name: Option<Arc<str>>,

    pub activity: Option<crate::ThreadActivity>,

    pub labels: Arc<[crate::ThreadLabel]>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InferiorState {
    /// No program has been launched or attached to.
    NotStarted,

    Running {
        process_id: ProcessId,

        execution_id: Option<ExecutionId>,
    },

    Stopped {
        process_id: ProcessId,

        stop_id: StopId,

        thread_id: ThreadId,

        reason: StopReason,
    },

    Exited {
        process_id: ProcessId,
        status: ExitStatus,
    },

    /// The debugger let the last program go, or lost it, as
    Detached { process_id: ProcessId },
}

impl InferiorState {
    #[must_use]
    pub const fn live_process(&self) -> Option<ProcessId> {
        match self {
            Self::Running { process_id, .. } | Self::Stopped { process_id, .. } => {
                Some(*process_id)
            }
            Self::NotStarted | Self::Exited { .. } | Self::Detached { .. } => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StateSnapshot {
    pub revision: u64,

    pub inferior: InferiorState,

    pub stop_id: Option<StopId>,

    pub selected: Option<ExecutionContext>,

    pub selected_frame: Option<StackFrameId>,

    pub threads: Arc<[ThreadSnapshot]>,

    pub presentation: Option<FramePresentation>,

    pub breakpoints: Arc<[Breakpoint]>,

    pub watchpoints: Arc<[Watchpoint]>,

    pub catchpoints: Arc<[Catchpoint]>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NamedProcess {
    pub process_id: ProcessId,

    pub command_line: Arc<str>,
    pub matched: NameMatch,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NameMatch {
    Command,

    Program,

    Title,

    Script,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DebuggerEvent {
    /// Debugger state changed in a way no more specific event describes.
    StateChanged { revision: u64 },

    InferiorLaunched {
        revision: u64,
        process_id: ProcessId,
        execution_id: ExecutionId,
    },

    InferiorAttached {
        revision: u64,
        process_id: ProcessId,
    },

    InferiorContinued {
        revision: u64,
        process_id: ProcessId,
        execution_id: ExecutionId,
        resumed: ResumeScope,
    },

    InferiorStopped {
        revision: u64,
        process_id: ProcessId,

        execution_id: Option<ExecutionId>,
        stop_id: StopId,

        thread_id: ThreadId,
        reason: StopReason,
    },

    ThreadStarted {
        revision: u64,
        process_id: ProcessId,
        thread_id: ThreadId,
    },

    ThreadExited {
        revision: u64,
        process_id: ProcessId,
        thread_id: ThreadId,
        status: ExitStatus,
    },

    ModuleLoaded {
        revision: u64,
        module: crate::LoadedModuleRecord,
    },

    ModuleUnloaded {
        revision: u64,
        module: crate::LoadedModuleRecord,
    },

    ProgramChanged {
        revision: u64,

        process_id: Option<ProcessId>,
        previous: crate::LoadedModuleRecord,
        program: crate::LoadedModuleRecord,
    },

    InferiorExited {
        revision: u64,
        process_id: ProcessId,

        execution_id: Option<ExecutionId>,
        status: ExitStatus,
    },

    InferiorDetached {
        revision: u64,
        process_id: ProcessId,
    },

    ChildReleased {
        revision: u64,
        process_id: ProcessId,
        child: ProcessId,
    },

    SignalReceived {
        revision: u64,
        process_id: ProcessId,
        thread_id: ThreadId,
        exception: ExceptionInfo,
    },

    LogMessage {
        revision: u64,
        process_id: ProcessId,
        thread_id: ThreadId,
        breakpoint: BreakpointId,
        parts: Arc<[LogPart]>,
    },

    ConditionFailed {
        revision: u64,
        process_id: ProcessId,
        thread_id: ThreadId,
        owner: ConditionOwner,
        error: Arc<str>,
    },

    /// The set of logical breakpoints changed.
    BreakpointsChanged { revision: u64 },

    /// The set of armed watchpoints changed.
    WatchpointsChanged { revision: u64 },

    /// The set of catchpoints changed.
    CatchpointsChanged { revision: u64 },

    WatchpointsInvalidated {
        revision: u64,
        invalidated: Arc<[InvalidatedWatchpoint]>,
    },
}

impl DebuggerEvent {
    #[must_use]
    pub const fn revision(&self) -> u64 {
        match self {
            Self::StateChanged { revision }
            | Self::InferiorLaunched { revision, .. }
            | Self::InferiorAttached { revision, .. }
            | Self::InferiorContinued { revision, .. }
            | Self::InferiorStopped { revision, .. }
            | Self::ThreadStarted { revision, .. }
            | Self::ThreadExited { revision, .. }
            | Self::ChildReleased { revision, .. }
            | Self::ModuleLoaded { revision, .. }
            | Self::ModuleUnloaded { revision, .. }
            | Self::ProgramChanged { revision, .. }
            | Self::InferiorExited { revision, .. }
            | Self::InferiorDetached { revision, .. }
            | Self::SignalReceived { revision, .. }
            | Self::LogMessage { revision, .. }
            | Self::ConditionFailed { revision, .. }
            | Self::BreakpointsChanged { revision }
            | Self::WatchpointsChanged { revision }
            | Self::CatchpointsChanged { revision }
            | Self::WatchpointsInvalidated { revision, .. } => *revision,
        }
    }
}

pub type Reply<T> = oneshot::Sender<Result<T>>;

pub enum Request {
    AddBreakpoint {
        spec: BreakpointSpec,
        options: Box<BreakpointOptions>,
        reply: Reply<Breakpoint>,
    },
    SetBreakpointHitCondition {
        id: BreakpointId,
        hit_condition: Option<HitCondition>,
        reply: Reply<Breakpoint>,
    },
    SetBreakpointCondition {
        id: BreakpointId,
        condition: Option<crate::Condition>,
        reply: Reply<Breakpoint>,
    },
    SetBreakpointLogMessage {
        id: BreakpointId,
        log_message: Option<crate::LogMessage>,
        reply: Reply<Breakpoint>,
    },
    SetBreakpointEnabled {
        id: BreakpointId,
        enabled: bool,
        reply: Reply<Breakpoint>,
    },
    RemoveBreakpoint {
        id: BreakpointId,
        reply: Reply<Breakpoint>,
    },
    RemoveAllBreakpoints {
        reply: Reply<Arc<[Breakpoint]>>,
    },
    ResolveWatchTarget {
        expression: crate::Expression,
        stop_id: StopId,
        context: ExecutionContext,
        frame: StackFrameId,
        reply: Reply<WatchTarget>,
    },
    AddWatchpoint {
        spec: WatchpointSpec,
        access: WatchAccess,
        options: WatchpointOptions,
        reply: Reply<Watchpoint>,
    },
    SetWatchpointHitCondition {
        id: WatchpointId,
        hit_condition: Option<HitCondition>,
        reply: Reply<Watchpoint>,
    },
    SetWatchpointCondition {
        id: WatchpointId,
        condition: Option<crate::Condition>,
        reply: Reply<Watchpoint>,
    },
    SetWatchpointEnabled {
        id: WatchpointId,
        enabled: bool,
        reply: Reply<Watchpoint>,
    },
    RemoveWatchpoint {
        id: WatchpointId,
        reply: Reply<Watchpoint>,
    },
    RemoveAllWatchpoints {
        reply: Reply<Arc<[Watchpoint]>>,
    },
    AddCatchpoint {
        calls: Arc<[crate::syscalls::SyscallId]>,
        phase: CatchPhase,
        condition: Option<crate::Condition>,
        reply: Reply<Catchpoint>,
    },
    RemoveCatchpoint {
        id: CatchpointId,
        reply: Reply<Catchpoint>,
    },
    Launch {
        options: Box<LaunchOptions>,
        reply: Reply<ExecutionId>,
    },
    Attach {
        process_id: ProcessId,

        observe: bool,
        reply: Reply<StopId>,
    },
    LaunchByExec {
        process_id: ProcessId,
        stop_at_entry: bool,
        release: Box<dyn FnOnce() + Send>,
        reply: Reply<ExecutionId>,
    },
    Continue {
        process_id: ProcessId,
        stop_id: StopId,
        scope: ResumeScope,
        exception: ExceptionDisposition,
        reply: Reply<ExecutionId>,
    },
    Step {
        process_id: ProcessId,
        stop_id: StopId,
        context: ExecutionContext,

        frame: StackFrameId,
        kind: StepKind,

        call: Option<VirtualAddress>,

        scope: ResumeScope,
        exception: ExceptionDisposition,
        reply: Reply<ExecutionId>,
    },

    Advance {
        process_id: ProcessId,
        stop_id: StopId,
        context: ExecutionContext,
        frame: StackFrameId,
        spec: BreakpointSpec,
        scope: ResumeScope,
        exception: ExceptionDisposition,
        reply: Reply<ExecutionId>,
    },

    Jump {
        process_id: ProcessId,
        stop_id: StopId,
        context: ExecutionContext,
        spec: BreakpointSpec,
        reply: Reply<ExecutionId>,
    },
    Pause {
        process_id: ProcessId,
        reply: Reply<ExecutionId>,
    },
    ReadMemory {
        process_id: ProcessId,
        stop_id: StopId,
        address: VirtualAddress,
        byte_count: u64,
        reply: Reply<crate::MemoryRead>,
    },
    WriteMemory {
        process_id: ProcessId,
        stop_id: StopId,
        address: VirtualAddress,
        bytes: Arc<[u8]>,
        reply: Reply<u64>,
    },
    LoadedModule {
        reply: Reply<LoadedModule>,
    },
    LoadedModules {
        reply: Reply<LoadedModuleSnapshot>,
    },
    ModuleImage {
        module: crate::ModuleId,
        reply: Reply<Arc<crate::ModuleImage>>,
    },
    Disassemble {
        query: crate::DisassemblyQuery,
        stop_id: StopId,
        context: ExecutionContext,
        reply: Reply<crate::Disassembly>,
    },
    DescribeAddress {
        stop_id: StopId,
        address: VirtualAddress,
        reply: Reply<AddressDescription>,
    },
    StoppedLocation {
        stop_id: StopId,
        context: ExecutionContext,
        frame: StackFrameId,
        reply: Reply<ExecutionLocation>,
    },
    Snapshot {
        reply: Reply<StateSnapshot>,
    },

    Tasks {
        stop_id: StopId,
        from: Option<crate::TaskCursor>,
        limit: usize,

        program_only: bool,
        reply: Reply<crate::TaskPage>,
    },

    StoppedSelection {
        reply: Reply<crate::StoppedSelection>,
    },
    Backtrace {
        stop_id: StopId,
        context: ExecutionContext,
        reply: Reply<Backtrace>,
    },
    Registers {
        stop_id: StopId,
        context: ExecutionContext,
        frame: StackFrameId,
        reply: Reply<RegisterSnapshot>,
    },

    StepTargets {
        stop_id: StopId,
        context: ExecutionContext,
        reply: Reply<Arc<[StepTarget]>>,
    },
    Variables {
        query: VariableQuery,
        limits: crate::InspectionLimits,
        stop_id: StopId,
        context: ExecutionContext,
        frame: StackFrameId,
        reply: Reply<VariableSnapshot>,
    },
    Evaluate {
        expression: crate::Expression,
        mode: crate::EvaluationMode,
        limits: crate::InspectionLimits,
        stop_id: StopId,
        context: ExecutionContext,
        frame: StackFrameId,
        reply: Reply<crate::Evaluation>,
    },
    ExpressionType {
        expression: crate::Expression,
        stop_id: StopId,
        context: ExecutionContext,
        frame: StackFrameId,
        reply: Reply<crate::TypeInfo>,
    },
    Dereference {
        reference: Box<DereferenceReference>,
        limits: crate::InspectionLimits,
        reply: Reply<DereferencedValue>,
    },
    ValueChildren {
        reference: Arc<ValueChildrenReference>,
        query: ValueChildQuery,
        limits: crate::InspectionLimits,
        reply: Reply<ValueChildPage>,
    },
    VisualizerInputs {
        reference: Arc<crate::VisualizerReference>,
        limits: crate::InspectionLimits,
        reply: Reply<VisualizerInputs>,
    },

    Numbers {
        reference: Arc<ValueChildrenReference>,
        most_bytes: u64,
        reply: Reply<Option<Numbers>>,
    },
    Renderers {
        reply: Reply<Arc<[Arc<Renderer>]>>,
    },
    Globals {
        query: GlobalVariableQuery,
        reply: Reply<GlobalVariablePage>,
    },

    SetViews {
        views: Arc<crate::view::ViewSet>,
        reply: Reply<()>,
    },

    EnableViews {
        enabled: bool,
        reply: Reply<()>,
    },

    SetStepIntoRuntime {
        enter: bool,
        reply: Reply<bool>,
    },

    SetStepSkips {
        skips: crate::StepSkips,
        reply: Reply<()>,
    },
    ExplainView {
        expression: crate::Expression,
        stop_id: StopId,
        context: ExecutionContext,
        frame: StackFrameId,
        reply: Reply<ViewExplanation>,
    },

    RecordKernels {
        expression: crate::Expression,
        stop_id: StopId,
        context: ExecutionContext,
        frame: StackFrameId,
        reply: Reply<Vec<String>>,
    },

    ExplainType {
        name: String,
        reply: Reply<Vec<TypeViews>>,
    },

    CheckViews {
        reply: Reply<ViewCheck>,
    },
    SelectContext {
        stop_id: StopId,
        context: ExecutionContext,
        reply: Reply<()>,
    },
    SelectFrame {
        stop_id: StopId,
        context: ExecutionContext,
        frame: StackFrameId,
        reply: Reply<StackFrame>,
    },
    SignalPolicy {
        signal: u64,
        reply: Reply<SignalPolicy>,
    },
    Kill {
        reply: Reply<()>,
    },
    Terminate {
        reply: Reply<()>,
    },
    SetSignalPolicy {
        signal: u64,
        policy: SignalPolicy,
        reply: Reply<SignalPolicy>,
    },
    SetExceptionStops {
        stops: ExceptionStops,
        reply: Reply<ExceptionStops>,
    },

    SetExecStops {
        stops: bool,
        reply: Reply<bool>,
    },

    ReleaseOnShutdown {
        reply: Reply<()>,
    },
    Shutdown {
        reply: Reply<()>,
    },
}

impl Request {
    #[expect(clippy::too_many_lines, reason = "one arm per request")]
    pub(crate) fn describe(&self) -> String {
        match self {
            Self::Launch { options, .. } => format!("launch {:?}", options.arguments),
            Self::Attach { process_id, .. } => format!("attach {process_id}"),
            Self::LaunchByExec {
                process_id,
                stop_at_entry,
                ..
            } => format!("launch by exec {process_id}, stop at entry {stop_at_entry}"),
            Self::Continue {
                stop_id,
                scope,
                exception,
                ..
            } => format!("continue {stop_id:?} {scope:?} {exception:?}"),
            Self::Step {
                stop_id,
                context,
                frame,
                kind,
                call,
                scope,
                exception,
                ..
            } => format!(
                "step {kind:?}{} {stop_id:?} {context:?} {frame:?} {scope:?} {exception:?}",
                call.map(|call| format!(" into the call at {call}"))
                    .unwrap_or_default()
            ),
            Self::Advance {
                stop_id,
                context,
                frame,
                spec,
                scope,
                exception,
                ..
            } => format!(
                "advance to {spec:?} {stop_id:?} {context:?} {frame:?} {scope:?} {exception:?}"
            ),
            Self::Jump {
                stop_id,
                context,
                spec,
                ..
            } => format!("jump to {spec:?} {stop_id:?} {context:?}"),
            Self::Pause { process_id, .. } => format!("pause {process_id}"),
            Self::AddBreakpoint { spec, .. } => format!("add breakpoint {spec:?}"),
            Self::SetBreakpointEnabled { id, enabled, .. } => {
                format!("set {id:?} enabled {enabled}")
            }
            Self::RemoveBreakpoint { id, .. } => format!("remove breakpoint {id:?}"),
            Self::SetBreakpointCondition { id, .. } => format!("set condition of {id:?}"),
            Self::SetBreakpointHitCondition { id, .. } => format!("set hit condition of {id:?}"),
            Self::SetBreakpointLogMessage { id, .. } => format!("set log message of {id:?}"),
            Self::AddWatchpoint { spec, access, .. } => {
                format!("add watchpoint {spec:?} {access:?}")
            }
            Self::SetWatchpointCondition { id, .. } => format!("set condition of {id:?}"),
            Self::SetWatchpointHitCondition { id, .. } => format!("set hit condition of {id:?}"),
            Self::SetWatchpointEnabled { id, enabled, .. } => {
                format!("set {id:?} enabled {enabled}")
            }
            Self::RemoveWatchpoint { id, .. } => format!("remove watchpoint {id:?}"),
            Self::WriteMemory {
                stop_id,
                address,
                bytes,
                ..
            } => format!("write {} bytes at {address} {stop_id:?}", bytes.len()),
            Self::SetSignalPolicy { signal, policy, .. } => {
                format!("set signal policy {signal} {policy:?}")
            }
            Self::SetExceptionStops { stops, .. } => format!("set exception stops {stops:?}"),
            Self::SetExecStops { stops, .. } => format!("set exec stops {stops}"),
            Self::ReleaseOnShutdown { .. } => "release on shutdown".to_owned(),
            Self::RemoveAllBreakpoints { .. } => "remove all breakpoints".to_owned(),
            Self::ResolveWatchTarget { .. } => "resolve watch target".to_owned(),
            Self::RemoveAllWatchpoints { .. } => "remove all watchpoints".to_owned(),
            Self::AddCatchpoint { calls, phase, .. } => {
                format!("add catchpoint {phase:?} {} calls", calls.len())
            }
            Self::RemoveCatchpoint { id, .. } => format!("remove catchpoint {id:?}"),
            Self::ReadMemory { .. } => "read memory".to_owned(),
            Self::LoadedModule { .. } => "loaded module".to_owned(),
            Self::LoadedModules { .. } => "loaded modules".to_owned(),
            Self::ModuleImage { .. } => "module image".to_owned(),
            Self::Disassemble { .. } => "disassemble".to_owned(),
            Self::DescribeAddress { .. } => "describe address".to_owned(),
            Self::StoppedLocation { .. } => "stopped location".to_owned(),
            Self::Snapshot { .. } => "snapshot".to_owned(),
            Self::Tasks { stop_id, from, .. } => format!("tasks {stop_id:?} from {from:?}"),
            Self::StoppedSelection { .. } => "stopped selection".to_owned(),
            Self::Backtrace { .. } => "backtrace".to_owned(),
            Self::Registers { .. } => "registers".to_owned(),
            Self::StepTargets { .. } => "step targets".to_owned(),
            Self::Variables { .. } => "variables".to_owned(),
            Self::Evaluate { expression, .. } => format!("evaluate `{}`", expression.text()),
            Self::ExpressionType { expression, .. } => format!("type of `{}`", expression.text()),
            Self::Dereference { .. } => "dereference".to_owned(),
            Self::ValueChildren { .. } => "value children".to_owned(),
            Self::VisualizerInputs { .. } => "visualizer inputs".to_owned(),
            Self::Numbers { .. } => "numbers".to_owned(),
            Self::Renderers { .. } => "renderers".to_owned(),
            Self::Globals { .. } => "globals".to_owned(),
            Self::SetViews { .. } => "set views".to_owned(),
            Self::EnableViews { enabled, .. } => format!("enable views {enabled}"),
            Self::SetStepIntoRuntime { enter, .. } => format!("step into runtime {enter}"),
            Self::SetStepSkips { skips, .. } => format!("step skips {:?}", skips.patterns()),
            Self::ExplainView { expression, .. } => {
                format!("explain the view of `{}`", expression.text())
            }
            Self::RecordKernels { expression, .. } => {
                format!("record the kernels of `{}`", expression.text())
            }
            Self::ExplainType { name, .. } => format!("explain the views of `{name}`"),
            Self::CheckViews { .. } => "check views".to_owned(),
            Self::SelectContext { context, .. } => format!("select {context}"),
            Self::SelectFrame { .. } => "select frame".to_owned(),
            Self::SignalPolicy { .. } => "signal policy".to_owned(),
            Self::Kill { .. } => "kill".to_owned(),
            Self::Terminate { .. } => "terminate".to_owned(),
            Self::Shutdown { .. } => "shutdown".to_owned(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const COMPARISONS: [HitComparison; 7] = [
        HitComparison::Equal,
        HitComparison::NotEqual,
        HitComparison::Less,
        HitComparison::LessOrEqual,
        HitComparison::Greater,
        HitComparison::GreaterOrEqual,
        HitComparison::Multiple,
    ];

    #[test]
    fn hit_conditions_parse_their_display_form_and_select_hits() {
        let selected = |text: &str| {
            let condition = text.parse::<HitCondition>().expect(text);
            assert_eq!(
                condition.to_string().parse::<HitCondition>().expect(text),
                condition
            );
            (1..=12)
                .filter(|&hit| condition.is_met(hit))
                .collect::<Vec<_>>()
        };

        assert_eq!(selected("==3"), [3]);
        assert_eq!(selected(" != 3 "), [1, 2, 4, 5, 6, 7, 8, 9, 10, 11, 12]);
        assert_eq!(selected("<3"), [1, 2]);
        assert_eq!(selected("<=3"), [1, 2, 3]);
        assert_eq!(selected(">10"), [11, 12]);
        assert_eq!(selected(">= 10"), [10, 11, 12]);
        assert_eq!(selected("%5"), [5, 10]);
        assert_eq!(selected(">=0"), (1..=12).collect::<Vec<_>>());
    }

    #[test]
    fn hit_conditions_reject_ambiguous_malformed_and_unsatisfiable_text() {
        let message = |text: &str| match text.parse::<HitCondition>() {
            Err(crate::Error::InvalidHitCondition(message)) => message,
            other => panic!("{text:?} parsed as {other:?}"),
        };

        assert!(message("5").contains("write ==5"), "{}", message("5"));
        for text in [
            "", "==", "= 5", "=>5", "<<5", "==-1", "==+1", "==5x", "==1e3", "%%2",
        ] {
            assert!(message(text).contains("not an operator"), "{text:?}");
        }

        assert!(message("==18446744073709551616").contains("not an operator"));
        for text in ["==0", "<1", "<0", "<=0", "%0", ">18446744073709551615"] {
            assert!(message(text).contains("no hit can satisfy"), "{text:?}");
        }
    }

    #[test]
    fn construction_accepts_exactly_the_conditions_some_hit_meets() {
        for comparison in COMPARISONS {
            for count in (0..8).chain([u64::MAX - 1, u64::MAX]) {
                let unchecked = HitCondition { comparison, count };
                let hits = (1..64).chain([u64::MAX - 1, u64::MAX]);
                assert_eq!(
                    HitCondition::new(comparison, count).is_ok(),
                    hits.clone().any(|hit| unchecked.is_met(hit)),
                    "{unchecked}"
                );
            }
        }
    }

    #[test]
    fn later_stops_are_predicted_exactly_by_the_conditions_hits() {
        for comparison in COMPARISONS {
            for count in 0..8 {
                let Ok(condition) = HitCondition::new(comparison, count) else {
                    continue;
                };

                for hits in 0..16 {
                    assert_eq!(
                        condition.may_stop_after(hits),
                        (hits + 1..64).any(|hit| condition.is_met(hit)),
                        "{condition} after {hits} hits"
                    );
                }
            }
        }
    }
}
