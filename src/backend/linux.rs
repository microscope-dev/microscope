//! Linux `x86_64` `ptrace` debugger backend.
//!
//! Each debugged process is driven by a `Controller<P>` hosted on the single OS tracer
//! thread (`tracer.rs`). Parameterizing over `InspectionOps` and `LinuxTraceOps` lets the
//! same controller logic run against live `ptrace`, deterministic simulation (`sim_edge`),
//! and post-mortem core dumps.

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fmt;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread::{self, JoinHandle};

pub use inspector::InspectorMessage;
use nix::errno::Errno;
use nix::libc;
use nix::unistd::Pid;
use run_phase::{RunPhase, StopBarrier};
use session::{Orphans, Session, Shutdown};
use signals::SignalPolicies;
pub use signals::{Signal, WaitEvent};
use tokio::sync::{mpsc, watch};

use crate::debug_info::{DebugInfo, UnwindInfo, VariableInfo};
use crate::protocol::{
    Breakpoint, BreakpointId, DebuggerEvent, ExceptionInfo, ExecutionId, ExitStatus,
    FramePresentation, FrameScopeEvidence, InferiorState, ProcessId, Reply, Request, ResumeScope,
    StateSnapshot, StepKind, StopId, StopReason, ThreadSnapshot,
    ThreadState as ObservableThreadState, WatchAccess, Watchpoint, WatchpointCapabilities,
    WatchpointId,
};

use crate::{
    CodeInstanceId, Error, ExecutionContext, LoadedModule, ModuleImage, Result, SourceLocation,
    StackFrameId, ThreadId as DebugThreadId, UnwindTermination, VirtualAddress,
};

use super::{ControllerChannels, ControllerMessage, EventSender, Executable, FileIdentity};
use activation::{Activation, StackPosition};
use arch::{BREAKPOINT_OPCODE, FloatingRegisters, Registers};
use classify::{is_stopping_signal, is_superseded};
use debug_registers::DebugRegisterPlan;
use memory::MemoryAccessError;
use modules::{ModuleMapping, loader_link_maps, mapped_module_load_bias, module_mappings};
use native::{InspectionOps, LinuxPtrace, LinuxTraceOps, is_vanished_tracee};
pub use proc_status::{Asleep, ThreadFiles};
pub(super) use tls::CLibrary;
pub use tls::RunningTls;
use tls::TlsModule;
use trap::{AtTrap, Lift, Trap, TrapEvent};

mod activation;
mod agent_files;
mod alarm;
mod arch;
mod assignments;
mod async_frames;
pub use async_frames::resume_address;
mod awaits;
pub mod bpf;
mod breakpoints;
mod callers;
mod catchpoints;
#[allow(dead_code, reason = "until the Node controller drives the inspector")]
mod cdp;
mod classify;
mod code_copies;
mod code_frames;
mod core_dump;
mod core_files;
mod debug_registers;
mod disassembly;
mod evaluation;
mod follow;
mod frames;
mod holdings;
mod indirect;
mod inspection;
mod inspector;
mod internal_stops;
mod interpreted;
mod interpreted_values;
mod interpreted_views;
mod interpreted_watches;
mod invariants;
mod jit_objects;
mod language_exceptions;
mod libraries;
mod lifecycle;
mod loops;
mod managed;
mod memory;
mod modules;
mod native;
#[cfg(test)]
pub mod native_tracee;
mod new_task;
mod node_launch;
mod panic_release;
pub mod perf;
mod post_mortem;
mod presentation;
mod proc_status;
mod process_names;
mod python_launch;
pub mod reaped;
#[cfg(debug_assertions)]
mod recorded;
mod returns;
mod run_control;
mod run_phase;
mod runtime_agents;
mod runtime_steps;
mod runtimes;
mod session;
mod signals;
#[cfg(any(test, feature = "sim"))]
pub mod sim_edge;
mod stack_start;
mod stack_watches;
mod step_targets;
mod stepping;
mod sys;
pub mod syscalls;
mod tls;
mod tracer;
mod trap;
mod vdso;
mod visualizers;
mod wait_hub;
mod watchpoints;
mod writes;

pub use follow::{Capacity, GroupNotice, Joined, Started, program_runtime};

#[cfg(feature = "tools")]
pub fn trace_on_exec(command: &mut std::process::Command) {
    sys::before_exec(
        command,
        sys::BeforeExec {
            dies_with_parent: true,
            traced: true,
            ..sys::BeforeExec::default()
        },
    );
}
pub use post_mortem::{PostMortemSession, open_core};

pub use process_names::processes_named;

#[cfg(feature = "fuzzing")]
pub fn fuzz_core_dump(data: &[u8]) {
    core_dump::fuzz(data);
}

#[cfg(feature = "fuzzing")]
pub fn fuzz_cdp_message(data: &[u8]) {
    cdp::fuzz(data);
}

#[cfg(feature = "fuzzing")]
pub fn fuzz_debug_register_plan(data: &[u8]) {
    debug_registers::fuzz(data);
}

pub fn running_thread_pointers(pid: u32) -> std::result::Result<Vec<(u32, u64)>, String> {
    let pid = i32::try_from(pid).map_err(|_| format!("process {pid} has no Linux id"))?;
    Ok(tls::running_thread_pointers(Pid::from_raw(pid))?
        .into_iter()
        .filter_map(|(tid, pointer)| Some((u32::try_from(tid).ok()?, pointer)))
        .collect())
}

pub fn thread_start_time(process: crate::model::Pid, thread: crate::model::Tid) -> Option<u64> {
    proc_status::thread_start_time(process, thread)
}

pub fn process_exited(process: crate::model::Pid) -> bool {
    proc_status::process_exited(process)
}

pub fn process_start_time(process: i32) -> Option<u64> {
    proc_status::process_start_time(process)
}

pub fn target_thread_ids(
    process: u32,
    threads: &[u32],
) -> Vec<(u32, crate::runtime_model::TargetThreadId)> {
    proc_status::target_thread_ids(process, threads)
}

pub fn target_thread_id(process: u32, thread: u32) -> Option<crate::runtime_model::TargetThreadId> {
    proc_status::target_thread_id(process, thread)
}

const WATCH_SLOTS: u32 = 4;
const _: () = assert!(WATCH_SLOTS as usize == debug_registers::SLOT_COUNT);

pub fn watchpoint_capabilities() -> WatchpointCapabilities {
    WatchpointCapabilities {
        slots: WATCH_SLOTS,
        max_slot_bytes: debug_registers::MAX_SLOT_BYTES,
        access: Arc::from([
            WatchAccess::Change,
            WatchAccess::Write,
            WatchAccess::ReadWrite,
        ]),
    }
}

const CONTROLLER_THREAD_NAME: &str = "microscope-controller";
const WAITER_THREAD_NAME: &str = "microscope-waitpid";
const TRAP_UNKNOWN: i32 = 5;
const TRAP_HARDWARE_BREAKPOINT: i32 = 4;
const MAX_LOGICAL_MEMORY_READ: usize = 1024 * 1024;
const MAX_PUBLIC_MEMORY_READ: u64 = 64 * 1024;
const MAX_VALUE_CHILD_PAGE_LIMIT: u32 = 256;

static LINUX_SESSION_ACTIVE: AtomicBool = AtomicBool::new(false);

fn backend_error(error: LinuxError) -> Error {
    Error::backend(error)
}

fn process_id(pid: Pid) -> ProcessId {
    ProcessId::new(u64::from(pid.as_raw().unsigned_abs()))
}

fn debug_thread_id(pid: Pid) -> DebugThreadId {
    DebugThreadId::new(u64::from(pid.as_raw().unsigned_abs()))
}

fn debug_pid(thread: DebugThreadId) -> Result<Pid> {
    i32::try_from(thread.get())
        .ok()
        .filter(|tid| *tid > 0)
        .map(Pid::from_raw)
        .ok_or(Error::UnknownThread(thread))
}

fn exception_info(signal: Signal) -> ExceptionInfo {
    ExceptionInfo::new(signal.code(), signal.to_string())
}

fn pending_exception_info(pending: PendingSignal) -> ExceptionInfo {
    ExceptionInfo::new(
        pending.signal.code(),
        signals::describe(
            &pending.signal.name(),
            pending.signal.number(),
            pending.code,
            pending.fault_address,
            pending.sender,
        ),
    )
}

/// Process-wide lease ensuring only one tracer thread runs `waitpid(-1)` at a time,
/// since concurrent `waitpid(-1)` loops would steal wait events from each other.
struct SessionLease {
    owns_global_lease: bool,
}

impl SessionLease {
    fn acquire() -> Result<Self> {
        LINUX_SESSION_ACTIVE
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|_| backend_error(LinuxError::SessionActive))?;
        Ok(Self {
            owns_global_lease: true,
        })
    }

    const fn detached() -> Self {
        Self {
            owns_global_lease: false,
        }
    }
}

impl Drop for SessionLease {
    fn drop(&mut self) {
        if !self.owns_global_lease {
            return;
        }

        assert!(
            LINUX_SESSION_ACTIVE.swap(false, Ordering::AcqRel),
            "Linux tracing session lease was active"
        );
    }
}

