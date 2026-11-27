use std::fmt;
use std::path::PathBuf;
use std::sync::Arc;

use crate::{Error, Result};

mod image;

pub use image::free;
pub use image::{Binding, SealError, seal};
pub use image::{ModuleImage, ModuleMetadata, PackageInfo, ThreadLocal};

macro_rules! address_type {
    ($name:ident, $description:literal) => {
        #[doc = $description]
        #[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub struct $name(u64);

        impl fmt::Debug for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{}({:#x})", stringify!($name), self.0)
            }
        }

        impl $name {
            #[must_use]
            pub const fn new(value: u64) -> Self {
                Self(value)
            }

            #[must_use]
            pub const fn get(self) -> u64 {
                self.0
            }

            #[must_use]
            pub const fn wrapping_add(self, bytes: u64) -> Self {
                Self(self.0.wrapping_add(bytes))
            }

            #[must_use]
            pub const fn checked_add(self, bytes: u64) -> Option<Self> {
                match self.0.checked_add(bytes) {
                    Some(address) => Some(Self(address)),
                    None => None,
                }
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{:#x}", self.0)
            }
        }

        impl fmt::LowerHex for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                fmt::LowerHex::fmt(&self.0, f)
            }
        }

        impl fmt::UpperHex for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                fmt::UpperHex::fmt(&self.0, f)
            }
        }
    };
}

address_type!(
    ImageAddress,
    "An address in the address space described by a module image."
);
address_type!(
    VirtualAddress,
    "An address in the virtual address space of a running process."
);

pub use microscope_data::model::{
    Architecture, ByteOrder, CodeRole, Pid, PointerWidth, TargetDescription, Tid,
};

/// Offset added to an [`ImageAddress`] to obtain its [`VirtualAddress`].
///
/// Stored as a wrapping `u64` difference because a PIE or relocated mapping can
/// sit either above or below its link-time address.
#[derive(Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct LoadBias(u64);

impl LoadBias {
    #[must_use]
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }

    #[must_use]
    pub const fn between(image: ImageAddress, loaded: VirtualAddress) -> Self {
        Self(loaded.get().wrapping_sub(image.get()))
    }

    #[must_use]
    pub const fn load(self, address: ImageAddress) -> Option<VirtualAddress> {
        match self.0.checked_add(address.get()) {
            Some(loaded) => Some(VirtualAddress::new(loaded)),
            None => None,
        }
    }

    #[must_use]
    pub const fn unload(self, address: VirtualAddress) -> Option<ImageAddress> {
        match address.get().checked_sub(self.0) {
            Some(image) => Some(ImageAddress::new(image)),
            None => None,
        }
    }

    #[must_use]
    pub const fn wrapping_load(self, address: ImageAddress) -> VirtualAddress {
        VirtualAddress::new(address.get().wrapping_add(self.0))
    }

    #[must_use]
    pub const fn wrapping_unload(self, address: VirtualAddress) -> ImageAddress {
        ImageAddress::new(address.get().wrapping_sub(self.0))
    }
}

impl fmt::Debug for LoadBias {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "LoadBias({:#x})", self.0)
    }
}

impl fmt::Display for LoadBias {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:#x}", self.0)
    }
}

impl fmt::LowerHex for LoadBias {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::LowerHex::fmt(&self.0, f)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct AddressRange<A> {
    pub start: A,

    pub end: A,
}

impl<A: Copy + Ord> AddressRange<A> {
    #[must_use]
    pub fn contains(self, address: A) -> bool {
        self.start <= address && address < self.end
    }

    #[must_use]
    pub fn is_empty(self) -> bool {
        self.end <= self.start
    }
}

macro_rules! id_type {
    ($name:ident, $description:literal) => {
        #[doc = $description]
        #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub struct $name(u32);

        impl $name {
            pub(crate) const fn new(value: u32) -> Self {
                Self(value)
            }

            #[must_use]
            #[allow(dead_code, reason = "a crate's own identifier may only index")]
            pub const fn get(self) -> u32 {
                self.0
            }

            #[allow(dead_code, reason = "not every identifier indexes a slice")]
            pub(crate) const fn index(self) -> usize {
                self.0 as usize
            }
        }

        impl ::std::fmt::Display for $name {
            fn fmt(&self, f: &mut ::std::fmt::Formatter<'_>) -> ::std::fmt::Result {
                self.0.fmt(f)
            }
        }
    };
}

pub(crate) use id_type;

macro_rules! numeric_id {
    ($name:ident, $description:literal) => {
        #[doc = $description]
        #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub struct $name(u64);

        impl $name {
            #[must_use]
            pub const fn new(value: u64) -> Self {
                Self(value)
            }

            #[must_use]
            pub const fn get(self) -> u64 {
                self.0
            }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                self.0.fmt(f)
            }
        }
    };
}

pub(crate) use numeric_id;

id_type!(
    ModuleImageId,
    "Identifies a module image within a debug session."
);
id_type!(
    ModuleId,
    "Identifies a loaded module within a debug session."
);
id_type!(FunctionId, "Identifies a function within a module image.");
id_type!(
    CodeInstanceId,
    "Identifies one concrete code instance within a module image."
);
id_type!(
    LineSequenceId,
    "Identifies one contiguous line-program sequence within a module image."
);
id_type!(
    SourceFileId,
    "Identifies a source file within a module image."
);
id_type!(
    SymbolId,
    "Identifies a linker symbol within a module image."
);
id_type!(
    SectionId,
    "Identifies an allocated section within a module image."
);
id_type!(
    GlobalVariableId,
    "Identifies a global variable within a module image."
);
id_type!(
    TypeId,
    "Identifies a normalized type within a module image."
);

id_type!(
    StackFrameId,
    "Identifies a frame of one thread's backtrace at one stop by its level."
);
id_type!(
    RegisterId,
    "Identifies a register within a target architecture."
);

impl StackFrameId {
    pub const INNERMOST: Self = Self(0);
}

numeric_id!(
    ThreadId,
    "Identifies a thread within a debug session by its platform value."
);

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RuntimeId(u32);

impl RuntimeId {
    #[must_use]
    pub const fn new(value: u32) -> Self {
        Self(value)
    }

    #[must_use]
    pub const fn get(self) -> u32 {
        self.0
    }
}

impl fmt::Display for RuntimeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TaskId {
    pub runtime: RuntimeId,

    pub number: u64,
}

impl std::fmt::Display for TaskId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.number.fmt(f)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TaskState {
    Running,

    Runnable,

    Blocked,

    Exited,

