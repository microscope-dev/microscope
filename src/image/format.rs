use zerocopy::little_endian::{U32, U64};
use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout, Unaligned};

pub const MAGIC: [u8; 8] = *b"USCOPEIM";

pub const FORMAT_VERSION: u32 = 1;

pub const NORMALIZATION_REVISION: u32 = 24;

pub const TABLE_ALIGNMENT: usize = 64;

#[repr(C)]
#[derive(Debug, Clone, Copy, FromBytes, IntoBytes, KnownLayout, Immutable, Unaligned)]
pub struct Header {
    pub magic: [u8; 8],
    pub format: U32,
    pub normalization: U32,

    pub layout: U64,

    pub length: U64,

    pub tables: U32,
    pub architecture: u8,
    pub byte_order: u8,
    pub address_size: u8,
    pub flags: u8,
    pub reserved: [u8; 24],
}

#[repr(C)]
#[derive(Debug, Clone, Copy, FromBytes, IntoBytes, KnownLayout, Immutable, Unaligned)]
pub struct DirectoryEntry {
    pub kind: U32,

    pub stride: U32,
    pub count: U64,

    pub offset: U64,

    pub length: U64,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, FromBytes, IntoBytes, KnownLayout, Immutable, Unaligned)]
pub struct Trailer {
    pub checksum: U64,
}

const _: () = assert!(size_of::<Header>() == 64);
const _: () = assert!(size_of::<DirectoryEntry>() == 32);
const _: () = assert!(size_of::<Trailer>() == 8);

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u32)]
pub enum TableKind {
    Strings = 1,

    Files = 2,

    LineAddresses = 3,

    LineRows = 4,

    LineExtras = 5,

    LineSequences = 6,

    LineRanges = 7,

    StatementIndex = 8,

    ControlBoundaries = 9,

    Paths = 10,

    Symbols = 11,

    SymbolNames = 12,

    SymbolExtents = 13,

    SymbolStorage = 14,

    UnsizedData = 15,

    Sections = 16,

    SectionRanges = 17,

    GotSlots = 18,

    Functions = 19,

    FunctionNames = 20,

    Generics = 21,

    FunctionInstances = 22,

    CodeInstances = 23,

    InstanceRanges = 24,

    CodeRanges = 25,

    RecommendedEntries = 26,

    InstructionStarts = 27,

    Unwind = 28,

    EhFrame = 29,

    DebugFrame = 30,

    EhFrameIndex = 31,

    DebugFrameIndex = 32,

    GoCode = 33,

    GoTable = 34,

    GoFrameSaves = 35,

    Facts = 36,

    ThreadLocals = 37,

    Packages = 38,

    PackagedNames = 39,

    LocalNames = 40,

    Types = 41,

    TypeMembers = 42,

    TypeBases = 43,

    TypeVariants = 44,

    TypeSelectors = 45,

    TypeEnumerators = 46,

    TypeDimensions = 47,

    TypeParameters = 48,

    TypeIdentities = 49,

    IdentityStrings = 50,

    TypeArguments = 51,

    TypeNames = 52,

    TypeBaseNames = 53,

    TypeClasses = 54,

    EnumeratorNames = 55,

    GoRuntimeTypes = 56,

    ExpressionBytes = 57,

    Expressions = 58,

    IndexedAddresses = 59,

    Procedures = 60,

    LocationLists = 61,

    LocationEntries = 62,

    EvaluationUnits = 63,

    BaseTypes = 64,

    ScopeRanges = 65,

    Scopes = 66,

    DataObjects = 67,

    Constants = 68,

    ConstantBytes = 69,

    VariableFunctions = 70,

    FunctionObjects = 71,

    Captures = 72,

    FunctionStarts = 73,

    GoEntries = 74,

    ObjectOffsets = 75,

    DwarfProcedures = 76,

    Globals = 77,

    CallingFunctions = 78,

    TailCalls = 79,

    CallSites = 80,

    SiteParameters = 81,

    CallReturns = 82,

    DictionaryIndices = 83,

    PassedByValue = 84,

    ComplexParts = 85,

    DynamicLayouts = 86,

    ResumeRanges = 87,

    Resumes = 88,

    ResumePoints = 89,

    Held = 90,

    NamedConstants = 91,

    Vtables = 92,

    Producers = 93,

    EmbeddedViews = 94,

    RuntimeDimensions = 95,

    StubFrames = 96,

    PythonRuntime = 97,

    RestoredFrames = 98,

    ProvedFrames = 99,

    V8Runtime = 100,

    ProvedFunctions = 101,
}

impl TableKind {
    pub const ALL: [Self; 101] = [
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
        Self::ProvedFunctions,
    ];

    pub const COUNT: usize = Self::ALL.len();

    pub const fn index(self) -> usize {
        self as usize - 1
    }

    pub fn from_number(number: u32) -> Option<Self> {
        Self::ALL.into_iter().find(|kind| *kind as u32 == number)
    }
}

pub const fn aligned(offset: usize) -> Option<usize> {
    match offset.checked_add(TABLE_ALIGNMENT - 1) {
        Some(end) => Some(end / TABLE_ALIGNMENT * TABLE_ALIGNMENT),
        None => None,
    }
}
