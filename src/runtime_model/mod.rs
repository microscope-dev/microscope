//! What a language runtime tells the debugger at one stop: which tasks it
//! schedules, which task each thread runs, and where a parked task's frames
//! begin.
//!
//! Static facts about code, such as which functions switch stacks, belong
//! to the debug-info provider. Facts about one stop of one process belong
//! here. A runtime model is pure: it reaches a program only through
//! [`RuntimeImage`], the static facts of the module that carries the
//! runtime, and [`RuntimeStop`], one validated stop. A boundary test keeps
//! process control, debug-information parsing, I/O, clocks, and threads out
//! of it, and keeps every language's runtime in a module of its own.

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
    SCRIPT_DIRECTORY_PREFIX as PYTHON_SCRIPT_PREFIX,
    is_script_filename as is_python_script_filename, script_name as python_script_name,
};
mod records;
mod rust;

/// Whether a function breakpoint names a function a runtime runs itself,
/// as `py:Shape.area` or `js:handler` do, which no native symbol answers
/// to. `py::run` is a native function in a `py` namespace.
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

/// A result with the reasons it may be incomplete, such as a task whose
/// memory could not be read. A result with no gaps is complete.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Partial<T> {
    pub value: T,
    pub gaps: Vec<Arc<str>>,
}

/// Where a member lies within a named type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Member {
    pub offset: u64,
    pub size: u64,
}

/// A named object or function of an image.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ImageSymbol {
    pub address: ImageAddress,

    /// Its size in bytes, when the symbol table records one.
    pub size: Option<u64>,
}

/// The static facts of the module that carries a runtime, by name.
pub trait RuntimeImage: std::fmt::Debug {
    /// The producers of the module's debug information.
    fn producers(&self) -> Vec<&str>;

    /// The value of a named integer constant.
    fn constant(&self, name: &str) -> Option<IntegerValue>;

    /// A named object or function.
    fn symbol(&self, name: &str) -> Option<ImageSymbol>;

    /// Every function whose symbol's name begins with `prefix` and records
    /// its size.
    fn functions_starting(&self, prefix: &str) -> Vec<ImageSymbol> {
        let _ = prefix;
        Vec::new()
    }

    /// The one function whose symbol demangles to a name as people write
    /// it, such as `__rustc::rust_panic`, whose symbol carries a hash.
    fn function_answering(&self, name: &str) -> Option<ImageSymbol>;

    /// The demangled name of the symbol that begins at an address.
    fn symbol_at(&self, address: ImageAddress) -> Option<Arc<str>>;

    /// The C++ class whose vtable holds an address, and where the vtable
    /// begins, by the vtable's symbol.
    fn vtable_class(&self, address: ImageAddress) -> Option<(String, ImageAddress)> {
        let _ = address;
        None
    }

    /// Whether the image has a function of this name, by its debug
    /// information, its symbols, or a language's own function table.
    fn has_function(&self, name: &str) -> bool;

    /// Where the named function's body begins, past the prologue that sets
    /// up its frame, or `None` when that is not known.
    fn function_body(&self, name: &str) -> Option<ImageAddress>;

    /// Where each copy of the functions the debug information names `name`
    /// begins: a generic function may have one in each unit that uses it.
    fn function_entries(&self, name: &str) -> Vec<ImageAddress> {
        let _ = name;
        Vec::new()
    }

    /// Where the member reached through `path` lies within a named record,
    /// through nested records.
    fn member(&self, type_name: &str, path: &[&str]) -> Option<Member>;

    /// The name of the function whose code holds an image address.
    fn function_name(&self, address: ImageAddress) -> Option<Arc<str>>;

    /// The generic arguments of the function whose code holds an image
    /// address, each with its parameter's name, as the full names of their
    /// types, which the debug information gives: what a legacy-mangled
    /// symbol leaves out, naming its parameters only (`<T as Any>::type_id`).
    fn function_generics(&self, address: ImageAddress) -> Vec<(Arc<str>, String)> {
        let _ = address;
        Vec::new()
    }

    /// Where each thread's copy of the named thread-local variable is, or
    /// why that is unknown; `None` when the image defines none by the name.
    fn thread_local(&self, name: &str) -> Option<Result<ThreadLocal, Arc<str>>>;

    /// Where each thread's copy is of the one thread-local variable named
    /// `name` somewhere within `scope`, such as the storage std's
    /// `thread_local!` makes for a variable; `None` when there is none.
    fn thread_local_within(
        &self,
        scope: &str,
        name: &str,
    ) -> Option<Result<ThreadLocal, Arc<str>>> {
        let _ = (scope, name);
        None
    }

    /// The image's types of a qualified name, such as
    /// `tokio::runtime::task::core::Header`: one for each unit that
    /// describes the type.
    fn types_named(&self, name: &str) -> Vec<TypeReference> {
        let _ = name;
        Vec::new()
    }

    /// What one of the image's types is.
    fn type_info(&self, ty: TypeReference) -> Option<&TypeInfo> {
        let _ = ty;
        None
    }

    /// Whether two of the image's types are one type, described by two
    /// units.
    fn same_type(&self, left: TypeReference, right: TypeReference) -> bool {
        left == right
    }

    /// The path of a source file the image's code was compiled from that
    /// ends with `suffix`, such as `src/runtime/task/raw.rs`.
    fn source_path_ending(&self, suffix: &str) -> Option<std::path::PathBuf> {
        let _ = suffix;
        None
    }

    /// What the coroutine of type `ty` is, or why its layout cannot be read
    /// as one; `None` for a type that is no coroutine.
    fn coroutine(&self, ty: TypeReference) -> Option<Result<&CoroutineInfo, &Arc<str>>> {
        let _ = ty;
        None
    }

    /// The concrete type a trait object's vtable at `address` is for.
    fn trait_object_type(&self, address: ImageAddress) -> Option<TypeReference> {
        let _ = address;
        None
    }

    /// Where the one function that runs the coroutine of type `ty` begins,
    /// when its code is out of line and no other body runs it.
    fn coroutine_body(&self, ty: TypeReference) -> Option<ImageAddress> {
        let _ = ty;
        None
    }

    /// The Python runtime the image carries, as its loader read it.
    fn python(&self) -> Option<crate::image::python::PythonRuntime> {
        None
    }

    /// The V8 runtime the image carries, as its loader read it.
    fn v8(&self) -> Option<crate::image::v8::V8Runtime> {
        None
    }
}

/// A native thread identifier as the target observes it. This is distinct
/// from the debugger's `ThreadId`, as in a nested PID namespace.
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

/// A local variable of a frame, as the runtime's own debugging interface
/// reported it before the stop's barrier completed: a name, which memory
/// does not keep, and the value it held, which proves where it is.
#[derive(Debug, Clone, PartialEq)]
pub struct ReportedLocal {
    pub name: Arc<str>,
    pub value: ReportedValue,
}

/// What a runtime's debugging interface said a variable held.
#[derive(Debug, Clone, PartialEq)]
pub enum ReportedValue {
    Undefined,
    Null,
    Boolean(bool),
    Number(f64),
    String(Arc<str>),
    /// A big integer's decimal digits, with its sign.
    BigInt(Arc<str>),
    Symbol,
    /// An object, by the name of its class.
    Object(Arc<str>),
    /// A value the interface reported unavailable: as V8 reports a `let`
    /// before its initialization.
    Unavailable,
    /// A value the interface described in a way microscope does not read.
    Unknown,
}

/// The stack locals a runtime's debugging interface named for a frame at
/// this stop, or why it named none.
pub type ReportedLocals = Result<Arc<[ReportedLocal]>, Arc<str>>;