struct BreakpointSite {
    original_byte: u8,
    trap: Trap,
    owners: BTreeSet<BreakpointOwner>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum BreakpointOwner {
    User(BreakpointId),
    Plan(ExecutionId),

    Loader,

    Runtime,

    StackMove,

    InterpretedMove,

    Resolver,

    InspectorHook,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NativeThreadState {
    Starting,
    Running,
    StopRequested,
    Stopped,
    Exiting,
}

#[derive(Debug, Clone)]
enum ExpectedStop {
    InitialExec,

    /// A process launched through its exec, which has not happened yet.
    AdoptedExec,
    InitialAttach,
    None,
    UserStep {
        kind: StepKind,
    },
}

struct TraceThread {
    state: NativeThreadState,
    expected: ExpectedStop,
    pending_signal: Option<PendingSignal>,

    held_signal: Option<PendingSignal>,
    reason: Option<StopReason>,
    trap: AtTrap,

    retrap: Option<VirtualAddress>,
    debugger_stop_pending: bool,

    armed: Option<u64>,

    watch_hits: BTreeMap<WatchpointId, u64>,

    call: catchpoints::CallPosition,

    stepping_call: bool,

    name: Option<Arc<str>>,

    stack_start: Option<u64>,
}

impl TraceThread {
    const fn starting(expected: ExpectedStop) -> Self {
        Self {
            state: NativeThreadState::Starting,
            expected,
            pending_signal: None,
            held_signal: None,
            reason: None,
            trap: AtTrap::Clear,
            retrap: None,
            debugger_stop_pending: false,
            armed: None,
            watch_hits: BTreeMap::new(),
            call: catchpoints::CallPosition::Outside,
            stepping_call: false,
            name: None,
            stack_start: None,
        }
    }

    const fn holds_siblings(&self) -> bool {
        self.trap.repairing().is_some() || self.retrap.is_some()
    }
}

#[derive(Default)]
struct WatchState {
    plan: DebugRegisterPlan,
    generation: u64,
    watchpoints: BTreeMap<WatchpointId, WatchRecord>,
}

struct WatchRecord {
    watchpoint: Watchpoint,
    frame: Option<FrameScopeEvidence>,

    observed: Option<Arc<[u8]>>,

    value: Option<Arc<str>>,
    code: Option<VirtualAddress>,

    task: Option<stack_watches::TaskWatch>,
}

#[derive(Clone, Copy)]
struct PendingSignal {
    signal: Signal,
    code: i32,
    sender: Option<i32>,
    fault_address: Option<u64>,
}

#[derive(Clone, Copy)]
struct SignalMetadata {
    code: i32,
    sender: Option<i32>,

    fault_address: Option<u64>,
}

impl fmt::Debug for PendingSignal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PendingSignal")
            .field("signal", &self.signal)
            .field("code", &self.code)
            .field("sender", &self.sender)
            .field("fault_address", &HexAddress(self.fault_address))
            .finish()
    }
}

impl fmt::Debug for SignalMetadata {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SignalMetadata")
            .field("code", &self.code)
            .field("sender", &self.sender)
            .field("fault_address", &HexAddress(self.fault_address))
            .finish()
    }
}

struct HexAddress(Option<u64>);

impl fmt::Debug for HexAddress {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0 {
            Some(address) => write!(f, "Some({address:#x})"),
            None => f.write_str("None"),
        }
    }
}

#[derive(Debug)]
struct RawStopRecord {
    status: String,
    siginfo: std::result::Result<SignalMetadata, Errno>,
}

#[derive(Debug)]
enum ClassifiedStop {
    ThreadStart,
    SignalDelivery(PendingSignal),
    GroupStop(Signal),
    Breakpoint(VirtualAddress),
    Watch(BTreeSet<WatchpointId>),
    Trace {
        /// Watchpoints hit by the stepped instruction.
        watch: BTreeSet<WatchpointId>,
    },
    DebuggerRequested,

    RemovedTrap,

    CarriedTrap,

    ProgramTrap(VirtualAddress),

    Superseded,
    Unclassifiable(RawStopRecord),
}

#[derive(Debug, Clone, Default)]
struct StepStart {
    module: Option<LoadedModule>,
    source: Option<SourceLocation>,
    code_instance: Option<CodeInstanceId>,
    physical_instance: Option<CodeInstanceId>,
    activation: Option<Activation>,

    stack_pointer: Option<StackPosition>,

    returned_to: Option<Activation>,

    running_on: bool,
    plan_addresses: BTreeSet<VirtualAddress>,

    targets: BTreeSet<VirtualAddress>,

    standing: Option<VirtualAddress>,
    epilogue_traversal: Option<EpilogueTraversal>,
    return_traversal: Option<ReturnTraversal>,

    resume_guard: Option<ResumeGuard>,

    into_call: Option<VirtualAddress>,

    call_return: Option<(VirtualAddress, StackPosition)>,

    enters_runtime: bool,

    panic_guards: BTreeSet<VirtualAddress>,

    following: bool,

    escape: BTreeSet<VirtualAddress>,

    loops: Option<loops::StepLoops>,

    returning: Option<returns::Returning>,

    returned: Option<returns::Returned>,

    new_task: Option<new_task::NewTask>,

    awaiting: Option<awaits::AwaitStep>,

    dropped: Option<awaits::RunningFuture>,

    judged_by_trap: Option<VirtualAddress>,
}

impl StepStart {
    fn plan_sites(&self) -> BTreeSet<VirtualAddress> {
        let starters = match &self.new_task {
            Some(new_task::NewTask::Watching(starters)) => starters.keys().copied().collect(),
            _ => Vec::new(),
        };

        self.plan_addresses
            .iter()
            .chain(&self.panic_guards)
            .chain(&self.targets)
            .chain(&starters)
            .copied()
            .collect()
    }

    fn owns_plan_site(&self, address: VirtualAddress) -> bool {
        self.plan_addresses.contains(&address)
            || self.panic_guards.contains(&address)
            || self.targets.contains(&address)
            || self.escape.contains(&address)
            || self
                .resume_guard
                .is_some_and(|guard| guard.address == address)
            || self
                .new_task
                .as_ref()
                .is_some_and(|task| task.owns(address))
    }
}

const fn steps_instructions(kind: StepKind) -> bool {
    matches!(kind, StepKind::Instruction | StepKind::OverInstruction)
}

#[derive(Debug, Clone, Copy)]
struct ResumeGuard {
    address: VirtualAddress,
    stack: StackPosition,

    call: bool,
}

#[derive(Debug, Clone)]
struct ReturnTraversal {
    return_address: VirtualAddress,

    guarded_activation: Activation,
    retire_return_after_repair: bool,
}

#[derive(Debug, Clone)]
struct EpilogueTraversal {
    return_address: VirtualAddress,
    completion_addresses: BTreeSet<VirtualAddress>,
    retire_return_after_repair: bool,
}

#[derive(Debug, Clone, Copy)]
enum Resume {
    Continue,
    Step,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct StepOwner {
    thread: Pid,

    task: Option<crate::TaskId>,
}

#[derive(Debug, Clone)]
enum ActiveKind {
    Launch,
    Continue,
    Step {
        owner: StepOwner,
        kind: StepKind,

        requested: StepKind,
        start: Box<StepStart>,

        progress_owed: bool,
    },

    RuntimeStep {
        owner: StepOwner,
        kind: StepKind,
        runtime: crate::RuntimeId,

        number: u64,
    },

    RuntimeAssignment {
        thread: Pid,
        runtime: crate::RuntimeId,
        number: u64,
    },
}

struct ActiveExecution {
    id: ExecutionId,
    kind: ActiveKind,
    scope: ResumeScope,
    resume_threads: BTreeSet<Pid>,
}

enum Edit {
    InspectorCleanup,

    InspectorPaused,

    InspectorAssigned,
    AddBreakpoint {
        spec: crate::BreakpointSpec,
        options: Box<crate::BreakpointOptions>,
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
    AddWatchpoint {
        spec: crate::WatchpointSpec,
        access: WatchAccess,
        options: crate::WatchpointOptions,
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
    SetExceptionStops {
        stops: crate::ExceptionStops,
        reply: Reply<crate::ExceptionStops>,
    },
    AddCatchpoint {
        id: Option<crate::CatchpointId>,
        calls: Arc<[crate::syscalls::SyscallId]>,
        phase: crate::CatchPhase,
        condition: Option<crate::Condition>,
        reply: Reply<crate::Catchpoint>,
    },
    RemoveCatchpoint {
        id: crate::CatchpointId,
        reply: Reply<crate::Catchpoint>,
    },

    RefreshModules,

    FollowStacks,

    FollowInterpreted,
    ArmAgents,

    ReturnToRuntime {
        pid: Pid,
    },
    AgentReport {
        runtime: crate::RuntimeId,
        generation: u64,
        failure: Option<Arc<str>>,
    },
}

struct PublicStop {
    id: StopId,
    triggering_thread: Pid,
    reason: StopReason,
    presentations: BTreeMap<Pid, FramePresentation>,

    selected: ExecutionContext,

    selected_thread: Option<Pid>,

    selected_frames: BTreeMap<ExecutionContext, StackFrameId>,
    caches: StopCaches,

    returned: Option<returns::Returned>,
}

#[derive(Default)]
struct StopCaches {
    activities: RefCell<BTreeMap<Pid, Option<crate::ThreadActivity>>>,

    labels: RefCell<Option<BTreeMap<Pid, Arc<[crate::ThreadLabel]>>>>,

    task_locators: RefCell<BTreeMap<crate::TaskId, u64>>,

    task_sets: RefCell<Option<runtimes::TaskSets>>,

    interpreted: interpreted::InterpretedCache,

    memory: crate::runtime_model::StopMemory,
}

impl StopCaches {
    fn after_memory_write(&mut self) {
        *self = Self::default();
    }

    fn after_register_write(&mut self) {
        *self = Self {
            task_locators: std::mem::take(&mut self.task_locators),
            memory: std::mem::take(&mut self.memory),
            ..Self::default()
        };
    }
}

static NEXT_STOP_ID: AtomicU64 = AtomicU64::new(1);

fn allocate_stop_id() -> Result<StopId> {
    #[allow(
        deprecated,
        reason = "try_update is not yet stable on the pinned toolchain"
    )]
    let issued = NEXT_STOP_ID
        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
            current.checked_add(1)
        })
        .ok()
        .and_then(|previous| previous.checked_add(1));
    let issued = issued.ok_or_else(|| Error::internal("stop identifiers are exhausted"))?;
    Ok(StopId::new(issued))
}

