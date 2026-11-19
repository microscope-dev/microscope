#![cfg_attr(
    not(test),
    deny(
        clippy::expect_used,
        clippy::indexing_slicing,
        clippy::panic,
        clippy::todo,
        clippy::unimplemented,
        clippy::unreachable,
        clippy::unwrap_used
    )
)]

macro_rules! record {
    ($($argument:tt)*) => {{
        #[cfg(debug_assertions)]
        $crate::flight_recorder::record(::std::format_args!($($argument)*));
    }};
}

#[cfg(test)]
extern crate self as microscope;

pub mod agent;
use microscope_data::analysis;
mod backend;
pub(crate) use microscope_data::capture;

mod condition;
mod debug_info;
mod demangle;
mod disassembly;
mod error;
mod eval;
#[cfg(debug_assertions)]
#[doc(hidden)]
pub mod flight_recorder;
mod group;
pub(crate) mod image;
pub use image::functions::{CodeInstance, Function};
pub use image::symbols::Symbol;
pub mod cache;
mod inspection;
pub(crate) mod model;

pub mod pool;
pub mod profiler;
mod protocol;
mod runtime_model;
mod rust_names;

#[cfg(any(test, feature = "sim"))]
#[doc(hidden)]
#[allow(
    clippy::expect_used,
    clippy::panic,
    clippy::unreachable,
    clippy::unwrap_used
)]
pub mod sim;
mod source_map;
mod step_skip;
pub mod suggest;
mod sync;
pub use microscope_data::{sanitize, syscalls};

pub use backend::syscall_engines;
#[cfg(test)]
mod test_memory;
#[cfg(test)]
pub(crate) use microscope_test_strategies as test_strategies;
pub mod timings;

#[doc(hidden)]
pub mod unsafe_parts {
    pub use crate::backend::perf::sys::{DATA_HEAD, DATA_TAIL, HEAP_DATA, RingMemory};
}

#[doc(hidden)]
pub mod python_agent {
    pub use crate::runtime_model::{
        is_python_agent_filename as is_agent_filename, python_agent_bootstrap as bootstrap,
        python_agent_script as script,
    };
}

#[cfg(feature = "tools")]
#[doc(hidden)]
pub mod tools;
mod type_identity;
mod unwind;
mod view;
pub mod view_files;

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{broadcast, mpsc, oneshot, watch};
use tokio::time::timeout;

use backend::ControllerMessage;
use protocol::Request;

pub use condition::{Condition, LogMessage, LogSegment};
pub use disassembly::{
    AssemblySyntax, BlockCompletion, BoundaryConflict, BoundaryEvidence, ContextShortfall,
    ControlFlow, DecodedInstruction, DisassembledFunction, DisassembledInstruction, Disassembly,
    DisassemblyBlock, DisassemblyQuery, DisassemblyRange, DisassemblyView, FunctionOrigin,
    IndirectTarget, InstructionContent, InstructionReference, InstructionReferenceKind,
    InstructionToken, InstructionTokenKind, MAX_BACKWARD_DISTANCE, MAX_FUNCTION_INSTRUCTIONS,
    MAX_WINDOW_AFTER, MAX_WINDOW_BEFORE, TargetBoundary,
};
pub use step_skip::StepSkips;

pub use error::{Error, Result};
pub use eval::Evaluation;
pub use eval::bind::Mode as EvaluationMode;
pub use eval::error::{ErrorKind as ExpressionErrorKind, ExpressionError};
pub use eval::syntax::{Expression, Span};
pub use inspection::MAX_INSPECTION_LIMITS;
pub use model::{
    Accessibility, AddressDescription, AddressRange, AddressValue, Architecture, ArgumentOrigin,
    ArrayBound, ArrayDimension, ArrayExtent, ArrayOrdering, Backtrace, BaseClass,
    BaseClassVirtuality, BaseType, BaseTypeEncoding, BreakpointEntry, BreakpointLocation,
    ByteOrder, CBaseType, CallFrameUnavailableReason, CodeInstanceId, CodeInstanceInfo,
    CodeInstanceKind, CodeRole, ColumnNumber, CoroutineInfo, CoroutineKind, CoroutineState,
    CoroutineStateKind, DebugFile, DebugInformation, DereferenceReference, DereferenceState,
    DereferenceUnavailableReason, DereferencedValue, DrivenFrames, EmbeddedSymbolTable,
    EntryProvenance, EntryValueUnavailableReason, EnumerationOrigin, Enumerator, ExecutionContext,
    ExecutionLocation, FloatValue, FrameKind, FunctionGenerics, FunctionId, FunctionInfo,
    GlobalVariableCandidate, GlobalVariableId, GlobalVariableInfo, GlobalVariablePage,
    GlobalVariableReference, GlobalVariableType, GlobalVariableVisibility, GoKind,
    GoTypeAttributes, GotSlot, GotTarget, ImageAddress, ImageAddressDescription, ImageLocation,
    InlineChain, InlineFrameLookup, InspectedValue, InspectionCompletion, InspectionExhaustion,
    InspectionLimit, InspectionLimits, InspectionUsage, IntegerValue, LineNumber, LineSequenceId,
    LoadBias, LoadedGlobalVariableInfo, LoadedModule, LoadedModuleRecord, LoadedModuleSnapshot,
    ManagedLocation, ManagedPlace, MapKey, MemoryRead, MemoryReadCompletion,
    MemoryReadUnavailableReason, ModuleAddress, ModuleId, ModuleImage, ModuleImageId,
    NamedTypeRelationship, OptimizedOutReason, PointerWidth, Presentation, PresentedCount,
    PresentedShape, RecordKind, RecordMember, RecordMemberLayout, RecordedFrame, RecordedPlace,
    ReferenceKind, RegisterDescriptor, RegisterId, RegisterRole, RegisterSnapshot, RegisterValue,
    ResumePoint, ResumePoints, RuntimeDimension, RuntimeId, ScalarValue, SectionId, SectionInfo,
    SectionLocation, ShapeUnresolvedReason, SliceWords, SourceContext, SourceFile, SourceFileId,
    SourceLanguage, SourceLine, SourceLocation, StackFrame, StackFrameId, StackSegment,
    StateMember, StatementFlags, StatementRow, SymbolBinding, SymbolExtent, SymbolExtentProvenance,
    SymbolId, SymbolInfo, SymbolKind, SymbolLocation, SymbolTableSources, TargetDescription,
    TaskCursor, TaskId, TaskLocation, TaskPage, TaskSnapshot, TaskState, TextCompletion,
    TextSummary, ThreadActivity, ThreadId, ThreadLabel, ThreadLocal, TlsUnavailableReason,
    TrapAddress, TypeArgument, TypeId, TypeIdentity, TypeInfo, TypeKind, TypeModifier, TypeNode,
    TypeReference, Unfollowed, UnsupportedVariableFeature, UnwindTermination,
    ValueAccessUnavailableReason, ValueBitRange, ValueChild, ValueChildPage,
    ValueChildRelationship, ValueChildren, ValueChildrenReference, Variable, VariableInvalidReason,
    VariableKind, VariableMalformedKind, VariableMalformedReason, VariableSnapshot, VariableState,
    VariableUnavailableReason, VariableValue, VariableValueSource, Variant, VariantDiscriminant,
    VariantSelection, VariantSelector, VariantStorageKind, ViewName, ViewProblem, VirtualAddress,
    Visualizer, VisualizerReference,
};

