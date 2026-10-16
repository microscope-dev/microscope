use std::fmt;
use std::path::PathBuf;
use std::sync::Arc;

use crate::{Error, Result};

mod image;

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
            /// Creates an address from its numeric representation.
            #[must_use]
            pub const fn new(value: u64) -> Self {
                Self(value)
            }

            /// Returns the numeric representation of this address.
            #[must_use]
            pub const fn get(self) -> u64 {
                self.0
            }

            /// The address `bytes` past this one, wrapping past the last.
            #[must_use]
            pub const fn wrapping_add(self, bytes: u64) -> Self {
                Self(self.0.wrapping_add(bytes))
            }

            /// The address `bytes` past this one, or `None` past the last.
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

/// A process id, as the kernel numbers it in some pid namespace. Not the
/// debugger's [`ProcessId`].
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    Default,
    serde::Serialize,
    serde::Deserialize,
)]
#[serde(transparent)]
pub struct Pid(u32);

/// A thread id, as the kernel numbers it. A process's first thread has its
/// process's id, but the two are never interchangeable. Not the debugger's
/// [`ThreadId`].
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    Default,
    serde::Serialize,
    serde::Deserialize,
)]
#[serde(transparent)]
pub struct Tid(u32);

macro_rules! os_id {
    ($name:ident) => {
        impl $name {
            #[must_use]
            pub const fn new(id: u32) -> Self {
                Self(id)
            }

            #[must_use]
            pub const fn get(self) -> u32 {
                self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                self.0.fmt(formatter)
            }
        }
    };
}

os_id!(Pid);
os_id!(Tid);

impl Pid {
    /// The process `leader` is the first thread of.
    #[must_use]
    pub const fn led_by(leader: Tid) -> Self {
        Self(leader.0)
    }

    /// The process's first thread.
    #[must_use]
    pub const fn leader(self) -> Tid {
        Tid(self.0)
    }
}

/// How far a loaded module lies from its image's addresses.
///
/// Each virtual address is its image address plus the bias. A module loaded
/// below its image's addresses, as the vDSO and a prelinked library can be,
/// has a bias that wraps.
#[derive(Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct LoadBias(u64);

impl LoadBias {
    /// Creates a bias from its numeric representation.
    #[must_use]
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    /// Returns the numeric representation of this bias.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }

    /// The bias that loads `image` at `loaded`.
    #[must_use]
    pub const fn between(image: ImageAddress, loaded: VirtualAddress) -> Self {
        Self(loaded.get().wrapping_sub(image.get()))
    }

    /// Where `address` is loaded, or `None` past the last address.
    #[must_use]
    pub const fn load(self, address: ImageAddress) -> Option<VirtualAddress> {
        match self.0.checked_add(address.get()) {
            Some(loaded) => Some(VirtualAddress::new(loaded)),
            None => None,
        }
    }

    /// The image address loaded at `address`, or `None` below the bias.
    #[must_use]
    pub const fn unload(self, address: VirtualAddress) -> Option<ImageAddress> {
        match address.get().checked_sub(self.0) {
            Some(image) => Some(ImageAddress::new(image)),
            None => None,
        }
    }

    /// Where `address` is loaded, wrapping past the last address as a
    /// bias below the image does.
    #[must_use]
    pub const fn wrapping_load(self, address: ImageAddress) -> VirtualAddress {
        VirtualAddress::new(address.get().wrapping_add(self.0))
    }

    /// The image address loaded at `address`, wrapping as a bias below
    /// the image does.
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

/// A half-open address range `[start, end)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct AddressRange<A> {
    /// The first address included in the range.
    pub start: A,

    /// The first address after the range.
    pub end: A,
}

impl<A: Copy + Ord> AddressRange<A> {
    /// Returns whether the address is contained in this range.
    #[must_use]
    pub fn contains(self, address: A) -> bool {
        self.start <= address && address < self.end
    }

    /// Returns whether the range contains no address.
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

            /// Returns the dense index within the owning collection.
            #[must_use]
            pub const fn get(self) -> u32 {
                self.0
            }

            /// Returns the dense index as a `usize` for indexing.
            #[allow(dead_code, reason = "not every identifier indexes a slice")]
            pub(crate) const fn index(self) -> usize {
                self.0 as usize
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                self.0.fmt(f)
            }
        }
    };
}

/// Defines a public `u64` identifier with numeric conversions and `Display`.
macro_rules! numeric_id {
    ($name:ident, $description:literal) => {
        #[doc = $description]
        #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub struct $name(u64);

        impl $name {
            /// Creates an identifier from its numeric representation.
            #[must_use]
            pub const fn new(value: u64) -> Self {
                Self(value)
            }

            /// Returns the numeric representation of this identifier.
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
    /// The innermost frame of every stopped thread, where execution stopped.
    pub const INNERMOST: Self = Self(0);
}

numeric_id!(
    ThreadId,
    "Identifies a thread within a debug session by its platform value."
);

/// Identifies one language runtime instance within a debug session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RuntimeId(u32);

impl RuntimeId {
    /// Creates an identifier from its numeric representation.
    #[must_use]
    pub const fn new(value: u32) -> Self {
        Self(value)
    }

    /// Returns the numeric representation of this identifier.
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

/// Identifies one task, such as a goroutine, of one language runtime.
///
/// The number is the runtime's own, such as a goroutine id. Some runtimes
/// reuse numbers once a task exits, so an id names a task only within the
/// process it was read from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TaskId {
    /// The runtime instance that schedules the task.
    pub runtime: RuntimeId,

    /// The runtime's number for the task.
    pub number: u64,
}

impl std::fmt::Display for TaskId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.number.fmt(f)
    }
}

/// What a language runtime's task is doing at a stop.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TaskState {
    /// On a thread, running or in a system call.
    Running,

    /// Ready to run, waiting for a thread.
    Runnable,

    /// Waiting for an event, such as a channel or a lock.
    Blocked,

    /// Finished, and not yet released by its runtime.
    Exited,

    /// The runtime's state for the task could not be read, for this reason.
    Unknown(Arc<str>),
}

/// A place in a task's code: where it runs or will resume, the call that
/// created it, or the function it began in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskLocation {
    /// The place's code, unless its runtime recorded it only by its source.
    pub address: Option<VirtualAddress>,

    /// The loaded module whose image holds the address, or whose source
    /// the runtime recorded the place in.
    pub module: Option<ModuleId>,

    /// The function holding the address, or the call at it for a return
    /// address.
    pub function: Option<Arc<str>>,
    pub source: Option<SourceLocation>,

    /// The place as its runtime recorded it, by its source's path, as
    /// Rust's `Location` does; the source file it names may be none of
    /// the module's.
    pub recorded: Option<RecordedPlace>,
}

/// A place in a program's source a runtime recorded by the path the
/// compiler gave it, rather than by an address.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordedPlace {
    pub path: Arc<str>,
    pub line: u32,
    pub column: u32,
}

/// One task of a language runtime at a stop.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskSnapshot {
    pub id: TaskId,

    /// What its runtime calls a task, such as Go's "goroutine".
    pub noun: &'static str,
    pub state: TaskState,

    /// The runtime's own words for what the task does or waits for, such
    /// as Go's "chan receive".
    pub detail: Option<Arc<str>>,

    /// The thread running the task, or its runtime's code for it, as while
    /// it is being parked.
    pub thread: Option<ThreadId>,

    /// Where a task that is not on a thread will resume.
    pub resume: Option<TaskLocation>,

    /// The call that created the task.
    pub creation: Option<TaskLocation>,

    /// The function the task began in.
    pub entry: Option<TaskLocation>,

    /// The task that created this one.
    pub parent: Option<TaskId>,

    /// Whether the runtime runs the task for its own work, such as a
    /// garbage collector's worker, rather than the program's.
    pub internal: bool,

    /// The key-value labels the program or its runtime gave the task, such
    /// as Go's profiler labels, or the tokio runtime that holds it in a
    /// process with several.
    pub labels: Arc<[(Arc<str>, Arc<str>)]>,

    /// Where the runtime keeps the task at this stop, such as a goroutine's
    /// `g`. A runtime may reuse the memory for another task once this one
    /// exits.
    pub object: Option<VirtualAddress>,
}

/// Whose stack a frame is on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StackSegment {
    /// An OS thread's stack, which no runtime knows.
    Thread,

    /// A task's own stack.
    Task,

    /// A runtime's scheduler stack for a thread, on which it runs its own
    /// code, for a task or for none.
    System,

    /// A runtime's signal-handling stack.
    Signal,

    /// No stack: the chain of awaits of a future that the frame below
    /// drives, such as the one a runtime's `block_on` polls, which is
    /// suspended between its polls.
    Future,
}

/// What a stopped thread runs for a language runtime.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ThreadActivity {
    /// A task, or the runtime's code on its behalf.
    Task { task: TaskId, stack: StackSegment },

    /// The runtime's scheduler with no task, or for Go, code the runtime
    /// does not know, such as a thread C created.
    Idle,

    /// The program's own code, on a thread no runtime schedules tasks on,
    /// such as one the program spawned or the one that blocks on a
    /// runtime.
    Outside,

    /// The runtime's state for the thread could not be read, for this
    /// reason.
    Unknown(Arc<str>),
}

/// Where a page of tasks continues.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TaskCursor {
    pub(crate) runtime: usize,
    pub(crate) position: u64,
}

/// One page of the tasks of every runtime in a stopped process.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskPage {
    pub tasks: Arc<[TaskSnapshot]>,

    /// Where the next page begins, or `None` after the last.
    pub next: Option<TaskCursor>,

    /// Why the page may be missing tasks or describe some wrongly, such
    /// as a task whose memory could not be read. A page with none is
    /// complete.
    pub gaps: Arc<[Arc<str>]>,

    /// What reading the page cost: the memory read of the runtimes.
    pub usage: crate::InspectionUsage,
}

/// Where a request inspects or controls execution: an operating-system
/// thread, or a language runtime's task, which runs on some thread or is
/// parked with its registers saved in memory.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ExecutionContext {
    /// An operating-system thread.
    Thread(ThreadId),

    /// A language runtime's task.
    Task(TaskId),
}

impl ExecutionContext {
    /// The thread this context names, or `None` for a task.
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

/// The architecture-independent purpose of a distinguished register.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum RegisterRole {
    /// The address of the current instruction.
    ProgramCounter,

    /// The address of the top of the current stack.
    StackPointer,

    /// The base address conventionally used for the current stack frame.
    FramePointer,
}

/// Static information about a target register.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegisterDescriptor {
    /// The register's identifier within its target architecture.
    pub id: RegisterId,

    /// The canonical architecture-defined register name.
    pub name: Arc<str>,

    /// The number of meaningful bits in the register.
    pub bits: u16,

    /// The register's architecture-independent role, when distinguished.
    pub role: Option<RegisterRole>,
}

/// One target register and its captured value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegisterValue {
    /// The register represented by this value.
    pub register: RegisterDescriptor,

    /// The register bytes in the target's byte order, or `None` in a caller's
    /// frame when a callee may have overwritten the register without saving
    /// it, so its value in that frame is unknown.
    pub bytes: Option<Arc<[u8]>>,
}

/// The general register set of one frame of a stopped thread at one
/// debugger revision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegisterSnapshot {
    /// The debugger revision at which these values were read.
    pub revision: u64,

    /// The thread or task whose registers were read.
    pub context: ExecutionContext,

    /// The architecture and data representation of the register values.
    pub target: TargetDescription,

    /// Register values in the architecture's canonical display order.
    pub registers: Arc<[RegisterValue]>,
}

/// The source-language encoding of a scalar base type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum BaseTypeEncoding {
    Boolean,
    Signed,
    SignedCharacter,
    Unsigned,
    UnsignedCharacter,
    Floating,

    /// A complex number: two floats, each half its size, the real part
    /// first.
    ComplexFloating,
}

/// The format of a float type's values.
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
    /// The format a float type's values are in: by its size, and by its
    /// name where formats share a size, so that a format the name does not
    /// say is never guessed.
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

/// A resolved scalar type independent of its debug-information encoding.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct BaseType {
    /// The source-facing type name.
    pub name: Arc<str>,

    /// The underlying base-type name, before typedef presentation.
    pub base_name: Arc<str>,

    /// How values of the type are encoded.
    pub encoding: BaseTypeEncoding,

    /// The number of bytes occupied in target storage.
    pub byte_size: u64,

    /// The meaningful low-order bits when the representation is narrower than its storage.
    pub bit_size: Option<u64>,
}

