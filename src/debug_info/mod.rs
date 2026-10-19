#[cfg(not(target_os = "linux"))]
compile_error!("microscope currently supports debug information only on Linux");

pub mod coroutines;
#[cfg(target_os = "linux")]
mod cpython;

/// The CPython layout generated for the interpreter the matrix names
/// `name`, as `3.14t`.
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
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
pub mod perf_map;
mod roles;
#[cfg(target_os = "linux")]
mod separate;
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
mod x86_64;

#[cfg(feature = "fuzzing")]
pub fn fuzz_dwarf_expression(data: &[u8]) {
    dwarf::fuzz_expression(data);
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

    /// The backtrace frame whose registers and call-frame address evaluate
    /// the values, which every capability they produce keeps.
    pub frame: StackFrameId,
    pub module: ModuleId,
    pub image: ModuleImageId,
    pub address: Option<ImageAddress>,
}

pub struct VariableRegister {
    pub descriptor: RegisterDescriptor,
    pub bytes: Arc<[u8]>,
}

/// Where a data object's storage lives, which bounds how long its address
/// identifies it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StorageClass {
    /// The value is not held in addressable memory (a constant, a register,
    /// a computed value, or no location at all).
    NotMemory,

    /// Static storage at a module-relative address.
    Static,

    /// One thread's instance of thread-local storage.
    ThreadLocal,

    /// Storage addressed relative to a function activation.
    Frame {
        /// One location expression applies throughout the object's scope, so
        /// its address cannot move while the activation lives.
        stable: bool,

        /// The language runtime may move the stack holding the object.
        moving_stack: bool,
    },

    /// The address is computed from memory read during evaluation.
    Indirect,
}

/// The storage of one data object and the instructions where it is in scope.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObjectStorage {
    pub class: StorageClass,
    pub ranges: Arc<[crate::AddressRange<ImageAddress>]>,
}

/// One data object an image catalogs: a local, a parameter, or a global.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ObjectKey(u32);

/// A typed value's storage in one image, found at one stop.
#[derive(Debug, Clone)]
pub struct Located {
    pub ty: TypeId,
    pub storage: ValueStorage,
}

/// One value a function returned, captured as the call to it returned.
#[derive(Debug, Clone)]
pub struct ReturnedValue {
    /// The result's name in the function, such as Go's `~r0` for one the
    /// source leaves unnamed.
    pub name: Arc<str>,

    /// Its type, unless its debug information is malformed.
    pub ty: Option<TypeId>,

    /// Its value as the call returned it, or why that cannot be known.
    pub value: Accessed,

    /// Why a generic result has its shape's type rather than its own.
    pub unresolved_shape: Option<crate::ShapeUnresolvedReason>,
}

/// Storage reached, or the unavailable or malformed state that stopped it.
pub type Accessed = std::result::Result<Located, VariableState>;

/// One structural step from a value to another.
#[derive(Debug, Clone, Copy)]
pub enum Step<'a> {
    /// Through a pointer or reference.
    Deref,

    /// To a member, through any pointers to the record holding it.
    Member(&'a str),

    /// To an element of an array or slice, holding `available` index values,
    /// of which an array takes one per dimension and a slice one.
    Index { available: usize },

    /// To the one base class subobject of a record whose type is a target.
    Base(BaseTarget<'a>),
}

/// Which types a [`Step::Base`] reaches: those its caller deems the same
/// type, as one type defined in several units is.
#[derive(Clone, Copy)]
pub struct BaseTarget<'a> {
    /// The base's name, for errors.
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

/// A step planned from types alone, which [`VariableInfo::apply`] follows at
/// a stop.
#[derive(Clone)]
pub struct PlannedStep {
    steps: Vec<dwarf::PathStep>,

    /// How many index values the step takes.
    consumed: usize,

    /// The type the step reaches, unless planning found it unavailable.
    result: Option<TypeId>,
}

impl PlannedStep {
    pub const fn consumed(&self) -> usize {
        self.consumed
    }

    pub const fn result(&self) -> Option<TypeId> {
        self.result
    }

    /// Checks index values against an array's static bounds, which needs no
    /// program state, so that a bad index is an error before any storage is
    /// read.
    pub fn check_indices(&self, indices: &[i128]) -> Result<()> {
        self.steps
            .iter()
            .try_for_each(|step| dwarf::array_byte_offset(step, indices).map(drop))
    }
}

/// Where a string's bytes are.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TextLocation {
    /// The first byte's address.
    pub address: VirtualAddress,

    /// How many bytes there are, when the string records it rather than
    /// ending at a NUL.
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

/// What a frame's location asks of the call that entered its function.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryParameter {
    /// The value a register held on entry.
    Register(DwarfRegister),

    /// The value stored, on entry, at the address a register held.
    Referent(DwarfRegister),

    /// The value passed for the parameter whose debug information entry
    /// is at this offset in the frame's module.
    Parameter(u64),
}

/// A described call, which returns to the address that found it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallSite {
    pub id: CallSiteId,
    pub target: CallTarget,
}

