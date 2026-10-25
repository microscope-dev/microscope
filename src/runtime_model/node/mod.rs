//! Node.js, as the V8 runtime that runs a program's JavaScript. The module
//! that carries V8, the `node` executable or a shared `libnode`, has the
//! layout its loader read and checked (`crate::image::v8`); this model reads
//! V8's frames through it at a stop.
//!
//! V8's frames are native frames: its builtins and the code it compiles
//! for JavaScript run on the thread's own stack, and the unwinder walks
//! them by V8's frame-pointer convention. The model names each such frame
//! as V8 recorded it ([`RuntimeModel::code_frame`]).

mod access;
mod awaits;
mod call_sites;
mod collections;
mod compiled;
mod constructors;
mod errors;
mod exceptions;
mod frames;
mod inspect;
pub mod inspector;

pub use exceptions::EXCEPTION_FILTERS;
mod lineage;
mod lookup;
mod names;
mod napi;
mod native_inspector;
mod objects;
mod own;
mod pool;
mod positions;
mod present;
mod properties;
mod scripts;
mod tasks;
mod timers;
mod translation;
mod typed;
mod values;
mod variables;
mod watches;

use std::path::Path;
use std::sync::Arc;

use super::{
    CodeFrame, Crossing, DescribedFrame, HeldIdentity, Partial, RuntimeException, RuntimeHook,
    RuntimeImage, RuntimeModel, RuntimeSignals, RuntimeStop, StartedTask, StopMemory, StoredValue,
    TaskContext, TaskPage, TaskRef, TaskStarter, ThreadActivity, ThreadHolding, Unbound,
};
use crate::ThreadLocal;
use crate::image::v8::{DispatchTable, EmbeddedBlob, NodeRelease, NodeTasks, V8Layout, V8Version};
use crate::inspection::InspectionBudget;
use crate::runtime_model::{
    AssignedBinding, BindingReach, FrameReplacer, InterpretedChild, InterpretedFailure,
    InterpretedFrameRef, InterpretedHandle, InterpretedLocal, InterpretedName, InterpretedOperand,
    InterpretedPlace, InterpretedSlot, InterpretedStep, InterpretedWatch, ObjectTypes,
};
use crate::unwind::RegisterFile;
use crate::{AddressRange, ImageAddress, StackSegment, ThreadId, VirtualAddress};

/// The place of `js:NAME`: every function V8 names `name`, as its `name`
/// property gives it. V8 names a method by its own name only, so a
/// qualified one would match nothing, and is refused.
fn function_place(name: &str) -> Result<Vec<crate::ManagedPlace>, Arc<str>> {
    if name.is_empty() {
        return Err("a JavaScript function breakpoint needs a function name".into());
    }
    if name.contains('.') {
        return Err(format!(
            "V8 names a function by its own name, so js:{name} matches nothing; use js:{}",
            name.rsplit('.').next().unwrap_or(name)
        )
        .into());
    }
    Ok(vec![crate::ManagedPlace::Function { name: name.into() }])
}

/// What Node calls the work its event loop waits on.
pub(super) const TASK_NOUN: (&str, &str) = ("handle", "handles");

/// The extensions of the scripts Node runs, whose breakpoints its
/// inspector arms.
pub const SCRIPT_EXTENSIONS: &[&str] = &["js", "mjs", "cjs", "jsx", "ts", "mts", "cts", "tsx"];

const GET_TARGET_IDS: &str = "_ZN4node9inspector19InspectorIoDelegate12GetTargetIdsB5cxx11Ev";

/// What V8 names each of its embedded builtins' code.
const BUILTIN_PREFIX: &str = "Builtins_";

/// Where V8 performs each collection, scavenge, minor mark-sweep, or full
/// mark-compact, on the thread of the isolate whose heap it collects
/// (`src/heap/heap.cc`).
const PERFORM_GARBAGE_COLLECTION: &str = "_ZN2v88internal4Heap24PerformGarbageCollectionENS0_16GarbageCollectorENS0_23GarbageCollectionReasonEPKc";

/// The builtins that build an unoptimized frame and fill its registers:
/// the interpreter's entry, and Sparkplug code's out-of-line prologue
/// (`src/builtins/x64/builtins-x64.cc`).
const FRAME_BUILDERS: &[&str] = &[
    "Builtins_InterpreterEntryTrampoline",
    "Builtins_InterpreterEntryTrampolineForProfiling",
    "Builtins_BaselineOutOfLinePrologue",
    "Builtins_BaselineOutOfLinePrologueDeopt",
];

/// The builtins a loop's back edge calls to enter optimized code in place
/// of the interpreter's or Sparkplug's frame, with the saved frame
/// pointers between the entry's frame pointer and that frame
/// (`src/builtins/x64/builtins-x64.cc`). The interpreter's `JumpLoop`
/// handler calls its builtin from the handler's own frame, which
/// `OnStackReplacement` drops (`leave`) before it enters the code;
/// Sparkplug's code calls its builtin from the frame itself. Each entry is
/// reached on every back edge while the replacement compiles. Entering
/// Sparkplug's code in place of the interpreter's keeps the frame's
/// registers where they are (`UnoptimizedJSFrame::ReadInterpreterRegister`),
/// so its builtin, `InterpreterOnStackReplacement_ToBaseline`, is not
/// among them.
const ON_STACK_REPLACEMENT: &[(&str, u8)] = &[
    ("Builtins_InterpreterOnStackReplacement", 1),
    ("Builtins_BaselineOnStackReplacement", 0),
];