/// An exact integral value with producer-defined signedness.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum IntegerValue {
    Signed(i128),
    Unsigned(u128),
}

/// Whether symbolic integer names came from a language enumeration or associated constants.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum EnumerationOrigin {
    /// A source-language enumeration represented by `DW_TAG_enumeration_type`.
    Language,

    /// Constants associated with a named integer type, as emitted by Go.
    NamedConstants,
}

/// One symbolic name and exact value in an enumeration-like type.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Enumerator {
    /// The producer/source name.
    pub name: Arc<str>,

    /// The exact signed or unsigned value.
    pub value: IntegerValue,
}

/// Stable identity of one normalized type in a module image.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TypeReference {
    /// The immutable image that owns the type.
    pub image: ModuleImageId,

    /// The type's dense identifier within that image.
    pub id: TypeId,
}

/// A source modifier retained as an ordered type-graph node.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum TypeModifier {
    /// C-family `const` qualification.
    Const,

    /// C-family `volatile` qualification.
    Volatile,

    /// C-family `restrict` qualification.
    Restrict,

    /// Atomic qualification.
    Atomic,

    /// Producer-defined immutable qualification.
    Immutable,

    /// Packed representation.
    Packed,

    /// Producer-defined shared qualification.
    Shared,
}

/// The relationship between a named type and its representation target.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum NamedTypeRelationship {
    /// A source-language synonym, such as a C or C++ typedef.
    Synonym,

    /// A source-language type with distinct identity, such as a Go definition.
    Distinct,

    /// A producer-created wrapper describing a language encoding.
    Encoding,

    /// The producer and language do not establish the relationship safely.
    Unspecified,
}

/// The source-level category represented by a DWARF reference type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ReferenceKind {
    Lvalue,
    Rvalue,
}

/// The source-level aggregate category represented by a record type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum RecordKind {
    Struct,
    Class,
}

/// The aggregate storage category that owns a discriminated variant part.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum VariantStorageKind {
    Struct,
    Class,
    Union,
}

/// Source visibility attached to a record member or base class.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Accessibility {
    Public,
    Protected,
    Private,
}

/// A normalized instance-member location within its containing record.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum RecordMemberLayout {
    /// A byte-aligned constant offset from the containing object.
    ByteOffset(u64),

    /// An exact bit range from the beginning of the containing object.
    BitRange {
        /// The first bit in the field.
        bit_offset: u64,

        /// The field width in bits.
        bit_size: u64,
    },

    /// A provider-owned computation requiring a concrete containing-object address.
    Runtime,
}

/// One instance member in a normalized record type.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct RecordMember {
    /// The source member name; anonymous members have no name.
    pub name: Option<Arc<str>>,
    pub type_ref: TypeReference,

    /// Its location within a containing instance.
    pub layout: RecordMemberLayout,

    /// Its normalized source accessibility.
    pub accessibility: Accessibility,

    /// Whether the producer marked this member as compiler-generated.
    pub artificial: bool,

    /// Whether a producer such as Go marked this as an embedded field.
    pub embedded: bool,

    /// Its declaration location, when supplied by debug metadata.
    pub declaration: Option<SourceLocation>,
}

/// Whether a base-class subobject is virtual.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum BaseClassVirtuality {
    /// An ordinary non-virtual base.
    None,
    Virtual,
}

/// One base-class subobject in a normalized class type.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct BaseClass {
    pub type_ref: TypeReference,

    /// Its location within the derived object.
    pub layout: RecordMemberLayout,

    /// Its normalized source accessibility.
    pub accessibility: Accessibility,

    /// Whether this is a virtual base.
    pub virtuality: BaseClassVirtuality,
}

/// One exact or inclusive-range selector for a discriminated variant.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum VariantSelector {
    /// One exact discriminator value.
    Value(IntegerValue),

    /// An inclusive discriminator range.
    Range {
        /// The first selected value.
        low: IntegerValue,

        /// The last selected value.
        high: IntegerValue,
    },
}

/// How a variant is selected by the discriminator.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum VariantSelection {
    /// The fallback when no explicit selector matches.
    Default,

    /// One or more exact values or inclusive ranges.
    Selectors(Arc<[VariantSelector]>),
}

/// A stored discriminator member, a tag type without runtime storage, or
/// neither.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum VariantDiscriminant {
    /// A concrete member whose location supplies the runtime discriminator.
    Stored(RecordMember),

    /// A tag type is described but no discriminator field exists in storage.
    TagType(TypeReference),

    /// Nothing is stored or described, as rustc describes a sum type only
    /// one of whose variants can hold a value, such as `Result<T,
    /// Infallible>`, or none can, such as `Infallible`.
    Absent,
}

/// One variant and the components selected with it.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Variant {
    /// The variant DIE name, when supplied independently of its components.
    pub name: Option<Arc<str>>,

    /// Its discriminator selection rule.
    pub selection: VariantSelection,

    /// Components in producer/source order.
    pub members: Arc<[RecordMember]>,
}

/// The normalized shape of a debug type.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum TypeKind {
    /// A directly encoded scalar base type.
    Base(BaseType),

    /// An integral representation with ordered symbolic names.
    Enumeration {
        /// The normalized integral storage representation.
        representation: BaseType,

        /// The producer-supplied underlying type edge, when present.
        underlying: Option<TypeReference>,

        /// Symbolic names in producer/source order. Duplicate values are preserved.
        enumerators: Arc<[Enumerator]>,

        /// Whether this is a language enum or a named integer with associated constants.
        origin: EnumerationOrigin,

        /// Whether the producer marked enumerator names as scoped.
        scoped: bool,
    },

    /// A thin pointer. A missing target represents an unspecified pointee such as `void`.
    Pointer {
        /// The pointed-to type, when supplied by the producer.
        target: Option<TypeReference>,

        /// The target-specific DWARF address class; zero is the default class.
        address_class: u64,
    },

    /// A language reference represented by an address-like value.
    Reference {
        /// Whether this is an lvalue or rvalue reference.
        kind: ReferenceKind,
        target: TypeReference,

        /// The target-specific DWARF address class; zero is the default class.
        address_class: u64,
    },

    /// A statically bounded array with one or more dimensions.
    Array {
        element: TypeReference,

        /// Dimensions in source order.
        dimensions: Arc<[ArrayDimension]>,

        /// Which dimension's elements are adjacent in memory.
        ordering: ArrayOrdering,
    },

    /// An array whose bounds, or whose elements' place, the program decides
    /// at run time, as a Fortran array's descriptor, an Ada array of an
    /// unconstrained type, and a C variable-length array do. Reading a
    /// value finds them; a C flexible array member's count is never known.
    RuntimeArray {
        element: TypeReference,

        /// Dimensions in source order.
        dimensions: Arc<[RuntimeDimension]>,

        /// Which dimension's elements are adjacent in memory.
        ordering: ArrayOrdering,
    },

    /// A language slice descriptor with a runtime element count.
    Slice {
        element: TypeReference,

        /// Which of the descriptor's words hold its parts.
        words: SliceWords,

        /// Whether the elements are the language's text, as in Rust's `str`
        /// and Zig's `[]const u8`.
        text: bool,
    },

    /// A structure or class with ordered instance members and base subobjects.
    Record {
        /// The source aggregate category.
        kind: RecordKind,

        /// Direct instance members in producer/source order.
        members: Arc<[RecordMember]>,

        /// Base-class subobjects in producer/source order.
        bases: Arc<[BaseClass]>,

        /// Whether this is a declaration without a complete layout.
        incomplete: bool,
    },

    /// Overlapping aggregate storage without a producer-described active member.
    Union {
        /// Alternative member interpretations in producer/source order.
        members: Arc<[RecordMember]>,

        /// Whether this is a declaration without a complete layout.
        incomplete: bool,
    },

    /// Aggregate storage whose active components are selected by a discriminator.
    Variant {
        /// The containing aggregate category.
        storage: VariantStorageKind,

        /// Ordinary members outside the variant part.
        common_members: Arc<[RecordMember]>,

        /// Base subobjects outside the variant part.
        bases: Arc<[BaseClass]>,

        /// Stored discriminator or tag-only metadata.
        discriminant: Box<VariantDiscriminant>,

        /// Variants in producer/source order.
        variants: Arc<[Variant]>,

        /// Whether the containing aggregate has no complete layout.
        incomplete: bool,
    },

    /// An ordered source modifier around another type.
    Modified {
        /// The modifier at this graph node.
        modifier: TypeModifier,
        target: TypeReference,
    },

    /// A named type relationship described by a producer wrapper.
    Named {
        /// The representation target, absent for an incomplete declaration.
        target: Option<TypeReference>,

        /// The source or producer relationship to that target.
        relationship: NamedTypeRelationship,
    },

    /// A deliberately unspecified type such as C `void`.
    Unspecified,

    /// A function value, such as Go's `func`: null, or a pointer to a
    /// context whose first word is the code it calls and whose rest holds
    /// what a closure captured.
    Function,

    /// The type of a function's code, as C's `int (int)`, which values have
    /// only through pointers to it.
    Signature {
        /// What the function returns, or `None` for nothing.
        returns: Option<TypeReference>,

        /// The parameters' types, in order.
        parameters: Arc<[TypeReference]>,

        /// Whether more arguments may follow the parameters, as C's `...`.
        variadic: bool,

        /// Whether the parameters were declared, which C distinguishes:
        /// `int (void)` takes none, while `int ()` says nothing.
        prototyped: bool,
    },

    /// A valid type whose value shape is not implemented yet.
    Opaque {
        /// A stable description of the unsupported DWARF type tag.
        description: Arc<str>,
    },
}

/// Where a slice descriptor keeps its parts: each is one target word,
/// counted in words from the descriptor's start. A descriptor may hold
/// other words too, as an Odin dynamic array's allocator.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SliceWords {
    /// The pointer to the first element.
    pub data: u8,

    /// How many elements there are.
    pub length: u8,

    /// How many elements there is room for, when the descriptor says, as
    /// Go's slices do.
    pub capacity: Option<u8>,
}

impl SliceWords {
    /// A pointer then a length, as Rust's and Zig's slices are.
    pub const POINTER_LENGTH: Self = Self {
        data: 0,
        length: 1,
        capacity: None,
    };

    /// A length then a pointer, as D's slices are.
    pub const LENGTH_POINTER: Self = Self {
        data: 1,
        length: 0,
        capacity: None,
    };

    /// A pointer, a length, then a capacity, as Go's slices are.
    pub const POINTER_LENGTH_CAPACITY: Self = Self {
        data: 0,
        length: 1,
        capacity: Some(2),
    };

    /// How many words the parts span, from the descriptor's start.
    #[must_use]
    pub fn span(self) -> u64 {
        u64::from(self.data.max(self.length).max(self.capacity.unwrap_or(0))) + 1
    }
}

/// How an array of several dimensions lays out its elements.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ArrayOrdering {
    /// Row by row, the last index varying fastest, as C's arrays are.
    RowMajor,

    /// Column by column, the first index varying fastest, as Fortran's
    /// arrays are.
    ColumnMajor,
}

/// One statically known array dimension.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ArrayDimension {
    /// The source lower bound.
    pub lower_bound: i128,

    /// The number of elements in this dimension.
    pub count: u64,
}

/// One dimension of an array bounded at run time.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct RuntimeDimension {
    /// The first index.
    pub lower_bound: ArrayBound,

    /// Where the dimension ends.
    pub extent: ArrayExtent,

    /// The distance in bytes from one element of the dimension to the
    /// next, when the producer gives one rather than the elements being
    /// adjacent.
    pub byte_stride: Option<ArrayBound>,
}

/// Where a dimension of an array bounded at run time ends.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ArrayExtent {
    /// At its last index.
    Upper(ArrayBound),

    /// After its count of elements.
    Count(ArrayBound),

    /// Nowhere the producer says, as for a C flexible array member: its
    /// elements are reached only by index.
    Unknown,
}

/// A bound, count, or stride of an array bounded at run time.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ArrayBound {
    /// Known when the program is loaded.
    Constant(i128),

    /// The value of an expression of the producer's, from where the array
    /// is and the frame reading it, as an integer of `byte_size` bytes.
    Computed { byte_size: u8, signed: bool },

    /// What the program stored where an expression of the producer's says,
    /// an integer of `byte_size` bytes.
    Stored { byte_size: u8, signed: bool },

    /// The value of the program's variable whose debugging entry is at
    /// this offset in `.debug_info`.
    Variable { debug_info_offset: u64 },
}

