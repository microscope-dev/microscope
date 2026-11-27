//! Binary and runtime debug-information loaders.
//!
//! Translates ELF, DWARF, split/separate debug files, language runtime tables,
//! and JIT descriptors into a uniform `DebugInfo` bundle of indexed metadata,
//! stack unwinding, and variable inspection.

#[cfg(not(target_os = "linux"))]
compile_error!("microscope currently supports debug information only on Linux");

#[cfg(target_os = "linux")]
mod compressed;
pub mod coroutines;
#[cfg(target_os = "linux")]
mod cpython;

#[cfg(test)]
pub fn generated_python_layout(name: &str) -> Option<crate::image::python::PythonLayout> {
    cpython::generated(name)
}

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
mod dispatch;
#[cfg(target_os = "linux")]
mod dwarf;
#[cfg(target_os = "linux")]
mod elf;
pub mod gdb_jit;
#[cfg(target_os = "linux")]
pub mod gopclntab;
#[allow(dead_code, reason = "until frames and breakpoints read source maps")]
pub mod js_source_map;
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
pub mod perf_map;
mod roles;
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
pub mod runtime_code;
#[cfg(target_os = "linux")]
mod separate;
pub mod v8;
mod x86_64;

#[cfg(feature = "fuzzing")]
pub fn fuzz_dwarf_expression(data: &[u8]) {
    dwarf::fuzz_expression(data);
}

#[cfg(all(feature = "fuzzing", target_arch = "x86_64"))]
mod jit_fuzz;
#[cfg(all(feature = "fuzzing", target_arch = "x86_64"))]
pub use jit_fuzz::fuzz as fuzz_jit_code;

#[cfg(all(feature = "fuzzing", feature = "tools"))]
pub fn fuzz_dwarf_load(data: &[u8]) {
    dwarf::fuzz_load(data);
}

#[cfg(feature = "fuzzing")]
pub fn fuzz_elf_symbols(data: &[u8]) {
    elf::fuzz(data);
}

#[cfg(all(feature = "fuzzing", target_arch = "x86_64"))]
pub fn fuzz_dispatch(data: &[u8]) {
    dispatch::fuzz(data);
}

#[cfg(feature = "fuzzing")]
pub fn fuzz_gopclntab(data: &[u8]) {
    gopclntab::fuzz(data);
}

#[cfg(feature = "fuzzing")]
pub fn fuzz_js_source_map(data: &[u8]) {
    js_source_map::fuzz(data);
}

use std::path::Path;
use std::sync::Arc;

use crate::inspection::InspectionBudget;
use crate::model::ValueStorage;
use crate::unwind::{DwarfRegister, FramePointerRule, MemoryReader, RegisterFile, UnwindStep};
use crate::{
    CodeInstanceId, DereferenceReference, DereferencedValue, GlobalVariableId, ImageAddress,
    InspectedValue, ModuleId, ModuleImage, ModuleImageId, RegisterDescriptor, Result, StackFrameId,
    StopId, TypeId, UnwindTermination, ValueChildPage, ValueChildrenReference, Variable,
    VariableQuery, VariableState, VariableUnavailableReason, VirtualAddress,
};

#[derive(Debug, Clone, Copy)]
pub struct VariableContext {
    pub stop_id: StopId,
    pub context: crate::ExecutionContext,

    pub frame: StackFrameId,
    pub module: ModuleId,
    pub image: ModuleImageId,
    pub address: Option<ImageAddress>,
}

pub struct VariableRegister {
    pub descriptor: RegisterDescriptor,
    pub bytes: Arc<[u8]>,
}

/// Lifetime and address-stability classification for a variable's backing storage.
///
/// Used by watchpoints to decide whether an object's address stays valid
/// across instructions or moves with the stack.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StorageClass {
    NotMemory,

    Static,

    ThreadLocal,

    Frame {
        stable: bool,

        /// The language runtime may move the stack holding the object.
        moving_stack: bool,
    },

    Indirect,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObjectStorage {
    pub class: StorageClass,
    pub ranges: Arc<[crate::AddressRange<ImageAddress>]>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ObjectKey(u32);

#[derive(Debug, Clone)]
pub struct Located {
    pub ty: TypeId,
    pub storage: ValueStorage,
}

#[derive(Debug, Clone)]
pub struct ReturnedValue {
    pub name: Arc<str>,

    pub ty: Option<TypeId>,

    pub value: Accessed,

    pub unresolved_shape: Option<crate::ShapeUnresolvedReason>,
}

pub type Accessed = std::result::Result<Located, VariableState>;

#[derive(Debug, Clone, Copy)]
pub enum Step<'a> {
    Deref,

    Member(&'a str),

    Index { available: usize },

    Base(BaseTarget<'a>),
}

#[derive(Clone, Copy)]
pub struct BaseTarget<'a> {
    pub name: &'a str,
    pub is_target: &'a dyn Fn(TypeId) -> bool,
}

impl std::fmt::Debug for BaseTarget<'_> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("BaseTarget")
            .field("name", &self.name)
            .finish_non_exhaustive()
    }
}