/// One validated stop of the process a runtime runs in.
pub trait RuntimeStop {
    /// The locals the runtime's debugging interface reported for the
    /// interpreted frame at `frame` before this stop's barrier completed,
    /// if it reported any for it.
    fn reported_locals(&self, _frame: VirtualAddress) -> Option<ReportedLocals> {
        None
    }

    /// Fills `bytes` from memory as the program sees it, without the
    /// debugger's breakpoints, or returns `false` when any byte is
    /// unreadable.
    fn read(&self, address: VirtualAddress, bytes: &mut [u8]) -> bool;

    /// The thread pointer (`fs_base` on x86-64) of a stopped thread.
    fn thread_pointer(&self, thread: ThreadId) -> Option<u64>;

    /// Where `thread`'s copy of a thread-local variable of the module
    /// carrying the runtime is, which the module's image places at
    /// `place`.
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

    /// Where `offset` lies in `thread`'s block of the thread-local storage
    /// of the module carrying the runtime, as the C library allocated it.
    fn module_thread_local(
        &self,
        _thread: ThreadId,
        _offset: u64,
    ) -> Result<VirtualAddress, Arc<str>> {
        Err("the module's block of thread-local storage cannot be found here".into())
    }

    /// The instruction a stopped thread will execute next.
    fn instruction(&self, thread: ThreadId) -> Option<VirtualAddress>;

    /// What the module carrying the runtime adds to its image addresses.
    fn load_bias(&self) -> crate::LoadBias;

    /// Every thread of the process, in the order of their ids.
    fn threads(&self) -> Vec<ThreadId>;

    /// The process's main thread, whose id is the process's, while it
    /// lives; `None` where the stop does not say.
    fn main_thread(&self) -> Option<ThreadId> {
        None
    }

    /// The id this live thread has in the target's namespace, if proven.
    /// Absence is unknown; it must never be inferred from `ThreadId`.
    fn target_thread_id(&self, _thread: ThreadId) -> Option<TargetThreadId> {
        None
    }

    /// Proven identities for a controlled thread catalog, read in a batch
    /// where the platform can share its namespace validation.
    fn target_thread_ids(&self, threads: &[ThreadId]) -> Vec<(ThreadId, TargetThreadId)> {
        threads
            .iter()
            .filter_map(|&thread| Some((thread, self.target_thread_id(thread)?)))
            .collect()
    }

    /// The sets of tasks the futures the stop's threads drive run, by
    /// where the runtime keeps them, as [`RuntimeModel::task_set`] named
    /// them: tasks that no runtime lists while their set waits.
    fn task_sets(&self) -> Vec<u64> {
        Vec::new()
    }

    /// The futex word a stopped thread waits on, the first argument of a
    /// `futex` system call it is blocked in, or `None` for a thread that is
    /// not in one or whose registers cannot be read.
    fn blocked_futex(&self, _thread: ThreadId) -> Option<u64> {
        None
    }

    /// The memory read at this stop so far, which lasts as long as the
    /// stop, where the debugger keeps it: none while the program runs, as
    /// between the stops it publishes.
    fn memory(&self) -> Option<&StopMemory> {
        None
    }
}

type ThreadIdPairs = Arc<[(ThreadId, TargetThreadId)]>;

/// Memory of a stopped process read so far, in aligned blocks of
/// [`StopMemory::BLOCK`] bytes, each as read or as unreadable. A stopped
/// process's memory does not change, so a read near one made before, as an
/// object's fields, a table's entries, and the objects allocated together
/// are, reads nothing more. Whoever keeps one forgets it once the program
/// runs or the debugger writes to it.
#[derive(Debug, Default)]
pub struct StopMemory {
    blocks: std::cell::RefCell<std::collections::BTreeMap<u64, Option<Box<[u8]>>>>,
    thread_ids: std::cell::RefCell<Option<ThreadIdPairs>>,
}

impl StopMemory {
    /// The size and alignment of a block. A page holds whole blocks, so a
    /// block is readable wherever any of its bytes are.
    pub const BLOCK: usize = 256;

    /// The most blocks kept, 4 MiB of them; past that, blocks are read as
    /// they are needed and not kept.
    const MAX_BLOCKS: usize = 1 << 14;

    /// Copies into `into` what the block that holds `at` holds from `at`
    /// on, as much as fits, reading the block with `read` if it was not
    /// read before: how many bytes it copied, or `None` where the block
    /// cannot be read. `read` fills a block from its start, saying whether
    /// it could, or fails as the caller's reads do.
    pub fn copy<E>(
        &self,
        at: u64,
        into: &mut [u8],
        read: impl FnOnce(VirtualAddress, &mut [u8]) -> Result<bool, E>,
    ) -> Result<Option<usize>, E> {
        let block = Self::BLOCK as u64;
        let start = at - at % block;
        let offset = usize::try_from(at - start).unwrap_or(Self::BLOCK);
        let copy = |held: &[u8], into: &mut [u8]| {
            let held = held.get(offset..)?;
            let copied = held.len().min(into.len());
            into[..copied].copy_from_slice(&held[..copied]);
            Some(copied)
        };

        if let Some(kept) = self.blocks.borrow().get(&start) {
            return Ok(kept.as_deref().and_then(|held| copy(held, into)));
        }

        let mut bytes = vec![0; Self::BLOCK].into_boxed_slice();
        let read = read(VirtualAddress::new(start), &mut bytes)?.then_some(bytes);
        let copied = read.as_deref().and_then(|held| copy(held, into));
        let mut blocks = self.blocks.borrow_mut();
        if blocks.len() < Self::MAX_BLOCKS {
            blocks.insert(start, read);
        }

        Ok(copied)
    }

    /// Identity evidence shares the stop's lifetime and invalidation: a
    /// core's TCB can be changed by a debugger memory write too. Only a
    /// complete match to the sorted distinct host catalog is retained.
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

        // Missing evidence can be a transient proc read failure. Keep a
        // complete catalog only; partial results can be retried at this stop.
        if matches(&ids) {
            *self.thread_ids.borrow_mut() = Some(Arc::clone(&ids));
        }
        ids
    }

    /// Forgets every block, once the memory may have changed.
    pub fn forget(&self) {
        self.blocks.borrow_mut().clear();
        *self.thread_ids.borrow_mut() = None;
    }
}

/// An address in a task's code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CodeAddress {
    pub address: VirtualAddress,

    /// Whether it is a return address, which names the call before it.
    pub after_call: bool,
}

/// A runtime function that starts tasks, and where the task it starts is
/// known: as it begins, from its arguments, or once it returns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TaskStarter {
    pub entry: ImageAddress,
    pub names_at_entry: bool,
}

/// A task a runtime's starter starts, and the coroutine it begins in, for
/// a runtime whose tasks are futures: where the coroutine's code is
/// inlined, and so has no entry, the task begins at the first of the
/// coroutine's statements it runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StartedTask {
    pub task: RuntimeTask,
    pub coroutine: Option<crate::TypeReference>,
}