/// Immutable, normalized metadata for one type-graph node.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct TypeInfo {
    /// Stable identity within the owning module image.
    pub reference: TypeReference,

    /// Producer name or a deterministic structural presentation.
    pub name: Arc<str>,

    /// Storage size in bytes, when known.
    pub byte_size: Option<u64>,

    /// The node's normalized shape.
    pub kind: TypeKind,

    /// What the type is, independent of how a producer spells its name:
    /// present for every type the producer names.
    pub identity: Option<Arc<TypeIdentity>>,
}

impl TypeInfo {
    /// The type's name with the path its producer's name leaves out, as in
    /// `std::vector<int, std::allocator<int> >`. Go and Zig names already
    /// carry their packages and modules.
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

/// The source language of the unit that defines a type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[non_exhaustive]
pub enum SourceLanguage {
    /// Any version of C.
    C,

    /// Any version of C++.
    Cpp,
    Rust,
    Go,

    /// Zig, whichever backend produced it.
    Zig,

    /// Odin, whose units say they are C99.
    Odin,

    /// Fortran, of any standard.
    Fortran,

    /// D.
    D,

    /// Nim, whose units are the C it compiles each module to.
    Nim,

    /// Ada, of any standard.
    Ada,

    /// Another language, by its DWARF language code.
    Other(u16),

    /// The unit does not say.
    Unknown,
}

/// What a named type is: its language, where it is declared, its base name,
/// and its arguments.
///
/// Two instances of one template have the same path and base and differ in
/// their arguments. Identities never depend on how a producer spells a name:
/// inline namespaces are removed from paths, and arguments refer to types
/// rather than to their spellings.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct TypeIdentity {
    /// The language of the unit that defines the type.
    pub language: SourceLanguage,

    /// The enclosing namespaces, modules, packages, types, and functions,
    /// outermost first, with inline namespaces removed.
    pub path: Arc<[Arc<str>]>,

    /// The inline namespaces among the enclosing scopes, such as libc++'s
    /// `__1`, which a name may spell or omit.
    pub inline_namespaces: Arc<[Arc<str>]>,

    /// The name without its path or arguments: `vector`, `Vec`, `Aligned`.
    pub base: Arc<str>,

    /// Template or generic arguments by position, with packs flattened.
    pub arguments: Arc<[TypeArgument]>,

    /// Where a C++ template parameter pack's arguments begin among
    /// `arguments`, when the type has a pack, even an empty one.
    pub pack: Option<usize>,

    /// Where the arguments came from.
    pub origin: ArgumentOrigin,

    /// What Go's runtime records about the type, for Go types.
    pub go: Option<GoTypeAttributes>,
}

/// One template or generic argument.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum TypeArgument {
    Type(TypeReference),

    /// An integral value, such as an array length.
    Value(IntegerValue),

    /// An argument the debugger cannot resolve, as the name spells it. It
    /// matches only a wildcard.
    Unknown(Arc<str>),
}

/// Where a type identity's arguments came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ArgumentOrigin {
    /// Template parameter entries, or Go's key and element attributes.
    /// Values the entries omit, such as Rust's const generic arguments,
    /// come from the name.
    Dwarf,

    /// The producer described no parameters, so the name was parsed and its
    /// arguments resolved through the image's types.
    ParsedName,

    /// The type has no arguments.
    None,
}

/// What Go records about a type for its runtime.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct GoTypeAttributes {
    /// The type's kind, which says what it is whatever it is named.
    pub kind: GoKind,

    /// The offset of the type's runtime descriptor from `runtime.types`.
    pub runtime_type: Option<u64>,
}

/// A Go type's kind, as `internal/abi.Kind` numbers it.
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

    /// A number this version does not know, or zero, which Go gives the
    /// types it synthesizes for its own runtime.
    Other(u8),
}

impl GoKind {
    /// The kind `internal/abi.Kind` numbers `value`.
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

/// One finalized node in an image's immutable normalized type graph.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum TypeNode {
    /// A type whose normalized metadata is available.
    Resolved(TypeInfo),

    /// A type DIE whose metadata is defective and cannot be interpreted safely.
    Malformed {
        /// Stable identity within the owning module image.
        reference: TypeReference,

        /// A stable description of the defect.
        description: Arc<str>,
    },
}

impl TypeNode {
    /// Returns this node's stable identity.
    #[must_use]
    pub const fn reference(&self) -> TypeReference {
        match self {
            Self::Resolved(info) => info.reference,
            Self::Malformed { reference, .. } => *reference,
        }
    }
}

/// Exact target bits for a supported floating-point value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum FloatValue {
    /// IEEE binary16 bits: C's `_Float16`, and Rust's and Zig's `f16`.
    Binary16(u16),

    /// bfloat16 bits: binary32's top half, as C's `__bf16`.
    BFloat16(u16),

    /// IEEE binary32 bits.
    Binary32(u32),

    /// IEEE binary64 bits.
    Binary64(u64),

    /// IEEE binary128 bits: C's `__float128`, and Rust's and Zig's `f128`.
    Binary128(u128),

    /// The meaningful 80 bits of an x87 extended value.
    X87Extended {
        /// The explicit integer bit and fraction.
        significand: u64,

        /// The sign and biased exponent.
        sign_exponent: u16,
    },
}

/// A decoded scalar value.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ScalarValue {
    Boolean(bool),

    /// A sign-extended integer.
    Signed(i128),
    Unsigned(u128),

    /// A binary floating-point value retained as exact target bits.
    Floating(FloatValue),

    /// A complex number, its parts retained as exact target bits.
    Complex {
        real: FloatValue,
        imaginary: FloatValue,
    },
}

/// A decoded thin pointer or reference representation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AddressValue {
    /// The target virtual address represented by the value.
    pub address: VirtualAddress,

    /// The function a pointer to code enters, by name, when the address is
    /// a function's first instruction in a loaded module.
    pub function: Option<Arc<str>>,
}

/// A decoded variable, child, or dereferenced value.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum VariableValue {
    /// A supported scalar value.
    Scalar(ScalarValue),

    /// An integral value retaining all exact symbolic matches.
    Enumeration {
        /// The exact target value.
        value: IntegerValue,

        /// Exact symbolic matches in producer/source order, or, when none
        /// equals the value, the flag constants whose bitwise OR it is.
        matches: Arc<[Enumerator]>,
    },

    /// A concrete thin pointer or reference address.
    Address(AddressValue),

    /// A function value. A closure's captured variables are its children.
    Function {
        /// The code it calls, or `None` for a null (Go's nil) function.
        code: Option<VirtualAddress>,

        /// The function that code begins, when the debug information
        /// describes one there.
        function: Option<Arc<str>>,
    },

    /// An optimized pointer with no concrete address representation.
    ImplicitPointer,

    /// An array whose elements are available through explicit child pages.
    Array { dimensions: Arc<[ArrayDimension]> },

    /// A Fortran allocatable array the program has not allocated.
    NotAllocated,

    /// A Fortran pointer to an array the program has not associated with
    /// one.
    NotAssociated,

    /// A decoded language slice whose elements are available through child pages.
    Slice {
        /// Runtime length from the descriptor.
        length: u64,

        /// Runtime capacity when present in the descriptor.
        capacity: Option<u64>,
    },

    /// A structure or class whose bases and members are available through child pages.
    Record,

    /// Overlapping union storage whose interpretations are available through child pages.
    Union,

    /// A discriminated aggregate whose selected components are available through child pages.
    Variant {
        /// The decoded stored discriminator; absent for tagless single variants.
        discriminant: Option<IntegerValue>,

        /// The selected variant, or `None` when no selector matched.
        active: Option<Arc<Variant>>,
    },
}

/// How one lazily evaluated child relates to its parent value.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ValueChildRelationship {
    /// One row-major array element and its source-language indices.
    ArrayElement {
        /// The zero-based row-major index.
        index: u64,

        /// One index per source dimension, including each declared lower bound.
        indices: Arc<[i128]>,
    },

    /// One runtime slice element.
    SliceElement {
        /// The zero-based runtime index.
        index: u64,
    },

    /// One direct record, union, common-variant, or active-variant member.
    Member(RecordMember),

    /// One base-class subobject.
    Base(BaseClass),

    /// One element of a value a view presents as a sequence.
    Element {
        /// The zero-based position in the sequence.
        index: u64,
    },

    /// One entry of a value a view presents as a map. The child is the
    /// entry's value.
    Entry {
        /// The zero-based position among the entries.
        index: u64,

        /// The entry's key.
        key: Arc<MapKey>,
    },

    /// One named child a view computes, such as a vector's capacity.
    Field {
        /// The name the view gives it.
        name: Arc<str>,
    },

    /// The value as it is stored, without its view.
    Raw,
}

/// The key of one entry of a value a view presents as a map.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MapKey {
    /// Its source-facing normalized type.
    pub type_info: TypeInfo,

    /// Its current availability and decoded summary.
    pub state: VariableState,
}

/// Which bounded resource prevented complete value materialization.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum InspectionLimit {
    /// Maximum visible variables examined or returned.
    Variables,

    /// Maximum requested target bytes read.
    MemoryBytes,

    /// Maximum number of logical target-memory reads.
    MemoryReads,

    /// Maximum number of value nodes.
    ValueNodes,

    /// Maximum aggregate nesting depth.
    AggregateDepth,

    /// Maximum debug-expression work.
    ExpressionWork,
}

/// Resource ceilings for one logical inspection operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InspectionLimits {
    /// Visible variables that may be examined or returned.
    pub variables: u64,

    /// Value nodes that may be materialized.
    pub value_nodes: u64,

    /// Deepest aggregate path or expansion level.
    pub aggregate_depth: u64,

    /// Logical target-memory reads.
    pub memory_reads: u64,

    /// Requested target-memory bytes.
    pub memory_bytes: u64,

    /// Conservatively reserved debug-expression work units.
    pub expression_work: u64,
}

impl Default for InspectionLimits {
    /// Enough for a frame's values and the text they hold: text is charged
    /// to the same budget, up to [`TextSummary::MAX_BYTES`] per value.
    fn default() -> Self {
        Self {
            variables: 256,
            value_nodes: 512,
            aggregate_depth: 64,
            memory_reads: 256,
            memory_bytes: 64 * 1_024,
            expression_work: 5_120_000,
        }
    }
}

impl InspectionLimits {
    /// Returns the unused allowance after one completed operation.
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

/// Resources consumed while completing one inspection operation.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct InspectionUsage {
    /// Visible variables examined or returned.
    pub variables: u64,

    /// Value nodes materialized.
    pub value_nodes: u64,

    /// Deepest aggregate path or expansion level reached.
    pub aggregate_depth: u64,

    /// Logical target-memory reads attempted.
    pub memory_reads: u64,

    /// Target-memory bytes requested.
    pub memory_bytes: u64,

    /// Conservatively reserved debug-expression work units.
    pub expression_work: u64,
}

/// The exact attempted reservation that exhausted an inspection resource.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InspectionExhaustion {
    /// Resource that could not be reserved.
    pub resource: InspectionLimit,

    /// Configured ceiling for the resource.
    pub limit: u64,

    /// Amount completed before the failed reservation.
    pub used: u64,

    /// Additional amount requested by the failed operation.
    pub requested: u64,
}

/// Whether a bounded inspection operation completed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum InspectionCompletion {
    /// The requested operation completed.
    Complete,

    /// Work stopped at a typed resource boundary.
    Truncated(InspectionExhaustion),
}

impl InspectionCompletion {
    /// Returns the typed exhaustion when this operation was truncated.
    #[must_use]
    pub const fn exhaustion(self) -> Option<InspectionExhaustion> {
        match self {
            Self::Complete => None,
            Self::Truncated(exhaustion) => Some(exhaustion),
        }
    }
}

/// Storage retained by an opaque child capability.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ValueStorage {
    /// Storage in the inferior's virtual address space.
    Memory(VirtualAddress),

    /// Bounded immutable bytes captured while evaluating the parent value.
    Bytes {
        source: VariableValueSource,
        raw: Arc<[u8]>,
        start: usize,
        end: usize,
        address: Option<VirtualAddress>,
    },

    /// A DWARF implicit pointer that must be resolved at the originating frame.
    ImplicitPointer {
        debug_info_offset: u64,
        byte_offset: i64,
    },

    /// Part of a value whose pieces lie in several places.
    Composite(CompositeStorage),
}

