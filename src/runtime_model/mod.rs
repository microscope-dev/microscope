//! Language and async runtime models for inspecting managed tasks, stacks,
//! exceptions, dynamic values, and interpreted frames.
//!
//! Each runtime implements [`RuntimeModel`] against two narrow abstractions:
//! [`RuntimeImage`] for static debug info and [`RuntimeStop`] for target
//! memory and thread state at a stop.

mod agents;
pub use agents::{
    AgentFailure, AgentRequestTicket, AgentRetirement, AgentRetirementProof, BootstrapTicket,
    PreparedAgentRequest, RuntimeAgent, TargetProcessId,
};

pub mod futures;
mod go;
mod node;
mod python;
pub use node::SCRIPT_EXTENSIONS as JAVASCRIPT_EXTENSIONS;
pub use node::inspector as inspector_protocol;
pub use python::BOOTSTRAP_DIRECTORY_PREFIX as PYTHON_BOOTSTRAP_PREFIX;
pub use python::{
    SCRIPT_DIRECTORY_PREFIX as PYTHON_SCRIPT_PREFIX, agent_bootstrap as python_agent_bootstrap,
    agent_script as python_agent_script, is_agent_filename as is_python_agent_filename,
    is_script_filename as is_python_script_filename, script_name as python_script_name,
};
mod records;
pub mod rust;

#[must_use]
pub fn names_runtime_function(name: &str) -> bool {
    ["py:", "js:"].iter().any(|prefix| {
        name.strip_prefix(prefix)
            .is_some_and(|name| !name.starts_with(':'))
    })
}
pub mod thread_ids;
mod tokio;

use std::sync::Arc;

use crate::inspection::InspectionBudget;
use crate::unwind::RegisterFile;
use crate::{
    AddressRange, CodeRole, CoroutineInfo, EntryProvenance, ExceptionFilter, ImageAddress,
    IntegerValue, ModuleImage, RecordMemberLayout, RecordedPlace, SourceLanguage, StackFrame,
    StackSegment, TaskState, ThreadId, ThreadLocal, TypeInfo, TypeKind, TypeReference,
    VirtualAddress,
};

