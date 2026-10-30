//! The image's fixed layout: a header, a directory of tables, the tables,
//! and a trailer. Every integer is little-endian and every record has
//! alignment one, so a record's bytes are the same on every host and a
//! table needs no padding between its records.

use zerocopy::little_endian::{U32, U64};
use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout, Unaligned};

/// The first bytes of every image.
pub const MAGIC: [u8; 8] = *b"USCOPEIM";

/// The container's version: the header, directory, and trailer.
pub const FORMAT_VERSION: u32 = 1;

/// What the tables mean. Bump it with any change to how metadata is
/// normalized, even one that changes no record's layout, such as a
/// demangling rule, so that no cached image built before is used.
pub const NORMALIZATION_REVISION: u32 = 17;

/// Every table starts at a multiple of this.
pub const TABLE_ALIGNMENT: usize = 64;

/// The header at the start of an image.
#[repr(C)]
#[derive(Debug, Clone, Copy, FromBytes, IntoBytes, KnownLayout, Immutable, Unaligned)]
pub struct Header {
    pub magic: [u8; 8],
    pub format: U32,
    pub normalization: U32,

    /// The fingerprint of every record's layout; see [`super::schema`].
    pub layout: U64,

    /// The image's length in bytes, trailer included.
    pub length: U64,

    /// How many directory entries follow the header.
    pub tables: U32,
    pub architecture: u8,
    pub byte_order: u8,
    pub address_size: u8,
    pub flags: u8,
    pub reserved: [u8; 24],
}

/// Where one table is.
#[repr(C)]
#[derive(Debug, Clone, Copy, FromBytes, IntoBytes, KnownLayout, Immutable, Unaligned)]
pub struct DirectoryEntry {
    pub kind: U32,

    /// One record's size in bytes.
    pub stride: U32,
    pub count: U64,

    /// From the image's start, a multiple of [`TABLE_ALIGNMENT`].
    pub offset: U64,

    /// `stride * count`.
    pub length: U64,
}

/// The last bytes of an image: XXH3-64 of every byte before it, which
/// catches truncation and accidental damage before validation reads the
/// tables.
#[repr(C)]
#[derive(Debug, Clone, Copy, FromBytes, IntoBytes, KnownLayout, Immutable, Unaligned)]
pub struct Trailer {
    pub checksum: U64,
}

const _: () = assert!(size_of::<Header>() == 64);
const _: () = assert!(size_of::<DirectoryEntry>() == 32);
const _: () = assert!(size_of::<Trailer>() == 8);

/// What a table holds. The numbers are part of the format; a reader that
/// meets one it does not know rejects the image.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u32)]
pub enum TableKind {
    /// NUL-terminated UTF-8 strings, named by offset.
    Strings = 1,

    /// Source files by their resolved paths.
    Files = 2,

    /// Each line-program row's address.
    LineAddresses = 3,

    /// Each line-program row's location and flags.
    LineRows = 4,

    /// The rare parts of rows that have them, by row.
    LineExtras = 5,

    /// Each line-program sequence's rows.
    LineSequences = 6,

    /// The ranges of code each line describes, ordered for interval
    /// lookups.
    LineRanges = 7,

    /// Statement rows by file and line.
    StatementIndex = 8,

    /// Rows that mark a prologue's end or an epilogue's start, by address.
    ControlBoundaries = 9,

    /// NUL-terminated filesystem paths, which [`super::PathId`] names by
    /// offset: bytes, which need not be UTF-8.
    Paths = 10,

    /// Linker symbols.
    Symbols = 11,

    /// Symbols by name.
    SymbolNames = 12,

    /// The code symbols' extents, for lookups by address.
    SymbolExtents = 13,

    /// The data symbols' storage, for lookups by address.
    SymbolStorage = 14,

    /// Unsized data symbols, each by its one-byte address.
    UnsizedData = 15,

    /// Allocated sections.
    Sections = 16,

    /// The sections, for lookups by address.
    SectionRanges = 17,

    /// The GOT slots the loader fills.
    GotSlots = 18,

    /// Source-level functions.
    Functions = 19,

    /// Functions by name.
    FunctionNames = 20,

    /// Generic functions' type arguments.
    Generics = 21,

    /// Each function's instances, function by function.
    FunctionInstances = 22,

    /// Code instances.
    CodeInstances = 23,

    /// The instances' address ranges, instance by instance.
    InstanceRanges = 24,

    /// The instances' ranges, for lookups by address.
    CodeRanges = 25,