#[derive(Debug, Clone, Copy)]
struct VforkWindow {
    child: Pid,

    resumed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Tracked {
    Retired,

    Vanished,

    Unseized,

    EarlyStop(WaitEvent),

    ForkChild {
        sites: Vec<(VirtualAddress, u8)>,
        vfork: bool,
    },

    Killed,

    HandedOver,
}

struct Inferior {
    origin: InferiorOrigin,
    tgid: Pid,
    loaded_module: LoadedModule,
    breakpoints: BTreeMap<VirtualAddress, BreakpointSite>,

    plan_sites: BTreeMap<ExecutionId, BTreeSet<VirtualAddress>>,
    threads: BTreeMap<Pid, TraceThread>,

    tracked: BTreeMap<Pid, Tracked>,

    former_sites: BTreeMap<VirtualAddress, u8>,

    observer: bool,

    vfork_windows: BTreeMap<Pid, VforkWindow>,
    waiter: Option<Waiter>,
    run: RunPhase,
    next_execution: u64,

    unfollowed_exec: Option<String>,

    loader_site: Option<VirtualAddress>,

    runtime_hooks: BTreeMap<VirtualAddress, language_exceptions::HookSite>,
    stack_moves: stack_watches::StackMoves,
    interpreted_moves: interpreted_watches::InterpretedMoves,

    managed: managed::ManagedSites,
    agents: BTreeMap<crate::RuntimeId, runtime_agents::Installation>,

    python_launch: Option<std::result::Result<python_launch::PythonLaunch, Arc<str>>>,

    node_launch: Option<std::result::Result<node_launch::NodeLaunch, Arc<str>>>,

    next_runtime_step: u64,

    runtime_step: Option<(crate::RuntimeId, crate::runtime_model::RuntimeStep)>,

    next_runtime_assignment: u64,
    runtime_assignment: Option<(crate::RuntimeId, crate::runtime_model::RuntimeAssignment)>,
    assignment: Option<assignments::PendingAssignment>,
    indirect: indirect::IndirectFunctions,
    inspectors: inspector::Inspectors,
    watch: WatchState,

    terminating: Option<Terminating>,

    holdings: RefCell<holdings::HoldingNumbers>,
}

#[derive(Clone, Copy, Debug)]
struct Terminating {
    signal: Signal,
    delivered: bool,
}

impl Inferior {
    fn new(
        origin: InferiorOrigin,
        tgid: Pid,
        loaded_module: LoadedModule,
        threads: BTreeMap<Pid, TraceThread>,
        waiter: Option<Waiter>,
        run: RunPhase,
    ) -> Self {
        Self {
            origin,
            tgid,
            loaded_module,
            breakpoints: BTreeMap::new(),
            plan_sites: BTreeMap::new(),
            threads,
            tracked: BTreeMap::new(),
            observer: false,
            former_sites: BTreeMap::new(),
            vfork_windows: BTreeMap::new(),
            waiter,
            run,
            next_execution: 0,
            unfollowed_exec: None,
            loader_site: None,
            runtime_hooks: BTreeMap::new(),
            stack_moves: stack_watches::StackMoves::default(),
            interpreted_moves: interpreted_watches::InterpretedMoves::default(),
            managed: managed::ManagedSites::default(),
            agents: BTreeMap::new(),
            python_launch: None,
            node_launch: None,
            next_runtime_step: 0,
            runtime_step: None,
            next_runtime_assignment: 0,
            runtime_assignment: None,
            assignment: None,
            indirect: indirect::IndirectFunctions::default(),
            inspectors: inspector::Inspectors::default(),
            watch: WatchState::default(),
            terminating: None,
            holdings: RefCell::default(),
        }
    }

    /// On Linux, a thread-group leader that exits while worker threads remain alive stays
    /// as a zombie until all siblings exit; treat it as settled so all-stop barriers finish.
    fn exited_leader(&self, pid: Pid, thread: &TraceThread) -> bool {
        pid == self.tgid
            && matches!(thread.state, NativeThreadState::Exiting)
            && self.threads.len() > 1
    }

    fn settled(&self, pid: Pid, thread: &TraceThread) -> bool {
        matches!(thread.state, NativeThreadState::Stopped) || self.exited_leader(pid, thread)
    }

    /// Defers pending signals while repairing a trap, stepping, or resuming a single thread
    /// so signal delivery cannot hijack the step or execute a handler while a trap is lifted.
    fn holds_signals(&self, thread: &TraceThread, expected: &ExpectedStop) -> bool {
        thread.holds_siblings()
            || matches!(expected, ExpectedStop::UserStep { .. })
            || self.run.execution().is_some_and(|active| {
                matches!(active.kind, ActiveKind::Step { .. })
                    || matches!(active.scope, ResumeScope::Thread(_))
            })
    }

    fn repairing(&self) -> Option<(Pid, VirtualAddress)> {
        self.threads
            .iter()
            .find_map(|(&pid, thread)| thread.trap.repairing().map(|at| (pid, at)))
    }

    fn next_repair(&self) -> Option<(Pid, VirtualAddress)> {
        let active = self.run.execution()?;
        active
            .resume_threads
            .iter()
            .filter_map(|&pid| {
                let thread = self.threads.get(&pid)?;
                let AtTrap::MustStepOver { at } = thread.trap else {
                    return None;
                };
                matches!(thread.state, NativeThreadState::Stopped).then_some((at, pid))
            })
            .min()
            .map(|(at, pid)| (pid, at))
    }

    fn vfork_child_runs(&self) -> bool {
        self.vfork_windows.values().any(|window| window.resumed)
    }

    fn repair_under_way(&self) -> bool {
        self.repairing().is_some()
            || self
                .breakpoints
                .values()
                .any(|site| site.trap == Trap::Lifted(Lift::Repair))
    }

    fn trap_event(&mut self, pid: Pid, event: TrapEvent) -> Result<()> {
        let thread = self.thread_mut(pid)?;
        thread.trap = thread.trap.on(event).map_err(|transition| {
            backend_error(LinuxError::UnexpectedWait(format!(
                "thread {pid} at a trap: {transition}"
            )))
        })?;
        Ok(())
    }

    fn memory_thread(&self) -> Pid {
        let stopped = |thread: &TraceThread| matches!(thread.state, NativeThreadState::Stopped);
        if self.threads.get(&self.tgid).is_some_and(stopped) {
            return self.tgid;
        }

        self.threads
            .iter()
            .find_map(|(&pid, thread)| stopped(thread).then_some(pid))
            .unwrap_or(self.tgid)
    }

    fn thread(&self, pid: Pid) -> Result<&TraceThread> {
        self.threads
            .get(&pid)
            .ok_or_else(|| Error::UnknownThread(debug_thread_id(pid)))
    }

    fn thread_mut(&mut self, pid: Pid) -> Result<&mut TraceThread> {
        self.threads
            .get_mut(&pid)
            .ok_or_else(|| Error::UnknownThread(debug_thread_id(pid)))
    }

    fn forget_thread(&mut self, pid: Pid, status: &ExitStatus) {
        if let Some(active) = self.run.execution_mut() {
            active.resume_threads.remove(&pid);
        }

        if let Some(stop) = self.run.stop_mut() {
            stop.presentations.remove(&pid);
            if stop.selected_thread == Some(pid) {
                stop.selected_thread = None;
            }
        }

        let others = || self.threads.iter().filter(|&(&other, _)| other != pid);
        let replacement = others()
            .find(|(_, thread)| matches!(thread.state, NativeThreadState::Stopped))
            .or_else(|| others().next())
            .map(|(&pid, _)| pid);
        if let Some(barrier) = self.run.barrier_mut()
            && let Some(replacement) = replacement
        {
            barrier.thread_exited(pid, replacement, status);
        }
    }

    fn take_pending_edits(&mut self) -> Vec<Edit> {
        self.run
            .barrier_mut()
            .map(StopBarrier::take_edits)
            .unwrap_or_default()
    }

    fn inherited_sites(&self) -> Vec<(VirtualAddress, u8)> {
        let mut sites = self.former_sites.clone();
        sites.extend(
            self.breakpoints
                .iter()
                .map(|(&address, site)| (address, site.original_byte)),
        );
        sites.into_iter().collect()
    }
}

struct Waiter {
    thread: Option<WaiterThread>,

    hub: Option<wait_hub::HubEdge>,
}

struct WaiterThread {
    stop: Arc<AtomicBool>,
    handle: JoinHandle<()>,
}

impl Waiter {
    #[cfg(any(test, feature = "sim"))]
    const fn external() -> Self {
        Self {
            thread: None,
            hub: None,
        }
    }

    fn failed(&self) -> bool {
        self.hub.as_ref().is_some_and(wait_hub::HubEdge::failed)
    }

