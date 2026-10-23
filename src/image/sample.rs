//! A small image with every kind of row, which tests and the fuzz harness
//! change to see what validation makes of it.
#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "tests and the fuzz harness build the sample"
)]

use std::path::PathBuf;

use zerocopy::IntoBytes as _;

use super::calls::{self, CallSite, CallView, CallingFunction, Calls, SiteParameter, SiteTarget};
use super::declarations::{self, DeclarationView};
use super::facts::{self, FactsView};
use super::format::Trailer;
use super::functions::{self, FunctionView};
use super::lines::{
    self, FileRecord, LineExtra, LineRange, LineRow, LineSequence, LineTables, Row, RowAddress,
};

use super::locations::{
    self, EvaluationUnit, ExpressionId, LocationListId, LocationTables, LocationsBuilder,
};

use super::packages::{self, PackageView};
use super::resumes::{self, ResumeView};
use super::symbols::{self, SymbolView};
use super::type_facts::{self, TypeFactsView};
use super::types::{self, TypeView};
use super::unwind::{self, UnwindView};
use super::variables::{
    self, Capture, ConstantValue, DataObject, Function, GoDeclaration, Metadata, MetadataAbsence,
    ReturnConvention, SystemV, TypeResolution, ValueDescription, VariableFunctionId, VariableView,
    Variables,
};

use super::{
    Builder, Image, ImageError, Limits, PathId, Paths, PathsBuilder, StringsBuilder, TableKind,
};

use crate::{
    AddressRange, BoundaryEvidence, BreakpointEntry, CodeInstanceId, CodeInstanceInfo,
    CodeInstanceKind, CodeRole, ColumnNumber, EntryProvenance, FunctionId, FunctionInfo, GotSlot,
    GotTarget, ImageAddress, LineNumber, SectionId, SectionInfo, SourceFileId, SourceLanguage,
    SourceLocation, SymbolBinding, SymbolExtent, SymbolExtentProvenance, SymbolId, SymbolInfo,
    SymbolKind, TypeId,
};

pub(super) const TARGET: crate::TargetDescription = crate::TargetDescription::X86_64;

pub(super) fn row(address: u64, file: Option<u32>, line: u64) -> Row {
    Row {
        address,
        file: file.map(SourceFileId::new),
        line,
        column: 0,
        operation_index: 0,
        discriminator: 0,
        isa: 0,
        statement: true,
        prologue_end: false,
        epilogue_begin: false,
        end_sequence: false,
    }
}

pub(super) const fn range(start: u64, end: u64) -> AddressRange<ImageAddress> {
    AddressRange {
        start: ImageAddress::new(start),
        end: ImageAddress::new(end),
    }
}

/// Two sequences with every kind of row: wide lines and columns, extras,
/// rows without files, boundaries, and equal addresses.
pub(super) fn sample_rows() -> Vec<Vec<Row>> {
    vec![
        vec![
            row(0x1000, Some(0), 3),
            Row {
                prologue_end: true,
                column: 7,
                ..row(0x1004, Some(0), 4)
            },
            Row {
                discriminator: 2,
                statement: false,
                ..row(0x1004, Some(0), 5)
            },
            Row {
                line: u64::from(u32::MAX) + 9,
                column: 1 << 20,
                ..row(0x1008, Some(1), 0)
            },
            Row {
                epilogue_begin: true,
                ..row(0x100c, None, 0)
            },
            Row {
                end_sequence: true,
                ..row(0x1010, Some(1), 9)
            },
        ],
        vec![
            row(0x2000, Some(1), 1),
            Row {
                isa: 3,
                operation_index: 1,
                ..row(0x2000, Some(1), 2)
            },
            Row {
                end_sequence: true,
                ..row(0x2008, Some(1), 2)
            },
        ],
    ]
}

pub(super) fn sample() -> (LineTables, lines::Files) {
    let mut files = lines::Files::default();
    files.intern(PathBuf::from("/src/a.c"));
    files.intern(PathBuf::from(
        <std::ffi::OsString as std::os::unix::ffi::OsStringExt>::from_vec(b"/src/\xffb.c".to_vec()),
    ));
    let mut tables = LineTables::default();

    for sequence in sample_rows() {
        tables.begin_sequence().unwrap();
        let mut previous: Option<(u32, Row)> = None;

        for row in sequence {
            let at = tables.push_row(&row).unwrap();

            if let Some((index, open)) = previous.take()
                && open.location().is_some()
                && open.address < row.address
            {
                tables
                    .push_range(range(open.address, row.address), index, open.statement)
                    .unwrap();
            }

            previous = Some((at, row));
        }
    }

    (tables, files)
}

/// Symbols of every shape: code that overlaps, declared and inferred,
/// sized and unsized data, and a symbol that names neither.
pub(super) fn sample_symbols() -> Vec<SymbolInfo> {
    let symbol = |id, name: &str, address, kind| SymbolInfo {
        id: SymbolId::new(id),
        name: name.into(),
        address: ImageAddress::new(address),
        kind,
        binding: SymbolBinding::Global,
        exported: false,
        extent: None,
        storage: None,
        role: CodeRole::Ordinary,
    };

    let code = |range, provenance| Some(SymbolExtent { range, provenance });
    vec![
        SymbolInfo {
            exported: true,
            extent: code(range(0x1000, 0x1010), SymbolExtentProvenance::Declared),
            ..symbol(0, "main", 0x1000, SymbolKind::Function)
        },
        SymbolInfo {
            binding: SymbolBinding::Local,
            role: CodeRole::Wrapper,
            extent: code(range(0x1008, 0x1020), SymbolExtentProvenance::Inferred),
            ..symbol(1, "helper", 0x1008, SymbolKind::IndirectFunction)
        },
        SymbolInfo {
            binding: SymbolBinding::Weak,
            storage: Some(range(0x3000, 0x3008)),
            ..symbol(2, "counter", 0x3000, SymbolKind::Data)
        },
        SymbolInfo {
            storage: Some(range(0x3010, 0x3010)),
            ..symbol(3, "counter", 0x3010, SymbolKind::Data)
        },
        symbol(4, "memcpy@GLIBC_2.2.5", 0x1000, SymbolKind::Unknown),
    ]
}

pub(super) fn sample_sections() -> Vec<SectionInfo> {
    let section = |id, name: &str, range, executable| SectionInfo {
        id: SectionId::new(id),
        name: name.into(),
        range,
        executable,
        writable: !executable,
    };

    vec![
        section(0, ".text", range(0x1000, 0x2000), true),
        section(1, ".data", range(0x3000, 0x3100), false),
    ]
}

