//! Node.js, as the V8 runtime that runs a program's JavaScript. The module
//! that carries V8, the `node` executable or a shared `libnode`, has the
//! layout its loader read and checked (`crate::image::v8`); this model reads
//! V8's frames through it at a stop.
//!
//! V8's frames are native frames: its builtins and the code it compiles
//! for JavaScript run on the thread's own stack, and the unwinder walks
//! them by V8's frame-pointer convention. The model names each such frame
//! as V8 recorded it ([`RuntimeModel::code_frame`]).

mod compiled;
mod frames;
mod names;
mod objects;
mod positions;

use std::path::Path;
use std::sync::Arc;

use super::ImageSymbol;
use super::{
    CodeFrame, Crossing, DescribedFrame, Partial, RuntimeException, RuntimeHook, RuntimeImage,
    RuntimeModel, RuntimeSignals, RuntimeStop, StartedTask, StoredValue, TaskContext, TaskPage,
    TaskRef, TaskStarter, ThreadActivity, Unbound,
};
use crate::ThreadLocal;
use crate::image::v8::{DispatchTable, V8Layout, V8Version};
use crate::unwind::RegisterFile;
use crate::{AddressRange, ImageAddress, StackSegment, ThreadId, VirtualAddress};

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
        bytecode_sizes: image.symbol(BYTECODE_SIZES),
    };
    Some(Ok(Arc::new(NodeRuntime {
        version: runtime.version,
        reading,
        builtins: builtins(image.as_ref()),
        statics,
    })))
}

/// `v8::internal::interpreter::Bytecodes::kBytecodeSizes`, each bytecode's
/// size at each operand scale, which only a symbol table names.
const BYTECODE_SIZES: &str = "_ZN2v88internal11interpreter9Bytecodes14kBytecodeSizesE";

/// What reading V8's compiled code needs of the module that carries it.
#[derive(Debug)]
struct Statics {
    /// Where each thread keeps its current isolate.
    isolate: Option<Result<ThreadLocal, Arc<str>>>,
    dispatch_table: Result<Option<DispatchTable>, Arc<str>>,
    bytecode_sizes: Option<ImageSymbol>,
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

#[derive(Debug)]
struct NodeRuntime {
    version: V8Version,

    /// The layout V8's frames and objects are read by, or why none can be.
    reading: Result<V8Layout, Arc<str>>,
    builtins: Option<AddressRange<ImageAddress>>,
    statics: Statics,
}

impl RuntimeModel for NodeRuntime {
    fn owns_code(&self, address: ImageAddress) -> bool {
        self.builtins
            .is_some_and(|range| range.start <= address && address < range.end)
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
        };
        frames::describe(
            &reader,
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

    fn own_source(&self, path: &Path) -> bool {
        path.to_str().is_some_and(|path| path.starts_with("node:"))
    }
}
