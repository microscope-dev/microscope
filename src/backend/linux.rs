//! The Linux x86-64 backend.
//!
//! One [`Controller`] per session owns all debugger state and runs on the
//! thread that created the tracee, which every ptrace call requires. A waiter
//! thread reports `waitpid` statuses back to it as messages. This module holds
//! the shared state and request dispatch; the submodules own one concern each:
//!
//! - [`native`]: the ptrace, waitpid, and `/proc` edge, behind traits that
//!   unit tests fake.
//! - [`lifecycle`]: launch, attach, thread and fork tracking, exec, shutdown.
//! - [`classify`]: turning raw wait statuses into classified stops.
//! - [`run_control`]: resuming threads, repairing breakpoint sites, and
//!   publishing all-stop snapshots.
//! - [`internal_stops`]: stopping every thread without publishing a stop,
//!   to edit breakpoints and watchpoints or repair a site while running.
//! - [`stepping`]: source and instruction stepping plans.
//! - [`breakpoints`] and [`watchpoints`]: software traps and debug registers.
//! - [`frames`], [`inspection`], [`memory`], [`registers`]: read-only views of
//!   a stopped snapshot.
//! - [`modules`]: the registry of mapped shared objects.
//! - [`post_mortem`]: serving the same views from a core dump.

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fmt;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread::{self, JoinHandle};

use nix::errno::Errno;
use nix::libc;
use nix::unistd::Pid;
use signals::SignalPolicies;
pub use signals::{Signal, WaitEvent};
use tokio::sync::mpsc;

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
use classify::{is_stopping_signal, is_superseded};
use debug_registers::DebugRegisterPlan;
use memory::MemoryAccessError;
use modules::{ModuleMapping, loader_link_maps, mapped_module_load_bias, module_mappings};
use native::{InspectionOps, LinuxPtrace, LinuxTraceOps, is_vanished_tracee};
use registers::Fxsave;
pub(super) use tls::CLibrary;
use tls::TlsModule;

mod activation;
mod async_frames;
pub use async_frames::resume_address;
mod awaits;
mod breakpoints;
mod callers;
mod catchpoints;
mod classify;
mod core_dump;
mod core_files;
mod debug_registers;
mod disassembly;
mod evaluation;
mod frames;
mod indirect;
mod inspection;
mod internal_stops;
mod invariants;
mod language_exceptions;
mod libraries;
mod lifecycle;
mod loops;
mod memory;
mod modules;
mod native;
#[cfg(test)]
pub mod native_tracee;
mod new_task;
mod panic_release;
pub mod perf;
mod post_mortem;
mod presentation;
#[cfg(debug_assertions)]
mod recorded;
mod registers;
mod returns;
mod run_control;
mod runtimes;
mod signals;
#[cfg(any(test, feature = "sim"))]
pub mod sim_edge;
mod stack_watches;
mod step_targets;
mod stepping;
mod sys;
pub mod syscalls;
mod tls;
mod vdso;
mod visualizers;
mod watchpoints;
mod writes;

pub use post_mortem::{PostMortemSession, open_core};

/// Ends the job-control stop of a held process, and returns whether it
/// did: a process no longer held is left alone.
pub fn release_held(held: &crate::HeldProcess) -> Result<bool> {
    native::continue_held(lifecycle::requested_pid(held.process_id)?, held.start_time)
}

/// Whether a held process is still held.
pub fn still_held(held: &crate::HeldProcess) -> Result<bool> {
    native::process_held(lifecycle::requested_pid(held.process_id)?, held.start_time)
}

/// The processes named `name`, as `pgrep -x` matches them: by the
/// command name the kernel records, or by the file name of the first
/// argument, which outlasts the kernel's fifteen-character limit. This
/// process is never one of them.
pub fn processes_named(name: &str) -> Result<Vec<crate::ProcessId>> {
    let own = std::process::id();
    let mut found = Vec::new();
    let entries = std::fs::read_dir("/proc")?;
    for entry in entries.filter_map(std::result::Result::ok) {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|pid| pid.parse::<u32>().ok())
        else {
            continue;
        };

        if pid == own {
            continue;
        }

        // A process that ended meanwhile matches nothing.
        let command = std::fs::read_to_string(entry.path().join("comm")).unwrap_or_default();
        let first_argument = std::fs::read(entry.path().join("cmdline"))
            .ok()
            .and_then(|line| {
                let first = line.split(|byte| *byte == 0).next()?.to_vec();
                let first = String::from_utf8(first).ok()?;
                Some(
                    std::path::Path::new(&first)
                        .file_name()?
                        .to_string_lossy()
                        .into_owned(),
                )
            });
        if command.trim_end_matches('\n') == name || first_argument.as_deref() == Some(name) {
            found.push(crate::ProcessId::new(u64::from(pid)));
        }
    }

    found.sort_unstable_by_key(|pid| pid.get());
    Ok(found)
}

#[cfg(feature = "fuzzing")]
pub fn fuzz_core_dump(data: &[u8]) {
    core_dump::fuzz(data);
}

#[cfg(feature = "fuzzing")]
pub fn fuzz_debug_register_plan(data: &[u8]) {
    debug_registers::fuzz(data);
}

#[cfg(feature = "fuzzing")]
pub fn fuzz_glibc_tls(data: &[u8]) {
    tls::fuzz_glibc(data);
}

/// Each thread's id and thread pointer in process `pid`, which may be
/// running, from its C library's own lists of its threads.
pub fn running_thread_pointers(pid: u32) -> std::result::Result<Vec<(u32, u64)>, String> {
    let pid = i32::try_from(pid).map_err(|_| format!("process {pid} has no Linux id"))?;
    Ok(tls::running_thread_pointers(Pid::from_raw(pid))?
        .into_iter()
        .filter_map(|(tid, pointer)| Some((u32::try_from(tid).ok()?, pointer)))
        .collect())
}

/// The debug register slots, which the `u32` capability reports.
const WATCH_SLOTS: u32 = 4;
const _: () = assert!(WATCH_SLOTS as usize == debug_registers::SLOT_COUNT);

/// x86-64 debug registers report writes or any access, but never reads alone.
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
const BREAKPOINT_OPCODE: u8 = 0xcc;
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