#[derive(Clone)]
pub struct PlannedStep {
    steps: Vec<dwarf::PathStep>,

    consumed: usize,

    result: Option<TypeId>,
}

impl PlannedStep {
    pub const fn consumed(&self) -> usize {
        self.consumed
    }

    pub const fn result(&self) -> Option<TypeId> {
        self.result
    }

    pub fn check_indices(&self, indices: &[i128]) -> Result<()> {
        self.steps
            .iter()
            .try_for_each(|step| dwarf::array_byte_offset(step, indices).map(drop))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TextLocation {
    pub address: VirtualAddress,

    pub length: Option<u64>,
}

#[derive(Clone)]
pub struct DebugInfo {
    pub image: Arc<ModuleImage>,
    pub unwind: Arc<dyn UnwindInfo>,
    pub variables: Arc<dyn VariableInfo>,
}

#[derive(Clone)]
pub enum VariableRuntimeError {
    Unavailable(VariableUnavailableReason),
    Malformed(Arc<str>),
    Fatal(Arc<str>),
}

impl From<VariableUnavailableReason> for VariableRuntimeError {
    fn from(reason: VariableUnavailableReason) -> Self {
        Self::Unavailable(reason)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryParameter {
    Register(DwarfRegister),

    Referent(DwarfRegister),

    Parameter(u64),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallSite {
    pub id: CallSiteId,
    pub target: CallTarget,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct CallSiteId(pub(crate) usize);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TailCallChain {
    pub links: Arc<[CallSiteId]>,

    pub functions: Arc<[Option<Arc<str>>]>,

    pub jumps: Arc<[Option<TailJump>]>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TailJump {
    pub instruction: ImageAddress,

    pub lookup: ImageAddress,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CallTarget {
    Code(ImageAddress),

    Symbol(Arc<str>),

    Computed(VirtualAddress),

    Unknown,
}

pub trait VariableRuntime {
    fn register(
        &mut self,
        register: DwarfRegister,
    ) -> std::result::Result<VariableRegister, VariableRuntimeError>;

    fn call_frame_cfa(&self) -> std::result::Result<VirtualAddress, VariableRuntimeError>;

    fn tls_address(
        &mut self,
        offset: u64,
    ) -> std::result::Result<VirtualAddress, VariableUnavailableReason>;

    fn relocate(&self, address: ImageAddress) -> std::result::Result<VirtualAddress, Arc<str>>;

    fn image_address(&self, address: VirtualAddress) -> Option<ImageAddress>;

    fn read_memory(
        &mut self,
        address: VirtualAddress,
        size: usize,
    ) -> std::result::Result<Arc<[u8]>, VariableRuntimeError>;

    fn entry_value(
        &mut self,
        parameter: EntryParameter,
        budget: &mut InspectionBudget,
    ) -> std::result::Result<u64, VariableRuntimeError>;

    fn function_at(&self, address: VirtualAddress) -> Option<Arc<str>> {
        let _ = address;
        None
    }
}

pub trait VariableInfo: Send + Sync {
    fn inspect(
        &self,
        address: ImageAddress,
        selected: Option<CodeInstanceId>,
        query: &VariableQuery,
        context: VariableContext,
        runtime: &mut dyn VariableRuntime,
        budget: &mut InspectionBudget,
    ) -> Result<Vec<Variable>>;

    fn visible_object(
        &self,
        address: ImageAddress,
        selected: Option<CodeInstanceId>,
        name: &str,
    ) -> Result<ObjectKey>;

    fn captured_future(
        &self,
        address: ImageAddress,
        selected: Option<CodeInstanceId>,
        runtime: &mut dyn VariableRuntime,
        budget: &mut InspectionBudget,
    ) -> Option<VirtualAddress>;

    fn global_object(&self, id: GlobalVariableId) -> Result<ObjectKey>;

    fn object_type(&self, object: ObjectKey) -> std::result::Result<TypeId, Arc<str>>;

    fn plan_step(&self, from: TypeId, step: Step<'_>) -> Result<PlannedStep>;

    fn locate(
        &self,
        object: ObjectKey,
        address: Option<ImageAddress>,
        runtime: &mut dyn VariableRuntime,
        budget: &mut InspectionBudget,
    ) -> Result<Accessed>;

    fn apply(
        &self,
        from: &Located,
        step: &PlannedStep,
        indices: &[i128],
        address: Option<ImageAddress>,
        runtime: &mut dyn VariableRuntime,
        budget: &mut InspectionBudget,
    ) -> Result<Accessed>;

    fn load(
        &self,
        at: &Located,
        context: VariableContext,
        runtime: &mut dyn VariableRuntime,
        budget: &mut InspectionBudget,
    ) -> Result<std::result::Result<crate::VariableValue, VariableState>>;

    fn text_span(
        &self,
        at: &Located,
        context: VariableContext,
        runtime: &mut dyn VariableRuntime,
        budget: &mut InspectionBudget,
    ) -> Result<std::result::Result<Option<TextLocation>, VariableState>> {
        let _ = (at, context, runtime, budget);
        Ok(Ok(None))
    }

    fn materialize(
        &self,
        at: &Located,
        context: VariableContext,
        runtime: &mut dyn VariableRuntime,
        budget: &mut InspectionBudget,
    ) -> Result<InspectedValue>;

    fn object_storage(&self, object: ObjectKey) -> ObjectStorage;

    fn inspect_global(
        &self,
        id: GlobalVariableId,
        address: Option<ImageAddress>,
        context: VariableContext,
        runtime: &mut dyn VariableRuntime,
        budget: &mut InspectionBudget,
    ) -> Result<Variable>;

    fn returned(
        &self,
        function: ImageAddress,
        runtime: &mut dyn VariableRuntime,
        budget: &mut InspectionBudget,
    ) -> Result<Option<Vec<ReturnedValue>>> {
        let _ = (function, runtime, budget);
        Ok(None)
    }

    fn dereference(
        &self,
        reference: &DereferenceReference,
        runtime: &mut dyn VariableRuntime,
        budget: &mut InspectionBudget,
    ) -> Result<DereferencedValue>;

    fn value_children(
        &self,
        reference: &ValueChildrenReference,
        offset: u64,
        limit: u32,
        runtime: &mut dyn VariableRuntime,
        budget: &mut InspectionBudget,
    ) -> Result<ValueChildPage>;

    fn may_describe_call_site(&self, return_address: ImageAddress) -> bool {
        let _ = return_address;
        true
    }

    fn call_site(
        &self,
        return_address: ImageAddress,
        runtime: &mut dyn VariableRuntime,
        budget: &mut InspectionBudget,
    ) -> std::result::Result<Option<CallSite>, VariableRuntimeError>;

    fn tail_calls(
        &self,
        from: ImageAddress,
        to: ImageAddress,
    ) -> std::result::Result<TailCallChain, VariableRuntimeError>;

    fn call_site_value(
        &self,
        site: CallSiteId,
        parameter: EntryParameter,
        runtime: &mut dyn VariableRuntime,
        budget: &mut InspectionBudget,
    ) -> std::result::Result<u64, VariableRuntimeError>;
}

pub trait UnwindInfo: Send + Sync {
    fn cfa(
        &self,
        address: ImageAddress,
        registers: &RegisterFile,
        memory: &mut dyn MemoryReader,
    ) -> std::result::Result<VirtualAddress, UnwindTermination>;

    fn unwind(
        &self,
        address: ImageAddress,
        registers: &RegisterFile,
        memory: &mut dyn MemoryReader,
    ) -> std::result::Result<UnwindStep, UnwindTermination>;

    fn frame_pointer_rule(&self, address: ImageAddress) -> FramePointerRule {
        crate::unwind::frame_pointer_rule(self, address)
    }

    fn task_register(&self, _address: ImageAddress) -> Option<DwarfRegister> {
        None
    }
}

#[cfg(target_os = "linux")]
pub use separate::DebugFileSearch;

#[cfg(any(test, feature = "sim"))]
pub fn load_bytes(path: &Path, data: &[u8]) -> Result<DebugInfo> {
    load_program(path, data, &DebugFileSearch::default())
}

pub fn load_program(path: &Path, data: &[u8], search: &DebugFileSearch) -> Result<DebugInfo> {
    let info = dwarf::load_bytes(path, data, crate::ModuleImageId::new(0), search)?;
    check_architecture(&info)?;
    Ok(info)
}

pub fn check_architecture(info: &DebugInfo) -> Result<()> {
    let architecture = info.image.target().architecture;
    if architecture != HOST_ARCHITECTURE {
        return Err(crate::Error::UnsupportedFile(
            format!(
                "it is a program for {}, and this debugger debugs {} programs",
                architecture_name(architecture),
                architecture_name(HOST_ARCHITECTURE)
            )
            .into(),
        ));
    }

    Ok(())
}

#[cfg(target_arch = "x86_64")]
const HOST_ARCHITECTURE: crate::Architecture = crate::Architecture::X86_64;

#[cfg(target_arch = "aarch64")]
const HOST_ARCHITECTURE: crate::Architecture = crate::Architecture::Aarch64;

const fn architecture_name(architecture: crate::Architecture) -> &'static str {
    match architecture {
        crate::Architecture::X86_64 => "x86-64",
        crate::Architecture::Aarch64 => "AArch64",
    }
}

#[expect(
    clippy::redundant_pub_crate,
    reason = "the private debug-info edge is shared by sibling backend modules"
)]
pub(crate) fn load_module(
    path: &Path,
    id: crate::ModuleImageId,
    search: &DebugFileSearch,
) -> Result<DebugInfo> {
    dwarf::load(path, id, search)
}

#[expect(
    clippy::redundant_pub_crate,
    reason = "the private debug-info edge is shared by sibling backend modules"
)]
pub(crate) fn load_modules(
    modules: &[(std::path::PathBuf, crate::ModuleImageId)],
    search: &DebugFileSearch,
) -> Vec<Result<DebugInfo>> {
    use rayon::prelude::*;
    let _batch = crate::span!("load_modules", "{} modules", modules.len());
    crate::pool::install(|| {
        modules
            .par_iter()
            .map(|(path, id)| load_module(path, *id, search))
            .collect()
    })
    .unwrap_or_else(|error| {
        modules
            .iter()
            .map(|_| Err(crate::Error::debug_info(error.clone())))
            .collect()
    })
}

#[expect(
    clippy::redundant_pub_crate,
    reason = "the private debug-info edge is shared by sibling backend modules"
)]
pub(crate) fn load_module_bytes(
    path: &Path,
    data: &[u8],
    id: crate::ModuleImageId,
    search: &DebugFileSearch,
) -> Result<DebugInfo> {
    dwarf::load_bytes(path, data, id, search)
}

#[expect(clippy::redundant_pub_crate, reason = "shared with the recorder")]
pub(crate) fn load_profile_module_bytes(
    path: &Path,
    data: &[u8],
    id: crate::ModuleImageId,
    search: &DebugFileSearch,
) -> Result<(DebugInfo, bool)> {
    dwarf::load_profile_bytes(path, data, id, search)
}

pub fn python_runtime(path: &Path) -> Option<crate::image::python::PythonRuntime> {
    let file = object::ReadCache::new(std::fs::File::open(path).ok()?);
    cpython::runtime(&object::File::parse(&file).ok()?)
}

#[expect(
    clippy::redundant_pub_crate,
    reason = "the private debug-info edge is shared by sibling backend modules"
)]
pub(crate) fn load_transient_bytes(
    path: &Path,
    data: &[u8],
    id: crate::ModuleImageId,
) -> Result<DebugInfo> {
    dwarf::load_transient_bytes(path, data, id)
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use crate::ModuleImageId;

    #[test]
    fn a_batch_answers_each_module_in_its_place() {
        let fixture = |name: &str| {
            PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("build/test-programs")
                .join(name)
        };

        let modules = [
            (fixture("containers-rust-o2"), ModuleImageId::new(4)),
            (fixture("no-such-program"), ModuleImageId::new(5)),
            (fixture("basic"), ModuleImageId::new(6)),
        ];
        let loads = super::load_modules(&modules, &super::DebugFileSearch::default());
        assert_eq!(loads.len(), modules.len());

        for ((path, id), load) in modules.iter().zip(&loads) {
            if path.ends_with("no-such-program") {
                assert!(load.is_err());
                continue;
            }

            let image = &load.as_ref().expect("run `just build-test-programs`").image;
            assert_eq!((image.path(), image.id()), (path.as_path(), *id));
        }
    }
}