    /// Where function breakpoints on each instance stop.
    RecommendedEntries = 26,

    /// Where instructions are known to begin, by address.
    InstructionStarts = 27,

    /// Facts about the call-frame sections and Go's table.
    Unwind = 28,

    /// `.eh_frame`'s bytes.
    EhFrame = 29,

    /// `.debug_frame`'s bytes.
    DebugFrame = 30,

    /// `.eh_frame`'s entries, for lookups by address.
    EhFrameIndex = 31,

    /// `.debug_frame`'s entries, for lookups by address.
    DebugFrameIndex = 32,

    /// Code the Go toolchain compiled, for lookups by address.
    GoCode = 33,

    /// Go's function table's bytes.
    GoTable = 34,

    /// Where each Go function keeps its caller's frame pointer saved.
    GoFrameSaves = 35,

    /// Which symbol tables the image provided, and whether it has
    /// thread-local storage.
    Facts = 36,

    /// Thread-local variables, by name.
    ThreadLocals = 37,

    /// Packages, by path.
    Packages = 38,

    /// Each function's package and local name.
    PackagedNames = 39,

    /// Functions by their local names within their packages.
    LocalNames = 40,

    /// Types.
    Types = 41,

    /// Record, union, and variant members.
    TypeMembers = 42,

    /// Base classes.
    TypeBases = 43,

    /// Variants of variant types.
    TypeVariants = 44,

    /// The values that select variants.
    TypeSelectors = 45,

    /// Enumerators.
    TypeEnumerators = 46,

    /// Array dimensions.
    TypeDimensions = 47,

    /// Signatures' parameter types.
    TypeParameters = 48,

    /// Named types' identities.
    TypeIdentities = 49,

    /// Identities' scopes, as strings.
    IdentityStrings = 50,

    /// Identities' template and generic arguments.
    TypeArguments = 51,

    /// Types by name.
    TypeNames = 52,

    /// Types by their identity's base.
    TypeBaseNames = 53,

    /// Each type's identity class.
    TypeClasses = 54,

    /// Enumerations by their enumerators' names.
    EnumeratorNames = 55,

    /// Types by Go runtime type descriptor.
    GoRuntimeTypes = 56,

    /// DWARF expressions' operations.
    ExpressionBytes = 57,

    /// DWARF expressions.
    Expressions = 58,

    /// The addresses expressions' indexes name.
    IndexedAddresses = 59,

    /// The procedures expressions call.
    Procedures = 60,

    /// Lists of locations.
    LocationLists = 61,

    /// The locations of lists.
    LocationEntries = 62,

    /// The units expressions were read from.
    EvaluationUnits = 63,

    /// The base types units' typed operations name.
    BaseTypes = 64,

    /// The code of scopes and of the functions variables are in.
    ScopeRanges = 65,

    /// The scopes data objects are declared in.
    Scopes = 66,

    /// Data objects.
    DataObjects = 67,

    /// Data objects' constant values.
    Constants = 68,

    /// The bytes of constant blocks.
    ConstantBytes = 69,

    /// The functions whose frames show data objects.
    VariableFunctions = 70,

    /// Each function's data objects.
    FunctionObjects = 71,

    /// The variables Go closures captured.
    Captures = 72,

    /// Functions by where their code begins.
    FunctionStarts = 73,

    /// Go functions by their entry address.
    GoEntries = 74,

    /// Data objects by their entries' offsets.
    ObjectOffsets = 75,

    /// DWARF procedures by their entries' offsets.
    DwarfProcedures = 76,

    /// The data objects of globals.
    Globals = 77,

    /// What each function says of the calls it makes.
    CallingFunctions = 78,

    /// The tail calls each function makes.
    TailCalls = 79,

    /// Call sites.
    CallSites = 80,

    /// What call sites passed.
    SiteParameters = 81,

    /// Call sites by where their calls return.
    CallReturns = 82,

    /// The dictionary entry each Go shape names.
    DictionaryIndices = 83,

    /// Whether calls pass each C++ class by value.
    PassedByValue = 84,

    /// The float types of complex numbers' parts.
    ComplexParts = 85,

    /// The expressions finding aggregates' children at run time.
    DynamicLayouts = 86,

    /// The code resume points and held ranges name.
    ResumeRanges = 87,

    /// Where each coroutine's body goes for its states.
    Resumes = 88,

    /// Each state a coroutine's body resumes in.
    ResumePoints = 89,

    /// Where async bodies' variables hold their values on resuming.
    Held = 90,

    /// Integer constants by name.
    NamedConstants = 91,