pub(super) fn sample_got() -> Vec<GotSlot> {
    vec![
        GotSlot {
            address: ImageAddress::new(0x3100),
            target: GotTarget::Import("puts".into()),
        },
        GotSlot {
            address: ImageAddress::new(0x3108),
            target: GotTarget::Indirect(ImageAddress::new(0x1008)),
        },
    ]
}

/// Functions of every shape: generic, enclosed, coroutine-running, of
/// another language, without code, and inlined.
pub(super) fn sample_functions() -> Vec<FunctionInfo> {
    let function = |id, name: &str| FunctionInfo {
        id: FunctionId::new(id),
        name: name.into(),
        linkage_name: None,
        declaration: None,
        language: SourceLanguage::C,
        role: CodeRole::Ordinary,
        enclosing: None,
        coroutine: None,
        generics: std::sync::Arc::from([]),
        main_subprogram: false,
    };

    vec![
        FunctionInfo {
            linkage_name: Some("_Z4mainv".into()),
            declaration: Some(SourceLocation {
                file: SourceFileId::new(0),
                line: LineNumber::new(u64::from(u32::MAX) + 3).unwrap(),
                column: ColumnNumber::new(7),
            }),
            language: SourceLanguage::Rust,
            coroutine: Some(TypeId::new(4)),
            generics: [("T".into(), TypeId::new(2)), ("U".into(), TypeId::new(3))].into(),
            ..function(0, "main")
        },
        FunctionInfo {
            language: SourceLanguage::Other(0x8001),
            role: CodeRole::Wrapper,
            enclosing: Some(FunctionId::new(0)),
            ..function(1, "helper")
        },
        FunctionInfo {
            main_subprogram: true,
            ..function(2, "declared")
        },
        function(3, "main"),
    ]
}

/// Instances of every shape: physical with two ranges, the same range
/// twice, and an inline expansion with a call site and without.
pub(super) fn sample_instances() -> Vec<CodeInstanceInfo> {
    let entry = |address, provenance| {
        Some(BreakpointEntry {
            address: ImageAddress::new(address),
            provenance,
        })
    };

    vec![
        CodeInstanceInfo {
            id: CodeInstanceId::new(0),
            function: FunctionId::new(0),
            parent: None,
            kind: CodeInstanceKind::OutOfLine,
            ranges: [range(0x1000, 0x1010), range(0x2000, 0x2008)].into(),
            breakpoint_entry: entry(0x1000, EntryProvenance::Explicit),
        },
        CodeInstanceInfo {
            id: CodeInstanceId::new(1),
            function: FunctionId::new(1),
            parent: Some(CodeInstanceId::new(0)),
            kind: CodeInstanceKind::Inline {
                call_site: Some(SourceLocation {
                    file: SourceFileId::new(1),
                    line: LineNumber::new(9).unwrap(),
                    column: None,
                }),
            },
            ranges: [range(0x1004, 0x1008), range(0x1004, 0x1008)].into(),
            breakpoint_entry: entry(0x1004, EntryProvenance::RangeStart),
        },
        CodeInstanceInfo {
            id: CodeInstanceId::new(2),
            function: FunctionId::new(3),
            parent: None,
            kind: CodeInstanceKind::Inline { call_site: None },
            ranges: [range(0x3000, 0x3004)].into(),
            breakpoint_entry: None,
        },
        CodeInstanceInfo {
            id: CodeInstanceId::new(3),
            function: FunctionId::new(0),
            parent: None,
            kind: CodeInstanceKind::OutOfLine,
            ranges: [range(0x4000, 0x4010)].into(),
            breakpoint_entry: entry(0x4008, EntryProvenance::CoroutineBody),
        },
    ]
}

pub(super) fn sample_starts() -> Vec<(ImageAddress, BoundaryEvidence)> {
    vec![
        (ImageAddress::new(0x1000), BoundaryEvidence::FunctionRange),
        (ImageAddress::new(0x2000), BoundaryEvidence::SectionStart),
    ]
}

/// Call-frame sections of arbitrary bytes with overlapping entries and a
/// malformed one, and Go's table.
pub(super) fn sample_unwind() -> unwind::Unwind {
    let index = |entries: &[(u64, u64, u32)], first_error: Option<(u64, &str)>| unwind::FdeIndex {
        entries: super::index::intervals(
            entries
                .iter()
                .map(|&(start, end, offset)| (range(start, end), offset)),
        ),
        first_error: first_error.map(|(offset, text)| (offset, text.to_owned())),
    };

    unwind::Unwind {
        eh_frame: (0..64).collect::<Vec<u8>>().into(),
        debug_frame: (0..32).collect::<Vec<u8>>().into(),
        eh_frame_index: index(
            &[
                (0x1000, 0x1100, 8),
                (0x1080, 0x1200, 24),
                (0x1400, 0x1500, 48),
            ],
            Some((40, "a CIE is malformed")),
        ),
        debug_frame_index: index(&[(0x2000, 0x2100, 0)], None),
        bases: unwind::Bases {
            eh_frame: Some(0x9000),
            text: Some(0x1000),
            got: None,
        },
        big_endian: false,
        address_size: 8,
        go_code: vec![range(0x3000, 0x3100), range(0x3200, 0x3300)],
        go: Some(unwind::GoTableData {
            bytes: (0..16).collect::<Vec<u8>>().into(),
            facts: unwind::GoTableFacts {
                address: 0x8000,
                text: 0x1000,
                go_func: Some(0x8800),
                release: Some((1, 25)),
            },
            frame_saves: vec![Some(0x3004..0x30f0), None],
        }),
        stub_frames: vec![
            (range(0x3400, 0x3406), 0),
            (range(0x3406, 0x340b), 0),
            (range(0x340b, 0x3410), 1),
        ],
        restored: vec![(range(0x3500, 0x350c), 0x3f), (range(0x3508, 0x3510), 2)],
        proved_frames: vec![(range(0x3600, 0x3601), 0), (range(0x3601, 0x3604), 0x21)],
    }
}

pub(super) const fn sample_address_range() -> crate::AddressRange<crate::ImageAddress> {
    crate::AddressRange {
        start: crate::ImageAddress::new(0x40_0000),
        end: crate::ImageAddress::new(0x48_0000),
    }
}

pub(super) fn sample_sources() -> crate::SymbolTableSources {
    crate::SymbolTableSources {
        static_table: true,
        dynamic_table: false,
        embedded_table: crate::EmbeddedSymbolTable::Unusable {
            reason: "the table is truncated".into(),
        },
        runtime_function_table: crate::EmbeddedSymbolTable::Loaded,
    }
}

pub(super) fn sample_thread_locals()
-> std::collections::BTreeMap<std::sync::Arc<str>, Result<crate::ThreadLocal, std::sync::Arc<str>>>
{
    [
        ("counter", Ok(crate::ThreadLocal::Offset(-16))),
        (
            "library_state",
            Ok(crate::ThreadLocal::Slot(ImageAddress::new(0x5000))),
        ),
        ("lost", Err("no code reads it".into())),
    ]
    .into_iter()
    .map(|(name, place)| (name.into(), place))
    .collect()
}