/// Converts a client-supplied thread identifier to a Linux TID.
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

    /// A lease for sessions that never trace, such as post-mortem core dumps.
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
    installed: bool,
    owners: BTreeSet<BreakpointOwner>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum BreakpointOwner {
    User(BreakpointId),
    Plan(ExecutionId),

    /// The dynamic loader's report of each change to the loaded libraries.
    Loader,

    /// A language runtime's report of an exception.
    Runtime,

    /// The entry or a return of a language runtime's code that moves a
    /// watched task's stack.
    StackMove,

    /// The entry of an indirect function's resolver, or the return of a
    /// call of one, that a breakpoint on the function waits for.
    Resolver,
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
    BreakpointRepair {
        address: VirtualAddress,
    },
    AwaitBreakpoint {
        address: VirtualAddress,
    },
    UserStep {
        kind: StepKind,
    },
}

struct TraceThread {
    state: NativeThreadState,
    expected: ExpectedStop,
    pending_signal: Option<PendingSignal>,

    /// A signal its runtime tolerates arriving late, held while the thread
    /// stepped or ran without its siblings, until it continues with them.
    held_signal: Option<PendingSignal>,
    reason: Option<StopReason>,
    stopped_at_breakpoint: Option<VirtualAddress>,

    /// The site whose trap the thread reported, until it steps over the
    /// site or runs on otherwise. It outlives the site's removal, so that a
    /// breakpoint added there before the thread moves is stepped over too.
    trapped_at: Option<VirtualAddress>,
    awaiting_breakpoint: Option<VirtualAddress>,
    debugger_stop_pending: bool,

    /// The watch-plan generation programmed into this thread's debug
    /// registers. `None` means the debugger never programmed them.
    armed: Option<u64>,

    /// The watchpoints whose hits this thread stops for since its last
    /// public stop, with each hit's number.
    watch_hits: BTreeMap<WatchpointId, u64>,

    /// The system call the thread stopped at the entry of, with what was
    /// read there, while its exit stop is still to come.
    syscall: Option<catchpoints::Entered>,

    /// Whether the thread's step makes a system call, which stops at its
    /// entry and exit for the catchpoints.
    stepping_call: bool,

    /// Whether the thread is at an event stop inside a system call, which
    /// returns when it next runs.
    inside_call: bool,

    /// The thread's name as of its start or the last published stop.
    name: Option<Arc<str>>,
}

impl TraceThread {
    const fn starting(expected: ExpectedStop) -> Self {
        Self {
            state: NativeThreadState::Starting,
            expected,
            pending_signal: None,
            held_signal: None,
            reason: None,
            stopped_at_breakpoint: None,
            trapped_at: None,
            awaiting_breakpoint: None,
            debugger_stop_pending: false,
            armed: None,
            watch_hits: BTreeMap::new(),
            syscall: None,
            stepping_call: false,
            inside_call: false,
            name: None,
        }
    }
}

/// The process-wide watchpoints and the debug-register plan every thread
/// carries.
#[derive(Default)]
struct WatchState {
    plan: DebugRegisterPlan,
    generation: u64,
    watchpoints: BTreeMap<WatchpointId, WatchRecord>,
}

struct WatchRecord {
    watchpoint: Watchpoint,
    frame: Option<FrameScopeEvidence>,

    /// The watched bytes when the debugger last observed them.
    observed: Option<Arc<[u8]>>,

    /// For a watch on a task's stack, where on the stack it is.
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

    /// The faulting address of a synchronous fault.
    fault_address: Option<u64>,
}

// Addresses print in hex, as `VirtualAddress` does.
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

    /// The thread executed a trap whose site was removed before its report
    /// was handled; it has been rewound to the restored instruction.
    RemovedTrap,

    /// The thread executed a trap carried by code that moved since the
    /// modules were refreshed; it has been rewound to the trap, which the
    /// refresh takes out.
    CarriedTrap,

    /// The thread executed a trap instruction of the program's own, at
    /// this address, which no breakpoint of the debugger's owns.
    ProgramTrap(VirtualAddress),

    /// SIGKILL took the thread out of the reported stop; its exit follows.
    Superseded,
    Unclassifiable(RawStopRecord),
}

#[derive(Debug, Clone, Default)]
struct StepStart {
    source: Option<SourceLocation>,
    code_instance: Option<CodeInstanceId>,
    physical_instance: Option<CodeInstanceId>,
    activation: Option<Activation>,

    /// The stack pointer where the step began. Where no activation is
    /// known, a frame below it was entered by a call, and code above it was
    /// returned to.
    stack_pointer: Option<StackPosition>,

    /// For a step over or out, the activation its frame returned to, once
    /// the frame it began in returned short of where the step ends, and
    /// the one that returned to in turn. The step then goes on by single
    /// steps and judges frames by this: a later call can make a new
    /// activation at the returned one's CFA.
    returned_to: Option<Activation>,

    /// Whether the step returned into code without source and runs on, to
    /// be ended only by a stop the user sees.
    running_on: bool,
    plan_addresses: BTreeSet<VirtualAddress>,

    /// For an advance, the addresses that end it when its thread reaches
    /// one. They are plan sites too, but no step logic retires them.
    targets: BTreeSet<VirtualAddress>,

    /// The target an advance's thread stood at without having trapped
    /// there. It arrives there first, which the user's breakpoints count
    /// and which does not end the advance.
    standing: Option<VirtualAddress>,
    epilogue_traversal: Option<EpilogueTraversal>,
    return_traversal: Option<ReturnTraversal>,

    /// Where a signal handler returns to the instruction it interrupted,
    /// or a call the step passes returns.
    resume_guard: Option<ResumeGuard>,

    /// For a step into one call of a line, that call's instruction; the
    /// step runs the line's other calls to their returns.
    into_call: Option<VirtualAddress>,

    /// For a step over a call instruction, the return address and the stack
    /// pointer the call returns with.
    call_return: Option<(VirtualAddress, StackPosition)>,

    /// Whether the step may stop in a language runtime's own code: it began
    /// there, or the session's steps enter the runtime.
    enters_runtime: bool,

    /// Where a step over or out traps a panic its task begins: the entries
    /// of the runtime's code that starts one.
    panic_guards: BTreeSet<VirtualAddress>,

    /// Whether a step over or out follows the runtime's calls into the
    /// program, as a step in does, since its task began a panic or its
    /// frame returned into code that calls deferred functions.
    following: bool,

    /// The return address of code the step leaves, where a plan breakpoint
    /// waits while the stepping thread runs freely, until the step's thread
    /// or task reaches one of its plan's breakpoints. Meanwhile the thread
    /// may run anything, such as another of its runtime's tasks.
    escape: Option<VirtualAddress>,

    /// The loops whose bodies, functions of their own, a step over or out
    /// treats as its own code.
    loops: Option<loops::StepLoops>,

    /// For a step out of a function's own frame, the function, whose
    /// returned values its stop shows.
    returning: Option<returns::Returning>,