/// One task of a runtime at a stop.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeTask {
    /// The runtime's number for the task.
    pub number: u64,

    /// Where the runtime keeps the task, by which a later request at the
    /// same stop may find it at once; see [`TaskRef`].
    pub locator: u64,
    pub state: TaskState,

    /// The runtime's own words for what the task waits for.
    pub detail: Option<Arc<str>>,

    /// The thread running the task, or the runtime's code for it, as while
    /// it makes a system call or is being parked.
    pub thread: Option<ThreadId>,

    /// Where the task will resume, for one that is not on a thread.
    pub resume: Option<CodeAddress>,

    /// The call that created the task.
    pub creation: Option<CodeAddress>,

    /// Where the program created the task, for a runtime that records the
    /// place by its source rather than by the call.
    pub spawned: Option<crate::RecordedPlace>,

    /// The function the task began in.
    pub entry: Option<VirtualAddress>,

    /// The task that created this one.
    pub parent: Option<u64>,

    /// Whether the runtime runs the task for its own work, such as a
    /// garbage collector's worker.
    pub internal: bool,

    /// The key-value labels the program or its runtime gave the task, in
    /// the runtime's order, such as Go's profiler labels, or the tokio
    /// runtime that holds it in a process with several.
    pub labels: TaskLabels,

    /// The function the runtime will call for the task, for one that runs
    /// no code until then, as an event loop's pending work does.
    pub callback: Option<TaskCallback>,

    /// Whether the task runs no code and keeps no frames: it is work the
    /// runtime will hand to `callback` once it is done, so it has no stack.
    pub stackless: bool,
}

/// A function a runtime will call for a task, and where the source it
/// recorded the function by begins it, if it is readable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskCallback {
    pub function: Arc<str>,
    pub place: Option<crate::RecordedPlace>,
}

/// A task the debugger asks a runtime about: its number, and the locator
/// the runtime gave it when it listed it at the same stop, if it did,
/// which spares a search. A locator that no longer holds the task, as
/// after the debugger wrote memory, is never trusted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TaskRef {
    pub number: u64,
    pub locator: Option<u64>,
}

/// A task's labels: keys and their values.
pub type TaskLabels = Vec<(Arc<str>, Arc<str>)>;

/// The runtime's tasks in one range of its own order, and where the next
/// range begins.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskPage {
    pub tasks: Vec<RuntimeTask>,

    /// Where the next page begins, or `None` after the last.
    pub next: Option<u64>,
}

/// What a stopped thread is doing for the runtime.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ThreadActivity {
    /// Running a task, or the runtime's code on its behalf.
    Task { number: u64, stack: StackSegment },

    /// Running the runtime's scheduler with no task, or code the runtime
    /// does not know, such as a thread C created.
    Idle,

    /// Running the program's own code on a thread the runtime schedules no
    /// task on.
    Outside,

    /// The runtime's state for the thread could not be read.
    Unknown(Arc<str>),
}

/// Where a task's frames begin.
#[derive(Debug, Clone)]
pub enum TaskContext {
    /// The task runs on a thread, whose registers begin its frames.
    OnThread(ThreadId),

    /// The task is parked with these registers saved; any other register is
    /// unknown.
    Saved {
        registers: RegisterFile,

        /// Whether the saved instruction is a return address, which names
        /// the call before it, as when a task parked by calling into its
        /// runtime.
        after_call: bool,
    },

    /// No thread runs the task: its frames are the chain of awaits that
    /// begins at its future, of type `ty`, at `future`.
    Suspended {
        future: VirtualAddress,
        ty: TypeReference,
    },

    /// No thread runs the task, whose frames are those its runtime's
    /// interpreter keeps, innermost first, as a suspended coroutine's and
    /// those of the coroutines it awaits; and why they end before the
    /// task's outermost, if they do.
    Interpreted {
        frames: Arc<[InterpretedFrame]>,
        end: Option<Arc<str>>,
    },
}

/// Where unwinding goes from a frame whose code switches stacks.
#[derive(Debug, Clone)]
pub enum Crossing {
    /// The frame is on the stack it was called on, and unwinds as any
    /// other frame does.
    Stay,

    /// The frame's own stack pointer is elsewhere than its registers say,
    /// as for a frame called on a task's stack that runs on the system
    /// stack. It unwinds as any other frame does, from these registers.
    Resume(RegisterFile),

    /// The frame never returns to its caller. The registers the task it
    /// switched from saved begin the next frame, whose instruction is a
    /// return address.
    Continue(RegisterFile),

    /// The frame is the first of its stack: no task's frames lie beyond it.
    Outermost,
}

/// What each runtime a model knows calls its tasks, singular and plural,
/// so that clients can speak of them as the runtime's users do.
pub const TASK_NOUNS: [(&str, &str); 3] = [go::TASK_NOUN, self::tokio::TASK_NOUN, node::TASK_NOUN];

/// The exceptions each runtime a model knows reports, which clients may
/// choose to stop at before they know which runtimes a program has.
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

/// How a runtime uses the process's signals, by Linux signal number.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RuntimeSignals {
    /// Signals the runtime tolerates arriving late. One that arrives while
    /// the debugger runs a thread alone for an instruction waits for the
    /// thread's next continue, rather than running a handler that may wait
    /// for threads the debugger holds.
    pub deferrable: &'static [i32],

    /// Signals the runtime handles as part of the program's own work, such
    /// as the faults Go turns into panics. By default they neither stop
    /// nor print, and are delivered; the runtime reports any it cannot
    /// handle as an exception of its own.
    pub handled: &'static [i32],
}

/// A runtime a module carries that its model cannot bind against the
/// module's debug information.
#[derive(Debug, Clone)]
pub struct Unbound {
    /// Why it cannot be bound, as when the debug information is stripped.
    pub reason: Arc<str>,

    /// The signals the runtime tolerates arriving late, as
    /// [`RuntimeSignals::deferrable`] says: how the runtime uses them holds
    /// without its data described, and one delivered while the debugger
    /// runs a thread alone may wait for threads it holds.
    pub deferrable: &'static [i32],
}

/// A runtime function that reports an exception as it is entered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RuntimeHook {
    /// The kind of exception it reports, which stops when its filter does.
    pub filter: &'static ExceptionFilter,
    pub address: ImageAddress,
}

/// What a runtime reports as one of its hooks is entered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeException {
    /// The runtime's message, as the runtime would print it.
    pub message: Arc<str>,

    /// An expression for the value the exception carries, when one can
    /// name it.
    pub value: Option<Arc<str>>,
}

/// Where one of a runtime's internal hooks is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum HookPlace {
    /// At this address in the runtime's own image.
    Own(ImageAddress),

    /// At the function exported under this name by a companion image: one
    /// the runtime loads beside its own, as a library its agent calls.
    Companion(&'static str),
}

/// A change to the stopped process's memory that a runtime model needs. A
/// model only reads, so it returns its writes for the debugger to make,
/// while every thread is stopped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeWrite {
    pub address: VirtualAddress,

    /// What the memory held as the model read it: the write is made only
    /// while it still does, so that it never applies to state the model
    /// did not see.
    pub expected: Vec<u8>,

    /// What the memory is to hold, as many bytes as `expected`.
    pub bytes: Vec<u8>,
}

/// What a thread that entered one of a runtime's internal hooks reports.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HookEvent {
    /// A safe entry point at which installation can be retried.
    AgentEntry,
    /// A configuration failed after its source was consumed. The owner
    /// may retire that generation's artifacts, but cannot claim it armed.
    AgentFailed {
        generation: u64,
        description: Arc<str>,
    },

    /// The agent applied this generation of its whole desired state. The
    /// injection owner may now retire that generation's script (P4.7).
    Armed { generation: u64 },

    /// Nothing the user sees: the thread goes on.
    Continue,

    /// An exception, which stops when its filter does.
    Exception {
        filter: &'static ExceptionFilter,
        report: RuntimeException,
    },

    /// Hits of the managed breakpoint locations the runtime was armed with
    /// under these numbers, at one event of one thread, which stop once.
    Breakpoint { numbers: Vec<u64> },

    /// The end of the step the runtime was armed with under this number,
    /// as the runtime judges it: the thread reached where the step stops.
    StepEnded { number: u64 },

    /// The step the runtime was armed with under this number stopped where
    /// the runtime could no longer follow it, for this reason.
    StepIncomplete { number: u64, description: Arc<str> },

    /// The step into the runtime was armed with under this number is about
    /// to call native code at `target`, where it ends if the code is the
    /// program's own and described.
    StepCalls { number: u64, target: VirtualAddress },

    /// The assignment the runtime was armed with under this number is
    /// made, or, with why, could not be.
    Assigned {
        number: u64,
        failure: Option<Arc<str>>,
    },
}