/// Packages, one named twice, whose last name counts.
pub(super) const SAMPLE_PACKAGES: [(&str, &str); 3] = [
    ("main", "first"),
    ("example.com/m/stack", "stack"),
    ("main", "main"),
];

/// Two of the sample functions' packages and local names, which are the
/// same.
pub(super) fn sample_packaged() -> Vec<Option<(&'static str, String)>> {
    vec![
        None,
        Some(("example.com/m/stack", "Push".to_owned())),
        None,
        Some(("main", "Push".to_owned())),
    ]
}

/// Types of every kind and shape, each at its identifier's index, and
/// their identity classes: the last type is the same as the second.
#[expect(clippy::too_many_lines, reason = "one type of each kind and shape")]
pub(super) fn sample_types() -> (Vec<crate::TypeNode>, Vec<u32>) {
    use crate::{
        Accessibility, ArgumentOrigin, ArrayDimension, ArrayOrdering, BaseClass,
        BaseClassVirtuality, BaseType, BaseTypeEncoding, EnumerationOrigin, Enumerator, GoKind,
        GoTypeAttributes, IntegerValue, ModuleImageId, NamedTypeRelationship, RecordKind,
        RecordMember, RecordMemberLayout, ReferenceKind, TypeArgument, TypeIdentity, TypeInfo,
        TypeKind, TypeModifier, TypeNode, TypeReference, Variant, VariantDiscriminant,
        VariantSelection, VariantSelector, VariantStorageKind,
    };

    let reference = |id| TypeReference {
        image: ModuleImageId::new(0),
        id: TypeId::new(id),
    };

    let identity = |language, path: &[&str], base: &str| TypeIdentity {
        language,
        path: path.iter().map(|segment| (*segment).into()).collect(),
        inline_namespaces: [].into(),
        base: base.into(),
        arguments: [].into(),
        pack: None,
        origin: ArgumentOrigin::None,
        go: None,
    };

    let member = |name: Option<&str>, ty, layout| RecordMember {
        name: name.map(Into::into),
        type_ref: reference(ty),
        layout,
        accessibility: Accessibility::Public,
        artificial: false,
        embedded: false,
        declaration: None,
    };

    let int = BaseType {
        name: "int".into(),
        base_name: "int".into(),
        encoding: BaseTypeEncoding::Signed,
        byte_size: 4,
        bit_size: None,
    };

    let kinds: Vec<(&str, Option<u64>, TypeKind, Option<TypeIdentity>)> = vec![
        (
            "int",
            Some(4),
            TypeKind::Base(int),
            Some(identity(SourceLanguage::C, &[], "int")),
        ),
        (
            "int *",
            Some(8),
            TypeKind::Pointer {
                target: Some(reference(0)),
                address_class: 0,
            },
            None,
        ),
        (
            "geo::Point<int, -3, N>",
            Some(16),
            TypeKind::Record {
                kind: RecordKind::Class,
                members: [
                    RecordMember {
                        declaration: Some(SourceLocation {
                            file: SourceFileId::new(0),
                            line: LineNumber::new(3).unwrap(),
                            column: ColumnNumber::new(5),
                        }),
                        accessibility: Accessibility::Private,
                        ..member(Some("x"), 0, RecordMemberLayout::ByteOffset(0))
                    },
                    RecordMember {
                        artificial: true,
                        ..member(
                            Some("y"),
                            0,
                            RecordMemberLayout::BitRange {
                                bit_offset: 32,
                                bit_size: 16,
                            },
                        )
                    },
                    RecordMember {
                        embedded: true,
                        ..member(None, 1, RecordMemberLayout::Runtime)
                    },
                ]
                .into(),
                bases: [BaseClass {
                    type_ref: reference(3),
                    layout: RecordMemberLayout::ByteOffset(8),
                    accessibility: Accessibility::Protected,
                    virtuality: BaseClassVirtuality::Virtual,
                }]
                .into(),
                incomplete: false,
            },
            Some(TypeIdentity {
                inline_namespaces: ["__1".into()].into(),
                arguments: [
                    TypeArgument::Type(reference(0)),
                    TypeArgument::Value(IntegerValue::Signed(-3)),
                    TypeArgument::Unknown("N".into()),
                ]
                .into(),
                pack: Some(1),
                origin: ArgumentOrigin::Dwarf,
                ..identity(SourceLanguage::Cpp, &["geo", "__1"], "Point")
            }),
        ),
        (
            "shapes::Shape",
            Some(8),
            TypeKind::Variant {
                storage: VariantStorageKind::Struct,
                common_members: [member(Some("tag"), 0, RecordMemberLayout::ByteOffset(0))].into(),
                bases: [].into(),
                discriminant: Box::new(VariantDiscriminant::Stored(member(
                    Some("tag"),
                    0,
                    RecordMemberLayout::ByteOffset(0),
                ))),
                variants: [
                    Variant {
                        name: Some("Circle".into()),
                        selection: VariantSelection::Selectors(
                            [
                                VariantSelector::Value(IntegerValue::Unsigned(u128::MAX)),
                                VariantSelector::Range {
                                    low: IntegerValue::Signed(i128::MIN),
                                    high: IntegerValue::Signed(5),
                                },
                            ]
                            .into(),
                        ),
                        members: [member(Some("r"), 0, RecordMemberLayout::ByteOffset(4))].into(),
                    },
                    Variant {
                        name: None,
                        selection: VariantSelection::Default,
                        members: [].into(),
                    },
                ]
                .into(),
                incomplete: false,
            },
            Some(identity(SourceLanguage::Rust, &["shapes"], "Shape")),
        ),
        (
            "main.Color",
            Some(4),
            TypeKind::Enumeration {
                representation: BaseType {
                    name: "main.Color".into(),
                    base_name: "unsigned int".into(),
                    encoding: BaseTypeEncoding::Unsigned,
                    byte_size: 4,
                    bit_size: Some(3),
                },
                underlying: Some(reference(0)),
                enumerators: [("Red", 0), ("Green", 1), ("Red", 2)]
                    .into_iter()
                    .map(|(name, value)| Enumerator {
                        name: name.into(),
                        value: IntegerValue::Unsigned(value),
                    })
                    .collect(),
                origin: EnumerationOrigin::NamedConstants,
                scoped: true,
            },
            Some(TypeIdentity {
                go: Some(GoTypeAttributes {
                    kind: GoKind::Uint,
                    runtime_type: Some(0x100),
                }),
                ..identity(SourceLanguage::Go, &["main"], "Color")
            }),
        ),
        (
            "[3][4]int",
            Some(48),
            TypeKind::Array {
                element: reference(0),
                dimensions: [
                    ArrayDimension {
                        lower_bound: -1,
                        count: 3,
                    },
                    ArrayDimension {
                        lower_bound: i128::MAX,
                        count: 4,
                    },
                ]
                .into(),
                ordering: ArrayOrdering::ColumnMajor,
            },
            None,
        ),
        (
            "[]int",
            Some(24),
            TypeKind::Slice {
                element: reference(0),
                words: crate::SliceWords::POINTER_LENGTH_CAPACITY,
                text: false,
            },
            None,
        ),
        (
            "union U",
            None,
            TypeKind::Union {
                members: [
                    member(Some("a"), 0, RecordMemberLayout::ByteOffset(0)),
                    member(Some("b"), 1, RecordMemberLayout::ByteOffset(0)),
                ]
                .into(),
                incomplete: true,
            },
            None,
        ),
        (
            "const int",
            Some(4),
            TypeKind::Modified {
                modifier: TypeModifier::Const,
                target: reference(0),
            },
            None,
        ),
        (
            "main.Alias",
            Some(4),
            TypeKind::Named {
                target: Some(reference(4)),
                relationship: NamedTypeRelationship::Distinct,
            },
            Some(TypeIdentity {
                go: Some(GoTypeAttributes {
                    kind: GoKind::Other(99),
                    runtime_type: Some(0x100),
                }),
                ..identity(SourceLanguage::Go, &["main"], "Alias")
            }),
        ),
        ("void", None, TypeKind::Unspecified, None),
        ("func()", Some(8), TypeKind::Function, None),
        (
            "int (int, int *, ...)",
            None,
            TypeKind::Signature {
                returns: Some(reference(0)),
                parameters: [reference(0), reference(1)].into(),
                variadic: true,
                prototyped: true,
            },
            None,
        ),
        (
            "member pointer",
            Some(16),
            TypeKind::Opaque {
                description: "DW_TAG_ptr_to_member_type".into(),
            },
            None,
        ),
        ("", None, TypeKind::Unspecified, None),
        (
            "Point &&",
            Some(8),
            TypeKind::Reference {
                kind: ReferenceKind::Rvalue,
                target: reference(2),
                address_class: 1,
            },
            None,
        ),
        (
            "Tagged",
            Some(4),
            TypeKind::Variant {
                storage: VariantStorageKind::Union,
                common_members: [].into(),
                bases: [].into(),
                discriminant: Box::new(VariantDiscriminant::TagType(reference(0))),
                variants: [].into(),
                incomplete: true,
            },
            None,
        ),
        (
            "Never",
            Some(0),
            TypeKind::Variant {
                storage: VariantStorageKind::Class,
                common_members: [].into(),
                bases: [].into(),
                discriminant: Box::new(VariantDiscriminant::Absent),
                variants: [].into(),
                incomplete: false,
            },
            None,
        ),
        (
            "short int",
            Some(2),
            TypeKind::Base(BaseType {
                name: "short int".into(),
                base_name: "short int".into(),
                encoding: BaseTypeEncoding::Signed,
                byte_size: 2,
                bit_size: None,
            }),
            Some(identity(SourceLanguage::C, &[], "short int")),
        ),
        (
            "integer(kind=4)[:,:]",
            None,
            TypeKind::RuntimeArray {
                element: reference(0),
                dimensions: [
                    crate::RuntimeDimension {
                        lower_bound: crate::ArrayBound::Computed {
                            byte_size: 8,
                            signed: true,
                        },
                        extent: crate::ArrayExtent::Upper(crate::ArrayBound::Stored {
                            byte_size: 4,
                            signed: false,
                        }),
                        byte_stride: Some(crate::ArrayBound::Constant(-8)),
                    },
                    crate::RuntimeDimension {
                        lower_bound: crate::ArrayBound::Constant(i128::MIN),
                        extent: crate::ArrayExtent::Count(crate::ArrayBound::Variable {
                            debug_info_offset: u64::MAX,
                        }),
                        byte_stride: None,
                    },
                    crate::RuntimeDimension {
                        lower_bound: crate::ArrayBound::Constant(0),
                        extent: crate::ArrayExtent::Unknown,
                        byte_stride: None,
                    },
                ]
                .into(),
                ordering: ArrayOrdering::ColumnMajor,
            },
            None,
        ),
        (
            "int *",
            Some(8),
            TypeKind::Pointer {
                target: Some(reference(0)),
                address_class: 0,
            },
            None,
        ),
    ];

    let mut nodes = kinds
        .into_iter()
        .enumerate()
        .map(|(index, (name, byte_size, kind, identity))| {
            TypeNode::Resolved(TypeInfo {
                reference: reference(u32::try_from(index).unwrap()),
                name: name.into(),
                byte_size,
                kind,
                identity: identity.map(std::sync::Arc::new),
            })
        })
        .collect::<Vec<_>>();

    nodes[14] = TypeNode::Malformed {
        reference: reference(14),
        description: "the type's size is negative".into(),
    };

    let mut classes = (0..u32::try_from(nodes.len()).unwrap()).collect::<Vec<_>>();
    *classes.last_mut().unwrap() = 1;

    (nodes, classes)
}