/// A value assembled from pieces, of which a storage selects the bits from
/// `start` on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompositeStorage {
    /// Pieces in order, covering the whole value without gaps.
    pub pieces: Arc<[StoragePiece]>,

    /// The first selected bit.
    pub start: u64,
}

/// One piece of a composite value: `size` bits from bit `offset` of the
/// value, held at `location`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoragePiece {
    pub offset: u64,
    pub size: u64,
    pub location: PieceLocation,
}

/// Where a piece's bits are, resolved once at the stop.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PieceLocation {
    /// Memory, from `bit_offset` bits past `address`.
    Memory {
        address: VirtualAddress,
        bit_offset: u64,
    },

    /// Bytes captured at the stop, from `bit_offset` bits into `raw`: a
    /// register's whole contents, least significant byte first, or a value
    /// the expression computed or holds.
    Bytes {
        source: VariableValueSource,
        raw: Arc<[u8]>,
        bit_offset: u64,
    },

    /// A pointer to an object with no address, as in
    /// [`ValueStorage::ImplicitPointer`].
    ImplicitPointer {
        debug_info_offset: u64,
        byte_offset: i64,
    },

    /// Bits the program did not keep.
    Undefined,

    /// Bits this stop cannot provide, such as a register a caller's callee
    /// did not save.
    Unavailable(VariableUnavailableReason),
}

/// Opaque capability for expanding one aggregate at one exact stopped state.
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

    /// The view whose children these are, rather than the stored value's.
    pub(crate) view: Option<ViewChildren>,

    /// Where the elements of an array bounded at run time are, from its
    /// storage, as its value found them.
    pub(crate) placement: Option<Arc<ArrayPlacement>>,
}

/// The bounds and strides one value of an array bounded at run time has.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArrayPlacement {
    pub(crate) dimensions: Arc<[ArrayDimension]>,

    /// Each dimension's distance in bytes between adjacent elements.
    pub(crate) strides: Arc<[i64]>,
}

/// The view a children capability presents through: its elements, then its
/// fields, then a `[raw]` child.
#[derive(Clone)]
pub struct ViewChildren {
    /// The bound view, which only the backend that bound it reads; `None`
    /// for what the debugger presents without a view, such as a sum type's
    /// active variant, which has no fields.
    pub(crate) bound: Option<Arc<dyn std::any::Any + Send + Sync>>,

    /// How many elements precede the fields.
    pub(crate) elements: u64,

    /// How many fields precede the `[raw]` child.
    pub(crate) fields: u64,

    /// For a view presenting the value as another, that value's children,
    /// which are the elements.
    pub(crate) inner: Option<Arc<ValueChildrenReference>>,

    /// Which of those children the elements are, by their positions, when
    /// they are only some of them.
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
    /// Returns the stopped snapshot that owns this capability.
    #[must_use]
    pub const fn stop_id(&self) -> crate::StopId {
        self.stop_id
    }

    /// Returns the deterministic number of children exposed by this capability.
    #[must_use]
    pub const fn total(&self) -> u64 {
        self.total
    }

    /// For the children a view presents, how many of the first are its
    /// elements; its named children, the fields and `[raw]`, follow them.
    #[must_use]
    pub const fn elements(&self) -> Option<u64> {
        match &self.view {
            Some(view) => Some(view.elements),
            None => None,
        }
    }
}

/// Whether an available value exposes lazily evaluated children.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ValueChildren {
    /// Scalars, enumerations, pointers, and references have no structural children.
    NotApplicable,

    /// Children can be requested in arbitrary bounded pages.
    Available(Arc<ValueChildrenReference>),

    /// The aggregate header is valid but its children cannot be evaluated.
    Unavailable(VariableUnavailableReason),
}

/// One child returned from a bounded aggregate page.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValueChild {
    /// Its stable relationship to the page's parent.
    pub relationship: ValueChildRelationship,

    /// Its source-facing normalized type.
    pub type_info: TypeInfo,

    /// Its current availability and decoded summary.
    pub state: VariableState,
}

/// One immutable page of children evaluated at a stopped snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValueChildPage {
    /// The stopped snapshot that authorized the reads.
    pub stop_id: crate::StopId,

    /// The zero-based child offset represented by this page.
    pub offset: u64,

    /// The deterministic number of children exposed by the parent.
    pub total: u64,

    /// Children represented in stable parent order.
    pub children: Arc<[ValueChild]>,

    /// Whether the requested interval completed.
    pub completion: InspectionCompletion,

    /// Resources consumed while producing this page.
    pub usage: InspectionUsage,
}

/// How a variable's current value was obtained.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum VariableValueSource {
    /// Memory in the inferior's virtual address space.
    Memory(VirtualAddress),

    /// A target register containing the complete value.
    Register(RegisterDescriptor),

    /// Debug metadata supplies the value as a constant.
    Constant,

    /// A DWARF expression computes a value that has no storage location.
    Computed,

    /// Optimization retained a referent value but eliminated the pointer's address.
    ImplicitPointer,

    /// The value is assembled from several places, or from part of a
    /// register above its least significant bit, so no one place holds it.
    Composite,
}

/// Why an otherwise available pointer or reference cannot be dereferenced.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum DereferenceUnavailableReason {
    /// The pointer contains the null address.
    Null,

    /// The producer did not supply a concrete pointee type.
    UnspecifiedPointee,

    /// The pointee is a valid type whose value shape is not implemented.
    UnsupportedPointee(Arc<str>),

    /// The pointer uses a target address class the backend cannot interpret.
    AddressClass(u64),

    /// The referent could not be read or reconstructed at this stop.
    Unavailable(VariableUnavailableReason),

    /// The type metadata needed to dereference the value is defective.
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

/// Opaque capability for dereferencing one value from one exact stopped state.
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
    /// Returns the thread or task whose frame produced this capability.
    #[must_use]
    pub const fn context(&self) -> ExecutionContext {
        self.context
    }
}

/// Whether an inspected value can be explicitly dereferenced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DereferenceState {
    /// The value is not a pointer or reference.
    NotApplicable,

    /// Dereference is valid at the capability's exact stopped state.
    Available(Box<DereferenceReference>),

    /// The value is an indirection, but dereference is unavailable for a typed reason.
    Unavailable {
        /// The dereferenced expression's type (the pointee), when it resolves.
        /// `None` when the producer supplied no concrete pointee type.
        pointee: Option<Box<TypeInfo>>,

        /// Why the dereference cannot be performed.
        reason: DereferenceUnavailableReason,
    },
}

/// A valid DWARF feature that variable inspection does not yet implement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum UnsupportedVariableFeature {
    /// Reconstructing a value as it existed at function entry.
    EntryValue,

    /// Recovering a parameter from the caller's call-site metadata.
    ParameterReference,

    /// Evaluating a referenced DIE's location expression.
    CrossDieEvaluation,

    /// Resolving a thread-local storage address.
    Tls,

    /// Reading from a non-default target address space.
    AddressSpace,

    /// Combining multiple or partial location pieces.
    CompositeLocation,

    /// Resolving a pointer to an object that has no concrete location.
    ImplicitPointer,

    /// Reading WebAssembly execution state.
    WasmLocation,

    /// Reading a target register class not captured by this backend.
    RegisterClass,

    /// Applying a typed DWARF operation outside the supported scalar types.
    TypedValue,

    /// Selecting among simultaneous valid locations for one object.
    AlternativeLocations,

    /// Materializing a valid source type representation not modeled by
    /// microscope.
    TypeRepresentation,

    /// Evaluating a valid runtime aggregate layout not modeled by microscope.
    RuntimeAggregateLocation,

    /// Decoding a valid scalar representation not modeled by microscope.
    ScalarRepresentation,

    /// Evaluating a DWARF expression operation microscope does not implement,
    /// such as `DW_OP_GNU_variable_value`.
    ExpressionOperation,

    /// Finding a returned value its calling convention does not say where
    /// to find, as for an aggregate in a language that leaves its own
    /// unspecified.
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

/// One unavailable bit range within a source value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ValueBitRange {
    /// Offset from the least-addressed byte of the value, in bits.
    pub offset: u64,

    /// Number of unavailable bits.
    pub size: u64,
}

/// Evidence that a source value has no complete executable representation.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum OptimizedOutReason {
    /// The defining entry supplies neither a location nor a constant.
    NoLocation,

    /// The selected location description is empty.
    EmptyLocation,

    /// Only the listed portions of a composite value are undefined.
    UndefinedPieces {
        /// Undefined destination ranges in source-value bit coordinates.
        ranges: Arc<[ValueBitRange]>,
    },
}

/// Why the selected frame cannot provide its call-frame address.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum CallFrameUnavailableReason {
    /// The instruction has no usable module-relative context.
    NoInstructionContext,

    /// Unwinding terminated without producing a CFA.
    UnwindTerminated(Arc<str>),

    /// The frame's function left by a tail call, which gave its place on
    /// the stack to the function it jumped to.
    TailCall,

    /// The frame is a suspended task's: its future holds its state, and it
    /// has no place on any stack.
    Suspended,
}

/// Why the value a parameter held on entry cannot be recovered from the
/// call site that passed it.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum EntryValueUnavailableReason {
    /// The frame has no caller, or was entered by a signal.
    NoCaller,

    /// The caller's debug information describes no call returning to it.
    NoCallSite,

    /// The call site calls another function, which jumped to this one.
    TargetMismatch,

    /// The call site's target cannot be identified, so another function
    /// may have jumped to this one.
    UnknownTarget,

    /// Chains of tail calls may have entered the function again since the
    /// call site called it.
    TailCalls,

    /// The call site does not describe the value it passed.
    NoParameter,

    /// A tail call passed it, computed from state the jump discarded.
    DiscardedState,

    /// The caller cannot provide the value its call site passed.
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

/// Why thread-local storage cannot be resolved for this value.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum TlsUnavailableReason {
    /// The module has no loader identity usable by the TLS provider.
    ModuleIdentityUnavailable,

    /// No compatible TLS provider is available.
    ProviderUnavailable,

    /// The provider could not resolve this thread's address.
    LookupFailed(Arc<str>),

    /// A parked task runs on no thread, so has no thread's storage.
    NoThread,
}

/// Why a requested structural value operation cannot be completed.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ValueAccessUnavailableReason {
    /// A pointer type does not name a concrete pointee type.
    UnspecifiedPointee,

    /// A dereference was requested from a non-pointer, non-reference value.
    NotPointerOrReference,

    /// The pointer uses a target-specific address class.
    AddressClass(u64),

    /// Dereferencing a null pointer cannot produce a value.
    NullPointer,

    /// Address arithmetic exceeded the target address width.
    AddressOverflow,

    /// A runtime member expression requires a concrete containing-object address.
    NoConcreteObjectAddress,

    /// The requested bit-field representation is non-integral.
    NonIntegralBitField,

    /// The selected member belongs to a different active variant.
    InactiveVariant(Option<Arc<str>>),

    /// Debug information does not describe what the closure a function
    /// value calls captured, or describes it malformedly.
    UndescribedClosure,

    /// The debug information does not say how many elements an array has,
    /// as for a C flexible array member, so only its elements by index
    /// are values.
    UnknownLength,

    /// An implicit-pointer view falls outside its referenced source object.
    ImplicitPointerOutOfBounds {
        /// Signed byte offset into the referenced object.
        offset: i64,

        /// Number of bytes requested from that offset.
        size: u64,

        /// Size of the referenced object.
        containing_size: u64,
    },
}