    /// What that function returned, read the instant its call returned,
    /// which the step's stop shows though the step goes on past code no
    /// line describes.
    returned: Option<returns::Returned>,

    /// For a step into a new task, how far it has followed the task's
    /// start.
    new_task: Option<new_task::NewTask>,

    /// For a step over or out of an async function's body, the future it
    /// runs for, which the step follows across the polls of its awaits.
    awaiting: Option<awaits::AwaitStep>,

    /// The future the step waited for, once it was dropped and the step
    /// went on in the code that dropped it.
    dropped: Option<awaits::RunningFuture>,

    /// Where the stepping thread stands before a trap, resumed to run it,
    /// whose hit judges the step's arrival there.
    judged_by_trap: Option<VirtualAddress>,
}

impl StepStart {
    /// Where the step's plan stops threads: its plan's own sites, its panic
    /// guards, an advance's targets, and the code that starts tasks while a
    /// step into a new task watches for one.
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
}

/// Whether a step kind executes machine instructions rather than source
/// lines.
const fn steps_instructions(kind: StepKind) -> bool {
    matches!(kind, StepKind::Instruction | StepKind::OverInstruction)
}

/// Where a step resumes once its thread returns there with this stack
/// pointer: the instruction a delivered signal interrupted, where its
/// handler returns, or the return address of a call a step into another
/// call runs to its return.
#[derive(Debug, Clone, Copy)]
struct ResumeGuard {
    address: VirtualAddress,
    stack: StackPosition,

    /// Whether a call returns there, so the step may end where it does.
    call: bool,
}

#[derive(Debug, Clone)]
struct ReturnTraversal {
    /// A frame's return address, proven by agreeing CFI and ABI stack-slot
    /// sources.
    return_address: VirtualAddress,

    /// The activation whose return reaches `return_address`. For a tail call
    /// this is the step's starting activation; for a regular call it is the
    /// nested callee activation.
    guarded_activation: Activation,
    retire_return_after_repair: bool,
}

#[derive(Debug, Clone)]
struct EpilogueTraversal {
    /// The caller instruction is always a guard destination. It is only a
    /// source-step destination when it is also present in `completion_addresses`.
    return_address: VirtualAddress,
    completion_addresses: BTreeSet<VirtualAddress>,
    retire_return_after_repair: bool,
}

/// How a stopped thread resumes.
#[derive(Debug, Clone, Copy)]
enum Resume {
    Continue,
    Step,
}

/// Whom a step belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct StepOwner {
    /// The thread the step runs on: the one it began on, until its task
    /// runs on another.
    thread: Pid,

    /// The task the step began in, which it follows from thread to thread.
    task: Option<crate::TaskId>,
}

#[derive(Debug, Clone)]
enum ActiveKind {
    Launch,
    Continue,
    Step {
        owner: StepOwner,
        kind: StepKind,

        /// The kind the client asked for, which the step's stop reports:
        /// a step into a new task runs as a step over until a task starts.
        requested: StepKind,
        start: Box<StepStart>,

        /// The stepping thread executed an instruction whose effect on the
        /// step has not been evaluated yet, because a breakpoint repair or
        /// an internal stop intervened.
        progress_owed: bool,
    },
}

struct ActiveExecution {
    id: ExecutionId,
    kind: ActiveKind,
    scope: ResumeScope,
    resume_threads: BTreeSet<Pid>,
}

struct RepairGroup {
    address: VirtualAddress,
    remaining: VecDeque<Pid>,
    current: Option<Pid>,
    site_removed: bool,
}

/// Stops every thread of the inferior, then either publishes a stop or,
/// when no thread produced a visible reason, applies its edits and resumes
/// the active execution as if nothing happened.
struct StopBarrier {
    triggering_thread: Pid,

    /// The stop to publish, or `None` while the stop is internal.
    reason: Option<StopReason>,

    /// Whether a client paused, so the stop is published as a pause when
    /// every reason it would have published is dropped.
    paused: bool,

    /// Why the active execution ended during the stop, such as its
    /// stepping thread exiting. The execution cannot resume, so the stop
    /// publishes this when every other reason is dropped.
    ended: Option<StopReason>,

    /// Client edits applied once every thread is stopped.
    edits: Vec<Edit>,
}

impl StopBarrier {
    /// A barrier that publishes `reason` from `triggering_thread`.
    fn visible(triggering_thread: Pid, reason: StopReason) -> Self {
        Self {
            triggering_thread,
            paused: reason == StopReason::Pause,
            reason: Some(reason),
            ended: None,
            edits: Vec::new(),
        }
    }
}

/// A breakpoint or watchpoint change that needs every thread stopped.
enum Edit {
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
        calls: Arc<[crate::syscalls::SyscallId]>,
        phase: crate::CatchPhase,
        condition: Option<crate::Condition>,
        reply: Reply<crate::Catchpoint>,
    },
    RemoveCatchpoint {
        id: crate::CatchpointId,
        reply: Reply<crate::Catchpoint>,
    },

    /// Bring modules and breakpoints up to date after the loader changed
    /// the loaded libraries.
    RefreshModules,

    /// Set aside the watches of tasks whose stacks a runtime is moving, and
    /// place those whose moves finished on their new stacks.
    FollowStacks,
}

struct PublicStop {
    id: StopId,
    triggering_thread: Pid,
    reason: StopReason,
    presentations: BTreeMap<Pid, FramePresentation>,

    /// The thread or task that implicit inspection follows: the triggering
    /// thread until a client selects another.
    selected: ExecutionContext,

    /// The thread the selected context runs on: none for a parked task, or
    /// once the thread exits.
    selected_thread: Option<Pid>,

    /// Frames selected by clients; an absent context has its innermost
    /// frame selected.
    selected_frames: BTreeMap<ExecutionContext, StackFrameId>,

    /// What each thread runs for a language runtime, read once asked for.
    activities: RefCell<BTreeMap<Pid, Option<crate::ThreadActivity>>>,

    /// Where each runtime said it keeps the tasks it listed at this stop,
    /// forgotten once the debugger writes memory.
    task_locators: RefCell<BTreeMap<crate::TaskId, u64>>,

    /// The sets of tasks the futures threads drive run, found once asked
    /// for, and why some may be missing; forgotten once the debugger
    /// writes memory.
    task_sets: RefCell<Option<runtimes::TaskSets>>,

    /// What the function a step out finished returned.
    returned: Option<returns::Returned>,
}

/// Stop identifiers are unique across every session in the process: a
/// stopped-state reference can outlive its controller, and must never
/// authenticate against a later session's stop.
static NEXT_STOP_ID: AtomicU64 = AtomicU64::new(1);