/// One call site that a module describes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct CallSiteId(pub(crate) usize);

/// The tail calls between the function a call entered and a frame's.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TailCallChain {
    /// The tail calls, in the order they ran.
    pub links: Arc<[CallSiteId]>,

    /// The linker name of each function entered: the call's target, then
    /// each link's. Another module may define one under the same name.
    pub functions: Arc<[Option<Arc<str>>]>,

    /// Where each link jumped from, when its call site says.
    pub jumps: Arc<[Option<TailJump>]>,
}

/// Where a tail call jumped from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TailJump {
    /// The jump's address, or the one after it where only that is
    /// described.
    pub instruction: ImageAddress,

    /// An address within the jump, whose code and source are the jumping
    /// function's.
    pub lookup: ImageAddress,
}

/// The function a call site calls.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CallTarget {
    /// Code in the same module.
    Code(ImageAddress),

    /// The function a linker symbol of this name defines, in any module.
    Symbol(Arc<str>),

    /// An address the call computed.
    Computed(VirtualAddress),

    /// The debug information does not say.
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

    /// Where `address` is in this module's image, or `None` when the
    /// module does not map it, such as code another module holds.
    fn image_address(&self, address: VirtualAddress) -> Option<ImageAddress>;

    fn read_memory(
        &mut self,
        address: VirtualAddress,
        size: usize,
    ) -> std::result::Result<Arc<[u8]>, VariableRuntimeError>;

    /// The register-sized value `parameter` names on entry to the frame's
    /// function, which its caller's call site describes.
    fn entry_value(
        &mut self,
        parameter: EntryParameter,
        budget: &mut InspectionBudget,
    ) -> std::result::Result<u64, VariableRuntimeError>;

    /// The name of the function whose first instruction `address` is, in
    /// whichever loaded module holds it.
    fn function_at(&self, address: VirtualAddress) -> Option<Arc<str>> {
        let _ = address;
        None
    }
}

pub trait VariableInfo: Send + Sync {
    /// Inspects the parameters and local variables visible in one selected logical
    /// frame. `selected` names the presented inline instance, or `None` for
    /// the physical frame; data objects belonging to other logical frames at the
    /// same address are out of scope.
    fn inspect(
        &self,
        address: ImageAddress,
        selected: Option<CodeInstanceId>,
        query: &VariableQuery,
        context: VariableContext,
        runtime: &mut dyn VariableRuntime,
        budget: &mut InspectionBudget,
    ) -> Result<Vec<Variable>>;

    /// The local or parameter `name` names in the selected logical frame,
    /// whose innermost declaration hides the others.
    fn visible_object(
        &self,
        address: ImageAddress,
        selected: Option<CodeInstanceId>,
        name: &str,
    ) -> Result<ObjectKey>;

    /// The data object of one cataloged global.
    fn global_object(&self, id: GlobalVariableId) -> Result<ObjectKey>;

    /// The object's type, or why its type is malformed.
    fn object_type(&self, object: ObjectKey) -> std::result::Result<TypeId, Arc<str>>;