/// Everything a runtime's internal hooks are to report.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Arming<'a> {
    /// The managed breakpoint locations, each by its number.
    pub breakpoints: &'a [ManagedArm],

    /// The step the runtime arms and judges, if one is active.
    pub step: Option<&'a RuntimeStep>,

    /// The assignment the runtime's agent is to make, if one is asked.
    pub assignment: Option<&'a RuntimeAssignment>,

    /// Which exceptions stop.
    pub exceptions: crate::protocol::ExceptionStops,
}

/// An assignment a runtime's agent makes to a variable of an interpreted
/// frame, on the frame's thread, as Python's stores through the frame's
/// locals.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeAssignment {
    /// The number the runtime reports the assignment by.
    pub number: u64,

    /// The thread whose interpreted frame at `frame` holds the variable,
    /// by its slot among the frame's.
    pub thread: ThreadId,
    pub frame: VirtualAddress,
    pub slot: u32,
    pub value: AssignedValue,
}

/// What a variable of an interpreted frame is given: a value the runtime
/// makes, or one of its objects, which the agent takes only where a name
/// of the frame still finds it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AssignedValue {
    /// An integer, in decimal.
    Integer(Arc<str>),

    /// A binary64 float, by its bits.
    Float(u64),
    Bool(bool),
    Text(Arc<str>),
    Nothing,
    Object(InterpretedHandle),
}

/// A step a runtime arms and judges itself, as Python's agent steps by the
/// lines of Python code, which have no address.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeStep {
    /// The number the runtime reports the step's end by.
    pub number: u64,
    pub kind: crate::StepKind,

    /// The thread the step runs on.
    pub thread: ThreadId,

    /// The interpreted frame the step begins in; `None` for one a runtime
    /// begins where it next pauses the thread, as V8 does from a native
    /// stop.
    pub frame: Option<VirtualAddress>,

    /// Whether the thread is in native code the frame called, from which a
    /// native step returned to the runtime: the step ends at the thread's
    /// next line in the frame or a caller.
    pub returning: bool,
}

/// A managed breakpoint location a runtime is armed with: where it stops,
/// and the number it reports a hit of it by.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManagedArm {
    pub number: u64,
    pub place: crate::ManagedPlace,
}

/// Where a value whose dynamic type a runtime records is stored.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StoredValue<'a> {
    /// In the program's memory.
    Memory(VirtualAddress),

    /// In these bytes, captured from registers or memory.
    Bytes(&'a [u8]),
}

/// What a value of a type whose dynamic type the runtime records holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DynamicValue {
    /// Nothing, as a nil interface holds.
    Nil,

    /// A value of the type the runtime describes at `descriptor`.
    Held {
        descriptor: VirtualAddress,

        /// The descriptor's offset in the type table of the module holding
        /// it, which that module's debug information names its types by.
        offset: u64,
        place: HeldPlace,
    },
}

/// Where a held value is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HeldPlace {
    /// In the program's memory.
    Memory(VirtualAddress),

    /// In the holding value's own bytes, this far in.
    Within(u64),
}

/// What a language runtime tells the debugger at a stop.
pub trait RuntimeModel: Send + Sync + std::fmt::Debug {
    /// The runtime's tasks from `start`, an index into its own order: at
    /// most `limit` of them, and only those that run the program's code
    /// when `program_only`. A page's work is bounded, so it may hold fewer
    /// and still have a next.
    fn tasks(
        &self,
        stop: &dyn RuntimeStop,
        start: u64,
        limit: usize,
        program_only: bool,
    ) -> Partial<TaskPage>;

    /// What a stopped thread is doing for the runtime.
    fn thread_activity(&self, stop: &dyn RuntimeStop, thread: ThreadId) -> ThreadActivity;

    /// The task a stopped thread runs, as the thread's own state names it,
    /// read without the runtime's shared state, which threads still running
    /// may be changing while run control decides, at a breakpoint's hit,
    /// whether the thread runs a step's or a condition's task. It may name
    /// a task the runtime runs for its own work, which the thread's
    /// activity leaves out.
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

    /// Where the frames of a task begin, or `None` when the runtime has no
    /// such task.
    fn task_context(
        &self,
        stop: &dyn RuntimeStop,
        task: TaskRef,
    ) -> Result<Option<TaskContext>, Arc<str>>;

    /// The stacks a stopped thread may run on for the runtime, each with
    /// whose it is: the bounds of the stack of the task it runs, and of the
    /// runtime's own stacks for it.
    fn thread_stacks(
        &self,
        stop: &dyn RuntimeStop,
        thread: ThreadId,
    ) -> Result<Vec<(std::ops::Range<u64>, StackSegment)>, Arc<str>>;

    /// Where unwinding goes from a frame of a stopped thread whose code
    /// switches stacks, given the frame's registers. `after_call` says the
    /// frame's instruction is a return address, as every frame's is but
    /// the innermost and one a signal interrupted: the frame is in a call.
    fn cross(
        &self,
        stop: &dyn RuntimeStop,
        thread: ThreadId,
        frame: &RegisterFile,
        after_call: bool,
    ) -> Result<Crossing, Arc<str>>;

    /// How the runtime uses signals.
    fn signals(&self) -> RuntimeSignals;

    /// The functions that report the runtime's exceptions as they are
    /// entered.
    fn hooks(&self) -> &[RuntimeHook];

    /// What a stopped thread reports as it enters the hook at `hook`,
    /// given the thread's registers there.
    fn exception(
        &self,
        stop: &dyn RuntimeStop,
        hook: ImageAddress,
        registers: &RegisterFile,
    ) -> Result<RuntimeException, Arc<str>>;

    /// What the value stored at `value` dynamically holds, when the record
    /// its type is represented by, named `representation`, is one the
    /// runtime records a dynamic type in; `None` for any other record.
    fn dynamic_value(
        &self,
        stop: &dyn RuntimeStop,
        representation: &str,
        value: StoredValue<'_>,
    ) -> Option<Result<DynamicValue, Arc<str>>>;

    /// The runtime's internal hooks: functions it calls to tell the
    /// debugger of its own events, each armed for as long as the runtime
    /// is loaded, whatever stops. At each hit, [`Self::hook_event`] says
    /// whether the thread stops.
    fn agent(&self) -> Option<&dyn RuntimeAgent> {
        None
    }

    fn internal_hooks(&self) -> &[HookPlace] {
        &[]
    }

    /// What a stopped thread entering the internal hook `hook` reports,
    /// given the thread's registers there.
    fn hook_event(
        &self,
        stop: &dyn RuntimeStop,
        hook: HookPlace,
        registers: &RegisterFile,
    ) -> Result<HookEvent, Arc<str>> {
        let _ = (stop, hook, registers);
        Err("the runtime has no internal hooks".into())
    }

    /// Where a breakpoint `spec` stops in the code the runtime runs
    /// itself, read through `stop`, as one of the debugger's resolvers:
    /// `None` when the spec names nothing the runtime runs, so that other
    /// resolvers decide, or why the runtime cannot stop there.
    fn resolve_breakpoint(
        &self,
        stop: &dyn RuntimeStop,
        spec: &crate::BreakpointSpec,
    ) -> Option<Result<Vec<crate::ManagedPlace>, Arc<str>>> {
        let _ = (stop, spec);
        None
    }