fn allocate_stop_id() -> Result<StopId> {
    // On exhaustion the counter stays put rather than wrap and reissue ids.
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

/// A vforked child running in its parent's memory, from the parent's vfork
/// event to its `VFORK_DONE`.
#[derive(Debug, Clone, Copy)]
struct VforkWindow {
    /// The child that shares the memory until it execs or exits.
    child: Pid,

    /// Whether the vforking thread has been resumed to let the child run.
    resumed: bool,
}

struct Inferior {
    origin: InferiorOrigin,
    tgid: Pid,
    loaded_module: LoadedModule,
    breakpoints: BTreeMap<VirtualAddress, BreakpointSite>,

    /// Every site each execution's plan installed, so that ending the plan
    /// visits only those. A site the plan already released may remain.
    plan_sites: BTreeMap<ExecutionId, BTreeSet<VirtualAddress>>,
    threads: BTreeMap<Pid, TraceThread>,

    /// Threads that left the inferior and whose exit status is still due.
    retired_threads: BTreeSet<Pid>,

    /// Initial stops of new threads or fork children that arrived before
    /// the event that announces them.
    unowned_stops: BTreeMap<Pid, WaitEvent>,

    /// Threads that began exiting before the event that announces them.
    vanished_threads: BTreeSet<Pid>,

    /// Sites removed since the address space began, with their original
    /// bytes: a process forked before their removal still holds their traps.
    former_sites: BTreeMap<VirtualAddress, u8>,

    /// Listed threads that exited before an attach could seize them, such
    /// as a leader that exited before the rest of its process. No status
    /// of theirs is due.
    unseized_threads: BTreeSet<Pid>,

    /// Fork children announced before their initial stop arrived, with the
    /// breakpoint sites each inherited.
    fork_children: BTreeMap<Pid, Vec<(VirtualAddress, u8)>>,

    /// Fork children killed, or found dying, as they were released, whose
    /// exits are still due: another process's exit may follow the leader's.
    killed_children: BTreeSet<Pid>,

    /// Threads in a vfork that shares this memory, by the thread that
    /// vforked. The traps are lifted while any window is open.
    vfork_windows: BTreeMap<Pid, VforkWindow>,

    /// Whether the traps are lifted from the memory every open vfork window
    /// shares, and so must be reinstalled when the last window closes.
    traps_lifted: bool,
    waiter: Option<Waiter>,
    active: Option<ActiveExecution>,
    repairs: VecDeque<RepairGroup>,
    barrier: Option<StopBarrier>,
    public_stop: Option<PublicStop>,
    next_execution: u64,
    exec_unsupported: bool,

    /// The loader's breakpoint, once the loader is known.
    loader_site: Option<VirtualAddress>,

    /// The runtime functions whose entry stops for an exception.
    runtime_hooks: BTreeMap<VirtualAddress, language_exceptions::HookSite>,
    stack_moves: stack_watches::StackMoves,
    indirect: indirect::IndirectFunctions,
    watch: WatchState,

    /// The signal the debugger sent to end the inferior, which never stops
    /// it whatever its policy.
    terminating: Option<Terminating>,

    /// How far an attach to a process another session held has ended the
    /// job-control stop it was held in.
    held: Option<HeldStop>,
}

/// An attach to a held process ends the job-control stop it was held in
/// with `SIGCONT`, whose delivery it then suppresses: the program never
/// sees the signal the debuggers exchanged.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum HeldStop {
    /// Still stopped, until the attach completes.
    Stopped,

    /// `SIGCONT` sent, its delivery not yet seen.
    Continued,

    /// Detaching, with `SIGCONT` unblocked in `thread`, which blocked it
    /// with `mask`, so that the thread takes it first. A fork child stops
    /// first inside `fork`, which blocks nearly every signal.
    Draining { thread: Pid, mask: u64 },
}

/// The debugger asked the inferior to end with `signal`. Programs often
/// handle it by cleaning up and raising it again with its default action,
/// as Go's runtime does, so every delivery of it passes silently until the
/// user sees a stop after the first one.
#[derive(Clone, Copy, Debug)]
struct Terminating {
    signal: Signal,
    delivered: bool,
}

impl Inferior {
    /// Creates an inferior with no execution, stop, breakpoints, or watchpoints.
    fn new(
        origin: InferiorOrigin,
        tgid: Pid,
        loaded_module: LoadedModule,
        threads: BTreeMap<Pid, TraceThread>,
        waiter: Option<Waiter>,
    ) -> Self {
        Self {
            origin,
            tgid,
            loaded_module,
            breakpoints: BTreeMap::new(),
            plan_sites: BTreeMap::new(),
            threads,
            retired_threads: BTreeSet::new(),
            unowned_stops: BTreeMap::new(),
            vanished_threads: BTreeSet::new(),
            unseized_threads: BTreeSet::new(),
            former_sites: BTreeMap::new(),
            fork_children: BTreeMap::new(),
            killed_children: BTreeSet::new(),
            vfork_windows: BTreeMap::new(),
            traps_lifted: false,
            waiter,
            active: None,
            repairs: VecDeque::new(),
            barrier: None,
            public_stop: None,
            next_execution: 0,
            exec_unsupported: false,
            loader_site: None,
            runtime_hooks: BTreeMap::new(),
            stack_moves: stack_watches::StackMoves::default(),
            indirect: indirect::IndirectFunctions::default(),
            watch: WatchState::default(),
            terminating: None,
            held: None,
        }
    }

    /// Whether `pid` is a group leader that exited while other threads run
    /// on. Linux reports its exit only after every other thread's, so no
    /// stop waits for it, and none lists it.
    fn exited_leader(&self, pid: Pid, thread: &TraceThread) -> bool {
        pid == self.tgid
            && matches!(thread.state, NativeThreadState::Exiting)
            && self.threads.len() > 1
    }

    /// Whether a thread is where a coherent stop needs it: stopped, or an
    /// exited leader.
    fn settled(&self, pid: Pid, thread: &TraceThread) -> bool {
        matches!(thread.state, NativeThreadState::Stopped) || self.exited_leader(pid, thread)
    }

    /// Whether a thread that resumes expecting `expected` holds the signals
    /// its runtime tolerates arriving late: while it steps over a
    /// breakpoint or returns to one, while it steps, and while it runs
    /// without the threads the debugger keeps stopped. A handler run then
    /// could wait for those threads, as Go's preemption does.
    fn holds_signals(&self, expected: &ExpectedStop) -> bool {
        matches!(
            expected,
            ExpectedStop::BreakpointRepair { .. }
                | ExpectedStop::AwaitBreakpoint { .. }
                | ExpectedStop::UserStep { .. }
        ) || self.active.as_ref().is_some_and(|active| {
            matches!(active.kind, ActiveKind::Step { .. })
                || matches!(active.scope, ResumeScope::Thread(_))
        })
    }