/// The V8 runtime a module carries. One whose layout was refused still
/// binds, so that its frames say why they cannot be read.
pub fn detect(
    image: &Arc<dyn RuntimeImage + Send + Sync>,
) -> Option<Result<Arc<dyn RuntimeModel>, Unbound>> {
    let runtime = image.v8()?;
    let reading = runtime
        .layout
        .map_err(|reason| format!("the JavaScript frames cannot be read: {reason}").into());
    let statics = Statics {
        isolate: image.thread_local(crate::image::v8::CURRENT_ISOLATE),
        dispatch_table: runtime.dispatch_table,
        awaits: await_builtins(image.as_ref(), runtime.version),
        handlers: bytecode_handlers(image.as_ref()),
    };
    Some(Ok(Arc::new(NodeRuntime {
        version: runtime.version,
        reading,
        builtins: builtins(image.as_ref()),
        blob: runtime.embedded_blob.ok(),
        writer: writer(runtime.version, runtime.node),
        statics,
        table: std::sync::Mutex::default(),
        inspector_hook: runtime
            .inspector
            .as_ref()
            .ok()
            .map(|layout| layout.targets.entry)
            // An unsupported body still traps at its named entry: inspector_target
            // returns the metadata refusal before the preload can print its URL.
            .or_else(|| image.symbol(GET_TARGET_IDS).map(|symbol| symbol.address)),
        native_entry: image.symbol(napi::INVOKE).map(|symbol| symbol.address),
        collector: image
            .symbol(PERFORM_GARBAGE_COLLECTION)
            .map(|symbol| symbol.address),
        replacers: ON_STACK_REPLACEMENT
            .iter()
            .filter_map(|&(name, links)| {
                image.symbol(name).map(|symbol| FrameReplacer {
                    entry: symbol.address,
                    links,
                })
            })
            .collect(),
        frame_builders: FRAME_BUILDERS
            .iter()
            .filter_map(|name| {
                let symbol = image.symbol(name)?;
                Some(AddressRange {
                    start: symbol.address,
                    end: symbol.address.checked_add(symbol.size?)?,
                })
            })
            .collect(),
        inspector_signal_handler: runtime
            .inspector
            .as_ref()
            .ok()
            .map(|layout| layout.signal_handler),
        inspector_close_functions: runtime
            .inspector
            .as_ref()
            .ok()
            .and_then(|layout| layout.close),
        native_inspector: runtime.inspector,
        tasks: runtime.tasks,
        image: Arc::clone(image),
    })))
}

/// The Node release whose output to match: the one that built the module,
/// or else the first on its V8 line (V8 12 is Node 22, 13 is 24, 14 is 26).
fn writer(version: V8Version, node: Result<NodeRelease, Arc<str>>) -> present::Writer {
    node.map_or_else(
        |reason| {
            let major = match version.major {
                ..=12 => 22,
                13 => 24,
                _ => 26,
            };
            present::Writer {
                release: NodeRelease {
                    major,
                    minor: 0,
                    patch: 0,
                },
                assumed: Some(
                    format!(
                        "could not tell which Node release built the module ({reason}); \
                         showing the value as Node {major}.0.0 prints it"
                    )
                    .into(),
                ),
            }
        },
        |release| present::Writer {
            release,
            assumed: None,
        },
    )
}

/// What reading V8's compiled code needs of the module that carries it.
#[derive(Debug)]
struct Statics {
    /// Where each thread keeps its current isolate.
    isolate: Option<Result<ThreadLocal, Arc<str>>>,
    dispatch_table: Result<Option<DispatchTable>, Arc<str>>,

    /// The code of V8's bytecode handlers ([`bytecode_handlers`]).
    handlers: Vec<AddressRange<ImageAddress>>,
    /// The numbers of the builtins an await chain is read by.
    awaits: Result<awaits::Builtins, Arc<str>>,
}

/// The numbers of the builtins an await chain is read by, in V8
/// `version`, checked against the image's own builtins where its symbols
/// name them: V8 numbers its builtins in the order it embeds them, and a
/// build whose configuration adds or drops one numbers the rest apart.
/// A module without the symbols, as Debian's, has the numbers of its V8
/// release's official build.
fn await_builtins(
    image: &(dyn RuntimeImage + Send + Sync),
    version: V8Version,
) -> Result<awaits::Builtins, Arc<str>> {
    let (count, numbers) = crate::image::v8::builtins(version)?;
    let mut addresses: Vec<ImageAddress> = image
        .functions_starting(BUILTIN_PREFIX)
        .into_iter()
        .map(|symbol| symbol.address)
        .collect();
    if addresses.is_empty() {
        return Ok(numbers);
    }
    addresses.sort_unstable();
    addresses.dedup();
    if addresses.len() != count as usize {
        return Err(format!(
            "the module has {} builtins, where V8 {}.{} has {count}",
            addresses.len(),
            version.major,
            version.minor
        )
        .into());
    }
    for (builtin, number) in numbers {
        let name = usize::try_from(*number)
            .ok()
            .and_then(|index| addresses.get(index))
            .and_then(|address| image.symbol_at(*address));
        if name
            .as_deref()
            .and_then(|name| name.strip_prefix(BUILTIN_PREFIX))
            != Some(builtin.name())
        {
            return Err(format!(
                "the module's builtin {number} is {}, not {}",
                name.as_deref().unwrap_or("unnamed"),
                builtin.name()
            )
            .into());
        }
    }
    Ok(numbers)
}