pub use group::{
    GroupEvent, GroupExecutionId, GroupHandle, GroupStop, Member, MemberNumber, MemberState,
    ProcessGroup,
};
pub use protocol::{
    Breakpoint, BreakpointHit, BreakpointId, BreakpointOptions, BreakpointSpec, CatchPhase,
    Catchpoint, CatchpointId, ConditionOwner, CoreDumpInfo, CoreDumpOptions, CoreModule,
    CoreModuleState, DebugFileOptions, DebuggerEvent, ExceptionDisposition, ExceptionFilter,
    ExceptionInfo, ExceptionStops, ExecutionId, ExitStatus, FollowMode, FollowPolicy,
    FramePresentation, GlobalVariableQuery, HitComparison, HitCondition, InferiorState,
    InvalidatedWatchpoint, KernelSource, LanguageException, LanguageExceptionKind, LaunchOptions,
    LogPart, MAX_DRAWING_BYTES, ModuleIdentity, NameMatch, NamedProcess, NumberKind, Numbers,
    PendingBreakpointLocation, PresentedFrame, ProcessId, Renderer, ResolvedBreakpointLocation,
    ResumeScope, SignalPolicy, StateSnapshot, StepKind, StepTarget, StopId, StopReason, StopScope,
    SyscallStop, TaskEnding, ThreadSnapshot, ThreadState, TypeViews, ValueChildQuery,
    VariableQuery, ViewCandidate, ViewCheck, ViewExplanation, VisualizerBinding, VisualizerInput,
    VisualizerInputs, VisualizerValue, WatchAccess, WatchScope, WatchTarget, Watchpoint,
    WatchpointCapabilities, WatchpointHit, WatchpointId, WatchpointInvalidation, WatchpointOptions,
    WatchpointSpec,
};

pub use runtime_model::{JAVASCRIPT_EXTENSIONS, TASK_NOUNS, names_runtime_function};
pub use source_map::SourcePathMap;
pub use view::summary::{
    Characters, characters as characters_of, float as float_text, function as function_text,
    integer as integer_text, quoted as quoted_text, scalar as scalar_text, symbol as symbol_text,
    value as value_summary,
};

pub use view::syntax::Error as ViewFileError;

#[must_use]
pub fn built_in_views() -> Vec<Arc<ViewName>> {
    view::ViewSet::built_in()
        .views()
        .iter()
        .map(|view| view::name_of(view))
        .collect()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplayedRun {
    pub kernel: String,

    pub events: usize,

    pub outcome: std::result::Result<usize, String>,
}

pub fn replay_kernel_runs(
    text: &str,
    wasm: Option<&[u8]>,
) -> std::result::Result<Vec<ReplayedRun>, String> {
    let recordings = view::kernel::parse_recordings(text)?;
    let mut replayed = Vec::new();

    for recording in &recordings {
        let kernel = match wasm {
            Some(wasm) => Arc::new(view::kernel::Kernel::new(
                &recording.kernel,
                "replayed",
                wasm,
            )?),
            None => view::ViewSet::built_in()
                .kernel(&recording.kernel)
                .ok_or_else(|| format!("no built-in kernel is named `{}`", recording.kernel))?,
        };

        replayed.push(ReplayedRun {
            kernel: recording.kernel.clone(),
            events: recording.events.len(),
            outcome: view::kernel::replay(&kernel, recording),
        });
    }

    Ok(replayed)
}

#[must_use]
pub fn is_built_in_view(name: &ViewName) -> bool {
    view::ViewSet::built_in()
        .views()
        .iter()
        .any(|view| view.source == name.source && view.line == name.line)
}

pub fn processes_named(name: &str) -> Result<Vec<NamedProcess>> {
    backend::processes_named(name)
}

#[must_use]
pub fn signal_named(name: &str) -> Option<u64> {
    backend::signal_named(name)
}

#[must_use]
pub fn signal_name(code: u64) -> Option<String> {
    backend::signal_name(code)
}

pub fn signal_codes() -> impl Iterator<Item = u64> {
    backend::signal_codes()
}

#[cfg(feature = "fuzzing")]
#[doc(hidden)]
pub fn fuzz_core_dump(data: &[u8]) {
    backend::fuzz_core_dump(data);
}

#[cfg(feature = "fuzzing")]
#[doc(hidden)]
pub fn fuzz_image(data: &[u8]) {
    image::sample::fuzz(data);
}

#[cfg(feature = "fuzzing")]
#[doc(hidden)]
pub fn fuzz_perf_records(data: &[u8]) {
    backend::perf::decode::fuzz(data);
}

#[cfg(feature = "fuzzing")]
#[doc(hidden)]
pub fn fuzz_capture(data: &[u8]) {
    capture::file::fuzz(data);
}

#[cfg(feature = "fuzzing")]
#[doc(hidden)]
pub fn fuzz_cdp_message(data: &[u8]) {
    backend::fuzz_cdp_message(data);
}

#[cfg(feature = "fuzzing")]
#[doc(hidden)]
pub fn fuzz_debug_register_plan(data: &[u8]) {
    backend::fuzz_debug_register_plan(data);
}

#[cfg(all(feature = "fuzzing", feature = "tools"))]
#[doc(hidden)]
pub fn fuzz_dwarf_load(data: &[u8]) {
    debug_info::fuzz_dwarf_load(data);
}

#[cfg(feature = "fuzzing")]
#[doc(hidden)]
pub fn fuzz_runtime_model(runtime: FuzzedRuntime, data: &[u8]) {
    runtime_fuzz::fuzz(runtime, data);
}

#[cfg(feature = "fuzzing")]
mod runtime_fuzz;
#[cfg(feature = "fuzzing")]
#[doc(hidden)]
pub use runtime_fuzz::Runtime as FuzzedRuntime;

#[cfg(all(feature = "fuzzing", target_arch = "x86_64"))]
#[doc(hidden)]
pub fn fuzz_jit_code(data: &[u8]) {
    debug_info::fuzz_jit_code(data);
}

#[cfg(feature = "fuzzing")]
#[doc(hidden)]
pub fn fuzz_elf_symbols(data: &[u8]) {
    debug_info::fuzz_elf_symbols(data);
}

#[cfg(all(feature = "fuzzing", target_arch = "x86_64"))]
#[doc(hidden)]
pub fn fuzz_dispatch(data: &[u8]) {
    debug_info::fuzz_dispatch(data);
}

#[cfg(feature = "fuzzing")]
#[doc(hidden)]
pub fn fuzz_gopclntab(data: &[u8]) {
    debug_info::fuzz_gopclntab(data);
}

#[cfg(feature = "fuzzing")]
#[doc(hidden)]
pub fn fuzz_js_source_map(data: &[u8]) {
    debug_info::fuzz_js_source_map(data);
}

#[cfg(feature = "fuzzing")]
#[doc(hidden)]
pub fn fuzz_disassembly(data: &[u8]) {
    disassembly::fuzz(data);
}

#[cfg(feature = "fuzzing")]
#[doc(hidden)]
pub fn fuzz_views(data: &[u8]) {
    view::fuzz::hostile(data);
}

#[cfg(feature = "fuzzing")]
#[doc(hidden)]
#[allow(clippy::panic, reason = "a fuzz harness reports a bug by panicking")]
pub fn fuzz_expression_parse(text: &str) {
    if let Err(failure) = eval::syntax::check_invariants(text) {
        panic!("{failure}");
    }
}

#[cfg(feature = "fuzzing")]
#[doc(hidden)]
pub fn fuzz_dwarf_expression(data: &[u8]) {
    debug_info::fuzz_dwarf_expression(data);
}

fn execution_end(event: DebuggerEvent, execution: ExecutionId) -> Option<Result<StopReason>> {
    match event {
        DebuggerEvent::InferiorStopped {
            execution_id: Some(event_execution),
            reason,
            ..
        } if event_execution == execution => Some(Ok(reason)),
        DebuggerEvent::InferiorExited {
            execution_id: Some(event_execution),
            status,
            ..
        } if event_execution == execution => Some(Ok(StopReason::Exited(status))),
        DebuggerEvent::InferiorDetached { .. } => Some(Err(Error::RequestCancelled)),
        _ => None,
    }
}

const REQUEST_CAPACITY: usize = 32;
const EVENT_CAPACITY: usize = 256;
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);

fn event_capacity() -> usize {
    std::env::var("MICROSCOPE_EVENT_CAPACITY")
        .ok()
        .and_then(|capacity| capacity.parse().ok())
        .filter(|capacity| *capacity != 0)
        .unwrap_or(EVENT_CAPACITY)
}

pub struct Debugger {
    handle: DebuggerHandle,
    controller: Option<backend::ControllerJoin>,
    shutdown_permit: Option<mpsc::OwnedPermit<ControllerMessage>>,
}

#[derive(Clone)]
pub struct DebuggerHandle {
    program: watch::Receiver<Arc<ModuleImage>>,

