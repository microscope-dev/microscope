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

pub mod futures;
mod go;
mod python;
mod records;
mod rust;
mod tokio;

use std::sync::Arc;

use crate::inspection::InspectionBudget;
use crate::unwind::RegisterFile;
use crate::{
    CodeRole, CoroutineInfo, EntryProvenance, ExceptionFilter, ImageAddress, IntegerValue,
    ModuleImage, RecordMemberLayout, RecordedPlace, SourceLanguage, StackFrame, StackSegment,
    TaskState, ThreadId, ThreadLocal, TypeInfo, TypeKind, TypeReference, VirtualAddress,
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

    /// The one function whose symbol demangles to a name as people write
    /// it, such as `__rustc::rust_panic`, whose symbol carries a hash.
    fn function_answering(&self, name: &str) -> Option<ImageSymbol>;

    /// The demangled name of the symbol that begins at an address.
    fn symbol_at(&self, address: ImageAddress) -> Option<Arc<str>>;

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
}

/// One validated stop of the process a runtime runs in.
pub trait RuntimeStop {
    /// Fills `bytes` from memory as the program sees it, without the
    /// debugger's breakpoints, or returns `false` when any byte is
    /// unreadable.
    fn read(&self, address: VirtualAddress, bytes: &mut [u8]) -> bool;

    /// The thread pointer (`fs_base` on x86-64) of a stopped thread.
    fn thread_pointer(&self, thread: ThreadId) -> Option<u64>;

    /// The instruction a stopped thread will execute next.
    fn instruction(&self, thread: ThreadId) -> Option<VirtualAddress>;

    /// What the module carrying the runtime adds to its image addresses.
    fn load_bias(&self) -> crate::LoadBias;

    /// Every thread of the process, in the order of their ids.
    fn threads(&self) -> Vec<ThreadId>;