/// Why valid variable metadata cannot produce a value at this stop.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum VariableUnavailableReason {
    /// The source value has no complete executable representation.
    OptimizedOut(OptimizedOutReason),

    /// No location-list entry applies at the current instruction.
    UnavailableAtInstruction,

    /// Selecting the location requires an instruction context that is unavailable.
    NoInstructionContext,

    /// The value requires a valid feature outside the current implementation.
    Unsupported(UnsupportedVariableFeature),

    /// A required target-memory interval is not completely readable.
    MemoryInaccessible {
        /// First address requested for the typed value.
        address: VirtualAddress,

        /// Total requested byte count.
        requested: u64,

        /// Contiguous bytes successfully read before the failure.
        completed: u64,

        /// First inaccessible address.
        next_address: VirtualAddress,
    },

    /// A required target register is unavailable.
    RegisterUnavailable(Arc<str>),

    /// A caller frame's value of a register its callees may overwrite was
    /// not saved, so the register's value in that frame is unknown.
    RegisterNotSaved(Arc<str>),

    /// The selected frame cannot provide its call-frame address.
    CallFrameUnavailable(CallFrameUnavailableReason),

    /// The value a parameter held on entry cannot be recovered.
    EntryValue(EntryValueUnavailableReason),

    /// Thread-local storage cannot be resolved for this value.
    TlsUnavailable(TlsUnavailableReason),

    /// A structural value operation cannot be completed.
    ValueAccess(ValueAccessUnavailableReason),

    /// The expression exceeded the debugger's bounded work limits.
    EvaluationLimit,

    /// The thread runs no task of its runtime, such as a thread idle in the
    /// runtime's scheduler.
    NoTask,

    /// A typed live-inspection resource was exhausted.
    InspectionLimit(InspectionExhaustion),

    /// A pointer's target is on the reading frame's stack below the
    /// frame's stack pointer, where only the frame's callees' memory is,
    /// live or freed, so the pointer is stale.
    BelowStackPointer {
        /// The pointer's target.
        address: VirtualAddress,
    },

    /// The variable of a function that can suspend was last written
    /// before the await its current run resumed from, which did not keep
    /// it, so its storage holds whatever has used that memory since.
    NotSavedAcrossAwait {
        /// The line of the await the function resumed from.
        line: LineNumber,
    },

    /// A runtime-sized array or slice index is outside its current bounds.
    IndexOutOfBounds {
        /// Requested source index.
        index: i128,

        /// First valid source index.
        lower_bound: i128,

        /// Number of valid elements.
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

/// Stable category for defective variable metadata.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum VariableMalformedKind {
    /// An attribute has an invalid form or value.
    InvalidAttribute,

    /// A metadata reference has no valid target.
    InvalidReference,

    /// The normalized type graph is inconsistent.
    InvalidTypeGraph,

    /// A location list violates its structural contract.
    InvalidLocationList,

    /// A location expression is malformed.
    InvalidExpression,

    /// Type or aggregate layout metadata is inconsistent.
    InconsistentLayout,

    /// A constant cannot inhabit its declared type.
    InvalidConstant,
}

/// Why one variable's debug metadata is defective.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VariableMalformedReason {
    /// Machine-readable defect category.
    pub kind: VariableMalformedKind,

    /// A stable human-readable diagnosis.
    pub description: Arc<str>,
}

/// Why readable bytes are not a valid value of the declared source type.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum VariableInvalidReason {
    /// A Boolean contains a representation other than zero or one.
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

/// The text a string value holds, read for display.
///
/// The bytes are the program's, which need not be UTF-8; presenting them is
/// the client's choice. A C string's bytes end before its terminating NUL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextSummary {
    /// The bytes read, at most [`TextSummary::MAX_BYTES`].
    pub bytes: Arc<[u8]>,

    /// Whether every byte of the text was read.
    pub completion: TextCompletion,
}

impl TextSummary {
    /// The most bytes of text read for one value.
    pub const MAX_BYTES: usize = 256;

    /// Text of code units wider than a byte, UTF-16 for two bytes and
    /// UTF-32 for four, as at most [`Self::MAX_BYTES`] of UTF-8. A unit
    /// that is no character, such as an unpaired surrogate, is U+FFFD, and
    /// a partial unit at the end is left out. `completion` says whether
    /// the units are all of the text, which they no longer are when their
    /// UTF-8 does not fit.
    #[must_use]
    pub fn from_units(
        raw: &[u8],
        width: usize,
        byte_order: ByteOrder,
        completion: TextCompletion,
    ) -> Self {
        let units = raw.chunks_exact(width).map(|unit| {
            let mut word = [0_u8; 4];
            match byte_order {
                ByteOrder::Little => {
                    word[..unit.len()].copy_from_slice(unit);
                    u32::from_le_bytes(word)
                }
                ByteOrder::Big => {
                    word[4 - unit.len()..].copy_from_slice(unit);
                    u32::from_be_bytes(word)
                }
            }
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
            bytes: Arc::from(text.into_bytes()),
            completion: match completion {
                TextCompletion::Complete if cut => TextCompletion::Truncated { length: None },
                completion => completion,
            },
        }
    }
}

/// Whether a text summary holds all of its text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TextCompletion {
    /// Every byte was read.
    Complete,

    /// More text follows the bytes read: `length` bytes in all, when the
    /// string records its length.
    Truncated { length: Option<u64> },

    /// The text continues into memory that could not be read.
    Unreadable { address: VirtualAddress },

    /// The inspection's budget could not afford reading more of the text:
    /// `length` bytes in all, when the string records its length.
    Limited {
        length: Option<u64>,
        exhaustion: InspectionExhaustion,
    },
}

/// Which view presents a value: where it was written and what it matches.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ViewName {
    /// The view file, or the built-in library it came from.
    pub source: Arc<str>,

    /// The line of its `view` keyword.
    pub line: u32,

    /// Its language and pattern, as written.
    pub header: Arc<str>,

    /// Whether it is an `extend`, which adds to a view.
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

/// What a view presents a value as.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum PresentedShape {
    /// Text, in the state's `text`.
    Text,

    /// Another value, standing for this one.
    Value,

    /// This value as the type it dynamically is, such as a C++ object of a
    /// derived class, a Rust trait object, or a Go interface's value.
    Dynamic,

    /// Nothing, described by the summary, such as `None`.
    Empty,

    /// Elements, which are children.
    Sequence,

    /// Entries, each a key and a value, which are children.
    Map,

    /// Members a view names, which are children, as a C++ `std::tuple`'s
    /// elements are.
    Record,

    /// The value as stored, written another way, such as in hexadecimal.
    Formatted,

    /// The view failed, for the reason in `problem`, so the value shows as
    /// it is stored.
    Raw,
}

/// How many elements or entries a presented sequence or map holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum PresentedCount {
    Exact(u64),

    /// At least this many: the view leaves the count to its generators, and
    /// the inspection's budget ended the count first.
    AtLeast(u64),
}

impl PresentedCount {
    /// How many elements are known to exist.
    #[must_use]
    pub const fn known(self) -> u64 {
        match self {
            Self::Exact(count) | Self::AtLeast(count) => count,
        }
    }
}

/// Why a view could not present a value, or presented only part of it.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ViewProblem {
    /// One of the view's invariants does not hold, so the value is not
    /// what the view describes.
    CheckFailed {
        check: Arc<str>,

        /// The values of the check's sides, when it compares.
        detail: Option<Arc<str>>,
    },

    /// The program state could not provide a value the view needed.
    Unavailable(VariableUnavailableReason),

    /// The view asked for something the debugger refuses at this stop.
    Refused(Arc<str>),

    /// The view declares one count and generates another.
    CountMismatch { declared: u64, generated: u64 },

    /// A linked structure leads back to a node already visited, so the
    /// element at this position would repeat an earlier one.
    Cycle { at: u64 },

    /// A tree is deeper than a view walks, so it is no tree a library
    /// builds.
    TooDeep { depth: u32 },

    /// The generators passed the most elements a view generates without a
    /// count.
    TooMany { limit: u64 },

    /// A kernel the view calls failed: it trapped, returned a failure, or
    /// used the host wrongly.
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

/// How a view presents a value as what it stands for (`docs/views.md`).
/// The stored value beside it is untouched.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Presentation {
    pub view: Arc<ViewName>,
    pub shape: PresentedShape,

    /// The elements or entries a sequence or map holds; `None` for other
    /// shapes.
    pub count: Option<PresentedCount>,

    /// A bounded one-line rendering, in one style for every language.
    pub summary: Arc<str>,

    /// The elements, the view's fields, and a `[raw]` child.
    pub children: ValueChildren,

    /// With [`PresentedShape::Raw`], why the view failed; otherwise why the
    /// summary stopped short.
    pub problem: Option<ViewProblem>,

    /// For a value presented as an integer, as an atomic is, that integer,
    /// so a client can write it as it writes any, such as in hexadecimal.
    pub number: Option<Arc<InspectedValue>>,

    /// For a value a view presents as another, as a `Box` is what it
    /// holds, that value.
    pub presented: Option<Arc<InspectedValue>>,

    /// The drawings the view and its `extend`s offer of the value, in
    /// order, which only the web page draws.
    pub visualizers: Arc<[Visualizer]>,
}

/// The header of the debugger's own presentation of Rust tuples.
pub const RUST_TUPLES: &str = "Rust tuples";

impl Presentation {
    /// Whether this is the debugger's own presentation of a Rust tuple,
    /// which shows its positional fields.
    #[must_use]
    pub fn is_rust_tuple(&self) -> bool {
        &*self.view.source == "microscope" && &*self.view.header == RUST_TUPLES
    }
}

/// A drawing a view offers of a value: the renderer that draws it, and the
/// inputs it hands the renderer, which
/// [`crate::DebuggerHandle::visualizer_inputs`] reads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Visualizer {
    pub renderer: Arc<crate::Renderer>,
    pub inputs: Arc<VisualizerReference>,
}

/// Opaque capability for reading one drawing's inputs at one exact stopped
/// state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VisualizerReference {
    /// The presented value's children, which name its view.
    pub(crate) children: Arc<ValueChildrenReference>,

    /// Which of the view's and its `extend`s' `visualize`s it is.
    pub(crate) index: usize,
}

impl VisualizerReference {
    /// The stop whose state the inputs are read at.
    #[must_use]
    pub fn stop_id(&self) -> crate::StopId {
        self.children.stop_id
    }
}

/// The inspection state of one visible variable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VariableState {
    /// The value was read and decoded exactly.
    Available {
        /// How the bytes were obtained.
        source: VariableValueSource,

        /// Exact bytes materialized for this value in target byte order. Aggregate
        /// summaries generally retain storage only in their opaque child capability.
        raw: Option<Arc<[u8]>>,

        /// The decoded scalar or aggregate summary.
        value: VariableValue,

        /// Explicit lazy dereference state.
        dereference: DereferenceState,

        /// Explicit lazy structural-child state.
        children: ValueChildren,

        /// The text the value holds, for values that are strings: a pointer
        /// to characters, a character array, or a language's string type.
        text: Option<Arc<TextSummary>>,

        /// How a view presents the value, when one applies.
        presentation: Option<Arc<Presentation>>,
    },

    /// Valid metadata does not provide a supported readable value here.
    Unavailable(VariableUnavailableReason),

    /// Bytes were read exactly, but do not form a valid value of the declared type.
    Invalid {
        /// How the bytes were obtained.
        source: VariableValueSource,

        /// Exact invalid bytes in target byte order.
        raw: Arc<[u8]>,

        /// Why the representation is invalid.
        reason: VariableInvalidReason,
    },

    /// This entry's metadata is defective.
    Malformed(VariableMalformedReason),
}

/// The terminal value produced by structural expression inspection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InspectedValue {
    /// The resolved terminal type, when valid and supported.
    pub type_info: Option<TypeInfo>,

    /// The terminal value's current availability and decoded representation.
    pub state: VariableState,

    /// Whether the requested inspection completed.
    pub completion: InspectionCompletion,

    /// Resources consumed while producing this value.
    pub usage: InspectionUsage,
}

/// The source-level role of a visible data object.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum VariableKind {
    /// A formal parameter of the selected function or inline instance.
    Parameter,

    /// A result of the selected function or inline instance that the
    /// debug information names as a variable, such as Go's named results
    /// and its unnamed `~r0`. It holds the value returned once the
    /// function sets it, at the latest as it returns.
    Result,

    /// A local variable declared within the selected function.
    Local,

    /// A data object with static storage described by a module image.
    Global,

    /// A value the function a step out finished returned, shown in the
    /// frame it returned to at the stop that step made.
    Returned,
}

/// The source visibility of a global data object.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum GlobalVariableVisibility {
    /// The producer marks the object as externally visible.
    External,

    /// The object is local to one compilation unit.
    CompilationUnit,
}

/// The static type state retained for a global catalog entry.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum GlobalVariableType {
    /// A normalized type whose top-level node is available.
    Resolved(TypeInfo),

    /// A valid type outside the current inspection contract.
    Unsupported(Arc<str>),

    /// Defective type metadata isolated to this entry.
    Malformed(VariableMalformedReason),
}

