//! What every table's records look like, and the fingerprint of it all
//! that each image's header carries: an image whose fingerprint differs
//! was written by a build whose records differ, and is not read.

use std::sync::OnceLock;

use super::format::{self, TableKind};
use super::{lines, python};

/// One field of a record: its name, offset, and size.
pub(super) type Field = (&'static str, usize, usize);

/// One table: its kind, its record's name and size, and its fields.
pub(super) struct TableSchema {
    pub kind: TableKind,
    pub record: &'static str,
    pub size: usize,
    pub fields: &'static [Field],
}

/// Every table's layout. A test checks each field against the record's
/// actual offsets and sizes, so this cannot drift from the code.
pub(super) const SCHEMA: &[TableSchema] = &[
    TableSchema {
        kind: TableKind::Strings,
        record: "u8",
        size: 1,
        fields: &[],
    },
    TableSchema {
        kind: TableKind::Files,
        record: "FileRecord",
        size: 4,
        fields: &[("path", 0, 4)],
    },
    TableSchema {
        kind: TableKind::LineAddresses,
        record: "RowAddress",
        size: 8,
        fields: &[("address", 0, 8)],
    },
    TableSchema {
        kind: TableKind::LineRows,
        record: "LineRow",
        size: 12,
        fields: &[
            ("file", 0, 4),
            ("line", 4, 4),
            ("column", 8, 2),
            ("flags", 10, 1),
            ("reserved", 11, 1),
        ],
    },
    TableSchema {
        kind: TableKind::LineExtras,
        record: "LineExtra",
        size: 44,
        fields: &[
            ("row", 0, 4),
            ("operation_index", 4, 8),
            ("discriminator", 12, 8),
            ("isa", 20, 8),
            ("line", 28, 8),
            ("column", 36, 8),
        ],
    },
    TableSchema {
        kind: TableKind::LineSequences,
        record: "LineSequence",
        size: 8,
        fields: &[("first", 0, 4), ("rows", 4, 4)],
    },
    TableSchema {
        kind: TableKind::LineRanges,
        record: "LineRange",
        size: 29,
        fields: &[
            ("start", 0, 8),
            ("end", 8, 8),
            ("prefix_max_end", 16, 8),
            ("row", 24, 4),
            ("statement", 28, 1),
        ],
    },
    TableSchema {
        kind: TableKind::StatementIndex,
        record: "StatementKey",
        size: 16,
        fields: &[("file", 0, 4), ("line", 4, 8), ("row", 12, 4)],
    },
    TableSchema {
        kind: TableKind::ControlBoundaries,
        record: "ControlBoundary",
        size: 4,
        fields: &[("row", 0, 4)],
    },
    TableSchema {
        kind: TableKind::Paths,
        record: "u8",
        size: 1,
        fields: &[],
    },
    TableSchema {
        kind: TableKind::Symbols,
        record: "SymbolRecord",
        size: 32,
        fields: &[
            ("name", 0, 4),
            ("address", 4, 8),
            ("end", 12, 8),
            ("extent_rank", 20, 4),
            ("storage_rank", 24, 4),
            ("kind", 28, 1),
            ("binding", 29, 1),
            ("role", 30, 1),
            ("flags", 31, 1),
        ],
    },
    names(TableKind::SymbolNames),
    intervals(TableKind::SymbolExtents),
    intervals(TableKind::SymbolStorage),
    intervals(TableKind::UnsizedData),
    TableSchema {
        kind: TableKind::Sections,
        record: "SectionRecord",
        size: 21,
        fields: &[
            ("name", 0, 4),
            ("start", 4, 8),
            ("end", 12, 8),
            ("flags", 20, 1),
        ],
    },
    intervals(TableKind::SectionRanges),
    TableSchema {
        kind: TableKind::GotSlots,
        record: "GotRecord",
        size: 21,
        fields: &[
            ("address", 0, 8),
            ("target", 8, 8),
            ("name", 16, 4),
            ("kind", 20, 1),
        ],
    },
    TableSchema {
        kind: TableKind::Functions,
        record: "FunctionRecord",
        size: 57,
        fields: &[
            ("name", 0, 4),
            ("linkage_name", 4, 4),
            ("declaration.file", 8, 4),
            ("declaration.line", 12, 8),
            ("declaration.column", 20, 8),
            ("enclosing", 28, 4),
            ("coroutine", 32, 4),
            ("generics.first", 36, 4),
            ("generics.count", 40, 4),
            ("instances.first", 44, 4),
            ("instances.count", 48, 4),
            ("other_language", 52, 2),
            ("language", 54, 1),
            ("role", 55, 1),
            ("main_subprogram", 56, 1),
        ],
    },
    names(TableKind::FunctionNames),
    TableSchema {
        kind: TableKind::Generics,
        record: "GenericRecord",
        size: 8,
        fields: &[("name", 0, 4), ("argument", 4, 4)],
    },
    TableSchema {
        kind: TableKind::FunctionInstances,
        record: "Member",
        size: 4,
        fields: &[("value", 0, 4)],
    },
    TableSchema {
        kind: TableKind::CodeInstances,
        record: "InstanceRecord",
        size: 54,
        fields: &[
            ("function", 0, 4),
            ("parent", 4, 4),
            ("call_site.file", 8, 4),
            ("call_site.line", 12, 8),
            ("call_site.column", 20, 8),
            ("ranges.first", 28, 4),
            ("ranges.count", 32, 4),
            ("entries.first", 36, 4),
            ("entries.count", 40, 4),
            ("entry", 44, 8),
            ("provenance", 52, 1),
            ("inline", 53, 1),
        ],
    },
    TableSchema {
        kind: TableKind::InstanceRanges,
        record: "RangeRecord",
        size: 16,
        fields: &[("start", 0, 8), ("end", 8, 8)],
    },
    intervals(TableKind::CodeRanges),
    TableSchema {
        kind: TableKind::RecommendedEntries,
        record: "EntryRecord",
        size: 9,
        fields: &[("address", 0, 8), ("provenance", 8, 1)],
    },
    TableSchema {
        kind: TableKind::InstructionStarts,
        record: "StartRecord",
        size: 9,
        fields: &[("address", 0, 8), ("evidence", 8, 1)],
    },
    TableSchema {
        kind: TableKind::Unwind,
        record: "UnwindRecord",
        size: 82,
        fields: &[
            ("eh_frame_base", 0, 8),
            ("text_base", 8, 8),
            ("got_base", 16, 8),
            ("eh_frame_error_offset", 24, 8),
            ("debug_frame_error_offset", 32, 8),
            ("eh_frame_error", 40, 4),
            ("debug_frame_error", 44, 4),
            ("go_table", 48, 8),
            ("go_text", 56, 8),
            ("go_func", 64, 8),
            ("go_major", 72, 4),
            ("go_minor", 76, 4),
            ("flags", 80, 1),
            ("address_size", 81, 1),
        ],
    },
    bytes(TableKind::EhFrame),
    bytes(TableKind::DebugFrame),
    intervals(TableKind::EhFrameIndex),
    intervals(TableKind::DebugFrameIndex),
    intervals(TableKind::GoCode),
    bytes(TableKind::GoTable),
    TableSchema {
        kind: TableKind::GoFrameSaves,
        record: "FrameSaveRecord",
        size: 17,
        fields: &[("start", 0, 8), ("end", 8, 8), ("saved", 16, 1)],
    },
    TableSchema {
        kind: TableKind::Facts,
        record: "FactsRecord",
        size: 41,
        fields: &[
            ("embedded_reason", 0, 4),
            ("runtime_reason", 4, 4),
            ("embedded_table", 8, 1),
            ("runtime_table", 9, 1),
            ("flags", 10, 1),
            ("debug_reason", 11, 4),
            ("debug_file", 15, 1),
            ("address_start", 16, 8),
            ("address_end", 24, 8),
            ("dwarf_reason", 32, 4),
            ("dwarf", 36, 1),
            ("locals_reason", 37, 4),
        ],
    },
    TableSchema {
        kind: TableKind::ThreadLocals,
        record: "ThreadLocalRecord",
        size: 17,
        fields: &[
            ("name", 0, 4),
            ("value", 4, 8),
            ("reason", 12, 4),
            ("place", 16, 1),
        ],
    },
    TableSchema {
        kind: TableKind::Packages,
        record: "PackageRecord",
        size: 8,
        fields: &[("path", 0, 4), ("name", 4, 4)],
    },
    TableSchema {
        kind: TableKind::PackagedNames,
        record: "PackagedRecord",
        size: 8,
        fields: &[("package", 0, 4), ("local", 4, 4)],
    },
    names(TableKind::LocalNames),
    TableSchema {
        kind: TableKind::Types,
        record: "TypeRecord",
        size: 76,
        fields: &[
            ("name", 0, 4),
            ("text", 4, 4),
            ("base_name", 8, 4),
            ("identity", 12, 4),
            ("target", 16, 4),
            ("parts.first", 20, 4),
            ("parts.count", 24, 4),
            ("bases.first", 28, 4),
            ("bases.count", 32, 4),
            ("variants.first", 36, 4),
            ("variants.count", 40, 4),
            ("discriminant", 44, 4),
            ("byte_size", 48, 8),
            ("value", 56, 8),
            ("bit_size", 64, 8),
            ("flags", 72, 2),
            ("kind", 74, 1),
            ("detail", 75, 1),
        ],
    },
    TableSchema {
        kind: TableKind::TypeMembers,
        record: "MemberRecord",
        size: 47,
        fields: &[
            ("name", 0, 4),
            ("ty", 4, 4),
            ("offset", 8, 8),
            ("bit_size", 16, 8),
            ("declaration.file", 24, 4),
            ("declaration.line", 28, 8),
            ("declaration.column", 36, 8),
            ("layout", 44, 1),
            ("accessibility", 45, 1),
            ("flags", 46, 1),
        ],
    },
    TableSchema {
        kind: TableKind::TypeBases,
        record: "BaseRecord",
        size: 23,
        fields: &[
            ("ty", 0, 4),
            ("offset", 4, 8),
            ("bit_size", 12, 8),
            ("layout", 20, 1),
            ("accessibility", 21, 1),
            ("virtual_base", 22, 1),
        ],
    },
    TableSchema {
        kind: TableKind::TypeVariants,
        record: "VariantRecord",
        size: 21,
        fields: &[
            ("name", 0, 4),
            ("selectors.first", 4, 4),
            ("selectors.count", 8, 4),
            ("members.first", 12, 4),
            ("members.count", 16, 4),
            ("default", 20, 1),
        ],
    },
    TableSchema {
        kind: TableKind::TypeSelectors,
        record: "SelectorRecord",
        size: 35,
        fields: &[
            ("low.bits", 0, 16),
            ("low.signed", 16, 1),
            ("high.bits", 17, 16),
            ("high.signed", 33, 1),
            ("range", 34, 1),
        ],
    },
    TableSchema {
        kind: TableKind::TypeEnumerators,
        record: "EnumeratorRecord",
        size: 21,
        fields: &[
            ("name", 0, 4),
            ("value.bits", 4, 16),
            ("value.signed", 20, 1),
        ],
    },
    TableSchema {
        kind: TableKind::TypeDimensions,
        record: "DimensionRecord",
        size: 24,
        fields: &[("lower_bound", 0, 16), ("count", 16, 8)],
    },
    items(TableKind::TypeParameters),
    TableSchema {
        kind: TableKind::TypeIdentities,
        record: "IdentityRecord",
        size: 46,
        fields: &[
            ("path.first", 0, 4),
            ("path.count", 4, 4),
            ("inline.first", 8, 4),
            ("inline.count", 12, 4),
            ("base", 16, 4),
            ("arguments.first", 20, 4),
            ("arguments.count", 24, 4),
            ("pack", 28, 4),
            ("runtime_type", 32, 8),
            ("other_language", 40, 2),
            ("language", 42, 1),
            ("origin", 43, 1),
            ("go_kind", 44, 1),
            ("flags", 45, 1),
        ],
    },
    items(TableKind::IdentityStrings),
    TableSchema {
        kind: TableKind::TypeArguments,
        record: "ArgumentRecord",
        size: 22,
        fields: &[
            ("value.bits", 0, 16),
            ("value.signed", 16, 1),
            ("reference", 17, 4),
            ("kind", 21, 1),
        ],
    },
    names(TableKind::TypeNames),
    names(TableKind::TypeBaseNames),
    items(TableKind::TypeClasses),
    names(TableKind::EnumeratorNames),
    TableSchema {
        kind: TableKind::GoRuntimeTypes,
        record: "RuntimeTypeRecord",
        size: 12,
        fields: &[("offset", 0, 8), ("ty", 8, 4)],
    },
    bytes(TableKind::ExpressionBytes),
    TableSchema {
        kind: TableKind::Expressions,
        record: "ExpressionRecord",
        size: 32,
        fields: &[
            ("bytes", 0, 4),
            ("length", 4, 4),
            ("unit", 8, 4),
            ("addresses.first", 12, 4),
            ("addresses.count", 16, 4),
            ("procedures.first", 20, 4),
            ("procedures.count", 24, 4),
            ("version", 28, 2),
            ("address_size", 30, 1),
            ("dwarf64", 31, 1),
        ],
    },
    TableSchema {
        kind: TableKind::IndexedAddresses,
        record: "IndexedAddressRecord",
        size: 16,
        fields: &[("index", 0, 8), ("address", 8, 8)],
    },
    TableSchema {
        kind: TableKind::Procedures,
        record: "ProcedureRecord",
        size: 12,
        fields: &[("offset", 0, 8), ("list", 8, 4)],
    },
    TableSchema {
        kind: TableKind::LocationLists,
        record: "LocationListRecord",
        size: 8,
        fields: &[("entries.first", 0, 4), ("entries.count", 4, 4)],
    },
    TableSchema {
        kind: TableKind::LocationEntries,
        record: "LocationEntryRecord",
        size: 21,
        fields: &[
            ("start", 0, 8),
            ("end", 8, 8),
            ("expression", 16, 4),
            ("flags", 20, 1),
        ],
    },
    TableSchema {
        kind: TableKind::EvaluationUnits,
        record: "EvaluationUnitRecord",
        size: 19,
        fields: &[
            ("offset", 0, 8),
            ("base_types.first", 8, 4),
            ("base_types.count", 12, 4),
            ("language", 16, 2),
            ("flags", 18, 1),
        ],
    },
    TableSchema {
        kind: TableKind::BaseTypes,
        record: "BaseTypeRecord",
        size: 9,
        fields: &[("offset", 0, 8), ("value_type", 8, 1)],
    },
    TableSchema {
        kind: TableKind::ScopeRanges,
        record: "CodeRange",
        size: 16,
        fields: &[("start", 0, 8), ("end", 8, 8)],
    },
    TableSchema {
        kind: TableKind::Scopes,
        record: "ScopeRecord",
        size: 25,
        fields: &[
            ("ranges.first", 0, 4),
            ("ranges.count", 4, 4),
            ("instance", 8, 4),
            ("go_instance", 12, 4),
            ("lexical_depth", 16, 4),
            ("frame_base", 20, 4),
            ("frame_base_kind", 24, 1),
        ],
    },
    TableSchema {
        kind: TableKind::DataObjects,
        record: "ObjectRecord",
        size: 59,
        fields: &[
            ("name", 0, 4),
            ("declaration.file", 4, 4),
            ("declaration.line", 8, 8),
            ("declaration.column", 16, 8),
            ("scope", 24, 4),
            ("ty", 28, 4),
            ("escaped", 32, 4),
            ("coroutine", 36, 4),
            ("value", 40, 4),
            ("malformed", 44, 4),
            ("debug_info_offset", 48, 8),
            ("kind", 56, 1),
            ("value_kind", 57, 1),
            ("flags", 58, 1),
        ],
    },
    TableSchema {
        kind: TableKind::Constants,
        record: "ConstantRecord",
        size: 17,
        fields: &[("low", 0, 8), ("high", 8, 8), ("kind", 16, 1)],
    },
    bytes(TableKind::ConstantBytes),
    TableSchema {
        kind: TableKind::VariableFunctions,
        record: "VariableFunctionRecord",
        size: 41,
        fields: &[
            ("ranges.first", 0, 4),
            ("ranges.count", 4, 4),
            ("objects.first", 8, 4),
            ("objects.count", 12, 4),
            ("name", 16, 4),
            ("captures.first", 20, 4),
            ("captures.count", 24, 4),
            ("returned_name", 28, 4),
            ("returned_type", 32, 4),
            ("other_language", 36, 2),
            ("language", 38, 1),
            ("returns", 39, 1),
            ("flags", 40, 1),
        ],
    },
    items(TableKind::FunctionObjects),
    TableSchema {
        kind: TableKind::Captures,
        record: "CaptureRecord",
        size: 17,
        fields: &[
            ("name", 0, 4),
            ("offset", 4, 8),
            ("ty", 12, 4),
            ("malformed", 16, 1),
        ],
    },
    TableSchema {
        kind: TableKind::FunctionStarts,
        record: "FunctionStartRecord",
        size: 28,
        fields: &[
            ("start", 0, 8),
            ("end", 8, 8),
            ("prefix_max_end", 16, 8),
            ("function", 24, 4),
        ],
    },
    keyed(TableKind::GoEntries),
    keyed(TableKind::ObjectOffsets),
    TableSchema {
        kind: TableKind::DwarfProcedures,
        record: "DwarfProcedureRecord",
        size: 13,
        fields: &[
            ("offset", 0, 8),
            ("location", 8, 4),
            ("location_kind", 12, 1),
        ],
    },
    TableSchema {
        kind: TableKind::Globals,
        record: "GlobalRecord",
        size: 13,
        fields: &[
            ("object", 0, 4),
            ("qualified_name", 4, 4),
            ("linkage_name", 8, 4),
            ("external", 12, 1),
        ],
    },
    TableSchema {
        kind: TableKind::CallingFunctions,
        record: "CallingFunctionRecord",
        size: 18,
        fields: &[
            ("name", 0, 4),
            ("frame_base", 4, 4),
            ("tail_calls.first", 8, 4),
            ("tail_calls.count", 12, 4),
            ("frame_base_kind", 16, 1),
            ("flags", 17, 1),
        ],
    },
    items(TableKind::TailCalls),
    TableSchema {
        kind: TableKind::CallSites,
        record: "CallSiteRecord",
        size: 54,
        fields: &[
            ("function", 0, 4),
            ("return_address", 4, 8),
            ("target", 12, 8),
            ("enters", 20, 4),
            ("jump_instruction", 24, 8),
            ("jump_lookup", 32, 8),
            ("parameters.first", 40, 4),
            ("parameters.count", 44, 4),
            ("malformed", 48, 4),
            ("target_kind", 52, 1),
            ("flags", 53, 1),
        ],
    },
    TableSchema {
        kind: TableKind::SiteParameters,
        record: "SiteParameterRecord",
        size: 19,
        fields: &[
            ("register", 0, 2),
            ("parameter", 2, 8),
            ("value", 10, 4),
            ("data_value", 14, 4),
            ("flags", 18, 1),
        ],
    },
    keyed(TableKind::CallReturns),
    TableSchema {
        kind: TableKind::DictionaryIndices,
        record: "TypeFactRecord",
        size: 12,
        fields: &[("ty", 0, 4), ("value", 4, 8)],
    },
    TableSchema {
        kind: TableKind::PassedByValue,
        record: "TypeFactRecord",
        size: 12,
        fields: &[("ty", 0, 4), ("value", 4, 8)],
    },
    TableSchema {
        kind: TableKind::ComplexParts,
        record: "ComplexPartRecord",
        size: 16,
        fields: &[("name", 0, 4), ("size", 4, 8), ("ty", 12, 4)],
    },
    TableSchema {
        kind: TableKind::DynamicLayouts,
        record: "DynamicLayoutRecord",
        size: 17,
        fields: &[
            ("aggregate", 0, 4),
            ("first", 4, 4),
            ("second", 8, 4),
            ("expression", 12, 4),
            ("kind", 16, 1),
        ],
    },
    TableSchema {
        kind: TableKind::ResumeRanges,
        record: "CodeRange",
        size: 16,
        fields: &[("start", 0, 8), ("end", 8, 8)],
    },
    TableSchema {
        kind: TableKind::Resumes,
        record: "ResumeRecord",
        size: 24,
        fields: &[
            ("instance", 0, 4),
            ("dispatch.first", 4, 4),
            ("dispatch.count", 8, 4),
            ("points.first", 12, 4),
            ("points.count", 16, 4),
            ("malformed", 20, 4),
        ],
    },
    TableSchema {
        kind: TableKind::ResumePoints,
        record: "ResumePointRecord",
        size: 24,
        fields: &[
            ("state", 0, 8),
            ("address", 8, 8),
            ("resumption.first", 16, 4),
            ("resumption.count", 20, 4),
        ],
    },
    TableSchema {
        kind: TableKind::Held,
        record: "HeldRecord",
        size: 24,
        fields: &[
            ("offset", 0, 8),
            ("state", 8, 8),
            ("ranges.first", 16, 4),
            ("ranges.count", 20, 4),
        ],
    },
    TableSchema {
        kind: TableKind::NamedConstants,
        record: "NamedConstantRecord",
        size: 21,
        fields: &[("name", 0, 4), ("value", 4, 16), ("signed", 20, 1)],
    },
    TableSchema {
        kind: TableKind::Vtables,
        record: "VtableRecord",
        size: 12,
        fields: &[("address", 0, 8), ("ty", 8, 4)],
    },
    items(TableKind::Producers),
    bytes(TableKind::EmbeddedViews),
    TableSchema {
        kind: TableKind::RuntimeDimensions,
        record: "RuntimeDimensionRecord",
        size: 58,
        fields: &[
            ("lower.value", 0, 16),
            ("lower.kind", 16, 1),
            ("lower.byte_size", 17, 1),
            ("lower.signed", 18, 1),
            ("extent.value", 19, 16),
            ("extent.kind", 35, 1),
            ("extent.byte_size", 36, 1),
            ("extent.signed", 37, 1),
            ("stride.value", 38, 16),
            ("stride.kind", 54, 1),
            ("stride.byte_size", 55, 1),
            ("stride.signed", 56, 1),
            ("ends", 57, 1),
        ],
    },
    intervals(TableKind::StubFrames),
    TableSchema {
        kind: TableKind::PythonRuntime,
        record: "PythonRuntimeRecord",
        size: PYTHON_OFFSETS + 4 * python::Field::COUNT + python::Constant::COUNT,
        fields: &[
            ("version", 0, 4),
            ("flags", 4, 2),
            ("reserved", 6, 2),
            ("runtime_start", 8, 8),
            ("runtime_end", 16, 8),
            ("problem", 24, 4),
            ("offsets", PYTHON_OFFSETS, 4 * python::Field::COUNT),
            (
                "constants",
                PYTHON_OFFSETS + 4 * python::Field::COUNT,
                python::Constant::COUNT,
            ),
        ],
    },
    intervals(TableKind::RestoredFrames),
];

/// Where a Python runtime record's offsets begin.
const PYTHON_OFFSETS: usize = 28;

const fn keyed(kind: TableKind) -> TableSchema {
    TableSchema {
        kind,
        record: "Keyed",
        size: 12,
        fields: &[("key", 0, 8), ("value", 8, 4)],
    }
}

const fn items(kind: TableKind) -> TableSchema {
    TableSchema {
        kind,
        record: "Item",
        size: 4,
        fields: &[("value", 0, 4)],
    }
}

const fn bytes(kind: TableKind) -> TableSchema {
    TableSchema {
        kind,
        record: "u8",
        size: 1,
        fields: &[],
    }
}

const fn intervals(kind: TableKind) -> TableSchema {
    TableSchema {
        kind,
        record: "Interval",
        size: 28,
        fields: &[
            ("start", 0, 8),
            ("end", 8, 8),
            ("prefix_max_end", 16, 8),
            ("value", 24, 4),
        ],
    }
}

const fn names(kind: TableKind) -> TableSchema {
    TableSchema {
        kind,
        record: "NameEntry",
        size: 12,
        fields: &[("hash", 0, 4), ("name", 4, 4), ("value", 8, 4)],
    }
}

/// The layout fingerprint: XXH3-64 of the schema's canonical text, with
/// the container's version.
pub(super) fn layout_fingerprint() -> u64 {
    static FINGERPRINT: OnceLock<u64> = OnceLock::new();
    *FINGERPRINT.get_or_init(|| twox_hash::XxHash3_64::oneshot(schema_text().as_bytes()))
}

/// The schema as one line per table.
pub(super) fn schema_text() -> String {
    use std::fmt::Write as _;
    let mut text = format!("format {}\n", format::FORMAT_VERSION);

    for table in SCHEMA {
        let _ = write!(
            text,
            "{} {} {} {}",
            table.kind as u32,
            table.record,
            table.size,
            table.fields.len()
        );
        for (name, offset, size) in table.fields {
            let _ = write!(text, " {name}@{offset}:{size}");
        }

        text.push('\n');
    }

    text
}

/// The name of the record a table holds.
pub(super) fn record(kind: TableKind) -> &'static str {
    SCHEMA
        .iter()
        .find(|table| table.kind == kind)
        .map_or("", |table| table.record)
}

/// The stride the schema gives a table.
pub(super) fn stride(kind: TableKind) -> usize {
    SCHEMA
        .iter()
        .find(|table| table.kind == kind)
        .map_or(0, |table| table.size)
}

const _: () = assert!(size_of::<lines::LineExtra>() == 44);
const _: () = assert!(size_of::<lines::StatementKey>() == 16);