    fn stop_and_join(self, mut serve: impl FnMut()) -> Result<()> {
        if let Some(hub) = self.hub {
            hub.stop_and_join(&mut serve);
        }

        let Some(thread) = self.thread else {
            return Ok(());
        };

        thread.stop.store(true, Ordering::Release);
        thread.handle.thread().unpark();
        while !thread.handle.is_finished() {
            serve();
            thread::sleep(std::time::Duration::from_millis(1));
        }

        thread
            .handle
            .join()
            .map_err(|_| Error::BackendThreadPanicked)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InferiorOrigin {
    Launched,
    Attached,

    LaunchedByExec,
    PostMortem,
}

impl InferiorOrigin {
    const fn owned(self) -> bool {
        matches!(self, Self::Launched | Self::LaunchedByExec)
    }

    const fn seized(self) -> bool {
        matches!(self, Self::Attached | Self::LaunchedByExec)
    }
}

#[derive(Clone)]
struct MainProgram {
    id: crate::ModuleId,

    variables: Arc<dyn VariableInfo>,
    executable: Arc<PathBuf>,
    identity: FileIdentity,

    c_library: CLibrary,
    image: Arc<ModuleImage>,
    unwind: Arc<dyn UnwindInfo>,
}

impl MainProgram {
    fn loaded(&self, load_bias: crate::LoadBias) -> LoadedModule {
        LoadedModule {
            id: self.id,
            image: self.image.id(),
            load_bias,
            window: None,
        }
    }
}

struct RuntimeModule {
    loaded: LoadedModule,
    image: Arc<ModuleImage>,
    unwind: Arc<dyn UnwindInfo>,
    variables: Arc<dyn VariableInfo>,

    tls: Option<TlsModule>,
}

#[derive(Debug, thiserror::Error)]
enum LinuxError {
    #[error("system tracing operation failed: {0}")]
    System(#[from] nix::Error),

    #[error("failed to parse executable object: {0}")]
    Object(#[from] object::Error),

    #[error("unexpected wait status: {0}")]
    UnexpectedWait(String),

    #[error("could not determine load bias for {0}")]
    LoadBias(PathBuf),

    #[error("invalid process mapping: {0}")]
    InvalidMapping(String),

    #[error("{0} is malformed")]
    ProcFile(String),

    #[error("dynamic-loader rendezvous is malformed: {0}")]
    LoaderRendezvous(String),

    #[error("another Linux tracing session is already active in this process")]
    SessionActive,

    #[error("the target's thread list did not stabilize while attaching")]
    AttachThreadsUnstable,

    #[error("unsupported clone created a different thread group {0}")]
    UnsupportedClone(i32),

    #[error("floating-point register reads are unsupported by this tracing effect")]
    UnsupportedFloatingRegisters,

    #[error("only general registers can be changed")]
    UnsupportedRegisterWrite,

    #[error("{0}")]
    UnsupportedExec(String),

    #[error("could not determine the caller frame: {0}")]
    CallerUnavailable(UnwindTermination),

    #[error("breakpoint site {0} was not found")]
    BreakpointSiteMissing(VirtualAddress),

    #[error("breakpoint site {0} did not have the expected owner")]
    BreakpointOwnerMissing(VirtualAddress),

    #[error("logical breakpoint identifiers were exhausted")]
    BreakpointIdExhausted,

    #[error("watchpoint identifiers were exhausted")]
    WatchpointIdExhausted,

    #[error("watchpoint arming failed ({cause}) and rollback also failed ({recovery})")]
    WatchpointArmRecovery { cause: String, recovery: String },

    #[error("loaded module identifiers were exhausted")]
    ModuleIdExhausted,

    #[error("module image identifiers were exhausted")]
    ModuleImageIdExhausted,

    #[error("breakpoint installation failed ({cause}) and rollback also failed ({recovery})")]
    BreakpointInstallRecovery { cause: String, recovery: String },

    #[error("breakpoint removal failed ({cause}) and rollback also failed ({recovery})")]
    BreakpointRemoveRecovery { cause: String, recovery: String },

    #[error("resume failed ({cause}) and recovery also failed ({recovery})")]
    ResumeRecovery { cause: String, recovery: String },

    #[error("logical memory read of {size} bytes exceeds the {maximum}-byte limit")]
    MemoryReadTooLarge { size: usize, maximum: usize },

    #[error("target memory is inaccessible at {address}")]
    MemoryInaccessible { address: VirtualAddress },

    #[error("the core dump does not describe thread {0}")]
    UnknownCoreThread(i32),

    #[error("the core dump saved no floating-point registers for thread {0}")]
    CoreFloatingRegistersUnsaved(i32),

    #[error("the process exited before its first stop: {0:?}")]
    ExitedBeforeStop(ExitStatus),

    #[error("the process executed a program other than {}", .0.display())]
    ExecutedAnotherProgram(PathBuf),

    #[error(
        "the inferior replaced its program with {}, which microscope cannot load: {reason}",
        path.display()
    )]
    ProgramUnreadable { path: PathBuf, reason: String },
}

struct Controller<P: InspectionOps> {
    _lease: SessionLease,

    main: MainProgram,

    launched: MainProgram,

    program: watch::Sender<Arc<ModuleImage>>,

    launch_path: Arc<PathBuf>,
    expected_process_start_time: Option<u64>,
    modules: BTreeMap<crate::ModuleId, RuntimeModule>,

    debug_files: crate::debug_info::DebugFileSearch,

    kept: Option<crate::backend::KeptModules>,

    mapped_modules: BTreeMap<ModuleMapping, (PathBuf, crate::LoadBias)>,
    next_module_id: u32,

    code_copies: std::collections::BTreeMap<(u64, u64), code_copies::Copy>,
    next_image_id: u32,
    messages: RefCell<mpsc::Receiver<ControllerMessage>>,

    pending: RefCell<VecDeque<ControllerMessage>>,

    alarm: RefCell<Option<alarm::AlarmThread>>,

    views: presentation::Views,
    message_sender: mpsc::Sender<ControllerMessage>,
    events: EventSender,
    ptrace: P,
    session: Session,

    shutdown: Option<Shutdown>,
    breakpoints: breakpoints::UserBreakpoints,
    next_breakpoint_id: u64,
    next_watchpoint_id: u64,

    catchpoints: BTreeMap<crate::CatchpointId, crate::Catchpoint>,
    next_catchpoint_id: u64,
    signals: SignalPolicies,

    exception_stops: crate::ExceptionStops,

    exec_stops: bool,

    ending: Ending,

    group: Option<follow::Membership>,

    step_into_runtime: bool,

    step_skips: crate::StepSkips,

    runtime_models: runtimes::RuntimeCache,
    revision: u64,

    messages_served: u64,

    #[cfg(debug_assertions)]
    panic_after: Option<String>,

    #[cfg(debug_assertions)]
    waiter_panic_after: Option<String>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Ending {
    Kill,

    Release,
}

enum Start {
    Launch(crate::LaunchOptions, Reply<ExecutionId>),
    Attach {
        requested: ProcessId,
        observe: bool,
        reply: Reply<StopId>,
    },
    LaunchByExec {
        requested: ProcessId,
        stop_at_entry: bool,
        release: Box<dyn FnOnce() + Send>,
        reply: Reply<ExecutionId>,
    },

    Adopt {
        pid: Pid,

        launched: bool,
        reply: Reply<StopId>,
    },

    AdoptExec {
        pid: Pid,
        launched: bool,
        reply: Reply<ExecutionId>,
    },
}

#[cfg(debug_assertions)]
const PANIC_AFTER: &str = "MICROSCOPE_TEST_PANIC_AFTER";

#[cfg(debug_assertions)]
const WAITER_PANIC_AFTER: &str = "MICROSCOPE_TEST_WAITER_PANIC_AFTER";

pub fn spawn_controller(
    executable: Executable,
    debug_info: DebugInfo,
    channels: ControllerChannels,
) -> Result<(watch::Receiver<Arc<ModuleImage>>, ControllerJoin)> {
    let program = watch::Sender::new(Arc::clone(&debug_info.image));
    let current = program.subscribe();

    let hosting = tracer::Tracer::alone(Box::new(move |host| {
        Box::new(hosted_controller(
            host, executable, debug_info, channels, program,
        ))
    }))?;

    Ok((current, ControllerJoin::Hosted(hosting)))
}

#[cfg(debug_assertions)]
type HostedEdge = recorded::Recorded<LinuxPtrace>;
#[cfg(not(debug_assertions))]
type HostedEdge = LinuxPtrace;

fn hosted_controller(
    host: &tracer::HostContext<'_>,
    executable: Executable,
    debug_info: DebugInfo,
    channels: ControllerChannels,
    program: watch::Sender<Arc<ModuleImage>>,
) -> Controller<HostedEdge> {
    record!("started for {}", executable.display_path.display());
    let membership = host.membership(channels.sender.clone());
    #[cfg(debug_assertions)]
    let ptrace = recorded::Recorded(LinuxPtrace::hosted(membership));
    #[cfg(not(debug_assertions))]
    let ptrace = LinuxPtrace::hosted(membership);
    Controller::new(
        SessionLease::detached(),
        executable,
        debug_info,
        channels,
        program,
        ptrace,
    )
}

pub struct GroupBackend {
    tracer: Option<tracer::Tracer>,
    thread: Option<JoinHandle<()>>,

    pub notices: tokio::sync::mpsc::UnboundedReceiver<follow::GroupNotice>,
}

impl GroupBackend {
    pub fn join(mut self) -> std::result::Result<(), ()> {
        drop(self.tracer.take());
        self.thread
            .take()
            .map_or(Ok(()), |thread| thread.join().map_err(|_| ()))
    }
}

pub fn spawn_group(
    executable: Executable,
    debug_info: DebugInfo,
    channels: ControllerChannels,
    policy: crate::FollowPolicy,
    capacity: follow::Capacity,
) -> Result<(
    watch::Receiver<Arc<ModuleImage>>,
    ControllerJoin,
    GroupBackend,
)> {
    let program = watch::Sender::new(Arc::clone(&debug_info.image));
    let current = program.subscribe();
    let follows = policy.mode != crate::FollowMode::None;
    let following = Arc::new(follow::Following::new(follow::Policy {
        policy,
        launched: Some(executable.identity.inode),
    }));
    let (notices, received) = tokio::sync::mpsc::unbounded_channel();
    let (tracer, thread) = tracer::Tracer::start(Some(follow::GroupSetup {
        following: Arc::clone(&following),
        notices,
        capacity,
        debug_files: executable.debug_files.clone(),
    }))?;

    let ended = tracer.host(Box::new(move |host| {
        let mut controller = hosted_controller(host, executable, debug_info, channels, program);
        if follows {
            controller.join_group(follow::Membership {
                following,
                root: true,
                parent: None,
                settings_taken: 0,
            });
        }
        Box::new(controller)
    }))?;

    Ok((
        current,
        ControllerJoin::Hosted(tracer::Hosting::member(ended)),
        GroupBackend {
            tracer: Some(tracer),
            thread: Some(thread),
            notices: received,
        },
    ))
}

pub enum ControllerJoin {
    Thread(JoinHandle<()>),

    Hosted(tracer::Hosting),
}

impl ControllerJoin {
    pub fn join(self) -> std::result::Result<(), ()> {
        match self {
            Self::Thread(thread) => thread.join().map_err(|_| ()),
            Self::Hosted(hosting) => hosting.join(),
        }
    }
}

impl<P: LinuxTraceOps> tracer::Hosted for Controller<P> {
    fn next_message(&self) -> Option<ControllerMessage> {
        Self::next_message(self, false)
    }

    fn handle_message(&mut self, message: ControllerMessage) -> bool {
        Self::handle_message(self, message)
    }

    fn poll_queue(&self, context: &mut std::task::Context<'_>) -> bool {
        match self.messages.borrow_mut().poll_recv(context) {
            std::task::Poll::Ready(Some(message)) => {
                self.pending.borrow_mut().push_back(message);
                true
            }
            std::task::Poll::Ready(None) | std::task::Poll::Pending => false,
        }
    }

    fn deadline(&self) -> Option<std::time::Instant> {
        self.agent_deadline()
    }

    fn release_after_panic(&mut self) {
        Self::release_after_panic(self);
    }
}

impl<P: InspectionOps> Controller<P> {
    fn new(
        lease: SessionLease,
        executable: Executable,
        debug_info: DebugInfo,
        channels: ControllerChannels,
        program: watch::Sender<Arc<ModuleImage>>,
        ptrace: P,
    ) -> Self {
        let DebugInfo {
            image: module_image,
            unwind: unwind_info,
            variables: variable_info,
        } = debug_info;
        let main = RuntimeModule {
            loaded: LoadedModule::main(module_image.id(), crate::LoadBias::default()),
            image: Arc::clone(&module_image),
            unwind: Arc::clone(&unwind_info),
            variables: Arc::clone(&variable_info),
            tls: None,
        };
        let launched = MainProgram {
            id: main.loaded.id,
            variables: variable_info,
            executable: executable.display_path,
            identity: executable.identity,
            c_library: executable.c_library,
            image: module_image,
            unwind: unwind_info,
        };

        Self {
            _lease: lease,
            main: launched.clone(),
            launched,
            program,
            launch_path: executable.launch_path,
            expected_process_start_time: executable.process_start_time,
            debug_files: executable.debug_files,
            modules: BTreeMap::from([(main.loaded.id, main)]),
            mapped_modules: BTreeMap::new(),
            next_module_id: 1,
            code_copies: std::collections::BTreeMap::new(),

            next_image_id: executable
                .kept
                .as_ref()
                .map_or(1, |kept| kept.next_image().max(1)),
            kept: executable.kept,
            messages: RefCell::new(channels.receiver),
            pending: RefCell::new(VecDeque::new()),
            alarm: RefCell::new(None),
            views: presentation::Views::default(),
            message_sender: channels.sender,
            events: channels.events,
            ptrace,
            session: Session::idle(),
            shutdown: None,
            breakpoints: breakpoints::UserBreakpoints::default(),
            next_breakpoint_id: 1,
            catchpoints: BTreeMap::new(),
            next_catchpoint_id: 1,
            next_watchpoint_id: 1,
            signals: SignalPolicies::default(),
            exception_stops: crate::ExceptionStops::default(),
            exec_stops: false,
            ending: Ending::Kill,
            group: None,
            step_into_runtime: false,
            step_skips: crate::StepSkips::default(),
            runtime_models: RefCell::default(),
            revision: 0,
            messages_served: 0,

            #[cfg(debug_assertions)]
            panic_after: std::env::var(PANIC_AFTER).ok(),

            #[cfg(debug_assertions)]
            waiter_panic_after: std::env::var(WAITER_PANIC_AFTER).ok(),
        }
    }
}

impl<P: InspectionOps> Controller<P> {
    fn drain_messages(&self) {
        let mut messages = self.messages.borrow_mut();
        let mut pending = self.pending.borrow_mut();
        while let Ok(message) = messages.try_recv() {
            pending.push_back(message);
        }
    }

    fn next_message(&self, block: bool) -> Option<ControllerMessage> {
        self.drain_messages();
        let deadline = self.agent_deadline();
        if deadline.is_some_and(|deadline| deadline <= std::time::Instant::now()) {
            return Some(ControllerMessage::AgentTimeout);
        }

        if deadline.is_some() {
            crate::count!("agent_outstanding_messages", 1);
        }

        let mut pending = self.pending.borrow_mut();
        if let Some(preempting) = pending
            .iter()
            .position(ControllerMessage::preempts_inspection)
        {
            let index = pending
                .iter()
                .take(preempting + 1)
                .position(|message| !message.reads_one_stop())
                .unwrap_or(preempting);
            return pending.remove(index);
        }

        if let Some(message) = pending.pop_front() {
            return Some(message);
        }

        drop(pending);
        if !block {
            return None;
        }

        if let Some(deadline) = deadline
            && !self.set_alarm(deadline)
        {
            return self.poll_messages();
        }

        self.messages.borrow_mut().blocking_recv()
    }

    fn agent_deadline(&self) -> Option<std::time::Instant> {
        self.session.inferior().and_then(|inferior| {
            inferior
                .agents
                .values()
                .filter_map(runtime_agents::Installation::deadline)
                .chain(inferior.inspectors.interrupt)
                .chain(inferior.inspectors.assignment_deadline())
                .chain(
                    inferior
                        .inspectors
                        .cleanup
                        .as_ref()
                        .and_then(inspector::teardown::Cleanup::deadline),
                )
                .min()
        })
    }

    fn set_alarm(&self, deadline: std::time::Instant) -> bool {
        let mut alarm = self.alarm.borrow_mut();
        if alarm.is_none() {
            let sender = self.message_sender.clone();
            match alarm::AlarmThread::spawn(move || {
                let _ = sender.try_send(ControllerMessage::AgentTimeout);
            }) {
                Ok(started) => *alarm = Some(started),
                #[cfg_attr(
                    not(debug_assertions),
                    expect(unused_variables, reason = "only recorded")
                )]
                Err(error) => {
                    record!("the alarm did not start: {error}");
                    return false;
                }
            }
        }

        alarm.as_ref().is_some_and(|alarm| {
            alarm.set(deadline);
            true
        })
    }

    fn poll_messages(&self) -> Option<ControllerMessage> {
        loop {
            let Some(deadline) = self.agent_deadline() else {
                return self.messages.borrow_mut().blocking_recv();
            };
            let now = std::time::Instant::now();
            if now >= deadline {
                return Some(ControllerMessage::AgentTimeout);
            }
            match self.messages.borrow_mut().try_recv() {
                Ok(message) => return Some(message),
                Err(mpsc::error::TryRecvError::Disconnected) => return None,
                Err(mpsc::error::TryRecvError::Empty) => {}
            }
            crate::count!("agent_poll_sleeps", 1);
            thread::sleep((deadline - now).min(std::time::Duration::from_millis(5)));
        }
    }

    fn run_control_waiting(&self) -> bool {
        self.drain_messages();
        self.pending
            .borrow()
            .iter()
            .any(ControllerMessage::preempts_inspection)
    }

    fn join_waiter(&self, waiter: Waiter) -> Result<()> {
        waiter.stop_and_join(|| self.drain_messages())
    }

    fn serve_later(&self, request: Request) {
        record!("interrupted {}", request.describe());
        self.pending
            .borrow_mut()
            .push_front(ControllerMessage::Request(request));
    }
}

impl<P: LinuxTraceOps> Controller<P> {
    fn handle_message(&mut self, message: ControllerMessage) -> bool {
        self.take_group_settings();
        let keeps_running = match message {
            ControllerMessage::Request(request) => {
                record!("request {}", request.describe());
                let _span = crate::span!("request", "{}", request.describe());

                #[cfg(debug_assertions)]
                let injected = self
                    .panic_after
                    .as_ref()
                    .is_some_and(|prefix| request.describe().starts_with(prefix.as_str()));
                #[cfg(debug_assertions)]
                let waiter_injected = self
                    .waiter_panic_after
                    .as_ref()
                    .is_some_and(|prefix| request.describe().starts_with(prefix.as_str()));
                let keeps_running = self.handle_request(request);

                #[cfg(debug_assertions)]
                if waiter_injected {
                    wait_hub::inject_panic();
                }

                #[cfg(debug_assertions)]
                assert!(!injected, "a panic injected by {PANIC_AFTER}");
                keeps_running
            }
            ControllerMessage::AgentTimeout => {
                let now = std::time::Instant::now();
                if let Err(error) = self
                    .agent_timeout(now)
                    .and_then(|()| self.interrupt_expired(now))
                    .and_then(|()| self.assignment_expired(now))
                    .and_then(|()| self.inspector_cleanup_timeout(now))
                {
                    self.fail_inferior(error);
                }
                true
            }
            ControllerMessage::Inspector(message) => {
                if let Err(error) = self.inspector_message(message) {
                    self.fail_inferior(error);
                }
                true
            }
            ControllerMessage::Wait(status) => {
                let _span = crate::span!("wait");
                self.handle_wait(status)
            }
        };

        if cfg!(debug_assertions) {
            self.messages_served += 1;
            if self
                .messages_served
                .is_multiple_of(self.invariant_check_stride())
            {
                self.check_invariants();
            }
        }

        let keeps_running = keeps_running && !(self.shutdown.is_some() && self.session.is_idle());
        if !keeps_running {
            self.answer_queued_shutdowns();
        }
        keeps_running
    }