pub(super) const ENCODING: gimli::Encoding = gimli::Encoding {
    format: gimli::Format::Dwarf32,
    version: 5,
    address_size: 8,
};

/// Two units' expressions, one calling procedures with addresses of its
/// own, and lists with ranged and default entries, one of them shared.
pub(super) fn sample_locations() -> LocationsBuilder {
    let mut pool = LocationsBuilder::default();
    let first = pool
        .unit(&EvaluationUnit {
            offset: Some(0x100),
            language: Some(gimli::DW_LANG_C11),
            base_types: vec![(0x30, gimli::ValueType::U32), (0x10, gimli::ValueType::I64)],
        })
        .unwrap();
    let second = pool.unit(&EvaluationUnit::default()).unwrap();
    let register = pool.expression(&[0x50], first, ENCODING, &[], &[]).unwrap();
    let address = pool
        .expression(&[0xa1, 0], second, ENCODING, &[(0, 0x1000)], &[])
        .unwrap();
    let procedure = pool
        .list(&[(None, register), (Some(range(0x1000, 0x1010)), address)])
        .unwrap();
    let caller = pool
        .expression(
            &[0x98, 0x20, 0, 0x9f],
            first,
            gimli::Encoding {
                format: gimli::Format::Dwarf64,
                version: 4,
                address_size: 4,
            },
            &[(3, 0x2000), (1, 0x1800)],
            &[(0x140, Some(procedure)), (0x120, None)],
        )
        .unwrap();

    pool.list(&[(Some(range(0x1000, 0x1004)), caller), (None, register)])
        .unwrap();
    pool.list(&[]).unwrap();
    pool
}