    /// Rust trait objects' vtables.
    Vtables = 92,

    /// The compilers that produced the debug information.
    Producers = 93,

    /// The bytes of the module's own views.
    EmbeddedViews = 94,

    /// Array types' dimensions bounded at run time.
    RuntimeDimensions = 95,

    /// How many words each PLT stub instruction finds pushed, for lookups
    /// by address.
    StubFrames = 96,

    /// The Python runtime the module carries.
    PythonRuntime = 97,

    /// The instructions at which a function has restored registers its
    /// caller keeps, by which, for lookups by address.
    RestoredFrames = 98,

    /// The frames of a runtime's code that no call-frame information
    /// describes, as its instructions and the runtime's conventions prove
    /// them, for lookups by address.
    ProvedFrames = 99,

    /// The V8 runtime the module carries.
    V8Runtime = 100,
}

impl TableKind {
    /// Every kind, in the order tables are laid out.
    pub const ALL: [Self; 100] = [
        Self::Strings,
        Self::Files,
        Self::LineAddresses,
        Self::LineRows,
        Self::LineExtras,
        Self::LineSequences,
        Self::LineRanges,
        Self::StatementIndex,
        Self::ControlBoundaries,
        Self::Paths,
        Self::Symbols,
        Self::SymbolNames,
        Self::SymbolExtents,
        Self::SymbolStorage,
        Self::UnsizedData,
        Self::Sections,
        Self::SectionRanges,
        Self::GotSlots,
        Self::Functions,
        Self::FunctionNames,
        Self::Generics,
        Self::FunctionInstances,
        Self::CodeInstances,
        Self::InstanceRanges,
        Self::CodeRanges,
        Self::RecommendedEntries,
        Self::InstructionStarts,
        Self::Unwind,
        Self::EhFrame,
        Self::DebugFrame,
        Self::EhFrameIndex,
        Self::DebugFrameIndex,
        Self::GoCode,
        Self::GoTable,
        Self::GoFrameSaves,
        Self::Facts,
        Self::ThreadLocals,
        Self::Packages,
        Self::PackagedNames,
        Self::LocalNames,
        Self::Types,
        Self::TypeMembers,
        Self::TypeBases,
        Self::TypeVariants,
        Self::TypeSelectors,
        Self::TypeEnumerators,
        Self::TypeDimensions,
        Self::TypeParameters,
        Self::TypeIdentities,
        Self::IdentityStrings,
        Self::TypeArguments,
        Self::TypeNames,
        Self::TypeBaseNames,
        Self::TypeClasses,
        Self::EnumeratorNames,
        Self::GoRuntimeTypes,
        Self::ExpressionBytes,
        Self::Expressions,
        Self::IndexedAddresses,
        Self::Procedures,
        Self::LocationLists,
        Self::LocationEntries,
        Self::EvaluationUnits,
        Self::BaseTypes,
        Self::ScopeRanges,
        Self::Scopes,
        Self::DataObjects,
        Self::Constants,
        Self::ConstantBytes,
        Self::VariableFunctions,
        Self::FunctionObjects,
        Self::Captures,
        Self::FunctionStarts,
        Self::GoEntries,
        Self::ObjectOffsets,
        Self::DwarfProcedures,
        Self::Globals,
        Self::CallingFunctions,
        Self::TailCalls,
        Self::CallSites,
        Self::SiteParameters,
        Self::CallReturns,
        Self::DictionaryIndices,
        Self::PassedByValue,
        Self::ComplexParts,
        Self::DynamicLayouts,
        Self::ResumeRanges,
        Self::Resumes,
        Self::ResumePoints,
        Self::Held,
        Self::NamedConstants,
        Self::Vtables,
        Self::Producers,
        Self::EmbeddedViews,
        Self::RuntimeDimensions,
        Self::StubFrames,
        Self::PythonRuntime,
        Self::RestoredFrames,
        Self::ProvedFrames,
        Self::V8Runtime,
    ];

    pub const COUNT: usize = Self::ALL.len();

    pub const fn index(self) -> usize {
        self as usize - 1
    }

    pub fn from_number(number: u32) -> Option<Self> {
        Self::ALL.into_iter().find(|kind| *kind as u32 == number)
    }
}

/// Rounds `offset` up to the next table boundary.
pub const fn aligned(offset: usize) -> Option<usize> {
    match offset.checked_add(TABLE_ALIGNMENT - 1) {
        Some(end) => Some(end / TABLE_ALIGNMENT * TABLE_ALIGNMENT),
        None => None,
    }
}
