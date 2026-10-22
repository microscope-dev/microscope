// Production code returns errors instead of panicking; see dev/assertions.md.
// Tests may unwrap.
#![cfg_attr(
    not(test),
    deny(
        clippy::expect_used,
        clippy::panic,
        clippy::todo,
        clippy::unimplemented,
        clippy::unreachable,
        clippy::unwrap_used
    )
)]

/// Appends a line to the flight recorder of a development build. Release
/// builds drop the arguments unevaluated, so they may use code that only
/// development builds compile.
macro_rules! record {
    ($($argument:tt)*) => {{
        #[cfg(debug_assertions)]
        $crate::flight_recorder::record(::std::format_args!($($argument)*));
    }};
}

// The library's own tests share support code with the integration tests,
// which name the library `microscope`.
#[cfg(test)]
extern crate self as microscope;

mod analysis;
mod backend;
pub(crate) mod capture;

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

// The simulator is a test harness, which may panic.
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
mod sync;
pub mod syscalls;

/// The engines that trace system calls into [`syscalls`]' events.
pub use backend::syscall_engines;
#[cfg(test)]
mod test_memory;
pub mod timings;

/// What the tests of microscope's unsafe code reach it through. They are a
/// test target of their own, `tests/miri.rs`, so that Miri checks only
/// them, not the whole crate, each time it runs them.
#[doc(hidden)]
pub mod unsafe_parts {
    pub use crate::backend::perf::sys::{DATA_HEAD, DATA_TAIL, HEAP_DATA, RingMemory};
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
    TextSummary, ThreadActivity, ThreadId, ThreadLocal, TlsUnavailableReason, TrapAddress,
    TypeArgument, TypeId, TypeIdentity, TypeInfo, TypeKind, TypeModifier, TypeNode, TypeReference,
    Unfollowed, UnsupportedVariableFeature, UnwindTermination, ValueAccessUnavailableReason,
    ValueBitRange, ValueChild, ValueChildPage, ValueChildRelationship, ValueChildren,
    ValueChildrenReference, Variable, VariableInvalidReason, VariableKind, VariableMalformedKind,
    VariableMalformedReason, VariableSnapshot, VariableState, VariableUnavailableReason,
    VariableValue, VariableValueSource, Variant, VariantDiscriminant, VariantSelection,
    VariantSelector, VariantStorageKind, ViewName, ViewProblem, VirtualAddress, Visualizer,
    VisualizerReference,
};

pub use group::{GroupEvent, GroupHandle, Member, MemberNumber, MemberState, ProcessGroup};
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

pub use runtime_model::TASK_NOUNS;
pub use source_map::SourcePathMap;
pub use view::summary::{
    Characters, characters as characters_of, float as float_text, function as function_text,
    integer as integer_text, quoted as quoted_text, scalar as scalar_text, symbol as symbol_text,
    value as value_summary,
};

pub use view::syntax::Error as ViewFileError;

/// The views built into microscope, in the order they are tried.
#[must_use]
pub fn built_in_views() -> Vec<Arc<ViewName>> {
    view::ViewSet::built_in()
        .views()
        .iter()
        .map(|view| view::name_of(view))
        .collect()
}

/// One recorded kernel run, replayed without a program.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplayedRun {
    pub kernel: String,

    /// The reads and items the recording holds.
    pub events: usize,

    /// How many of them the kernel reproduced, or the first difference.
    pub outcome: std::result::Result<usize, String>,
}

/// Replays the kernel runs `text` records, as `record_kernels` writes them,
/// with the kernel module `wasm` when one is given, and otherwise with the
/// built-in kernel each run names.
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

/// Whether a view is one microscope builds in, rather than one loaded for the
/// session or carried by a module.
#[must_use]
pub fn is_built_in_view(name: &ViewName) -> bool {
    view::ViewSet::built_in()
        .views()
        .iter()
        .any(|view| view.source == name.source && view.line == name.line)
}

/// The processes `name` names exactly.
///
/// A name matches the command name the platform records, the file name the
/// process was started as, a title it set, or the file name of a script it
/// runs. The calling process is never one of them.
pub fn processes_named(name: &str) -> Result<Vec<NamedProcess>> {
    backend::processes_named(name)
}

/// Finds a signal's exception code by name, with or without its `SIG`
/// prefix and in any case, or by number: `SIGUSR1`, `usr1`, `10`, `SIG34`.
#[must_use]
pub fn signal_named(name: &str) -> Option<u64> {
    backend::signal_named(name)
}

/// Names the signal with an exception code, such as `SIGSEGV`. Real-time
/// signals are named by number, as gdb does: `SIG34`.
#[must_use]
pub fn signal_name(code: u64) -> Option<String> {
    backend::signal_name(code)
}

/// The exception codes of every signal this target defines, in order.
pub fn signal_codes() -> impl Iterator<Item = u64> {
    backend::signal_codes()
}

/// Exercises core-dump parsing and memory reads for the fuzz harness.
#[cfg(feature = "fuzzing")]
#[doc(hidden)]
pub fn fuzz_core_dump(data: &[u8]) {
    backend::fuzz_core_dump(data);
}

/// Exercises image validation, and every lookup in an image it accepts,
/// for the fuzz harness.
#[cfg(feature = "fuzzing")]
#[doc(hidden)]
pub fn fuzz_image(data: &[u8]) {
    image::sample::fuzz(data);
}

/// Decodes perf ring records for the fuzz harness.
#[cfg(feature = "fuzzing")]
#[doc(hidden)]
pub fn fuzz_perf_records(data: &[u8]) {
    backend::perf::decode::fuzz(data);
}

/// Reads a capture file, and queries one it accepts, for the fuzz harness.
#[cfg(feature = "fuzzing")]
#[doc(hidden)]
pub fn fuzz_capture(data: &[u8]) {
    if let Ok(read) = capture::file::read(data) {
        let keys = analysis::Keys::new(&read.capture, analysis::Granularity::Function);
        let everything = analysis::Query::default();
        let _ = analysis::aggregate(&read.capture, &keys, &everything);
        let _ = analysis::functions(&read.capture, &keys, &everything);
    }
}