    launched: Arc<PathBuf>,
    core_dump: Option<Arc<CoreDumpInfo>>,
    source_paths: Arc<SourcePathMap>,
    requests: mpsc::Sender<ControllerMessage>,
    events: broadcast::Sender<DebuggerEvent>,
}

#[derive(Clone)]
pub struct Program {
    executable: backend::Executable,
    debug_info: debug_info::DebugInfo,
}

impl Program {
    pub fn load(executable: impl AsRef<Path>, debug_files: &DebugFileOptions) -> Result<Self> {
        let mut executable = backend::executable_source(executable.as_ref())?;
        executable.debug_files = debug_info::DebugFileSearch::new(debug_files);
        Self::from_executable_source(executable)
    }

    async fn of_process(
        process: ProcessId,
        executable: Option<&Path>,
        debug_files: &DebugFileOptions,
    ) -> Result<Self> {
        let executable = executable.map(Path::to_owned);
        let debug_files = debug_info::DebugFileSearch::new(debug_files);
        tokio::task::spawn_blocking(move || {
            let mut source = match executable {
                Some(executable) => backend::executable_source(&executable)?,
                None => backend::process_executable_source(process)?,
            };
            source.debug_files = debug_files;
            Self::from_executable_source(source)
        })
        .await
        .map_err(|_| Error::internal("reading the program to attach to panicked"))?
    }

    #[must_use]
    pub fn keeping_modules(mut self) -> Self {
        self.executable.kept = Some(backend::KeptModules::default());
        self
    }

    fn from_executable_source(executable: backend::ExecutableSource) -> Result<Self> {
        let debug_info = debug_info::load_program(
            &executable.display_path,
            &executable.data,
            &executable.debug_files,
        )?;

        Ok(Self {
            executable: executable.described(),
            debug_info,
        })
    }
}

impl Debugger {
    pub fn new(executable: impl AsRef<Path>) -> Result<Self> {
        Self::new_with(executable, &DebugFileOptions::default())
    }

    pub fn new_with(executable: impl AsRef<Path>, debug_files: &DebugFileOptions) -> Result<Self> {
        Self::for_program(Program::load(executable, debug_files)?)
    }

    pub fn for_program(program: Program) -> Result<Self> {
        let Program {
            executable,
            debug_info,
        } = program;

        let launched = Arc::clone(&executable.launch_path);
        Self::start(launched, |channels| {
            let (program, controller) =
                backend::spawn_controller(executable, debug_info, channels)?;
            Ok((program, None, controller))
        })
    }

    fn for_group(program: Program, policy: FollowPolicy) -> Result<(Self, backend::GroupBackend)> {
        let Program {
            executable,
            debug_info,
        } = program;

        let launched = Arc::clone(&executable.launch_path);
        let mut group = None;
        let debugger = Self::start(launched, |channels| {
            let (program, controller, backend) = backend::spawn_group(
                executable,
                debug_info,
                channels,
                policy,
                backend::Capacity {
                    requests: REQUEST_CAPACITY,
                    events: event_capacity(),
                },
            )?;
            group = Some(backend);
            Ok((program, None, controller))
        })?;

        let group = group.ok_or_else(|| Error::internal("a group's tracer started"))?;
        Ok((debugger, group))
    }

    fn joined(joined: backend::Joined) -> Result<(Self, backend::Started)> {
        let backend::Joined {
            requests,
            events,
            program,
            launched,
            controller,
            started,
            ..
        } = joined;
        let shutdown_permit = requests
            .clone()
            .try_reserve_owned()
            .map_err(|_| Error::internal("a new request channel has no shutdown capacity"))?;

        Ok((
            Self {
                handle: DebuggerHandle {
                    program,
                    launched,
                    core_dump: None,
                    source_paths: Arc::default(),
                    requests,
                    events,
                },
                controller: Some(controller),
                shutdown_permit: Some(shutdown_permit),
            },
            started,
        ))
    }

    pub async fn attach(process: ProcessId) -> Result<Self> {
        Self::attach_with(process, None, &DebugFileOptions::default()).await
    }

    pub async fn attach_with_executable(
        process: ProcessId,
        executable: impl AsRef<Path>,
    ) -> Result<Self> {
        Self::attach_with(
            process,
            Some(executable.as_ref()),
            &DebugFileOptions::default(),
        )
        .await
    }

    pub async fn attach_with(
        process: ProcessId,
        executable: Option<&Path>,
        debug_files: &DebugFileOptions,
    ) -> Result<Self> {
        let program = Program::of_process(process, executable, debug_files).await?;
        let debugger = Self::for_program(program)?;
        let attached = debugger
            .handle
            .request(|reply| Request::Attach {
                process_id: process,
                observe: false,
                reply,
            })
            .await;
        if let Err(error) = attached {
            let _ = debugger.shutdown().await;
            return Err(error);
        }

        Ok(debugger)
    }

    pub fn open_core(options: &CoreDumpOptions) -> Result<Self> {
        Self::start(Arc::new(options.core.clone()), |channels| {
            let session = backend::open_core(options, channels)?;
            let (_, program) = watch::channel(session.image);
            Ok((
                program,
                Some(session.info),
                backend::ControllerJoin::Thread(session.controller),
            ))
        })
    }

    fn start(
        launched: Arc<PathBuf>,
        spawn: impl FnOnce(
            backend::ControllerChannels,
        ) -> Result<(
            watch::Receiver<Arc<ModuleImage>>,
            Option<Arc<CoreDumpInfo>>,
            backend::ControllerJoin,
        )>,
    ) -> Result<Self> {
        #[cfg(debug_assertions)]
        flight_recorder::record_panics();

        let (requests, receiver) = mpsc::channel(REQUEST_CAPACITY);
        let shutdown_permit = requests
            .clone()
            .try_reserve_owned()
            .map_err(|_| Error::internal("a new request channel has no shutdown capacity"))?;
        let (events, _) = broadcast::channel(event_capacity());
        let (program, core_dump, controller) = spawn(backend::ControllerChannels {
            sender: requests.clone(),
            receiver,
            events: events.clone().into(),
        })?;

        Ok(Self {
            handle: DebuggerHandle {
                program,
                launched,
                core_dump,
                source_paths: Arc::default(),
                requests,
                events,
            },
            controller: Some(controller),
            shutdown_permit: Some(shutdown_permit),
        })
    }

    #[must_use]
    pub fn handle(&self) -> DebuggerHandle {
        self.handle.clone()
    }

    pub async fn shutdown(mut self) -> Result<()> {
        let (send, mut receive) = oneshot::channel();
        self.shutdown_permit
            .take()
            .ok_or_else(|| Error::internal("the shutdown permit is present"))?
            .send(ControllerMessage::Request(Request::Shutdown {
                reply: send,
            }));

        let replied = timeout(SHUTDOWN_TIMEOUT, async {
            tokio::select! {
                biased;
                replied = &mut receive => Some(replied.ok()),
                () = self.handle.requests.closed() => None,
            }
        })
        .await
        .map_err(|_| Error::ShutdownTimedOut)?;

        if let Some(controller) = self.controller.take() {
            tokio::task::spawn_blocking(move || controller.join())
                .await
                .map_err(|_| Error::BackendThreadPanicked)?
                .map_err(|()| Error::BackendThreadPanicked)?;
        }

        replied
            .unwrap_or_else(|| receive.try_recv().ok())
            .unwrap_or(Err(Error::RequestCancelled))
    }
}

impl Drop for Debugger {
    fn drop(&mut self) {
        if let Some(permit) = self.shutdown_permit.take() {
            let (reply, _) = oneshot::channel();
            permit.send(ControllerMessage::Request(Request::Shutdown { reply }));
        }
    }
}

impl DebuggerHandle {
    #[must_use]
    pub fn with_source_paths(mut self, source_paths: SourcePathMap) -> Self {
        self.source_paths = Arc::new(source_paths);
        self
    }

    #[must_use]
    pub fn executable(&self) -> PathBuf {
        self.program.borrow().path().to_owned()
    }

    #[must_use]
    pub fn launched(&self) -> &Path {
        &self.launched
    }

    #[must_use]
    pub fn module_image(&self) -> Arc<ModuleImage> {
        Arc::clone(&self.program.borrow())
    }

    #[must_use]
    pub const fn core_dump(&self) -> Option<&Arc<CoreDumpInfo>> {
        self.core_dump.as_ref()
    }