    /// The writes that arm the runtime with exactly `arming`, everything
    /// the debugger wants its internal hooks to report, so that they
    /// report that and nothing else; or why it cannot be armed so. Each
    /// arming is whole, so a model keeps no state between them.
    fn arm(
        &self,
        stop: &dyn RuntimeStop,
        arming: &Arming<'_>,
    ) -> Result<Vec<RuntimeWrite>, Arc<str>> {
        let _ = (stop, arming);
        Err("the runtime reports nothing of its own".into())
    }

    /// The runtime function that moves a task's stack to another, which
    /// the move has finished once it returns.
    fn stack_mover(&self) -> Option<ImageAddress>;

    /// The function the runtime's inspector calls to list its targets,
    /// where the debugger reads the inspector's address and hides it.
    fn inspector_hook(&self) -> Option<ImageAddress> {
        None
    }

    /// A hidden listing's immutable target, or no edit for native upgrade
    /// validation. An unrecognized caller is refused rather than exposed.
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

    fn inspector_endpoint(
        &self,
        _stop: &dyn RuntimeStop,
    ) -> Result<Option<(std::net::SocketAddr, Arc<str>)>, Arc<str>> {
        Ok(None)
    }

    /// The number of the task whose stack a stopped thread entering the
    /// stack mover, given its registers there, is moving.
    fn moving_task(
        &self,
        stop: &dyn RuntimeStop,
        registers: &RegisterFile,
    ) -> Result<u64, Arc<str>>;

    /// The bounds of a task's stack, or `None` when the runtime has no such
    /// task.
    fn task_stack(
        &self,
        stop: &dyn RuntimeStop,
        task: TaskRef,
    ) -> Result<Option<std::ops::Range<u64>>, Arc<str>>;

    /// The program's code that the runtime function at `entry` goes on to
    /// call for the program, as Go's calls between Go and C do, given the
    /// registers of a stopped thread entering it; `None` when it calls
    /// none.
    fn call_out(
        &self,
        entry: ImageAddress,
        registers: &RegisterFile,
    ) -> Option<Result<VirtualAddress, Arc<str>>>;

    /// The runtime functions that start tasks.
    fn task_starters(&self) -> Vec<TaskStarter>;

    /// The task a stopped thread starts in the starter at `starter`, given
    /// its registers where the starter names the task: at its entry or at
    /// its return address. `None` when this call starts no task, as one
    /// that also schedules a woken task may not.
    fn started_task(
        &self,
        stop: &dyn RuntimeStop,
        starter: ImageAddress,
        registers: &RegisterFile,
    ) -> Result<Option<StartedTask>, Arc<str>>;

    /// What the runtime calls one of its tasks.
    fn task_noun(&self) -> &'static str;

    /// Where the runtime keeps the set of tasks that the future at
    /// `future`, of type `ty`, runs when a thread drives it, as a set of
    /// tasks no runtime lists runs while its future is polled; `None` for
    /// a future that runs no such set.
    fn task_set(
        &self,
        stop: &dyn RuntimeStop,
        future: VirtualAddress,
        ty: crate::TypeReference,
    ) -> Option<u64> {
        let _ = (stop, future, ty);
        None
    }

    /// Whether a future of type `ty`, which a thread drives where its
    /// value cannot be read, may run a set of tasks that only
    /// [`Self::task_set`] finds.
    fn may_run_task_set(&self, ty: crate::TypeReference) -> bool {
        let _ = ty;
        false
    }

    /// The role of the runtime's frames that drive frames the native stack
    /// does not show, as a scheduler's `block_on` drives a future no task
    /// holds. A backtrace asks [`Self::driven`] only of frames in this role.
    fn driver_role(&self) -> Option<CodeRole> {
        None
    }

    /// What `frame`, one of the runtime's frames in its
    /// [driver role](Self::driver_role), drives, given the frame `inner`
    /// to it that it called; `None` when it drives nothing.
    fn driven(&self, frame: &StackFrame, inner: Option<&StackFrame>) -> Option<Driven> {
        let _ = (frame, inner);
        None
    }