    fn answer_queued_shutdowns(&self) {
        self.messages.borrow_mut().close();
        self.drain_messages();
        for message in self.pending.borrow_mut().drain(..) {
            if let ControllerMessage::Request(Request::Shutdown { reply }) = message {
                let _ = reply.send(Ok(()));
            }
        }
    }

    #[expect(
        clippy::too_many_lines,
        reason = "the exhaustive request dispatcher keeps protocol routing in one place"
    )]
    fn handle_request(&mut self, request: Request) -> bool {
        match request {
            Request::AddBreakpoint {
                spec,
                options,
                reply,
            } => {
                self.edit(Edit::AddBreakpoint {
                    spec,
                    options,
                    reply,
                });
            }

            Request::SetBreakpointCondition {
                id,
                condition,
                reply,
            } => {
                let _ = reply.send(self.set_breakpoint_condition(id, condition));
            }

            Request::SetBreakpointHitCondition {
                id,
                hit_condition,
                reply,
            } => {
                let _ = reply.send(self.set_breakpoint_hit_condition(id, hit_condition));
            }

            Request::SetBreakpointLogMessage {
                id,
                log_message,
                reply,
            } => {
                let _ = reply.send(self.set_breakpoint_log_message(id, log_message));
            }
            Request::SetBreakpointEnabled { id, enabled, reply } => {
                self.edit(Edit::SetBreakpointEnabled { id, enabled, reply });
            }
            Request::RemoveBreakpoint { id, reply } => {
                self.edit(Edit::RemoveBreakpoint { id, reply });
            }
            Request::RemoveAllBreakpoints { reply } => {
                self.edit(Edit::RemoveAllBreakpoints { reply });
            }

            Request::AddWatchpoint {
                spec,
                access,
                options,
                reply,
            } => {
                self.edit(Edit::AddWatchpoint {
                    spec,
                    access,
                    options,
                    reply,
                });
            }

            Request::SetWatchpointCondition {
                id,
                condition,
                reply,
            } => {
                let _ = reply.send(self.set_watchpoint_condition(id, condition));
            }

            Request::SetWatchpointHitCondition {
                id,
                hit_condition,
                reply,
            } => {
                let _ = reply.send(self.set_watchpoint_hit_condition(id, hit_condition));
            }
            Request::SetWatchpointEnabled { id, enabled, reply } => {
                self.edit(Edit::SetWatchpointEnabled { id, enabled, reply });
            }
            Request::RemoveWatchpoint { id, reply } => {
                self.edit(Edit::RemoveWatchpoint { id, reply });
            }
            Request::RemoveAllWatchpoints { reply } => {
                self.edit(Edit::RemoveAllWatchpoints { reply });
            }
            Request::SetExceptionStops { stops, reply } => {
                self.share(follow::Setting::ExceptionStops(stops));
                self.edit(Edit::SetExceptionStops { stops, reply });
            }
            Request::SetExecStops { stops, reply } => {
                self.share(follow::Setting::ExecStops(stops));
                let _ = reply.send(Ok(std::mem::replace(&mut self.exec_stops, stops)));
            }
            Request::ReleaseOnShutdown { reply } => {
                self.ending = Ending::Release;
                let _ = reply.send(Ok(()));
            }

            Request::AddCatchpoint {
                calls,
                phase,
                condition,
                reply,
            } => self.edit(Edit::AddCatchpoint {
                id: None,
                calls,
                phase,
                condition,
                reply,
            }),
            Request::RemoveCatchpoint { id, reply } => {
                self.edit(Edit::RemoveCatchpoint { id, reply });
            }
            Request::Launch { options, reply } => self.start(Start::Launch(*options, reply)),
            Request::Attach {
                process_id,
                observe,
                reply,
            } => self.start(Start::Attach {
                requested: process_id,
                observe,
                reply,
            }),
            Request::LaunchByExec {
                process_id,
                stop_at_entry,
                release,
                reply,
            } => self.start(Start::LaunchByExec {
                requested: process_id,
                stop_at_entry,
                release,
                reply,
            }),
            Request::Continue {
                process_id,
                stop_id,
                scope,
                exception,
                reply,
            } => self.resume(process_id, stop_id, scope, exception, reply),
            Request::Step {
                process_id,
                stop_id,
                context,
                frame,
                kind,
                call,
                scope,
                exception,
                reply,
            } => match self.context_thread(stop_id, context) {
                Ok(pid) => self.step(
                    process_id,
                    stop_id,
                    pid,
                    frame,
                    (kind, call),
                    scope,
                    exception,
                    reply,
                ),
                Err(Error::TaskParked(task)) if call.is_none() => {
                    let result = self.step_suspended(
                        (process_id, stop_id),
                        task,
                        frame,
                        kind,
                        scope,
                        exception,
                    );
                    self.reply_execution(result, scope, reply);
                }
                Err(error) => {
                    let _ = reply.send(Err(error));
                }
            },
            Request::Advance {
                process_id,
                stop_id,
                context,
                frame,
                spec,
                scope,
                exception,
                reply,
            } => match self.context_thread(stop_id, context) {
                Ok(pid) => self.advance(
                    process_id, stop_id, pid, frame, spec, scope, exception, reply,
                ),
                Err(error) => {
                    let _ = reply.send(Err(error));
                }
            },
            Request::Jump {
                process_id,
                stop_id,
                context,
                spec,
                reply,
            } => match self.context_thread(stop_id, context) {
                Ok(pid) => self.jump(process_id, stop_id, pid, spec, reply),
                Err(error) => {
                    let _ = reply.send(Err(error));
                }
            },
            Request::Pause { process_id, reply } => {
                let _ = reply.send(self.begin_pause(process_id));
            }

            Request::WriteMemory {
                process_id,
                stop_id,
                address,
                bytes,
                reply,
            } => {
                let _ = reply.send(self.write_memory(process_id, stop_id, address, &bytes));
            }

            Request::Evaluate {
                expression,
                mode: crate::EvaluationMode::Assign,
                limits,
                stop_id,
                context,
                frame,
                reply,
            } => {
                let result = self.stack_root(stop_id, context).and_then(|root| {
                    self.evaluate_assigning(stop_id, &root, frame, &expression, limits)
                });
                match result {
                    Ok(evaluation::Assigning::Made(evaluation, moved)) => {
                        let _ = reply.send(Ok(*evaluation));
                        if let Some(moved) = moved {
                            let _ = self.events.send(moved);
                        }
                    }

                    Ok(evaluation::Assigning::Running) => self.await_assignment(reply),
                    Err(error) => {
                        let _ = reply.send(Err(error));
                    }
                }
            }
            Request::Shutdown { reply } => {
                let idle = self.session.is_idle();
                match &mut self.shutdown {
                    None => self.begin_shutdown(Some(reply)),
                    Some(_) if idle => {
                        let _ = reply.send(Ok(()));
                    }

                    Some(Shutdown {
                        reply: waiting @ None,
                    }) => *waiting = Some(reply),
                    Some(_) => {
                        let _ = reply.send(Err(Error::RequestCancelled));
                    }
                }

                return !self.session.is_idle();
            }
            Request::Kill { reply } => self.kill(reply),
            Request::Terminate { reply } => {
                let _ = reply.send(self.terminate());
            }
            request => self.handle_inspection_request(request),
        }

        true
    }
}