    #[must_use]
    pub fn subscribe(&self) -> broadcast::Receiver<DebuggerEvent> {
        self.events.subscribe()
    }

    pub async fn add_breakpoint(&self, spec: BreakpointSpec) -> Result<Breakpoint> {
        self.add_breakpoint_with(spec, BreakpointOptions::default())
            .await
    }

    pub async fn add_breakpoint_with(
        &self,
        spec: BreakpointSpec,
        options: BreakpointOptions,
    ) -> Result<Breakpoint> {
        self.request(|reply| Request::AddBreakpoint {
            spec,
            options: Box::new(options),
            reply,
        })
        .await
    }

    pub async fn add_breakpoint_with_hit_condition(
        &self,
        spec: BreakpointSpec,
        hit_condition: HitCondition,
    ) -> Result<Breakpoint> {
        self.add_breakpoint_with(
            spec,
            BreakpointOptions {
                hit_condition: Some(hit_condition),
                ..BreakpointOptions::default()
            },
        )
        .await
    }

    pub async fn set_breakpoint_hit_condition(
        &self,
        id: BreakpointId,
        hit_condition: Option<HitCondition>,
    ) -> Result<Breakpoint> {
        self.request(|reply| Request::SetBreakpointHitCondition {
            id,
            hit_condition,
            reply,
        })
        .await
    }

    pub async fn set_breakpoint_condition(
        &self,
        id: BreakpointId,
        condition: Option<Condition>,
    ) -> Result<Breakpoint> {
        self.request(|reply| Request::SetBreakpointCondition {
            id,
            condition,
            reply,
        })
        .await
    }

    pub async fn set_breakpoint_log_message(
        &self,
        id: BreakpointId,
        log_message: Option<LogMessage>,
    ) -> Result<Breakpoint> {
        self.request(|reply| Request::SetBreakpointLogMessage {
            id,
            log_message,
            reply,
        })
        .await
    }

    pub async fn set_breakpoint_enabled(
        &self,
        id: BreakpointId,
        enabled: bool,
    ) -> Result<Breakpoint> {
        self.request(|reply| Request::SetBreakpointEnabled { id, enabled, reply })
            .await
    }

    pub async fn remove_breakpoint(&self, id: BreakpointId) -> Result<Breakpoint> {
        self.request(|reply| Request::RemoveBreakpoint { id, reply })
            .await
    }

    pub async fn remove_all_breakpoints(&self) -> Result<Arc<[Breakpoint]>> {
        self.request(|reply| Request::RemoveAllBreakpoints { reply })
            .await
    }

    #[must_use]
    pub fn watchpoint_capabilities(&self) -> WatchpointCapabilities {
        backend::watchpoint_capabilities()
    }

    pub async fn resolve_watch_target(&self, expression: &Expression) -> Result<WatchTarget> {
        self.selected()
            .await?
            .resolve_watch_target(expression)
            .await
    }

    pub async fn add_watchpoint(
        &self,
        spec: WatchpointSpec,
        access: WatchAccess,
    ) -> Result<Watchpoint> {
        self.add_watchpoint_with(spec, access, WatchpointOptions::default())
            .await
    }

    pub async fn add_watchpoint_with(
        &self,
        spec: WatchpointSpec,
        access: WatchAccess,
        options: WatchpointOptions,
    ) -> Result<Watchpoint> {
        self.request(|reply| Request::AddWatchpoint {
            spec,
            access,
            options,
            reply,
        })
        .await
    }

    pub async fn watch(&self, expression: &Expression, access: WatchAccess) -> Result<Watchpoint> {
        self.watch_with(expression, access, WatchpointOptions::default())
            .await
    }

    pub async fn watch_with(
        &self,
        expression: &Expression,
        access: WatchAccess,
        options: WatchpointOptions,
    ) -> Result<Watchpoint> {
        let target = self.resolve_watch_target(expression).await?;
        self.add_watchpoint_with(WatchpointSpec::Target(Box::new(target)), access, options)
            .await
    }

    pub async fn set_watchpoint_hit_condition(
        &self,
        id: WatchpointId,
        hit_condition: Option<HitCondition>,
    ) -> Result<Watchpoint> {
        self.request(|reply| Request::SetWatchpointHitCondition {
            id,
            hit_condition,
            reply,
        })
        .await
    }

    pub async fn set_watchpoint_condition(
        &self,
        id: WatchpointId,
        condition: Option<Condition>,
    ) -> Result<Watchpoint> {
        self.request(|reply| Request::SetWatchpointCondition {
            id,
            condition,
            reply,
        })
        .await
    }

    pub async fn set_watchpoint_enabled(
        &self,
        id: WatchpointId,
        enabled: bool,
    ) -> Result<Watchpoint> {
        self.request(|reply| Request::SetWatchpointEnabled { id, enabled, reply })
            .await
    }

    pub async fn remove_watchpoint(&self, id: WatchpointId) -> Result<Watchpoint> {
        self.request(|reply| Request::RemoveWatchpoint { id, reply })
            .await
    }

    pub async fn add_catchpoint(
        &self,
        calls: Arc<[syscalls::SyscallId]>,
        phase: CatchPhase,
        condition: Option<Condition>,
    ) -> Result<Catchpoint> {
        self.request(|reply| Request::AddCatchpoint {
            calls,
            phase,
            condition,
            reply,
        })
        .await
    }

    pub async fn remove_catchpoint(&self, id: CatchpointId) -> Result<Catchpoint> {
        self.request(|reply| Request::RemoveCatchpoint { id, reply })
            .await
    }

    pub async fn remove_all_watchpoints(&self) -> Result<Arc<[Watchpoint]>> {
        self.request(|reply| Request::RemoveAllWatchpoints { reply })
            .await
    }

    pub async fn signal_policy(&self, signal: u64) -> Result<SignalPolicy> {
        self.request(|reply| Request::SignalPolicy { signal, reply })
            .await
    }

    pub async fn set_signal_policy(
        &self,
        signal: u64,
        policy: SignalPolicy,
    ) -> Result<SignalPolicy> {
        self.request(|reply| Request::SetSignalPolicy {
            signal,
            policy,
            reply,
        })
        .await
    }

    pub async fn set_exception_stops(&self, stops: ExceptionStops) -> Result<ExceptionStops> {
        self.request(|reply| Request::SetExceptionStops { stops, reply })
            .await
    }

    pub async fn set_exec_stops(&self, stops: bool) -> Result<bool> {
        self.request(|reply| Request::SetExecStops { stops, reply })
            .await
    }

    pub async fn kill(&self) -> Result<()> {
        self.request(|reply| Request::Kill { reply }).await
    }

    pub async fn terminate(&self) -> Result<()> {
        self.request(|reply| Request::Terminate { reply }).await
    }

    pub async fn launch(&self) -> Result<ExecutionId> {
        self.launch_with(LaunchOptions::default()).await
    }

    pub async fn launch_with(&self, options: LaunchOptions) -> Result<ExecutionId> {
        self.request(|reply| Request::Launch {
            options: Box::new(options),
            reply,
        })
        .await
    }

    pub async fn launch_by_exec(
        &self,
        process_id: ProcessId,
        stop_at_entry: bool,
        release: impl FnOnce() + Send + 'static,
    ) -> Result<ExecutionId> {
        self.request(|reply| Request::LaunchByExec {
            process_id,
            stop_at_entry,
            release: Box::new(release),
            reply,
        })
        .await
    }

    pub async fn attach_process(&self, process_id: ProcessId) -> Result<StopId> {
        self.request(|reply| Request::Attach {
            process_id,
            observe: false,
            reply,
        })
        .await
    }

    pub async fn observe_process(&self, process_id: ProcessId) -> Result<StopId> {
        self.request(|reply| Request::Attach {
            process_id,
            observe: true,
            reply,
        })
        .await
    }

    pub async fn run(&self) -> Result<StopReason> {
        self.run_with(LaunchOptions::default()).await
    }

    pub async fn run_with(&self, options: LaunchOptions) -> Result<StopReason> {
        let mut events = self.subscribe();
        let execution = self.launch_with(options).await?;

        self.wait_for_execution(&mut events, execution).await
    }