    /// The variable of `frame`, one of the runtime's frames in its
    /// [driver role](Self::driver_role), that holds the future of a task it
    /// is polling at this moment, which the frames inner to it run; `None`
    /// when it polls none.
    ///
    /// Unlike [`Self::driven`], the future runs on the stack, so the
    /// backtrace shows it already. This lets a frame of a coroutine find
    /// the future it runs when the debug information does not say, as no
    /// Rust before 1.93's does: the chain of awaits from the task's future
    /// down to it. It reads only the polling thread's stack, never the
    /// runtime's shared state, so run control may ask it while other
    /// threads run.
    fn polling(&self, frame: &StackFrame) -> Option<&'static str> {
        let _ = frame;
        None
    }

    /// Whether a native frame that runs the function or symbol `name` is
    /// an activation of the runtime's interpreter's loop, given what the
    /// frame it called runs, `inner`: the frame each segment of
    /// [`Self::interpreted`]'s frames goes before.
    fn activation(&self, name: &str, inner: Option<&str>) -> bool {
        let _ = (name, inner);
        false
    }

    /// The frames the runtime's interpreter runs on the stop's threads,
    /// and why some threads' may be missing; `None` for a runtime that
    /// interprets nothing.
    fn interpreted(&self, stop: &dyn RuntimeStop) -> Option<Partial<InterpretedStacks>> {
        let _ = stop;
        None
    }

    /// The labels the runtime gives each thread of the stop that it has
    /// any for, as `key` and `value`, such as whether the thread holds a
    /// lock the runtime's threads take turns at, and why some threads' may
    /// be missing. A profile's samples of the thread carry them.
    fn thread_labels(&self, stop: &dyn RuntimeStop) -> Partial<Vec<ThreadLabels>> {
        let _ = stop;
        Partial {
            value: Vec::new(),
            gaps: Vec::new(),
        }
    }

    /// What the stopped thread `thread` works for, of the things of a kind
    /// the runtime keeps several of, such as the V8 isolate a Node thread
    /// runs JavaScript in or a V8 helper thread compiles for; `None` when
    /// it works for none. The debugger numbers each identity of a kind in
    /// the order it first sees it, and keeps the number while a thread it
    /// saw holding it lives, so the thread list shows `isolate 1` rather
    /// than an address.
    fn thread_holding(&self, stop: &dyn RuntimeStop, thread: ThreadId) -> Option<ThreadHolding> {
        let _ = (stop, thread);
        None
    }

    /// Where a walker in the kernel begins each of the interpreter's
    /// threads, read through `stop`, and why some may be missing; `None`
    /// for a runtime no such walker reads (see `backend::bpf::sampler`).
    fn walked_threads(&self, stop: &dyn RuntimeStop) -> Option<Partial<Vec<WalkedThread>>> {
        let _ = stop;
        None
    }

    /// The frames a walker in the kernel read of one thread at a sample,
    /// innermost first, as `frames` holds them, named through `stop`: as
    /// [`InterpretedStacks`] holds a thread's, each activation's outermost
    /// marked, and why they end before the thread's outermost, if they do.
    /// A frame whose code object is gone, or is no longer the one the
    /// walker read, is unreadable.
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

    /// The local variables of the interpreted frame the interpreter keeps
    /// at `frame`, as [`RuntimeModel::interpreted`] named it at this stop,
    /// in the order the code declares them. Each read is charged to
    /// `budget`.
    fn interpreted_locals(
        &self,
        stop: &dyn RuntimeStop,
        frame: InterpretedFrameRef,
        budget: &mut InspectionBudget,
    ) -> Result<Vec<InterpretedLocal>, Arc<str>> {
        let _ = (stop, frame, budget);
        Err("the runtime interprets no frames".into())
    }

    /// The children `range` of the interpreter's object `object`, or of
    /// its part at `part` ([`InterpretedValue::part`]), which
    /// [`InterpretedValue::children`] counted at this stop: a sequence's
    /// elements, or a map's entries, in order.
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

    /// What `name` means in the interpreted frame the interpreter keeps at
    /// `frame`, as an expression binds it: one of the frame's variables,
    /// or the global or built-in it finds; `None` when it means nothing
    /// there.
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

    /// The object the variable in slot `slot` of the interpreted frame at
    /// `frame` holds, which [`Self::interpreted_name`] named.
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

    /// Where the variable in slot `slot` of the interpreted frame at
    /// `frame` keeps the object it holds, to watch, and the code the frame
    /// runs; or why the variable cannot be watched so.
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

    /// Whether `thread` still runs the interpreted frame at `frame`, which
    /// runs `code`, as when its variable was watched.
    fn interpreted_frame_runs(
        &self,
        stop: &dyn RuntimeStop,
        thread: ThreadId,
        frame: VirtualAddress,
        code: VirtualAddress,
    ) -> Result<bool, Arc<str>> {
        let _ = (stop, thread, frame, code);
        Err("the runtime interprets no frames".into())
    }

    /// Whether `thread` is the only thread the runtime knows, which alone
    /// may run its agent's code while every other thread is stopped.
    fn sole_thread(&self, stop: &dyn RuntimeStop, thread: ThreadId) -> Result<bool, Arc<str>> {
        let _ = (stop, thread);
        Err("the runtime cannot say which threads run it".into())
    }

    /// The object a watched variable whose word is `bits` holds, none when
    /// it holds none.
    fn interpreted_watched(&self, bits: u64) -> Result<Option<InterpretedHandle>, Arc<str>> {
        let _ = bits;
        Err("the runtime interprets no frames".into())
    }

    /// The object a step from the interpreter's object `object` reaches.
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

    /// The interpreter's object `object`, as an operation computes with
    /// it.
    fn interpreted_operand(
        &self,
        stop: &dyn RuntimeStop,
        object: InterpretedHandle,
        budget: &mut InspectionBudget,
    ) -> Result<InterpretedOperand, InterpretedFailure> {
        let _ = (stop, object, budget);
        Err(InterpretedFailure::interprets_nothing())
    }

    /// The length of the interpreter's object `object`, as its language
    /// measures it.
    fn interpreted_length(
        &self,
        stop: &dyn RuntimeStop,
        object: InterpretedHandle,
        budget: &mut InspectionBudget,
    ) -> Result<u64, InterpretedFailure> {
        let _ = (stop, object, budget);
        Err(InterpretedFailure::interprets_nothing())
    }

    /// The language whose values the runtime interprets, which names their
    /// types. Only a runtime that interprets values is asked.
    fn interpreted_language(&self) -> crate::SourceLanguage {
        crate::SourceLanguage::Python
    }

    /// The names native code gives the interpreter's objects: the type a
    /// pointer to one points to, as C's `PyObject`, and the dynamic type an
    /// expression casts such a pointer to, to reach into the object. A
    /// pointer of that type is presented as the object it points to.
    fn object_types(&self) -> Option<ObjectTypes> {
        None
    }

    /// Where the value of `handle` lives in memory, if it does.
    fn interpreted_address(&self, handle: InterpretedHandle) -> Option<VirtualAddress> {
        let _ = handle;
        None
    }

    /// The object that a native pointer to [`ObjectTypes::native`] with value
    /// `address` points to, as a cast to [`ObjectTypes::cast`] reaches it.
    fn interpreted_handle(
        &self,
        stop: &dyn RuntimeStop,
        address: VirtualAddress,
        budget: &mut InspectionBudget,
    ) -> Result<InterpretedHandle, InterpretedFailure> {
        let _ = (stop, address, budget);
        Err(InterpretedFailure::interprets_nothing())
    }

    /// Whether the code at `address` of the image the runtime was bound to
    /// is the runtime's own, which it compiled rather than the image's
    /// compiler, as V8's builtins: [`Self::code_frame`] describes its
    /// frames. A copy of the image's code binds the same image.
    fn owns_code(&self, address: ImageAddress) -> bool {
        let _ = address;
        false
    }

    /// The code the runtime owns as the process says at `stop`, in the
    /// process's addresses, where the image cannot say: V8's builtins in a
    /// module stripped of the symbols that bound them, whose bounds V8
    /// stores when it starts. The code keeps the runtime's frame-pointer
    /// convention, by which the unwinder walks it where no call-frame
    /// information describes it, and [`Self::code_frame`] describes its
    /// frames as it does those of [`Self::owns_code`]. Empty where
    /// [`Self::owns_code`] already bounds the code, before the runtime has
    /// stored the bounds, and where they cannot be read.
    fn process_code(&self, stop: &dyn RuntimeStop) -> Vec<AddressRange<VirtualAddress>> {
        let _ = stop;
        Vec::new()
    }

    /// Whether the code no image holds that the runtime generated, which
    /// the debugger keeps as modules of their own, is the runtime's to
    /// describe.
    fn owns_generated_code(&self) -> bool {
        false
    }

    /// What the runtime recorded of a physical frame whose code is its own
    /// ([`Self::owns_code`], [`Self::owns_generated_code`]): the functions
    /// it runs, and its role; `None` where the frame is none the runtime
    /// describes, which keeps the name its code's symbol gives it.
    fn code_frame(&self, stop: &dyn RuntimeStop, frame: &CodeFrame<'_>) -> Option<DescribedFrame> {
        let _ = (stop, frame);
        None
    }

    /// The identities of the class `class` and of each class it derives
    /// from, in the order the language looks attributes up in, which
    /// choose the view that presents its objects.
    fn interpreted_lineage(
        &self,
        stop: &dyn RuntimeStop,
        class: InterpretedHandle,
        budget: &mut InspectionBudget,
    ) -> Result<Vec<crate::TypeIdentity>, InterpretedFailure> {
        let _ = (stop, class, budget);
        Err(InterpretedFailure::interprets_nothing())
    }

    /// The interpreter's object `object`, presented as a variable's value
    /// is.
    fn interpreted_value(
        &self,
        stop: &dyn RuntimeStop,
        object: InterpretedHandle,
        budget: &mut InspectionBudget,
    ) -> InterpretedSlot {
        let _ = (stop, object, budget);
        InterpretedSlot::Unreadable("the runtime interprets no objects".into())
    }

    /// Whether code from the source file at `path` is the runtime's own
    /// library, which raises its exceptions on the program's behalf: an
    /// exception blames the program's frame that called it.
    fn own_source(&self, path: &std::path::Path) -> bool {
        let _ = path;
        false
    }

    /// The runtime's code that takes up the task `task` again, which a
    /// step that waits for the task watches to see it end; `None` when
    /// the runtime names no such code.
    fn task_entries(
        &self,
        stop: &dyn RuntimeStop,
        task: TaskRef,
    ) -> Result<Option<TaskEntries>, Arc<str>> {
        let _ = (stop, task);
        Ok(None)
    }

    /// Whether a stopped thread entering `entry`, one of `task`'s entries,
    /// given its registers there, takes up that task rather than another.
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

    /// How the task `task` ended, or is ending: cancelled once its runtime
    /// has begun to cancel it, finished once its future returned; `None`
    /// while it runs on. Fails when the place the task was is no longer
    /// the task's.
    fn task_end(
        &self,
        stop: &dyn RuntimeStop,
        task: &TaskEntries,
    ) -> Result<Option<TaskEnd>, Arc<str>> {
        let _ = (stop, task);
        Ok(None)
    }
}