    /// A stopped thread through which to read and write the shared address
    /// space: the leader when it is stopped, otherwise any stopped thread.
    /// While some threads run, only a stopped thread accepts ptrace requests.
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

    /// Removes every reference to a thread that exited while others live on.
    /// Returns whether the thread's breakpoint repair step was interrupted,
    /// in which case execution must advance to the next repair.
    fn forget_thread(&mut self, pid: Pid, status: &ExitStatus) -> bool {
        if let Some(active) = self.active.as_mut() {
            active.resume_threads.remove(&pid);
        }

        let mut interrupted = false;
        for group in &mut self.repairs {
            group.remaining.retain(|&waiting| waiting != pid);
            if group.current == Some(pid) {
                group.current = None;
                interrupted = true;
            }
        }

        if let Some(stop) = self.public_stop.as_mut() {
            stop.presentations.remove(&pid);
            if stop.selected_thread == Some(pid) {
                stop.selected_thread = None;
            }
        }

        // A barrier is presented from its triggering thread, which must live.
        // An internal stop still has nothing to present. A leader that
        // exited alone stays listed until its process ends.
        let others = || self.threads.iter().filter(|&(&other, _)| other != pid);
        let replacement = others()
            .find(|(_, thread)| matches!(thread.state, NativeThreadState::Stopped))
            .or_else(|| others().next())
            .map(|(&pid, _)| pid);
        if let Some(barrier) = self
            .barrier
            .as_mut()
            .filter(|barrier| barrier.triggering_thread == pid)
            && let Some(replacement) = replacement
        {
            barrier.triggering_thread = replacement;

            // An internal stop stays internal.
            if barrier.reason.is_some() {
                barrier.reason = Some(StopReason::ThreadExited {
                    thread_id: debug_thread_id(pid),
                    status: status.clone(),
                });
            }
        }

        interrupted
    }

    /// The edits an internal stop holds until every thread stopped.
    fn take_pending_edits(&mut self) -> Vec<Edit> {
        self.barrier
            .as_mut()
            .map(|barrier| std::mem::take(&mut barrier.edits))
            .unwrap_or_default()
    }

    /// Every trap a process forked from this one may hold, with its
    /// original byte: the sites installed now, one lifted for a repair
    /// among them, and those removed since the address space began, as a
    /// fork's event may be handled after edits that followed the fork. A
    /// child is scrubbed only where its memory still holds a trap.
    fn inherited_sites(&self) -> Vec<(VirtualAddress, u8)> {
        let mut sites = self.former_sites.clone();
        sites.extend(
            self.breakpoints
                .iter()
                .map(|(&address, site)| (address, site.original_byte)),
        );
        sites.into_iter().collect()
    }

    /// Ends `pid`'s step over the front repair group's breakpoint at `address`.
    pub(super) fn finish_current_repair(
        &mut self,
        pid: Pid,
        address: VirtualAddress,
    ) -> Result<()> {
        let thread = self.thread_mut(pid)?;
        thread.stopped_at_breakpoint = None;
        thread.trapped_at = None;
        let group = self
            .repairs
            .front_mut()
            .filter(|group| group.address == address && group.current == Some(pid))
            .ok_or_else(|| {
                backend_error(LinuxError::UnexpectedWait(format!(
                    "breakpoint repair by {pid} at {address} does not match the active repair"
                )))
            })?;
        group.current = None;
        Ok(())
    }
}

/// Collects an inferior's wait statuses for the controller: a thread in a
/// live session, or nothing when whoever drives the controller reports
/// statuses itself, as test fakes and the simulator do.
struct Waiter {
    thread: Option<WaiterThread>,
}

struct WaiterThread {
    stop: Arc<AtomicBool>,
    handle: JoinHandle<()>,
}

impl Waiter {
    /// A waiter whose statuses the controller's driver delivers.
    #[cfg(any(test, feature = "sim"))]
    const fn external() -> Self {
        Self { thread: None }
    }

    /// Stops the waiter, calling `serve` until it ends: one blocked
    /// sending a status to a full queue waits for the controller to make
    /// room, which `serve` does.
    fn stop_and_join(self, mut serve: impl FnMut()) -> Result<()> {
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

    /// Seized while waiting to exec the executable, and owned as a launch.
    LaunchedByExec,
    PostMortem,
}

impl InferiorOrigin {
    /// Whether the process dies with the debugger.
    const fn owned(self) -> bool {
        matches!(self, Self::Launched | Self::LaunchedByExec)
    }

    /// Whether its threads were seized, so they stop by `PTRACE_INTERRUPT`
    /// and report group-stops as events.
    const fn seized(self) -> bool {
        matches!(self, Self::Attached | Self::LaunchedByExec)
    }
}

struct RuntimeModule {
    loaded: LoadedModule,
    image: Arc<ModuleImage>,
    unwind: Arc<dyn UnwindInfo>,
    variables: Arc<dyn VariableInfo>,

    /// How the C library identifies the module's TLS block, once known.
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

    #[error("the inferior replaced its executable image; loading the new image is not supported")]
    UnsupportedExec,

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
}

struct Controller<P: InspectionOps> {
    _lease: SessionLease,
    executable: Arc<PathBuf>,
    executable_identity: FileIdentity,

    /// The C library the executable runs on, whose structures locate TLS.
    c_library: CLibrary,
    expected_process_start_time: Option<u64>,
    module_image: Arc<ModuleImage>,
    unwind_info: Arc<dyn UnwindInfo>,
    modules: BTreeMap<crate::ModuleId, RuntimeModule>,

    /// Where loaded modules' separate debug files are found.
    debug_files: crate::debug_info::DebugFileSearch,

    /// The canonical path and load bias computed for each mapping, so known
    /// modules are not re-read from disk at every stop.
    mapped_modules: BTreeMap<ModuleMapping, (PathBuf, crate::LoadBias)>,
    next_module_id: u32,
    next_image_id: u32,
    messages: RefCell<mpsc::Receiver<ControllerMessage>>,

    /// Messages taken from the queue and not yet served, in arrival order.
    pending: RefCell<VecDeque<ControllerMessage>>,

    /// The views values are presented with.
    views: presentation::Views,
    message_sender: mpsc::Sender<ControllerMessage>,
    events: EventSender,
    ptrace: P,
    inferior: Option<Inferior>,
    breakpoints: breakpoints::UserBreakpoints,
    next_breakpoint_id: u64,
    next_watchpoint_id: u64,