    pub async fn continue_execution(
        &self,
        stop_id: StopId,
        scope: ResumeScope,
        exception: ExceptionDisposition,
    ) -> Result<ExecutionId> {
        let process_id = match scope {
            ResumeScope::Process(process_id) => process_id,
            ResumeScope::Thread(_) => self.stopped_selection().await?.process,
        };

        self.request(|reply| Request::Continue {
            process_id,
            stop_id,
            scope,
            exception,
            reply,
        })
        .await
    }

    pub async fn resume(&self) -> Result<StopReason> {
        self.resume_with_exception(ExceptionDisposition::Pass).await
    }

    pub async fn resume_with_exception(
        &self,
        exception: ExceptionDisposition,
    ) -> Result<StopReason> {
        let selection = self.stopped_selection().await?;
        let mut events = self.subscribe();
        let execution = self
            .continue_execution(
                selection.stop,
                ResumeScope::Process(selection.process),
                exception,
            )
            .await?;

        self.wait_for_execution(&mut events, execution).await
    }

    pub async fn start_step(
        &self,
        stop_id: StopId,
        context: impl Into<ExecutionContext>,
        frame: StackFrameId,
        kind: StepKind,
        scope: ResumeScope,
        exception: ExceptionDisposition,
    ) -> Result<ExecutionId> {
        let process_id = self.stopped_selection().await?.process;
        let context = context.into();

        self.request(|reply| Request::Step {
            process_id,
            stop_id,
            context,
            frame,
            kind,
            call: None,
            scope,
            exception,
            reply,
        })
        .await
    }

    pub async fn start_step_into(
        &self,
        stop_id: StopId,
        context: impl Into<ExecutionContext>,
        call: VirtualAddress,
        scope: ResumeScope,
        exception: ExceptionDisposition,
    ) -> Result<ExecutionId> {
        let process_id = self.stopped_selection().await?.process;
        let context = context.into();

        self.request(|reply| Request::Step {
            process_id,
            stop_id,
            context,
            frame: StackFrameId::INNERMOST,
            kind: StepKind::IntoSource,
            call: Some(call),
            scope,
            exception,
            reply,
        })
        .await
    }

    pub async fn step_into(&self, call: VirtualAddress) -> Result<StopReason> {
        let selection = self.stopped_selection().await?;
        let mut events = self.subscribe();
        let execution = self
            .start_step_into(
                selection.stop,
                selection.execution,
                call,
                ResumeScope::Process(selection.process),
                ExceptionDisposition::Pass,
            )
            .await?;
        self.wait_for_execution(&mut events, execution).await
    }

    pub async fn step_targets(&self) -> Result<Arc<[StepTarget]>> {
        self.selected().await?.step_targets().await
    }

    pub async fn step(&self, kind: StepKind) -> Result<StopReason> {
        let selection = self.stopped_selection().await?;
        let frame = if kind == StepKind::Out || selection.interpreted {
            selection.frame
        } else {
            StackFrameId::INNERMOST
        };

        let mut events = self.subscribe();
        let execution = self
            .start_step(
                selection.stop,
                selection.execution,
                frame,
                kind,
                ResumeScope::Process(selection.process),
                ExceptionDisposition::Pass,
            )
            .await?;

        self.wait_for_execution(&mut events, execution).await
    }

    pub async fn advance(&self, spec: BreakpointSpec) -> Result<StopReason> {
        let selection = self.stopped_selection().await?;
        let mut events = self.subscribe();
        let execution = self
            .start_advance(
                selection.stop,
                selection.execution,
                selection.frame,
                spec,
                ResumeScope::Process(selection.process),
                ExceptionDisposition::Pass,
            )
            .await?;

        self.wait_for_execution(&mut events, execution).await
    }

    pub async fn start_advance(
        &self,
        stop_id: StopId,
        context: impl Into<ExecutionContext>,
        frame: StackFrameId,
        spec: BreakpointSpec,
        scope: ResumeScope,
        exception: ExceptionDisposition,
    ) -> Result<ExecutionId> {
        let process_id = self.stopped_selection().await?.process;
        let context = context.into();

        self.request(|reply| Request::Advance {
            process_id,
            stop_id,
            context,
            frame,
            spec,
            scope,
            exception,
            reply,
        })
        .await
    }

    pub async fn start_jump(
        &self,
        stop_id: StopId,
        context: impl Into<ExecutionContext>,
        spec: BreakpointSpec,
    ) -> Result<ExecutionId> {
        let process_id = self.stopped_selection().await?.process;
        let context = context.into();

        self.request(|reply| Request::Jump {
            process_id,
            stop_id,
            context,
            spec,
            reply,
        })
        .await
    }

    pub async fn jump(&self, spec: BreakpointSpec) -> Result<StopReason> {
        let selection = self.stopped_selection().await?;
        let mut events = self.subscribe();
        let execution = self
            .start_jump(selection.stop, selection.execution, spec)
            .await?;
        self.wait_for_execution(&mut events, execution).await
    }

    pub async fn pause(&self) -> Result<StopReason> {
        if self.core_dump.is_some() {
            return Err(Error::PostMortemTarget);
        }

        let snapshot = self.snapshot().await?;
        let InferiorState::Running { process_id, .. } = snapshot.inferior else {
            return Err(if snapshot.inferior.live_process().is_none() {
                Error::NotRunning
            } else {
                Error::AlreadyStopped
            });
        };

        let mut events = self.subscribe();
        let execution = self
            .request(|reply| Request::Pause { process_id, reply })
            .await?;

        self.wait_for_execution(&mut events, execution).await
    }

    pub async fn read_memory(
        &self,
        address: VirtualAddress,
        byte_count: u64,
    ) -> Result<MemoryRead> {
        let stop = self.stopped_selection().await?.stop;
        self.read_memory_at(stop, address, byte_count).await
    }

    pub async fn read_memory_at(
        &self,
        stop: StopId,
        address: VirtualAddress,
        byte_count: u64,
    ) -> Result<MemoryRead> {
        let selection = self.stopped_selection().await?;

        self.request(|reply| Request::ReadMemory {
            process_id: selection.process,
            stop_id: stop,
            address,
            byte_count,
            reply,
        })
        .await
    }

    pub async fn read_word(&self, address: VirtualAddress) -> Result<u64> {
        let read = self.read_memory(address, 8).await?;
        match read.completion {
            MemoryReadCompletion::Complete => {
                let word = read
                    .bytes
                    .as_ref()
                    .first_chunk::<8>()
                    .ok_or_else(|| Error::internal("a complete read has 8 bytes"))?;
                Ok(u64::from_le_bytes(*word))
            }
            MemoryReadCompletion::Incomplete { next_address, .. } => {
                Err(Error::MemoryNotReadable(next_address))
            }
        }
    }

    pub async fn write_word(&self, address: VirtualAddress, value: u64) -> Result<()> {
        let written = self.write_memory(address, &value.to_le_bytes()).await?;
        if written < 8 {
            return Err(Error::MemoryNotWritable(VirtualAddress::new(
                address.get() + written,
            )));
        }

        Ok(())
    }

    pub async fn write_memory(&self, address: VirtualAddress, bytes: &[u8]) -> Result<u64> {
        let stop = self.stopped_selection().await?.stop;
        self.write_memory_at(stop, address, bytes).await
    }

    pub async fn write_memory_at(
        &self,
        stop: StopId,
        address: VirtualAddress,
        bytes: &[u8],
    ) -> Result<u64> {
        let selection = self.stopped_selection().await?;

        self.request(|reply| Request::WriteMemory {
            process_id: selection.process,
            stop_id: stop,
            address,
            bytes: bytes.into(),
            reply,
        })
        .await
    }