impl<P: InspectionOps> Controller<P> {
    #[expect(
        clippy::too_many_lines,
        reason = "the exhaustive inspection dispatcher keeps protocol routing in one place"
    )]
    fn handle_inspection_request(&mut self, request: Request) {
        match request {
            Request::ReadMemory {
                process_id,
                stop_id,
                address,
                byte_count,
                reply,
            } => {
                let _ = reply.send(self.read_memory(process_id, stop_id, address, byte_count));
            }

            Request::Tasks {
                stop_id,
                from,
                limit,
                program_only,
                reply,
            } => {
                let _ = reply.send(self.tasks(stop_id, from, limit, program_only));
            }
            Request::LoadedModule { reply } => {
                let _ = reply.send(self.loaded_module());
            }
            Request::LoadedModules { reply } => {
                let _ = reply.send(self.loaded_modules());
            }
            Request::ModuleImage { module, reply } => {
                let _ = reply.send(
                    self.modules
                        .get(&module)
                        .map(|module| Arc::clone(&module.image))
                        .ok_or(Error::ModuleNotLoaded(module)),
                );
            }

            Request::Disassemble {
                query,
                stop_id,
                context,
                reply,
            } => {
                let _ = reply.send(
                    self.context_thread(stop_id, context)
                        .and_then(|pid| self.disassemble(stop_id, pid, query)),
                );
            }

            Request::DescribeAddress {
                stop_id,
                address,
                reply,
            } => {
                let _ = reply.send(self.describe_address(stop_id, address));
            }

            Request::StoppedLocation {
                stop_id,
                context,
                frame,
                reply,
            } => {
                let _ = reply.send(
                    self.stack_root(stop_id, context)
                        .and_then(|root| self.stopped_location(stop_id, &root, frame)),
                );
            }
            Request::Snapshot { reply } => {
                let _ = reply.send(Ok(self.snapshot()));
            }
            Request::StoppedSelection { reply } => {
                let _ = reply.send(self.stopped_selection().map(|mut selection| {
                    selection.interpreted = self.selects_interpreted(&selection);
                    selection
                }));
            }

            Request::Backtrace {
                stop_id,
                context,
                reply,
            } => {
                let _ = reply.send(
                    self.stack_root(stop_id, context)
                        .and_then(|root| self.backtrace(stop_id, &root)),
                );
            }

            Request::StepTargets {
                stop_id,
                context,
                reply,
            } => {
                let _ = reply.send(
                    self.context_thread(stop_id, context)
                        .and_then(|pid| self.step_targets(stop_id, pid)),
                );
            }

            Request::Registers {
                stop_id,
                context,
                frame,
                reply,
            } => {
                let _ = reply.send(
                    self.stack_root(stop_id, context)
                        .and_then(|root| self.registers(stop_id, &root, frame)),
                );
            }

            Request::Variables {
                query,
                limits,
                stop_id,
                context,
                frame,
                reply,
            } => {
                let result = self
                    .stack_root(stop_id, context)
                    .and_then(|root| self.variables(stop_id, &root, frame, &query, limits));
                if matches!(result, Err(Error::Interrupted)) {
                    self.serve_later(Request::Variables {
                        query,
                        limits,
                        stop_id,
                        context,
                        frame,
                        reply,
                    });
                } else {
                    let _ = reply.send(result);
                }
            }

            Request::Evaluate {
                expression,
                mode,
                limits,
                stop_id,
                context,
                frame,
                reply,
            } => {
                let result = self.stack_root(stop_id, context).and_then(|root| {
                    self.evaluate(stop_id, &root, frame, &expression, mode, limits)
                });
                if matches!(result, Err(Error::Interrupted)) {
                    self.serve_later(Request::Evaluate {
                        expression,
                        mode,
                        limits,
                        stop_id,
                        context,
                        frame,
                        reply,
                    });
                } else {
                    let _ = reply.send(result);
                }
            }

            Request::ExpressionType {
                expression,
                stop_id,
                context,
                frame,
                reply,
            } => {
                let _ = reply.send(
                    self.stack_root(stop_id, context)
                        .and_then(|root| self.expression_type(stop_id, &root, frame, &expression)),
                );
            }

            Request::Dereference {
                reference,
                limits,
                reply,
            } => {
                let result = self.dereference(&reference, limits);
                if matches!(result, Err(Error::Interrupted)) {
                    self.serve_later(Request::Dereference {
                        reference,
                        limits,
                        reply,
                    });
                } else {
                    let _ = reply.send(result);
                }
            }

            Request::ValueChildren {
                reference,
                query,
                limits,
                reply,
            } => {
                let result = self.value_children(&reference, &query, limits);
                if matches!(result, Err(Error::Interrupted)) {
                    self.serve_later(Request::ValueChildren {
                        reference,
                        query,
                        limits,
                        reply,
                    });
                } else {
                    let _ = reply.send(result);
                }
            }

            Request::VisualizerInputs {
                reference,
                limits,
                reply,
            } => {
                let result = self.visualizer_inputs(&reference, limits);
                if matches!(result, Err(Error::Interrupted)) {
                    self.serve_later(Request::VisualizerInputs {
                        reference,
                        limits,
                        reply,
                    });
                } else {
                    let _ = reply.send(result);
                }
            }

            Request::Numbers {
                reference,
                most_bytes,
                reply,
            } => {
                let _ = reply.send(self.numbers(&reference, most_bytes));
            }
            Request::Renderers { reply } => {
                let _ = reply.send(Ok(self.renderers()));
            }
            Request::Globals { query, reply } => {
                let _ = reply.send(self.globals(&query));
            }
            Request::SetViews { views, reply } => {
                self.share(follow::Setting::Views(Arc::clone(&views)));
                self.views.replace(views);
                let _ = reply.send(Ok(()));
            }
            Request::EnableViews { enabled, reply } => {
                self.share(follow::Setting::ViewsEnabled(enabled));
                self.views.enabled = enabled;
                let _ = reply.send(Ok(()));
            }
            Request::SetStepIntoRuntime { enter, reply } => {
                self.share(follow::Setting::StepIntoRuntime(enter));
                let _ = reply.send(Ok(std::mem::replace(&mut self.step_into_runtime, enter)));
            }
            Request::SetStepSkips { skips, reply } => {
                self.share(follow::Setting::StepSkips(skips.clone()));
                self.step_skips = skips;
                let _ = reply.send(Ok(()));
            }
            Request::ExplainType { name, reply } => {
                let _ = reply.send(Ok(self.explain_type(&name)));
            }
            Request::CheckViews { reply } => {
                let _ = reply.send(Ok(self.check_views()));
            }

            Request::ExplainView {
                expression,
                stop_id,
                context,
                frame,
                reply,
            } => {
                let result = self
                    .stack_root(stop_id, context)
                    .and_then(|root| self.explain_view(stop_id, &root, frame, &expression));
                if matches!(result, Err(Error::Interrupted)) {
                    self.serve_later(Request::ExplainView {
                        expression,
                        stop_id,
                        context,
                        frame,
                        reply,
                    });
                } else {
                    let _ = reply.send(result);
                }
            }

            Request::RecordKernels {
                expression,
                stop_id,
                context,
                frame,
                reply,
            } => {
                let result = self
                    .stack_root(stop_id, context)
                    .and_then(|root| self.record_kernels(stop_id, &root, frame, &expression));
                if matches!(result, Err(Error::Interrupted)) {
                    self.serve_later(Request::RecordKernels {
                        expression,
                        stop_id,
                        context,
                        frame,
                        reply,
                    });
                } else {
                    let _ = reply.send(result);
                }
            }

            Request::SelectContext {
                stop_id,
                context,
                reply,
            } => {
                let result = self.select_context(stop_id, context);
                let _ = reply.send(result);
            }

            Request::SelectFrame {
                stop_id,
                context,
                frame,
                reply,
            } => {
                let result = self.select_frame(stop_id, context, frame);
                let _ = reply.send(result);
            }

            Request::ResolveWatchTarget {
                expression,
                stop_id,
                context,
                frame,
                reply,
            } => {
                let _ =
                    reply.send(self.context_thread(stop_id, context).and_then(|pid| {
                        self.resolve_watch_target(stop_id, pid, frame, &expression)
                    }));
            }
            Request::SignalPolicy { signal, reply } => {
                let _ = reply.send(
                    Signal::from_code(signal)
                        .map(|signal| self.signals.get(signal))
                        .ok_or(Error::UnknownSignal(signal)),
                );
            }

            Request::SetSignalPolicy {
                signal,
                policy,
                reply,
            } => {
                let _ = reply.send(match Signal::from_code(signal) {
                    Some(signal) => {
                        self.share(follow::Setting::Signal(signal, policy));
                        Ok(self.signals.set(signal, policy))
                    }
                    None => Err(Error::UnknownSignal(signal)),
                });
            }

            Request::AddBreakpoint { .. }
            | Request::SetBreakpointHitCondition { .. }
            | Request::SetBreakpointCondition { .. }
            | Request::SetBreakpointLogMessage { .. }
            | Request::SetBreakpointEnabled { .. }
            | Request::RemoveBreakpoint { .. }
            | Request::RemoveAllBreakpoints { .. }
            | Request::AddWatchpoint { .. }
            | Request::SetWatchpointCondition { .. }
            | Request::SetWatchpointHitCondition { .. }
            | Request::SetWatchpointEnabled { .. }
            | Request::RemoveWatchpoint { .. }
            | Request::RemoveAllWatchpoints { .. }
            | Request::SetExceptionStops { .. }
            | Request::SetExecStops { .. }
            | Request::ReleaseOnShutdown { .. }
            | Request::AddCatchpoint { .. }
            | Request::RemoveCatchpoint { .. }
            | Request::Launch { .. }
            | Request::Attach { .. }
            | Request::LaunchByExec { .. }
            | Request::Continue { .. }
            | Request::Step { .. }
            | Request::Advance { .. }
            | Request::Jump { .. }
            | Request::Pause { .. }
            | Request::WriteMemory { .. }
            | Request::Kill { .. }
            | Request::Terminate { .. }
            | Request::Shutdown { .. } => {
                debug_assert!(
                    false,
                    "run-control requests are routed by the session dispatcher"
                );
            }
        }
    }
}