/// Immutable source metadata for one global data object.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GlobalVariableInfo {
    /// The identifier within the containing module image.
    pub id: GlobalVariableId,

    /// The unqualified source name.
    pub name: Arc<str>,

    /// The producer-normalized source qualification.
    pub qualified_name: Arc<str>,

    /// The linker identity, when supplied by debug metadata.
    pub linkage_name: Option<Arc<str>>,

    /// The declaration location, when supplied by debug metadata.
    pub declaration: Option<SourceLocation>,

    /// The resolved scalar type or an explicit unsupported/malformed state.
    pub type_info: GlobalVariableType,

    /// Whether the object is external or compilation-unit local.
    pub visibility: GlobalVariableVisibility,
}

/// One structured candidate returned for an ambiguous global selector.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GlobalVariableCandidate {
    /// The candidate within its module image.
    pub id: GlobalVariableId,

    /// The canonical source qualification.
    pub qualified_name: Arc<str>,

    /// The resolved declaration path, when known.
    pub declaration_path: Option<Arc<PathBuf>>,

    /// The declaration location, when known.
    pub declaration: Option<SourceLocation>,
}

/// Stable identity of an evaluated global in a loaded module.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GlobalVariableReference {
    /// The runtime module mapping used for evaluation.
    pub module: ModuleId,

    /// The immutable image containing the catalog entry.
    pub image: ModuleImageId,

    /// The entry within that image.
    pub variable: GlobalVariableId,
}

/// One immutable global entry associated with its runtime module mapping.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadedGlobalVariableInfo {
    /// The module mapping when the image is currently loaded.
    pub module: Option<LoadedModule>,

    /// The immutable module image that owns this entry.
    pub image: ModuleImageId,

    /// The image-level global metadata.
    pub variable: GlobalVariableInfo,
}

/// One bounded page of global catalog metadata.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GlobalVariablePage {
    /// The debugger revision at which loaded modules were enumerated.
    pub revision: u64,

    /// The zero-based offset represented by this page.
    pub offset: u64,

    /// The total number of entries matching the filter.
    pub total: u64,

    /// Deterministically ordered entries in this page.
    pub variables: Arc<[LoadedGlobalVariableInfo]>,
}

/// One variable or parameter visible in the selected stopped frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Variable {
    /// Whether this data object is a parameter or local variable.
    pub kind: VariableKind,

    /// Stable module identity for a global; absent for parameters and locals.
    pub global: Option<GlobalVariableReference>,

    /// The source-level variable name.
    pub name: Arc<str>,

    /// Its declaration location, when supplied by debug metadata.
    pub declaration: Option<SourceLocation>,

    /// Its resolved type, when valid and supported.
    pub type_info: Option<TypeInfo>,

    /// Why a generic value has the type of the shape its code was compiled
    /// for, such as Go's `go.shape.int`, rather than its own type.
    pub unresolved_shape: Option<ShapeUnresolvedReason>,

    /// Its current availability and value.
    pub state: VariableState,
}

/// Why the type argument a generic value has could not be found, so the
/// value shows the shape its code was compiled for.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ShapeUnresolvedReason {
    /// The function has no dictionary of type arguments here.
    NoDictionary,

    /// The dictionary, or its entry for the type, cannot be read.
    DictionaryUnavailable(VariableUnavailableReason),

    /// Optimized code describes its dictionary in the slot where the
    /// function may spill it, which holds a stale value until it does.
    UnreliableDictionary,

    /// The table of the runtime's type descriptors, Go's
    /// `runtime.firstmoduledata`, is not described or cannot be read.
    ModuleDataUnavailable(VariableUnavailableReason),

    /// The dictionary names a type descriptor outside this module's.
    ForeignType,

    /// No type in the debug information has the dictionary's descriptor.
    UndescribedType,

    /// The type the dictionary names is laid out unlike the shape.
    MismatchedShape,

    /// The debug information describing the dictionary or the table is
    /// malformed.
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

/// One value produced by explicitly dereferencing a pointer or reference.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DereferencedValue {
    /// The pointee type.
    pub type_info: TypeInfo,

    /// Its current availability and value.
    pub state: VariableState,

    /// Whether the dereference completed.
    pub completion: InspectionCompletion,

    /// Resources consumed while producing this value.
    pub usage: InspectionUsage,
}

/// Variables inspected from one logical frame of a stopped thread.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VariableSnapshot {
    /// The debugger revision at which the values were read.
    pub revision: u64,

    /// The stopped snapshot that authorized the reads.
    pub stop_id: crate::StopId,

    /// The thread or task whose frame was inspected.
    pub context: ExecutionContext,

    /// The backtrace frame that was inspected.
    pub stack_frame: StackFrameId,

    /// The logical frame whose source scope selected these variables.
    pub frame: crate::PresentedFrame,

    /// The canonical frame address of the inspected activation, which
    /// identifies it while it lives, when its unwind information gives one.
    pub frame_address: Option<VirtualAddress>,

    /// Target data representation used for decoding.
    pub target: TargetDescription,

    /// Visible parameters followed by local variables in source declaration order.
    pub variables: Arc<[Variable]>,

    /// Whether every selected variable was represented.
    pub completion: InspectionCompletion,

    /// Resources consumed while producing this snapshot.
    pub usage: InspectionUsage,
}

/// The target CPU architecture described by a module.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Architecture {
    X86_64,
    Aarch64,
}

/// The byte order used by the target.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ByteOrder {
    /// Least-significant byte first.
    Little,

    /// Most-significant byte first.
    Big,
}

/// The width of an address on the target.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PointerWidth {
    Bits32,
    Bits64,
}

impl PointerWidth {
    /// The size of an address in bytes.
    #[must_use]
    pub const fn bytes(self) -> u8 {
        match self {
            Self::Bits32 => 4,
            Self::Bits64 => 8,
        }
    }
}

/// Platform-independent properties of a debug target.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TargetDescription {
    pub architecture: Architecture,
    pub byte_order: ByteOrder,
    pub pointer_width: PointerWidth,
}

impl TargetDescription {
    /// x86-64: little-endian, with 64-bit pointers.
    pub const X86_64: Self = Self {
        architecture: Architecture::X86_64,
        byte_order: ByteOrder::Little,
        pointer_width: PointerWidth::Bits64,
    };

    /// The target's C base type, as its C compiler lays it out, or `None`
    /// for a target whose C data model microscope does not know.
    #[must_use]
    pub fn c_base_type(&self, ty: CBaseType) -> Option<BaseType> {
        use BaseTypeEncoding as E;
        if self.architecture != Architecture::X86_64 || self.pointer_width != PointerWidth::Bits64 {
            return None;
        }

        // The System V x86-64 data model: LP64, signed `char`, and an x87
        // `long double` padded to sixteen bytes.
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
}

/// One of C's base types, by what it is rather than how it is spelled.
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
    /// Every C base type, in declaration order.
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

    /// The type's shortest spelling, such as `unsigned long`.
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

    /// The type a shortest spelling names.
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|ty| ty.name() == name)
    }
}

/// Why a stopped target-memory read could not continue.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum MemoryReadUnavailableReason {
    /// The target rejected access to the next requested address.
    Inaccessible,
}

impl fmt::Display for MemoryReadUnavailableReason {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Inaccessible => formatter.write_str("memory inaccessible"),
        }
    }
}

/// Whether a stopped target-memory read returned its complete requested range.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum MemoryReadCompletion {
    /// Every requested byte was returned.
    Complete,

    /// A contiguous prefix was returned before target memory became unavailable.
    Incomplete {
        /// The first requested address not represented in `MemoryRead::bytes`.
        next_address: VirtualAddress,

        /// Why reading could not continue.
        reason: MemoryReadUnavailableReason,
    },
}

/// Immutable bytes read from one exact stopped target state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryRead {
    /// The debugger revision at which the bytes were read.
    pub revision: u64,

    /// The stopped snapshot that authorized the read.
    pub stop_id: crate::StopId,

    /// Target properties relevant to address and byte presentation.
    pub target: TargetDescription,

    /// The first requested virtual address.
    pub address: VirtualAddress,

    /// The number of bytes requested.
    pub requested: u64,

    /// The readable contiguous prefix beginning at `address`.
    pub bytes: Arc<[u8]>,

    /// Whether every requested byte was returned.
    pub completion: MemoryReadCompletion,
}

/// A one-based source line number.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct LineNumber(std::num::NonZeroU64);

impl LineNumber {
    /// Creates a line number, returning `None` for zero.
    #[must_use]
    pub const fn new(value: u64) -> Option<Self> {
        match std::num::NonZeroU64::new(value) {
            Some(value) => Some(Self(value)),
            None => None,
        }
    }

    /// Returns the numeric line number.
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

/// A one-based source column number.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ColumnNumber(std::num::NonZeroU64);

impl ColumnNumber {
    /// Creates a column number, returning `None` for zero.
    #[must_use]
    pub const fn new(value: u64) -> Option<Self> {
        match std::num::NonZeroU64::new(value) {
            Some(value) => Some(Self(value)),
            None => None,
        }
    }

    /// Returns the numeric column number.
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

/// A source file referenced by debug information.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceFile {
    pub id: SourceFileId,

    /// The source path resolved from the debug metadata.
    pub path: Arc<PathBuf>,
}

/// A location in a source file.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SourceLocation {
    /// The source file containing the location.
    pub file: SourceFileId,

    /// The one-based source line.
    pub line: LineNumber,

    /// The one-based column, when present in the debug information.
    pub column: Option<ColumnNumber>,
}

/// One numbered line read from a source file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceLine {
    /// The one-based line number.
    pub number: LineNumber,

    /// The source text without its line terminator.
    pub text: Arc<str>,
}

/// Source lines surrounding an execution location.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceContext {
    /// The source file the debug information names.
    pub file: SourceFile,

    /// The file that was read: the recorded path, or where a source path
    /// map found it.
    pub path: Arc<PathBuf>,

    /// The execution location within the file.
    pub location: SourceLocation,

    /// Contiguous source lines ordered by line number.
    pub lines: Arc<[SourceLine]>,
}

/// Static information about a function in a module image.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FunctionInfo {
    pub id: FunctionId,

    /// The source-level function name.
    pub name: Arc<str>,

    /// The linker-visible function name, when known.
    pub linkage_name: Option<Arc<str>>,

    /// The function's declaration location, when known.
    pub declaration: Option<SourceLocation>,

    /// The language of the unit that defines the function.
    pub language: SourceLanguage,

    /// What the function is to unwinding and stepping.
    pub role: CodeRole,

    /// The function whose loop this one is the body of, when a compiler
    /// made a loop's body a function of its own, as Go does for a range
    /// over a function. A step treats the body as its enclosing function's
    /// own code, and the code between them as a call it makes.
    pub enclosing: Option<FunctionId>,

    /// For the function that runs a coroutine, such as the body of a Rust
    /// `async fn`, the coroutine's type: the future the body's state lives
    /// in between polls.
    pub coroutine: Option<TypeId>,

    /// The type arguments a generic function was instantiated with, each
    /// with its parameter's name, as tokio's `poll::<T, S>` names the
    /// future it polls `T`. Empty for a function that is not generic, or
    /// whose debug information names none.
    pub generics: FunctionGenerics,

    /// Whether the function is the program's main subprogram, which a
    /// language's runtime enters from a C `main` of its own, as gfortran's
    /// `MAIN__` is. The name `main` finds it too.
    pub main_subprogram: bool,
}

/// A generic function's type arguments, each with its parameter's name.
pub type FunctionGenerics = Arc<[(Arc<str>, TypeId)]>;

/// A state machine a compiler generated for code that can suspend.
///
/// The future of a Rust `async fn` or `async` block is one. Between polls,
/// its state number says where it waits and which of its variables it
/// saved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CoroutineInfo {
    pub kind: CoroutineKind,

    /// Where the state number lies in the coroutine, and its size.
    pub state: StateMember,

    /// The states, by number.
    pub states: Arc<[CoroutineState]>,

    /// What the coroutine captured, which every state holds: an `async
    /// fn`'s arguments, or the variables an `async` block uses.
    pub captures: Arc<[RecordMember]>,
}

/// What source code a coroutine runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CoroutineKind {
    /// The body of an asynchronous function, whose arguments it captures
    /// and moves into its body as it starts.
    AsyncFunction,

    /// An asynchronous block, which captures the variables it uses.
    AsyncBlock,

    /// An asynchronous closure's body.
    AsyncClosure,
}

/// The member of a coroutine holding its state number.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StateMember {
    /// Its byte offset in the coroutine.
    pub offset: u64,

    /// Its size in bytes.
    pub size: u64,
}