    pub async fn runtime_address(&self, name: &str) -> Result<VirtualAddress> {
        let loaded = self.loaded_module().await?;
        let mut modules = vec![(loaded, self.module_image())];

        for record in self.loaded_modules().await?.modules.iter() {
            if record.module.id != loaded.id
                && let Ok(image) = self.loaded_module_image(record.module.id).await
            {
                modules.push((record.module, image));
            }
        }

        let mut found = BTreeSet::new();

        for (module, image) in &modules {
            let symbols = image.symbols_answering(name).collect::<Vec<_>>();

            if let Some(exported) = symbols
                .iter()
                .filter(|symbol| symbol.exported())
                .min_by_key(|symbol| (symbol.name() != name, !symbol.name().contains("@@")))
            {
                return module.virtual_address(exported.address());
            }

            for symbol in symbols {
                found.insert(module.virtual_address(symbol.address())?);
            }
        }

        let mut found = found.into_iter();

        match (found.next(), found.next()) {
            (Some(address), None) => Ok(address),
            (None, _) => Err(Error::SymbolNotFound(name.to_owned())),
            (Some(_), Some(_)) => Err(Error::DuplicateSymbol(name.to_owned())),
        }
    }

    pub async fn describe_address(&self, address: VirtualAddress) -> Result<AddressDescription> {
        let selection = self.stopped_selection().await?;

        self.request(|reply| Request::DescribeAddress {
            stop_id: selection.stop,
            address,
            reply,
        })
        .await
    }

    pub async fn disassemble(&self, query: DisassemblyQuery) -> Result<Disassembly> {
        self.selected().await?.disassemble(query).await
    }

    pub async fn current_location(&self) -> Result<ExecutionLocation> {
        self.selected().await?.location().await
    }

    pub async fn source_context(&self, radius: u32) -> Result<SourceContext> {
        self.selected().await?.source_context(radius).await
    }

    async fn source_context_at(
        &self,
        module: ModuleId,
        location: Option<SourceLocation>,
        radius: u32,
    ) -> Result<SourceContext> {
        let location = location.ok_or(Error::SourceLocationUnavailable)?;

        let file = self
            .loaded_module_image(module)
            .await?
            .source_file(location.file)
            .cloned()
            .ok_or(Error::SourceLocationUnavailable)?;

        self.source_around(file.path, location.line, location.column, radius)
            .await
    }

    async fn source_around(
        &self,
        recorded: Arc<PathBuf>,
        line: LineNumber,
        column: Option<ColumnNumber>,
        radius: u32,
    ) -> Result<SourceContext> {
        let (path, contents) = self.read_source(&recorded).await?;

        let all_lines: Vec<_> = contents.lines().collect();
        let number = line;
        let line = number.get();
        let target = usize::try_from(line)
            .ok()
            .and_then(|line| line.checked_sub(1))
            .filter(|line| *line < all_lines.len())
            .ok_or_else(|| Error::SourceLineOutOfRange {
                path: path.clone(),
                line,
            })?;

        let radius = radius as usize;
        let start = target.saturating_sub(radius);
        let end = target
            .saturating_add(radius)
            .saturating_add(1)
            .min(all_lines.len());

        let lines = all_lines
            .get(start..end)
            .ok_or_else(|| Error::internal("the source window lies within the file"))?
            .iter()
            .enumerate()
            .map(|(offset, text)| {
                Some(SourceLine {
                    number: LineNumber::new((start + offset + 1) as u64)?,
                    text: Arc::from(*text),
                })
            })
            .collect::<Option<Vec<_>>>()
            .ok_or_else(|| Error::internal("source line numbers are nonzero"))?
            .into();

        Ok(SourceContext {
            recorded,
            path: Arc::new(path),
            line: number,
            column,
            lines,
        })
    }

    pub async fn read_source_file(&self, file: &SourceFile) -> Result<(PathBuf, String)> {
        self.read_source(&file.path).await
    }

    pub async fn read_recorded_source(&self, recorded: &Path) -> Result<(PathBuf, String)> {
        self.read_source(recorded).await
    }

    async fn read_source(&self, recorded: &Path) -> Result<(PathBuf, String)> {
        let candidates = self.source_paths.candidates(recorded);

        for candidate in &candidates {
            match tokio::fs::read_to_string(candidate).await {
                Ok(contents) => return Ok((candidate.clone(), contents)),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => {
                    return Err(Error::SourceFileRead {
                        path: candidate.clone(),
                        error,
                    });
                }
            }
        }

        Err(Error::SourceFileMissing {
            path: recorded.to_owned(),
            tried: candidates,
        })
    }

    pub async fn snapshot(&self) -> Result<StateSnapshot> {
        self.request(|reply| Request::Snapshot { reply }).await
    }

    pub async fn tasks(&self, from: Option<TaskCursor>, limit: usize) -> Result<TaskPage> {
        self.task_page(from, limit, false).await
    }

    pub async fn program_tasks(&self, from: Option<TaskCursor>, limit: usize) -> Result<TaskPage> {
        self.task_page(from, limit, true).await
    }

    pub async fn program_tasks_at(
        &self,
        stop: StopId,
        from: Option<TaskCursor>,
        limit: usize,
    ) -> Result<TaskPage> {
        self.request(|reply| Request::Tasks {
            stop_id: stop,
            from,
            limit,
            program_only: true,
            reply,
        })
        .await
    }

    async fn task_page(
        &self,
        from: Option<TaskCursor>,
        limit: usize,
        program_only: bool,
    ) -> Result<TaskPage> {
        let selection = self.stopped_selection().await?;
        self.request(|reply| Request::Tasks {
            stop_id: selection.stop,
            from,
            limit,
            program_only,
            reply,
        })
        .await
    }

    pub async fn backtrace(&self) -> Result<Backtrace> {
        self.selected().await?.backtrace().await
    }

    pub async fn registers(&self) -> Result<RegisterSnapshot> {
        self.selected().await?.registers().await
    }

    pub async fn variables(&self) -> Result<VariableSnapshot> {
        self.variables_with_limits(InspectionLimits::default())
            .await
    }

    pub async fn variables_with_limits(
        &self,
        limits: InspectionLimits,
    ) -> Result<VariableSnapshot> {
        self.selected().await?.variables_with_limits(limits).await
    }

    pub async fn variable(&self, name: impl Into<String>) -> Result<Variable> {
        let name = name.into();
        let snapshot = self
            .selected()
            .await?
            .variable_query(
                VariableQuery::Name(name.clone()),
                InspectionLimits::default(),
            )
            .await?;

        snapshot
            .variables
            .first()
            .cloned()
            .ok_or(Error::VariableNotFound(name))
    }

    pub async fn evaluate(&self, expression: &Expression) -> Result<Evaluation> {
        self.selected().await?.evaluate(expression).await
    }

    pub async fn evaluate_with(
        &self,
        expression: &Expression,
        mode: EvaluationMode,
        limits: InspectionLimits,
    ) -> Result<Evaluation> {
        self.selected()
            .await?
            .evaluate_with(expression, mode, limits)
            .await
    }

    pub async fn expression_type(&self, expression: &Expression) -> Result<TypeInfo> {
        self.selected().await?.expression_type(expression).await
    }

    pub async fn explain_view(&self, expression: &Expression) -> Result<ViewExplanation> {
        self.selected().await?.explain_view(expression).await
    }

    pub async fn record_kernels(&self, expression: &Expression) -> Result<Vec<String>> {
        self.selected().await?.record_kernels(expression).await
    }

    pub async fn load_views(
        &self,
        files: &[(&str, &str)],
        kernels: &[view_files::KernelFile],
        renderers: &[view_files::RendererFile],
    ) -> Result<Arc<[ViewFileError]>> {
        let mut views = view::ViewSet::new(files.iter().copied());
        for renderer in renderers {
            views.add_renderers(
                &renderer.path,
                [(renderer.name.as_str(), renderer.source.as_str())],
            );
        }

        for kernel in kernels {
            views.add_kernels(
                &kernel.path,
                [(
                    kernel.name.as_str(),
                    kernel.path.as_str(),
                    kernel.module.as_slice(),
                )],
            );
        }

        let views = Arc::new(views);
        let errors: Arc<[ViewFileError]> = views.errors().into();

        self.request(|reply| Request::SetViews { views, reply })
            .await?;

        Ok(errors)
    }

    pub async fn explain_type(&self, name: &str) -> Result<Vec<TypeViews>> {
        let name = name.to_owned();
        self.request(|reply| Request::ExplainType { name, reply })
            .await
    }

    pub async fn check_views(&self) -> Result<ViewCheck> {
        self.request(|reply| Request::CheckViews { reply }).await
    }

    pub async fn enable_views(&self, enabled: bool) -> Result<()> {
        self.request(|reply| Request::EnableViews { enabled, reply })
            .await
    }