    /// The system-call catchpoints, which outlive any process.
    catchpoints: BTreeMap<crate::CatchpointId, crate::Catchpoint>,
    next_catchpoint_id: u64,
    launch_reply: Option<Reply<ExecutionId>>,
    attach_reply: Option<Reply<StopId>>,

    /// How many times the attach in progress found threads it had not
    /// traced.
    attach_rescans: u32,

    /// Whether the session is ending: wait events then only advance the kill
    /// or detach, and the controller exits once the inferior is gone.
    shutting_down: bool,
    shutdown_reply: Option<Reply<()>>,

    /// The client waiting for a killed inferior to be gone.
    kill_reply: Option<Reply<()>>,

    /// Fork children still traced after the inferior ended.
    orphans: Option<Orphans>,

    /// Where the processes the inferior forks are held, while a receiver
    /// takes them; otherwise they are released.
    held_children: Option<tokio::sync::mpsc::UnboundedSender<crate::HeldChild>>,

    /// A launch or attach waiting for those children to be released.
    deferred_start: Option<Start>,
    signals: SignalPolicies,

    /// Which exceptions that runtimes report stop the program.
    exception_stops: crate::ExceptionStops,

    /// Whether steps stop in a language runtime's own code.
    step_into_runtime: bool,

    /// The language runtime each image carries, bound on first need.
    runtime_models: runtimes::RuntimeCache,
    revision: u64,

    /// How many messages the controller served, which paces the invariant
    /// checks of a large state.
    messages_served: u64,

    /// The description a served request begins with that makes the
    /// controller panic, from [`PANIC_AFTER`], to test what a panic leaves.
    #[cfg(debug_assertions)]
    panic_after: Option<String>,
}

/// Fork children still on their way to their first stop when their parent's
/// process ended, with the waiter that reports them. Each is scrubbed of the
/// traps it inherited and released at that stop; the waiter, which would
/// otherwise poll them forever, is joined once none remains.
struct Orphans {
    /// The process that forked them.
    parent: Pid,

    /// Each child with the traps it may hold.
    children: BTreeMap<Pid, Vec<(VirtualAddress, u8)>>,

    /// Children that could not be scrubbed, killed and awaiting their exit.
    killed: BTreeSet<Pid>,
    waiter: Option<Waiter>,
}

/// A requested launch or attach.
enum Start {
    Launch(crate::LaunchOptions, Reply<ExecutionId>),
    Attach {
        requested: ProcessId,

        /// Whether the process is one a session held.
        held: bool,
        reply: Reply<StopId>,
    },
    LaunchByExec {
        requested: ProcessId,
        stop_at_entry: bool,
        release: Box<dyn FnOnce() + Send>,
        reply: Reply<ExecutionId>,
    },
}

/// In development builds, the variable that names a request after which
/// the controller panics, by the start of its description.
#[cfg(debug_assertions)]
const PANIC_AFTER: &str = "MICROSCOPE_TEST_PANIC_AFTER";

pub fn spawn_controller(
    executable: Executable,
    debug_info: DebugInfo,
    channels: ControllerChannels,
) -> Result<JoinHandle<()>> {
    let lease = SessionLease::acquire()?;

    Ok(thread::Builder::new()
        .name(CONTROLLER_THREAD_NAME.into())
        .spawn(move || {
            record!("started for {}", executable.display_path.display());
            #[cfg(debug_assertions)]
            let ptrace = recorded::Recorded(LinuxPtrace::new());
            #[cfg(not(debug_assertions))]
            let ptrace = LinuxPtrace::new();
            Controller::new(lease, executable, debug_info, channels, ptrace).run();
            record!("exited");
        })?)
}

impl<P: InspectionOps> Controller<P> {
    fn new(
        lease: SessionLease,
        executable: Executable,
        debug_info: DebugInfo,
        channels: ControllerChannels,
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
            variables: variable_info,
            tls: None,
        };

        Self {
            _lease: lease,
            executable: executable.display_path,
            c_library: executable.c_library,
            executable_identity: executable.identity,
            expected_process_start_time: executable.process_start_time,
            debug_files: executable.debug_files,
            module_image,
            unwind_info,
            modules: BTreeMap::from([(main.loaded.id, main)]),
            mapped_modules: BTreeMap::new(),
            next_module_id: 1,
            next_image_id: 1,
            messages: RefCell::new(channels.receiver),
            pending: RefCell::new(VecDeque::new()),
            views: presentation::Views::default(),
            message_sender: channels.sender,
            events: channels.events,
            ptrace,
            inferior: None,
            breakpoints: breakpoints::UserBreakpoints::default(),
            next_breakpoint_id: 1,
            catchpoints: BTreeMap::new(),
            next_catchpoint_id: 1,
            next_watchpoint_id: 1,
            launch_reply: None,
            attach_reply: None,
            attach_rescans: 0,
            shutting_down: false,
            shutdown_reply: None,
            kill_reply: None,
            orphans: None,
            held_children: None,
            deferred_start: None,
            signals: SignalPolicies::default(),
            exception_stops: crate::ExceptionStops::default(),
            step_into_runtime: false,
            runtime_models: RefCell::default(),
            revision: 0,
            messages_served: 0,

            #[cfg(debug_assertions)]
            panic_after: std::env::var(PANIC_AFTER).ok(),
        }
    }
}

impl<P: InspectionOps> Controller<P> {
    /// Takes every message already queued, keeping them in arrival order.
    fn drain_messages(&self) {
        let mut messages = self.messages.borrow_mut();
        let mut pending = self.pending.borrow_mut();
        while let Ok(message) = messages.try_recv() {
            pending.push_back(message);
        }
    }

    /// The next message to serve: in arrival order, except that inspection
    /// of a stop waits behind run control queued after it, which it would
    /// only delay. Waits for one when `block` holds and none is queued.
    fn next_message(&self, block: bool) -> Option<ControllerMessage> {
        self.drain_messages();
        let mut pending = self.pending.borrow_mut();
        if let Some(preempting) = pending
            .iter()
            .position(ControllerMessage::preempts_inspection)
        {
            let index = pending
                .iter()
                .take(preempting + 1)
                .position(|message| !message.reads_one_stop())
                // The preempting message is itself one that does not wait.
                .unwrap_or(preempting);
            return pending.remove(index);
        }

        if let Some(message) = pending.pop_front() {
            return Some(message);
        }

        drop(pending);
        if block {
            self.messages.borrow_mut().blocking_recv()
        } else {
            None
        }
    }