    Unknown(Arc<str>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskLocation {
    pub address: Option<VirtualAddress>,

    pub module: Option<ModuleId>,

    pub function: Option<Arc<str>>,
    pub source: Option<SourceLocation>,

    pub recorded: Option<RecordedPlace>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordedPlace {
    pub path: Arc<str>,
    pub line: u32,
    pub column: u32,
}

impl RecordedPlace {
    #[must_use]
    pub fn names_file(&self) -> bool {
        let bracketed = self.path.starts_with('<') && self.path.ends_with('>');
        let module_id = self.path.split_once(':').is_some_and(|(scheme, _)| {
            scheme.len() > 1 && scheme.bytes().all(|byte| byte.is_ascii_alphabetic())
        });
        !bracketed && !module_id
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskSnapshot {
    pub id: TaskId,

    pub noun: &'static str,
    pub state: TaskState,

    pub detail: Option<Arc<str>>,

    pub thread: Option<ThreadId>,

    pub resume: Option<TaskLocation>,

    pub creation: Option<TaskLocation>,

    pub entry: Option<TaskLocation>,

    pub parent: Option<TaskId>,

    pub internal: bool,

    pub labels: Arc<[(Arc<str>, Arc<str>)]>,

    pub object: Option<VirtualAddress>,

    pub stackless: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StackSegment {
    Thread,

    Task,

    System,

    Signal,

    Future,

    Unplaced,

    Awaiting,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ThreadActivity {
    Task { task: TaskId, stack: StackSegment },

    Idle,

    Outside,

    Unknown(Arc<str>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThreadLabel {
    pub runtime: RuntimeId,

    pub kind: Arc<str>,

    pub value: std::result::Result<Arc<str>, Arc<str>>,
}

impl fmt::Display for ThreadLabel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.value {
            Ok(value) => write!(f, "{} {value}", self.kind),
            Err(reason) => write!(f, "{} unknown: {reason}", self.kind),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TaskCursor {
    pub(crate) runtime: usize,
    pub(crate) position: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskPage {
    pub tasks: Arc<[TaskSnapshot]>,

    pub next: Option<TaskCursor>,

    pub gaps: Arc<[Arc<str>]>,

    pub usage: crate::InspectionUsage,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ExecutionContext {
    Thread(ThreadId),

    Task(TaskId),
}

impl ExecutionContext {
    #[must_use]
    pub const fn as_thread(self) -> Option<ThreadId> {
        match self {
            Self::Thread(thread) => Some(thread),
            Self::Task(_) => None,
        }
    }
}

impl From<ThreadId> for ExecutionContext {
    fn from(thread: ThreadId) -> Self {
        Self::Thread(thread)
    }
}

impl From<TaskId> for ExecutionContext {
    fn from(task: TaskId) -> Self {
        Self::Task(task)
    }
}

impl std::fmt::Display for ExecutionContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Thread(thread) => write!(f, "thread {thread}"),
            Self::Task(task) => write!(f, "task {task}"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum RegisterRole {
    ProgramCounter,

    StackPointer,

    FramePointer,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegisterDescriptor {
    pub id: RegisterId,

    pub name: Arc<str>,

    pub bits: u16,

    pub role: Option<RegisterRole>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegisterValue {
    pub register: RegisterDescriptor,

    pub bytes: Option<Arc<[u8]>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegisterSnapshot {
    pub revision: u64,

    pub context: ExecutionContext,

    pub target: TargetDescription,

    pub registers: Arc<[RegisterValue]>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum BaseTypeEncoding {
    Boolean,
    Signed,
    SignedCharacter,
    Unsigned,
    UnsignedCharacter,
    Floating,

    ComplexFloating,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FloatLayout {
    Binary16,
    BFloat16,
    Binary32,
    Binary64,
    X87Extended,
    Binary128,
}

impl BaseType {
    #[must_use]
    pub fn float_layout(&self) -> Option<FloatLayout> {
        if self.encoding != BaseTypeEncoding::Floating {
            return None;
        }

        let name = self.base_name.as_ref();
        Some(match (self.byte_size, name) {
            (2, "_Float16" | "__fp16" | "f16") => FloatLayout::Binary16,
            (2, "__bf16") => FloatLayout::BFloat16,
            (4, _) => FloatLayout::Binary32,
            (8, _) => FloatLayout::Binary64,
            (10 | 12 | 16, "long double" | "__float80" | "_Float64x" | "f80" | "c_longdouble") => {
                FloatLayout::X87Extended
            }
            (16, "_Float128" | "__float128" | "f128") => FloatLayout::Binary128,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct BaseType {
    pub name: Arc<str>,

    pub base_name: Arc<str>,

    pub encoding: BaseTypeEncoding,

    pub byte_size: u64,

    pub bit_size: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum IntegerValue {
    Signed(i128),
    Unsigned(u128),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum EnumerationOrigin {
    Language,

    NamedConstants,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Enumerator {
    pub name: Arc<str>,

    pub value: IntegerValue,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TypeReference {
    pub image: ModuleImageId,

    pub id: TypeId,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum TypeModifier {
    Const,

    Volatile,

    Restrict,

    Atomic,

    Immutable,

    Packed,

    Shared,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum NamedTypeRelationship {
    Synonym,

    Distinct,

    Encoding,

    Unspecified,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ReferenceKind {
    Lvalue,
    Rvalue,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum RecordKind {
    Struct,
    Class,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum VariantStorageKind {
    Struct,
    Class,
    Union,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Accessibility {
    Public,
    Protected,
    Private,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum RecordMemberLayout {
    ByteOffset(u64),

    BitRange {
        bit_offset: u64,

        /// The field width in bits.
        bit_size: u64,
    },

    Runtime,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct RecordMember {
    pub name: Option<Arc<str>>,
    pub type_ref: TypeReference,

    pub layout: RecordMemberLayout,

    pub accessibility: Accessibility,

    pub artificial: bool,

    pub embedded: bool,

    pub declaration: Option<SourceLocation>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum BaseClassVirtuality {
    None,
    Virtual,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct BaseClass {
    pub type_ref: TypeReference,

    pub layout: RecordMemberLayout,

    pub accessibility: Accessibility,

    pub virtuality: BaseClassVirtuality,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum VariantSelector {
    Value(IntegerValue),

    Range {
        low: IntegerValue,

        high: IntegerValue,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum VariantSelection {
    Default,

    Selectors(Arc<[VariantSelector]>),
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum VariantDiscriminant {
    Stored(RecordMember),

    TagType(TypeReference),

    Absent,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Variant {
    pub name: Option<Arc<str>>,

    pub selection: VariantSelection,

    pub members: Arc<[RecordMember]>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum TypeKind {
    Base(BaseType),

    Enumeration {
        representation: BaseType,

        underlying: Option<TypeReference>,

        enumerators: Arc<[Enumerator]>,

        origin: EnumerationOrigin,

        scoped: bool,
    },

    Pointer {
        target: Option<TypeReference>,

        address_class: u64,
    },

    Reference {
        kind: ReferenceKind,
        target: TypeReference,

        address_class: u64,
    },

    Array {
        element: TypeReference,

        dimensions: Arc<[ArrayDimension]>,

        ordering: ArrayOrdering,
    },

    RuntimeArray {
        element: TypeReference,

        dimensions: Arc<[RuntimeDimension]>,

        ordering: ArrayOrdering,
    },

    Slice {
        element: TypeReference,

        words: SliceWords,

        text: bool,
    },

    Record {
        kind: RecordKind,

        members: Arc<[RecordMember]>,

        bases: Arc<[BaseClass]>,

        incomplete: bool,
    },

    Union {
        members: Arc<[RecordMember]>,

        incomplete: bool,
    },

    Variant {
        storage: VariantStorageKind,

        common_members: Arc<[RecordMember]>,

        bases: Arc<[BaseClass]>,

        discriminant: Box<VariantDiscriminant>,

        variants: Arc<[Variant]>,

        incomplete: bool,
    },

    Modified {
        modifier: TypeModifier,
        target: TypeReference,
    },

    Named {
        target: Option<TypeReference>,

        relationship: NamedTypeRelationship,
    },

    Unspecified,

    Function,

    Signature {
        returns: Option<TypeReference>,

        parameters: Arc<[TypeReference]>,

        variadic: bool,

        prototyped: bool,
    },

    Opaque {
        description: Arc<str>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SliceWords {
    pub data: u8,

    pub length: u8,

    pub capacity: Option<u8>,
}

impl SliceWords {
    pub const POINTER_LENGTH: Self = Self {
        data: 0,
        length: 1,
        capacity: None,
    };

    pub const LENGTH_POINTER: Self = Self {
        data: 1,
        length: 0,
        capacity: None,
    };

    pub const POINTER_LENGTH_CAPACITY: Self = Self {
        data: 0,
        length: 1,
        capacity: Some(2),
    };

    #[must_use]
    pub fn span(self) -> u64 {
        u64::from(self.data.max(self.length).max(self.capacity.unwrap_or(0))) + 1
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ArrayOrdering {
    RowMajor,

    ColumnMajor,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ArrayDimension {
    pub lower_bound: i128,

    pub count: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct RuntimeDimension {
    pub lower_bound: ArrayBound,

    pub extent: ArrayExtent,

    pub byte_stride: Option<ArrayBound>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ArrayExtent {
    Upper(ArrayBound),

    Count(ArrayBound),

    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ArrayBound {
    Constant(i128),

    Computed { byte_size: u8, signed: bool },

    Stored { byte_size: u8, signed: bool },

    Variable { debug_info_offset: u64 },
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct TypeInfo {
    pub reference: TypeReference,

    pub name: Arc<str>,

    pub byte_size: Option<u64>,

    pub kind: TypeKind,

    pub identity: Option<Arc<TypeIdentity>>,
}

impl TypeInfo {
    #[must_use]
    pub fn qualified_name(&self) -> String {
        let Some(identity) = self.identity.as_deref() else {
            return self.name.to_string();
        };

        let spells_path = identity.path.first().is_none_or(|first| {
            self.name
                .strip_prefix(first.as_ref())
                .is_some_and(|rest| rest.starts_with("::"))
        });
        if spells_path
            || !matches!(
                identity.language,
                SourceLanguage::C | SourceLanguage::Cpp | SourceLanguage::Rust
            )
        {
            return self.name.to_string();
        }

        let mut name = String::new();

        for segment in identity.path.iter() {
            name.push_str(segment);
            name.push_str("::");
        }

        name.push_str(&self.name);
        name
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[non_exhaustive]
pub enum SourceLanguage {
    C,

    Cpp,
    Rust,
    Go,

    Zig,

    Odin,

    Fortran,

    D,

    Nim,

    Ada,

    Python,

    JavaScript,

    TypeScript,

    Other(u16),

    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct TypeIdentity {
    pub language: SourceLanguage,

    pub path: Arc<[Arc<str>]>,

    pub inline_namespaces: Arc<[Arc<str>]>,

    pub base: Arc<str>,

    pub arguments: Arc<[TypeArgument]>,

    pub pack: Option<usize>,

    pub origin: ArgumentOrigin,

    pub go: Option<GoTypeAttributes>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum TypeArgument {
    Type(TypeReference),

    Value(IntegerValue),

    Unknown(Arc<str>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ArgumentOrigin {
    Dwarf,

    ParsedName,

    None,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct GoTypeAttributes {
    pub kind: GoKind,

    pub runtime_type: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum GoKind {
    Bool,
    Int,
    Int8,
    Int16,
    Int32,
    Int64,
    Uint,
    Uint8,
    Uint16,
    Uint32,
    Uint64,
    Uintptr,
    Float32,
    Float64,
    Complex64,
    Complex128,
    Array,
    Chan,
    Func,
    Interface,
    Map,
    Pointer,
    Slice,
    String,
    Struct,
    UnsafePointer,

    Other(u8),
}

impl GoKind {
    #[must_use]
    pub const fn from_abi(value: u8) -> Self {
        match value {
            1 => Self::Bool,
            2 => Self::Int,
            3 => Self::Int8,
            4 => Self::Int16,
            5 => Self::Int32,
            6 => Self::Int64,
            7 => Self::Uint,
            8 => Self::Uint8,
            9 => Self::Uint16,
            10 => Self::Uint32,
            11 => Self::Uint64,
            12 => Self::Uintptr,
            13 => Self::Float32,
            14 => Self::Float64,
            15 => Self::Complex64,
            16 => Self::Complex128,
            17 => Self::Array,
            18 => Self::Chan,
            19 => Self::Func,
            20 => Self::Interface,
            21 => Self::Map,
            22 => Self::Pointer,
            23 => Self::Slice,
            24 => Self::String,
            25 => Self::Struct,
            26 => Self::UnsafePointer,
            other => Self::Other(other),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum TypeNode {
    Resolved(TypeInfo),

    Malformed {
        reference: TypeReference,

        description: Arc<str>,
    },
}

impl TypeNode {
    #[must_use]
    pub const fn reference(&self) -> TypeReference {
        match self {
            Self::Resolved(info) => info.reference,
            Self::Malformed { reference, .. } => *reference,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum FloatValue {
    Binary16(u16),

    BFloat16(u16),

    Binary32(u32),

    Binary64(u64),

    Binary128(u128),

    X87Extended {
        significand: u64,

        sign_exponent: u16,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ScalarValue {
    Boolean(bool),

    Signed(i128),
    Unsigned(u128),

    Floating(FloatValue),

    Complex {
        real: FloatValue,
        imaginary: FloatValue,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AddressValue {
    pub address: VirtualAddress,

    pub function: Option<Arc<str>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum VariableValue {
    /// A supported scalar value.
    Scalar(ScalarValue),

    Enumeration {
        value: IntegerValue,

        matches: Arc<[Enumerator]>,
    },

    /// A concrete thin pointer or reference address.
    Address(AddressValue),

    Function {
        code: Option<VirtualAddress>,

        function: Option<Arc<str>>,
    },

    /// An optimized pointer with no concrete address representation.
    ImplicitPointer,

    /// An array whose elements are available through explicit child pages.
    Array { dimensions: Arc<[ArrayDimension]> },

    /// A Fortran allocatable array the program has not allocated.
    NotAllocated,

    /// A Fortran pointer array that is not associated with a target.
    NotAssociated,

    Slice {
        length: u64,

        /// Runtime capacity when present in the descriptor.
        capacity: Option<u64>,
    },

    /// A structure or class whose bases and members are available through child pages.
    Record,

    /// Overlapping union storage whose interpretations are available through child pages.
    Union,

    Variant {
        discriminant: Option<IntegerValue>,

        active: Option<Arc<Variant>>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ValueChildRelationship {
    ArrayElement {
        index: u64,

        /// One index per source dimension, including each declared lower bound.
        indices: Arc<[i128]>,
    },

    SliceElement {
        index: u64,
    },

    Member(RecordMember),

    Base(BaseClass),

    Element {
        index: u64,
    },

    Entry {
        index: u64,

        key: Arc<MapKey>,
    },

    Field {
        name: Arc<str>,
    },

    Raw,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MapKey {
    pub type_info: TypeInfo,

    pub state: VariableState,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum InspectionLimit {
    Variables,

    MemoryBytes,

    MemoryReads,

    ValueNodes,

    AggregateDepth,

    ExpressionWork,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InspectionLimits {
    pub variables: u64,

    pub value_nodes: u64,

    pub aggregate_depth: u64,

    pub memory_reads: u64,

    pub memory_bytes: u64,

    pub expression_work: u64,
}

impl Default for InspectionLimits {
    fn default() -> Self {
        Self {
            variables: 256,
            value_nodes: 512,
            aggregate_depth: 64,
            memory_reads: 1_024,
            memory_bytes: 256 * 1_024,
            expression_work: 5_120_000,
        }
    }
}

impl InspectionLimits {
    #[must_use]
    pub const fn remaining_after(self, usage: InspectionUsage) -> Self {
        Self {
            variables: self.variables.saturating_sub(usage.variables),
            value_nodes: self.value_nodes.saturating_sub(usage.value_nodes),
            aggregate_depth: self.aggregate_depth.saturating_sub(usage.aggregate_depth),
            memory_reads: self.memory_reads.saturating_sub(usage.memory_reads),
            memory_bytes: self.memory_bytes.saturating_sub(usage.memory_bytes),
            expression_work: self.expression_work.saturating_sub(usage.expression_work),
        }
    }
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct InspectionUsage {
    pub variables: u64,

    pub value_nodes: u64,

    pub aggregate_depth: u64,

    pub memory_reads: u64,

    pub memory_bytes: u64,

    pub expression_work: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InspectionExhaustion {
    pub resource: InspectionLimit,

    pub limit: u64,

    pub used: u64,

    pub requested: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum InspectionCompletion {
    Complete,

    Truncated(InspectionExhaustion),
}

impl InspectionCompletion {
    #[must_use]
    pub const fn exhaustion(self) -> Option<InspectionExhaustion> {
        match self {
            Self::Complete => None,
            Self::Truncated(exhaustion) => Some(exhaustion),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ValueStorage {
    Memory(VirtualAddress),

    Bytes {
        source: VariableValueSource,
        raw: Arc<[u8]>,
        start: usize,
        end: usize,
        address: Option<VirtualAddress>,
    },

    ImplicitPointer {
        debug_info_offset: u64,
        byte_offset: i64,
    },

    Composite(CompositeStorage),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompositeStorage {
    pub pieces: Arc<[StoragePiece]>,

    pub start: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoragePiece {
    pub offset: u64,
    pub size: u64,
    pub location: PieceLocation,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PieceLocation {
    Memory {
        address: VirtualAddress,
        bit_offset: u64,
    },

    Bytes {
        source: VariableValueSource,
        raw: Arc<[u8]>,
        bit_offset: u64,
    },

    ImplicitPointer {
        debug_info_offset: u64,
        byte_offset: i64,
    },

    Undefined,

    Unavailable(VariableUnavailableReason),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValueChildrenReference {
    pub(crate) stop_id: crate::StopId,
    pub(crate) context: ExecutionContext,
    pub(crate) frame: StackFrameId,
    pub(crate) module: ModuleId,
    pub(crate) image: ModuleImageId,
    pub(crate) context_address: Option<ImageAddress>,
    pub(crate) target_type: TypeId,
    pub(crate) storage: ValueStorage,
    pub(crate) total: u64,
    pub(crate) active_variant: Option<usize>,

    pub(crate) view: Option<ViewChildren>,

    pub(crate) placement: Option<Arc<ArrayPlacement>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArrayPlacement {
    pub(crate) dimensions: Arc<[ArrayDimension]>,

    pub(crate) strides: Arc<[i64]>,
}

#[derive(Clone)]
pub struct ViewChildren {
    pub(crate) bound: Option<Arc<dyn std::any::Any + Send + Sync>>,

    pub(crate) elements: u64,

    pub(crate) fields: u64,

    pub(crate) inner: Option<Arc<ValueChildrenReference>>,

    pub(crate) picked: Option<Arc<[u64]>>,
}

impl fmt::Debug for ViewChildren {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ViewChildren")
            .field("elements", &self.elements)
            .field("fields", &self.fields)
            .finish_non_exhaustive()
    }
}

impl PartialEq for ViewChildren {
    fn eq(&self, other: &Self) -> bool {
        let same_view = match (&self.bound, &other.bound) {
            (Some(left), Some(right)) => Arc::ptr_eq(left, right),
            (None, None) => true,
            _ => false,
        };

        same_view
            && self.elements == other.elements
            && self.fields == other.fields
            && self.inner == other.inner
            && self.picked == other.picked
    }
}

impl Eq for ViewChildren {}

impl ValueChildrenReference {
    #[must_use]
    pub const fn stop_id(&self) -> crate::StopId {
        self.stop_id
    }

    #[must_use]
    pub const fn total(&self) -> u64 {
        self.total
    }

    #[must_use]
    pub const fn elements(&self) -> Option<u64> {
        match &self.view {
            Some(view) => Some(view.elements),
            None => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ValueChildren {
    NotApplicable,

    Available(Arc<ValueChildrenReference>),

    Unavailable(VariableUnavailableReason),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValueChild {
    pub relationship: ValueChildRelationship,

    pub type_info: TypeInfo,

    pub state: VariableState,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValueChildPage {
    pub stop_id: crate::StopId,

    pub offset: u64,

    pub total: u64,

    pub children: Arc<[ValueChild]>,

    pub completion: InspectionCompletion,

    pub usage: InspectionUsage,
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum VariableValueSource {
    Memory(VirtualAddress),

    Register(RegisterDescriptor),

    Constant,

    Computed,

    ImplicitPointer,

    Composite,
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum DereferenceUnavailableReason {
    Null,

    UnspecifiedPointee,

    UnsupportedPointee(Arc<str>),

    AddressClass(u64),

    Unavailable(VariableUnavailableReason),

    Malformed(VariableMalformedReason),
}

impl fmt::Display for DereferenceUnavailableReason {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Null => formatter.write_str("cannot dereference a null pointer"),
            Self::UnspecifiedPointee => {
                formatter.write_str("the pointer has no concrete pointee type")
            }
            Self::UnsupportedPointee(description) => formatter.write_str(description),
            Self::AddressClass(class) => {
                write!(formatter, "address class {class} is unsupported")
            }
            Self::Unavailable(reason) => reason.fmt(formatter),
            Self::Malformed(reason) => write!(formatter, "malformed: {}", reason.description),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DereferenceTarget {
    Address(VirtualAddress),
    ImplicitPointer {
        debug_info_offset: u64,
        byte_offset: i64,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DereferenceReference {
    pub(crate) stop_id: crate::StopId,
    pub(crate) context: ExecutionContext,
    pub(crate) frame: StackFrameId,
    pub(crate) module: ModuleId,
    pub(crate) image: ModuleImageId,
    pub(crate) context_address: Option<ImageAddress>,
    pub(crate) target_type: TypeId,
    pub(crate) target: DereferenceTarget,
}

impl DereferenceReference {
    #[must_use]
    pub const fn context(&self) -> ExecutionContext {
        self.context
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DereferenceState {
    NotApplicable,

    Available(Box<DereferenceReference>),

    Unavailable {
        pointee: Option<Box<TypeInfo>>,

        reason: DereferenceUnavailableReason,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum UnsupportedVariableFeature {
    EntryValue,

    ParameterReference,

    CrossDieEvaluation,

    Tls,

    AddressSpace,

    CompositeLocation,

    ImplicitPointer,

    WasmLocation,

    RegisterClass,

    TypedValue,

    AlternativeLocations,

    TypeRepresentation,

    RuntimeAggregateLocation,

    ScalarRepresentation,

    ExpressionOperation,

    ReturnPlace,
}

impl fmt::Display for UnsupportedVariableFeature {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::EntryValue => "DWARF entry values",
            Self::ParameterReference => "DWARF parameter references",
            Self::CrossDieEvaluation => "cross-entry DWARF evaluation",
            Self::Tls => "thread-local storage",
            Self::AddressSpace => "non-default address spaces",
            Self::CompositeLocation => "composite DWARF locations",
            Self::ImplicitPointer => "DWARF implicit pointers",
            Self::WasmLocation => "WebAssembly locations",
            Self::RegisterClass => "the requested register class",
            Self::TypedValue => "the requested typed DWARF value",
            Self::AlternativeLocations => "simultaneous alternative DWARF locations",
            Self::TypeRepresentation => "the source type representation",
            Self::ExpressionOperation => "the DWARF expression operation",
            Self::RuntimeAggregateLocation => "the runtime aggregate location",
            Self::ScalarRepresentation => "the scalar representation",
            Self::ReturnPlace => "returning this type by the function's calling convention",
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ValueBitRange {
    pub offset: u64,

    pub size: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum OptimizedOutReason {
    NoLocation,

    EmptyLocation,

    UndefinedPieces {
        /// Undefined destination ranges in source-value bit coordinates.
        ranges: Arc<[ValueBitRange]>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum CallFrameUnavailableReason {
    NoInstructionContext,

    UnwindTerminated(Arc<str>),

    TailCall,

    Suspended,

    Interpreted,
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum EntryValueUnavailableReason {
    NoCaller,

    NoCallSite,

    TargetMismatch,

    UnknownTarget,

    TailCalls,

    NoParameter,

    DiscardedState,

    Caller(Box<VariableUnavailableReason>),
}

impl fmt::Display for EntryValueUnavailableReason {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoCaller => formatter.write_str("the frame has no caller to recover it from"),
            Self::NoCallSite => {
                formatter.write_str("the caller's debug information describes no call here")
            }
            Self::TargetMismatch => {
                formatter.write_str("the caller called another function, which jumped here")
            }
            Self::UnknownTarget => formatter.write_str("the caller's call target is unknown"),
            Self::TailCalls => {
                formatter.write_str("tail calls may have entered the function again since")
            }
            Self::NoParameter => formatter.write_str("the call site does not describe it"),
            Self::DiscardedState => {
                formatter.write_str("a tail call passed it from state its jump discarded")
            }
            Self::Caller(reason) => write!(formatter, "the caller cannot provide it: {reason}"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum TlsUnavailableReason {
    ModuleIdentityUnavailable,

    ProviderUnavailable,

    LookupFailed(Arc<str>),

    NoThread,
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ValueAccessUnavailableReason {
    UnspecifiedPointee,

    NotPointerOrReference,

    AddressClass(u64),

    NullPointer,

    AddressOverflow,

    NoConcreteObjectAddress,

    NonIntegralBitField,

    InactiveVariant(Option<Arc<str>>),

    UndescribedClosure,

    UnknownLength,

    ImplicitPointerOutOfBounds {
        offset: i64,

        size: u64,

        containing_size: u64,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum VariableUnavailableReason {
    OptimizedOut(OptimizedOutReason),

    UnavailableAtInstruction,

    NoInstructionContext,

    Unsupported(UnsupportedVariableFeature),

    MemoryInaccessible {
        address: VirtualAddress,

        requested: u64,

        completed: u64,

        next_address: VirtualAddress,
    },

    RegisterUnavailable(Arc<str>),

    RegisterNotSaved(Arc<str>),

    CallFrameUnavailable(CallFrameUnavailableReason),

    EntryValue(EntryValueUnavailableReason),

    TlsUnavailable(TlsUnavailableReason),

    ValueAccess(ValueAccessUnavailableReason),

    EvaluationLimit,

    NoTask,

    InspectionLimit(InspectionExhaustion),

    BelowStackPointer {
        address: VirtualAddress,
    },

    NotSavedAcrossAwait {
        line: LineNumber,
    },

    Unbound,

    Interpreted(Arc<str>),

    IndexOutOfBounds {
        index: i128,

        lower_bound: i128,

        count: u64,
    },
}

impl fmt::Display for VariableUnavailableReason {
    #[expect(
        clippy::too_many_lines,
        reason = "every public unavailable category has one stable user-facing diagnosis"
    )]
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::OptimizedOut(
                OptimizedOutReason::NoLocation | OptimizedOutReason::EmptyLocation,
            ) => formatter.write_str("the value is optimized out"),
            Self::OptimizedOut(OptimizedOutReason::UndefinedPieces { ranges }) => {
                formatter.write_str("the value is partially optimized out in ")?;
                for (index, range) in ranges.iter().enumerate() {
                    if index != 0 {
                        formatter.write_str(", ")?;
                    }

                    write!(
                        formatter,
                        "bits {}..{}",
                        range.offset,
                        range.offset.saturating_add(range.size)
                    )?;
                }

                Ok(())
            }
            Self::UnavailableAtInstruction => {
                formatter.write_str("the value is unavailable at the current instruction")
            }
            Self::NoInstructionContext => {
                formatter.write_str("no instruction context is available to select the value")
            }
            Self::Unsupported(feature) => {
                write!(formatter, "unsupported variable feature: {feature}")
            }

            Self::MemoryInaccessible {
                address,
                requested,
                completed,
                next_address,
            } => write!(
                formatter,
                "memory is inaccessible at {next_address}; read {completed} of {requested} bytes from {address}"
            ),
            Self::RegisterUnavailable(register) => {
                write!(formatter, "register {register} is unavailable")
            }
            Self::RegisterNotSaved(register) => write!(
                formatter,
                "register {register} was not saved by the frame's callees"
            ),
            Self::CallFrameUnavailable(CallFrameUnavailableReason::NoInstructionContext) => {
                formatter.write_str("the instruction has no call-frame context")
            }
            Self::CallFrameUnavailable(CallFrameUnavailableReason::UnwindTerminated(reason)) => {
                write!(formatter, "the call-frame address is unavailable: {reason}")
            }
            Self::CallFrameUnavailable(CallFrameUnavailableReason::TailCall) => {
                formatter.write_str("a tail call discarded the frame")
            }
            Self::CallFrameUnavailable(CallFrameUnavailableReason::Suspended) => {
                formatter.write_str("the frame is suspended, and is on no stack")
            }
            Self::CallFrameUnavailable(CallFrameUnavailableReason::Interpreted) => {
                formatter.write_str("the frame is interpreted, and has no machine frame")
            }
            Self::EntryValue(reason) => {
                write!(formatter, "the entry value is unavailable: {reason}")
            }
            Self::TlsUnavailable(TlsUnavailableReason::ModuleIdentityUnavailable) => {
                formatter.write_str("the module has no TLS loader identity")
            }
            Self::TlsUnavailable(TlsUnavailableReason::ProviderUnavailable) => {
                formatter.write_str("no compatible TLS provider is available")
            }
            Self::TlsUnavailable(TlsUnavailableReason::LookupFailed(reason)) => {
                write!(formatter, "TLS lookup failed: {reason}")
            }
            Self::TlsUnavailable(TlsUnavailableReason::NoThread) => {
                formatter.write_str("a parked task has no thread-local storage")
            }
            Self::ValueAccess(ValueAccessUnavailableReason::UnspecifiedPointee) => {
                formatter.write_str("the pointer has no concrete pointee type")
            }
            Self::ValueAccess(ValueAccessUnavailableReason::NotPointerOrReference) => {
                formatter.write_str("the value is not a pointer or reference")
            }
            Self::ValueAccess(ValueAccessUnavailableReason::AddressClass(class)) => {
                write!(formatter, "pointer address class {class} is unsupported")
            }
            Self::ValueAccess(ValueAccessUnavailableReason::NullPointer) => {
                formatter.write_str("cannot dereference a null pointer")
            }
            Self::ValueAccess(ValueAccessUnavailableReason::UnknownLength) => {
                formatter.write_str("the array's length is not described; index its elements")
            }
            Self::ValueAccess(ValueAccessUnavailableReason::AddressOverflow) => {
                formatter.write_str("value address arithmetic overflowed")
            }
            Self::ValueAccess(ValueAccessUnavailableReason::NoConcreteObjectAddress) => {
                formatter.write_str("the value has no concrete containing-object address")
            }
            Self::ValueAccess(ValueAccessUnavailableReason::NonIntegralBitField) => {
                formatter.write_str("non-integral bit-fields are unsupported")
            }
            Self::ValueAccess(ValueAccessUnavailableReason::UndescribedClosure) => {
                formatter.write_str("debug information does not describe what the closure captured")
            }
            Self::ValueAccess(ValueAccessUnavailableReason::InactiveVariant(name)) => {
                if let Some(name) = name {
                    write!(formatter, "the member belongs to inactive variant '{name}'")
                } else {
                    formatter.write_str("the member belongs to an inactive variant")
                }
            }

            Self::ValueAccess(ValueAccessUnavailableReason::ImplicitPointerOutOfBounds {
                offset,
                size,
                containing_size,
            }) => write!(
                formatter,
                "implicit-pointer range at offset {offset} with size {size} is outside its {containing_size}-byte object"
            ),
            Self::EvaluationLimit => {
                formatter.write_str("DWARF expression evaluation limit exceeded")
            }
            Self::NoTask => formatter.write_str("the thread runs no task"),
            Self::NotSavedAcrossAwait { line } => write!(
                formatter,
                "the value was not kept across the await at line {line}, which this call resumed from"
            ),
            Self::BelowStackPointer { address } => write!(
                formatter,
                "the pointer is stale: {address} is below the frame's stack pointer, in memory only its callees use"
            ),
            Self::InspectionLimit(exhaustion) => write!(
                formatter,
                "{:?} limit {} exhausted after {} while requesting {}",
                exhaustion.resource, exhaustion.limit, exhaustion.used, exhaustion.requested
            ),
            Self::Unbound => {
                formatter.write_str("the variable is unbound: not yet assigned, or deleted")
            }
            Self::Interpreted(reason) => write!(formatter, "the interpreter's object: {reason}"),
            Self::IndexOutOfBounds {
                index,
                lower_bound,
                count,
            } => write!(
                formatter,
                "index {index} is outside the source bounds starting at {lower_bound} with {count} elements"
            ),
        }
    }
}

impl From<UnsupportedVariableFeature> for VariableUnavailableReason {
    fn from(feature: UnsupportedVariableFeature) -> Self {
        Self::Unsupported(feature)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum VariableMalformedKind {
    InvalidAttribute,

    InvalidReference,

    InvalidTypeGraph,

    InvalidLocationList,

    InvalidExpression,

    InconsistentLayout,

    InvalidConstant,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VariableMalformedReason {
    pub kind: VariableMalformedKind,

    pub description: Arc<str>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum VariableInvalidReason {
    BooleanRepresentation(u128),
}

impl fmt::Display for VariableInvalidReason {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BooleanRepresentation(value) => {
                write!(formatter, "invalid boolean representation {value}")
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextSummary {
    pub bytes: Arc<[u8]>,

    pub completion: TextCompletion,

    pub extent: Option<u64>,
}

impl TextSummary {
    pub const MAX_BYTES: usize = 256;

    #[must_use]
    pub fn from_units(
        raw: &[u8],
        width: usize,
        byte_order: ByteOrder,
        completion: TextCompletion,
    ) -> Self {
        debug_assert!(
            matches!(width, 2 | 4),
            "text units are 2 or 4 bytes wide, not {width}",
        );
        let units = raw.chunks_exact(width).map(|unit| {
            let mut word = [0_u8; 4];
            let (bytes, from_bytes): (_, fn([u8; 4]) -> u32) = match byte_order {
                ByteOrder::Little => (word.get_mut(..unit.len()), u32::from_le_bytes),
                ByteOrder::Big => (
                    4_usize
                        .checked_sub(unit.len())
                        .and_then(|start| word.get_mut(start..)),
                    u32::from_be_bytes,
                ),
            };
            let Some(bytes) = bytes else {
                return 0xfffd;
            };

            bytes.copy_from_slice(unit);
            from_bytes(word)
        });
        let characters: Box<dyn Iterator<Item = char>> = if width == 2 {
            Box::new(
                char::decode_utf16(units.map(|unit| u16::try_from(unit).unwrap_or(0xfffd)))
                    .map(|character| character.unwrap_or(char::REPLACEMENT_CHARACTER)),
            )
        } else {
            Box::new(units.map(|unit| char::from_u32(unit).unwrap_or(char::REPLACEMENT_CHARACTER)))
        };

        let mut text = String::new();
        let mut cut = false;

        for character in characters {
            if text.len() + character.len_utf8() > Self::MAX_BYTES {
                cut = true;
                break;
            }

            text.push(character);
        }

        Self {
            extent: None,
            bytes: Arc::from(text.into_bytes()),
            completion: match completion {
                TextCompletion::Complete if cut => TextCompletion::Truncated { length: None },
                completion => completion,
            },
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TextCompletion {
    /// Every byte was read.
    Complete,

    /// Reading stopped at the byte limit; `length` is set when the string records its length.
    Truncated { length: Option<u64> },

    /// The text continues into memory that could not be read.
    Unreadable { address: VirtualAddress },

    Limited {
        length: Option<u64>,
        exhaustion: InspectionExhaustion,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ViewName {
    pub source: Arc<str>,

    pub line: u32,

    pub header: Arc<str>,

    pub extend: bool,
}

impl fmt::Display for ViewName {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let keyword = if self.extend { "extend " } else { "" };
        write!(
            formatter,
            "{}:{} `{keyword}{}`",
            self.source, self.line, self.header
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum PresentedShape {
    Text,

    Value,

    Dynamic,

    Empty,

    Sequence,

    Map,

    Record,

    Formatted,

    Raw,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum PresentedCount {
    Exact(u64),

    AtLeast(u64),
}

impl PresentedCount {
    #[must_use]
    pub const fn known(self) -> u64 {
        match self {
            Self::Exact(count) | Self::AtLeast(count) => count,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ViewProblem {
    CheckFailed {
        check: Arc<str>,

        detail: Option<Arc<str>>,
    },

    /// The program state could not provide a value the view needed.
    Unavailable(VariableUnavailableReason),

    /// The view asked for something the debugger refuses at this stop.
    Refused(Arc<str>),

    /// The view declares one count and generates another.
    CountMismatch { declared: u64, generated: u64 },

    /// A generator repeated an earlier element at this position.
    Cycle { at: u64 },

    /// Nesting exceeded the maximum view depth.
    TooDeep { depth: u32 },

    /// Generators exceeded the maximum number of elements for a view.
    TooMany { limit: u64 },

    /// A visualizer Wasm kernel failed or misused the host interface.
    Kernel { kernel: Arc<str>, reason: Arc<str> },

    /// The debugger failed presenting the value; a defect in microscope.
    Internal(Arc<str>),
}

impl fmt::Display for ViewProblem {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::CheckFailed {
                check,
                detail: Some(detail),
            } => write!(formatter, "check `{check}` failed: {detail}"),
            Self::CheckFailed {
                check,
                detail: None,
            } => write!(formatter, "check `{check}` failed"),
            Self::Unavailable(reason) => reason.fmt(formatter),
            Self::Refused(message) | Self::Internal(message) => formatter.write_str(message),
            Self::CountMismatch {
                declared,
                generated,
            } => write!(
                formatter,
                "the view declares {declared} elements and generates {generated}"
            ),
            Self::Cycle { at } => write!(
                formatter,
                "cycle at element {at}: it leads back to a node already visited"
            ),
            Self::TooDeep { depth } => {
                write!(formatter, "the tree is deeper than {depth} levels")
            }
            Self::TooMany { limit } => write!(
                formatter,
                "the view generates more than {limit} elements without a count"
            ),
            Self::Kernel { kernel, reason } => {
                write!(formatter, "kernel `{kernel}` failed: {reason}")
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Presentation {
    pub view: Arc<ViewName>,
    pub shape: PresentedShape,

    pub count: Option<PresentedCount>,

    pub summary: Arc<str>,

    pub detail: Option<Arc<str>>,

    pub children: ValueChildren,

    pub problem: Option<ViewProblem>,

    pub number: Option<Arc<InspectedValue>>,

    pub presented: Option<Arc<InspectedValue>>,

    pub visualizers: Arc<[Visualizer]>,
}

pub const RUST_TUPLES: &str = "Rust tuples";

impl Presentation {
    #[must_use]
    pub fn text(&self) -> &Arc<str> {
        self.detail.as_ref().unwrap_or(&self.summary)
    }

    #[must_use]
    pub fn is_rust_tuple(&self) -> bool {
        &*self.view.source == "microscope" && &*self.view.header == RUST_TUPLES
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Visualizer {
    pub renderer: Arc<crate::Renderer>,
    pub inputs: Arc<VisualizerReference>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VisualizerReference {
    pub(crate) children: Arc<ValueChildrenReference>,

    pub(crate) index: usize,
}

impl VisualizerReference {
    #[must_use]
    pub fn stop_id(&self) -> crate::StopId {
        self.children.stop_id
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VariableState {
    Available {
        source: VariableValueSource,

        raw: Option<Arc<[u8]>>,

        value: VariableValue,

        dereference: DereferenceState,

        children: ValueChildren,

        text: Option<Arc<TextSummary>>,

        presentation: Option<Arc<Presentation>>,
    },

    Unavailable(VariableUnavailableReason),

    Invalid {
        source: VariableValueSource,

        raw: Arc<[u8]>,

        reason: VariableInvalidReason,
    },

    Malformed(VariableMalformedReason),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InspectedValue {
    pub type_info: Option<TypeInfo>,

    pub state: VariableState,

    pub completion: InspectionCompletion,

    pub usage: InspectionUsage,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum VariableKind {
    Parameter,

    Result,

    Local,

    Global,

    Returned,

    Captured,

    Module,

    Unnamed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum GlobalVariableVisibility {
    External,

    CompilationUnit,
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum GlobalVariableType {
    Resolved(TypeInfo),

    Unsupported(Arc<str>),

    Malformed(VariableMalformedReason),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GlobalVariableInfo {
    pub id: GlobalVariableId,

    pub name: Arc<str>,

    pub qualified_name: Arc<str>,

    pub linkage_name: Option<Arc<str>>,

    pub declaration: Option<SourceLocation>,

    pub type_info: GlobalVariableType,

    pub visibility: GlobalVariableVisibility,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GlobalVariableCandidate {
    pub id: GlobalVariableId,

    pub qualified_name: Arc<str>,

    pub declaration_path: Option<Arc<PathBuf>>,

    pub declaration: Option<SourceLocation>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GlobalVariableReference {
    pub module: ModuleId,

    pub image: ModuleImageId,

    pub variable: GlobalVariableId,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadedGlobalVariableInfo {
    pub module: Option<LoadedModule>,

    pub image: ModuleImageId,

    pub variable: GlobalVariableInfo,

    pub selector: Arc<str>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GlobalVariablePage {
    pub revision: u64,

    pub offset: u64,

    pub total: u64,

    pub variables: Arc<[LoadedGlobalVariableInfo]>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Variable {
    pub kind: VariableKind,

    pub global: Option<GlobalVariableReference>,

    pub name: Arc<str>,

    pub declaration: Option<SourceLocation>,

    pub type_info: Option<TypeInfo>,

    pub unresolved_shape: Option<ShapeUnresolvedReason>,

    pub state: VariableState,

    pub unnamed: Option<Arc<str>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ShapeUnresolvedReason {
    NoDictionary,

    DictionaryUnavailable(VariableUnavailableReason),

    UnreliableDictionary,

    ModuleDataUnavailable(VariableUnavailableReason),

    ForeignType,

    UndescribedType,

    MismatchedShape,

    Malformed(Arc<str>),
}

impl fmt::Display for ShapeUnresolvedReason {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoDictionary => formatter.write_str("the function has no type dictionary here"),
            Self::DictionaryUnavailable(reason) => {
                write!(formatter, "the type dictionary is unavailable: {reason}")
            }
            Self::UnreliableDictionary => formatter.write_str(
                "optimized code may not have stored its type dictionary where described",
            ),
            Self::ModuleDataUnavailable(reason) => {
                write!(
                    formatter,
                    "the runtime's type table is unavailable: {reason}"
                )
            }
            Self::ForeignType => {
                formatter.write_str("the type argument is described by another module")
            }
            Self::UndescribedType => {
                formatter.write_str("no debug information describes the type argument")
            }
            Self::MismatchedShape => {
                formatter.write_str("the type argument is laid out unlike its shape")
            }
            Self::Malformed(description) => write!(
                formatter,
                "the type dictionary's debug information is malformed: {description}"
            ),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DereferencedValue {
    pub type_info: TypeInfo,

    pub state: VariableState,

    pub completion: InspectionCompletion,

    pub usage: InspectionUsage,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VariableSnapshot {
    pub revision: u64,

    pub stop_id: crate::StopId,

    pub context: ExecutionContext,

    pub stack_frame: StackFrameId,

    pub frame: crate::PresentedFrame,

    pub frame_address: Option<VirtualAddress>,

    pub target: TargetDescription,

    pub variables: Arc<[Variable]>,

    pub completion: InspectionCompletion,

    pub usage: InspectionUsage,
}

pub fn c_base_type(target: TargetDescription, ty: CBaseType) -> Option<BaseType> {
    use BaseTypeEncoding as E;
    if target.architecture != Architecture::X86_64 || target.pointer_width != PointerWidth::Bits64 {
        return None;
    }

    let (encoding, byte_size) = match ty {
        CBaseType::Char | CBaseType::SignedChar => (E::SignedCharacter, 1),
        CBaseType::UnsignedChar => (E::UnsignedCharacter, 1),
        CBaseType::Short => (E::Signed, 2),
        CBaseType::UnsignedShort => (E::Unsigned, 2),
        CBaseType::Int => (E::Signed, 4),
        CBaseType::UnsignedInt => (E::Unsigned, 4),
        CBaseType::Long | CBaseType::LongLong => (E::Signed, 8),
        CBaseType::UnsignedLong | CBaseType::UnsignedLongLong => (E::Unsigned, 8),
        CBaseType::Float => (E::Floating, 4),
        CBaseType::Double => (E::Floating, 8),
        CBaseType::LongDouble => (E::Floating, 16),
    };

    let name: Arc<str> = ty.name().into();

    Some(BaseType {
        name: Arc::clone(&name),
        base_name: name,
        encoding,
        byte_size,
        bit_size: None,
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum CBaseType {
    Char,
    SignedChar,
    UnsignedChar,
    Short,
    UnsignedShort,
    Int,
    UnsignedInt,
    Long,
    UnsignedLong,
    LongLong,
    UnsignedLongLong,
    Float,
    Double,
    LongDouble,
}

impl CBaseType {
    pub const ALL: [Self; 14] = [
        Self::Char,
        Self::SignedChar,
        Self::UnsignedChar,
        Self::Short,
        Self::UnsignedShort,
        Self::Int,
        Self::UnsignedInt,
        Self::Long,
        Self::UnsignedLong,
        Self::LongLong,
        Self::UnsignedLongLong,
        Self::Float,
        Self::Double,
        Self::LongDouble,
    ];

    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Char => "char",
            Self::SignedChar => "signed char",
            Self::UnsignedChar => "unsigned char",
            Self::Short => "short",
            Self::UnsignedShort => "unsigned short",
            Self::Int => "int",
            Self::UnsignedInt => "unsigned int",
            Self::Long => "long",
            Self::UnsignedLong => "unsigned long",
            Self::LongLong => "long long",
            Self::UnsignedLongLong => "unsigned long long",
            Self::Float => "float",
            Self::Double => "double",
            Self::LongDouble => "long double",
        }
    }

    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|ty| ty.name() == name)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum MemoryReadUnavailableReason {
    Inaccessible,
}

impl fmt::Display for MemoryReadUnavailableReason {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Inaccessible => formatter.write_str("memory inaccessible"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum MemoryReadCompletion {
    Complete,

    Incomplete {
        next_address: VirtualAddress,

        reason: MemoryReadUnavailableReason,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryRead {
    pub revision: u64,

    pub stop_id: crate::StopId,

    pub target: TargetDescription,

    pub address: VirtualAddress,

    pub requested: u64,

    pub bytes: Arc<[u8]>,

    pub completion: MemoryReadCompletion,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct LineNumber(std::num::NonZeroU64);

impl LineNumber {
    #[must_use]
    pub const fn new(value: u64) -> Option<Self> {
        match std::num::NonZeroU64::new(value) {
            Some(value) => Some(Self(value)),
            None => None,
        }
    }

    #[must_use]
    pub const fn get(self) -> u64 {
        self.0.get()
    }
}

impl fmt::Display for LineNumber {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ColumnNumber(std::num::NonZeroU64);

impl ColumnNumber {
    #[must_use]
    pub const fn new(value: u64) -> Option<Self> {
        match std::num::NonZeroU64::new(value) {
            Some(value) => Some(Self(value)),
            None => None,
        }
    }

    #[must_use]
    pub const fn get(self) -> u64 {
        self.0.get()
    }
}

impl fmt::Display for ColumnNumber {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceFile {
    pub id: SourceFileId,

    pub path: Arc<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SourceLocation {
    pub file: SourceFileId,

    pub line: LineNumber,

    pub column: Option<ColumnNumber>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceLine {
    pub number: LineNumber,

    pub text: Arc<str>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceContext {
    pub recorded: Arc<PathBuf>,

    pub path: Arc<PathBuf>,

    pub line: LineNumber,

    pub column: Option<ColumnNumber>,

    pub lines: Arc<[SourceLine]>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FunctionInfo {
    pub id: FunctionId,

    pub name: Arc<str>,

    pub linkage_name: Option<Arc<str>>,

    pub declaration: Option<SourceLocation>,

    pub language: SourceLanguage,

    pub role: CodeRole,

    pub enclosing: Option<FunctionId>,

    pub coroutine: Option<TypeId>,

    pub generics: FunctionGenerics,

    pub main_subprogram: bool,
}

pub type FunctionGenerics = Arc<[(Arc<str>, TypeId)]>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CoroutineInfo {
    pub kind: CoroutineKind,

    pub state: StateMember,

    pub states: Arc<[CoroutineState]>,

    pub captures: Arc<[RecordMember]>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CoroutineKind {
    AsyncFunction,

    AsyncBlock,

    AsyncClosure,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StateMember {
    pub offset: u64,

    pub size: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CoroutineState {
    pub value: u64,
    pub kind: CoroutineStateKind,

    pub location: Option<SourceLocation>,

    pub saved: Arc<[RecordMember]>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CoroutineStateKind {
    Unresumed,

    Returned,

    Panicked,

    Suspended { index: u32 },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResumePoints {
    pub dispatch: Arc<[AddressRange<ImageAddress>]>,

    pub points: Arc<[ResumePoint]>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResumePoint {
    pub state: u64,

    pub address: ImageAddress,

    pub resumption: Arc<[AddressRange<ImageAddress>]>,
}

impl ResumePoints {
    #[must_use]
    pub fn point(&self, state: u64) -> Option<&ResumePoint> {
        self.points.iter().find(|point| point.state == state)
    }
}

impl CoroutineInfo {
    #[must_use]
    pub fn state(&self, value: u64) -> Option<&CoroutineState> {
        self.states.iter().find(|state| state.value == value)
    }

    pub fn variables<'a>(
        &'a self,
        state: &'a CoroutineState,
    ) -> impl Iterator<Item = (&'a RecordMember, bool)> {
        let captured = state.kind == CoroutineStateKind::Unresumed
            || self.kind != CoroutineKind::AsyncFunction;
        let own = |member: &&RecordMember| {
            member
                .name
                .as_deref()
                .is_some_and(|name| !name.starts_with("__"))
        };

        state
            .saved
            .iter()
            .filter(own)
            .map(|member| (member, false))
            .chain(
                self.captures
                    .iter()
                    .filter(move |_| captured)
                    .filter(own)
                    .map(|member| (member, true)),
            )
    }

    #[must_use]
    pub fn unresumed(&self) -> Option<&CoroutineState> {
        self.states
            .iter()
            .find(|state| state.kind == CoroutineStateKind::Unresumed)
    }
}

impl CoroutineState {
    #[must_use]
    pub fn awaitee(&self) -> Option<&RecordMember> {
        self.saved
            .iter()
            .find(|member| member.name.as_deref() == Some("__awaitee"))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CodeInstanceKind {
    OutOfLine,

    Inline {
        /// The call expression in the containing instance, when described.
        call_site: Option<SourceLocation>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryProvenance {
    Explicit,

    Statement,

    AnalyzedPrologue,

    RangeStart,

    CoroutineBody,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BreakpointEntry {
    pub address: ImageAddress,

    pub provenance: EntryProvenance,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodeInstanceInfo {
    pub id: CodeInstanceId,

    pub function: FunctionId,

    pub parent: Option<CodeInstanceId>,

    pub kind: CodeInstanceKind,

    pub ranges: Arc<[AddressRange<ImageAddress>]>,

    pub breakpoint_entry: Option<BreakpointEntry>,
}

impl CodeInstanceInfo {
    #[must_use]
    pub fn contains(&self, address: ImageAddress) -> bool {
        self.ranges.iter().any(|range| range.contains(address))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum SymbolKind {
    Function,

    IndirectFunction,

    Data,

    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum SymbolBinding {
    Global,

    Weak,

    Local,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum SymbolExtentProvenance {
    Declared,

    Inferred,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SymbolExtent {
    pub range: AddressRange<ImageAddress>,

    pub provenance: SymbolExtentProvenance,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SymbolInfo {
    pub id: SymbolId,

    pub name: Arc<str>,

    pub address: ImageAddress,

    pub kind: SymbolKind,

    pub binding: SymbolBinding,

    pub exported: bool,

    pub extent: Option<SymbolExtent>,

    pub storage: Option<AddressRange<ImageAddress>>,

    pub role: CodeRole,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DebugFile {
    Used(Arc<PathBuf>),

    Unusable {
        path: Arc<PathBuf>,
        reason: Arc<str>,
    },
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum DebugInformation {
    #[default]
    Absent,

    Loaded,

    Incomplete {
        reason: Arc<str>,
    },

    Unusable {
        reason: Arc<str>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GotSlot {
    pub address: ImageAddress,

    pub target: GotTarget,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GotTarget {
    Import(Arc<str>),

    Indirect(ImageAddress),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SymbolTableSources {
    pub static_table: bool,

    pub dynamic_table: bool,

    pub embedded_table: EmbeddedSymbolTable,

    pub runtime_function_table: EmbeddedSymbolTable,
}

impl Default for SymbolTableSources {
    fn default() -> Self {
        Self {
            static_table: false,
            dynamic_table: false,
            embedded_table: EmbeddedSymbolTable::Absent,
            runtime_function_table: EmbeddedSymbolTable::Absent,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EmbeddedSymbolTable {
    Absent,

    Loaded,

    Unusable {
        /// Why the table could not be read.
        reason: Arc<str>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SymbolLocation {
    pub symbol: SymbolId,

    pub name: Arc<str>,

    pub kind: SymbolKind,

    pub offset: u64,

    pub provenance: SymbolExtentProvenance,
}

impl SymbolInfo {
    #[must_use]
    pub fn demangled_name(&self) -> Option<String> {
        crate::demangle::demangle(&self.name)
    }

    #[must_use]
    pub fn answers_to(&self, name: &str) -> bool {
        &*self.name == name
            || self.unversioned_name() == name
            || crate::demangle::spells(&self.name, name)
    }

    #[must_use]
    pub fn unversioned_name(&self) -> &str {
        match self.name.split_once('@') {
            Some((name, version)) if !name.is_empty() && version != "plt" => name,
            _ => &self.name,
        }
    }
}

impl SymbolLocation {
    #[must_use]
    pub fn demangled_name(&self) -> Option<String> {
        crate::demangle::demangle(&self.name)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SectionInfo {
    pub id: SectionId,

    pub name: Arc<str>,

    pub range: AddressRange<ImageAddress>,

    pub executable: bool,

    pub writable: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SectionLocation {
    pub section: SectionId,

    pub name: Arc<str>,

    pub offset: u64,

    pub executable: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageAddressDescription {
    pub address: ImageAddress,

    pub section: Option<SectionLocation>,

    pub symbol: Option<SymbolLocation>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModuleAddress {
    pub module: ModuleId,

    pub path: Arc<PathBuf>,

    pub image: ImageAddressDescription,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AddressDescription {
    pub address: VirtualAddress,

    pub module: Option<ModuleAddress>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageLocation {
    pub address: ImageAddress,

    pub function: Option<FunctionInfo>,

    pub physical_instance: Option<CodeInstanceId>,

    pub inline_frames: InlineFrameLookup,

    pub source: Option<SourceLocation>,

    pub symbol: Option<SymbolLocation>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InlineChain {
    pub instances: Arc<[CodeInstanceId]>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InlineFrameLookup {
    None,

    Unique(InlineChain),

    Ambiguous(Arc<[InlineChain]>),
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum BreakpointLocation {
    Image(ImageAddress),

    Virtual(VirtualAddress),

    Managed(ManagedLocation),
}

impl BreakpointLocation {
    #[must_use]
    pub const fn address(&self) -> Option<TrapAddress> {
        match self {
            Self::Image(address) => Some(TrapAddress::Image(*address)),
            Self::Virtual(address) => Some(TrapAddress::Virtual(*address)),
            Self::Managed(_) => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum TrapAddress {
    Image(ImageAddress),

    Virtual(VirtualAddress),
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ManagedLocation {
    pub runtime: RuntimeId,
    pub place: ManagedPlace,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ManagedPlace {
    Line { path: Arc<str>, line: u32 },

    Function { name: Arc<str> },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecutionLocation {
    pub module: ModuleId,

    pub address: VirtualAddress,

    pub image: ImageLocation,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameKind {
    /// A normal machine-code activation.
    Physical,

    /// A source-level inline expansion within a physical activation.
    Inline,

    /// A signal trampoline activation.
    Signal,

    /// A synthetic tail-call frame whose caller state is gone.
    TailCall,

    /// An async function or block of a suspended task at `object`.
    Async { object: VirtualAddress },

    Awaited {
        object: VirtualAddress,
        ty: TypeReference,
    },

    /// An interpreter frame whose runtime record is at `frame`.
    Interpreted { frame: VirtualAddress },
}

impl FrameKind {
    #[must_use]
    pub const fn is_suspended(self) -> bool {
        matches!(self, Self::Async { .. } | Self::Awaited { .. })
    }

    #[must_use]
    pub const fn has_registers(self) -> bool {
        !self.is_suspended() && !matches!(self, Self::Interpreted { .. })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordedFrame {
    pub function: Arc<str>,

    pub language: SourceLanguage,

    pub place: Option<RecordedPlace>,

    pub generated: Option<RecordedPlace>,

    pub first_line: Option<u32>,

    pub declared_in: Option<Arc<str>>,

    pub problem: Option<Arc<str>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StackFrame {
    pub id: StackFrameId,

    pub level: u32,

    pub kind: FrameKind,

    pub module: Option<ModuleId>,

    pub instruction: Option<VirtualAddress>,

    pub segment: StackSegment,

    pub code_instance: Option<CodeInstanceId>,

    pub function: Option<FunctionInfo>,

    pub source: Option<SourceLocation>,

    pub symbol: Option<SymbolLocation>,

    pub role: CodeRole,

    pub awaiting: Option<Arc<str>>,

    pub recorded: Option<RecordedFrame>,
}

pub struct FrameMetadata {
    pub code_instance: Option<CodeInstanceId>,
    pub function: Option<FunctionInfo>,
    pub source: Option<SourceLocation>,
    pub symbol: Option<SymbolLocation>,
    pub role: CodeRole,
}

impl StackFrame {
    pub(crate) fn new(
        level: u32,
        kind: FrameKind,
        module: Option<ModuleId>,
        instruction: VirtualAddress,
    ) -> Self {
        Self::from_parts(
            level,
            kind,
            module,
            Some(instruction),
            FrameMetadata {
                code_instance: None,
                function: None,
                source: None,
                symbol: None,
                role: CodeRole::Ordinary,
            },
        )
    }

    pub(crate) fn from_parts(
        level: u32,
        kind: FrameKind,
        module: Option<ModuleId>,
        instruction: Option<VirtualAddress>,
        metadata: FrameMetadata,
    ) -> Self {
        Self {
            id: StackFrameId::new(level),
            level,
            kind,
            module,
            instruction,
            segment: StackSegment::Thread,
            code_instance: metadata.code_instance,
            function: metadata.function,
            source: metadata.source,
            symbol: metadata.symbol,
            role: metadata.role,
            awaiting: None,
            recorded: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UnwindTermination {
    Complete,

    NoUnwindInfo { address: VirtualAddress },

    ModuleNotFound { address: VirtualAddress },

    UnsupportedUnwindInfo { feature: Arc<str> },

    CorruptUnwindInfo { description: Arc<str> },

    RegisterUnavailable { register: Arc<str> },

    MemoryReadFailed { address: VirtualAddress },

    StackNotSaved { address: VirtualAddress },

    InvalidCaller { description: Arc<str> },

    UnresolvedStackSwitch { reason: Arc<str> },

    BrokenAwaitChain { reason: Arc<str> },

    CycleDetected,

    DepthLimit,
}

impl fmt::Display for UnwindTermination {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Complete => formatter.write_str("the outermost frame has no caller"),
            Self::NoUnwindInfo { address } => {
                write!(formatter, "no unwind information covers {address}")
            }
            Self::ModuleNotFound { address } => {
                write!(formatter, "no loaded module contains {address}")
            }
            Self::UnsupportedUnwindInfo { feature } => {
                write!(formatter, "unsupported unwind feature: {feature}")
            }
            Self::CorruptUnwindInfo { description } => {
                write!(formatter, "malformed unwind information: {description}")
            }
            Self::RegisterUnavailable { register } => {
                write!(formatter, "register {register} is unavailable")
            }
            Self::MemoryReadFailed { address } => {
                write!(formatter, "unwind memory is unreadable at {address}")
            }
            Self::StackNotSaved { address } => write!(
                formatter,
                "the core dump did not save the thread's stack at {address}, so its callers are missing"
            ),
            Self::InvalidCaller { description } => {
                write!(formatter, "invalid unwind caller: {description}")
            }
            Self::UnresolvedStackSwitch { reason } => {
                write!(
                    formatter,
                    "the stack continues where its runtime switched stacks: {reason}"
                )
            }
            Self::BrokenAwaitChain { reason } => {
                write!(formatter, "the chain of awaits ends early: {reason}")
            }
            Self::CycleDetected => formatter.write_str("unwind metadata produced a frame cycle"),
            Self::DepthLimit => formatter.write_str("unwind depth limit reached"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Backtrace {
    pub context: ExecutionContext,

    pub frames: Arc<[StackFrame]>,

    pub termination: UnwindTermination,

    pub unfollowed: Arc<[Unfollowed]>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unfollowed {
    pub driver: StackFrameId,
    pub driven: DrivenFrames,

    pub reason: Arc<str>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DrivenFrames {
    Future,

    Interpreted,

    Awaiting,
}

impl Backtrace {
    #[must_use]
    pub fn user_frame(&self) -> Option<&StackFrame> {
        let interprets = self
            .frames
            .iter()
            .any(|frame| matches!(frame.kind, FrameKind::Interpreted { .. }));

        self.frames
            .iter()
            .take_while(|frame| frame.role != CodeRole::Dispatch)
            .find(|frame| {
                frame.role == CodeRole::Ordinary
                    && !matches!(frame.kind, FrameKind::Awaited { .. })
                    && (!interprets
                        || matches!(frame.kind, FrameKind::Interpreted { .. })
                        || frame.source.is_some()
                        || frame.recorded.is_some())
            })
    }

    #[must_use]
    pub fn loop_iterators(&self) -> Vec<Option<u32>> {
        let mut iterators = vec![None; self.frames.len()];

        let mut open: Vec<(Option<ModuleId>, FunctionId, usize)> = Vec::new();

        for (index, frame) in self.frames.iter().enumerate() {
            let Some(function) = &frame.function else {
                continue;
            };

            if let Some(&(module, enclosing, start)) = open.last()
                && module == frame.module
                && enclosing == function.id
            {
                open.pop();
                for iterator in iterators.iter_mut().take(index).skip(start) {
                    *iterator = Some(frame.level);
                }
            }

            if let Some(enclosing) = function.enclosing {
                open.push((frame.module, enclosing, index + 1));
            }
        }

        iterators
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LineEntry {
    pub range: AddressRange<ImageAddress>,
    pub location: SourceLocation,

    pub statement: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatementRow {
    pub address: ImageAddress,

    pub operation_index: u64,

    pub location: Option<SourceLocation>,

    pub discriminator: u64,

    pub flags: StatementFlags,

    pub isa: u64,

    pub sequence: LineSequenceId,

    pub ordinal: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StatementFlags(u8);

impl StatementFlags {
    const IS_STATEMENT: u8 = 1 << 0;
    const PROLOGUE_END: u8 = 1 << 1;
    const EPILOGUE_BEGIN: u8 = 1 << 2;

    pub(crate) const fn empty() -> Self {
        Self(0)
    }

    pub(crate) const fn with_statement(self, enabled: bool) -> Self {
        self.with(Self::IS_STATEMENT, enabled)
    }

    pub(crate) const fn with_prologue_end(self, enabled: bool) -> Self {
        self.with(Self::PROLOGUE_END, enabled)
    }

    pub(crate) const fn with_epilogue_begin(self, enabled: bool) -> Self {
        self.with(Self::EPILOGUE_BEGIN, enabled)
    }

    const fn with(self, flag: u8, enabled: bool) -> Self {
        if enabled { Self(self.0 | flag) } else { self }
    }

    #[must_use]
    pub const fn is_statement(self) -> bool {
        self.0 & Self::IS_STATEMENT != 0
    }

    #[must_use]
    pub const fn prologue_end(self) -> bool {
        self.0 & Self::PROLOGUE_END != 0
    }

    #[must_use]
    pub const fn epilogue_begin(self) -> bool {
        self.0 & Self::EPILOGUE_BEGIN != 0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LoadedModule {
    pub id: ModuleId,

    pub image: ModuleImageId,

    pub load_bias: LoadBias,

    pub window: Option<AddressRange<ImageAddress>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadedModuleRecord {
    pub module: LoadedModule,

    pub path: Arc<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadedModuleSnapshot {
    pub revision: u64,

    pub modules: Arc<[LoadedModuleRecord]>,

    pub main: ModuleId,
}

impl LoadedModule {
    pub(crate) const fn main(image: ModuleImageId, load_bias: LoadBias) -> Self {
        Self {
            id: ModuleId::new(0),
            image,
            load_bias,
            window: None,
        }
    }

    pub fn virtual_address(self, address: ImageAddress) -> Result<VirtualAddress> {
        if !self.windows(address) {
            return Err(Error::AddressOutsideModule);
        }
        self.load_bias.load(address).ok_or(Error::AddressOverflow)
    }

    pub fn image_address(self, address: VirtualAddress) -> Result<ImageAddress> {
        self.load_bias
            .unload(address)
            .filter(|address| self.windows(*address))
            .ok_or(Error::AddressOutsideModule)
    }

    pub(crate) fn windows(self, address: ImageAddress) -> bool {
        self.window.is_none_or(|window| window.contains(address))
    }
}