/// The range of the image's code its embedded builtins span, where it
/// names them.
fn builtins(image: &(dyn RuntimeImage + Send + Sync)) -> Option<AddressRange<ImageAddress>> {
    let symbols = image.functions_starting(BUILTIN_PREFIX);
    let start = symbols.iter().map(|symbol| symbol.address).min()?;
    let end = symbols
        .iter()
        .filter_map(|symbol| symbol.address.checked_add(symbol.size?))
        .max()?;
    Some(AddressRange { start, end })
}

/// The code of V8's bytecode handlers, `Builtins_` followed by a bytecode's
/// name and `Handler`, `WideHandler`, or `ExtraWideHandler`, as ranges in
/// address order. A handler runs in the interpreter's frame: the
/// interpreter's entry calls the first, each jumps to the next, and the
/// last returns, none moving rbp off the frame
/// (`Generate_InterpreterEntryTrampoline`, `src/builtins/x64/builtins-x64.cc`).
/// A build that names no builtins names no handler.
fn bytecode_handlers(image: &(dyn RuntimeImage + Send + Sync)) -> Vec<AddressRange<ImageAddress>> {
    let mut ranges = image
        .functions_starting(BUILTIN_PREFIX)
        .into_iter()
        .filter(|symbol| {
            image
                .symbol_at(symbol.address)
                .is_some_and(|name| name.ends_with("Handler"))
        })
        .filter_map(|symbol| {
            Some(AddressRange {
                start: symbol.address,
                end: symbol.address.checked_add(symbol.size?)?,
            })
        })
        .collect::<Vec<_>>();
    ranges.sort_unstable_by_key(|range| range.start);
    let mut merged: Vec<AddressRange<ImageAddress>> = Vec::with_capacity(ranges.len());
    for range in ranges {
        match merged.last_mut() {
            Some(last) if range.start <= last.end => last.end = last.end.max(range.end),
            _ => merged.push(range),
        }
    }
    merged
}

/// The address of the isolate `thread` has entered, from its copy of the
/// variable at `place`: 0 for none.
fn current_isolate(
    stop: &dyn RuntimeStop,
    thread: ThreadId,
    place: ThreadLocal,
) -> Result<u64, Arc<str>> {
    let slot = stop.thread_local_address(thread, place)?;
    let mut word = [0; 8];
    if !stop.read(slot, &mut word) {
        return Err("the thread's current isolate is unreadable".into());
    }
    Ok(u64::from_le_bytes(word))
}

#[derive(Debug)]
struct NodeRuntime {
    version: V8Version,

    /// The layout V8's frames and objects are read by, or why none can be.
    reading: Result<V8Layout, Arc<str>>,
    builtins: Option<AddressRange<ImageAddress>>,

    /// Where the process keeps the bounds of the embedded builtins, which
    /// bound them where no symbol does.
    blob: Option<EmbeddedBlob>,

    /// The release whose `util.inspect` output values should match.
    writer: present::Writer,
    statics: Statics,

    /// The scripts and functions frames name, for the life of the process.
    table: std::sync::Mutex<scripts::Table>,

    /// Node's `GetTargetIds`, where the inspector's address is read.
    inspector_hook: Option<ImageAddress>,

    /// Where V8 enters a Node-API function's C callback.
    native_entry: Option<ImageAddress>,

    /// Where V8 performs a collection, which a watch follows.
    collector: Option<ImageAddress>,

    /// Where V8 replaces an unoptimized frame by optimized code.
    replacers: Vec<FrameReplacer>,

    /// The code that builds an unoptimized frame ([`FRAME_BUILDERS`]).
    frame_builders: Vec<AddressRange<ImageAddress>>,
    inspector_signal_handler: Option<ImageAddress>,
    native_inspector: Result<crate::image::v8::NodeInspector, Arc<str>>,
    inspector_close_functions: Option<(ImageAddress, ImageAddress)>,

    /// Where Node keeps its event loop's work, or why the module's code
    /// does not say.
    tasks: Result<NodeTasks, Arc<str>>,

    /// The module's symbols, which name the classes of Node's wrappers.
    image: Arc<dyn RuntimeImage + Send + Sync>,
}