    pub async fn set_step_into_runtime(&self, enter: bool) -> Result<bool> {
        self.request(|reply| Request::SetStepIntoRuntime { enter, reply })
            .await
    }

    pub async fn set_step_skips(&self, skips: StepSkips) -> Result<()> {
        self.request(|reply| Request::SetStepSkips { skips, reply })
            .await
    }

    pub async fn inspect(&self, expression: &Expression) -> Result<InspectedValue> {
        self.inspect_with_limits(expression, InspectionLimits::default())
            .await
    }

    pub async fn inspect_with_limits(
        &self,
        expression: &Expression,
        limits: InspectionLimits,
    ) -> Result<InspectedValue> {
        self.selected()
            .await?
            .inspect_with_limits(expression, limits)
            .await
    }

    pub async fn main_global(&self, id: GlobalVariableId) -> Result<Variable> {
        let module = self.loaded_module().await?;

        self.loaded_global(GlobalVariableReference {
            module: module.id,
            image: module.image,
            variable: id,
        })
        .await
    }

    pub async fn loaded_global(&self, global: GlobalVariableReference) -> Result<Variable> {
        self.selected()
            .await?
            .global_with_limits(global, InspectionLimits::default())
            .await
    }

    pub async fn dereference(
        &self,
        reference: Box<DereferenceReference>,
    ) -> Result<DereferencedValue> {
        self.dereference_with_limits(reference, InspectionLimits::default())
            .await
    }

    pub async fn dereference_with_limits(
        &self,
        reference: Box<DereferenceReference>,
        limits: InspectionLimits,
    ) -> Result<DereferencedValue> {
        self.request(|reply| Request::Dereference {
            reference,
            limits,
            reply,
        })
        .await
    }

    pub async fn value_children(
        &self,
        reference: Arc<ValueChildrenReference>,
        query: ValueChildQuery,
    ) -> Result<ValueChildPage> {
        self.value_children_with_limits(reference, query, InspectionLimits::default())
            .await
    }

    pub async fn value_children_with_limits(
        &self,
        reference: Arc<ValueChildrenReference>,
        query: ValueChildQuery,
        limits: InspectionLimits,
    ) -> Result<ValueChildPage> {
        self.request(|reply| Request::ValueChildren {
            reference,
            query,
            limits,
            reply,
        })
        .await
    }

    pub async fn visualizer_inputs(
        &self,
        reference: Arc<VisualizerReference>,
        limits: InspectionLimits,
    ) -> Result<VisualizerInputs> {
        self.request(|reply| Request::VisualizerInputs {
            reference,
            limits,
            reply,
        })
        .await
    }

    pub async fn numbers(
        &self,
        reference: Arc<ValueChildrenReference>,
        most_bytes: u64,
    ) -> Result<Option<Numbers>> {
        self.request(|reply| Request::Numbers {
            reference,
            most_bytes,
            reply,
        })
        .await
    }

    pub async fn renderers(&self) -> Result<Arc<[Arc<Renderer>]>> {
        self.request(|reply| Request::Renderers { reply }).await
    }

    pub async fn globals(&self, query: GlobalVariableQuery) -> Result<GlobalVariablePage> {
        self.request(|reply| Request::Globals { query, reply })
            .await
    }

    pub async fn loaded_modules(&self) -> Result<LoadedModuleSnapshot> {
        self.request(|reply| Request::LoadedModules { reply }).await
    }

    pub async fn loaded_module_image(&self, module: ModuleId) -> Result<Arc<ModuleImage>> {
        self.request(|reply| Request::ModuleImage { module, reply })
            .await
    }

    pub async fn select_frame(&self, frame: StackFrameId) -> Result<StackFrame> {
        let selection = self.stopped_selection().await?;

        self.request(|reply| Request::SelectFrame {
            stop_id: selection.stop,
            context: selection.execution,
            frame,
            reply,
        })
        .await
    }

    pub async fn select_context(&self, context: impl Into<ExecutionContext>) -> Result<()> {
        let selection = self.stopped_selection().await?;
        let context = context.into();

        self.request(|reply| Request::SelectContext {
            stop_id: selection.stop,
            context,
            reply,
        })
        .await
    }

    async fn loaded_module(&self) -> Result<LoadedModule> {
        self.request(|reply| Request::LoadedModule { reply }).await
    }

    async fn selected(&self) -> Result<StopView<'_>> {
        let selection = self.stopped_selection().await?;
        Ok(self.at(StopContext {
            stop: selection.stop,
            execution: selection.execution,
            frame: selection.frame,
        }))
    }

    #[must_use]
    pub const fn at(&self, context: StopContext) -> StopView<'_> {
        StopView {
            handle: self,
            context,
        }
    }

    async fn stopped_selection(&self) -> Result<StoppedSelection> {
        self.request(|reply| Request::StoppedSelection { reply })
            .await
    }

    pub(crate) async fn ended(&self) {
        self.requests.closed().await;
    }

    #[cfg(any(test, feature = "sim"))]
    pub(crate) fn has_ended(&self) -> bool {
        self.requests.is_closed()
    }

    async fn wait_for_execution(
        &self,
        events: &mut broadcast::Receiver<DebuggerEvent>,
        execution: ExecutionId,
    ) -> Result<StopReason> {
        loop {
            let event = tokio::select! {
                biased;
                event = events.recv() => event,
                () = self.requests.closed() => return Err(Error::RequestCancelled),
            };

            let lagged = match event {
                Ok(event) => match execution_end(event, execution) {
                    Some(end) => return end,
                    None => continue,
                },
                Err(broadcast::error::RecvError::Closed) => return Err(Error::RequestCancelled),
                Err(broadcast::error::RecvError::Lagged(count)) => count,
            };

            if let InferiorState::Running {
                execution_id: Some(running),
                ..
            } = self.snapshot().await?.inferior
                && running == execution
            {
                continue;
            }

            loop {
                match events.try_recv() {
                    Ok(event) => {
                        if let Some(end) = execution_end(event, execution) {
                            return end;
                        }
                    }
                    Err(broadcast::error::TryRecvError::Lagged(_)) => {}
                    Err(broadcast::error::TryRecvError::Closed) => {
                        return Err(Error::RequestCancelled);
                    }
                    Err(broadcast::error::TryRecvError::Empty) => {
                        return Err(Error::EventStreamLagged(lagged));
                    }
                }
            }
        }
    }

    async fn request<T>(
        &self,
        make: impl FnOnce(oneshot::Sender<Result<T>>) -> Request,
    ) -> Result<T> {
        let (send, receive) = oneshot::channel();

        self.requests
            .send(ControllerMessage::Request(make(send)))
            .await
            .map_err(|_| Error::RequestQueueClosed)?;

        receive.await.map_err(|_| Error::ReplyDropped)?
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StopContext {
    pub stop: StopId,

    pub execution: ExecutionContext,

    pub frame: StackFrameId,
}

#[derive(Clone, Copy)]
pub struct StopView<'a> {
    handle: &'a DebuggerHandle,
    context: StopContext,
}