impl<P: LinuxTraceOps> Controller<P> {
    fn handle_wait(&mut self, status: WaitEvent) -> bool {
        if matches!(self.session, Session::Draining { .. }) {
            return self.handle_orphan_wait(status);
        }

        if self.shutdown.is_some() && !self.inspector_cleanup_running() {
            return self.handle_shutdown_wait(status);
        }

        let reported = (!matches!(
            status,
            WaitEvent::Exited(..) | WaitEvent::Signaled(..) | WaitEvent::Continued(_)
        ))
        .then(|| status.pid());

        if let Err(error) = self.process_wait(status)
            && !(is_vanished_tracee(&error)
                && (self.release_superseded_threads(reported) || self.every_thread_exiting()))
        {
            self.fail_inferior(error);
        }

        true
    }

    fn lost_to_sigkill(&self, error: &Error) -> bool {
        is_vanished_tracee(error)
            && self.session.inferior().is_some_and(|inferior| {
                inferior.threads.iter().any(|(&pid, thread)| {
                    matches!(thread.state, NativeThreadState::Stopped)
                        && is_superseded(&self.ptrace.signal_metadata(pid))
                })
            })
    }

    fn every_thread_exiting(&self) -> bool {
        self.session.inferior().is_some_and(|inferior| {
            inferior
                .threads
                .values()
                .all(|thread| matches!(thread.state, NativeThreadState::Exiting))
        })
    }