impl NodeRuntime {
    /// Runs `read` against the process at `stop`, charging each read to
    /// `budget`.
    fn reading<T>(
        &self,
        stop: &dyn RuntimeStop,
        budget: &mut InspectionBudget,
        read: impl FnOnce(&objects::Reader<'_>) -> Result<T, objects::Problem>,
    ) -> Result<T, objects::Problem> {
        let layout = self.reading.as_ref().map_err(Arc::clone)?;
        let budget = std::cell::RefCell::new(budget);
        let read_now = StopMemory::default();
        let charge = |bytes: usize| -> Result<(), objects::Problem> {
            budget
                .borrow_mut()
                .consume_memory(bytes)
                .map_err(|exhaustion| {
                    crate::VariableUnavailableReason::InspectionLimit(exhaustion)
                        .to_string()
                        .into()
                })
        };
        let reader = objects::Reader {
            stop,
            layout,
            version: self.version,
            charge: Some(&charge),
            // Cache blocks for the whole stop when the debugger provides a
            // cache, or just for this read otherwise.
            blocks: Some(stop.memory().unwrap_or(&read_now)),
        };
        read(&reader)
    }

    /// Every task at `stop`, by number, and why some may be missing.
    fn all_tasks(&self, stop: &dyn RuntimeStop) -> (Vec<super::RuntimeTask>, Vec<Arc<str>>) {
        let walked = self.walk(stop, tasks::listed);
        let (mut found, mut gaps) = match walked {
            Ok(walked) => walked,
            Err(reason) => {
                return (
                    Vec::new(),
                    vec![format!("Node's tasks cannot be read: {reason}").into()],
                );
            }
        };
        match self.pool(stop) {
            Some(Ok(pool)) => found.extend(pool.busy.into_iter().map(pool::job)),
            Some(Err(problem)) => {
                gaps.push(format!("libuv's thread-pool jobs cannot be read: {problem}").into());
            }
            None => gaps.push(
                "libuv's thread-pool jobs are not listed: the module does not link libuv's pool, \
                 which a shared libuv keeps"
                    .into(),
            ),
        }
        (found, gaps)
    }

    /// Runs `walk` over Node's event loop work at `stop`, given where each
    /// thread keeps its isolate, or says why it cannot.
    fn walk<T>(
        &self,
        stop: &dyn RuntimeStop,
        walk: impl FnOnce(&mut tasks::Walk<'_>, ThreadLocal) -> T,
    ) -> Result<T, Arc<str>> {
        let layout = self.reading.as_ref().map_err(Arc::clone)?;
        let tasks = self.tasks.as_ref().map_err(Arc::clone)?;
        let isolate = match &self.statics.isolate {
            Some(Ok(place)) => *place,
            Some(Err(reason)) => {
                return Err(format!("no thread's isolate can be found: {reason}").into());
            }
            None => return Err("the module names no current isolate".into()),
        };
        let reader = objects::Reader {
            stop,
            layout,
            version: self.version,
            charge: None,
            blocks: stop.memory(),
        };
        let mut table = self
            .table
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        Ok(walk(
            &mut tasks::Walk {
                reader,
                layout: tasks,
                image: self.image.as_ref(),
                table: &mut table,
            },
            isolate,
        ))
    }

    /// libuv's pool at `stop`, or `None` where the module does not hold it.
    fn pool(&self, stop: &dyn RuntimeStop) -> Option<Result<pool::Pool, Arc<str>>> {
        let tasks = self.tasks.as_ref().ok()?;
        let uv = tasks.pool.as_ref()?;
        let layout = match &self.reading {
            Ok(layout) => layout,
            Err(reason) => return Some(Err(Arc::clone(reason))),
        };
        let reader = objects::Reader {
            stop,
            layout,
            version: self.version,
            charge: None,
            blocks: stop.memory(),
        };
        Some(pool::read(&reader, uv))
    }

    /// [`Self::reading`] for expressions, where a failure means the value is
    /// unavailable.
    fn expression<T>(
        &self,
        stop: &dyn RuntimeStop,
        budget: &mut InspectionBudget,
        read: impl FnOnce(&objects::Reader<'_>) -> Result<T, objects::Problem>,
    ) -> Result<T, InterpretedFailure> {
        self.reading(stop, budget, read).map_err(|problem| {
            InterpretedFailure::Unavailable(crate::VariableUnavailableReason::Interpreted(problem))
        })
    }

    /// The embedded builtins' code as `stop` says V8 stored its bounds:
    /// a pointer, then a 32-bit size. `None` before V8 stored them, and
    /// where they cannot be read.
    fn stored_builtins(&self, stop: &dyn RuntimeStop) -> Option<AddressRange<VirtualAddress>> {
        let blob = self.blob?;
        let bias = stop.load_bias();
        let mut pointer = [0; 8];
        let mut size = [0; 4];
        if !stop.read(bias.load(blob.code)?, &mut pointer)
            || !stop.read(bias.load(blob.code_size)?, &mut size)
        {
            return None;
        }
        let start = u64::from_le_bytes(pointer);
        let size = u64::from(u32::from_le_bytes(size));
        if start == 0 || size == 0 {
            return None;
        }
        Some(AddressRange {
            start: VirtualAddress::new(start),
            end: VirtualAddress::new(start.checked_add(size)?),
        })
    }
}

impl RuntimeModel for NodeRuntime {
    fn owns_code(&self, address: ImageAddress) -> bool {
        self.builtins
            .is_some_and(|range| range.start <= address && address < range.end)
    }

    fn process_code(&self, stop: &dyn RuntimeStop) -> Vec<AddressRange<VirtualAddress>> {
        // Where symbols bound the builtins, they own them in the image and
        // in any copy V8 made of them, which the stored bounds may name.
        if self.builtins.is_some() {
            return Vec::new();
        }
        self.stored_builtins(stop).into_iter().collect()
    }

    fn owns_generated_code(&self) -> bool {
        true
    }

    fn code_frame(&self, stop: &dyn RuntimeStop, frame: &CodeFrame<'_>) -> Option<DescribedFrame> {
        let layout = self.reading.as_ref().ok()?;
        let reader = objects::Reader {
            stop,
            layout,
            version: self.version,
            charge: None,
            blocks: stop.memory(),
        };
        let mut table = self
            .table
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        frames::describe(
            &reader,
            &mut table,
            positions::Encoding::of(self.version),
            &self.statics,
            frame,
        )
    }

    /// Every handle, request, and timer of each JavaScript thread's event
    /// loop, by number, then the jobs libuv's pool runs.
    fn tasks(
        &self,
        stop: &dyn RuntimeStop,
        start: u64,
        limit: usize,
        _program_only: bool,
    ) -> Partial<TaskPage> {
        let (all, gaps) = self.all_tasks(stop);
        let start = usize::try_from(start).unwrap_or(usize::MAX);
        let tasks: Vec<_> = all.iter().skip(start).take(limit).cloned().collect();
        let end = start.saturating_add(tasks.len());
        Partial {
            value: TaskPage {
                tasks,
                next: (end < all.len()).then_some(end as u64),
            },
            // Every page reads the whole list; its gaps are said once.
            gaps: if start == 0 { gaps } else { Vec::new() },
        }
    }

    /// A pool thread runs its job; no other thread runs a task of Node's,
    /// whose event loop's work runs no code until it is done.
    fn thread_activity(&self, stop: &dyn RuntimeStop, thread: ThreadId) -> ThreadActivity {
        match self.pool(stop) {
            Some(Ok(pool)) if pool.busy.contains(&thread) => ThreadActivity::Task {
                number: tasks::pool_job(thread),
                stack: StackSegment::Thread,
            },
            None | Some(Ok(_)) => ThreadActivity::Outside,
            Some(Err(problem)) => ThreadActivity::Unknown(problem),
        }
    }

    /// The address of the isolate the thread works for. Main and worker
    /// threads hold theirs for their whole event loop; from V8 13.6 a helper
    /// thread holds it while its `LocalHeap` works for the isolate (e.g. a
    /// background compile). libuv's workers hold none.
    fn thread_holding(&self, stop: &dyn RuntimeStop, thread: ThreadId) -> Option<ThreadHolding> {
        let identity = match &self.statics.isolate {
            Some(Ok(place)) => match current_isolate(stop, thread, *place) {
                Ok(0) => return None,
                Ok(isolate) => Ok(HeldIdentity::new(isolate)),
                Err(reason) => Err(reason),
            },
            Some(Err(reason)) => Err(format!("its variable cannot be found: {reason}").into()),
            None => Err("the module names no current isolate".into()),
        };
        Some(ThreadHolding {
            kind: "isolate",
            identity,
        })
    }

    /// A thread-pool job runs on its thread. A handle, request, or timer
    /// runs no code until Node calls its callback, so it has no frames.
    fn task_context(
        &self,
        stop: &dyn RuntimeStop,
        task: TaskRef,
    ) -> Result<Option<TaskContext>, Arc<str>> {
        if let Some(thread) = tasks::pool_thread(task.number) {
            return match self.pool(stop) {
                Some(Ok(pool)) if pool.busy.contains(&thread) => {
                    Ok(Some(TaskContext::OnThread(thread)))
                }
                Some(Err(problem)) => Err(problem),
                _ => Ok(None),
            };
        }
        let stackless = || "it runs no code until Node calls its callback, so it has no frames";
        // The entry this stop's listing located is read alone; without it,
        // or where it no longer proves the task, every list is.
        if let Some(locator) = task.locator
            && self.walk(stop, |walk, isolate| {
                tasks::located(walk, isolate, task.number, locator)
            }) == Ok(Ok(true))
        {
            return Err(stackless().into());
        }
        let (all, _) = self.all_tasks(stop);
        if all.iter().any(|listed| listed.number == task.number) {
            return Err(stackless().into());
        }
        Ok(None)
    }

    fn thread_stacks(
        &self,
        _stop: &dyn RuntimeStop,
        _thread: ThreadId,
    ) -> Result<Vec<(std::ops::Range<u64>, StackSegment)>, Arc<str>> {
        Ok(Vec::new())
    }

    fn cross(
        &self,
        _stop: &dyn RuntimeStop,
        _thread: ThreadId,
        _frame: &RegisterFile,
        _after_call: bool,
    ) -> Result<Crossing, Arc<str>> {
        Ok(Crossing::Stay)
    }

    fn signals(&self) -> RuntimeSignals {
        RuntimeSignals::default()
    }

    fn hooks(&self) -> &[RuntimeHook] {
        &[]
    }

    fn exception(
        &self,
        _stop: &dyn RuntimeStop,
        _hook: ImageAddress,
        _registers: &RegisterFile,
    ) -> Result<RuntimeException, Arc<str>> {
        Err("V8 has no exception hook yet".into())
    }

    fn dynamic_value(
        &self,
        _stop: &dyn RuntimeStop,
        _representation: &str,
        _value: StoredValue<'_>,
    ) -> Option<Result<super::DynamicValue, Arc<str>>> {
        None
    }

    fn stack_mover(&self) -> Option<ImageAddress> {
        None
    }

    fn frame_pointer_code(&self) -> Vec<AddressRange<ImageAddress>> {
        self.statics.handlers.clone()
    }

    fn inspector_hook(&self) -> Option<ImageAddress> {
        self.inspector_hook
    }

    fn native_entry(&self) -> Option<ImageAddress> {
        self.native_entry
    }

    fn native_callee(
        &self,
        stop: &dyn RuntimeStop,
        registers: &RegisterFile,
    ) -> Result<crate::VirtualAddress, Arc<str>> {
        let info = registers
            .get(crate::unwind::RDI)
            .ok_or("the call's info is not in rdi")?;
        let mut budget = InspectionBudget::new(crate::InspectionLimits::default());
        self.reading(stop, &mut budget, |reader| {
            napi::callback(reader, self.version, info)
        })
        .map(crate::VirtualAddress::new)
    }

    fn inspector_target(
        &self,
        stop: &dyn RuntimeStop,
        caller: VirtualAddress,
        delegate: VirtualAddress,
    ) -> Result<Option<Arc<str>>, Arc<str>> {
        native_inspector::target(
            self.native_inspector.as_ref().map_err(Clone::clone)?,
            stop,
            caller,
            delegate,
        )
    }

    fn inspector_signal_handler(&self) -> Option<ImageAddress> {
        self.inspector_signal_handler
    }

    fn inspector_close_functions(&self) -> Option<(ImageAddress, ImageAddress)> {
        self.inspector_close_functions
    }

    fn inspector_startup_hooks(&self) -> Option<[ImageAddress; 3]> {
        let startup = self.native_inspector.as_ref().ok()?.startup?;
        Some([startup.before_wait, startup.after_wait, startup.check])
    }

    fn inspector_startup_pending(
        &self,
        stop: &dyn RuntimeStop,
        agent: VirtualAddress,
    ) -> Result<bool, Arc<str>> {
        native_inspector::startup_pending(
            self.native_inspector.as_ref().map_err(Clone::clone)?,
            stop,
            agent,
        )
    }

    fn inspector_agent(
        &self,
        stop: &dyn RuntimeStop,
    ) -> Result<Option<(VirtualAddress, Arc<str>)>, Arc<str>> {
        native_inspector::server(self.native_inspector.as_ref().map_err(Clone::clone)?, stop)
            .map(|server| server.map(|server| (server.agent, server.target)))
    }

    fn inspector_message_matches(
        &self,
        stop: &dyn RuntimeStop,
        view: VirtualAddress,
        expected: &[u8],
    ) -> Result<bool, Arc<str>> {
        native_inspector::message_matches(stop, view, expected)
    }

    fn inspector_endpoint(
        &self,
        stop: &dyn RuntimeStop,
    ) -> Result<Option<(std::net::SocketAddr, Arc<str>)>, Arc<str>> {
        native_inspector::endpoint(self.native_inspector.as_ref().map_err(Clone::clone)?, stop)
    }

    fn moving_task(
        &self,
        _stop: &dyn RuntimeStop,
        _registers: &RegisterFile,
    ) -> Result<u64, Arc<str>> {
        Err("V8 moves no task between stacks".into())
    }

    fn task_stack(
        &self,
        _stop: &dyn RuntimeStop,
        _task: TaskRef,
    ) -> Result<Option<std::ops::Range<u64>>, Arc<str>> {
        Ok(None)
    }

    fn call_out(
        &self,
        _entry: ImageAddress,
        _registers: &RegisterFile,
    ) -> Option<Result<VirtualAddress, Arc<str>>> {
        None
    }

    fn task_starters(&self) -> Vec<TaskStarter> {
        Vec::new()
    }

    fn started_task(
        &self,
        _stop: &dyn RuntimeStop,
        _starter: ImageAddress,
        _registers: &RegisterFile,
    ) -> Result<Option<StartedTask>, Arc<str>> {
        Err("V8 starts no task microscope follows".into())
    }

    fn task_noun(&self) -> &'static str {
        TASK_NOUN.0
    }

    fn resolve_breakpoint(
        &self,
        _stop: &dyn RuntimeStop,
        spec: &crate::BreakpointSpec,
    ) -> Option<Result<Vec<crate::ManagedPlace>, Arc<str>>> {
        let (path, line) = match spec {
            crate::BreakpointSpec::Source { path, line } => (path, line),
            // `js::run` is a native function in a `js` namespace.
            crate::BreakpointSpec::Function(name)
                if let Some(name) = name.strip_prefix("js:")
                    && !name.starts_with(':') =>
            {
                return Some(function_place(name));
            }
            _ => return None,
        };
        if !path
            .extension()
            .and_then(|extension| extension.to_str())
            .is_some_and(|extension| SCRIPT_EXTENSIONS.contains(&extension))
        {
            return None;
        }

        // As a native breakpoint's path does, a relative path matches the
        // scripts whose paths end with its components.
        let path = path
            .components()
            .filter(|component| *component != std::path::Component::CurDir)
            .collect::<std::path::PathBuf>();
        let Some(path) = path.to_str() else {
            return Some(Err("the script's path is not UTF-8".into()));
        };
        let Ok(line) = u32::try_from(line.get()) else {
            return Some(Err("the script's line exceeds 32 bits".into()));
        };
        Some(Ok(vec![crate::ManagedPlace::Line {
            path: path.into(),
            line,
        }]))
    }

    fn own_source(&self, path: &Path) -> bool {
        path.to_str().is_some_and(|path| path.starts_with("node:"))
    }

    fn interpreted_language(&self) -> crate::SourceLanguage {
        crate::SourceLanguage::JavaScript
    }

    fn interpreted_locals(
        &self,
        stop: &dyn RuntimeStop,
        frame: InterpretedFrameRef,
        budget: &mut InspectionBudget,
    ) -> Result<Vec<InterpretedLocal>, Arc<str>> {
        let writer = &self.writer;
        self.reading(stop, budget, |reader| {
            Ok(variables::variables(reader, frame)?
                .into_iter()
                .map(|variable| InterpretedLocal {
                    name: variable.name.into(),
                    kind: variable.kind.shown(),
                    unnamed: match &variable.kind {
                        variables::Kind::Register { unnamed, .. } => {
                            Some(unnamed.clone().unwrap_or_else(|| variables::UNNAMED.into()))
                        }
                        _ => None,
                    },
                    slot: match variable.word {
                        Ok(word) => {
                            present::value(reader, present::Handle::Tagged(word).handle(), writer)
                        }
                        Err(problem) if *problem == *variables::UNINITIALIZED => {
                            InterpretedSlot::Unbound
                        }
                        Err(problem) => InterpretedSlot::Unreadable(problem),
                    },
                })
                .collect())
        })
    }

    fn interpreted_children(
        &self,
        stop: &dyn RuntimeStop,
        (object, _part): (InterpretedHandle, &[u64]),
        range: std::ops::Range<u64>,
        budget: &mut InspectionBudget,
    ) -> Result<Vec<InterpretedChild>, Arc<str>> {
        let writer = &self.writer;
        self.reading(stop, budget, |reader| {
            present::children(reader, object, range, writer)
        })
    }

    fn interpreted_name(
        &self,
        stop: &dyn RuntimeStop,
        frame: InterpretedFrameRef,
        name: &str,
        budget: &mut InspectionBudget,
    ) -> Result<Option<InterpretedName>, InterpretedFailure> {
        self.expression(stop, budget, |reader| {
            Ok(access::bind(reader, frame, name)?.map(|bound| match bound {
                access::Bound::Variable(index) => InterpretedName::Variable(index),
                access::Bound::Value(handle) => InterpretedName::Object(handle),
            }))
        })
    }

    fn interpreted_variable(
        &self,
        stop: &dyn RuntimeStop,
        frame: InterpretedFrameRef,
        slot: u32,
        budget: &mut InspectionBudget,
    ) -> Result<InterpretedHandle, InterpretedFailure> {
        self.expression(stop, budget, |reader| access::variable(reader, frame, slot))
    }

    fn assigned_binding(
        &self,
        stop: &dyn RuntimeStop,
        frame: InterpretedFrameRef,
        slot: u32,
        budget: &mut InspectionBudget,
    ) -> Result<AssignedBinding, InterpretedFailure> {
        use variables::Kind;
        let found = self.expression(stop, budget, |reader| {
            variables::variables(reader, frame)?
                .into_iter()
                .nth(usize::try_from(slot).unwrap_or(usize::MAX))
                .ok_or_else(|| "the frame no longer has the variable".into())
        })?;
        let refused = |reason: &str| {
            InterpretedFailure::Refused(
                crate::eval::error::ErrorKind::Assignment,
                format!("{} {reason}", found.name).into(),
            )
        };
        if let Err(problem) = &found.word
            && **problem == *variables::WITH_CONTEXT
        {
            return Err(refused(
                "is found through a `with` statement's object, which V8 does not assign",
            ));
        }
        let mut refusable = None;
        let reach = match &found.kind {
            Kind::Argument(_) | Kind::Named(_) | Kind::Local => BindingReach::Frame,
            // V8 gives a value to an optimized frame's variable that a
            // context holds, but not to one optimized code keeps.
            Kind::Optimized => {
                refusable = Some(
                    "V8 assigns no variable optimized code keeps, only one a context holds".into(),
                );
                BindingReach::Frame
            }
            Kind::Captured => BindingReach::Enclosing,
            Kind::Module => BindingReach::Script,
            Kind::Receiver => return Err(refused("cannot be given a value in JavaScript")),
            Kind::Register { .. } => {
                return Err(refused(
                    "is a register V8 names no variable, which its debugger does not assign",
                ));
            }
            Kind::Global => return Err(refused("is the global object, which is not assigned")),
        };
        Ok(AssignedBinding {
            name: found.name.into(),
            reach,
            refusable,
        })
    }

    fn interpreted_watch(
        &self,
        stop: &dyn RuntimeStop,
        frame: InterpretedFrameRef,
        slot: u32,
        budget: &mut InspectionBudget,
    ) -> Result<InterpretedWatch, InterpretedFailure> {
        let refused = |reason: Arc<str>| {
            InterpretedFailure::Refused(crate::eval::error::ErrorKind::Unsupported, reason)
        };
        // Without the collector's entry, no watch can follow the contexts
        // a collection moves, nor tell its rewrites of a frame's slots
        // from the program's stores.
        if self.collector.is_none() || self.replacers.len() != ON_STACK_REPLACEMENT.len() {
            return Err(refused(
                "this build of Node does not name V8's collector and on-stack replacement, \
                 which a watch of a JavaScript variable follows"
                    .into(),
            ));
        }
        self.expression(stop, budget, |reader| watches::watch(reader, frame, slot))?
            .map_err(refused)
    }

    fn interpreted_frame_runs(
        &self,
        stop: &dyn RuntimeStop,
        thread: ThreadId,
        frame: VirtualAddress,
        code: VirtualAddress,
        watched: VirtualAddress,
    ) -> Result<bool, Arc<str>> {
        let _ = thread;
        let mut budget = InspectionBudget::new(crate::InspectionLimits::default());
        self.reading(stop, &mut budget, |reader| {
            watches::runs(reader, frame.get(), code.get(), watched.get())
        })
    }

    fn collector(&self) -> Option<ImageAddress> {
        self.collector
    }

    fn stack_frame_builders(&self) -> Option<Vec<AddressRange<ImageAddress>>> {
        Some(self.frame_builders.clone())
    }

    fn frame_replacers(&self) -> Vec<FrameReplacer> {
        self.replacers.clone()
    }

    fn interpreted_place(
        &self,
        stop: &dyn RuntimeStop,
        frame: VirtualAddress,
        watched: VirtualAddress,
    ) -> Result<InterpretedPlace, Arc<str>> {
        let mut budget = InspectionBudget::new(crate::InspectionLimits::default());
        self.reading(stop, &mut budget, |reader| {
            watches::place(reader, frame.get(), watched.get())?
                .ok_or_else(|| "the frame no longer reaches the watched word".into())
        })
    }

    fn interpreted_placed(
        &self,
        stop: &dyn RuntimeStop,
        frame: VirtualAddress,
        place: InterpretedPlace,
    ) -> Result<InterpretedWatch, Arc<str>> {
        let mut budget = InspectionBudget::new(crate::InspectionLimits::default());
        self.reading(stop, &mut budget, |reader| {
            watches::placed(reader, frame.get(), place)
        })
    }

    fn interpreted_watched(&self, bits: u64) -> Result<Option<InterpretedHandle>, Arc<str>> {
        Ok(Some(present::Handle::Tagged(bits).handle()))
    }

    fn interpreted_step(
        &self,
        stop: &dyn RuntimeStop,
        object: InterpretedHandle,
        taken: InterpretedStep<'_>,
        budget: &mut InspectionBudget,
    ) -> Result<InterpretedHandle, InterpretedFailure> {
        let found = self.expression(stop, budget, |reader| match taken {
            InterpretedStep::Attribute(name) => access::member(reader, object, name),
            InterpretedStep::Item(key) => access::item(reader, object, key),
        })?;
        // JavaScript reads a property missing from the whole prototype chain
        // as undefined; the debugger reports it as a missing key.
        found.ok_or_else(|| {
            InterpretedFailure::Refused(
                crate::eval::error::ErrorKind::MissingKey,
                "the value has no such property".into(),
            )
        })
    }

    fn interpreted_operand(
        &self,
        stop: &dyn RuntimeStop,
        object: InterpretedHandle,
        budget: &mut InspectionBudget,
    ) -> Result<InterpretedOperand, InterpretedFailure> {
        let writer = &self.writer;
        self.expression(stop, budget, |reader| {
            access::operand(reader, object, writer)
        })
    }

    fn interpreted_length(
        &self,
        stop: &dyn RuntimeStop,
        object: InterpretedHandle,
        budget: &mut InspectionBudget,
    ) -> Result<u64, InterpretedFailure> {
        self.expression(stop, budget, |reader| access::length(reader, object))
    }

    fn interpreted_lineage(
        &self,
        stop: &dyn RuntimeStop,
        class: InterpretedHandle,
        budget: &mut InspectionBudget,
    ) -> Result<Vec<crate::TypeIdentity>, InterpretedFailure> {
        self.expression(stop, budget, |reader| lineage::lineage(reader, class))
    }

    fn interpreted_address(&self, handle: InterpretedHandle) -> Option<VirtualAddress> {
        match present::Handle::of(handle) {
            present::Handle::Double(address) => Some(VirtualAddress::new(address)),
            present::Handle::Tagged(word) => {
                let layout = self.reading.as_ref().ok()?;
                let mask = layout.constant(crate::image::v8::Constant::HeapObjectTagMask)?;
                let tag = layout.constant(crate::image::v8::Constant::HeapObjectTag)?;
                (word & mask.cast_unsigned() == tag.cast_unsigned())
                    .then(|| VirtualAddress::new(word - tag.cast_unsigned()))
            }
        }
    }

    /// A Node-API handle, `napi_value`, points to the slot that holds the
    /// value's tagged word.
    fn interpreted_handle(
        &self,
        stop: &dyn RuntimeStop,
        address: VirtualAddress,
        budget: &mut InspectionBudget,
    ) -> Result<InterpretedHandle, InterpretedFailure> {
        self.expression(stop, budget, |reader| {
            let word = reader.word(address.get())?;
            if reader.smi(word)?.is_none() && reader.object(word)?.is_none() {
                return Err(format!("{address} holds no JavaScript value").into());
            }
            Ok(present::Handle::Tagged(word).handle())
        })
    }

    fn object_types(&self) -> Option<ObjectTypes> {
        Some(ObjectTypes {
            native: "napi_value__",
            cast: "js",
        })
    }

    fn interpreted_value(
        &self,
        stop: &dyn RuntimeStop,
        object: InterpretedHandle,
        budget: &mut InspectionBudget,
    ) -> InterpretedSlot {
        let writer = &self.writer;
        self.reading(stop, budget, |reader| {
            Ok(present::value(reader, object, writer))
        })
        .unwrap_or_else(InterpretedSlot::Unreadable)
    }
}