/// Data objects of every kind of value, sharing scopes and code, and
/// functions with nested code, captures, and each return convention.
#[expect(clippy::too_many_lines, reason = "one object or function of each kind")]
pub(super) fn sample_variables() -> Variables {
    let code: std::sync::Arc<[_]> = [range(0x1000, 0x1010)].into();
    let declared = SourceLocation {
        file: SourceFileId::new(0),
        line: LineNumber::new(3).unwrap(),
        column: ColumnNumber::new(7),
    };

    let local = |name: &str, value| DataObject {
        debug_info_offset: Some(0x40),
        kind: crate::VariableKind::Local,
        name: name.into(),
        declaration: Some(declared.clone()),
        ranges: std::sync::Arc::clone(&code),
        go_declaration: Some(Box::new(GoDeclaration {
            location: declared.clone(),
            instance: Some(CodeInstanceId::new(1)),
        })),
        instance: Some(CodeInstanceId::new(1)),
        lexical_depth: 1,
        order: 0,
        type_info: TypeResolution::Resolved(TypeId::new(0)),
        escaped: None,
        hidden: false,
        coroutine: None,
        value,
        frame_base: Metadata::Value(LocationListId(1)),
        malformed: None,
    };

    let objects = vec![
        local(
            "x",
            Metadata::Value(ValueDescription::Location(LocationListId(0))),
        ),
        DataObject {
            debug_info_offset: Some(0x30),
            kind: crate::VariableKind::Parameter,
            declaration: None,
            go_declaration: None,
            instance: None,
            lexical_depth: 0,
            type_info: TypeResolution::Malformed("no type".into()),
            escaped: Some(TypeId::new(1)),
            frame_base: Metadata::Absent(MetadataAbsence::NoFrameBase),
            malformed: Some("bad".into()),
            ..local(
                "p",
                Metadata::Value(ValueDescription::Constant(Box::new(ConstantValue::Signed(
                    -3,
                )))),
            )
        },
        DataObject {
            debug_info_offset: None,
            kind: crate::VariableKind::Global,
            ranges: [].into(),
            go_declaration: None,
            instance: None,
            lexical_depth: 0,
            hidden: true,
            coroutine: Some(TypeId::new(2)),
            frame_base: Metadata::Absent(MetadataAbsence::NotApplicable),
            ..local(
                "g",
                Metadata::Value(ValueDescription::Constant(Box::new(ConstantValue::Bytes(
                    [1, 2, 3].into(),
                )))),
            )
        },
        DataObject {
            debug_info_offset: Some(0x50),
            ..local("y", Metadata::Malformed("unreadable".into()))
        },
        DataObject {
            debug_info_offset: Some(0x60),
            ..local(
                "u",
                Metadata::Value(ValueDescription::Constant(Box::new(
                    ConstantValue::Unsigned(u128::MAX - 1),
                ))),
            )
        },
        DataObject {
            debug_info_offset: Some(0x30),
            go_declaration: None,
            frame_base: Metadata::Malformed("no frame base".into()),
            ..local(
                "f",
                Metadata::Value(ValueDescription::Constant(Box::new(ConstantValue::Fixed(
                    0xff,
                )))),
            )
        },
        DataObject {
            debug_info_offset: None,
            ..local("n", Metadata::Absent(MetadataAbsence::NoLocation))
        },
    ];

    let functions = vec![
        Function {
            ranges: [range(0x1000, 0x1010), range(0x2000, 0x2008)].into(),
            objects: vec![1, 0, 3, 4, 5, 6],
            name: Some("main.f".into()),
            captures: Ok(vec![
                Capture {
                    name: "&n".into(),
                    offset: 8,
                    type_info: TypeResolution::Resolved(TypeId::new(0)),
                },
                Capture {
                    name: "m".into(),
                    offset: 16,
                    type_info: TypeResolution::Malformed("?".into()),
                },
            ]),
            returns: Some(ReturnConvention::GoRegisters),
        },
        Function {
            ranges: [range(0x1004, 0x100c)].into(),
            objects: Vec::new(),
            name: None,
            captures: Err("closure".into()),
            returns: Some(ReturnConvention::SystemV(Box::new(SystemV {
                name: "g".into(),
                ty: TypeResolution::Resolved(TypeId::new(1)),
                language: SourceLanguage::Other(0x8001),
                rewritten: true,
            }))),
        },
        Function {
            ranges: [].into(),
            objects: vec![2],
            name: None,
            captures: Ok(Vec::new()),
            returns: Some(ReturnConvention::SystemV(Box::new(SystemV {
                name: "h".into(),
                ty: TypeResolution::Malformed("unknown".into()),
                language: SourceLanguage::C,
                rewritten: false,
            }))),
        },
        Function {
            ranges: [range(0x3000, 0x3004)].into(),
            objects: Vec::new(),
            name: None,
            captures: Ok(Vec::new()),
            returns: None,
        },
    ];

    Variables {
        objects,
        functions,
        globals: vec![variables::Global {
            object: 2,
            qualified_name: "ns::g".into(),
            linkage_name: Some("_ZN2ns1gE".into()),
            external: true,
        }],
        go_entries: vec![
            (ImageAddress::new(0x1000), 0),
            (ImageAddress::new(0x1004), 1),
            (ImageAddress::new(0x1000), 1),
        ],
        procedures: vec![
            (0x80, Metadata::Value(LocationListId(2))),
            (0x70, Metadata::Malformed("m".into())),
            (0x80, Metadata::Absent(MetadataAbsence::NoLocation)),
        ],
    }
}

/// Functions that describe their tail calls and some that do not, and a
/// call site of every kind of target, two of which return to one place.
pub(super) fn sample_calls() -> Calls {
    let site = |function, return_address: Option<u64>, target| CallSite {
        function,
        return_address: return_address.map(ImageAddress::new),
        target,
        enters: None,
        jump: None,
        parameters: Vec::new(),
        malformed: None,
    };

    let jump = |instruction, lookup| crate::debug_info::TailJump {
        instruction: ImageAddress::new(instruction),
        lookup: ImageAddress::new(lookup),
    };

    Calls {
        functions: vec![
            CallingFunction {
                name: Some("f".into()),
                frame_base: Metadata::Value(LocationListId(1)),
                tail_calls_described: true,
                tail_calls: vec![1],
            },
            CallingFunction {
                name: None,
                frame_base: Metadata::Malformed("no frame base".into()),
                tail_calls_described: false,
                tail_calls: Vec::new(),
            },
            CallingFunction {
                name: Some("g".into()),
                frame_base: Metadata::Absent(MetadataAbsence::NoFrameBase),
                tail_calls_described: true,
                tail_calls: vec![2],
            },
        ],
        sites: vec![
            CallSite {
                parameters: vec![
                    SiteParameter {
                        register: Some(5),
                        parameter: None,
                        value: Some(ExpressionId(0)),
                        data_value: None,
                    },
                    SiteParameter {
                        register: None,
                        parameter: Some(0x44),
                        value: None,
                        data_value: Some(ExpressionId(1)),
                    },
                ],
                ..site(0, Some(0x1008), SiteTarget::Code(ImageAddress::new(0x3000)))
            },
            CallSite {
                enters: Some(2),
                jump: Some(jump(0x100c, 0x100c)),
                ..site(0, None, SiteTarget::Symbol("ext".into()))
            },
            CallSite {
                enters: Some(0),
                jump: Some(jump(0x2010, 0x200f)),
                ..site(2, None, SiteTarget::Computed(Ok(LocationListId(0))))
            },
            CallSite {
                malformed: Some("broken".into()),
                ..site(
                    1,
                    Some(0x1008),
                    SiteTarget::Computed(Err("bad target".into())),
                )
            },
            site(1, Some(0x1004), SiteTarget::Unknown),
        ],
    }
}