/// The runtime's code that takes up one task again.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskEntries {
    /// The task, as the runtime locates it.
    pub task: TaskRef,

    /// Where the functions that run the task, such as polling it or
    /// shutting it down, begin: the task may end before they return.
    pub runs: Vec<VirtualAddress>,

    /// Where each copy of the function that frees the task begins, once it
    /// has ended.
    pub frees: Vec<VirtualAddress>,
}

/// The frames an interpreter runs on the threads of one stop, which no
/// native unwind finds: each thread's, innermost first, with the outermost
/// of those each activation of the interpreter's loop runs marked.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct InterpretedStacks {
    /// Each thread that runs interpreted frames, in the order of their ids
    /// and then of their interpreters. A thread may have several states in
    /// one interpreter, as CPython's may.
    pub threads: Vec<InterpretedThread>,

    /// Every thread's frames, each thread's together and innermost first.
    pub frames: Vec<InterpretedFrame>,
}

/// Where a walker in the kernel begins one of an interpreter's threads:
/// the state it keeps for the thread, which names the thread by its id in
/// its own pid namespace.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WalkedThread {
    pub native: u64,
    pub state: VirtualAddress,
}

/// One interpreted frame as a walker in the kernel read it at a sample:
/// the code object it ran, and what of the object proves it is that one
/// when it is read again; where its instruction is in its bytecode; and
/// its owner, as the runtime numbers owners.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SampledFrame {
    pub code: VirtualAddress,
    pub proof: SampledCode,

    /// The instruction's byte offset from the bytecode's start, which may
    /// be just before it; `None` where it could not be read.
    pub offset: Option<i64>,
    pub owner: u32,
}

/// What a walker in the kernel read of a frame's code object, which proves
/// it is the same object when it is read again: an object put in a freed
/// one's place has a line table of its own, though its name and first line
/// are the same.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SampledCode {
    pub qualname: u64,
    pub linetable: u64,
    pub first_line: i32,
}

/// A physical frame whose code a runtime owns, as the unwinder found it.
#[derive(Debug, Clone, Copy)]
pub struct CodeFrame<'a> {
    /// The thread whose stack the frame is on.
    pub thread: ThreadId,

    /// The frame's instruction: the return address of its call where
    /// [`Self::after_call`].
    pub instruction: VirtualAddress,

    /// Whether its instruction is the return address of a call it made,
    /// as every caller's is.
    pub after_call: bool,

    /// Whether the frame's code is code the runtime generated, which no
    /// image holds, rather than the image's.
    pub generated: bool,

    /// The registers the unwinder recovered for the frame, as its frame
    /// pointer.
    pub registers: &'a RegisterFile,
}

/// What a runtime recorded of a physical frame whose code is its own.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DescribedFrame {
    /// The functions the frame runs, innermost first: those its code
    /// inlined, then its own. None where the frame is the runtime's
    /// machinery, which its code's symbol names.
    pub functions: Vec<crate::RecordedFrame>,

    /// What the frame's code is to stepping and backtraces.
    pub role: CodeRole,

    /// Whether the interpreter's registers in the frame hold its variables.
    /// V8's optimized code keeps the interpreter's frame but not its values.
    pub live_registers: bool,
}

/// An interpreted frame: where the interpreter keeps it, and whether its
/// registers there hold the frame's variables.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct InterpretedFrameRef {
    pub address: VirtualAddress,
    pub live_registers: bool,
}

/// The labels a runtime gives one thread.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThreadLabels {
    pub thread: ThreadId,
    pub labels: Vec<(Arc<str>, Arc<str>)>,
}

/// One of several things of a kind a runtime keeps, such as V8's isolates,
/// that a thread runs in at a stop: what the runtime calls one, and which
/// it is, or why that could not be read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThreadHolding {
    pub kind: &'static str,
    pub identity: Result<HeldIdentity, Arc<str>>,
}

/// Which one of its kind a thread runs in: an opaque value, such as an
/// isolate's address, that no two of the kind share while either lives.
/// Another may have it once the one that did is gone.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct HeldIdentity(u64);

impl HeldIdentity {
    pub const fn new(value: u64) -> Self {
        Self(value)
    }
}

/// One thread's interpreted frames, in one interpreter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InterpretedThread {
    pub thread: ThreadId,

    /// The interpreter the frames are in, by the runtime's number for it,
    /// for a runtime that runs several in one process.
    pub interpreter: i64,

    /// Where its frames are in [`InterpretedStacks::frames`].
    pub frames: std::ops::Range<usize>,

    /// Why its frames end before the thread's outermost, if they do.
    pub end: Option<Arc<str>>,
}

/// A frame an interpreter runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InterpretedFrame {
    /// Where the interpreter keeps the frame.
    pub frame: VirtualAddress,

    /// The language of the code it runs.
    pub language: SourceLanguage,

    /// The function it runs, and where in its source, as the interpreter
    /// records them; none where they cannot be read, which `problem` says.
    pub function: Option<Arc<str>>,
    pub place: Option<RecordedPlace>,
    pub problem: Option<Arc<str>>,

    /// The line of the place's file its function's code begins at, which
    /// tells it from another function of its name in that file.
    pub first_line: Option<u32>,

    /// Whether it is the outermost of the frames one activation of the
    /// interpreter's loop runs.
    pub outermost: bool,

    /// The part its code plays: the program's own, or the debugger's agent,
    /// which a backtrace hides as the interpreter's machinery.
    pub role: crate::CodeRole,
}

impl InterpretedStacks {
    /// The frames of one of [`Self::threads`].
    #[must_use]
    pub fn frames_of(&self, thread: &InterpretedThread) -> &[InterpretedFrame] {
        self.frames.get(thread.frames.clone()).unwrap_or_default()
    }