/// One state of a coroutine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CoroutineState {
    /// The state number that selects it.
    pub value: u64,
    pub kind: CoroutineStateKind,

    /// Where the source shows the state: an `await`'s line for a suspended
    /// state, the function's header before it starts, and its end after.
    pub location: Option<SourceLocation>,

    /// The members the state holds besides the captures, laid out from the
    /// coroutine's start: the variables live across its `await`, and the
    /// future it awaits.
    pub saved: Arc<[RecordMember]>,
}

/// What a coroutine's state means.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CoroutineStateKind {
    /// Built but never polled.
    Unresumed,

    /// Finished, having produced its value.
    Returned,

    /// Finished by panicking.
    Panicked,

    /// Waiting at an `await`, the `index`th in the source.
    Suspended { index: u32 },
}

/// Where the function that runs a coroutine goes for each of its states,
/// by decoding its dispatch on the state number.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResumePoints {
    /// The dispatch's code, which runs before any state's own.
    pub dispatch: Arc<[AddressRange<ImageAddress>]>,

    /// Where each state's code begins.
    pub points: Arc<[ResumePoint]>,
}

/// Where the code of one state of a coroutine begins.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResumePoint {
    /// The state number.
    pub state: u64,

    /// Where the dispatch leaves for the state.
    pub address: ImageAddress,

    /// The code that runs from there as the coroutine resumes in the state
    /// before it goes on to new work: for a suspended state, the code of
    /// its await's line that resuming reaches, such as polling the awaited
    /// future again; before the first poll, the code before the body's
    /// first statement.
    pub resumption: Arc<[AddressRange<ImageAddress>]>,
}

impl ResumePoints {
    /// Where the state with number `state` begins.
    #[must_use]
    pub fn point(&self, state: u64) -> Option<&ResumePoint> {
        self.points.iter().find(|point| point.state == state)
    }
}

impl CoroutineInfo {
    /// The state with number `value`.
    #[must_use]
    pub fn state(&self, value: u64) -> Option<&CoroutineState> {
        self.states.iter().find(|state| state.value == value)
    }

    /// The variables a coroutine in `state` holds, as its source names
    /// them, each with whether it is a capture: what the state keeps
    /// across its await, but for the compiler's own members, named `__`;
    /// and the captures while they are the coroutine's own. An async
    /// block's are always; an async function's are its arguments until
    /// its body starts, which moves them into variables of its own.
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

    /// The state a coroutine is in before it is first polled.
    #[must_use]
    pub fn unresumed(&self) -> Option<&CoroutineState> {
        self.states
            .iter()
            .find(|state| state.kind == CoroutineStateKind::Unresumed)
    }
}

impl CoroutineState {
    /// The future a suspended state awaits, among its saved members.
    #[must_use]
    pub fn awaitee(&self) -> Option<&RecordMember> {
        self.saved
            .iter()
            .find(|member| member.name.as_deref() == Some("__awaitee"))
    }
}

/// What a function is to unwinding and stepping, whatever its language.
///
/// The debug-info provider sets it once, when an image loads; unwinding and
/// stepping read roles, never names or languages.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub enum CodeRole {
    /// Code the program's author wrote, or a library they call.
    #[default]
    Ordinary,

    /// Forwards to another function and never shows to a step:
    /// trampolines, ABI wrappers, and code a compiler generates.
    Wrapper,

    /// The language runtime's own machinery: a step passes through it to
    /// user code it calls, and a backtrace marks it.
    RuntimeInternal,

    /// The runtime's code that begins a panic, which calls the program's
    /// deferred functions as it unwinds: a step goes through it into them.
    Panic,

    /// Continues on another stack; only a runtime model can say where.
    StackSwitch,

    /// The outermost frame of any stack: unwinding ends here, complete.
    Outermost,

    /// Entered by a trap, not a call: its caller's instruction is the one
    /// that trapped, not a return address.
    TrapEntry,

    /// The signal-return trampoline a handler returns to: the interrupted
    /// registers are in the kernel's signal frame above it.
    SignalTrampoline,

    /// The runtime's code that gives the thread to a task, which may be
    /// another than the task that switched to the stack it runs on. That
    /// task may have left the thread, so a stack it switched from ends at
    /// the switch.
    Dispatch,
}

/// Describes whether a function instance is emitted out of line or inlined.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CodeInstanceKind {
    /// A physical, independently callable function body.
    OutOfLine,

    /// A function body expanded at a source call site.
    Inline {
        /// The call expression in the containing instance, when described.
        call_site: Option<SourceLocation>,
    },
}

/// Explains how an entry address was selected for a concrete function instance.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryProvenance {
    /// The debug format supplied an explicit entry address.
    Explicit,

    /// A recommended line-program boundary supplied the entry address.
    Statement,

    /// Target-specific instruction analysis proved a post-prologue address.
    AnalyzedPrologue,

    /// The first concrete address range supplied the entry address.
    RangeStart,

    /// The function runs a coroutine, and the entry is where its decoded
    /// dispatch leaves for the state before the first poll: the body's
    /// first statement, past the dispatch.
    CoroutineBody,
}

/// A concrete entry address suitable for a function breakpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BreakpointEntry {
    /// The entry address in the module image.
    pub address: ImageAddress,

    /// How the address was selected.
    pub provenance: EntryProvenance,
}

/// One concrete placement of a source-level function in a module image.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodeInstanceInfo {
    pub id: CodeInstanceId,

    /// The source-level function represented by this instance.
    pub function: FunctionId,

    /// The nearest containing function instance, for an inline expansion.
    pub parent: Option<CodeInstanceId>,

    /// Whether the instance is physical or inlined.
    pub kind: CodeInstanceKind,

    /// Every image-address range occupied by the instance.
    pub ranges: Arc<[AddressRange<ImageAddress>]>,

    /// The preferred location for a function breakpoint, when one exists.
    pub breakpoint_entry: Option<BreakpointEntry>,
}

impl CodeInstanceInfo {
    /// Returns whether the instance contains an image address.
    #[must_use]
    pub fn contains(&self, address: ImageAddress) -> bool {
        self.ranges.iter().any(|range| range.contains(address))
    }
}

/// The kind of entity a linker symbol names.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum SymbolKind {
    /// Machine code entered by calling the symbol's address.
    Function,

    /// An indirect function's resolver, which selects the implementation
    /// that calls through the symbol reach.
    IndirectFunction,

    /// A data object.
    Data,

    /// The object file does not describe what the symbol names.
    Unknown,
}

/// The linkage visibility of a symbol.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum SymbolBinding {
    /// Visible to other objects and preferred during linking.
    Global,

    /// Visible to other objects but overridable by a global definition.
    Weak,

    /// Private to the object that defines it.
    Local,
}

/// Explains how the end of a code symbol's extent was determined.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum SymbolExtentProvenance {
    /// The symbol table declared the symbol's size.
    Declared,

    /// The symbol declared no size, so its extent ends at the first evidence
    /// of other code: the next symbol, the next unwind-table function, or
    /// the end of its section.
    Inferred,
}

/// The machine code named by a code symbol.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SymbolExtent {
    /// The non-empty image-address range beginning at the symbol's address.
    pub range: AddressRange<ImageAddress>,

    /// How the end of the range was determined.
    pub provenance: SymbolExtentProvenance,
}

/// A linker symbol defined by a module image.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SymbolInfo {
    pub id: SymbolId,

    /// The linker-visible symbol name.
    pub name: Arc<str>,

    /// The symbol's image address.
    pub address: ImageAddress,

    /// What the symbol names.
    pub kind: SymbolKind,

    /// The symbol's linkage visibility.
    pub binding: SymbolBinding,

    /// Whether the module's dynamic symbol table exports the symbol.
    pub exported: bool,

    /// The code the symbol names, for a code symbol whose extent lies within
    /// one executable section.
    pub extent: Option<SymbolExtent>,

    /// The storage a data symbol names, for a data symbol defined in an
    /// allocated, non-thread-local section whose declared size fits within
    /// it. The range is empty for an unsized symbol, which names only its own
    /// address.
    pub storage: Option<AddressRange<ImageAddress>>,

    /// What the code the symbol names is to unwinding and stepping, for
    /// code no debug information describes.
    pub role: CodeRole,
}

/// The separate debug file found for a module stripped of its debug
/// information.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DebugFile {
    /// The file the module's debug information and symbols came from.
    Used(Arc<PathBuf>),

    /// A file that names the module but could not be loaded, which leaves
    /// the module as its own file describes it.
    Unusable {
        path: Arc<PathBuf>,
        reason: Arc<str>,
    },
}

/// What became of a module's DWARF, the debug information that names its
/// functions and describes its source lines, types, and variables.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum DebugInformation {
    /// The module has none, so its symbols alone describe its code.
    #[default]
    Absent,

    /// All of it was read.
    Loaded,

    /// Some of it was read; the rest is in a form the debugger does not
    /// read, so the code it describes is described by symbols alone.
    Incomplete {
        /// What was left out, and why.
        reason: Arc<str>,
    },

    /// None of it could be read, so the module's symbols alone describe its
    /// code.
    Unusable {
        /// Why it could not be read.
        reason: Arc<str>,
    },
}

/// A slot of a module's global offset table that the loader fills with a
/// function's address as it relocates the module.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GotSlot {
    /// Where the slot is in the module.
    pub address: ImageAddress,

    /// The function whose address the loader writes there.
    pub target: GotTarget,
}

/// The function a [`GotSlot`] holds once its module is relocated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GotTarget {
    /// A function named for the loader to look up in the loaded modules.
    Import(Arc<str>),

    /// The implementation chosen by this module's indirect function whose
    /// resolver is at this address.
    Indirect(ImageAddress),
}

/// Records which symbol tables a module image provided.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SymbolTableSources {
    /// Whether the image has a static symbol table.
    pub static_table: bool,

    /// Whether the image has a dynamic symbol table.
    pub dynamic_table: bool,

    /// The state of the image's embedded compressed symbol table.
    pub embedded_table: EmbeddedSymbolTable,

    /// The state of a language runtime's own function table, such as Go's
    /// `.gopclntab`, which names, places, and unwinds functions when no
    /// debug information describes them.
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

/// The state of a table an image embeds beside its ELF symbol tables: a
/// symbol table in compressed form (on ELF, the `.gnu_debugdata`
/// `MiniDebugInfo` section), or a language runtime's function table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EmbeddedSymbolTable {
    /// The image embeds no such table.
    Absent,

    /// The embedded table was read.
    Loaded,

    /// The embedded table could not be read, so what it holds is absent.
    Unusable {
        /// Why the table could not be read.
        reason: Arc<str>,
    },
}

/// The symbol whose code extent or data storage contains an address.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SymbolLocation {
    pub symbol: SymbolId,

    /// The linker-visible symbol name.
    pub name: Arc<str>,

    /// What the symbol names.
    pub kind: SymbolKind,

    /// The distance from the symbol's address to the described address.
    pub offset: u64,

    /// How the end of the symbol's extent or storage was determined. An
    /// unsized data symbol named at its own address is `Inferred`.
    pub provenance: SymbolExtentProvenance,
}

impl SymbolInfo {
    /// Returns the source-level spelling of a Rust, C++, or D mangled name, or
    /// `None` when the name is not mangled in a recognized scheme.
    #[must_use]
    pub fn demangled_name(&self) -> Option<String> {
        crate::demangle::demangle(&self.name)
    }

    /// Whether a name a person writes names the symbol: its linker name,
    /// that name without its version, or its demangled spelling, with or
    /// without a C++ function's parameters, as `shapes::scale` names
    /// `_ZN6shapes5scaleEd`, `shapes::scale(double)`.
    #[must_use]
    pub fn answers_to(&self, name: &str) -> bool {
        &*self.name == name
            || self.unversioned_name() == name
            || crate::demangle::spells(&self.name, name)
    }

    /// The name without the version that tells it apart from other
    /// definitions of the name, as `memcpy` for `memcpy@GLIBC_2.2.5`.
    #[must_use]
    pub fn unversioned_name(&self) -> &str {
        match self.name.split_once('@') {
            Some((name, version)) if !name.is_empty() && version != "plt" => name,
            _ => &self.name,
        }
    }
}

impl SymbolLocation {
    /// Returns the source-level spelling of a Rust, C++, or D mangled name, or
    /// `None` when the name is not mangled in a recognized scheme.
    #[must_use]
    pub fn demangled_name(&self) -> Option<String> {
        crate::demangle::demangle(&self.name)
    }
}