/// Facts of several types, given out of order.
pub(super) fn sample_type_facts() -> type_facts::TypeFacts {
    use type_facts::LayoutChild;
    let ty = TypeId::new;
    type_facts::TypeFacts {
        dictionary_indices: vec![(ty(5), 0), (ty(2), 1)],
        passed_by_value: vec![(ty(4), false), (ty(1), true)],
        complex_parts: vec![
            ("float".into(), 8, ty(3)),
            ("double".into(), 8, ty(2)),
            ("float".into(), 4, ty(1)),
        ],
        dynamic_layouts: vec![
            (ty(4), LayoutChild::Member(1), ExpressionId(2)),
            (
                ty(1),
                LayoutChild::VariantMember {
                    variant: 1,
                    member: 0,
                },
                ExpressionId(1),
            ),
            (ty(4), LayoutChild::Discriminant, ExpressionId(0)),
            (
                ty(5),
                LayoutChild::Bound {
                    dimension: 1,
                    part: type_facts::BoundPart::Stride,
                },
                ExpressionId(1),
            ),
            (ty(5), LayoutChild::DataLocation, ExpressionId(0)),
        ],
    }
}

/// Constants and vtables out of order, one of each named twice, and
/// producers, one named twice.
pub(super) fn sample_declarations() -> declarations::Declarations {
    use crate::IntegerValue::{Signed, Unsigned};
    declarations::Declarations {
        constants: vec![
            ("runtime._Grunning".into(), Unsigned(2)),
            ("max".into(), Unsigned(u128::MAX)),
            ("min".into(), Signed(i128::MIN)),
            ("runtime._Grunning".into(), Signed(-1)),
        ],
        vtables: vec![
            (ImageAddress::new(0x6000), TypeId::new(2)),
            (ImageAddress::new(0x5000), TypeId::new(1)),
            (ImageAddress::new(0x6000), TypeId::new(0)),
        ],
        producers: vec!["rustc".into(), "clang".into(), "rustc".into()],
    }
}

/// Resume points of two instances, one undecodable, given out of order,
/// and held ranges, one key twice.
pub(super) fn sample_resumes() -> resumes::Resumes {
    let point = |state, address, resumption: &[AddressRange<ImageAddress>]| crate::ResumePoint {
        state,
        address: ImageAddress::new(address),
        resumption: resumption.into(),
    };

    resumes::Resumes {
        points: vec![
            (CodeInstanceId::new(3), Err("an indirect dispatch".into())),
            (
                CodeInstanceId::new(0),
                Ok(crate::ResumePoints {
                    dispatch: [range(0x1000, 0x1002), range(0x2000, 0x2000)].into(),
                    points: [
                        point(0, 0x1002, &[range(0x1002, 0x1004)]),
                        point(3, 0x1008, &[range(0x1008, 0x100c), range(0x2000, 0x2004)]),
                        point(4, 0x100c, &[]),
                    ]
                    .into(),
                }),
            ),
        ],
        held: vec![
            ((0x40, 3), vec![range(0x1008, 0x1010)]),
            ((0x20, 3), vec![]),
            ((0x40, 3), vec![range(0x100c, 0x1010)]),
            (
                (0x40, 4),
                vec![range(0x100c, 0x1010), range(0x2000, 0x2008)],
            ),
        ],
    }
}

pub(super) fn seal(tables: &LineTables, files: &lines::Files) -> Result<Image, ImageError> {
    // The builder borrows these until it seals.
    let unwind = sample_unwind();
    let locations = sample_locations();
    let mut builder = Builder::new(TARGET);
    tables.add_to(&mut builder);
    let mut paths = PathsBuilder::default();
    files.add_to(&mut builder, &mut paths).unwrap();
    let mut strings = StringsBuilder::default();
    symbols::add_to(
        &mut builder,
        &mut strings,
        &sample_symbols(),
        &sample_sections(),
        &sample_got(),
    )
    .unwrap();
    let instances = sample_instances();
    functions::add_functions_to(&mut builder, &mut strings, &sample_functions(), &instances)
        .unwrap();
    functions::add_code_to(
        &mut builder,
        &functions::Code {
            instances: &instances,
            prologue_ends: &tables.prologue_ends(),
            instruction_starts: &sample_starts(),
        },
    )
    .unwrap();
    unwind::add_to(&mut builder, &mut strings, &unwind).unwrap();
    let (nodes, classes) = sample_types();
    types::add_to(
        &mut builder,
        &mut strings,
        &types::Types {
            nodes: &nodes,
            classes: &classes,
        },
    )
    .unwrap();
    packages::add_to(
        &mut builder,
        &mut strings,
        SAMPLE_PACKAGES,
        &sample_packaged(),
    )
    .unwrap();
    facts::add_to(
        &mut builder,
        &mut strings,
        &facts::Facts {
            address_range: sample_address_range(),
            symbol_sources: &sample_sources(),
            thread_local_storage: true,
            thread_locals: &sample_thread_locals(),
            debug_file: Some(&crate::DebugFile::Unusable {
                path: std::sync::Arc::new("/usr/lib/debug/.build-id/ab/cdef.debug".into()),
                reason: "its build id differs".into(),
            }),
            debug_information: &crate::DebugInformation::Incomplete {
                reason: "a unit is in a split DWARF file".into(),
            },
            locals_left_out: Some("they exceed the budget"),
        },
    )
    .unwrap();
    locations.add_to(&mut builder);
    variables::add_to(&mut builder, &mut strings, &sample_variables()).unwrap();
    calls::add_to(&mut builder, &mut strings, &sample_calls()).unwrap();
    type_facts::add_to(&mut builder, &mut strings, &sample_type_facts()).unwrap();
    resumes::add_to(&mut builder, &mut strings, &sample_resumes()).unwrap();
    declarations::add_to(&mut builder, &mut strings, &sample_declarations()).unwrap();

    builder
        .bytes(TableKind::EmbeddedViews, b"views".to_vec())
        .bytes(TableKind::Paths, paths.into_bytes())
        .bytes(TableKind::Strings, strings.into_bytes());
    builder.seal(Limits::default())
}