    /// Panics unless the threads are in order, their frames lie within
    /// [`Self::frames`] in order with none between them, and each thread's
    /// outermost frame ends an activation's.
    pub fn check_invariants(&self) {
        let mut next = 0;

        for (index, thread) in self.threads.iter().enumerate() {
            debug_assert!(
                index == 0
                    || (
                        self.threads[index - 1].thread,
                        self.threads[index - 1].interpreter
                    ) <= (thread.thread, thread.interpreter),
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

/// A local variable of an interpreted frame.
#[derive(Debug, Clone, PartialEq)]
pub struct InterpretedLocal {
    pub name: Arc<str>,

    /// Which of the frame's scopes it is in: a parameter, a local, a
    /// captured variable, a module's binding, the global object, or an
    /// unnamed value.
    pub kind: crate::VariableKind,

    /// For an unnamed value, why it has no name.
    pub unnamed: Option<Arc<str>>,
    pub slot: InterpretedSlot,
}

/// What a variable or a container's element holds.
#[derive(Debug, Clone, PartialEq)]
pub enum InterpretedSlot {
    Value(InterpretedValue),

    /// A number an object keeps as the machine does, outside any object of
    /// its own, as an element of a numpy array is.
    Number(InterpretedNumber),

    /// Nothing: the variable is not yet assigned, or was deleted.
    Unbound,

    /// The inspection's limits were reached before it was read.
    Limit(crate::InspectionExhaustion),

    /// It cannot be read, and why.
    Unreadable(Arc<str>),
}

/// The names native code gives an interpreter's objects.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ObjectTypes {
    /// The type a pointer to an object points to, as C names it.
    pub native: &'static str,

    /// The type an expression casts a pointer to, to reach into the object
    /// it points to.
    pub cast: &'static str,
}

/// The identity of an interpreted language's class: its module's path and
/// its qualified name's, with the last part of its qualified name as its
/// base, as `collections.OrderedDict` and `app.Outer.Inner` are named, and
/// a view's pattern `collections::OrderedDict` names the first.
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

/// A runtime's opaque name for a value, which only that runtime decodes and
/// which is valid for the stop it was read at: CPython's is a `PyObject`
/// address, V8's a tagged word. Do not use it as an address;
/// [`RuntimeModel::interpreted_address`] gives where the value lives.
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

/// An object the interpreter keeps, as the debugger presents it.
#[derive(Debug, Clone, PartialEq)]
pub struct InterpretedValue {
    pub object: InterpretedHandle,

    /// Where the value lives in memory, if it does.
    pub address: Option<VirtualAddress>,

    /// The object's class, which names the class for as long as the stop
    /// lasts.
    pub class: InterpretedHandle,

    /// The object's type: the module that defines it, where the debugger
    /// knows it, and its name there.
    pub module: Option<Arc<str>>,
    pub type_name: Arc<str>,
    pub kind: InterpretedKind,

    /// How many attributes the object shows by name, after the elements or
    /// entries its kind gives it.
    pub attributes: u64,

    /// The object as text, in the runtime's own notation, with what a
    /// container holds past the summary's bounds left out as `...`.
    pub summary: Arc<str>,

    /// The object's multiline text as the runtime prints it, when that
    /// differs from the summary.
    pub detail: Option<Arc<str>>,

    /// Why the summary or the children are shown in part, if they are.
    pub problem: Option<Arc<str>>,

    /// Which part of the object this is, by the index on each axis that
    /// leads to it, as a row of a numpy array is; empty for the object
    /// itself.
    pub part: Arc<[u64]>,

    /// The numbers the object, or its part, keeps as an array, which its
    /// elements are, if it keeps them so.
    pub array: Option<Arc<InterpretedArray>>,
}

/// An array of numbers an object keeps apart from any object, as a numpy
/// array keeps its elements: `shape[i]` on axis `i`, `strides[i]` bytes
/// apart, the first at `data`, each an `element`. Its elements on the first
/// axis are numbers where it has one axis, and arrays of the rest where it
/// has more.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InterpretedArray {
    pub data: VirtualAddress,
    pub shape: Arc<[u64]>,
    pub strides: Arc<[i64]>,
    pub element: ArrayElement,

    /// The element type's name, in the runtime's notation, as `float64`.
    pub element_name: Arc<str>,
}

impl InterpretedArray {
    /// Where its elements on the first axis lie, when they lie next to
    /// each other: numbers, the first at `data`, `shape[0]` of them.
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

/// What each element of an [`InterpretedArray`] is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArrayElement {
    /// A truth value, a byte of 0 or 1.
    Bool,
    Number(crate::NumberKind),
}

/// A number kept as the machine keeps it: where it is, its value, and its
/// type, by its module and name, in the runtime's notation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InterpretedNumber {
    pub address: VirtualAddress,
    pub value: crate::ScalarValue,
    pub element: ArrayElement,
    pub module: Option<Arc<str>>,
    pub type_name: Arc<str>,

    /// The number as text, in the runtime's notation.
    pub summary: Arc<str>,
}

impl InterpretedValue {
    /// How many elements or entries the object's kind gives it, which its
    /// children start with.
    #[must_use]
    pub const fn elements(&self) -> u64 {
        match self.kind {
            InterpretedKind::Sequence { length } | InterpretedKind::Map { length } => length,
            _ => 0,
        }
    }

    /// How many children [`RuntimeModel::interpreted_children`] gives: the
    /// elements or entries, then the attributes.
    #[must_use]
    pub const fn children(&self) -> u64 {
        self.elements().saturating_add(self.attributes)
    }
}

/// What kind of value an object is.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum InterpretedKind {
    /// The runtime's value that stands for no value, as Python's `None`
    /// and JavaScript's `null`.
    Nothing,

    /// JavaScript's `undefined`, which converts to a number differently
    /// from `null`.
    Undefined,
    Bool(bool),

    /// An integer, its value where it fits in 128 bits; the summary holds
    /// it at any size.
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

    /// An object of a class that holds no value of a built-in type, which
    /// its attributes alone describe.
    Instance,

    /// An object the debugger presents by its type and address alone.
    Other,
}

/// One child of an interpreted container.
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

/// Where an interpreted frame's variable keeps the object it holds, which
/// a watch of the variable watches.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InterpretedWatch {
    pub address: VirtualAddress,
    pub byte_size: u64,

    /// The code the frame runs, which tells a later frame at its address
    /// from it.
    pub code: VirtualAddress,
}

/// What a name means in an interpreted frame, as an expression binds it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InterpretedName {
    /// The frame's variable in this slot of its code's variables.
    Variable(u32),

    /// A global of the frame's module, or a built-in: the object the name
    /// is bound to.
    Object(InterpretedHandle),
}

/// A step an expression takes from an interpreter's object to another.
#[derive(Debug, Clone, Copy)]
pub enum InterpretedStep<'a> {
    /// To its attribute of this name.
    Attribute(&'a str),

    /// To the item a key or an index finds.
    Item(&'a InterpretedKey),
}

/// A key or an index, as an expression writes it.
#[derive(Debug, Clone, PartialEq)]
pub enum InterpretedKey {
    Int(i128),
    Float(f64),
    Bool(bool),

    /// Text, as UTF-8.
    Text(Vec<u8>),

    /// The value for nothing, such as Python's `None`.
    Nothing,

    /// Several keys in order: a tuple.
    Tuple(Vec<Self>),

    /// The program's own object, which a lookup compares by its identity
    /// before its value, as Python's does.
    Object(InterpretedHandle),
}

/// An interpreter's object, as an operation computes with it.
#[derive(Debug, Clone, PartialEq)]
pub struct InterpretedOperand {
    /// The name of its type, as messages show it.
    pub type_name: Arc<str>,
    pub kind: InterpretedKind,

    /// The characters of text as UTF-8, or the bytes of bytes, as far as
    /// they were read; `None` for any other kind.
    pub text: Option<crate::TextSummary>,
}

/// Why an expression's step through an interpreter's objects has no
/// result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InterpretedFailure {
    /// What the expression asks does not apply to the value, or is not
    /// done: the kind of the expression's error, and why.
    Refused(crate::eval::error::ErrorKind, Arc<str>),

    /// The value cannot be had, and why, as a variable's state says.
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

/// What a runtime's frame drives that the native stack does not show.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Driven {
    /// A future the frame polls, held in its variable `variable`. Its
    /// chain of awaits shows before the frame.
    Future { variable: &'static str },

    /// A segment of its thread's interpreted frames, which the runtime's
    /// [`RuntimeModel::interpreted`] gives: the frame is an activation of
    /// the interpreter's loop, and the innermost such activation runs the
    /// innermost segment.
    Interpreted,
}

/// How a task ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskEnd {
    /// Its future returned its output.
    Finished,

    /// Its future was dropped before it finished.
    Cancelled,
}

/// Every runtime a module carries, each bound against its debug
/// information or with why it cannot be; none for a module with no runtime
/// a model knows.
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

    /// Where a function breakpoint enters the function, once that is past
    /// its first instruction.
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

    /// Only the types whose path and name spell the name whole.
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

/// The type that lays out a value of `ty`, through names and qualifiers.
fn representation<'a>(image: &'a ModuleImage, mut ty: &'a TypeInfo) -> Option<&'a TypeInfo> {
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