/// An allocated section of a module image.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SectionInfo {
    pub id: SectionId,

    /// The section name recorded by the object file.
    pub name: Arc<str>,

    /// The non-empty image-address range the section occupies.
    pub range: AddressRange<ImageAddress>,

    /// Whether the section holds machine code.
    pub executable: bool,

    /// Whether the section is writable at run time.
    pub writable: bool,
}

/// The section containing an address.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SectionLocation {
    pub section: SectionId,

    /// The section name.
    pub name: Arc<str>,

    /// The distance from the section's start to the described address.
    pub offset: u64,

    /// Whether the section holds machine code.
    pub executable: bool,
}

/// What a module image's static metadata says about one image address.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageAddressDescription {
    /// The address that was described.
    pub address: ImageAddress,

    /// The allocated section containing the address, when the image records
    /// section headers.
    pub section: Option<SectionLocation>,

    /// The code symbol whose extent contains the address or, failing that,
    /// the data symbol whose declared storage contains it.
    pub symbol: Option<SymbolLocation>,
}

/// A process address resolved to the loaded module containing it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModuleAddress {
    /// The loaded module containing the address.
    pub module: ModuleId,

    /// The path of the module's image.
    pub path: Arc<PathBuf>,

    /// The image's description of the corresponding image address.
    pub image: ImageAddressDescription,
}

/// A process address resolved against the loaded modules of one stop.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AddressDescription {
    /// The described process virtual address.
    pub address: VirtualAddress,

    /// The module containing the address, or `None` when no loaded module's
    /// image covers it.
    pub module: Option<ModuleAddress>,
}

/// A resolved source and function location in a module image.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageLocation {
    /// The address that was resolved.
    pub address: ImageAddress,

    /// The containing function, when known.
    pub function: Option<FunctionInfo>,

    /// The physical code instance containing the address, when known.
    pub physical_instance: Option<CodeInstanceId>,

    /// Active inline frames at this address.
    pub inline_frames: InlineFrameLookup,

    /// The corresponding source location, when known.
    pub source: Option<SourceLocation>,

    /// The code symbol containing the address, when known.
    pub symbol: Option<SymbolLocation>,
}

/// An ordered set of active inline instances, from outermost to innermost.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InlineChain {
    /// Concrete inline instances in logical call order.
    pub instances: Arc<[CodeInstanceId]>,
}

/// The result of resolving inline frames at one image address.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InlineFrameLookup {
    /// No presentable inline frame is active.
    None,

    /// Exactly one logical inline chain is active.
    Unique(InlineChain),

    /// The debug metadata describes incompatible active chains.
    Ambiguous(Arc<[InlineChain]>),
}

/// The address space in which a breakpoint was resolved.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum BreakpointLocation {
    /// A location relative to an immutable module image.
    Image(ImageAddress),

    /// An absolute location in a running process.
    Virtual(VirtualAddress),
}

/// A resolved execution location in a running process.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecutionLocation {
    /// The loaded module containing the address.
    pub module: ModuleId,

    /// The frame's instruction: where execution stopped in the innermost
    /// frame, or the return address of an outer frame's call.
    pub address: VirtualAddress,

    /// Static metadata resolved from the corresponding module image. An outer
    /// frame's is resolved one byte before its return address, inside the
    /// call it is making.
    pub image: ImageLocation,
}

/// Describes how a stack frame was reconstructed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameKind {
    /// A normal machine-code activation.
    Physical,

    /// A source-level inline expansion within a physical activation.
    Inline,

    /// A signal trampoline activation.
    Signal,

    /// A function that left by a tail call, which replaced its activation
    /// with the frame's below it. Only the debug information's one chain
    /// of tail calls between a call and the frame it entered shows it, and
    /// its state is gone but for what was passed to it.
    TailCall,

    /// An async function or block of a suspended task, whose state is the
    /// future at `object`: no thread runs it, so it has no registers, and
    /// its variables are those its state keeps.
    Async { object: VirtualAddress },

    /// The future at `object`, of type `ty`, that a suspended task's
    /// innermost async frame awaits, which is no async function's.
    Awaited {
        object: VirtualAddress,
        ty: TypeReference,
    },
}

impl FrameKind {
    /// Whether the frame is a suspended task's, read from its future: an
    /// async frame or the future it awaits.
    #[must_use]
    pub const fn is_suspended(self) -> bool {
        matches!(self, Self::Async { .. } | Self::Awaited { .. })
    }
}

/// A platform-independent stack frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StackFrame {
    /// The frame's identifier within the current stop revision.
    pub id: StackFrameId,

    /// Zero-based position, beginning with the stopped frame.
    pub level: u32,

    /// How the frame was reconstructed.
    pub kind: FrameKind,

    /// The loaded module containing the instruction, when known. The
    /// future an [`FrameKind::Awaited`] frame names has no instruction:
    /// its module is the one whose image describes the future's type.
    pub module: Option<ModuleId>,

    /// The exact instruction or resume address for the frame. A suspended
    /// task's async frame has the address its function resumes at, where
    /// that is known, and the future it awaits has none.
    pub instruction: Option<VirtualAddress>,

    /// Whose stack the frame is on. A backtrace changes segment where a
    /// runtime switched stacks.
    pub segment: StackSegment,

    /// The concrete code instance represented by the frame, when known.
    pub code_instance: Option<CodeInstanceId>,

    /// The containing function, when known.
    pub function: Option<FunctionInfo>,

    /// The corresponding source location, when known.
    pub source: Option<SourceLocation>,

    /// The code symbol containing a physical or signal frame's code, with
    /// its offset measured to [`Self::instruction`]. Inline frames carry no
    /// symbol because they are source-level expansions within one.
    pub symbol: Option<SymbolLocation>,

    /// What the frame's code is to stepping and unwinding: its function's
    /// role, or its symbol's where no function describes it.
    pub role: CodeRole,

    /// What the future an [`FrameKind::Awaited`] frame names waits for, as
    /// the view that presents it summarizes it; `None` for any other frame,
    /// or a future no view presents.
    pub awaiting: Option<Arc<str>>,
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
        }
    }
}

/// Explains why a backtrace stopped growing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UnwindTermination {
    /// The unwind metadata declared that no caller exists.
    Complete,

    /// No unwind information covered the supplied instruction.
    NoUnwindInfo { address: VirtualAddress },

    /// The instruction could not be associated with a loaded module.
    ModuleNotFound { address: VirtualAddress },

    /// Valid metadata used a feature not implemented by this debugger.
    UnsupportedUnwindInfo { feature: Arc<str> },

    /// The unwind metadata was malformed.
    CorruptUnwindInfo { description: Arc<str> },

    /// A register required to reconstruct the caller was unavailable.
    RegisterUnavailable { register: Arc<str> },

    /// Inferior memory required by an unwind rule could not be read.
    MemoryReadFailed { address: VirtualAddress },

    /// The reconstructed caller did not make valid progress.
    InvalidCaller { description: Arc<str> },

    /// A runtime switched stacks at the frame, and where the stack it
    /// switched from continues could not be found.
    UnresolvedStackSwitch { reason: Arc<str> },

    /// A suspended task's chain of awaits could not be followed past its
    /// last frame, for this reason.
    BrokenAwaitChain { reason: Arc<str> },

    /// A previously visited frame state was encountered again.
    CycleDetected,

    /// The configured maximum frame count was reached.
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

/// A backtrace and the reason its reconstruction ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Backtrace {
    /// The thread or task whose stack was inspected.
    pub context: ExecutionContext,

    /// Frames ordered from the stopped frame outward.
    pub frames: Arc<[StackFrame]>,

    /// The completion or failure reason for the trace.
    pub termination: UnwindTermination,

    /// The futures frames of the trace drive whose chains of awaits it
    /// shows in part or not at all.
    pub unfollowed: Arc<[UnfollowedFuture]>,
}

/// A future a frame drives whose chain of awaits a backtrace does not show
/// in full.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnfollowedFuture {
    /// The frame that drives the future.
    pub driver: StackFrameId,

    /// Why the chain is not shown, or where it ends.
    pub reason: Arc<str>,
}

impl Backtrace {
    /// The innermost frame of code the program's author wrote or calls,
    /// past a runtime's machinery and the wrappers a compiler writes, as a
    /// runtime's own traceback shows a task: where it waits, not how. The
    /// future a suspended task awaits runs no code, and is never one; nor
    /// is a frame past a runtime's dispatch, which runs the runtime's code
    /// for the thread, not the task's.
    #[must_use]
    pub fn user_frame(&self) -> Option<&StackFrame> {
        self.frames
            .iter()
            .take_while(|frame| frame.role != CodeRole::Dispatch)
            .find(|frame| {
                frame.role == CodeRole::Ordinary && !matches!(frame.kind, FrameKind::Awaited { .. })
            })
    }

    /// For each frame, the level of the frame whose loop it runs as an
    /// iterator: a frame between a loop body that is a function of its own
    /// and the body's enclosing function. A body whose enclosing frame the
    /// trace does not reach marks nothing.
    #[must_use]
    pub fn loop_iterators(&self) -> Vec<Option<u32>> {
        let mut iterators = vec![None; self.frames.len()];

        // The loops whose enclosing frames are still to come, innermost
        // last: each one's function, and where its iterators begin.
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
                for iterator in &mut iterators[start..index] {
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

/// An internal image-address range associated with a source location.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LineEntry {
    pub range: AddressRange<ImageAddress>,
    pub location: SourceLocation,

    /// Whether the row that opened this range is a recommended breakpoint
    /// location (`is_stmt`). Source stepping stops only at statement rows.
    pub statement: bool,
}

/// One ordered row emitted by a source line program.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatementRow {
    /// The image address associated with this row.
    pub address: ImageAddress,

    /// The operation index for architectures with multiple operations per instruction.
    pub operation_index: u64,

    /// The corresponding source location, when the producer supplied one that
    /// can be resolved.
    ///
    /// Control-flow markers remain meaningful on rows without source
    /// attribution, including rows whose line is zero.
    pub location: Option<SourceLocation>,

    /// The producer-defined discriminator for this source position.
    pub discriminator: u64,

    /// Semantic flags associated with the row.
    pub flags: StatementFlags,

    /// The instruction-set identifier supplied by the producer.
    pub isa: u64,

    /// The containing line-program sequence.
    pub sequence: LineSequenceId,

    /// The row's order within its sequence, including equal-address rows.
    pub ordinal: u32,
}

/// Semantic markers attached to one source line-program row.
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

    /// Returns whether the row is a recommended breakpoint location.
    #[must_use]
    pub const fn is_statement(self) -> bool {
        self.0 & Self::IS_STATEMENT != 0
    }

    /// Returns whether the row marks the end of a function prologue.
    #[must_use]
    pub const fn prologue_end(self) -> bool {
        self.0 & Self::PROLOGUE_END != 0
    }

    /// Returns whether the row marks the beginning of a function epilogue.
    #[must_use]
    pub const fn epilogue_begin(self) -> bool {
        self.0 & Self::EPILOGUE_BEGIN != 0
    }
}

/// A module image mapped into a running process.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LoadedModule {
    /// The loaded module's session-scoped identifier.
    pub id: ModuleId,

    /// The corresponding immutable module image.
    pub image: ModuleImageId,

    /// The load bias applied to image addresses.
    pub load_bias: LoadBias,
}

/// Public identity and path for one runtime module mapping.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadedModuleRecord {
    /// The checked runtime mapping.
    pub module: LoadedModule,

    /// The canonical ELF image path.
    pub path: Arc<PathBuf>,
}

/// Immutable process-wide loaded-module registry snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadedModuleSnapshot {
    /// The debugger revision represented by this snapshot.
    pub revision: u64,

    /// Loaded modules ordered by module identifier.
    pub modules: Arc<[LoadedModuleRecord]>,
}

impl LoadedModule {
    pub(crate) const fn main(image: ModuleImageId, load_bias: LoadBias) -> Self {
        Self {
            id: ModuleId::new(0),
            image,
            load_bias,
        }
    }

    /// Converts an image address into a process virtual address.
    pub fn virtual_address(self, address: ImageAddress) -> Result<VirtualAddress> {
        self.load_bias.load(address).ok_or(Error::AddressOverflow)
    }

    /// Converts a process virtual address into an image address.
    pub fn image_address(self, address: VirtualAddress) -> Result<ImageAddress> {
        self.load_bias
            .unload(address)
            .ok_or(Error::AddressOutsideModule)
    }
}