    /// Plans one step from a value of type `from`, reading no program state.
    fn plan_step(&self, from: TypeId, step: Step<'_>) -> Result<PlannedStep>;

    /// Finds one data object's storage at `address`, the frame's
    /// module-relative instruction, if any.
    fn locate(
        &self,
        object: ObjectKey,
        address: Option<ImageAddress>,
        runtime: &mut dyn VariableRuntime,
        budget: &mut InspectionBudget,
    ) -> Result<Accessed>;

    /// Follows a planned step from `from`, with `indices` as its index
    /// values. An index outside an array's static bounds is an error.
    fn apply(
        &self,
        from: &Located,
        step: &PlannedStep,
        indices: &[i128],
        address: Option<ImageAddress>,
        runtime: &mut dyn VariableRuntime,
        budget: &mut InspectionBudget,
    ) -> Result<Accessed>;

    /// Decodes the scalar stored at `at`, without the text it may point to.
    fn load(
        &self,
        at: &Located,
        context: VariableContext,
        runtime: &mut dyn VariableRuntime,
        budget: &mut InspectionBudget,
    ) -> Result<std::result::Result<crate::VariableValue, VariableState>>;

    /// Where the bytes of the text stored at `at` are, when it is a string:
    /// the first byte's address, and how many there are when the string
    /// records it rather than ending at a NUL.
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

    /// Decodes the value stored at `at`, with its children and dereference
    /// capabilities.
    fn materialize(
        &self,
        at: &Located,
        context: VariableContext,
        runtime: &mut dyn VariableRuntime,
        budget: &mut InspectionBudget,
    ) -> Result<InspectedValue>;

    /// Classifies where one data object's storage lives.
    fn object_storage(&self, object: ObjectKey) -> ObjectStorage;

    /// Evaluates one cataloged global at the selected thread's current stop.
    /// `address` is the module-relative program counter, `None` outside this
    /// module, where a range-gated location is unavailable.
    fn inspect_global(
        &self,
        id: GlobalVariableId,
        address: Option<ImageAddress>,
        context: VariableContext,
        runtime: &mut dyn VariableRuntime,
        budget: &mut InspectionBudget,
    ) -> Result<Variable>;

    /// The values the function whose code holds `function` returned, read
    /// through `runtime` the instant a call to it has returned, with the
    /// caller's registers and stack as the return leaves them; captured, so
    /// later execution cannot change them. `None` when the function's
    /// convention for returning values is not one the provider knows.
    fn returned(
        &self,
        function: ImageAddress,
        runtime: &mut dyn VariableRuntime,
        budget: &mut InspectionBudget,
    ) -> Result<Option<Vec<ReturnedValue>>> {
        let _ = (function, runtime, budget);
        Ok(None)
    }

    /// Dereferences one stop-scoped capability produced by this image.
    fn dereference(
        &self,
        reference: &DereferenceReference,
        runtime: &mut dyn VariableRuntime,
        budget: &mut InspectionBudget,
    ) -> Result<DereferencedValue>;

    /// Evaluates one arbitrary bounded interval from a stop-scoped aggregate
    /// capability.
    fn value_children(
        &self,
        reference: &ValueChildrenReference,
        offset: u64,
        limit: u32,
        runtime: &mut dyn VariableRuntime,
        budget: &mut InspectionBudget,
    ) -> Result<ValueChildPage>;

    /// Whether a call that returns to `return_address` may be described,
    /// which is cheaper to ask than [`Self::call_site`]: when this is
    /// false, that finds none.
    fn may_describe_call_site(&self, return_address: ImageAddress) -> bool {
        let _ = return_address;
        true
    }

    /// The call that returns to `return_address`, when one is described,
    /// with its target computed in the calling frame `runtime` reads.
    fn call_site(
        &self,
        return_address: ImageAddress,
        runtime: &mut dyn VariableRuntime,
        budget: &mut InspectionBudget,
    ) -> std::result::Result<Option<CallSite>, VariableRuntimeError>;

    /// The tail calls by which the function whose code holds `from`, entered
    /// by a call, became the activation of the function whose code holds
    /// `to`: none when they are the same function. Fails unless exactly one
    /// chain of tail calls is possible within this module.
    fn tail_calls(
        &self,
        from: ImageAddress,
        to: ImageAddress,
    ) -> std::result::Result<TailCallChain, VariableRuntimeError>;

    /// The register-sized value a call site passed for `parameter`, in the
    /// state of the calling frame `runtime` reads.
    fn call_site_value(
        &self,
        site: CallSiteId,
        parameter: EntryParameter,
        runtime: &mut dyn VariableRuntime,
        budget: &mut InspectionBudget,
    ) -> std::result::Result<u64, VariableRuntimeError>;
}

/// Call-frame information for one module image. Instruction addresses are
/// image addresses; registers and memory belong to the stopped process.
pub trait UnwindInfo: Send + Sync {
    /// Computes the canonical frame address of the frame executing `address`.
    fn cfa(
        &self,
        address: ImageAddress,
        registers: &RegisterFile,
        memory: &mut dyn MemoryReader,
    ) -> std::result::Result<VirtualAddress, UnwindTermination>;

    /// Reconstructs the caller of the frame executing `address`.
    fn unwind(
        &self,
        address: ImageAddress,
        registers: &RegisterFile,
        memory: &mut dyn MemoryReader,
    ) -> std::result::Result<UnwindStep, UnwindTermination>;

    /// Whether the frame executing `address` keeps the frame-pointer
    /// chain there, so that a walk of the chain finds its caller.
    fn frame_pointer_rule(&self, address: ImageAddress) -> FramePointerRule {
        crate::unwind::frame_pointer_rule(self, address)
    }

    /// The register, by its DWARF number, in which the code at `address`
    /// keeps its runtime's current task: Go's compiled code keeps the
    /// goroutine's `g` in R14. Other code keeps none.
    fn task_register(&self, _address: ImageAddress) -> Option<DwarfRegister> {
        None
    }
}

#[cfg(target_os = "linux")]
pub use separate::DebugFileSearch;

/// Loads a program's debug information from its own file alone.
#[cfg(any(test, feature = "sim"))]
pub fn load_bytes(path: &Path, data: &[u8]) -> Result<DebugInfo> {
    load_program(path, data, &DebugFileSearch::default())
}

/// Loads a program's debug information, from a separate debug file that
/// `search` finds when its own file has none. A program for another
/// architecture than the debugger's own cannot be debugged.
pub fn load_program(path: &Path, data: &[u8], search: &DebugFileSearch) -> Result<DebugInfo> {
    let info = dwarf::load_bytes(path, data, crate::ModuleImageId::new(0), search)?;
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

    Ok(info)
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

/// Loads module files at once on the loader's workers, each result where
/// its module is in `modules`, whatever order the loads finish in.
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

/// Loads an image a process holds only in memory for a while, as a JIT
/// compiler's objects, without the cache.
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

    /// A batch answers each module where it was asked for, whichever load
    /// finishes first, and one module that does not load fails alone.
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