    /// The sets of tasks the futures the stop's threads drive run, by
    /// where the runtime keeps them, as [`RuntimeModel::task_set`] named
    /// them: tasks that no runtime lists while their set waits.
    fn task_sets(&self) -> Vec<u64> {
        Vec::new()
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
pub const TASK_NOUNS: [(&str, &str); 2] = [go::TASK_NOUN, self::tokio::TASK_NOUN];

/// The exceptions each runtime a model knows reports, which clients may
/// choose to stop at before they know which runtimes a program has.
pub const EXCEPTION_FILTERS: [ExceptionFilter; 4] = [
    go::EXCEPTION_FILTERS[0],
    go::EXCEPTION_FILTERS[1],
    go::EXCEPTION_FILTERS[2],
    rust::EXCEPTION_FILTERS[0],
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

    /// The runtime function that moves a task's stack to another, which
    /// the move has finished once it returns.
    fn stack_mover(&self) -> Option<ImageAddress>;

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
    /// [driver role](Self::driver_role), drives; `None` when it drives
    /// nothing.
    fn driven(&self, frame: &StackFrame) -> Option<Driven> {
        let _ = frame;
        None
    }

    /// The frames the runtime's interpreter runs on the stop's threads,
    /// and why some threads' may be missing; `None` for a runtime that
    /// interprets nothing.
    fn interpreted(&self, stop: &dyn RuntimeStop) -> Option<Partial<InterpretedStacks>> {
        let _ = stop;
        None
    }

    /// The local variables of the interpreted frame the interpreter keeps
    /// at `frame`, as [`RuntimeModel::interpreted`] named it at this stop,
    /// in the order the code declares them. Each read is charged to
    /// `budget`.
    fn interpreted_locals(
        &self,
        stop: &dyn RuntimeStop,
        frame: VirtualAddress,
        budget: &mut InspectionBudget,
    ) -> Result<Vec<InterpretedLocal>, Arc<str>> {
        let _ = (stop, frame, budget);
        Err("the runtime interprets no frames".into())
    }

    /// The children `range` of the interpreter's object at `object`, which
    /// [`InterpretedValue::children`] counted at this stop: a sequence's
    /// elements, or a map's entries, in order.
    fn interpreted_children(
        &self,
        stop: &dyn RuntimeStop,
        object: VirtualAddress,
        range: std::ops::Range<u64>,
        budget: &mut InspectionBudget,
    ) -> Result<Vec<InterpretedChild>, Arc<str>> {
        let _ = (stop, object, range, budget);
        Err("the runtime interprets no objects".into())
    }

    /// What `name` means in the interpreted frame the interpreter keeps at
    /// `frame`, as an expression binds it: one of the frame's variables,
    /// or the global or built-in it finds; `None` when it means nothing
    /// there.
    fn interpreted_name(
        &self,
        stop: &dyn RuntimeStop,
        frame: VirtualAddress,
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
        frame: VirtualAddress,
        slot: u32,
        budget: &mut InspectionBudget,
    ) -> Result<VirtualAddress, InterpretedFailure> {
        let _ = (stop, frame, slot, budget);
        Err(InterpretedFailure::interprets_nothing())
    }

    /// The object a step from the interpreter's object at `object` reaches.
    fn interpreted_step(
        &self,
        stop: &dyn RuntimeStop,
        object: VirtualAddress,
        taken: InterpretedStep<'_>,
        budget: &mut InspectionBudget,
    ) -> Result<VirtualAddress, InterpretedFailure> {
        let _ = (stop, object, taken, budget);
        Err(InterpretedFailure::interprets_nothing())
    }

    /// The interpreter's object at `object`, as an operation computes with
    /// it.
    fn interpreted_operand(
        &self,
        stop: &dyn RuntimeStop,
        object: VirtualAddress,
        budget: &mut InspectionBudget,
    ) -> Result<InterpretedOperand, InterpretedFailure> {
        let _ = (stop, object, budget);
        Err(InterpretedFailure::interprets_nothing())
    }

    /// The length of the interpreter's object at `object`, as its language
    /// measures it.
    fn interpreted_length(
        &self,
        stop: &dyn RuntimeStop,
        object: VirtualAddress,
        budget: &mut InspectionBudget,
    ) -> Result<u64, InterpretedFailure> {
        let _ = (stop, object, budget);
        Err(InterpretedFailure::interprets_nothing())
    }

    /// The interpreter's object at `object`, presented as a variable's
    /// value is.
    fn interpreted_value(
        &self,
        stop: &dyn RuntimeStop,
        object: VirtualAddress,
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

    /// Whether it is the outermost of the frames one activation of the
    /// interpreter's loop runs.
    pub outermost: bool,
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

    /// Whether the code's caller passes it, as a parameter.
    pub parameter: bool,
    pub slot: InterpretedSlot,
}

/// What a variable or a container's element holds.
#[derive(Debug, Clone, PartialEq)]
pub enum InterpretedSlot {
    Value(InterpretedValue),

    /// Nothing: the variable is not yet assigned, or was deleted.
    Unbound,

    /// A cell the frame shares with the closures it makes or was made in,
    /// which holds the value.
    Cell,

    /// The inspection's limits were reached before it was read.
    Limit(crate::InspectionExhaustion),

    /// It cannot be read, and why.
    Unreadable(Arc<str>),
}

/// An object the interpreter keeps, as the debugger presents it.
#[derive(Debug, Clone, PartialEq)]
pub struct InterpretedValue {
    pub object: VirtualAddress,

    /// The object's type: the module that defines it, where the debugger
    /// knows it, and its name there.
    pub module: Option<Arc<str>>,
    pub type_name: Arc<str>,
    pub kind: InterpretedKind,

    /// The object as text, in the runtime's own notation, with what a
    /// container holds past the summary's bounds left out as `...`.
    pub summary: Arc<str>,

    /// Why the summary or the children are shown in part, if they are.
    pub problem: Option<Arc<str>>,
}

impl InterpretedValue {
    /// How many children [`RuntimeModel::interpreted_children`] gives.
    #[must_use]
    pub const fn children(&self) -> u64 {
        match self.kind {
            InterpretedKind::Sequence { length } | InterpretedKind::Map { length } => length,
            _ => 0,
        }
    }
}

/// What kind of value an object is.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum InterpretedKind {
    /// The runtime's value that stands for no value, as Python's `None`.
    Nothing,
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
}

/// What a name means in an interpreted frame, as an expression binds it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InterpretedName {
    /// The frame's variable in this slot of its code's variables.
    Variable(u32),

    /// A global of the frame's module, or a built-in: the object the name
    /// is bound to.
    Object(VirtualAddress),
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
        .collect()
}

impl RuntimeImage for ModuleImage {
    fn producers(&self) -> Vec<&str> {
        Self::producers(self).collect()
    }

    fn python(&self) -> Option<crate::image::python::PythonRuntime> {
        self.python_runtime()
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