/// Reads everything a valid image's views can read, so that a test can
/// show none of it panics.
pub(super) fn read_everything(image: &Image) -> u64 {
    let addresses = image.table::<RowAddress>();
    let rows = image.table::<LineRow>();
    let extras = image.table::<LineExtra>();
    let mut read = 0_u64;
    for index in 0..rows.len() {
        read = read.wrapping_add(lines::decode_row(addresses, rows, extras, index).line);
    }

    read += lines::statement_rows(addresses, rows, extras, image.table::<LineSequence>()).count()
        as u64;
    for range in image.table::<LineRange>() {
        read = read.wrapping_add(
            lines::line_entry(addresses, rows, extras, range)
                .expect("every range has a location")
                .location
                .line
                .get(),
        );
    }

    let paths = Paths(image.bytes(TableKind::Paths));
    for file in image.table::<FileRecord>() {
        read += paths.get(PathId(file.path.get())).as_os_str().len() as u64;
    }

    let view = SymbolView::new(image);
    for symbol in view.all() {
        let info = symbol.info();
        read += view.named(&info.name).count() as u64;
        for address in [info.address.get(), symbol.range_end().get()] {
            let address = ImageAddress::new(address);
            read += u64::from(view.code_at(address).is_some());
            read += u64::from(view.data_at(address).is_some());
        }

        read += u64::from(symbol.answers_to("main"));
    }

    read += symbols::sections(image).len() as u64;
    read += symbols::got_slots(image).len() as u64;
    let view = FunctionView::new(image);
    for function in view.functions() {
        let info = function.info();
        read += view.named(&info.name).count() as u64;
        read += function
            .instances()
            .map(|instance| instance.info().ranges.len() as u64)
            .sum::<u64>();
    }

    for instance in view.instances() {
        let info = instance.info();
        read += instance.recommended_entries().count() as u64;
        for range in info.ranges.iter() {
            read += view.instances_containing(range.start).count() as u64;
            read += view.instruction_starts(*range).count() as u64;
        }
    }

    let view = UnwindView::new(image);
    for lookup in [view.eh_frame_index(), view.debug_frame_index()] {
        for interval in lookup.entries {
            for address in [interval.start.get(), interval.end.get() - 1] {
                read += u64::from(lookup.lookup(address).is_ok());
            }
        }
    }

    read += (view.eh_frame().len() + view.debug_frame().len()) as u64;
    read += u64::from(view.is_go_code(ImageAddress::new(0x3000)));
    read += u64::from(view.stub_pushes(ImageAddress::new(0x340b)).unwrap_or(0));
    read += view.frame_saves().flatten().count() as u64;
    if let Some((bytes, _)) = view.go() {
        read += bytes.len() as u64;
    }

    let view = TypeView::new(image);
    for index in 0..view.len() {
        let id = TypeId::new(u32::try_from(index).unwrap());
        if let Some(crate::TypeNode::Resolved(info)) = view.node(crate::ModuleImageId::new(0), id) {
            read += view.named(&info.name).count() as u64;
            read += u64::from(view.class(id).is_some());
            if let Some(identity) = &info.identity {
                read += view.with_base(&identity.base).count() as u64;
            }

            if let crate::TypeKind::Enumeration { enumerators, .. } = &info.kind {
                for enumerator in enumerators.iter() {
                    read += view.with_enumerator(&enumerator.name).count() as u64;
                }
            }
        }
    }

    read += u64::from(view.go_runtime_type(0x100).is_some());

    let view = PackageView::new(image);
    for index in 0..image.table::<packages::PackagedRecord>().len() {
        let function = FunctionId::new(u32::try_from(index).unwrap());
        if let Some((package, local)) = view.packaged_name(function) {
            read += view.with_local_name(local).count() as u64;
            read += u64::from(view.package_name(package).is_some());
        }
    }

    read + read_families(image)
}

/// Reads what the families after lines, functions, and types hold.
fn read_families(image: &Image) -> u64 {
    read_locations(
        LocationTables::new(image),
        image.table::<locations::ExpressionRecord>().len(),
        image.table::<locations::LocationListRecord>().len(),
    ) + read_variables(image)
        + read_calls(image)
        + read_type_facts(image)
        + read_resumes(image)
        + read_declarations(image)
}

pub(super) fn reseal(bytes: &mut [u8]) {
    let end = bytes.len() - size_of::<Trailer>();
    let checksum = twox_hash::XxHash3_64::oneshot(&bytes[..end]);
    bytes[end..].copy_from_slice(
        Trailer {
            checksum: checksum.into(),
        }
        .as_bytes(),
    );
}

/// Validates `data` as an image and reads everything a valid one holds.
/// Odd-first-byte inputs are changes to the sample: (offset, byte) pairs
/// `XORed` into it. Every input is resealed with a correct checksum, so that
/// the fuzzer reaches the checks past it.
#[cfg(feature = "fuzzing")]
pub fn fuzz(data: &[u8]) {
    let Some((&mode, data)) = data.split_first() else {
        return;
    };

    let mut bytes = if mode & 1 == 1 {
        let (tables, files) = sample();
        let mut bytes = seal(&tables, &files)
            .expect("the sample is valid")
            .as_bytes()
            .to_vec();
        for [low, high, flip] in data.as_chunks::<3>().0 {
            let at = usize::from(u16::from_le_bytes([*low, *high])) % bytes.len();
            bytes[at] ^= flip;
        }

        bytes
    } else {
        data.to_vec()
    };

    if bytes.len() >= size_of::<Trailer>() {
        reseal(&mut bytes);
    }

    let Some(mut aligned) = super::AlignedBytes::zeroed(bytes.len()) else {
        return;
    };

    aligned.as_mut_bytes().copy_from_slice(&bytes);
    if let Ok(image) = Image::from_bytes(aligned, Limits::default()) {
        read_everything(&image);
        let view = lines::LineView::new(&image);
        for row in view.statement_rows() {
            let _ = view.control_boundaries_at(row.address).count();
            let _ = view.line_entry_containing(row.address);
            if let Some(location) = row.location {
                let _ = view.statements(location.file, 0..=location.line.get());
            }
        }
    }
}