/// Reads a profile in any format microscope imports, and writes one it accepts
/// in every format, for the fuzz harness.
#[cfg(feature = "fuzzing")]
#[doc(hidden)]
pub fn fuzz_profile_import(data: &[u8]) {
    if let Ok(read) = profiler::Profile::import(data) {
        for format in profiler::Format::ALL {
            let _ = read.profile.export(format, profiler::StackView::Logical);
        }
    }
}

/// Exercises debug-register planning invariants for the fuzz harness.
#[cfg(feature = "fuzzing")]
#[doc(hidden)]
pub fn fuzz_debug_register_plan(data: &[u8]) {
    backend::fuzz_debug_register_plan(data);
}

/// Locates glibc TLS blocks in loader state made of arbitrary bytes, for
/// the fuzz harness.
#[cfg(feature = "fuzzing")]
#[doc(hidden)]
pub fn fuzz_glibc_tls(data: &[u8]) {
    backend::fuzz_glibc_tls(data);
}

/// Exercises ELF symbol-table normalization and symbol lookup invariants for
/// the fuzz harness.
#[cfg(feature = "fuzzing")]
#[doc(hidden)]
pub fn fuzz_elf_symbols(data: &[u8]) {
    debug_info::fuzz_elf_symbols(data);
}

/// Exercises the decoding of a coroutine's dispatch on its state on
/// hostile code for the fuzz harness.
#[cfg(all(feature = "fuzzing", target_arch = "x86_64"))]
#[doc(hidden)]
pub fn fuzz_dispatch(data: &[u8]) {
    debug_info::fuzz_dispatch(data);
}

/// Exercises Go function-table decoding on hostile bytes for the fuzz
/// harness.
#[cfg(feature = "fuzzing")]
#[doc(hidden)]
pub fn fuzz_gopclntab(data: &[u8]) {
    debug_info::fuzz_gopclntab(data);
}

/// Exercises disassembly boundary and decoding invariants for the fuzz
/// harness.
#[cfg(feature = "fuzzing")]
#[doc(hidden)]
pub fn fuzz_disassembly(data: &[u8]) {
    disassembly::fuzz(data);
}

/// Runs every built-in view, and a view file made of the input's tail,
/// over memory made of its bytes, for the fuzz harness.
#[cfg(feature = "fuzzing")]
#[doc(hidden)]
pub fn fuzz_views(data: &[u8]) {
    view::fuzz::hostile(data);
}

/// Checks the expression parser's invariants on `text` for the fuzz
/// harness, panicking when one fails.
#[cfg(feature = "fuzzing")]
#[doc(hidden)]
#[allow(clippy::panic, reason = "a fuzz harness reports a bug by panicking")]
pub fn fuzz_expression_parse(text: &str) {
    if let Err(failure) = eval::syntax::check_invariants(text) {
        panic!("{failure}");
    }
}

/// Exercises bounded DWARF-expression parsing for the fuzz harness.
#[cfg(feature = "fuzzing")]
#[doc(hidden)]
pub fn fuzz_dwarf_expression(data: &[u8]) {
    debug_info::fuzz_dwarf_expression(data);
}

/// How `event` ends `execution`, if it does: with a stop or an exit, or, as
/// a detach does any execution, cancelled.
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

/// The number of events a subscriber may fall behind by before it misses
/// some. `MICROSCOPE_EVENT_CAPACITY` lowers it so tests can make clients lag.
fn event_capacity() -> usize {
    std::env::var("MICROSCOPE_EVENT_CAPACITY")
        .ok()
        .and_then(|capacity| capacity.parse().ok())
        .filter(|capacity| *capacity != 0)
        .unwrap_or(EVENT_CAPACITY)
}

/// A debug session, owning the controller thread that serves its requests.
///
/// Dropping a debugger requests shutdown without waiting for it; prefer
/// [`Debugger::shutdown`], which reports whether cleanup succeeded.
pub struct Debugger {
    handle: DebuggerHandle,
    controller: Option<backend::ControllerJoin>,
    shutdown_permit: Option<mpsc::OwnedPermit<ControllerMessage>>,
}

/// A clonable client of a running [`Debugger`].
///
/// Requests are queued to the controller, which acknowledges each one.
/// Execution control is acknowledged before the stop it eventually causes;
/// subscribe to events, or use the methods that wait, to observe the stop.
#[derive(Clone)]
pub struct DebuggerHandle {
    /// The program the process runs, which an exec replaces.
    program: watch::Receiver<Arc<ModuleImage>>,

    /// The program the session was made for, which a launch executes.
    launched: Arc<PathBuf>,
    core_dump: Option<Arc<CoreDumpInfo>>,
    source_paths: Arc<SourcePathMap>,
    requests: mpsc::Sender<ControllerMessage>,
    events: broadcast::Sender<DebuggerEvent>,
}

/// A native executable and its debug information, read once.
///
/// Every debugger made from it with [`Debugger::for_program`] shares what
/// was read, which is immutable, so debugging the same program again does
/// not read it again.
#[derive(Clone)]
pub struct Program {
    executable: backend::Executable,
    debug_info: debug_info::DebugInfo,
}