    /// Whether run control waits behind the inspection being served, which
    /// then stops and is served again after it.
    fn run_control_waiting(&self) -> bool {
        self.drain_messages();
        self.pending
            .borrow()
            .iter()
            .any(ControllerMessage::preempts_inspection)
    }

    /// Stops and joins a waiter, keeping what it sends meanwhile to serve.
    fn join_waiter(&self, waiter: Waiter) -> Result<()> {
        waiter.stop_and_join(|| self.drain_messages())
    }

    /// Serves a request again after the run control that interrupted it.
    fn serve_later(&self, request: Request) {
        record!("interrupted {}", request.describe());
        self.pending
            .borrow_mut()
            .push_front(ControllerMessage::Request(request));
    }
}

impl<P: LinuxTraceOps> Controller<P> {
    fn run(mut self) {
        // Any panic ends the controller, an assertion's or another's; an
        // attached process is released before it does.
        let served = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            while let Some(message) = self.next_message(true) {
                if !self.handle_message(message) {
                    return;
                }
            }

            // The controller holds a request sender for its waiter, so the
            // queue never closes; dropping the `Debugger` sends a shutdown
            // request.
        }));
        if let Err(panic) = served {
            self.release_after_panic();
            std::panic::resume_unwind(panic);
        }
    }

    /// Serves one queued request or wait status, and returns whether the
    /// controller keeps running. A driver that owns the queue, such as the
    /// simulator, delivers messages one at a time through this.
    fn handle_message(&mut self, message: ControllerMessage) -> bool {
        let keeps_running = match message {
            ControllerMessage::Request(request) => {
                record!("request {}", request.describe());
                let _span = crate::span!("request", "{}", request.describe());
                #[cfg(debug_assertions)]
                let injected = self
                    .panic_after
                    .as_ref()
                    .is_some_and(|prefix| request.describe().starts_with(prefix.as_str()));
                let keeps_running = self.handle_request(request);
                #[cfg(debug_assertions)]
                assert!(!injected, "a panic injected by {PANIC_AFTER}");
                keeps_running
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

        // A shutdown, whether requested or begun when an attached process
        // failed, ends the controller once nothing is left to release.
        keeps_running && !(self.shutting_down && self.inferior.is_none() && self.orphans.is_none())
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
                self.edit(Edit::SetExceptionStops { stops, reply });
            }

            Request::AddCatchpoint {
                calls,
                phase,
                condition,
                reply,
            } => self.edit(Edit::AddCatchpoint {
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
                held,
                reply,
            } => self.start(Start::Attach {
                requested: process_id,
                held,
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
                    Ok((evaluation, moved)) => {
                        let _ = reply.send(Ok(evaluation));
                        if let Some(moved) = moved {
                            let _ = self.events.send(moved);
                        }
                    }
                    Err(error) => {
                        let _ = reply.send(Err(error));
                    }
                }
            }
            Request::Shutdown { reply } => {
                let waiting = self.inferior.is_some() || self.orphans.is_some();
                if !self.shutting_down {
                    self.begin_shutdown(Some(reply));
                } else if !waiting {
                    let _ = reply.send(Ok(()));
                } else if self.shutdown_reply.is_none() {
                    // A shutdown the controller began itself, as a failed
                    // attach does, answers the client's when it ends.
                    self.shutdown_reply = Some(reply);
                } else {
                    let _ = reply.send(Err(Error::RequestCancelled));
                }

                return self.inferior.is_some() || self.orphans.is_some();
            }
            Request::Kill { reply } => self.kill(reply),
            Request::Terminate { reply } => {
                let _ = reply.send(self.terminate());
            }
            Request::HoldForks { children, reply } => {
                self.held_children = Some(children);
                let _ = reply.send(Ok(()));
            }
            request => self.handle_inspection_request(request),
        }

        true
    }
}

