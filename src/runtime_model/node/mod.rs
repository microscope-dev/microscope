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
mod objects;
mod own;
mod positions;
mod present;
mod properties;
mod scripts;
mod translation;
mod typed;
mod values;
mod variables;

use std::path::Path;
use std::sync::Arc;

use super::{
    CodeFrame, Crossing, DescribedFrame, HeldIdentity, Partial, RuntimeException, RuntimeHook,
    RuntimeImage, RuntimeModel, RuntimeSignals, RuntimeStop, StartedTask, StopMemory, StoredValue,
    TaskContext, TaskPage, TaskRef, TaskStarter, ThreadActivity, ThreadHolding, Unbound,
};
use crate::ThreadLocal;
use crate::image::v8::{DispatchTable, EmbeddedBlob, NodeRelease, V8Layout, V8Version};
use crate::inspection::InspectionBudget;
use crate::runtime_model::{
    InterpretedChild, InterpretedFailure, InterpretedHandle, InterpretedLocal, InterpretedName,
    InterpretedOperand, InterpretedSlot, InterpretedStep, ObjectTypes,
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

/// The extensions of the scripts Node runs, whose breakpoints its
/// inspector arms.
pub const SCRIPT_EXTENSIONS: &[&str] = &["js", "mjs", "cjs", "jsx", "ts", "mts", "cts", "tsx"];

/// `node::inspector::InspectorIoDelegate::GetTargetIds()`, which Node calls
/// to list its inspector's targets (F29).
const GET_TARGET_IDS: &str = "_ZN4node9inspector19InspectorIoDelegate12GetTargetIdsB5cxx11Ev";

/// What V8 names each of its embedded builtins' code.
const BUILTIN_PREFIX: &str = "Builtins_";

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
    };
    Some(Ok(Arc::new(NodeRuntime {
        version: runtime.version,
        reading,
        builtins: builtins(image.as_ref()),
        blob: runtime.embedded_blob.ok(),
        writer: writer(runtime.version, runtime.node),
        statics,
        table: std::sync::Mutex::default(),
        inspector_hook: image.symbol(GET_TARGET_IDS).map(|symbol| symbol.address),
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

    fn tasks(
        &self,
        _stop: &dyn RuntimeStop,
        _start: u64,
        _limit: usize,
        _program_only: bool,
    ) -> Partial<TaskPage> {
        Partial {
            value: TaskPage {
                tasks: Vec::new(),
                next: None,
            },
            gaps: Vec::new(),
        }
    }

    fn thread_activity(&self, _stop: &dyn RuntimeStop, _thread: ThreadId) -> ThreadActivity {
        ThreadActivity::Outside
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

    fn task_context(
        &self,
        _stop: &dyn RuntimeStop,
        _task: TaskRef,
    ) -> Result<Option<TaskContext>, Arc<str>> {
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

    fn inspector_hook(&self) -> Option<ImageAddress> {
        self.inspector_hook
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
        "handle"
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
        frame: VirtualAddress,
        budget: &mut InspectionBudget,
    ) -> Result<Vec<InterpretedLocal>, Arc<str>> {
        let writer = &self.writer;
        self.reading(stop, budget, |reader| {
            Ok(variables::variables(reader, frame.get())?
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
        frame: VirtualAddress,
        name: &str,
        budget: &mut InspectionBudget,
    ) -> Result<Option<InterpretedName>, InterpretedFailure> {
        self.expression(stop, budget, |reader| {
            Ok(
                access::bind(reader, frame.get(), name)?.map(|bound| match bound {
                    access::Bound::Variable(index) => InterpretedName::Variable(index),
                    access::Bound::Value(handle) => InterpretedName::Object(handle),
                }),
            )
        })
    }

    fn interpreted_variable(
        &self,
        stop: &dyn RuntimeStop,
        frame: VirtualAddress,
        slot: u32,
        budget: &mut InspectionBudget,
    ) -> Result<InterpretedHandle, InterpretedFailure> {
        self.expression(stop, budget, |reader| {
            access::variable(reader, frame.get(), slot)
        })
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