impl Program {
    /// Reads a native executable and its debug information, finding its
    /// modules' separate debug files as `debug_files` says.
    pub fn load(executable: impl AsRef<Path>, debug_files: &DebugFileOptions) -> Result<Self> {
        let mut executable = backend::executable_source(executable.as_ref())?;
        executable.debug_files = debug_info::DebugFileSearch::new(debug_files);
        Self::from_executable_source(executable)
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
    /// Creates a debugger for a native executable and starts its backend controller.
    pub fn new(executable: impl AsRef<Path>) -> Result<Self> {
        Self::new_with(executable, &DebugFileOptions::default())
    }

    /// Creates a debugger for a native executable whose modules' separate
    /// debug files are found as `debug_files` says.
    pub fn new_with(executable: impl AsRef<Path>, debug_files: &DebugFileOptions) -> Result<Self> {
        Self::for_program(Program::load(executable, debug_files)?)
    }

    /// Creates a debugger for a program already read and starts its
    /// backend controller.
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

    /// Creates the root session of a process group for `program`, whose
    /// tracer thread follows the processes it starts as `policy` says.
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

    /// The debugger of a process that joined a group, and the stop it
    /// joins in, which it publishes before it runs an instruction.
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

    /// Attaches to an existing local process and returns once it is coherently stopped.
    pub async fn attach(process: ProcessId) -> Result<Self> {
        Self::attach_with(process, None, &DebugFileOptions::default()).await
    }

    /// Attaches using an explicitly supplied executable when automatic discovery is unavailable.
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

    /// Attaches to an existing local process, reading its executable from
    /// `executable` when given, and finding its modules' separate debug
    /// files as `debug_files` says.
    pub async fn attach_with(
        process: ProcessId,
        executable: Option<&Path>,
        debug_files: &DebugFileOptions,
    ) -> Result<Self> {
        let mut executable = match executable {
            Some(executable) => backend::executable_source(executable)?,
            None => backend::process_executable_source(process)?,
        };

        executable.debug_files = debug_info::DebugFileSearch::new(debug_files);

        Self::attach_from_source(process, executable).await
    }

    async fn attach_from_source(
        process: ProcessId,
        executable: backend::ExecutableSource,
    ) -> Result<Self> {
        let debugger = Self::for_program(Program::from_executable_source(executable)?)?;
        let attached = debugger
            .handle
            .request(|reply| Request::Attach {
                process_id: process,
                reply,
            })
            .await;
        if let Err(error) = attached {
            let _ = debugger.shutdown().await;
            return Err(error);
        }

        Ok(debugger)
    }

    /// Opens a post-mortem core dump as one permanent stopped snapshot.
    ///
    /// Every recorded module file must be proven to match the dump unless
    /// [`CoreDumpOptions::allow_module_mismatch`] is set. The files of a dump
    /// written on another machine are found through
    /// [`CoreDumpOptions::sysroot`] and [`CoreDumpOptions::module_paths`].
    /// Execution control, memory writes, and breakpoints fail with
    /// [`Error::PostMortemTarget`].
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

    /// Creates the request and event channels and starts a controller that
    /// serves them, reserving request capacity so shutdown can always be sent.
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

    /// Returns a clonable handle for sending requests to the debugger.
    #[must_use]
    pub fn handle(&self) -> DebuggerHandle {
        self.handle.clone()
    }

    /// Ends the session and joins the backend controller. A launched inferior
    /// is killed and reaped; an attached one is detached and left running.
    pub async fn shutdown(mut self) -> Result<()> {
        let (send, mut receive) = oneshot::channel();
        self.shutdown_permit
            .take()
            .ok_or_else(|| Error::internal("the shutdown permit is present"))?
            .send(ControllerMessage::Request(Request::Shutdown {
                reply: send,
            }));

        // A controller that ended, as a panic ends it, closes its queue
        // with the request in it unanswered. One that answered closed it
        // after, so the answer is there to take, even when the runtime's
        // budget held back the wait for it.
        let replied = timeout(SHUTDOWN_TIMEOUT, async {
            tokio::select! {
                biased;
                replied = &mut receive => replied.ok(),
                () = self.handle.requests.closed() => receive.try_recv().ok(),
            }
        });
        let result = match replied.await {
            Ok(Some(result)) => result,
            Ok(None) => Err(Error::RequestCancelled),
            Err(_) => return Err(Error::ShutdownTimedOut),
        };

        if let Some(controller) = self.controller.take() {
            tokio::task::spawn_blocking(move || controller.join())
                .await
                .map_err(|_| Error::BackendThreadPanicked)?
                .map_err(|()| Error::BackendThreadPanicked)?;
        }

        result
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
    /// Returns this handle reading source files through `source_paths`.
    /// Other handles of the same debugger keep their own maps.
    #[must_use]
    pub fn with_source_paths(mut self, source_paths: SourcePathMap) -> Self {
        self.source_paths = Arc::new(source_paths);
        self
    }

    /// Returns the canonical path of the program the process runs, which
    /// an exec into another program changes.
    #[must_use]
    pub fn executable(&self) -> PathBuf {
        self.program.borrow().path().to_owned()
    }

    /// Returns the program the session was made for, which a launch
    /// executes: a script rather than its interpreter, for one.
    #[must_use]
    pub fn launched(&self) -> &Path {
        &self.launched
    }

    /// Returns the immutable debug metadata of the program the process
    /// runs, which an exec into another program replaces, as
    /// [`DebuggerEvent::ProgramChanged`] reports.
    #[must_use]
    pub fn module_image(&self) -> Arc<ModuleImage> {
        Arc::clone(&self.program.borrow())
    }

    /// Describes the opened core dump, or `None` for a live session.
    #[must_use]
    pub const fn core_dump(&self) -> Option<&Arc<CoreDumpInfo>> {
        self.core_dump.as_ref()
    }

    /// Subscribes to debugger state and lifecycle events.
    #[must_use]
    pub fn subscribe(&self) -> broadcast::Receiver<DebuggerEvent> {
        self.events.subscribe()
    }

    /// Adds a logical breakpoint that stops at every hit and returns all
    /// locations resolved by the backend. Adding the spec of an existing
    /// breakpoint without a hit condition returns that breakpoint.
    pub async fn add_breakpoint(&self, spec: BreakpointSpec) -> Result<Breakpoint> {
        self.add_breakpoint_with(spec, BreakpointOptions::default())
            .await
    }

    /// Adds a logical breakpoint that stops or logs at the hits `options`
    /// select. Adding the spec and options of an existing breakpoint
    /// returns that breakpoint, with the hits it already counted.
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

    /// Adds a logical breakpoint that stops only at hits meeting
    /// `hit_condition`. Adding the spec and condition of an existing
    /// breakpoint returns that breakpoint, with the hits it already counted.
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

    /// Replaces which hits of a breakpoint stop execution; `None` stops at
    /// every hit. The hits already counted are kept. Unlike adding and
    /// removing breakpoints, this needs no stop and works while the inferior
    /// runs, taking effect from the next hit.
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

    /// Replaces a breakpoint's condition; `None` removes it. Like a hit
    /// condition, this needs no stop and applies from the next hit.
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

    /// Replaces the message a breakpoint logs instead of stopping; `None`
    /// makes it stop again. Like a condition, this needs no stop and applies
    /// from the next hit.
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

    /// Enables or disables a breakpoint, keeping its definition and count.
    /// Like adding and removing breakpoints, this stops every running
    /// thread briefly, without a reported stop.
    pub async fn set_breakpoint_enabled(
        &self,
        id: BreakpointId,
        enabled: bool,
    ) -> Result<Breakpoint> {
        self.request(|reply| Request::SetBreakpointEnabled { id, enabled, reply })
            .await
    }

    /// Removes one logical breakpoint and returns its prior definition.
    pub async fn remove_breakpoint(&self, id: BreakpointId) -> Result<Breakpoint> {
        self.request(|reply| Request::RemoveBreakpoint { id, reply })
            .await
    }

    /// Removes every logical breakpoint and returns their prior definitions.
    pub async fn remove_all_breakpoints(&self) -> Result<Arc<[Breakpoint]>> {
        self.request(|reply| Request::RemoveAllBreakpoints { reply })
            .await
    }

    /// Describes what this platform's debug hardware can watch.
    #[must_use]
    pub fn watchpoint_capabilities(&self) -> WatchpointCapabilities {
        backend::watchpoint_capabilities()
    }

    /// Resolves an expression in the selected thread's selected frame to the
    /// memory it occupies and the lifetime of that storage.
    pub async fn resolve_watch_target(&self, expression: &Expression) -> Result<WatchTarget> {
        self.selected()
            .await?
            .resolve_watch_target(expression)
            .await
    }

    /// Arms a hardware watchpoint that stops at every hit on every thread
    /// of the stopped process.
    ///
    /// Arming is atomic: on failure no thread is left armed and no event is
    /// published.
    pub async fn add_watchpoint(
        &self,
        spec: WatchpointSpec,
        access: WatchAccess,
    ) -> Result<Watchpoint> {
        self.add_watchpoint_with(spec, access, WatchpointOptions::default())
            .await
    }

    /// Arms a hardware watchpoint that stops at the hits `options` select.
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

    /// Resolves an expression at the current stop and watches its memory.
    pub async fn watch(&self, expression: &Expression, access: WatchAccess) -> Result<Watchpoint> {
        self.watch_with(expression, access, WatchpointOptions::default())
            .await
    }

    /// Resolves an expression at the current stop and watches its memory,
    /// stopping at the hits `options` select.
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

    /// Replaces which hits of a watchpoint stop execution; `None` stops at
    /// every hit. As for breakpoints, the hits already counted are kept, and
    /// this needs no stop: it applies from the next hit.
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

    /// Replaces a watchpoint's condition; `None` removes it. This needs no
    /// stop and applies from the next hit.
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

    /// Enables or disables a watchpoint. A disabled one releases its debug
    /// registers; enabling it plans them again, and fails, leaving it
    /// disabled, when other watchpoints hold them.
    pub async fn set_watchpoint_enabled(
        &self,
        id: WatchpointId,
        enabled: bool,
    ) -> Result<Watchpoint> {
        self.request(|reply| Request::SetWatchpointEnabled { id, enabled, reply })
            .await
    }

    /// Disarms one watchpoint and returns its prior definition.
    pub async fn remove_watchpoint(&self, id: WatchpointId) -> Result<Watchpoint> {
        self.request(|reply| Request::RemoveWatchpoint { id, reply })
            .await
    }

    /// Stops threads at `calls`, every call when empty, at the phase
    /// given, when `condition` holds. While any catchpoint exists, every
    /// thread stops at each of its calls, which costs microseconds a call.
    /// Catchpoints outlive the process, as breakpoints do.
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

    /// Removes a catchpoint and returns its prior definition.
    pub async fn remove_catchpoint(&self, id: CatchpointId) -> Result<Catchpoint> {
        self.request(|reply| Request::RemoveCatchpoint { id, reply })
            .await
    }

    /// Disarms every watchpoint and returns their prior definitions.
    pub async fn remove_all_watchpoints(&self) -> Result<Arc<[Watchpoint]>> {
        self.request(|reply| Request::RemoveAllWatchpoints { reply })
            .await
    }

    /// Returns how the debugger handles a signal, named by its exception
    /// code.
    pub async fn signal_policy(&self, signal: u64) -> Result<SignalPolicy> {
        self.request(|reply| Request::SignalPolicy { signal, reply })
            .await
    }

    /// Changes how the debugger handles a signal and returns the previous
    /// policy. The change applies at once, also while the inferior runs, and
    /// lasts for the whole session.
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

    /// Changes which exceptions a language runtime reports stop the
    /// inferior, and returns the previous choice. The change applies at
    /// once and lasts for the whole session.
    pub async fn set_exception_stops(&self, stops: ExceptionStops) -> Result<ExceptionStops> {
        self.request(|reply| Request::SetExceptionStops { stops, reply })
            .await
    }

    /// Chooses whether the process stops when it executes a program, as
    /// gdb's `catch exec` does, and returns the previous choice. Without
    /// it, a continued process that executes a program it is followed into
    /// runs on, with [`DebuggerEvent::ProgramChanged`] sent when the
    /// program is another; a step that executes one ends there either way.
    pub async fn set_exec_stops(&self, stops: bool) -> Result<bool> {
        self.request(|reply| Request::SetExecStops { stops, reply })
            .await
    }

    /// Kills the inferior and waits until it is gone, keeping the session:
    /// the program can be launched again, or another process attached.
    pub async fn kill(&self) -> Result<()> {
        self.request(|reply| Request::Kill { reply }).await
    }

    /// Asks the inferior to end, as with `SIGTERM` on Linux, and returns once
    /// the request is sent. The request itself never stops the inferior,
    /// whatever the signal's policy, and a stopped inferior is resumed to
    /// receive it; the program then exits, or stops for a breakpoint or
    /// signal while ending, as events report.
    pub async fn terminate(&self) -> Result<()> {
        self.request(|reply| Request::Terminate { reply }).await
    }

    /// Launches the inferior and acknowledges once native execution has started.
    pub async fn launch(&self) -> Result<ExecutionId> {
        self.launch_with(LaunchOptions::default()).await
    }

    /// Launches the inferior as `options` describe and acknowledges once
    /// native execution has started, or once it stopped at its entry.
    pub async fn launch_with(&self, options: LaunchOptions) -> Result<ExecutionId> {
        self.request(|reply| Request::Launch {
            options: Box::new(options),
            reply,
        })
        .await
    }

    /// Launches through a process that is waiting to exec this debugger's
    /// executable, such as a launcher started in a terminal, and
    /// acknowledges as [`Self::launch_with`] does once it has.
    ///
    /// The process must be single-threaded. `release` runs once it is
    /// traced and must then let it exec; until it does, the process runs
    /// and receives signals as it would untraced. Exiting first fails the
    /// launch.
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

    /// Attaches to an existing process and returns its coherent initial stop.
    pub async fn attach_process(&self, process_id: ProcessId) -> Result<StopId> {
        self.request(|reply| Request::Attach { process_id, reply })
            .await
    }

    /// Launches the inferior and waits until that execution stops or exits.
    pub async fn run(&self) -> Result<StopReason> {
        self.run_with(LaunchOptions::default()).await
    }

    /// Launches the inferior as `options` describe and waits until that
    /// execution stops or exits.
    pub async fn run_with(&self, options: LaunchOptions) -> Result<StopReason> {
        let mut events = self.subscribe();
        let execution = self.launch_with(options).await?;

        self.wait_for_execution(&mut events, execution).await
    }

    /// Continues the stopped inferior and acknowledges once threads have resumed.
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

    /// Continues every thread until the inferior stops or exits.
    pub async fn resume(&self) -> Result<StopReason> {
        self.resume_with_exception(ExceptionDisposition::Pass).await
    }

    /// Continues every thread with an explicit pending-exception disposition.
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

    /// Starts stepping one thread or task.
    ///
    /// [`StepKind::Out`] runs until `frame` returns to its caller; every
    /// other kind steps from the innermost frame, which `frame` must be.
    ///
    /// With [`ResumeScope::Process`], every other thread runs while the step
    /// does, so a step over a call that waits for another thread completes.
    /// Another thread's stop, such as a breakpoint or a signal, then ends
    /// the step before it completes. [`ResumeScope::Thread`] naming the
    /// stepping thread keeps every other thread stopped.
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

    /// Starts a step into the one call of the stopped line at `call`, a
    /// [`StepTarget`] of the innermost frame, which runs the line's other
    /// calls to their returns. It stops as [`StepKind::IntoSource`] does:
    /// in the called function, or, when the line ends first or the callee
    /// has no source, where a step in would.
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

    /// Steps the selected thread into the call at `call` on its line,
    /// running every other thread too, and waits for the step's stop.
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

    /// The calls a step into the selected thread's line could go into.
    pub async fn step_targets(&self) -> Result<Arc<[StepTarget]>> {
        self.selected().await?.step_targets().await
    }

    /// Steps the selected thread, running every other thread too, and waits
    /// until the operation stops or exits.
    ///
    /// Stepping out leaves the selected frame; every other step begins at
    /// the innermost frame, whichever frame is selected.
    pub async fn step(&self, kind: StepKind) -> Result<StopReason> {
        let selection = self.stopped_selection().await?;
        // A native step by lines begins at the innermost frame; a runtime
        // steps the frame it interprets that is selected, as pdb's `next`
        // after `up` does.
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

    /// Runs, every thread with it, until the selected thread reaches a
    /// location `spec` resolves to, stopping with [`StepKind::Advance`], or
    /// the selected frame returns first, stopping as [`StepKind::Out`]
    /// does. Nothing it plants outlives the stop that ends it.
    pub async fn advance(&self, spec: BreakpointSpec) -> Result<StopReason> {
        let selection = self.stopped_selection().await?;
        let mut events = self.subscribe();
        let execution = self
            .request(|reply| Request::Advance {
                process_id: selection.process,
                stop_id: selection.stop,
                context: selection.execution,
                frame: selection.frame,
                spec,
                scope: ResumeScope::Process(selection.process),
                exception: ExceptionDisposition::Pass,
                reply,
            })
            .await?;

        self.wait_for_execution(&mut events, execution).await
    }

    /// Moves one stopped thread, without running it, to resume at the one
    /// location `spec` resolves to in the function it is stopped in, and
    /// publishes the stop again there with [`StopReason::Jump`] under a new
    /// stop. A breakpoint at the new location stops the thread as it
    /// resumes, before it runs anything. A location with no code in the function, or with code in
    /// several places of it, is refused; to move a thread anywhere, assign
    /// `$pc`.
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

    /// Moves the selected thread to resume at `spec`, as
    /// [`Self::start_jump`] does, and waits for the stop it publishes.
    pub async fn jump(&self, spec: BreakpointSpec) -> Result<StopReason> {
        let selection = self.stopped_selection().await?;
        let mut events = self.subscribe();
        let execution = self
            .start_jump(selection.stop, selection.execution, spec)
            .await?;
        self.wait_for_execution(&mut events, execution).await
    }

    /// Pauses a running process and waits for a coherent all-stop snapshot.
    ///
    /// A process that is still launching stops at its initial exec stop
    /// instead of running first.
    pub async fn pause(&self) -> Result<StopReason> {
        if self.core_dump.is_some() {
            return Err(Error::PostMortemTarget);
        }

        let snapshot = self.snapshot().await?;
        let InferiorState::Running { process_id, .. } = snapshot.inferior else {
            return Err(if matches!(snapshot.inferior, InferiorState::NotRunning) {
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

    /// Reads a bounded byte range from a stopped inferior.
    ///
    /// Readable bytes are returned as one contiguous prefix. Target memory
    /// becoming inaccessible is represented by `MemoryRead::completion`;
    /// debugger and process-control failures remain errors.
    pub async fn read_memory(
        &self,
        address: VirtualAddress,
        byte_count: u64,
    ) -> Result<MemoryRead> {
        let stop = self.stopped_selection().await?.stop;
        self.read_memory_at(stop, address, byte_count).await
    }

    /// Reads memory as [`Self::read_memory`] does, but only at `stop`: once
    /// the program has left it, the read fails with [`Error::StaleStop`]
    /// instead of reading a later stop's memory.
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

    /// Reads one little-endian 64-bit word from a stopped inferior.
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

    /// Writes one little-endian 64-bit word into a stopped inferior; see
    /// [`Self::write_memory`].
    pub async fn write_word(&self, address: VirtualAddress, value: u64) -> Result<()> {
        let written = self.write_memory(address, &value.to_le_bytes()).await?;
        if written < 8 {
            return Err(Error::MemoryNotWritable(VirtualAddress::new(
                address.get() + written,
            )));
        }

        Ok(())
    }

    /// Writes bytes into a stopped inferior and returns how many were
    /// written: all of them, or those before memory that cannot be written,
    /// which fails when it is the first. Debugger breakpoint traps stay in
    /// place, hiding the written bytes as they hid the old ones.
    pub async fn write_memory(&self, address: VirtualAddress, bytes: &[u8]) -> Result<u64> {
        let stop = self.stopped_selection().await?.stop;
        self.write_memory_at(stop, address, bytes).await
    }

    /// Writes memory as [`Self::write_memory`] does, but only at `stop`:
    /// once the program has left it, the write fails with
    /// [`Error::StaleStop`].
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

    /// Resolves a linker symbol to its address in the running process.
    ///
    /// The symbol may be any loaded module's, named as
    /// [`SymbolInfo::answers_to`] reads names. Where several modules define
    /// the name, the first in load order that exports it wins, as the
    /// dynamic loader binds it; otherwise the name must be one address's.
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

            // A versioned name's default version is the one the loader binds.
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

    /// Describes a process address by the loaded module, section, and symbol
    /// containing it at the current stop.
    ///
    /// A code address is named by the symbol whose extent contains it, and a
    /// data address only by a symbol whose declared storage contains it; the
    /// nearest preceding symbol is never substituted.
    pub async fn describe_address(&self, address: VirtualAddress) -> Result<AddressDescription> {
        let selection = self.stopped_selection().await?;

        self.request(|reply| Request::DescribeAddress {
            stop_id: selection.stop,
            address,
            reply,
        })
        .await
    }

    /// Disassembles code at the current stop as the selected thread sees it.
    ///
    /// Bytes come from the stopped process or core dump with debugger
    /// breakpoint traps hidden. Instructions are decoded only forward from
    /// proven instruction starts, among them the selected thread's program
    /// counter; see [`Disassembly`] for how unproven, unreadable, and
    /// conflicting code is reported. Indirect branches name the targets the
    /// stopped state gives them; see [`IndirectTarget`].
    pub async fn disassemble(&self, query: DisassemblyQuery) -> Result<Disassembly> {
        self.selected().await?.disassemble(query).await
    }

    /// Resolves the selected frame's location to normalized function and
    /// source metadata: where execution stopped in the innermost frame, and
    /// the call in progress in an outer one.
    pub async fn current_location(&self) -> Result<ExecutionLocation> {
        self.selected().await?.location().await
    }

    /// Lazily reads source lines surrounding the selected frame's location,
    /// from the first place this handle's [`SourcePathMap`] finds the file.
    pub async fn source_context(&self, radius: u32) -> Result<SourceContext> {
        self.selected().await?.source_context(radius).await
    }

    /// Reads source lines surrounding a source location of `module`.
    async fn source_context_at(
        &self,
        module: ModuleId,
        location: Option<SourceLocation>,
        radius: u32,
    ) -> Result<SourceContext> {
        let location = location.ok_or(Error::SourceLocationUnavailable)?;

        // Source files are identified within the image of the module that
        // contains the frame's code.
        let file = self
            .loaded_module_image(module)
            .await?
            .source_file(location.file)
            .cloned()
            .ok_or(Error::SourceLocationUnavailable)?;

        self.source_around(file.path, location.line, location.column, radius)
            .await
    }

    /// Reads the source lines within `radius` of `line` of the file at the
    /// `recorded` path.
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

        let lines = all_lines[start..end]
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

    /// Reads a source file a module image names, through the handle's
    /// [`SourcePathMap`], returning the path read and its contents. Only
    /// files the debug information names can be read this way.
    pub async fn read_source_file(&self, file: &SourceFile) -> Result<(PathBuf, String)> {
        self.read_source(&file.path).await
    }

    /// Reads the source file at the `recorded` path a language runtime gave
    /// one of the program's frames, as [`StackFrame::recorded`] has it, or
    /// will give it, through the handle's [`SourcePathMap`]. A client that
    /// serves files to others reads only paths its frames named this way,
    /// or that are the program's own by rules it can state.
    pub async fn read_recorded_source(&self, recorded: &Path) -> Result<(PathBuf, String)> {
        self.read_source(recorded).await
    }

    /// Reads the first candidate for a recorded source path that exists. A
    /// file that exists but cannot be read is an error rather than skipped.
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

    /// Returns an immutable snapshot of the debugger's current state.
    pub async fn snapshot(&self) -> Result<StateSnapshot> {
        self.request(|reply| Request::Snapshot { reply }).await
    }

    /// One page of the tasks every language runtime in the stopped process
    /// schedules, such as Go's goroutines, beginning at `from` or at the
    /// first. A page holds at most `limit` tasks, and says why it may be
    /// incomplete.
    pub async fn tasks(&self, from: Option<TaskCursor>, limit: usize) -> Result<TaskPage> {
        self.task_page(from, limit, false).await
    }

    /// One page of the tasks that run the program's code, as
    /// [`Self::tasks`] gives, leaving out those a runtime runs for its own
    /// work before paging, so a page holds only the program's.
    pub async fn program_tasks(&self, from: Option<TaskCursor>, limit: usize) -> Result<TaskPage> {
        self.task_page(from, limit, true).await
    }

    /// One page of the program's tasks, as [`Self::program_tasks`] gives,
    /// at `stop`, which must still be the current stop.
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

    /// Reconstructs the selected thread's stack frames.
    pub async fn backtrace(&self) -> Result<Backtrace> {
        self.selected().await?.backtrace().await
    }

    /// Reads the general register set of the selected frame.
    pub async fn registers(&self) -> Result<RegisterSnapshot> {
        self.selected().await?.registers().await
    }

    /// Inspects every visible parameter and local variable in the selected frame.
    pub async fn variables(&self) -> Result<VariableSnapshot> {
        self.variables_with_limits(InspectionLimits::default())
            .await
    }

    /// Inspects visible variables under explicit bounded resource limits.
    pub async fn variables_with_limits(
        &self,
        limits: InspectionLimits,
    ) -> Result<VariableSnapshot> {
        self.selected().await?.variables_with_limits(limits).await
    }

    /// Inspects the innermost visible data object with the supplied name.
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

    /// Evaluates an expression in the selected frame of the selected
    /// stopped thread, reading only.
    pub async fn evaluate(&self, expression: &Expression) -> Result<Evaluation> {
        self.selected().await?.evaluate(expression).await
    }

    /// Evaluates an expression in the selected frame, assigning when `mode`
    /// allows, under explicit resource limits.
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

    /// The type an expression has in the selected frame, reading no memory.
    pub async fn expression_type(&self, expression: &Expression) -> Result<TypeInfo> {
        self.selected().await?.expression_type(expression).await
    }

    /// Why an expression's value in the selected frame is presented as it
    /// is: the views its type matched, and how the one that binds presents
    /// it.
    pub async fn explain_view(&self, expression: &Expression) -> Result<ViewExplanation> {
        self.selected().await?.explain_view(expression).await
    }

    /// Presents an expression's value in the selected frame, and its first
    /// page of children, and returns a recording of each kernel run that
    /// took, as text that `microscope views replay` replays without a program.
    pub async fn record_kernels(&self, expression: &Expression) -> Result<Vec<String>> {
        self.selected().await?.record_kernels(expression).await
    }

    /// Presents values with these view files ahead of the views modules
    /// embed and the built-in views, replacing any loaded before, with the
    /// kernels and renderers beside them, and returns what kept parts of
    /// them out. Files are parsed, and kernels loaded, here, before the
    /// debugger sees them; a kernel or renderer whose name an earlier one
    /// has is left out.
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

    /// The views whose patterns name the types `name` means in each loaded
    /// module, and why each did not bind, with no process needed.
    pub async fn explain_type(&self, name: &str) -> Result<Vec<TypeViews>> {
        let name = name.to_owned();
        self.request(|reply| Request::ExplainType { name, reply })
            .await
    }

    /// How every loaded module's types are presented: each type a view's
    /// pattern names, with the views tried, and the loaded and embedded
    /// views that present no type. Needs no process.
    pub async fn check_views(&self) -> Result<ViewCheck> {
        self.request(|reply| Request::CheckViews { reply }).await
    }

    /// Turns presenting values with views on or off.
    pub async fn enable_views(&self, enabled: bool) -> Result<()> {
        self.request(|reply| Request::EnableViews { enabled, reply })
            .await
    }

    /// Chooses whether steps stop in a language runtime's own code, as
    /// they otherwise pass through it to the program's code it calls, and
    /// returns the previous choice. The choice applies to the steps that
    /// begin after it and lasts for the whole session. Frames keep their
    /// roles either way: backtraces fold the runtime's frames as before.
    pub async fn set_step_into_runtime(&self, enter: bool) -> Result<bool> {
        self.request(|reply| Request::SetStepIntoRuntime { enter, reply })
            .await
    }

    /// Evaluates an expression in the selected frame for its value, reading
    /// only. A range has no single value; evaluate it for its elements.
    pub async fn inspect(&self, expression: &Expression) -> Result<InspectedValue> {
        self.inspect_with_limits(expression, InspectionLimits::default())
            .await
    }

    /// Evaluates an expression for its value under explicit resource limits.
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

    /// Inspects one global of the main executable image as the selected
    /// thread sees it. A [`GlobalVariableId`] is unique only within its
    /// image; use [`Self::loaded_global`] for a shared library's.
    pub async fn main_global(&self, id: GlobalVariableId) -> Result<Variable> {
        let module = self.loaded_module().await?;

        self.loaded_global(GlobalVariableReference {
            module: module.id,
            image: module.image,
            variable: id,
        })
        .await
    }

    /// Inspects one exact global in a specific loaded module as the selected
    /// thread sees it.
    pub async fn loaded_global(&self, global: GlobalVariableReference) -> Result<Variable> {
        self.selected()
            .await?
            .global_with_limits(global, InspectionLimits::default())
            .await
    }

    /// Explicitly dereferences a pointer or reference value produced at the
    /// current stopped snapshot.
    pub async fn dereference(
        &self,
        reference: Box<DereferenceReference>,
    ) -> Result<DereferencedValue> {
        self.dereference_with_limits(reference, InspectionLimits::default())
            .await
    }

    /// Dereferences a value under explicit bounded resource limits.
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

    /// Evaluates one arbitrary bounded page from an aggregate child capability.
    pub async fn value_children(
        &self,
        reference: Arc<ValueChildrenReference>,
        query: ValueChildQuery,
    ) -> Result<ValueChildPage> {
        self.value_children_with_limits(reference, query, InspectionLimits::default())
            .await
    }

    /// Evaluates one child page under explicit bounded resource limits.
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

    /// Reads the inputs of one drawing a value's view offers, at the stop
    /// its reference names, under explicit bounded resource limits. Only
    /// the web page draws.
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

    /// Reads the elements of a sequence of numbers at once, when they lie
    /// next to each other in memory and take at most `most_bytes`, as an
    /// array's, a slice's, and a view's of the form `range(n) => p[i]` do.
    /// `None` means they do not, and are read as children.
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

    /// Every renderer drawings may name, in the order a name is looked up:
    /// the session's, each module's own, then the built-in ones.
    pub async fn renderers(&self) -> Result<Arc<[Arc<Renderer>]>> {
        self.request(|reply| Request::Renderers { reply }).await
    }

    /// Lists one filtered, bounded page of immutable global metadata.
    pub async fn globals(&self, query: GlobalVariableQuery) -> Result<GlobalVariablePage> {
        self.request(|reply| Request::Globals { query, reply })
            .await
    }

    /// Returns the process-wide loaded-module registry.
    pub async fn loaded_modules(&self) -> Result<LoadedModuleSnapshot> {
        self.request(|reply| Request::LoadedModules { reply }).await
    }

    /// Returns the immutable debug metadata of one loaded module, such as the
    /// module owning a backtrace frame.
    pub async fn loaded_module_image(&self, module: ModuleId) -> Result<Arc<ModuleImage>> {
        self.request(|reply| Request::ModuleImage { module, reply })
            .await
    }

    /// Selects a frame of the selected thread, numbered as
    /// [`Self::backtrace`] presents it, for implicit inspection commands:
    /// variables, expressions, watch targets, the current location and its
    /// source, and stepping out. Every new stop selects the innermost frame.
    ///
    /// Values in an outer frame come from the registers its callees saved.
    /// A register a callee may overwrite without saving it is unknown there,
    /// so a value held in one is
    /// [unavailable](VariableUnavailableReason::RegisterNotSaved) rather
    /// than read from the stopped thread.
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

    /// Selects the stopped thread or task used by implicit inspection
    /// commands.
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

    /// Returns a view of the selected frame of the selected thread at the
    /// current stop.
    async fn selected(&self) -> Result<StopView<'_>> {
        let selection = self.stopped_selection().await?;
        Ok(self.at(StopContext {
            stop: selection.stop,
            execution: selection.execution,
            frame: selection.frame,
        }))
    }

    /// Inspects one frame of one thread at one stop, independently of the
    /// selected thread and frame. Every request through the view fails with
    /// [`Error::StaleStop`] once execution has left that stop.
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

    /// Waits for the stop or exit that ends `execution`.
    ///
    /// The wait is cancelled if the inferior is detached first, such as by a
    /// concurrent shutdown, or if the controller exits. Every handle keeps
    /// the event channel open, so the controller's request queue closing is
    /// what reveals its exit.
    ///
    /// Falling behind loses the oldest events, and the one that ends
    /// `execution` is most likely newer: a burst of them, as a stop that
    /// loads many modules sends, comes before it. So a lag asks the
    /// controller whether `execution` still runs. If it does, its end is
    /// still to come; if not, every event before the answer is already
    /// sent, and those still held are searched for its end, which fails
    /// only where it was lost.
    /// Waits for the controller to end, as it does once shut down, or once
    /// it panicked and released its process.
    pub(crate) async fn ended(&self) {
        self.requests.closed().await;
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

        receive.await.map_err(|_| Error::RequestCancelled)?
    }
}

/// Names one frame of one thread or task at one stop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StopContext {
    /// The stop the frame belongs to.
    pub stop: StopId,

    /// The thread or task whose stack holds the frame.
    pub execution: ExecutionContext,

    /// The frame, numbered as [`DebuggerHandle::backtrace`] presents it.
    pub frame: StackFrameId,
}

/// Inspects one explicit frame of one stopped thread; see
/// [`DebuggerHandle::at`].
///
/// Values in an outer frame come from the registers its callees saved, as
/// for [`DebuggerHandle::select_frame`].
#[derive(Clone, Copy)]
pub struct StopView<'a> {
    handle: &'a DebuggerHandle,
    context: StopContext,
}

impl StopView<'_> {
    /// Reconstructs the thread's stack frames; the view's frame does not
    /// limit them.
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

    /// Reads the frame's general registers.
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

    /// The calls of the line the frame's thread is stopped at that a step
    /// into could go into, in address order. Only the innermost frame has
    /// any.
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

    /// Resolves the frame's location to function and source metadata.
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

    /// Lazily reads source lines surrounding the frame's location through
    /// the handle's [`SourcePathMap`].
    pub async fn source_context(&self, radius: u32) -> Result<SourceContext> {
        let (module, source) = match self.location().await {
            Ok(location) => (location.module, location.image.source),
            // A suspended task's frame with no code to locate still waits
            // at a line, and an interpreted frame runs one its runtime
            // recorded.
            Err(Error::FrameSuspended | Error::FrameInterpreted) => {
                let trace = self.backtrace().await?;
                let frame = trace
                    .frames
                    .iter()
                    .find(|frame| frame.id == self.context.frame)
                    .ok_or(Error::SourceLocationUnavailable)?;
                if let Some(place) = frame
                    .recorded
                    .as_ref()
                    .and_then(|recorded| recorded.place.as_ref())
                {
                    let line = LineNumber::new(u64::from(place.line))
                        .ok_or(Error::SourceLocationUnavailable)?;
                    let column = ColumnNumber::new(u64::from(place.column));
                    if place.path.starts_with('<') && place.path.ends_with('>') {
                        return Err(Error::SourceNotAFile {
                            name: Arc::clone(&place.path),
                        });
                    }
                    let recorded = Arc::new(PathBuf::from(&*place.path));
                    return self
                        .handle
                        .source_around(recorded, line, column, radius)
                        .await;
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

    /// Inspects every visible parameter and local variable of the frame.
    pub async fn variables(&self) -> Result<VariableSnapshot> {
        self.variables_with_limits(InspectionLimits::default())
            .await
    }

    /// Inspects the frame's variables under explicit resource limits.
    pub async fn variables_with_limits(
        &self,
        limits: InspectionLimits,
    ) -> Result<VariableSnapshot> {
        self.variable_query(VariableQuery::All, limits).await
    }

    /// Inspects one exact global as the frame's thread sees it, such as its
    /// instance of a thread-local variable.
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

    /// Evaluates an expression in the frame, reading only.
    pub async fn evaluate(&self, expression: &Expression) -> Result<Evaluation> {
        self.evaluate_with(
            expression,
            EvaluationMode::Read,
            InspectionLimits::default(),
        )
        .await
    }

    /// Evaluates an expression in the frame, assigning when `mode` allows,
    /// under explicit resource limits.
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

    /// Why an expression's value in the frame is presented as it is.
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

    /// Presents an expression's value in the frame, and its first page of
    /// children, and records each kernel run that took.
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

    /// The type an expression has in the frame, reading no memory.
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

    /// Evaluates an expression for its value under explicit resource limits.
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

    /// Resolves an expression in the frame to the memory it occupies and
    /// the lifetime of that storage.
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

    /// Disassembles code as the thread sees it at the stop; see
    /// [`DebuggerHandle::disassemble`].
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

/// The stop that implicit inspection reads and the thread and frame selected
/// in it.
#[derive(Clone, Copy)]
pub(crate) struct StoppedSelection {
    pub(crate) process: ProcessId,
    pub(crate) stop: StopId,
    pub(crate) execution: ExecutionContext,
    pub(crate) frame: StackFrameId,

    /// Whether the selected frame is one a runtime interprets, which it
    /// steps from wherever the frame is.
    pub(crate) interpreted: bool,
}