/// Result paired with non-fatal diagnostic gaps encountered while reading
/// target state.
///
/// Runtime queries return partial data rather than failing outright when a
/// single goroutine, shard, or label set is unreadable or mid-mutation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Partial<T> {
    pub value: T,
    pub gaps: Vec<Arc<str>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DrivenFuture {
    pub object: VirtualAddress,
    pub ty: crate::TypeReference,

    pub chain: Vec<futures::AsyncFrame>,

    pub running: Vec<crate::TypeReference>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Member {
    pub offset: u64,
    pub size: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ImageSymbol {
    pub address: ImageAddress,

    pub size: Option<u64>,
}

/// Static debug-info view of a loaded binary used during runtime detection and
/// layout binding.
///
/// Kept as a narrow trait over [`ModuleImage`] so runtime models stay decoupled
/// from DWARF/ELF internals and can be tested with synthetic images.
pub trait RuntimeImage: std::fmt::Debug {
    fn producers(&self) -> Vec<&str>;

    fn constant(&self, name: &str) -> Option<IntegerValue>;

    fn symbol(&self, name: &str) -> Option<ImageSymbol>;

    fn functions_starting(&self, prefix: &str) -> Vec<ImageSymbol> {
        let _ = prefix;
        Vec::new()
    }

    fn function_answering(&self, name: &str) -> Option<ImageSymbol>;

    fn symbol_at(&self, address: ImageAddress) -> Option<Arc<str>>;

    fn vtable_class(&self, address: ImageAddress) -> Option<(String, ImageAddress)> {
        let _ = address;
        None
    }

    fn has_function(&self, name: &str) -> bool;

    fn function_body(&self, name: &str) -> Option<ImageAddress>;

    fn function_entries(&self, name: &str) -> Vec<ImageAddress> {
        let _ = name;
        Vec::new()
    }

    fn member(&self, type_name: &str, path: &[&str]) -> Option<Member>;

    fn function_name(&self, address: ImageAddress) -> Option<Arc<str>>;

    fn function_generics(&self, address: ImageAddress) -> Vec<(Arc<str>, String)> {
        let _ = address;
        Vec::new()
    }

    fn thread_local(&self, name: &str) -> Option<Result<ThreadLocal, Arc<str>>>;

    fn thread_local_within(
        &self,
        scope: &str,
        name: &str,
    ) -> Option<Result<ThreadLocal, Arc<str>>> {
        let _ = (scope, name);
        None
    }

    fn types_named(&self, name: &str) -> Vec<TypeReference> {
        let _ = name;
        Vec::new()
    }

    fn type_info(&self, ty: TypeReference) -> Option<&TypeInfo> {
        let _ = ty;
        None
    }

    fn same_type(&self, left: TypeReference, right: TypeReference) -> bool {
        left == right
    }

    fn source_path_ending(&self, suffix: &str) -> Option<std::path::PathBuf> {
        let _ = suffix;
        None
    }

    fn coroutine(&self, ty: TypeReference) -> Option<Result<&CoroutineInfo, &Arc<str>>> {
        let _ = ty;
        None
    }

    fn trait_object_type(&self, address: ImageAddress) -> Option<TypeReference> {
        let _ = address;
        None
    }

    fn coroutine_body(&self, ty: TypeReference) -> Option<ImageAddress> {
        let _ = ty;
        None
    }

    fn python(&self) -> Option<crate::image::python::PythonRuntime> {
        None
    }

    fn v8(&self) -> Option<crate::image::v8::V8Runtime> {
        None
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TargetThreadId(u64);

impl TargetThreadId {
    pub const fn new(value: u64) -> Option<Self> {
        if value == 0 { None } else { Some(Self(value)) }
    }

    pub const fn get(self) -> u64 {
        self.0
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ReportedLocal {
    pub name: Arc<str>,
    pub value: ReportedValue,

    pub block: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ReportedValue {
    Undefined,
    Null,
    Boolean(bool),
    Number(f64),
    String(Arc<str>),

    BigInt(Arc<str>),
    Symbol,

    Object(Arc<str>),

    Unavailable,

    Unknown,
}

pub type ReportedLocals = Result<Arc<[ReportedLocal]>, Arc<str>>;

/// Live target view at a single debugger stop (memory, threads, TLS, and
/// load bias).
pub trait RuntimeStop {
    fn reported_locals(&self, _frame: VirtualAddress) -> Option<ReportedLocals> {
        None
    }

    fn read(&self, address: VirtualAddress, bytes: &mut [u8]) -> bool;

    fn thread_pointer(&self, thread: ThreadId) -> Option<u64>;

    fn thread_local_address(
        &self,
        thread: ThreadId,
        place: ThreadLocal,
    ) -> Result<VirtualAddress, Arc<str>> {
        let offset = match place {
            ThreadLocal::Offset(offset) => offset,
            ThreadLocal::Slot(slot) => {
                let mut word = [0; 8];
                if !self.read(self.load_bias().wrapping_load(slot), &mut word) {
                    return Err(
                        "the slot of its offset from the thread pointer is unreadable".into(),
                    );
                }
                i64::from_le_bytes(word)
            }
            ThreadLocal::Dynamic(offset) => return self.module_thread_local(thread, offset),
        };
        let pointer = self
            .thread_pointer(thread)
            .ok_or("the thread's thread pointer is unreadable")?;
        Ok(VirtualAddress::new(pointer.wrapping_add_signed(offset)))
    }

    fn module_thread_local(
        &self,
        _thread: ThreadId,
        _offset: u64,
    ) -> Result<VirtualAddress, Arc<str>> {
        Err("the module's block of thread-local storage cannot be found here".into())
    }

    fn instruction(&self, thread: ThreadId) -> Option<VirtualAddress>;

    fn load_bias(&self) -> crate::LoadBias;

    fn threads(&self) -> Vec<ThreadId>;

    fn main_thread(&self) -> Option<ThreadId> {
        None
    }

    fn target_thread_id(&self, _thread: ThreadId) -> Option<TargetThreadId> {
        None
    }

    fn target_thread_ids(&self, threads: &[ThreadId]) -> Vec<(ThreadId, TargetThreadId)> {
        threads
            .iter()
            .filter_map(|&thread| Some((thread, self.target_thread_id(thread)?)))
            .collect()
    }

    fn task_sets(&self) -> Vec<u64> {
        Vec::new()
    }

    fn blocked_futex(&self, _thread: ThreadId) -> Option<u64> {
        None
    }

    fn memory(&self) -> Option<&StopMemory> {
        None
    }

    fn runs(&self) -> bool {
        false
    }

    fn read_each(&self, reads: &mut [(VirtualAddress, &mut [u8])]) -> Vec<bool> {
        reads
            .iter_mut()
            .map(|(address, bytes)| self.read(*address, bytes))
            .collect()
    }
}

type ThreadIdPairs = Arc<[(ThreadId, TargetThreadId)]>;

/// Per-stop aligned block cache for target memory reads and thread identity
/// lookups.
///
/// Coalesces small struct-field reads into fixed-size blocks and tracks which
/// blocks were touched so non-stopping live stack walks (`stop.runs()`) can
/// batch-read pages via `read_each` to reduce tearing.
#[derive(Debug)]
pub struct StopMemory {
    blocks: std::cell::RefCell<std::collections::BTreeMap<u64, Block>>,
    thread_ids: std::cell::RefCell<Option<ThreadIdPairs>>,
    block: usize,
}

#[derive(Debug)]
struct Block {
    bytes: Option<Box<[u8]>>,
    used: std::cell::Cell<bool>,
}

impl Default for StopMemory {
    fn default() -> Self {
        Self::with_block(Self::BLOCK)
    }
}

impl StopMemory {
    pub const BLOCK: usize = 256;

    pub fn with_block(block: usize) -> Self {
        debug_assert!(block.is_power_of_two() && block <= 4096, "{block}");
        Self {
            blocks: std::cell::RefCell::default(),
            thread_ids: std::cell::RefCell::default(),
            block,
        }
    }

    pub const fn block(&self) -> usize {
        self.block
    }

    const MAX_BLOCKS: usize = 1 << 14;

    pub fn copy<E>(
        &self,
        at: u64,
        into: &mut [u8],
        read: impl FnOnce(VirtualAddress, &mut [u8]) -> Result<bool, E>,
    ) -> Result<Option<usize>, E> {
        let block = self.block as u64;
        let start = at - at % block;
        let offset = usize::try_from(at - start).unwrap_or(self.block);
        let copy = |held: &[u8], into: &mut [u8]| {
            let held = held.get(offset..)?;
            let copied = held.len().min(into.len());
            into.get_mut(..copied)?.copy_from_slice(held.get(..copied)?);
            Some(copied)
        };

        if let Some(kept) = self.blocks.borrow().get(&start) {
            kept.used.set(true);
            return Ok(kept.bytes.as_deref().and_then(|held| copy(held, into)));
        }

        let mut bytes = vec![0; self.block].into_boxed_slice();
        let read = read(VirtualAddress::new(start), &mut bytes)?.then_some(bytes);
        let copied = read.as_deref().and_then(|held| copy(held, into));
        let mut blocks = self.blocks.borrow_mut();
        if blocks.len() < Self::MAX_BLOCKS {
            blocks.insert(
                start,
                Block {
                    bytes: read,
                    used: std::cell::Cell::new(true),
                },
            );
        }

        Ok(copied)
    }

    pub fn prefetch(
        &self,
        starts: &[u64],
        read: impl FnOnce(&mut [(VirtualAddress, &mut [u8])]) -> Vec<bool>,
    ) {
        let block = self.block as u64;
        let held = self.blocks.borrow();
        let mut wanted: Vec<u64> = starts
            .iter()
            .filter(|&&start| start % block == 0 && !held.contains_key(&start))
            .copied()
            .collect();
        wanted.sort_unstable();
        wanted.dedup();
        wanted.truncate(Self::MAX_BLOCKS.saturating_sub(held.len()));
        drop(held);
        if wanted.is_empty() {
            return;
        }
        let mut buffers: Vec<Box<[u8]>> = wanted
            .iter()
            .map(|_| vec![0; self.block].into_boxed_slice())
            .collect();
        let mut reads: Vec<(VirtualAddress, &mut [u8])> = wanted
            .iter()
            .zip(buffers.iter_mut())
            .map(|(&start, bytes)| (VirtualAddress::new(start), &mut **bytes))
            .collect();
        let read = read(&mut reads);
        drop(reads);
        let mut blocks = self.blocks.borrow_mut();
        for ((start, bytes), read) in wanted.into_iter().zip(buffers).zip(read) {
            if read {
                blocks.insert(
                    start,
                    Block {
                        bytes: Some(bytes),
                        used: std::cell::Cell::new(false),
                    },
                );
            }
        }
    }

    pub fn used_blocks(&self) -> Vec<u64> {
        self.blocks
            .borrow()
            .iter()
            .filter(|(_, held)| held.used.get() && held.bytes.is_some())
            .map(|(&start, _)| start)
            .collect()
    }

    pub fn thread_identities(
        &self,
        expected_threads: &[ThreadId],
        read: impl FnOnce() -> Vec<(ThreadId, TargetThreadId)>,
    ) -> Arc<[(ThreadId, TargetThreadId)]> {
        let matches = |ids: &[(ThreadId, TargetThreadId)]| {
            ids.iter()
                .map(|(host, _)| *host)
                .eq(expected_threads.iter().copied())
        };
        if let Some(known) = &*self.thread_ids.borrow()
            && matches(known)
        {
            return Arc::clone(known);
        }

        let mut ids = read();
        ids.sort_unstable();
        ids.dedup();
        let ids: Arc<[_]> = ids.into();

        if matches(&ids) {
            *self.thread_ids.borrow_mut() = Some(Arc::clone(&ids));
        }
        ids
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CodeAddress {
    pub address: VirtualAddress,

    pub after_call: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TaskStarter {
    pub entry: ImageAddress,
    pub names_at_entry: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StartedTask {
    pub task: RuntimeTask,
    pub coroutine: Option<crate::TypeReference>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeTask {
    pub number: u64,

    pub locator: u64,
    pub state: TaskState,

    pub detail: Option<Arc<str>>,

    pub thread: Option<ThreadId>,

    pub resume: Option<CodeAddress>,

    pub creation: Option<CodeAddress>,

    pub spawned: Option<crate::RecordedPlace>,

    pub entry: Option<VirtualAddress>,

    pub parent: Option<u64>,

    pub internal: bool,

    pub labels: TaskLabels,

    pub callback: Option<TaskCallback>,

    pub stackless: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskCallback {
    pub function: Arc<str>,
    pub place: Option<crate::RecordedPlace>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TaskRef {
    pub number: u64,
    pub locator: Option<u64>,
}

pub type TaskLabels = Vec<(Arc<str>, Arc<str>)>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskPage {
    pub tasks: Vec<RuntimeTask>,

    pub next: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ThreadActivity {
    Task { number: u64, stack: StackSegment },

    Idle,

    Outside,

    Unknown(Arc<str>),
}

#[derive(Debug, Clone)]
pub enum TaskContext {
    OnThread(ThreadId),

    Saved {
        registers: Box<RegisterFile>,

        after_call: bool,
    },

    Suspended {
        future: VirtualAddress,
        ty: TypeReference,
    },

    Interpreted {
        frames: Arc<[InterpretedFrame]>,
        end: Option<Arc<str>>,
    },
}

/// Directive returned by [`RuntimeModel::cross`] when the unwinder reaches a
/// runtime stack-switching frame.
#[derive(Debug, Clone)]
pub enum Crossing {
    /// Keep unwinding normally on the current stack.
    Stay,

    /// Switch to the returned registers and re-unwind the same frame on the
    /// target stack (used when a call returns across stacks).
    Resume(RegisterFile),

    /// Switch directly to the returned registers as the next caller frame
    /// (used when a runtime trampoline abandons the current stack).
    Continue(RegisterFile),

    /// Stop unwinding; this frame is the bottom of the stack.
    Outermost,
}

pub const TASK_NOUNS: [(&str, &str); 3] = [go::TASK_NOUN, self::tokio::TASK_NOUN, node::TASK_NOUN];

pub const EXCEPTION_FILTERS: [ExceptionFilter; 8] = [
    go::EXCEPTION_FILTERS[0],
    go::EXCEPTION_FILTERS[1],
    go::EXCEPTION_FILTERS[2],
    rust::EXCEPTION_FILTERS[0],
    python::EXCEPTION_FILTERS[0],
    python::EXCEPTION_FILTERS[1],
    node::EXCEPTION_FILTERS[0],
    node::EXCEPTION_FILTERS[1],
];

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RuntimeSignals {
    pub deferrable: &'static [i32],

    pub handled: &'static [i32],
}

#[derive(Debug, Clone)]
pub struct Unbound {
    pub reason: Arc<str>,

    pub deferrable: &'static [i32],
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RuntimeHook {
    pub filter: &'static ExceptionFilter,
    pub address: ImageAddress,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeException {
    pub message: Arc<str>,

    pub value: Option<Arc<str>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum HookPlace {
    Own(ImageAddress),

    Companion(&'static str),
}

/// Compare-and-swap patch against target memory.
///
/// Callers verify `expected` matches current target bytes before writing
/// `bytes` so stale arming writes cannot clobber concurrent runtime state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeWrite {
    pub address: VirtualAddress,

    pub expected: Vec<u8>,

    pub bytes: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HookEvent {
    /// A safe entry point at which installation can be retried.
    AgentEntry,

    AgentFailed {
        generation: u64,
        description: Arc<str>,
    },

    /// The agent applied this generation of its whole desired state.
    Armed { generation: u64 },

    /// Nothing the user sees: the thread goes on.
    Continue,

    Exception {
        filter: &'static ExceptionFilter,
        report: RuntimeException,
    },

    /// One or more managed breakpoints hit together at a single thread event.
    Breakpoint { numbers: Vec<u64> },

    /// The armed runtime step finished at its target location.
    StepEnded { number: u64 },

    /// The armed runtime step stopped before reaching its target location.
    StepIncomplete { number: u64, description: Arc<str> },

    /// The armed runtime step is about to enter native code at `target`.
    StepCalls { number: u64, target: VirtualAddress },

    Assigned {
        number: u64,
        failure: Option<Arc<str>>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Arming<'a> {
    pub breakpoints: &'a [ManagedArm],

    pub step: Option<&'a RuntimeStep>,

    pub assignment: Option<&'a RuntimeAssignment>,

    pub exceptions: crate::protocol::ExceptionStops,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeAssignment {
    pub number: u64,

    pub thread: ThreadId,
    pub frame: VirtualAddress,
    pub slot: u32,
    pub value: AssignedValue,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AssignedValue {
    Integer(Arc<str>),

    Float(u64),
    Bool(bool),
    Text(Arc<str>),
    Nothing,
    Object(InterpretedHandle),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeStep {
    pub number: u64,
    pub kind: crate::StepKind,

    pub thread: ThreadId,

    pub frame: Option<VirtualAddress>,

    pub returning: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManagedArm {
    pub number: u64,
    pub place: crate::ManagedPlace,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StoredValue<'a> {
    Memory(VirtualAddress),

    Bytes(&'a [u8]),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DynamicValue {
    Nil,

    Held {
        descriptor: VirtualAddress,

        offset: u64,
        place: HeldPlace,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HeldPlace {
    Memory(VirtualAddress),

    Within(u64),
}

/// Runtime-specific inspection and control interface implemented per detected
/// language or async runtime (Go, Rust, Tokio, Python, Node).
///
/// Most methods default to no-op or unsupported so each runtime only implements
/// the capabilities it owns.
pub trait RuntimeModel: Send + Sync + std::fmt::Debug {
    fn tasks(
        &self,
        stop: &dyn RuntimeStop,
        start: u64,
        limit: usize,
        program_only: bool,
    ) -> Partial<TaskPage>;

    fn thread_activity(&self, stop: &dyn RuntimeStop, thread: ThreadId) -> ThreadActivity;

    fn current_task(
        &self,
        stop: &dyn RuntimeStop,
        thread: ThreadId,
    ) -> Result<Option<u64>, Arc<str>> {
        match self.thread_activity(stop, thread) {
            ThreadActivity::Task { number, .. } => Ok(Some(number)),
            ThreadActivity::Unknown(reason) => Err(reason),
            ThreadActivity::Idle | ThreadActivity::Outside => Ok(None),
        }
    }

    fn task_context(
        &self,
        stop: &dyn RuntimeStop,
        task: TaskRef,
    ) -> Result<Option<TaskContext>, Arc<str>>;

    fn running_future(
        &self,
        stop: &dyn RuntimeStop,
        thread: ThreadId,
    ) -> Result<Option<(VirtualAddress, TypeReference)>, Arc<str>> {
        let _ = (stop, thread);
        Ok(None)
    }

    fn thread_stacks(
        &self,
        stop: &dyn RuntimeStop,
        thread: ThreadId,
    ) -> Result<Vec<(std::ops::Range<u64>, StackSegment)>, Arc<str>>;

    fn cross(
        &self,
        stop: &dyn RuntimeStop,
        thread: ThreadId,
        frame: &RegisterFile,
        after_call: bool,
    ) -> Result<Crossing, Arc<str>>;

    fn signals(&self) -> RuntimeSignals;

    fn hooks(&self) -> &[RuntimeHook];

    fn exception(
        &self,
        stop: &dyn RuntimeStop,
        hook: ImageAddress,
        registers: &RegisterFile,
    ) -> Result<RuntimeException, Arc<str>>;

    fn dynamic_value(
        &self,
        stop: &dyn RuntimeStop,
        representation: &str,
        value: StoredValue<'_>,
    ) -> Option<Result<DynamicValue, Arc<str>>>;

    fn agent(&self) -> Option<&dyn RuntimeAgent> {
        None
    }

    fn internal_hooks(&self) -> &[HookPlace] {
        &[]
    }

    fn hook_event(
        &self,
        stop: &dyn RuntimeStop,
        hook: HookPlace,
        registers: &RegisterFile,
    ) -> Result<HookEvent, Arc<str>> {
        let _ = (stop, hook, registers);
        Err("the runtime has no internal hooks".into())
    }

    fn resolve_breakpoint(
        &self,
        stop: &dyn RuntimeStop,
        spec: &crate::BreakpointSpec,
    ) -> Option<Result<Vec<crate::ManagedPlace>, Arc<str>>> {
        let _ = (stop, spec);
        None
    }

    fn arm(
        &self,
        stop: &dyn RuntimeStop,
        arming: &Arming<'_>,
    ) -> Result<Vec<RuntimeWrite>, Arc<str>> {
        let _ = (stop, arming);
        Err("the runtime reports nothing of its own".into())
    }

    fn stack_mover(&self) -> Option<ImageAddress>;

    fn frame_pointer_code(&self) -> Vec<AddressRange<ImageAddress>> {
        Vec::new()
    }

    fn inspector_hook(&self) -> Option<ImageAddress> {
        None
    }

    fn native_entry(&self) -> Option<ImageAddress> {
        None
    }

    fn native_callee(
        &self,
        stop: &dyn RuntimeStop,
        registers: &RegisterFile,
    ) -> Result<VirtualAddress, Arc<str>> {
        let _ = (stop, registers);
        Err("the runtime enters no native functions of the program's".into())
    }

    fn inspector_target(
        &self,
        _stop: &dyn RuntimeStop,
        _caller: VirtualAddress,
        _delegate: VirtualAddress,
    ) -> Result<Option<Arc<str>>, Arc<str>> {
        Err("the runtime cannot prove its inspector target caller".into())
    }

    fn inspector_signal_handler(&self) -> Option<ImageAddress> {
        None
    }

    fn inspector_close_functions(&self) -> Option<(ImageAddress, ImageAddress)> {
        None
    }

    fn inspector_startup_hooks(&self) -> Option<[ImageAddress; 3]> {
        None
    }

    fn inspector_startup_pending(
        &self,
        _stop: &dyn RuntimeStop,
        _agent: VirtualAddress,
    ) -> Result<bool, Arc<str>> {
        Err("the runtime cannot prove inspector startup quiescence".into())
    }

    fn inspector_agent(
        &self,
        _stop: &dyn RuntimeStop,
    ) -> Result<Option<(VirtualAddress, Arc<str>)>, Arc<str>> {
        Err("the runtime cannot identify its native inspector agent".into())
    }

    fn inspector_message_matches(
        &self,
        _stop: &dyn RuntimeStop,
        _view: VirtualAddress,
        _expected: &[u8],
    ) -> Result<bool, Arc<str>> {
        Err("the runtime cannot read a native inspector message".into())
    }

    fn inspector_message_received(&self) -> Option<ImageAddress> {
        None
    }

    fn inspector_received_matches(
        &self,
        _stop: &dyn RuntimeStop,
        _message: VirtualAddress,
        _expected: &[u8],
    ) -> Result<bool, Arc<str>> {
        Err("the runtime cannot read a message its inspector received".into())
    }

    fn inspector_endpoint(
        &self,
        _stop: &dyn RuntimeStop,
    ) -> Result<Option<(std::net::SocketAddr, Arc<str>)>, Arc<str>> {
        Ok(None)
    }

    fn moving_task(
        &self,
        stop: &dyn RuntimeStop,
        registers: &RegisterFile,
    ) -> Result<u64, Arc<str>>;

    fn task_stack(
        &self,
        stop: &dyn RuntimeStop,
        task: TaskRef,
    ) -> Result<Option<std::ops::Range<u64>>, Arc<str>>;

    fn call_out(
        &self,
        entry: ImageAddress,
        registers: &RegisterFile,
    ) -> Option<Result<VirtualAddress, Arc<str>>>;

    fn task_starters(&self) -> Vec<TaskStarter>;

    fn started_task(
        &self,
        stop: &dyn RuntimeStop,
        starter: ImageAddress,
        registers: &RegisterFile,
    ) -> Result<Option<StartedTask>, Arc<str>>;

    fn task_noun(&self) -> &'static str;

    fn held_task_sets(&self, stop: &dyn RuntimeStop, driven: &DrivenFuture) -> Partial<Vec<u64>> {
        let _ = (stop, driven);
        Partial {
            value: Vec::new(),
            gaps: Vec::new(),
        }
    }

    fn may_run_task_set(&self, ty: crate::TypeReference) -> bool {
        let _ = ty;
        false
    }

    fn has_task_sets(&self) -> bool {
        false
    }

    fn driver_role(&self) -> Option<CodeRole> {
        None
    }

    fn driven(&self, frame: &StackFrame, inner: Option<&StackFrame>) -> Option<Driven> {
        let _ = (frame, inner);
        None
    }

    fn polling(&self, frame: &StackFrame) -> Option<&'static str> {
        let _ = frame;
        None
    }

    fn activation(&self, name: &str, inner: Option<&str>) -> bool {
        let _ = (name, inner);
        false
    }

    fn interpreted(&self, stop: &dyn RuntimeStop) -> Option<Partial<InterpretedStacks>> {
        let _ = stop;
        None
    }

    fn thread_labels(&self, stop: &dyn RuntimeStop) -> Partial<Vec<ThreadLabels>> {
        let _ = stop;
        Partial {
            value: Vec::new(),
            gaps: Vec::new(),
        }
    }

    fn thread_holding(&self, stop: &dyn RuntimeStop, thread: ThreadId) -> Option<ThreadHolding> {
        let _ = (stop, thread);
        None
    }

    fn walked_threads(&self, stop: &dyn RuntimeStop) -> Option<Partial<Vec<WalkedThread>>> {
        let _ = stop;
        None
    }

    fn sampled_frames(
        &self,
        stop: &dyn RuntimeStop,
        frames: &[SampledFrame],
    ) -> (Vec<InterpretedFrame>, Option<Arc<str>>) {
        let _ = (stop, frames);
        (
            Vec::new(),
            Some("the runtime reads no sampled frames".into()),
        )
    }

    fn sampled_code_frame(
        &self,
        stop: &dyn RuntimeStop,
        frame: &SampledCodeFrame,
    ) -> Option<Vec<crate::RecordedFrame>> {
        let _ = (stop, frame);
        None
    }

    fn interpreted_locals(
        &self,
        stop: &dyn RuntimeStop,
        frame: InterpretedFrameRef,
        budget: &mut InspectionBudget,
    ) -> Result<Vec<InterpretedLocal>, Arc<str>> {
        let _ = (stop, frame, budget);
        Err("the runtime interprets no frames".into())
    }

    fn interpreted_children(
        &self,
        stop: &dyn RuntimeStop,
        (object, part): (InterpretedHandle, &[u64]),
        range: std::ops::Range<u64>,
        budget: &mut InspectionBudget,
    ) -> Result<Vec<InterpretedChild>, Arc<str>> {
        let _ = (stop, object, part, range, budget);
        Err("the runtime interprets no objects".into())
    }

    fn interpreted_name(
        &self,
        stop: &dyn RuntimeStop,
        frame: InterpretedFrameRef,
        name: &str,
        budget: &mut InspectionBudget,
    ) -> Result<Option<InterpretedName>, InterpretedFailure> {
        let _ = (stop, frame, name, budget);
        Err(InterpretedFailure::interprets_nothing())
    }

    fn interpreted_variable(
        &self,
        stop: &dyn RuntimeStop,
        frame: InterpretedFrameRef,
        slot: u32,
        budget: &mut InspectionBudget,
    ) -> Result<InterpretedHandle, InterpretedFailure> {
        let _ = (stop, frame, slot, budget);
        Err(InterpretedFailure::interprets_nothing())
    }

    fn assigned_binding(
        &self,
        stop: &dyn RuntimeStop,
        frame: InterpretedFrameRef,
        slot: u32,
        budget: &mut InspectionBudget,
    ) -> Result<AssignedBinding, InterpretedFailure> {
        let _ = (stop, frame, slot, budget);
        Err(InterpretedFailure::interprets_nothing())
    }

    fn interpreted_watch(
        &self,
        stop: &dyn RuntimeStop,
        frame: InterpretedFrameRef,
        slot: u32,
        budget: &mut InspectionBudget,
    ) -> Result<InterpretedWatch, InterpretedFailure> {
        let _ = (stop, frame, slot, budget);
        Err(InterpretedFailure::interprets_nothing())
    }

    fn interpreted_frame_runs(
        &self,
        stop: &dyn RuntimeStop,
        thread: ThreadId,
        frame: VirtualAddress,
        code: VirtualAddress,
        watched: VirtualAddress,
    ) -> Result<bool, Arc<str>> {
        let _ = (stop, thread, frame, code, watched);
        Err("the runtime interprets no frames".into())
    }

    fn stack_frame_builders(&self) -> Option<Vec<AddressRange<ImageAddress>>> {
        None
    }

    fn collector(&self) -> Option<ImageAddress> {
        None
    }

    fn frame_replacers(&self) -> Vec<FrameReplacer> {
        Vec::new()
    }

    fn interpreted_place(
        &self,
        stop: &dyn RuntimeStop,
        frame: VirtualAddress,
        watched: VirtualAddress,
    ) -> Result<InterpretedPlace, Arc<str>> {
        let _ = (stop, frame, watched);
        Err("the runtime moves no interpreted frame's objects".into())
    }

    fn interpreted_placed(
        &self,
        stop: &dyn RuntimeStop,
        frame: VirtualAddress,
        place: InterpretedPlace,
    ) -> Result<InterpretedWatch, Arc<str>> {
        let _ = (stop, frame, place);
        Err("the runtime moves no interpreted frame's objects".into())
    }

    fn sole_thread(&self, stop: &dyn RuntimeStop, thread: ThreadId) -> Result<bool, Arc<str>> {
        let _ = (stop, thread);
        Err("the runtime cannot say which threads run it".into())
    }

    fn interpreted_watched(&self, bits: u64) -> Result<Option<InterpretedHandle>, Arc<str>> {
        let _ = bits;
        Err("the runtime interprets no frames".into())
    }

    fn interpreted_step(
        &self,
        stop: &dyn RuntimeStop,
        object: InterpretedHandle,
        taken: InterpretedStep<'_>,
        budget: &mut InspectionBudget,
    ) -> Result<InterpretedHandle, InterpretedFailure> {
        let _ = (stop, object, taken, budget);
        Err(InterpretedFailure::interprets_nothing())
    }

    fn interpreted_operand(
        &self,
        stop: &dyn RuntimeStop,
        object: InterpretedHandle,
        budget: &mut InspectionBudget,
    ) -> Result<InterpretedOperand, InterpretedFailure> {
        let _ = (stop, object, budget);
        Err(InterpretedFailure::interprets_nothing())
    }

    fn interpreted_length(
        &self,
        stop: &dyn RuntimeStop,
        object: InterpretedHandle,
        budget: &mut InspectionBudget,
    ) -> Result<u64, InterpretedFailure> {
        let _ = (stop, object, budget);
        Err(InterpretedFailure::interprets_nothing())
    }

    fn interpreted_language(&self) -> crate::SourceLanguage {
        crate::SourceLanguage::Python
    }

    fn object_types(&self) -> Option<ObjectTypes> {
        None
    }

    fn interpreted_address(&self, handle: InterpretedHandle) -> Option<VirtualAddress> {
        let _ = handle;
        None
    }

    fn interpreted_handle(
        &self,
        stop: &dyn RuntimeStop,
        address: VirtualAddress,
        budget: &mut InspectionBudget,
    ) -> Result<InterpretedHandle, InterpretedFailure> {
        let _ = (stop, address, budget);
        Err(InterpretedFailure::interprets_nothing())
    }

    fn owns_code(&self, address: ImageAddress) -> bool {
        let _ = address;
        false
    }

    fn process_code(&self, stop: &dyn RuntimeStop) -> Vec<AddressRange<VirtualAddress>> {
        let _ = stop;
        Vec::new()
    }

    fn owns_generated_code(&self) -> bool {
        false
    }

    fn code_frame(&self, stop: &dyn RuntimeStop, frame: &CodeFrame<'_>) -> Option<DescribedFrame> {
        let _ = (stop, frame);
        None
    }

    fn live_frames(&self, stop: &dyn RuntimeStop, thread: &LiveThread<'_>) -> Option<LiveFrames> {
        let _ = (stop, thread);
        None
    }

    fn interpreted_lineage(
        &self,
        stop: &dyn RuntimeStop,
        class: InterpretedHandle,
        budget: &mut InspectionBudget,
    ) -> Result<Vec<crate::TypeIdentity>, InterpretedFailure> {
        let _ = (stop, class, budget);
        Err(InterpretedFailure::interprets_nothing())
    }

    fn interpreted_value(
        &self,
        stop: &dyn RuntimeStop,
        object: InterpretedHandle,
        budget: &mut InspectionBudget,
    ) -> InterpretedSlot {
        let _ = (stop, object, budget);
        InterpretedSlot::Unreadable("the runtime interprets no objects".into())
    }

    fn own_source(&self, path: &std::path::Path) -> bool {
        let _ = path;
        false
    }

    fn task_entries(
        &self,
        stop: &dyn RuntimeStop,
        task: TaskRef,
    ) -> Result<Option<TaskEntries>, Arc<str>> {
        let _ = (stop, task);
        Ok(None)
    }

    fn takes_up(
        &self,
        stop: &dyn RuntimeStop,
        task: &TaskEntries,
        entry: VirtualAddress,
        registers: &RegisterFile,
    ) -> bool {
        let _ = (stop, task, entry, registers);
        false
    }

    fn task_end(
        &self,
        stop: &dyn RuntimeStop,
        task: &TaskEntries,
    ) -> Result<Option<TaskEnd>, Arc<str>> {
        let _ = (stop, task);
        Ok(None)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskEntries {
    pub task: TaskRef,

    pub runs: Vec<VirtualAddress>,

    pub frees: Vec<VirtualAddress>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct InterpretedStacks {
    pub threads: Vec<InterpretedThread>,

    pub frames: Vec<InterpretedFrame>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WalkedThread {
    pub native: u64,
    pub state: VirtualAddress,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SampledFrame {
    pub code: VirtualAddress,
    pub proof: SampledCode,

    pub offset: Option<i64>,
    pub owner: u32,

    pub function: Option<u64>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SampledCode {
    pub qualname: u64,
    pub linetable: u64,
    pub first_line: i32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SampledCodeFrame {
    pub frame_pointer: u64,
    pub instruction: VirtualAddress,
    pub after_call: bool,

    pub generated: bool,

    pub function: u64,
    pub shared: u64,
    pub script: u64,
    pub script_id: u64,
    pub literal_id: i32,

    pub bytecode: u64,
    pub bytecode_offset: u64,
}

#[derive(Debug, Clone, Copy)]
pub struct CodeFrame<'a> {
    pub thread: ThreadId,

    pub instruction: VirtualAddress,

    pub after_call: bool,

    pub generated: bool,

    pub image_instruction: Option<ImageAddress>,

    pub entered: bool,

    pub registers: &'a RegisterFile,
}

#[derive(Clone)]
pub struct LiveThread<'a> {
    pub thread: ThreadId,
    pub stack_pointer: u64,

    pub stack: std::ops::Range<u64>,

    pub code: &'a dyn Fn(VirtualAddress) -> CodeAt,
}

impl std::fmt::Debug for LiveThread<'_> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("LiveThread")
            .field("thread", &self.thread)
            .field("stack_pointer", &self.stack_pointer)
            .field("stack", &self.stack)
            .finish_non_exhaustive()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CodeAt {
    Image(ImageAddress),

    Generated,

    Other,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LiveFrames {
    None,

    Frames(Vec<InterpretedFrame>),

    Unread(Arc<str>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DescribedFrame {
    pub functions: Vec<crate::RecordedFrame>,

    pub role: CodeRole,

    pub live_registers: bool,

    pub awaiting: Option<Awaiting>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Awaiting {
    pub frames: Vec<InterpretedFrame>,

    pub end: Option<Arc<str>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct InterpretedFrameRef {
    pub address: VirtualAddress,
    pub live_registers: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThreadLabels {
    pub thread: ThreadId,
    pub labels: Vec<(Arc<str>, Arc<str>)>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThreadHolding {
    pub kind: &'static str,
    pub identity: Result<HeldIdentity, Arc<str>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct HeldIdentity(u64);

impl HeldIdentity {
    pub const fn new(value: u64) -> Self {
        Self(value)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InterpretedThread {
    pub thread: ThreadId,

    pub interpreter: i64,

    pub frames: std::ops::Range<usize>,

    pub end: Option<Arc<str>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InterpretedFrame {
    pub frame: VirtualAddress,

    pub language: SourceLanguage,

    pub function: Option<Arc<str>>,
    pub place: Option<RecordedPlace>,
    pub problem: Option<Arc<str>>,

    pub first_line: Option<u32>,

    pub outermost: bool,

    pub role: crate::CodeRole,
}

impl InterpretedStacks {
    #[must_use]
    pub fn frames_of(&self, thread: &InterpretedThread) -> &[InterpretedFrame] {
        self.frames.get(thread.frames.clone()).unwrap_or_default()
    }

    #[must_use]
    pub fn whole(&self, thread: &InterpretedThread) -> bool {
        thread.end.is_none()
            && self
                .frames_of(thread)
                .iter()
                .all(|frame| frame.problem.is_none())
    }

    pub fn check_invariants(&self) {
        let mut next = 0;

        for (index, thread) in self.threads.iter().enumerate() {
            debug_assert!(
                index
                    .checked_sub(1)
                    .and_then(|previous| self.threads.get(previous))
                    .is_none_or(|previous| {
                        (previous.thread, previous.interpreter)
                            <= (thread.thread, thread.interpreter)
                    }),
                "thread {index} is out of order"
            );
            debug_assert_eq!(
                thread.frames.start, next,
                "thread {index}'s frames do not follow the previous thread's"
            );
            debug_assert!(
                thread.frames.start < thread.frames.end,
                "thread {index} has no frames"
            );
            debug_assert!(
                self.frames_of(thread)
                    .last()
                    .is_some_and(|frame| frame.outermost),
                "thread {index}'s outermost frame ends no activation's frames"
            );
            next = thread.frames.end;
        }

        debug_assert_eq!(
            next,
            self.frames.len(),
            "frames {next}.. belong to no thread"
        );
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct InterpretedLocal {
    pub name: Arc<str>,

    pub kind: crate::VariableKind,

    pub unnamed: Option<Arc<str>>,
    pub slot: InterpretedSlot,
}

#[derive(Debug, Clone, PartialEq)]
pub enum InterpretedSlot {
    Value(InterpretedValue),

    Number(InterpretedNumber),

    Unbound,

    Limit(crate::InspectionExhaustion),

    Unreadable(Arc<str>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ObjectTypes {
    pub native: &'static str,

    pub cast: &'static str,
}

#[must_use]
pub fn interpreted_identity(
    language: crate::SourceLanguage,
    module: Option<&str>,
    qualified: &str,
) -> crate::TypeIdentity {
    let mut path: Vec<Arc<str>> = module
        .into_iter()
        .flat_map(|module| module.split('.'))
        .chain(qualified.split('.'))
        .map(Arc::from)
        .collect();
    let base = path.pop().unwrap_or_else(|| Arc::from(qualified));

    crate::TypeIdentity {
        language,
        path: path.into(),
        inline_namespaces: Arc::from([]),
        base,
        arguments: Arc::from([]),
        pack: None,
        origin: crate::ArgumentOrigin::None,
        go: None,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct InterpretedHandle(u64);

impl InterpretedHandle {
    #[must_use]
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct InterpretedValue {
    pub object: InterpretedHandle,

    pub address: Option<VirtualAddress>,

    pub class: InterpretedHandle,

    pub module: Option<Arc<str>>,
    pub type_name: Arc<str>,
    pub kind: InterpretedKind,

    pub attributes: u64,

    pub summary: Arc<str>,

    pub detail: Option<Arc<str>>,

    pub problem: Option<Arc<str>>,

    pub part: Arc<[u64]>,

    pub array: Option<Arc<InterpretedArray>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InterpretedArray {
    pub data: VirtualAddress,
    pub shape: Arc<[u64]>,
    pub strides: Arc<[i64]>,
    pub element: ArrayElement,

    pub element_name: Arc<str>,
}

impl InterpretedArray {
    #[must_use]
    pub fn contiguous(&self) -> Option<(VirtualAddress, u64, crate::NumberKind)> {
        let (&[count], &[stride], ArrayElement::Number(kind)) =
            (&*self.shape, &*self.strides, self.element)
        else {
            return None;
        };

        let next = u64::try_from(stride).is_ok_and(|stride| stride == kind.size());
        (next || count <= 1).then_some((self.data, count, kind))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArrayElement {
    Bool,
    Number(crate::NumberKind),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InterpretedNumber {
    pub address: VirtualAddress,
    pub value: crate::ScalarValue,
    pub element: ArrayElement,
    pub module: Option<Arc<str>>,
    pub type_name: Arc<str>,

    pub summary: Arc<str>,
}

impl InterpretedValue {
    #[must_use]
    pub const fn elements(&self) -> u64 {
        match self.kind {
            InterpretedKind::Sequence { length } | InterpretedKind::Map { length } => length,
            _ => 0,
        }
    }

    #[must_use]
    pub const fn children(&self) -> u64 {
        self.elements().saturating_add(self.attributes)
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum InterpretedKind {
    Nothing,

    Undefined,
    Bool(bool),

    Int(Option<i128>),
    Float(f64),
    Text,
    Bytes,
    Sequence {
        length: u64,
    },
    Map {
        length: u64,
    },

    /// User-defined class instance without a built-in scalar or collection value.
    Instance,

    Other,
}

#[derive(Debug, Clone, PartialEq)]
pub enum InterpretedChild {
    Element(InterpretedSlot),
    Entry {
        key: InterpretedSlot,
        value: InterpretedSlot,
    },
    Attribute {
        name: Arc<str>,
        value: InterpretedSlot,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AssignedBinding {
    pub name: Arc<str>,
    pub reach: BindingReach,

    pub refusable: Option<Arc<str>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BindingReach {
    Frame,

    Enclosing,

    Script,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InterpretedWatch {
    pub address: VirtualAddress,
    pub byte_size: u64,

    pub code: VirtualAddress,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameReplacer {
    pub entry: ImageAddress,

    pub links: u8,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InterpretedPlace {
    Frame(VirtualAddress),

    Scope { depth: u32, offset: u64 },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InterpretedName {
    Variable(u32),

    Object(InterpretedHandle),
}

#[derive(Debug, Clone, Copy)]
pub enum InterpretedStep<'a> {
    Attribute(&'a str),

    Item(&'a InterpretedKey),
}

#[derive(Debug, Clone, PartialEq)]
pub enum InterpretedKey {
    Int(i128),
    Float(f64),
    Bool(bool),

    Text(Vec<u8>),

    Nothing,

    Tuple(Vec<Self>),

    Object(InterpretedHandle),
}

#[derive(Debug, Clone, PartialEq)]
pub struct InterpretedOperand {
    pub type_name: Arc<str>,
    pub kind: InterpretedKind,

    pub text: Option<crate::TextSummary>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InterpretedFailure {
    Refused(crate::eval::error::ErrorKind, Arc<str>),

    Unavailable(crate::VariableUnavailableReason),
}

impl InterpretedFailure {
    fn interprets_nothing() -> Self {
        Self::Refused(
            crate::eval::error::ErrorKind::Unsupported,
            "the runtime interprets no code".into(),
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Driven {
    Future { variable: &'static str },

    Interpreted,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskEnd {
    Finished,

    Cancelled,
}

pub fn detect(
    image: &Arc<dyn RuntimeImage + Send + Sync>,
) -> Vec<Result<Arc<dyn RuntimeModel>, Unbound>> {
    go::detect(Arc::clone(image))
        .into_iter()
        .chain(rust::detect(image).map(Ok))
        .chain(self::tokio::detect(image))
        .chain(python::detect(image))
        .chain(node::detect(image))
        .collect()
}

impl RuntimeImage for ModuleImage {
    fn producers(&self) -> Vec<&str> {
        Self::producers(self).collect()
    }

    fn python(&self) -> Option<crate::image::python::PythonRuntime> {
        self.python_runtime()
    }

    fn v8(&self) -> Option<crate::image::v8::V8Runtime> {
        self.v8_runtime()
    }

    fn constant(&self, name: &str) -> Option<IntegerValue> {
        Self::constant(self, name)
    }

    fn has_function(&self, name: &str) -> bool {
        self.functions_named(name).next().is_some() || self.symbol_named(name).is_ok()
    }

    fn function_answering(&self, name: &str) -> Option<ImageSymbol> {
        let mut found = self
            .symbols_answering(name)
            .filter(|symbol| symbol.kind() == crate::SymbolKind::Function);
        let symbol = found.next()?;
        found.next().is_none().then_some(ImageSymbol {
            address: symbol.address(),
            size: symbol
                .extent()
                .map(|extent| extent.range.end.get() - extent.range.start.get()),
        })
    }

    fn vtable_class(&self, address: ImageAddress) -> Option<(String, ImageAddress)> {
        Self::vtable_class(self, address)
    }

    fn symbol_at(&self, address: ImageAddress) -> Option<Arc<str>> {
        let symbol = self
            .symbolize(address)
            .filter(|symbol| symbol.offset == 0)?;
        Some(crate::demangle::demangle(&symbol.name).map_or(symbol.name, Arc::from))
    }

    fn symbol(&self, name: &str) -> Option<ImageSymbol> {
        let symbol = self.symbol_named(name).ok()?;
        let size = symbol
            .extent()
            .map(|extent| extent.range)
            .or_else(|| symbol.storage())
            .map(|range| range.end.get() - range.start.get());
        Some(ImageSymbol {
            address: symbol.address(),
            size,
        })
    }

    fn functions_starting(&self, prefix: &str) -> Vec<ImageSymbol> {
        self.symbols()
            .filter(|symbol| {
                symbol.kind() == crate::SymbolKind::Function && symbol.name().starts_with(prefix)
            })
            .filter_map(|symbol| {
                let range = symbol.extent()?.range;
                Some(ImageSymbol {
                    address: symbol.address(),
                    size: Some(range.end.get() - range.start.get()),
                })
            })
            .collect()
    }

    fn function_entries(&self, name: &str) -> Vec<ImageAddress> {
        self.functions_called(name)
            .flat_map(|function| self.instances_for_function(function.id()))
            .filter(|instance| matches!(instance.kind(), crate::CodeInstanceKind::OutOfLine))
            .filter_map(|instance| instance.ranges().map(|range| range.start).min())
            .collect()
    }

    fn function_body(&self, name: &str) -> Option<ImageAddress> {
        let entry = self.symbol_named(name).ok()?.address();
        let instance = self.locate(entry).physical_instance?;
        self.recommended_entries_for_instance(instance)
            .filter(|body| {
                matches!(
                    body.provenance,
                    EntryProvenance::AnalyzedPrologue | EntryProvenance::Statement
                ) && body.address > entry
            })
            .map(|body| body.address)
            .min()
    }

    fn member(&self, type_name: &str, path: &[&str]) -> Option<Member> {
        let mut ty = self.types_named_exactly(type_name).next()?;
        let mut offset = 0_u64;

        for name in path {
            let TypeKind::Record { members, .. } = &representation(self, ty)?.kind else {
                return None;
            };

            let member = members
                .iter()
                .find(|member| member.name.as_deref() == Some(*name))?;
            let RecordMemberLayout::ByteOffset(at) = member.layout else {
                return None;
            };

            offset = offset.checked_add(at)?;
            ty = self.type_info(member.type_ref)?;
        }

        Some(Member {
            offset,
            size: representation(self, ty)?.byte_size?,
        })
    }

    fn thread_local(&self, name: &str) -> Option<Result<ThreadLocal, Arc<str>>> {
        Self::thread_local(self, name)
    }

    fn thread_local_within(
        &self,
        scope: &str,
        name: &str,
    ) -> Option<Result<ThreadLocal, Arc<str>>> {
        Self::thread_local_within(self, scope, name)
    }

    fn types_named(&self, name: &str) -> Vec<TypeReference> {
        Self::types_named(self, name)
            .into_iter()
            .filter(|ty| {
                Self::type_info(self, *ty).is_some_and(|info| {
                    let mut qualified = info
                        .identity
                        .as_ref()
                        .map(|identity| identity.path.join("::"))
                        .unwrap_or_default();
                    if !qualified.is_empty() {
                        qualified.push_str("::");
                    }

                    qualified.push_str(&info.name);
                    qualified == name
                })
            })
            .collect()
    }

    fn type_info(&self, ty: TypeReference) -> Option<&TypeInfo> {
        Self::type_info(self, ty)
    }

    fn same_type(&self, left: TypeReference, right: TypeReference) -> bool {
        left == right || Self::same_type(self, left, right)
    }

    fn source_path_ending(&self, suffix: &str) -> Option<std::path::PathBuf> {
        self.source_files()
            .iter()
            .find(|file| file.path.ends_with(suffix))
            .map(|file| file.path.to_path_buf())
    }

    fn coroutine(&self, ty: TypeReference) -> Option<Result<&CoroutineInfo, &Arc<str>>> {
        Self::coroutine(self, ty.id)
    }

    fn trait_object_type(&self, address: ImageAddress) -> Option<TypeReference> {
        Self::trait_object_type(self, address)
    }

    fn coroutine_body(&self, ty: TypeReference) -> Option<ImageAddress> {
        if ty.image != self.id() {
            return None;
        }

        let mut starts = self
            .coroutine_functions(ty.id)
            .into_iter()
            .flat_map(|function| self.instances_for_function(function.id()))
            .filter(|instance| matches!(instance.kind(), crate::CodeInstanceKind::OutOfLine))
            .filter_map(|instance| instance.ranges().map(|range| range.start).min());
        let first = starts.next()?;

        starts.all(|start| start == first).then_some(first)
    }

    fn function_generics(&self, address: ImageAddress) -> Vec<(Arc<str>, String)> {
        let Some(function) = self
            .locate(address)
            .physical_instance
            .and_then(|instance| self.code_instance(instance))
            .and_then(|instance| self.function(instance.function()))
        else {
            return Vec::new();
        };

        function
            .generics()
            .filter_map(|(name, id)| {
                let ty = self.type_info(TypeReference {
                    image: self.id(),
                    id,
                })?;
                Some((Arc::from(name), ty.qualified_name()))
            })
            .collect()
    }

    fn function_name(&self, address: ImageAddress) -> Option<Arc<str>> {
        let location = self.locate(address);
        location
            .physical_instance
            .and_then(|instance| self.code_instance(instance))
            .and_then(|instance| self.function(instance.function()))
            .map(|function| Arc::from(function.name()))
            .or_else(|| self.symbolize(address).map(|symbol| symbol.name))
    }
}

pub fn representation<'a>(image: &'a ModuleImage, mut ty: &'a TypeInfo) -> Option<&'a TypeInfo> {
    for _ in 0..16 {
        match &ty.kind {
            TypeKind::Named { target, .. } => ty = image.type_info((*target)?)?,
            TypeKind::Modified { target, .. } => ty = image.type_info(*target)?,
            _ => return Some(ty),
        }
    }

    None
}

#[cfg(test)]
mod tests;