/// Reads the first `expressions` expressions and `lists` lists of
/// `tables`, so that a test can show none of it panics and compare two
/// copies of it.
pub(super) fn read_locations(tables: LocationTables<'_>, expressions: usize, lists: usize) -> u64 {
    let mut read = 0_u64;
    let summarize = |expression: locations::Expression<'_>| {
        let mut read = expression.bytes().len() as u64;
        read += u64::from(expression.unit());
        read += expression.unit_offset().unwrap_or(7);
        read += u64::from(expression.encoding().version);
        for (index, address) in expression.addresses() {
            read = read.wrapping_add(index ^ address);
            read += u64::from(expression.indexed_address(index) == Some(address));
        }

        for (offset, list) in expression.procedures() {
            read = read.wrapping_add(offset + list.map_or(9, |list| u64::from(list.0)));
            read += expression
                .procedure(offset)
                .and_then(locations::Procedure::locations)
                .map_or(0, |list| list.entries().count() as u64);
        }

        read + u64::from(expression.base_type(0x30).is_some())
    };

    for id in (0..).map(ExpressionId).take(expressions) {
        read = read.wrapping_add(summarize(tables.expression(id)));
    }

    for id in (0..).map(LocationListId).take(lists) {
        for (range, expression) in tables.list(id).entries() {
            read = read.wrapping_add(range.map_or(5, |range| range.start.get() ^ range.end.get()));
            read = read.wrapping_add(u64::from(expression.id().0));
        }
    }

    read
}

/// Reads every data object and function an image holds through its views.
pub(super) fn read_variables(image: &Image) -> u64 {
    let view = VariableView::new(image);
    let mut read = 0_u64;
    let objects = image.table::<variables::ObjectRecord>().len();
    for index in 0..objects {
        let object = view.object(variables::ObjectId(u32::try_from(index).unwrap()));
        read += object.name().len() as u64;
        read += object.ranges().count() as u64;
        read += u64::from(object.in_scope(ImageAddress::new(0x1008)));
        read += u64::from(object.go_declaration().is_some());
        read += u64::from(object.instance().is_some()) + u64::from(object.lexical_depth());
        read += u64::from(matches!(object.type_info(), TypeResolution::Resolved(_)));
        read += u64::from(object.escaped().is_some() || object.coroutine().is_some());
        read += u64::from(object.hidden()) + u64::from(object.malformed().is_some());
        read += u64::from(matches!(object.value(), Metadata::Value(_)));
        read += u64::from(matches!(object.frame_base(), Metadata::Value(_)));
        read += object.debug_info_offset().unwrap_or(1);
        read += u64::from(object.declaration().is_some()) + object.kind() as u64;
    }

    let functions = image.table::<variables::VariableFunctionRecord>().len();
    for index in 0..functions {
        let function = view.function(VariableFunctionId(u32::try_from(index).unwrap()));
        read += function.objects().count() as u64;
        read += function.name().map_or(0, |name| name.len() as u64);
        read += function
            .captures()
            .map_or(1, |captures| captures.len() as u64);
        read += u64::from(function.returns().is_some());
    }

    for start in image.table::<variables::FunctionStartRecord>() {
        for address in [start.start.get(), start.end.get() - 1, start.end.get()] {
            let address = ImageAddress::new(address);
            read += view
                .function_at(address)
                .map_or(0, |found| u64::from(found.id().0));
            read += u64::from(view.go_function(address).is_some());
        }
    }

    read += view.globals().count() as u64;
    for offset in [0x30, 0x40, 0x70, 0x80] {
        read += u64::from(view.object_at_offset(offset).is_some());
        read += u64::from(view.procedure(offset).is_some());
    }

    read
}

/// Reads every call site and calling function an image holds.
pub(super) fn read_calls(image: &Image) -> u64 {
    let view = CallView::new(image);
    let mut read = 0_u64;
    for index in 0..image.table::<calls::CallingFunctionRecord>().len() {
        let function = view.function(u32::try_from(index).unwrap());
        read += function.name().map_or(0, |name| name.len() as u64);
        read += u64::from(matches!(function.frame_base(), Metadata::Value(_)));
        read += u64::from(function.tail_calls_described());
        read += function
            .tail_calls()
            .map(|site| u64::from(site.0))
            .sum::<u64>();
    }

    for index in 0..image.table::<calls::CallSiteRecord>().len() {
        let site = view
            .site(calls::SiteId(u32::try_from(index).unwrap()))
            .unwrap();
        read += u64::from(site.function());
        if let Some(address) = site.return_address() {
            read += view.returning_to(address).count() as u64;
        }

        read += u64::from(matches!(site.target(), SiteTarget::Computed(_)));
        read += u64::from(site.enters().is_some()) + u64::from(site.jump().is_some());
        read += site.parameters().count() as u64;
        read += u64::from(site.malformed().is_some());
    }

    read
}

/// Asks an image's declarations of every name and address they hold, and
/// of some they do not.
pub(super) fn read_declarations(image: &Image) -> u64 {
    let view = DeclarationView::new(image);
    let mut read = view.producers().map(|producer| producer.len() as u64).sum();
    for (name, _) in view.constants() {
        read += u64::from(view.constant(name).is_some());
    }

    read += u64::from(view.constant("").is_some());
    for (address, _) in view.vtables() {
        read += u64::from(view.vtable(address).is_some());
        read += u64::from(view.vtable(ImageAddress::new(address.get() + 1)).is_some());
    }

    read
}

/// Asks an image's resume points and held ranges of the first instances,
/// entries, and states.
pub(super) fn read_resumes(image: &Image) -> u64 {
    let view = ResumeView::new(image);
    let mut read = view.resume_code().count() as u64;
    for index in 0..8 {
        read += match view.resume_points(CodeInstanceId::new(index)) {
            Some(Ok(points)) => points
                .points
                .iter()
                .map(|point| point.resumption.len() as u64)
                .sum::<u64>(),
            Some(Err(why)) => why.len() as u64,
            None => 0,
        };

        for state in 0..8 {
            read += view
                .held(u64::from(index) * 0x20, state)
                .map_or(0, |held| held.count() as u64);
        }
    }

    read
}

/// Asks an image's facts and its type facts of the first types.
pub(super) fn read_type_facts(image: &Image) -> u64 {
    let mut read = 0_u64;
    let view = FactsView::new(image);
    read += u64::from(view.thread_local_storage());
    read += u64::from(view.symbol_sources().static_table);
    for (name, _) in view.thread_locals() {
        read += u64::from(view.thread_local(name).is_some());
    }

    let view = TypeFactsView::new(image);
    for index in 0..8 {
        let ty = TypeId::new(index);
        read += view.dictionary_index(ty).unwrap_or(1);
        read += u64::from(view.passed_by_value(ty).is_some());
        read += u64::from(view.complex_part("float", u64::from(index)).is_some());
        read += u64::from(
            view.dynamic_layout(ty, type_facts::LayoutChild::Discriminant)
                .is_some(),
        );
    }

    read
}