impl StopView<'_> {
    pub async fn backtrace(&self) -> Result<Backtrace> {
        let context = self.context;
        self.handle
            .request(|reply| Request::Backtrace {
                stop_id: context.stop,
                context: context.execution,
                reply,
            })
            .await
    }

    pub async fn registers(&self) -> Result<RegisterSnapshot> {
        let context = self.context;
        self.handle
            .request(|reply| Request::Registers {
                stop_id: context.stop,
                context: context.execution,
                frame: context.frame,
                reply,
            })
            .await
    }

    pub async fn step_targets(&self) -> Result<Arc<[StepTarget]>> {
        let context = self.context;
        if context.frame != StackFrameId::INNERMOST {
            return Err(Error::FrameStepUnsupported(
                "only the innermost frame's line can be stepped into".into(),
            ));
        }

        self.handle
            .request(|reply| Request::StepTargets {
                stop_id: context.stop,
                context: context.execution,
                reply,
            })
            .await
    }

    pub async fn location(&self) -> Result<ExecutionLocation> {
        let context = self.context;
        self.handle
            .request(|reply| Request::StoppedLocation {
                stop_id: context.stop,
                context: context.execution,
                frame: context.frame,
                reply,
            })
            .await
    }

    pub async fn source_context(&self, radius: u32) -> Result<SourceContext> {
        let (module, source) = match self.location().await {
            Ok(location) if location.image.source.is_some() => {
                (location.module, location.image.source)
            }
            Ok(location) => {
                let frame = self.selected_trace_frame().await?;
                if let Some(context) = self.recorded_source(&frame, radius).await {
                    return context;
                }
                (location.module, location.image.source)
            }
            Err(Error::FrameSuspended | Error::FrameInterpreted) => {
                let frame = self.selected_trace_frame().await?;
                if let Some(context) = self.recorded_source(&frame, radius).await {
                    return context;
                }
                (
                    frame.module.ok_or(Error::SourceLocationUnavailable)?,
                    frame.source.clone(),
                )
            }
            Err(error) => return Err(error),
        };

        self.handle.source_context_at(module, source, radius).await
    }

    async fn selected_trace_frame(&self) -> Result<StackFrame> {
        let trace = self.backtrace().await?;
        trace
            .frames
            .iter()
            .find(|frame| frame.id == self.context.frame)
            .cloned()
            .ok_or(Error::SourceLocationUnavailable)
    }

    async fn recorded_source(
        &self,
        frame: &StackFrame,
        radius: u32,
    ) -> Option<Result<SourceContext>> {
        let place = frame.recorded.as_ref()?.place.as_ref()?;
        let Some(line) = LineNumber::new(u64::from(place.line)) else {
            return Some(Err(Error::SourceLocationUnavailable));
        };
        let column = ColumnNumber::new(u64::from(place.column));
        if !place.names_file() {
            return Some(Err(Error::SourceNotAFile {
                name: Arc::clone(&place.path),
            }));
        }
        let recorded = Arc::new(PathBuf::from(&*place.path));
        Some(
            self.handle
                .source_around(recorded, line, column, radius)
                .await,
        )
    }

    pub async fn variables(&self) -> Result<VariableSnapshot> {
        self.variables_with_limits(InspectionLimits::default())
            .await
    }

    pub async fn variables_with_limits(
        &self,
        limits: InspectionLimits,
    ) -> Result<VariableSnapshot> {
        self.variable_query(VariableQuery::All, limits).await
    }

    pub async fn global_with_limits(
        &self,
        global: GlobalVariableReference,
        limits: InspectionLimits,
    ) -> Result<Variable> {
        let snapshot = self
            .variable_query(VariableQuery::Global(global), limits)
            .await?;

        snapshot
            .variables
            .first()
            .cloned()
            .ok_or_else(|| Error::VariableNotFound(global.variable.to_string()))
    }

    async fn variable_query(
        &self,
        query: VariableQuery,
        limits: InspectionLimits,
    ) -> Result<VariableSnapshot> {
        let context = self.context;
        self.handle
            .request(|reply| Request::Variables {
                query,
                limits,
                stop_id: context.stop,
                context: context.execution,
                frame: context.frame,
                reply,
            })
            .await
    }

    pub async fn evaluate(&self, expression: &Expression) -> Result<Evaluation> {
        self.evaluate_with(
            expression,
            EvaluationMode::Read,
            InspectionLimits::default(),
        )
        .await
    }

    pub async fn evaluate_with(
        &self,
        expression: &Expression,
        mode: EvaluationMode,
        limits: InspectionLimits,
    ) -> Result<Evaluation> {
        let context = self.context;
        let expression = expression.clone();

        self.handle
            .request(|reply| Request::Evaluate {
                expression,
                mode,
                limits,
                stop_id: context.stop,
                context: context.execution,
                frame: context.frame,
                reply,
            })
            .await
    }

    pub async fn explain_view(&self, expression: &Expression) -> Result<ViewExplanation> {
        let context = self.context;
        let expression = expression.clone();

        self.handle
            .request(|reply| Request::ExplainView {
                expression,
                stop_id: context.stop,
                context: context.execution,
                frame: context.frame,
                reply,
            })
            .await
    }

    pub async fn record_kernels(&self, expression: &Expression) -> Result<Vec<String>> {
        let context = self.context;
        let expression = expression.clone();

        self.handle
            .request(|reply| Request::RecordKernels {
                expression,
                stop_id: context.stop,
                context: context.execution,
                frame: context.frame,
                reply,
            })
            .await
    }

    pub async fn expression_type(&self, expression: &Expression) -> Result<TypeInfo> {
        let context = self.context;
        let expression = expression.clone();

        self.handle
            .request(|reply| Request::ExpressionType {
                expression,
                stop_id: context.stop,
                context: context.execution,
                frame: context.frame,
                reply,
            })
            .await
    }

    pub async fn inspect_with_limits(
        &self,
        expression: &Expression,
        limits: InspectionLimits,
    ) -> Result<InspectedValue> {
        match self
            .evaluate_with(expression, EvaluationMode::Read, limits)
            .await?
        {
            Evaluation::Value { value, .. } => Ok(value),
            _ => Err(Error::InvalidValueRange(
                "a range has no single value; evaluate it for its elements".into(),
            )),
        }
    }

    pub async fn resolve_watch_target(&self, expression: &Expression) -> Result<WatchTarget> {
        let context = self.context;
        let expression = expression.clone();

        self.handle
            .request(|reply| Request::ResolveWatchTarget {
                expression,
                stop_id: context.stop,
                context: context.execution,
                frame: context.frame,
                reply,
            })
            .await
    }

    pub async fn disassemble(&self, query: DisassemblyQuery) -> Result<Disassembly> {
        let context = self.context;
        self.handle
            .request(|reply| Request::Disassemble {
                query,
                stop_id: context.stop,
                context: context.execution,
                reply,
            })
            .await
    }
}

#[derive(Clone, Copy)]
pub(crate) struct StoppedSelection {
    pub(crate) process: ProcessId,
    pub(crate) stop: StopId,
    pub(crate) execution: ExecutionContext,
    pub(crate) frame: StackFrameId,

    pub(crate) interpreted: bool,
}

#[cfg(test)]
mod shutdown_tests {
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::thread::{self, JoinHandle};

    use tokio::sync::{mpsc, watch};

    use super::{ControllerMessage, Debugger, Error, Request, backend};

    fn debugger(
        start: impl FnOnce(mpsc::Receiver<ControllerMessage>) -> JoinHandle<()>,
    ) -> Debugger {
        Debugger::start(Arc::new(PathBuf::from("/test")), |channels| {
            let image = crate::ModuleImage::new(
                PathBuf::from("/test"),
                crate::TargetDescription {
                    architecture: crate::Architecture::X86_64,
                    byte_order: crate::ByteOrder::Little,
                    pointer_width: crate::PointerWidth::Bits64,
                },
                crate::AddressRange {
                    start: crate::ImageAddress::new(0),
                    end: crate::ImageAddress::new(0x100),
                },
                crate::model::ModuleMetadata::default(),
            );
            let (_, program) = watch::channel(Arc::new(image));
            let controller = backend::ControllerJoin::Thread(start(channels.receiver));
            Ok((program, None, controller))
        })
        .expect("start the debugger")
    }

    #[tokio::test]
    async fn a_shutdown_answered_after_its_queue_closed_succeeds() {
        let debugger = debugger(|mut queue| {
            thread::spawn(move || {
                queue.close();
                while let Some(message) = queue.blocking_recv() {
                    if let ControllerMessage::Request(Request::Shutdown { reply }) = message {
                        let _ = reply.send(Ok(()));
                    }
                }
            })
        });
        assert!(matches!(debugger.shutdown().await, Ok(())));
    }

    #[tokio::test]
    async fn a_shutdown_dropped_unanswered_is_cancelled() {
        let debugger = debugger(|mut queue| thread::spawn(move || drop(queue.blocking_recv())));
        assert!(matches!(
            debugger.shutdown().await,
            Err(Error::RequestCancelled)
        ));
    }

    #[tokio::test]
    async fn a_shutdown_after_the_controller_panicked_says_so() {
        let debugger = debugger(|queue| {
            drop(queue);
            thread::spawn(|| panic!("the controller panics"))
        });
        assert!(matches!(
            debugger.shutdown().await,
            Err(Error::BackendThreadPanicked)
        ));
    }
}