impl<P: InspectionOps> Controller<P> {
    /// Answers a read-only request against the current stopped snapshot.
    /// Live and post-mortem sessions route every other request themselves.
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
                let _ = reply.send(self.stopped_selection());
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
                self.views.replace(views);
                let _ = reply.send(Ok(()));
            }
            Request::EnableViews { enabled, reply } => {
                self.views.enabled = enabled;
                let _ = reply.send(Ok(()));
            }
            Request::SetStepIntoRuntime { enter, reply } => {
                let _ = reply.send(Ok(std::mem::replace(&mut self.step_into_runtime, enter)));
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
                let _ = reply.send(
                    Signal::from_code(signal)
                        .map(|signal| self.signals.set(signal, policy))
                        .ok_or(Error::UnknownSignal(signal)),
                );
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
            | Request::HoldForks { .. }
            | Request::Shutdown { .. } => {
                // Their replies differ by variant, so this arm cannot answer
                // them. Dropping the request cancels it at the client.
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
        if self.inferior.is_none() && self.orphans.is_some() {
            return self.handle_orphan_wait(status);
        }

        if self.shutting_down {
            return self.handle_shutdown_wait(status);
        }

        // The status reported a stop, whatever its thread is counted as.
        let reported = (!matches!(
            status,
            WaitEvent::Exited(..) | WaitEvent::Signaled(..) | WaitEvent::Continued(_)
        ))
        .then(|| status.pid());

        // A ptrace request failing because SIGKILL took its thread out of a
        // stop is explained by threads that left their stop, by every
        // thread having reached its exit, or by the debugger's own kill.
        if let Err(error) = self.process_wait(status)
            && !(is_vanished_tracee(&error)
                && (self.release_superseded_threads(reported)
                    || self.every_thread_exiting()
                    || self.kill_reply.is_some()))
        {
            self.fail_inferior(error);
        }

        true
    }

    /// Whether `error` came from SIGKILL taking threads out of the stop the
    /// controller counts them in, so the address space is dying and needs
    /// nothing restored. The failure's handler releases them.
    fn lost_to_sigkill(&self, error: &Error) -> bool {
        is_vanished_tracee(error)
            && self.inferior.as_ref().is_some_and(|inferior| {
                inferior.threads.iter().any(|(&pid, thread)| {
                    matches!(thread.state, NativeThreadState::Stopped)
                        && is_superseded(&self.ptrace.signal_metadata(pid))
                })
            })
    }

    /// Whether every live thread passed its exit event, leaving none to
    /// read or write the dying address space through.
    fn every_thread_exiting(&self) -> bool {
        self.inferior.as_ref().is_some_and(|inferior| {
            inferior
                .threads
                .values()
                .all(|thread| matches!(thread.state, NativeThreadState::Exiting))
        })
    }

    /// Finds the threads counted as stopped, or whose reported stop is
    /// being handled, that SIGKILL took out of their stop, typically a
    /// sibling's `exit_group` while a stop was handled, which explains a
    /// ptrace request failing with ESRCH. Each runs on to its exit, which
    /// retires it. Returns whether any was found.
    fn release_superseded_threads(&mut self, handled: Option<Pid>) -> bool {
        let Some(inferior) = self.inferior.as_mut() else {
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
        let inferior = self.inferior.as_ref().ok_or(Error::NotRunning)?;
        let known = inferior.threads.contains_key(&pid);
        let retired = inferior.retired_threads.contains(&pid);

        if !known {
            // A retired thread's exit, a fork child, or a new thread's first
            // stop arriving before its parent's clone or fork event, which is
            // SIGSTOP for a launched process and PTRACE_EVENT_STOP for a
            // seized one.
            let stop = matches!(
                status,
                WaitEvent::Stopped(..) | WaitEvent::PtraceEvent(_, _, libc::PTRACE_EVENT_STOP)
            );
            let expected = stop
                || retired
                || inferior.fork_children.contains_key(&pid)
                || matches!(
                    status,
                    WaitEvent::PtraceEvent(_, _, libc::PTRACE_EVENT_EXIT)
                        | WaitEvent::Exited(..)
                        | WaitEvent::Signaled(..)
                );
            if expected && self.absorb_untracked_wait(&status) {
                return Ok(());
            }

            return Err(backend_error(LinuxError::UnexpectedWait(format!(
                "unowned {status:?}"
            ))));
        }

        if matches!(inferior.thread(pid)?.expected, ExpectedStop::AdoptedExec) {
            // Until its exec, a process launched through it runs as it
            // would untraced: signals are delivered and job control is
            // ignored.
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
                    .inferior
                    .as_ref()
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

    /// Routes a `PTRACE_EVENT_STOP`, which seized threads report for several
    /// unrelated reasons.
    fn handle_event_stop(&mut self, pid: Pid, signal: Signal) -> Result<()> {
        // SIGKILL, typically a sibling's `exit_group`, may have taken the
        // thread out of the stop since it was reported.
        if is_superseded(&self.ptrace.signal_metadata(pid)) {
            return self.handle_classified_stop(pid, ClassifiedStop::Superseded);
        }

        let thread = self
            .inferior
            .as_ref()
            .ok_or(Error::NotRunning)?
            .thread(pid)?;
        if matches!(thread.expected, ExpectedStop::InitialAttach) {
            return self.handle_initial_attach_stop(pid);
        }

        let stop = if matches!(thread.state, NativeThreadState::Starting)
            && matches!(thread.expected, ExpectedStop::None)
        {
            // Threads auto-attached to a seized process start here instead
            // of with SIGSTOP.
            ClassifiedStop::ThreadStart
        } else if thread.debugger_stop_pending {
            ClassifiedStop::DebuggerRequested
        } else if is_stopping_signal(signal) {
            // A seized thread reports a group-stop here, with its signal.
            ClassifiedStop::GroupStop(signal)
        } else if signal == Signal::SIGTRAP {
            // A `PTRACE_INTERRUPT` that reached a thread already in another
            // ptrace-stop, such as a clone event while attaching, is kept
            // until the thread resumes. It then stops before running any
            // instruction, after the debugger took the other stop for it.
            ClassifiedStop::DebuggerRequested
        } else {
            return self.handle_ptrace_event(pid, libc::PTRACE_EVENT_STOP);
        };

        self.handle_classified_stop(pid, stop)
    }
}

impl<P: InspectionOps> Controller<P> {
    fn snapshot(&self) -> StateSnapshot {
        let Some(inferior) = self.inferior.as_ref() else {
            return StateSnapshot {
                revision: self.revision,
                inferior: InferiorState::NotRunning,
                stop_id: None,
                selected: None,
                selected_frame: None,
                threads: Arc::from([]),
                presentation: None,
                breakpoints: Arc::from(&*self.breakpoints),
                watchpoints: Arc::from([]),
                catchpoints: self.catchpoints.values().cloned().collect(),
            };
        };

        let process_id = process_id(inferior.tgid);
        let state = inferior.public_stop.as_ref().map_or_else(
            || InferiorState::Running {
                process_id,
                execution_id: inferior.active.as_ref().map(|active| active.id),
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
            stop_id: inferior.public_stop.as_ref().map(|stop| stop.id),
            selected: inferior.public_stop.as_ref().map(|stop| stop.selected),
            selected_frame: inferior.public_stop.as_ref().map(selected_frame),
            threads,
            presentation: inferior.public_stop.as_ref().and_then(|stop| {
                stop.selected_thread
                    .and_then(|pid| stop.presentations.get(&pid))
                    .cloned()
            }),
            breakpoints: Arc::from(&*self.breakpoints),
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

    /// The selection a snapshot reports, without copying the breakpoints
    /// and threads that every stopped request would otherwise pay for.
    fn stopped_selection(&self) -> Result<crate::StoppedSelection> {
        let inferior = self.inferior.as_ref().ok_or(Error::NotRunning)?;
        let stop = inferior.public_stop.as_ref().ok_or(Error::NotStopped)?;
        Ok(crate::StoppedSelection {
            process: process_id(inferior.tgid),
            stop: stop.id,
            execution: stop.selected,
            frame: selected_frame(stop),
        })
    }
}

/// The frame selected in the stop's selected context: the innermost until a
/// client selects another.
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
            activities: RefCell::default(),
            task_locators: RefCell::default(),
            task_sets: RefCell::default(),
            returned: None,
        }
    }

    /// A stopped thread through which the process's memory is read: the
    /// selected context's, or the triggering thread for a parked task.
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
    match (&inferior.public_stop, requested) {
        (Some(current), Some(requested)) if current.id == requested => Ok(()),
        (Some(_), Some(_)) => Err(Error::StaleStop),
        (Some(_), None) => Ok(()),
        (None, _) => Err(Error::NotStopped),
    }
}

/// Rejects execution from a stop that cannot be resumed safely.
fn validate_resumable(inferior: &Inferior) -> Result<()> {
    if inferior
        .public_stop
        .as_ref()
        .is_some_and(|stop| matches!(stop.reason, StopReason::Unclassifiable { .. }))
    {
        return Err(Error::UnclassifiableStop);
    }

    validate_image_current(inferior)
}

/// Rejects requests that interpret the address space through the debugged
/// image after exec(2) replaced it; the new image is not loaded.
fn validate_image_current(inferior: &Inferior) -> Result<()> {
    if inferior.exec_unsupported {
        return Err(backend_error(LinuxError::UnsupportedExec));
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