    fn release_superseded_threads(&mut self, handled: Option<Pid>) -> bool {
        let Some(inferior) = self.session.inferior_mut() else {
            return false;
        };

        let mut found = false;
        for (&pid, thread) in &mut inferior.threads {
            if (matches!(thread.state, NativeThreadState::Stopped) || handled == Some(pid))
                && is_superseded(&self.ptrace.signal_metadata(pid))
            {
                record!("{pid} left its stop while it was handled");
                thread.state = NativeThreadState::Running;
                found = true;
            }
        }

        found
    }

    fn process_wait(&mut self, status: WaitEvent) -> Result<()> {
        let pid = status.pid();
        let inferior = self.session.inferior().ok_or(Error::NotRunning)?;
        if !inferior.threads.contains_key(&pid) {
            if self.absorb_untracked_wait(&status) {
                return Ok(());
            }

            return Err(backend_error(LinuxError::UnexpectedWait(format!(
                "unowned {status:?}"
            ))));
        }

        if matches!(inferior.thread(pid)?.expected, ExpectedStop::AdoptedExec) {
            match status {
                WaitEvent::Stopped(pid, signal) => {
                    return self.ptrace.continue_execution(pid, Some(signal));
                }
                WaitEvent::PtraceEvent(pid, _, libc::PTRACE_EVENT_STOP) => {
                    return self.ptrace.continue_execution(pid, None);
                }
                _ => {}
            }
        }

        match status {
            WaitEvent::Exited(pid, code) => {
                self.handle_terminal(pid, ExitStatus::Code(i64::from(code)))
            }
            WaitEvent::Signaled(pid, signal, _) => {
                self.handle_terminal(pid, ExitStatus::Terminated(exception_info(signal)))
            }
            WaitEvent::PtraceEvent(pid, signal, libc::PTRACE_EVENT_STOP) => {
                self.handle_event_stop(pid, signal)
            }
            WaitEvent::PtraceEvent(pid, _, event) => self.handle_ptrace_event(pid, event),
            WaitEvent::PtraceSyscall(pid) => self.handle_syscall_stop(pid),
            WaitEvent::Stopped(pid, signal) => {
                let initial = self
                    .session
                    .inferior()
                    .and_then(|inferior| inferior.threads.get(&pid))
                    .is_some_and(|thread| matches!(thread.expected, ExpectedStop::InitialExec));
                if initial && signal == Signal::SIGTRAP {
                    self.handle_initial_stop(pid)
                } else {
                    let stop = self.classify_stop(pid, signal);
                    self.handle_classified_stop(pid, stop)
                }
            }
            other @ WaitEvent::Continued(_) => Err(backend_error(LinuxError::UnexpectedWait(
                format!("{other:?}"),
            ))),
        }
    }

    fn handle_event_stop(&mut self, pid: Pid, signal: Signal) -> Result<()> {
        if is_superseded(&self.ptrace.signal_metadata(pid)) {
            return self.handle_classified_stop(pid, ClassifiedStop::Superseded);
        }

        let thread = self
            .session
            .inferior()
            .ok_or(Error::NotRunning)?
            .thread(pid)?;
        if matches!(thread.expected, ExpectedStop::InitialAttach) {
            return self.handle_initial_attach_stop(pid);
        }

        let stop = if matches!(thread.state, NativeThreadState::Starting)
            && matches!(thread.expected, ExpectedStop::None)
        {
            ClassifiedStop::ThreadStart
        } else if thread.debugger_stop_pending {
            ClassifiedStop::DebuggerRequested
        } else if is_stopping_signal(signal) {
            ClassifiedStop::GroupStop(signal)
        } else if signal == Signal::SIGTRAP {
            ClassifiedStop::DebuggerRequested
        } else {
            return self.handle_ptrace_event(pid, libc::PTRACE_EVENT_STOP);
        };

        self.handle_classified_stop(pid, stop)
    }
}

impl<P: InspectionOps> Controller<P> {
    fn snapshot(&self) -> StateSnapshot {
        let Some(inferior) = self.session.inferior() else {
            return StateSnapshot {
                revision: self.revision,
                inferior: self
                    .session
                    .ended()
                    .map_or(InferiorState::NotStarted, session::Ended::state),
                stop_id: None,
                selected: None,
                selected_frame: None,
                threads: Arc::from([]),
                presentation: None,
                breakpoints: self.breakpoint_snapshot(),
                watchpoints: Arc::from([]),
                catchpoints: self.catchpoints.values().cloned().collect(),
            };
        };

        let process_id = process_id(inferior.tgid);
        let state = inferior.run.stop().map_or_else(
            || InferiorState::Running {
                process_id,
                execution_id: inferior.run.execution().map(|active| active.id),
            },
            |stop| InferiorState::Stopped {
                process_id,
                stop_id: stop.id,
                thread_id: debug_thread_id(stop.triggering_thread),
                reason: stop.reason.clone(),
            },
        );
        let threads = inferior
            .threads
            .iter()
            .filter(|&(&pid, thread)| !inferior.exited_leader(pid, thread))
            .map(|(&pid, thread)| ThreadSnapshot {
                id: debug_thread_id(pid),
                name: thread.name.clone(),
                activity: matches!(thread.state, NativeThreadState::Stopped)
                    .then(|| self.thread_activity(inferior, pid))
                    .flatten(),
                labels: if matches!(thread.state, NativeThreadState::Stopped) {
                    self.thread_labels(inferior, pid)
                } else {
                    Arc::from([])
                },
                state: if matches!(thread.state, NativeThreadState::Stopped) {
                    ObservableThreadState::Stopped {
                        reason: thread.reason.clone(),
                    }
                } else {
                    ObservableThreadState::Running
                },
            })
            .collect::<Vec<_>>()
            .into();

        StateSnapshot {
            revision: self.revision,
            inferior: state,
            stop_id: inferior.run.stop().map(|stop| stop.id),
            selected: inferior.run.stop().map(|stop| stop.selected),
            selected_frame: inferior.run.stop().map(selected_frame),
            threads,
            presentation: inferior.run.stop().and_then(|stop| {
                stop.selected_thread
                    .and_then(|pid| stop.presentations.get(&pid))
                    .cloned()
            }),
            breakpoints: self.breakpoint_snapshot(),
            watchpoints: inferior
                .watch
                .watchpoints
                .values()
                .map(|record| record.watchpoint.clone())
                .collect::<Vec<_>>()
                .into(),
            catchpoints: self.catchpoints.values().cloned().collect(),
        }
    }

    fn stopped_selection(&self) -> Result<crate::StoppedSelection> {
        let inferior = self.session.inferior().ok_or(Error::NotRunning)?;
        let stop = inferior.run.stop().ok_or(Error::NotStopped)?;
        Ok(crate::StoppedSelection {
            process: process_id(inferior.tgid),
            stop: stop.id,
            execution: stop.selected,
            frame: selected_frame(stop),
            interpreted: false,
        })
    }
}

fn selected_frame(stop: &PublicStop) -> StackFrameId {
    stop.selected_frames
        .get(&stop.selected)
        .copied()
        .unwrap_or(StackFrameId::INNERMOST)
}

impl PublicStop {
    fn new(
        id: StopId,
        triggering_thread: Pid,
        reason: StopReason,
        presentations: BTreeMap<Pid, FramePresentation>,
    ) -> Self {
        Self {
            id,
            triggering_thread,
            reason,
            presentations,
            selected: ExecutionContext::Thread(debug_thread_id(triggering_thread)),
            selected_thread: Some(triggering_thread),
            selected_frames: BTreeMap::new(),
            caches: StopCaches::default(),
            returned: None,
        }
    }

    fn reader(&self) -> Pid {
        self.selected_thread.unwrap_or(self.triggering_thread)
    }
}

impl<P: InspectionOps> Controller<P> {
    fn bump_revision(&mut self) {
        self.revision = self.revision.wrapping_add(1);
        let _ = self.events.send(DebuggerEvent::StateChanged {
            revision: self.revision,
        });
    }
}

fn validate_process(inferior: &Inferior, requested: ProcessId) -> Result<()> {
    if process_id(inferior.tgid) == requested {
        Ok(())
    } else {
        Err(Error::NotRunning)
    }
}

fn validate_public_stop(inferior: &Inferior, requested: Option<StopId>) -> Result<()> {
    match (inferior.run.stop(), requested) {
        (Some(current), Some(requested)) if current.id == requested => Ok(()),
        (Some(_), Some(_)) => Err(Error::StaleStop),
        (Some(_), None) => Ok(()),
        (None, _) => Err(Error::NotStopped),
    }
}

fn validate_resumable(inferior: &Inferior) -> Result<()> {
    if inferior
        .run
        .stop()
        .is_some_and(|stop| matches!(stop.reason, StopReason::Unclassifiable { .. }))
    {
        return Err(Error::UnclassifiableStop);
    }

    validate_image_current(inferior)
}

fn validate_image_current(inferior: &Inferior) -> Result<()> {
    if let Some(reason) = &inferior.unfollowed_exec {
        return Err(backend_error(LinuxError::UnsupportedExec(reason.clone())));
    }

    Ok(())
}

fn validate_stopped_thread(inferior: &Inferior, pid: Pid) -> Result<()> {
    let thread = inferior
        .threads
        .get(&pid)
        .ok_or_else(|| Error::UnknownThread(debug_thread_id(pid)))?;
    if matches!(thread.state, NativeThreadState::Stopped) {
        Ok(())
    } else {
        Err(Error::NotStopped)
    }
}

fn scoped_threads(inferior: &Inferior, scope: ResumeScope) -> Result<BTreeSet<Pid>> {
    match scope {
        ResumeScope::Process(requested) => {
            validate_process(inferior, requested)?;
            Ok(inferior.threads.keys().copied().collect())
        }
        ResumeScope::Thread(thread) => {
            let pid = debug_pid(thread)?;
            validate_stopped_thread(inferior, pid)?;
            Ok(BTreeSet::from([pid]))
        }
    }
}

#[cfg(test)]
mod tests;
