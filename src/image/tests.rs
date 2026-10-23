use super::calls::{CallSiteRecord, CallingFunctionRecord, SiteParameterRecord};
use super::declarations::{NamedConstantRecord, VtableRecord};
use super::facts::{FactsRecord, FactsView, ThreadLocalRecord};
use super::format::Header;
use super::functions::{
    EntryRecord, FunctionRecord, FunctionView, GenericRecord, InstanceRecord, Member, RangeRecord,
    StartRecord,
};
use super::python::PythonRuntimeRecord;
use super::v8::V8RuntimeRecord;

use super::index::{Interval, NameEntry};
use super::lines::{
    self, ControlBoundary, FileRecord, LineExtra, LineRange, LineRow, LineSequence, LineTables,
    RowAddress, StatementKey, row_flags,
};

use super::locations::{
    BaseTypeRecord, EvaluationUnitRecord, ExpressionRecord, IndexedAddressRecord,
    LocationEntryRecord, LocationListRecord, ProcedureRecord,
};

use super::packages::{PackageRecord, PackageView, PackagedRecord};
use super::resumes::{HeldRecord, ResumePointRecord, ResumeRecord};
use super::sample::{
    ENCODING, SAMPLE_PACKAGES, TARGET, read_everything, read_locations, reseal, sample,
    sample_address_range, sample_functions, sample_got, sample_instances, sample_locations,
    sample_packaged, sample_rows, sample_sections, sample_sources, sample_symbols,
    sample_thread_locals, sample_types, sample_unwind, seal,
};

use super::symbols::{GotRecord, SectionRecord, SymbolRecord, SymbolView};
use super::type_facts::{ComplexPartRecord, DynamicLayoutRecord, TypeFactRecord};
use super::types::{
    ArgumentRecord, BaseRecord, DimensionRecord, EnumeratorRecord, IdentityRecord, Item,
    MemberRecord, RuntimeDimensionRecord, RuntimeTypeRecord, SelectorRecord, TypeRecord, TypeTable,
    TypeView, VariantRecord,
};

use super::unwind::{FdeMiss, FrameSaveRecord, UnwindRecord, UnwindView, unwind_flags};
use super::variables::{
    CaptureRecord, CodeRange, ConstantRecord, DwarfProcedureRecord, FunctionStartRecord,
    GlobalRecord, Keyed, ObjectRecord, ScopeRecord, VariableFunctionRecord,
};

use super::*;
use crate::{
    AddressRange, BreakpointEntry, CodeInstanceId, EntryProvenance, FunctionId, ImageAddress,
    SymbolId,
};

fn reopen(bytes: &[u8]) -> Result<Image, ImageError> {
    Image::from_bytes(AlignedBytes::copy_of(bytes).unwrap(), Limits::default())
}

#[test]
#[expect(clippy::too_many_lines, reason = "one check for each table")]
fn the_schema_is_the_records_layout() {
    macro_rules! check {
        ($kind:expr, $record:ty, [$($($field:ident).+),*]) => {{
            let table = schema::SCHEMA.iter().find(|table| table.kind == $kind).unwrap();
            assert_eq!(table.record, stringify!($record));
            assert_eq!(table.size, size_of::<$record>(), "{}", table.record);
            let actual: &[(&str, usize)] = &[$((
                concat!($(stringify!($field), "."),+).trim_end_matches('.'),
                std::mem::offset_of!($record, $($field).+),
            )),*];
            assert_eq!(table.fields.len(), actual.len(), "{}", table.record);
            for ((name, offset, size), (field, at)) in table.fields.iter().zip(actual) {
                assert_eq!((name, offset), (field, at), "{}", table.record);
                let next = table.fields.iter().map(|field| field.1).filter(|next| next > offset).min();
                assert_eq!(offset + size, next.unwrap_or(table.size), "{}.{name}", table.record);
            }
        }};
    }

    check!(TableKind::Files, FileRecord, [path]);
    check!(TableKind::LineAddresses, RowAddress, [address]);
    check!(
        TableKind::LineRows,
        LineRow,
        [file, line, column, flags, reserved]
    );
    check!(
        TableKind::LineExtras,
        LineExtra,
        [row, operation_index, discriminator, isa, line, column]
    );
    check!(TableKind::LineSequences, LineSequence, [first, rows]);
    check!(
        TableKind::LineRanges,
        LineRange,
        [start, end, prefix_max_end, row, statement]
    );
    check!(TableKind::StatementIndex, StatementKey, [file, line, row]);
    check!(TableKind::ControlBoundaries, ControlBoundary, [row]);
    check!(
        TableKind::Symbols,
        SymbolRecord,
        [
            name,
            address,
            end,
            extent_rank,
            storage_rank,
            kind,
            binding,
            role,
            flags
        ]
    );
    check!(TableKind::SymbolNames, NameEntry, [hash, name, value]);
    check!(
        TableKind::SymbolExtents,
        Interval,
        [start, end, prefix_max_end, value]
    );
    check!(
        TableKind::Sections,
        SectionRecord,
        [name, start, end, flags]
    );
    check!(
        TableKind::GotSlots,
        GotRecord,
        [address, target, name, kind]
    );
    check!(
        TableKind::Functions,
        FunctionRecord,
        [
            name,
            linkage_name,
            declaration.file,
            declaration.line,
            declaration.column,
            enclosing,
            coroutine,
            generics.first,
            generics.count,
            instances.first,
            instances.count,
            other_language,
            language,
            role,
            main_subprogram
        ]
    );
    check!(TableKind::Generics, GenericRecord, [name, argument]);
    check!(TableKind::FunctionInstances, Member, [value]);
    check!(
        TableKind::CodeInstances,
        InstanceRecord,
        [
            function,
            parent,
            call_site.file,
            call_site.line,
            call_site.column,
            ranges.first,
            ranges.count,
            entries.first,
            entries.count,
            entry,
            provenance,
            inline
        ]
    );
    check!(TableKind::InstanceRanges, RangeRecord, [start, end]);
    check!(
        TableKind::RecommendedEntries,
        EntryRecord,
        [address, provenance]
    );
    check!(
        TableKind::InstructionStarts,
        StartRecord,
        [address, evidence]
    );
    check!(
        TableKind::Unwind,
        UnwindRecord,
        [
            eh_frame_base,
            text_base,
            got_base,
            eh_frame_error_offset,
            debug_frame_error_offset,
            eh_frame_error,
            debug_frame_error,
            go_table,
            go_text,
            go_func,
            go_major,
            go_minor,
            flags,
            address_size
        ]
    );
    check!(
        TableKind::GoFrameSaves,
        FrameSaveRecord,
        [start, end, saved]
    );
    check!(
        TableKind::Facts,
        FactsRecord,
        [
            embedded_reason,
            runtime_reason,
            embedded_table,
            runtime_table,
            flags,
            debug_reason,
            debug_file,
            address_start,
            address_end,
            dwarf_reason,
            dwarf,
            locals_reason
        ]
    );
    check!(
        TableKind::ThreadLocals,
        ThreadLocalRecord,
        [name, value, reason, place]
    );
    check!(TableKind::Packages, PackageRecord, [path, name]);
    check!(
        TableKind::PythonRuntime,
        PythonRuntimeRecord,
        [
            version,
            flags,
            reserved,
            runtime_start,
            runtime_end,
            problem,
            offsets,
            constants
        ]
    );
    check!(
        TableKind::V8Runtime,
        V8RuntimeRecord,
        [
            major,
            minor,
            build,
            patch,
            flags,
            reserved,
            problem,
            dispatch_kind,
            dispatch_reserved,
            dispatch_problem,
            dispatch_address,
            dispatch_offset,
            blob_problem,
            blob,
            node_problem,
            node,
            offsets,
            constants
        ]
    );
    check!(
        TableKind::Types,
        TypeRecord,
        [
            name,
            text,
            base_name,
            identity,
            target,
            parts.first,
            parts.count,
            bases.first,
            bases.count,
            variants.first,
            variants.count,
            discriminant,
            byte_size,
            value,
            bit_size,
            flags,
            kind,
            detail
        ]
    );
    check!(
        TableKind::TypeMembers,
        MemberRecord,
        [
            name,
            ty,
            offset,
            bit_size,
            declaration.file,
            declaration.line,
            declaration.column,
            layout,
            accessibility,
            flags
        ]
    );
    check!(
        TableKind::TypeBases,
        BaseRecord,
        [ty, offset, bit_size, layout, accessibility, virtual_base]
    );
    check!(
        TableKind::TypeVariants,
        VariantRecord,
        [
            name,
            selectors.first,
            selectors.count,
            members.first,
            members.count,
            default
        ]
    );
    check!(
        TableKind::TypeSelectors,
        SelectorRecord,
        [low.bits, low.signed, high.bits, high.signed, range]
    );
    check!(
        TableKind::TypeEnumerators,
        EnumeratorRecord,
        [name, value.bits, value.signed]
    );
    check!(
        TableKind::TypeDimensions,
        DimensionRecord,
        [lower_bound, count]
    );
    check!(
        TableKind::RuntimeDimensions,
        RuntimeDimensionRecord,
        [
            lower.value,
            lower.kind,
            lower.byte_size,
            lower.signed,
            extent.value,
            extent.kind,
            extent.byte_size,
            extent.signed,
            stride.value,
            stride.kind,
            stride.byte_size,
            stride.signed,
            ends
        ]
    );
    check!(
        TableKind::TypeIdentities,
        IdentityRecord,
        [
            path.first,
            path.count,
            inline.first,
            inline.count,
            base,
            arguments.first,
            arguments.count,
            pack,
            runtime_type,
            other_language,
            language,
            origin,
            go_kind,
            flags
        ]
    );
    check!(
        TableKind::TypeArguments,
        ArgumentRecord,
        [value.bits, value.signed, reference, kind]
    );
    check!(TableKind::GoRuntimeTypes, RuntimeTypeRecord, [offset, ty]);
    check!(
        TableKind::Expressions,
        ExpressionRecord,
        [
            bytes,
            length,
            unit,
            addresses.first,
            addresses.count,
            procedures.first,
            procedures.count,
            version,
            address_size,
            dwarf64
        ]
    );
    check!(
        TableKind::IndexedAddresses,
        IndexedAddressRecord,
        [index, address]
    );
    check!(TableKind::Procedures, ProcedureRecord, [offset, list]);
    check!(
        TableKind::LocationLists,
        LocationListRecord,
        [entries.first, entries.count]
    );
    check!(
        TableKind::LocationEntries,
        LocationEntryRecord,
        [start, end, expression, flags]
    );
    check!(
        TableKind::EvaluationUnits,
        EvaluationUnitRecord,
        [offset, base_types.first, base_types.count, language, flags]
    );
    check!(TableKind::BaseTypes, BaseTypeRecord, [offset, value_type]);
    check!(TableKind::ScopeRanges, CodeRange, [start, end]);
    check!(
        TableKind::Scopes,
        ScopeRecord,
        [
            ranges.first,
            ranges.count,
            instance,
            go_instance,
            lexical_depth,
            frame_base,
            frame_base_kind
        ]
    );
    check!(
        TableKind::DataObjects,
        ObjectRecord,
        [
            name,
            declaration.file,
            declaration.line,
            declaration.column,
            scope,
            ty,
            escaped,
            coroutine,
            value,
            malformed,
            debug_info_offset,
            kind,
            value_kind,
            flags
        ]
    );
    check!(TableKind::Constants, ConstantRecord, [low, high, kind]);
    check!(
        TableKind::VariableFunctions,
        VariableFunctionRecord,
        [
            ranges.first,
            ranges.count,
            objects.first,
            objects.count,
            name,
            captures.first,
            captures.count,
            returned_name,
            returned_type,
            other_language,
            language,
            returns,
            flags
        ]
    );
    check!(
        TableKind::Captures,
        CaptureRecord,
        [name, offset, ty, malformed]
    );
    check!(
        TableKind::FunctionStarts,
        FunctionStartRecord,
        [start, end, prefix_max_end, function]
    );
    check!(
        TableKind::DwarfProcedures,
        DwarfProcedureRecord,
        [offset, location, location_kind]
    );
    check!(
        TableKind::CallingFunctions,
        CallingFunctionRecord,
        [
            name,
            frame_base,
            tail_calls.first,
            tail_calls.count,
            frame_base_kind,
            flags
        ]
    );
    check!(
        TableKind::CallSites,
        CallSiteRecord,
        [
            function,
            return_address,
            target,
            enters,
            jump_instruction,
            jump_lookup,
            parameters.first,
            parameters.count,
            malformed,
            target_kind,
            flags
        ]
    );
    check!(
        TableKind::SiteParameters,
        SiteParameterRecord,
        [register, parameter, value, data_value, flags]
    );
    for kind in [TableKind::DictionaryIndices, TableKind::PassedByValue] {
        assert_eq!(schema::record(kind), "TypeFactRecord");
        check!(kind, TypeFactRecord, [ty, value]);
    }

    check!(TableKind::ComplexParts, ComplexPartRecord, [name, size, ty]);
    check!(
        TableKind::DynamicLayouts,
        DynamicLayoutRecord,
        [aggregate, first, second, expression, kind]
    );
    check!(TableKind::ResumeRanges, CodeRange, [start, end]);
    check!(
        TableKind::NamedConstants,
        NamedConstantRecord,
        [name, value, signed]
    );
    check!(TableKind::Vtables, VtableRecord, [address, ty]);
    check!(
        TableKind::Globals,
        GlobalRecord,
        [object, qualified_name, linkage_name, external]
    );
    check!(
        TableKind::Resumes,
        ResumeRecord,
        [
            instance,
            dispatch.first,
            dispatch.count,
            points.first,
            points.count,
            malformed
        ]
    );
    check!(
        TableKind::ResumePoints,
        ResumePointRecord,
        [state, address, resumption.first, resumption.count]
    );
    check!(
        TableKind::Held,
        HeldRecord,
        [offset, state, ranges.first, ranges.count]
    );
    for kind in [
        TableKind::GoEntries,
        TableKind::ObjectOffsets,
        TableKind::CallReturns,
    ] {
        assert_eq!(schema::record(kind), "Keyed");
        check!(kind, Keyed, [key, value]);
    }

    for kind in [
        TableKind::TypeParameters,
        TableKind::IdentityStrings,
        TableKind::TypeClasses,
        TableKind::FunctionObjects,
        TableKind::TailCalls,
        TableKind::Producers,
    ] {
        assert_eq!(schema::record(kind), "Item");
        check!(kind, Item, [value]);
    }

    for kind in [
        TableKind::TypeNames,
        TableKind::TypeBaseNames,
        TableKind::EnumeratorNames,
    ] {
        assert_eq!(schema::record(kind), "NameEntry");
    }

    check!(TableKind::PackagedNames, PackagedRecord, [package, local]);
    assert_eq!(schema::record(TableKind::FunctionNames), "NameEntry");
    assert_eq!(schema::record(TableKind::LocalNames), "NameEntry");
    assert_eq!(schema::record(TableKind::CodeRanges), "Interval");
    for kind in [
        TableKind::SymbolStorage,
        TableKind::UnsizedData,
        TableKind::SectionRanges,
        TableKind::EhFrameIndex,
        TableKind::DebugFrameIndex,
        TableKind::GoCode,
        TableKind::StubFrames,
        TableKind::RestoredFrames,
        TableKind::ProvedFrames,
    ] {
        assert_eq!(schema::record(kind), "Interval");
    }

    let kinds = schema::SCHEMA
        .iter()
        .map(|table| table.kind)
        .collect::<Vec<_>>();
    assert_eq!(kinds, TableKind::ALL);

    // A changed record has a new fingerprint. The header rejects older
    // layouts before reading tables; the container format is independent.
    assert_eq!(
        schema::layout_fingerprint(),
        0x57f8_a551_123e_04dc,
        "the layout changed:\n{}",
        schema::schema_text()
    );
}

#[test]
fn an_image_reads_back_what_was_built() {
    let (tables, files) = sample();
    let image = seal(&tables, &files).unwrap();
    let again = reopen(image.as_bytes()).unwrap();
    assert_eq!(again.as_bytes(), image.as_bytes());

    let rows = sample_rows().into_iter().flatten().collect::<Vec<_>>();
    let decoded = (0..rows.len())
        .map(|index| {
            lines::decode_row(
                again.table::<RowAddress>(),
                again.table::<LineRow>(),
                again.table::<LineExtra>(),
                index,
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(decoded, rows);
    assert_eq!(
        lines::statement_rows(
            again.table(),
            again.table(),
            again.table(),
            again.table::<LineSequence>()
        )
        .collect::<Vec<_>>(),
        tables.statement_rows()
    );
    let paths = Paths(again.bytes(TableKind::Paths));
    let read = again
        .table::<FileRecord>()
        .iter()
        .map(|file| paths.get(PathId(file.path.get())).to_path_buf())
        .collect::<Vec<_>>();
    assert_eq!(read, files.paths());
    assert_eq!(again.table::<StatementKey>().len(), 5);
    assert_eq!(again.table::<ControlBoundary>().len(), 2);
}

#[test]
fn symbols_read_back_with_their_preferences() {
    let (tables, files) = sample();
    let image = reopen(seal(&tables, &files).unwrap().as_bytes()).unwrap();
    let view = SymbolView::new(&image);
    let infos = view.all().map(symbols::Symbol::info).collect::<Vec<_>>();
    assert_eq!(infos, sample_symbols());
    assert_eq!(symbols::sections(&image), sample_sections());
    assert_eq!(symbols::got_slots(&image), sample_got());

    let id = |symbol: Option<symbols::Symbol<'_>>| symbol.map(|symbol| symbol.id().get());
    let at = ImageAddress::new;

    // A declared extent outranks an inferred one where they overlap.
    assert_eq!(id(view.code_at(at(0x1008))), Some(0));
    assert_eq!(id(view.code_at(at(0x1010))), Some(1));
    assert_eq!(id(view.code_at(at(0x1020))), None);
    assert_eq!(id(view.data_at(at(0x3007))), Some(2));
    assert_eq!(id(view.data_at(at(0x3008))), None);

    // An unsized symbol names only its own address.
    assert_eq!(id(view.data_at(at(0x3010))), Some(3));
    assert_eq!(id(view.data_at(at(0x3011))), None);
    let named = |name| {
        view.named(name)
            .map(|symbol| symbol.id().get())
            .collect::<Vec<_>>()
    };

    assert_eq!(named("counter"), [2, 3]);
    assert_eq!(named("memcpy"), [] as [u32; 0]);
    let unversioned = view.get(SymbolId::new(4)).unwrap();
    assert!(unversioned.answers_to("memcpy"));
    assert_eq!(unversioned.unversioned_name(), "memcpy");
}

#[test]
fn functions_read_back_with_their_indexes() {
    let (tables, files) = sample();
    let image = reopen(seal(&tables, &files).unwrap().as_bytes()).unwrap();
    let view = FunctionView::new(&image);
    let functions = view
        .functions()
        .map(super::functions::Function::info)
        .collect::<Vec<_>>();
    assert_eq!(functions, sample_functions());
    let instances = view
        .instances()
        .map(super::functions::CodeInstance::info)
        .collect::<Vec<_>>();
    assert_eq!(instances, sample_instances());

    let ids = |instances: &mut dyn Iterator<Item = super::functions::CodeInstance<'_>>| {
        instances
            .map(|instance| instance.id().get())
            .collect::<Vec<_>>()
    };

    let function = |id| view.function(FunctionId::new(id)).unwrap();
    assert_eq!(ids(&mut function(0).instances()), [0, 3]);
    assert_eq!(ids(&mut function(2).instances()), [] as [u32; 0]);
    assert_eq!(ids(&mut function(3).instances()), [2]);
    let named = |name| view.named(name).map(|f| f.id().get()).collect::<Vec<_>>();

    // The main subprogram answers to `main` as well as its own name, and
    // in place of what is named `main`.
    assert_eq!(named("main"), [2]);
    assert_eq!(named("declared"), [2]);
    assert_eq!(named("missing"), [] as [u32; 0]);

    // Latest start first, each instance once though it names a range twice.
    let at = ImageAddress::new;
    assert_eq!(ids(&mut view.instances_containing(at(0x1006))), [1, 0]);
    assert_eq!(
        ids(&mut view.instances_containing(at(0x1010))),
        [] as [u32; 0]
    );

    // A physical instance stops where its prologue ends; an inlined one, or
    // a coroutine's body, at its own entry; one without an entry nowhere.
    let entries = |id| {
        view.instance(CodeInstanceId::new(id))
            .unwrap()
            .recommended_entries()
            .collect::<Vec<_>>()
    };

    let entry = |address, provenance| BreakpointEntry {
        address: at(address),
        provenance,
    };

    assert_eq!(entries(0), [entry(0x1004, EntryProvenance::Statement)]);
    assert_eq!(entries(1), [entry(0x1004, EntryProvenance::RangeStart)]);
    assert_eq!(entries(2), []);
    assert_eq!(entries(3), [entry(0x4008, EntryProvenance::CoroutineBody)]);

    let starts = view
        .instruction_starts(AddressRange {
            start: at(0x1000),
            end: at(0x2000),
        })
        .count();
    assert_eq!(starts, 1);
}

#[test]
fn every_table_size_lays_out() {
    for length in [0, 1, 63, 64, 65, 4095, 4096, 4097] {
        let mut pool = vec![b'x'; length];
        if let Some(last) = pool.last_mut() {
            *last = 0;
        }

        let mut builder = Builder::new(TARGET);
        builder.bytes(TableKind::Paths, pool.clone());
        let image = builder.seal(Limits::default()).unwrap();
        assert_eq!(image.bytes(TableKind::Paths), pool, "{length}");
        assert_eq!(
            image.as_bytes().len() % format::TABLE_ALIGNMENT,
            8,
            "{length}"
        );
        reopen(image.as_bytes()).unwrap();
    }
}

#[test]
fn damage_is_caught_by_the_checksum_or_by_validation() {
    let (tables, files) = sample();
    let image = seal(&tables, &files).unwrap();
    let bytes = image.as_bytes();

    for length in 0..bytes.len() {
        assert!(reopen(&bytes[..length]).is_err(), "truncated to {length}");
    }

    let mut accepted = 0;
    for at in 0..bytes.len() {
        for flip in [0x01, 0x80, 0xff] {
            let mut damaged = bytes.to_vec();
            damaged[at] ^= flip;
            assert!(
                reopen(&damaged).is_err(),
                "byte {at} ^ {flip:#x} passed the checksum"
            );

            // Past the checksum, an image validation accepts must read
            // without a panic.
            reseal(&mut damaged);
            if let Ok(image) = reopen(&damaged) {
                read_everything(&image);
                accepted += 1;
            }
        }
    }

    // Some changes are still valid images, such as another address.
    assert!(accepted > 0);
}

#[test]
fn the_header_is_checked() {
    let (tables, files) = sample();
    let bytes = seal(&tables, &files).unwrap().as_bytes().to_vec();
    let header = |change: fn(&mut Header)| {
        let mut bytes = bytes.clone();
        let (header, _) = <Header as zerocopy::FromBytes>::mut_from_prefix(&mut bytes).unwrap();
        change(header);
        reseal(&mut bytes);
        reopen(&bytes).unwrap_err()
    };

    assert_eq!(
        header(|header| header.magic[0] = b'X'),
        ImageError::NotAnImage
    );
    assert!(matches!(
        header(|header| header.format = 2.into()),
        ImageError::Format { .. }
    ));
    assert!(matches!(
        header(|header| header.normalization = (format::NORMALIZATION_REVISION + 1).into()),
        ImageError::Normalization { .. }
    ));
    assert_eq!(
        header(|header| header.layout = 1.into()),
        ImageError::Layout
    );
    assert_eq!(
        header(|header| header.length = 8.into()),
        ImageError::Truncated
    );
    assert!(matches!(
        header(|header| header.architecture = 9),
        ImageError::Malformed(_)
    ));
    assert!(matches!(
        header(|header| header.reserved[3] = 1),
        ImageError::Malformed(_)
    ));
    assert!(matches!(
        header(|header| header.tables = u32::try_from(TableKind::COUNT + 1).unwrap().into()),
        ImageError::Malformed(_)
    ));

    let small = Limits { bytes: 64 };
    let error = Image::from_bytes(AlignedBytes::copy_of(&bytes).unwrap(), small).unwrap_err();
    assert_eq!(error, ImageError::TooLarge);

    // The padding after each table that does not end at a boundary, and
    // before the trailer.
    let image = reopen(&bytes).unwrap();
    let mut gaps = image
        .tables
        .iter()
        .flatten()
        .map(|placed| placed.offset + placed.length)
        .filter(|end| end % format::TABLE_ALIGNMENT != 0)
        .collect::<Vec<_>>();
    assert!(!gaps.is_empty());
    gaps.push(bytes.len() - size_of::<format::Trailer>() - 1);
    for gap in gaps {
        let mut padded = bytes.clone();
        padded[gap] = 1;
        reseal(&mut padded);
        assert!(
            matches!(reopen(&padded).unwrap_err(), ImageError::Malformed(_)),
            "padding at {gap}"
        );
    }
}

/// Every table, indexes included, for sabotage.
struct Parts {
    tables: LineTables,
    ranges: Vec<LineRange>,
    statements: Vec<StatementKey>,
    boundaries: Vec<ControlBoundary>,
    files: Vec<FileRecord>,
    paths: Vec<u8>,
}

fn sabotaged(change: impl FnOnce(&mut Parts)) -> ImageError {
    let (tables, files) = sample();
    let indexes = lines::build_indexes(&tables);
    let mut paths = PathsBuilder::default();
    let records = files
        .paths()
        .iter()
        .map(|path| FileRecord {
            path: paths.intern(path).unwrap().0.into(),
        })
        .collect();

    let mut parts = Parts {
        tables,
        ranges: indexes.ranges,
        statements: indexes.statements,
        boundaries: indexes.boundaries,
        files: records,
        paths: paths.into_bytes(),
    };

    change(&mut parts);
    let mut builder = Builder::new(TARGET);
    builder
        .table(&parts.tables.addresses)
        .table(&parts.tables.rows)
        .table(&parts.tables.extras)
        .table(&parts.tables.sequences)
        .table(&parts.ranges)
        .table(&parts.statements)
        .table(&parts.boundaries)
        .table(&parts.files)
        .bytes(TableKind::Paths, parts.paths);
    builder.seal(Limits::default()).unwrap_err()
}

/// A change to the tables, and what validation says about it.
type Sabotage = (&'static str, &'static str, fn(&mut Parts));

const SABOTAGES: &[Sabotage] = &[
    (
        "an address too few",
        "rows and their addresses differ",
        |parts| {
            parts.tables.addresses.pop();
        },
    ),
    (
        "a file out of range",
        "names a file that does not exist",
        |parts| parts.tables.rows[0].file = 2.into(),
    ),
    ("an unknown flag", "flags it does not define", |parts| {
        parts.tables.rows[0].flags |= 0x80;
    }),
    ("a reserved byte", "flags it does not define", |parts| {
        parts.tables.rows[0].reserved = 1;
    }),
    ("a wide line without an extra", "has no extra", |parts| {
        parts.tables.rows[0].line = lines::WIDE_LINE.into();
    }),
    (
        "a flag without an extra",
        "rows and their extras differ",
        |parts| {
            parts.tables.rows[0].flags |= row_flags::EXTRA;
        },
    ),
    ("extras out of order", "extras are out of order", |parts| {
        parts.tables.extras.swap(0, 1);
    }),
    ("an extra for nothing", "extra it does not need", |parts| {
        parts.tables.extras[0].discriminator = 0.into();
    }),
    (
        "an extra whose line disagrees",
        "extra disagrees with its row",
        |parts| {
            parts.tables.extras[0].line = 6.into();
        },
    ),
    (
        "a narrow line in a wide extra",
        "extra disagrees with its row",
        |parts| {
            parts.tables.extras[1].line = 3.into();
        },
    ),
    ("a sequence gap", "sequences do not cover", |parts| {
        parts.tables.sequences[1].first = 7.into();
    }),
    ("a sequence past the rows", "runs past the rows", |parts| {
        parts.tables.sequences[1].rows = 4.into();
    }),
    ("an early end", "ends before its last row", |parts| {
        parts.tables.rows[1].flags |= row_flags::END_SEQUENCE;
    }),
    ("an empty range", "empty or names no located row", |parts| {
        parts.ranges[0].end = parts.ranges[0].start;
    }),
    (
        "a range at a fileless row",
        "empty or names no located row",
        |parts| {
            parts.ranges[0].row = 4.into();
        },
    ),
    (
        "ranges out of order",
        "line ranges are out of order",
        |parts| parts.ranges.swap(0, 1),
    ),
    (
        "a range's wrong running end",
        "running ends are wrong",
        |parts| {
            parts.ranges[1].prefix_max_end = 1.into();
        },
    ),
    ("a statement missing", "statement index misses", |parts| {
        parts.statements.pop();
    }),
    (
        "a statement's wrong line",
        "statement index disagrees",
        |parts| {
            // The last key, so that the index stays ordered.
            parts.statements.last_mut().unwrap().line = u64::MAX.into();
        },
    ),
    (
        "statements out of order",
        "statement index is out of order",
        |parts| parts.statements.swap(0, 1),
    ),
    (
        "a non-statement indexed",
        "statement index disagrees",
        |parts| parts.statements[0].row = 2.into(),
    ),
    ("a boundary missing", "boundary index misses", |parts| {
        parts.boundaries.pop();
    }),
    (
        "a boundary at a plain row",
        "names a row that marks none",
        |parts| parts.boundaries[0].row = 0.into(),
    ),
    (
        "boundaries out of order",
        "boundary index is out of order",
        |parts| parts.boundaries.swap(0, 1),
    ),
    ("a path past the pool", "names no path", |parts| {
        parts.files[0].path = 1000.into();
    }),
    (
        "an unterminated path",
        "path pool is not NUL-terminated",
        |parts| {
            parts.paths.pop();
        },
    ),
];

#[test]
fn validation_rejects_tables_that_disagree() {
    for (name, expected, sabotage) in SABOTAGES {
        let error = sabotaged(sabotage);
        assert!(
            matches!(&error, ImageError::Malformed(why) if why.contains(expected)),
            "{name}: {error}"
        );
    }
}

/// The sample with `kind`'s records changed and resealed, and what
/// validation says about it.
fn tampered<T>(kind: TableKind, change: impl FnOnce(&mut [T])) -> Result<(), ImageError>
where
    T: zerocopy::FromBytes + zerocopy::IntoBytes + zerocopy::KnownLayout,
{
    let (tables, files) = sample();
    let image = seal(&tables, &files).unwrap();
    let placed = image.tables[kind.index()].unwrap();
    let mut bytes = image.as_bytes().to_vec();

    change(
        <[T]>::mut_from_bytes(&mut bytes[placed.offset..placed.offset + placed.length]).unwrap(),
    );
    reseal(&mut bytes);
    reopen(&bytes).map(drop)
}

#[test]
#[expect(clippy::too_many_lines, reason = "one case for each check")]
fn validation_rejects_symbols_that_disagree() {
    use symbols::symbol_flags;
    let symbol = |change: fn(&mut [SymbolRecord])| tampered(TableKind::Symbols, change);
    let interval = |kind, change: fn(&mut [Interval])| tampered(kind, change);
    let cases = [
        (
            "an unknown kind",
            symbol(|s| s[4].kind = 9),
            "symbol is malformed",
        ),
        (
            "an unknown binding",
            symbol(|s| s[4].binding = 3),
            "symbol is malformed",
        ),
        (
            "an unknown role",
            symbol(|s| s[4].role = 99),
            "symbol is malformed",
        ),
        (
            "an unknown flag",
            symbol(|s| s[4].flags |= 0x80),
            "symbol is malformed",
        ),
        (
            "a name past the pool",
            symbol(|s| s[4].name = 9999.into()),
            "symbol is malformed",
        ),
        (
            "data with an extent",
            symbol(|s| s[0].kind = 2),
            "symbol is malformed",
        ),
        (
            "an extent and storage",
            symbol(|s| s[0].flags |= symbol_flags::STORAGE),
            "symbol is malformed",
        ),
        (
            "an inferred storage",
            symbol(|s| s[2].flags |= symbol_flags::INFERRED),
            "symbol is malformed",
        ),
        (
            "an empty extent",
            symbol(|s| s[0].end = s[0].address),
            "symbol is malformed",
        ),
        (
            "a repeated rank",
            symbol(|s| s[1].extent_rank = s[0].extent_rank),
            "out of range or repeated",
        ),
        (
            "a rank out of range",
            symbol(|s| s[2].storage_rank = 2.into()),
            "out of range or repeated",
        ),
        (
            "a rank it cannot have",
            symbol(|s| s[4].extent_rank = 0.into()),
            "a rank it cannot",
        ),
        (
            "an extent unindexed",
            interval(TableKind::SymbolExtents, |i| i[1].value = 0.into()),
            "indexes disagree",
        ),
        (
            "an index entry's wrong end",
            interval(TableKind::SymbolStorage, |i| {
                i[0].end = (i[0].end.get() - 1).into();
            }),
            "indexes disagree",
        ),
        (
            "unsized data's wrong end",
            interval(TableKind::UnsizedData, |i| {
                i[0].end = (i[0].end.get() + 1).into();
                i[0].prefix_max_end = i[0].end;
            }),
            "indexes disagree",
        ),
        (
            "a name for another symbol",
            tampered(TableKind::SymbolNames, |n: &mut [NameEntry]| {
                n[0].value = 4.into();
            }),
            "name index disagrees",
        ),
        (
            "names out of order",
            tampered(TableKind::SymbolNames, |n: &mut [NameEntry]| n.swap(0, 1)),
            "name index disagrees",
        ),
        (
            "an empty section",
            tampered(TableKind::Sections, |s: &mut [SectionRecord]| {
                s[0].end = s[0].start;
            }),
            "section is malformed",
        ),
        (
            "a section unindexed",
            interval(TableKind::SectionRanges, |i| i[1].value = 0.into()),
            "section index disagrees",
        ),
        (
            "an unknown GOT slot",
            tampered(TableKind::GotSlots, |g: &mut [GotRecord]| g[0].kind = 2),
            "GOT slot is malformed",
        ),
        (
            "a named indirect slot",
            tampered(TableKind::GotSlots, |g: &mut [GotRecord]| {
                g[1].name = g[0].name;
            }),
            "GOT slot is malformed",
        ),
    ];

    for (name, error, expected) in cases {
        assert!(
            matches!(&error, Err(ImageError::Malformed(why)) if why.contains(expected)),
            "{name}: {error:?}"
        );
    }
}

#[test]
#[expect(clippy::too_many_lines, reason = "one case for each check")]
fn validation_rejects_functions_that_disagree() {
    let function = |change: fn(&mut [FunctionRecord])| tampered(TableKind::Functions, change);
    let instance = |change: fn(&mut [InstanceRecord])| tampered(TableKind::CodeInstances, change);
    let cases = [
        (
            "a name past the pool",
            function(|f| f[0].name = 9999.into()),
            "function is malformed",
        ),
        (
            "an unknown language",
            function(|f| f[0].language = 200),
            "function is malformed",
        ),
        (
            "another language's code on a known one",
            function(|f| f[0].other_language = 1.into()),
            "function is malformed",
        ),
        (
            "an unknown role",
            function(|f| f[0].role = 99),
            "function is malformed",
        ),
        (
            "a main subprogram flag that is not one bit",
            function(|f| f[2].main_subprogram = 2),
            "function is malformed",
        ),
        (
            "a main subprogram `main` does not name",
            function(|f| f[1].main_subprogram = 1),
            "name index disagrees",
        ),
        (
            "`main` naming what is not the main subprogram",
            function(|f| f[2].main_subprogram = 0),
            "name index disagrees",
        ),
        (
            "a missing enclosure",
            function(|f| f[1].enclosing = 9.into()),
            "function is malformed",
        ),
        (
            "a declaration on line zero",
            function(|f| f[0].declaration.line = 0.into()),
            "function is malformed",
        ),
        (
            "a declaration in a missing file",
            function(|f| f[0].declaration.file = 7.into()),
            "function is malformed",
        ),
        (
            "generics past their table",
            function(|f| f[0].generics.count = 9.into()),
            "function is malformed",
        ),
        (
            "an instance claimed twice",
            function(|f| f[2].instances.count = 1.into()),
            "instance index disagrees",
        ),
        (
            "another function's instance",
            tampered(TableKind::FunctionInstances, |m: &mut [Member]| {
                m.swap(1, 2);
            }),
            "instance index disagrees",
        ),
        (
            "a name for another function",
            tampered(TableKind::FunctionNames, |n: &mut [NameEntry]| {
                n[0].value = 0.into();
            }),
            "name index disagrees",
        ),
        (
            "an unknown kind",
            instance(|i| i[0].inline = 2),
            "instance is malformed",
        ),
        (
            "a later parent",
            instance(|i| i[1].parent = 1.into()),
            "instance is malformed",
        ),
        (
            "a physical instance's call site",
            instance(|i| i[0].call_site = i[1].call_site),
            "instance is malformed",
        ),
        (
            "ranges past their table",
            instance(|i| i[3].ranges.count = 9.into()),
            "instance is malformed",
        ),
        (
            "an unknown provenance",
            instance(|i| i[0].provenance = 9),
            "instance is malformed",
        ),
        (
            "an entry without a provenance",
            instance(|i| i[2].entry = 1.into()),
            "instance is malformed",
        ),
        (
            "an empty range",
            tampered(TableKind::InstanceRanges, |r: &mut [RangeRecord]| {
                r[0].end = r[0].start;
            }),
            "ranges or entries are malformed",
        ),
        (
            "a range unindexed",
            tampered(TableKind::CodeRanges, |i: &mut [Interval]| {
                i[0].value = 3.into();
            }),
            "code range index disagrees",
        ),
        (
            // The second instance names one range twice, which the index
            // holds once; changing the copy leaves every interval one of
            // its instance's ranges, but the new range in none.
            "an instance's range missing from the index",
            tampered(TableKind::InstanceRanges, |r: &mut [RangeRecord]| {
                assert_eq!(r[2], r[3], "the sample repeats a range");
                r[3].end = (r[3].end.get() + 1).into();
            }),
            "code range index disagrees",
        ),
        (
            "unordered starts",
            tampered(TableKind::InstructionStarts, |s: &mut [StartRecord]| {
                s.swap(0, 1);
            }),
            "instruction starts are malformed",
        ),
        (
            "an unknown evidence",
            tampered(TableKind::InstructionStarts, |s: &mut [StartRecord]| {
                s[0].evidence = 9;
            }),
            "instruction starts are malformed",
        ),
    ];

    for (name, error, expected) in cases {
        assert!(
            matches!(&error, Err(ImageError::Malformed(why)) if why.contains(expected)),
            "{name}: {error:?}"
        );
    }
}

#[test]
fn unwinding_tables_read_back_as_their_lookups() {
    let (tables, files) = sample();
    let image = reopen(seal(&tables, &files).unwrap().as_bytes()).unwrap();
    let view = UnwindView::new(&image);
    let sample = sample_unwind();
    assert_eq!(view.eh_frame(), &*sample.eh_frame);
    assert_eq!(view.debug_frame(), &*sample.debug_frame);
    assert_eq!(
        (view.eh_frame_base(), view.text_base(), view.got_base()),
        (Some(0x9000), Some(0x1000), None)
    );
    assert_eq!((view.big_endian(), view.address_size()), (false, 8));

    // The earliest entry in the section wins, unless the section is
    // malformed before it.
    let eh_frame = view.eh_frame_index();
    let debug_frame = view.debug_frame_index();

    for (address, before, kept) in [
        (0x1090, Ok(8), eh_frame),
        (0x1150, Ok(24), eh_frame),
        (
            0x1450,
            Err(FdeMiss::Malformed("a CIE is malformed")),
            eh_frame,
        ),
        (
            0x0fff,
            Err(FdeMiss::Malformed("a CIE is malformed")),
            eh_frame,
        ),
        (0x2050, Ok(0), debug_frame),
        (0x2100, Err(FdeMiss::NoEntry), debug_frame),
    ] {
        assert_eq!(kept.lookup(address), before, "{address:#x}");
    }

    assert_eq!(eh_frame.entries, sample.eh_frame_index.entries);
    for (address, go) in [
        (0x2fff, false),
        (0x3000, true),
        (0x3100, false),
        (0x32ff, true),
    ] {
        assert_eq!(
            view.is_go_code(ImageAddress::new(address)),
            go,
            "{address:#x}"
        );
    }

    for (address, pushed) in [
        (0x33ff, None),
        (0x3400, Some(0)),
        (0x340a, Some(0)),
        (0x340b, Some(1)),
        (0x3410, None),
    ] {
        assert_eq!(
            view.stub_pushes(ImageAddress::new(address)),
            pushed,
            "{address:#x}"
        );
    }

    for (address, restored) in [
        (0x34ff, 0),
        (0x3500, 0x3f),
        (0x350b, 0x3f),
        (0x350c, 2),
        (0x3510, 0),
    ] {
        assert_eq!(
            view.restored(ImageAddress::new(address)),
            restored,
            "{address:#x}"
        );
    }

    for (address, frame) in [
        (0x35ff, None),
        (0x3600, Some(0)),
        (0x3601, Some(0x21)),
        (0x3603, Some(0x21)),
        (0x3604, None),
    ] {
        assert_eq!(
            view.proved_frame(ImageAddress::new(address)),
            frame,
            "{address:#x}"
        );
    }

    let go = sample.go.unwrap();
    let (bytes, facts) = view.go().unwrap();
    assert_eq!((bytes, facts), (&*go.bytes, go.facts));
    assert_eq!(view.frame_saves().collect::<Vec<_>>(), go.frame_saves);
}

#[test]
fn validation_rejects_unwinding_tables_that_disagree() {
    let facts = |change: fn(&mut [UnwindRecord])| tampered(TableKind::Unwind, change);
    let saves = |change: fn(&mut [FrameSaveRecord])| tampered(TableKind::GoFrameSaves, change);
    let interval = |kind, change: fn(&mut [Interval])| tampered(kind, change);
    let cases = [
        (
            "an unknown flag",
            facts(|u| u[0].flags |= 0x80),
            "facts are malformed",
        ),
        (
            "an address size of 2",
            facts(|u| u[0].address_size = 2),
            "facts are malformed",
        ),
        (
            "an error past its section",
            facts(|u| u[0].eh_frame_error_offset = 64.into()),
            "facts are malformed",
        ),
        (
            "an error offset without an error",
            facts(|u| u[0].debug_frame_error_offset = 4.into()),
            "facts are malformed",
        ),
        (
            "an error past the pool",
            facts(|u| u[0].eh_frame_error = 9999.into()),
            "facts are malformed",
        ),
        (
            "a base without its flag",
            facts(|u| u[0].got_base = 0x10.into()),
            "facts are malformed",
        ),
        (
            "Go facts without Go's table",
            facts(|u| u[0].flags &= !unwind_flags::GO_TABLE),
            "facts are malformed",
        ),
        (
            "a release without its flag",
            facts(|u| u[0].flags &= !unwind_flags::GO_RELEASE),
            "facts are malformed",
        ),
        (
            "an entry past its section",
            interval(TableKind::EhFrameIndex, |i| i[0].value = 64.into()),
            "unwinding index is malformed",
        ),
        (
            "unordered entries",
            interval(TableKind::DebugFrameIndex, |i| i[0].start = 0x2100.into()),
            "unwinding index is malformed",
        ),
        (
            "a Go range's wrong reach",
            interval(TableKind::GoCode, |i| i[1].prefix_max_end = 0x3100.into()),
            "unwinding index is malformed",
        ),
        (
            "a stub that pushes more than any does",
            interval(TableKind::StubFrames, |i| i[2].value = 3.into()),
            "unwinding index is malformed",
        ),
        (
            "a restored register no ABI keeps",
            interval(TableKind::RestoredFrames, |i| i[0].value = 0x40.into()),
            "unwinding index is malformed",
        ),
        (
            "an unknown save",
            saves(|s| s[0].saved = 2),
            "frame save is malformed",
        ),
        (
            "a backward save",
            saves(|s| s[0].end = 0x3000.into()),
            "frame save is malformed",
        ),
        (
            "a range never saved",
            saves(|s| s[1].end = 4.into()),
            "frame save is malformed",
        ),
    ];

    for (name, error, expected) in cases {
        assert!(
            matches!(&error, Err(ImageError::Malformed(why)) if why.contains(expected)),
            "{name}: {error:?}"
        );
    }
}

/// Strings whose hashes collide are pooled apart, each once.
#[test]
fn a_pool_keeps_strings_whose_hashes_collide() {
    let mut pool = super::strings::StringsBuilder::default();
    let first = pool.push_hashed("first", 7).unwrap();
    let second = pool.push_hashed("second", 7).unwrap();
    let third = pool.push_hashed("third", 7).unwrap();
    assert_eq!(
        [
            pool.push_hashed("third", 7),
            pool.push_hashed("first", 7),
            pool.push_hashed("second", 7),
        ],
        [Some(third), Some(first), Some(second)]
    );
    let bytes = pool.into_bytes();
    let strings = super::strings::Strings(&bytes);
    assert_eq!(
        [first, second, third].map(|id| strings.get(id)),
        ["first", "second", "third"]
    );
    assert_eq!(bytes.len(), "first second third ".len());
}

/// Two names of one record whose hashes collide are both kept and both
/// found.
#[test]
fn a_name_index_keeps_every_name_whose_hash_collides() {
    let mut seen = std::collections::HashMap::new();
    let (first, second) = (0..)
        .map(|number| format!("n{number}"))
        .find_map(|name| {
            seen.insert(index::hash(name.as_bytes()), name.clone())
                .map(|other| (other, name))
        })
        .unwrap();

    let mut strings = StringsBuilder::default();
    let ids = [
        strings.push(&first).unwrap(),
        strings.push(&second).unwrap(),
    ];
    let names = index::names([(first.as_str(), ids[0], 7), (second.as_str(), ids[1], 7)]);
    let pool = strings.into_bytes();
    let strings = Strings(&pool);
    assert_eq!(names.len(), 2);
    assert!(index::valid_names(&strings, &names, 8));
    for name in [&first, &second] {
        assert_eq!(index::named(strings, &names, name).collect::<Vec<_>>(), [7]);
    }
}

#[test]
fn types_read_back_with_their_indexes() {
    let (tables, files) = sample();
    let image = std::sync::Arc::new(reopen(seal(&tables, &files).unwrap().as_bytes()).unwrap());
    let (nodes, classes) = sample_types();
    let table = TypeTable::new(std::sync::Arc::clone(&image), crate::ModuleImageId::new(0));
    assert_eq!(table.nodes().cloned().collect::<Vec<_>>(), nodes);
    let view = TypeView::new(&image);
    let ids = |found: Vec<crate::TypeId>| {
        found
            .into_iter()
            .map(crate::TypeId::get)
            .collect::<Vec<_>>()
    };

    assert_eq!(ids(view.named("int *").collect()), [1, 20]);
    assert!(view.named("the type's size is negative").next().is_none());
    assert_eq!(ids(view.with_base("Point").collect()), [2]);

    // C's own spelling of a base type finds it too.
    assert_eq!(ids(view.with_base("short").collect()), [18]);
    assert_eq!(ids(view.with_enumerator("Red").collect()), [4]);
    assert!(view.with_enumerator("Blue").next().is_none());
    assert_eq!(view.go_runtime_type(0x100), Some(crate::TypeId::new(4)));
    assert_eq!(view.go_runtime_type(0x101), None);

    for (index, class) in classes.iter().enumerate() {
        assert_eq!(
            view.class(crate::TypeId::new(u32::try_from(index).unwrap())),
            Some(*class)
        );
    }

    // A type decodes once, and only when asked for.
    let other = TypeTable::new(image, crate::ModuleImageId::new(7));
    let first = other.node(crate::TypeId::new(2)).unwrap();
    assert_eq!(first.reference().image, crate::ModuleImageId::new(7));
    assert!(std::ptr::eq(
        first,
        other.node(crate::TypeId::new(2)).unwrap()
    ));
    assert!(other.node(crate::TypeId::new(21)).is_none());
}

#[test]
#[expect(clippy::too_many_lines, reason = "one case for each check")]
fn validation_rejects_types_that_disagree() {
    use super::types::{kinds, type_flags};
    let ty = |change: fn(&mut [TypeRecord])| tampered(TableKind::Types, change);
    let member = |change: fn(&mut [MemberRecord])| tampered(TableKind::TypeMembers, change);
    let identity = |change: fn(&mut [IdentityRecord])| tampered(TableKind::TypeIdentities, change);
    let argument = |change: fn(&mut [ArgumentRecord])| tampered(TableKind::TypeArguments, change);
    let item = |kind, change: fn(&mut [Item])| tampered(kind, change);
    let name = |kind, change: fn(&mut [NameEntry])| tampered(kind, change);
    let cases = [
        (
            "an unknown kind",
            ty(|t| t[0].kind = 16),
            "a type is malformed",
        ),
        (
            "a name past the pool",
            ty(|t| t[0].name = 9999.into()),
            "a type is malformed",
        ),
        (
            "an unknown encoding",
            ty(|t| t[0].detail = 7),
            "a type is malformed",
        ),
        (
            "a flag of another kind",
            ty(|t| t[0].flags = 0x41.into()),
            "a type is malformed",
        ),
        (
            "a size without its flag",
            ty(|t| t[10].byte_size = 1.into()),
            "a type is malformed",
        ),
        (
            "a bit size without its flag",
            ty(|t| t[0].bit_size = 1.into()),
            "a type is malformed",
        ),
        (
            "a target past the types",
            ty(|t| t[1].target = 21.into()),
            "a type is malformed",
        ),
        (
            "a reference without a target",
            ty(|t| t[15].target = NONE.into()),
            "a type is malformed",
        ),
        (
            "a pointer's list",
            ty(|t| t[1].parts.count = 1.into()),
            "a type is malformed",
        ),
        (
            "a malformed type's identity",
            ty(|t| t[14].identity = 0.into()),
            "a type is malformed",
        ),
        (
            "members out of order",
            ty(|t| t[2].parts.first = 1.into()),
            "a type is malformed",
        ),
        (
            "a member claimed twice",
            ty(|t| t[7].parts.first = t[2].parts.first),
            "a type is malformed",
        ),
        (
            "a stored and tagged discriminant",
            ty(|t| t[3].flags = (t[3].flags.get() | type_flags::TAG_TYPE).into()),
            "a type is malformed",
        ),
        (
            "a discriminant before the variants' members",
            ty(|t| t[3].discriminant = t[3].parts.first),
            "a type is malformed",
        ),
        (
            "an opaque type without text",
            ty(|t| t[13].text = NONE.into()),
            "a type is malformed",
        ),
        (
            "an identity out of order",
            ty(|t| t.swap(2, 3)),
            "a type is malformed",
        ),
        (
            "a list longer than its types say",
            ty(|t| t[12].parts.count = 1.into()),
            "lists do not match",
        ),
        (
            "a union read as a record",
            ty(|t| t[7].kind = kinds::RECORD),
            "a type is malformed",
        ),
        (
            "an unknown layout",
            member(|m| m[0].layout = 3),
            "member, base",
        ),
        (
            "a byte offset's bit size",
            member(|m| m[0].bit_size = 1.into()),
            "member, base",
        ),
        (
            "a member of no type",
            member(|m| m[0].ty = 21.into()),
            "member, base",
        ),
        (
            "an unknown accessibility",
            member(|m| m[0].accessibility = 3),
            "member, base",
        ),
        (
            "a declaration on line zero",
            member(|m| m[0].declaration.line = 0.into()),
            "member, base",
        ),
        (
            "an unknown selector",
            tampered(TableKind::TypeSelectors, |s: &mut [SelectorRecord]| {
                s[0].range = 2;
            }),
            "member, base",
        ),
        (
            "a value's high end",
            tampered(TableKind::TypeSelectors, |s: &mut [SelectorRecord]| {
                s[0].high.signed = 1;
            }),
            "member, base",
        ),
        (
            "a default variant's selectors",
            tampered(TableKind::TypeVariants, |v: &mut [VariantRecord]| {
                v[0].default = 1;
            }),
            "a type is malformed",
        ),
        (
            "an integer of unknown sign",
            tampered(
                TableKind::TypeEnumerators,
                |e: &mut [EnumeratorRecord]| {
                    e[0].value.signed = 2;
                },
            ),
            "member, base",
        ),
        (
            "a parameter of no type",
            item(TableKind::TypeParameters, |p| p[0].value = 21.into()),
            "member, base",
        ),
        (
            "an unknown language",
            identity(|i| i[0].language = 200),
            "identity is malformed",
        ),
        (
            "a pack past the arguments",
            identity(|i| i[1].pack = 4.into()),
            "identity is malformed",
        ),
        (
            "an unknown origin",
            identity(|i| i[0].origin = 3),
            "identity is malformed",
        ),
        (
            "a runtime type outside Go",
            identity(|i| i[0].runtime_type = 1.into()),
            "identity is malformed",
        ),
        (
            "a Go kind outside Go",
            identity(|i| i[0].go_kind = 1),
            "identity is malformed",
        ),
        (
            "segments out of order",
            identity(|i| i[1].inline = i[1].path),
            "identity is malformed",
        ),
        (
            "an unknown argument",
            argument(|a| a[0].kind = 3),
            "segments or arguments",
        ),
        (
            "a type argument's value",
            argument(|a| a[0].value.bits = 1.into()),
            "segments or arguments",
        ),
        (
            "a value argument's text",
            argument(|a| a[1].reference = 0.into()),
            "segments or arguments",
        ),
        (
            "a generic of no type",
            tampered(TableKind::Generics, |g: &mut [GenericRecord]| {
                g[0].argument = 21.into();
            }),
            "names a type the image lacks",
        ),
        (
            "a coroutine of no type",
            tampered(TableKind::Functions, |f: &mut [FunctionRecord]| {
                f[0].coroutine = 21.into();
            }),
            "names a type the image lacks",
        ),
        (
            "a name for another type",
            name(TableKind::TypeNames, |n| n[0].value = 14.into()),
            "type name index",
        ),
        (
            "a base for another type",
            name(TableKind::TypeBaseNames, |n| n[0].value = 1.into()),
            "base index",
        ),
        (
            "classes out of order",
            item(TableKind::TypeClasses, |c| c.swap(0, 1)),
            "classes are malformed",
        ),
        (
            "an enumerator of another type",
            name(TableKind::EnumeratorNames, |n| n[0].value = 0.into()),
            "enumerator index",
        ),
        (
            "a run-time dimension without its end",
            tampered(
                TableKind::RuntimeDimensions,
                |d: &mut [RuntimeDimensionRecord]| d[2].ends = 1,
            ),
            "a type is malformed",
        ),
        (
            "a later type for a descriptor",
            tampered(
                TableKind::GoRuntimeTypes,
                |r: &mut [RuntimeTypeRecord]| {
                    r[0].ty = 9.into();
                },
            ),
            "runtime type index",
        ),
    ];

    for (name, error, expected) in cases {
        assert!(
            matches!(&error, Err(ImageError::Malformed(why)) if why.contains(expected)),
            "{name}: {error:?}"
        );
    }
}

#[test]
fn packages_read_back_with_functions_by_local_name() {
    let (tables, files) = sample();
    let image = reopen(seal(&tables, &files).unwrap().as_bytes()).unwrap();
    let view = PackageView::new(&image);
    assert_eq!(view.package_name("main"), Some("main"));
    assert_eq!(view.package_name("example.com/m/stack"), Some("stack"));
    assert_eq!(view.package_name("example.com/m"), None);

    for (index, expected) in sample_packaged().iter().enumerate() {
        let function = FunctionId::new(u32::try_from(index).unwrap());
        assert_eq!(
            view.packaged_name(function),
            expected
                .as_ref()
                .map(|(package, local)| (*package, local.as_str())),
        );
    }

    assert_eq!(view.packaged_name(FunctionId::new(4)), None);
    assert_eq!(
        view.with_local_name("Push").collect::<Vec<_>>(),
        [FunctionId::new(1), FunctionId::new(3)]
    );
    assert_eq!(view.with_local_name("Pop").count(), 0);
    assert_eq!(SAMPLE_PACKAGES.len(), 3);
}

#[test]
fn validation_rejects_packages_that_disagree() {
    let package = |change: fn(&mut [PackageRecord])| tampered(TableKind::Packages, change);
    let packaged = |change: fn(&mut [PackagedRecord])| tampered(TableKind::PackagedNames, change);
    let local = |change: fn(&mut [NameEntry])| tampered(TableKind::LocalNames, change);
    let cases = [
        (
            "unordered packages",
            package(|p| p.swap(0, 1)),
            "package is malformed",
        ),
        (
            "a path past the pool",
            package(|p| p[0].path = 9999.into()),
            "package is malformed",
        ),
        (
            "a local without a package",
            packaged(|p| p[1].package = NONE.into()),
            "packaged name",
        ),
        (
            "a package without a local",
            packaged(|p| p[0].package = p[1].package),
            "packaged name",
        ),
        (
            "an unindexed local",
            packaged(|p| p[0] = p[1]),
            "local name index",
        ),
        (
            "another function's name",
            local(|n| n[0].value = 0.into()),
            "local name index",
        ),
        (
            "a function named twice",
            local(|n| n[1].value = n[0].value),
            "local name index",
        ),
    ];

    for (name, error, expected) in cases {
        assert!(
            matches!(&error, Err(ImageError::Malformed(why)) if why.contains(expected)),
            "{name}: {error:?}"
        );
    }
}

#[test]
fn facts_read_back_with_thread_locals_by_name() {
    let (tables, files) = sample();
    let image = reopen(seal(&tables, &files).unwrap().as_bytes()).unwrap();
    let view = FactsView::new(&image);
    assert_eq!(view.symbol_sources(), sample_sources());
    assert!(view.thread_local_storage());
    let expected = sample_thread_locals();
    let read = view
        .thread_locals()
        .map(|(name, place)| (std::sync::Arc::from(name), place))
        .collect::<std::collections::BTreeMap<_, _>>();
    assert_eq!(read, expected);

    for (name, place) in &expected {
        assert_eq!(view.thread_local(name).as_ref(), Some(place), "{name}");
    }

    assert_eq!(view.thread_local("count"), None);
    assert_eq!(view.thread_local("zzz"), None);
    assert_eq!(view.address_range(), sample_address_range());

    // The debug file's path is where binding found it, and binding must
    // name one when the image was built with one.
    let path = std::sync::Arc::new(std::path::PathBuf::from("/elsewhere/a.debug"));
    assert_eq!(
        view.debug_file(Some(path.clone())),
        Ok(Some(crate::DebugFile::Unusable {
            path,
            reason: "its build id differs".into(),
        }))
    );
    assert_eq!(view.debug_file(None), Err(super::facts::Unbound));
    assert_eq!(
        view.debug_information(),
        crate::DebugInformation::Incomplete {
            reason: "a unit is in a split DWARF file".into(),
        }
    );
    assert_eq!(
        view.locals_left_out().as_deref(),
        Some("they exceed the budget")
    );
    assert_eq!(image.bytes(TableKind::EmbeddedViews), b"views");
}

#[test]
#[expect(clippy::too_many_lines, reason = "one case for each check")]
fn validation_rejects_facts_that_disagree() {
    let facts = |change: fn(&mut [FactsRecord])| tampered(TableKind::Facts, change);
    let local = |change: fn(&mut [ThreadLocalRecord])| tampered(TableKind::ThreadLocals, change);
    let cases = [
        (
            "an unknown flag",
            facts(|f| f[0].flags |= 0x80),
            "facts are malformed",
        ),
        (
            "an unknown state",
            facts(|f| f[0].runtime_table = 3),
            "facts are malformed",
        ),
        (
            "a reason for a loaded table",
            facts(|f| f[0].runtime_reason = f[0].embedded_reason),
            "facts are malformed",
        ),
        (
            "an unusable table without a reason",
            facts(|f| f[0].embedded_reason = NONE.into()),
            "facts are malformed",
        ),
        (
            "an unknown debug file state",
            facts(|f| f[0].debug_file = 3),
            "facts are malformed",
        ),
        (
            "an unusable debug file without a reason",
            facts(|f| f[0].debug_reason = NONE.into()),
            "facts are malformed",
        ),
        (
            "a used debug file's reason",
            facts(|f| f[0].debug_file = super::facts::DEBUG_FILE_USED),
            "facts are malformed",
        ),
        (
            "no debug file's reason",
            facts(|f| f[0].debug_file = super::facts::DEBUG_FILE_NONE),
            "facts are malformed",
        ),
        (
            "an unknown DWARF state",
            facts(|f| f[0].dwarf = 4),
            "facts are malformed",
        ),
        (
            "an incomplete DWARF without a reason",
            facts(|f| f[0].dwarf_reason = NONE.into()),
            "facts are malformed",
        ),
        (
            "a loaded DWARF's reason",
            facts(|f| f[0].dwarf = super::facts::DWARF_LOADED),
            "facts are malformed",
        ),
        (
            "locals left out without a reason",
            facts(|f| f[0].locals_reason = NONE.into()),
            "facts are malformed",
        ),
        (
            "a reason for locals that were read",
            facts(|f| f[0].flags &= !super::facts::fact_flags::LOCALS_LEFT_OUT),
            "facts are malformed",
        ),
        (
            "an address range that ends before it starts",
            facts(|f| f[0].address_start = (f[0].address_end.get() + 1).into()),
            "facts are malformed",
        ),
        (
            "an unknown place",
            local(|l| l[0].place = 4),
            "thread-local variable",
        ),
        (
            "unordered names",
            local(|l| l.swap(0, 1)),
            "thread-local variable",
        ),
        (
            "a repeated name",
            local(|l| l[1].name = l[0].name),
            "thread-local variable",
        ),
        (
            "a known place's reason",
            local(|l| l[0].reason = l[2].reason),
            "thread-local variable",
        ),
        (
            "an unknown place's value",
            local(|l| l[2].value = 1.into()),
            "thread-local variable",
        ),
        (
            "a name past the pool",
            local(|l| l[0].name = 9999.into()),
            "thread-local variable",
        ),
    ];

    for (name, error, expected) in cases {
        assert!(
            matches!(&error, Err(ImageError::Malformed(why)) if why.contains(expected)),
            "{name}: {error:?}"
        );
    }
}

mod snapshot {
    use std::io::Write as _;

    use super::super::AlignedBytes;
    use super::super::backing::{FileStamp, ReadError};

    fn read_snapshot(
        path: &std::path::Path,
        limit: u64,
    ) -> Result<(AlignedBytes, FileStamp), ReadError> {
        read_snapshot_with(path, limit, &mut |_| {})
    }

    fn read_snapshot_with(
        path: &std::path::Path,
        limit: u64,
        between: &mut dyn FnMut(u32),
    ) -> Result<(AlignedBytes, FileStamp), ReadError> {
        super::super::backing::read_snapshot(path, limit, between, AlignedBytes::zeroed)
    }

    /// A directory removed when the test ends.
    struct ScratchDir(std::path::PathBuf);

    impl ScratchDir {
        fn new(name: &str) -> Self {
            let path =
                std::env::temp_dir().join(format!("microscope-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }

        fn path(&self) -> &std::path::Path {
            &self.0
        }
    }

    impl Drop for ScratchDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn a_snapshot_is_the_whole_file() {
        let dir = ScratchDir::new("image-snapshot");

        for length in [0_u16, 1, 4095, 4096, 4097] {
            let path = dir.path().join(format!("{length}"));
            let contents = (0..length)
                .map(|at| at.to_le_bytes()[0])
                .collect::<Vec<_>>();
            std::fs::write(&path, &contents).unwrap();
            let (bytes, stamp) = read_snapshot(&path, 1 << 20).unwrap();
            assert_eq!(&*bytes, contents, "{length}");
            assert_eq!(stamp.length, u64::from(length));
        }

        assert!(matches!(
            read_snapshot(&dir.path().join("4097"), 4096),
            Err(ReadError::TooLarge { .. })
        ));
        assert!(matches!(
            read_snapshot(dir.path(), 1),
            Err(ReadError::NotAFile { .. })
        ));
    }

    /// A stream is read to its end, within the limit.
    #[test]
    fn a_fifo_is_read_as_it_is_written() {
        let dir = ScratchDir::new("image-fifo");
        let path = dir.path().join("fifo");
        nix::unistd::mkfifo(&path, nix::sys::stat::Mode::S_IRWXU).unwrap();

        for (contents, limit) in [(vec![7_u8; 5000], 1 << 20), (vec![1_u8; 65], 64)] {
            let writer = {
                let (path, contents) = (path.clone(), contents.clone());
                std::thread::spawn(move || {
                    // A reader that stops at its limit closes the FIFO
                    // under the writer.
                    let _ = std::fs::write(path, contents);
                })
            };

            let read = read_snapshot(&path, limit);
            writer.join().unwrap();
            if contents.len() as u64 <= limit {
                let (bytes, stamp) = read.unwrap();
                assert_eq!(&*bytes, contents);
                assert_eq!(stamp.length, contents.len() as u64);
            } else {
                assert!(matches!(read, Err(ReadError::TooLarge { .. })));
            }
        }
    }

    #[test]
    fn a_file_that_changes_is_read_again() {
        let dir = ScratchDir::new("image-changing");
        let path = dir.path().join("changing");
        std::fs::write(&path, b"first").unwrap();
        let (bytes, _) = read_snapshot_with(&path, 1 << 20, &mut |attempt| {
            if attempt == 0 {
                std::fs::write(&path, b"second, longer").unwrap();
            }
        })
        .unwrap();
        assert_eq!(&*bytes, b"second, longer");

        // A file that grows under every read never yields a snapshot.
        let error = read_snapshot_with(&path, 1 << 20, &mut |_| {
            let mut file = std::fs::OpenOptions::new()
                .append(true)
                .open(&path)
                .unwrap();
            file.write_all(b"!").unwrap();
        })
        .unwrap_err();
        assert!(
            matches!(error, ReadError::Changed { attempts: 3, .. }),
            "{error}"
        );
    }
}

/// Locations read back from an image as they were pooled, and pooling one
/// again finds it rather than adding a copy.
#[test]
#[expect(clippy::too_many_lines, reason = "one check for each field and query")]
fn locations_read_back_as_they_were_pooled() {
    use super::locations::{ExpressionId, LocationListId, LocationTables};

    let mut pool = sample_locations();
    let (tables, files) = sample();
    let image = reopen(seal(&tables, &files).unwrap().as_bytes()).unwrap();
    let read = LocationTables::new(&image);
    let (expressions, lists) = (
        image.table::<ExpressionRecord>().len(),
        image.table::<LocationListRecord>().len(),
    );
    assert_eq!((expressions, lists), (3, 3));
    assert_eq!(
        read_locations(read, expressions, lists),
        read_locations(pool.tables(), expressions, lists)
    );

    let caller = read.expression(ExpressionId(2));
    assert_eq!(caller.bytes(), [0x98, 0x20, 0, 0x9f]);
    assert_eq!(caller.unit_offset(), Some(0x100));
    assert_eq!(read.unit_language(caller.unit()), Some(gimli::DW_LANG_C11));
    assert_eq!(caller.encoding().format, gimli::Format::Dwarf64);
    assert_eq!(caller.encoding().address_size, 4);
    assert_eq!(caller.base_type(0x10), Some(gimli::ValueType::I64));
    assert_eq!(caller.base_type(0x30), Some(gimli::ValueType::U32));
    assert_eq!(caller.base_type(0x20), None);
    assert_eq!(
        caller.addresses().collect::<Vec<_>>(),
        [(1, 0x1800), (3, 0x2000)]
    );
    assert_eq!(caller.indexed_address(3), Some(0x2000));
    assert_eq!(caller.indexed_address(2), None);
    assert_eq!(
        caller.procedures().collect::<Vec<_>>(),
        [(0x120, None), (0x140, Some(LocationListId(0)))]
    );
    assert!(matches!(
        caller.procedure(0x120),
        Some(super::locations::Procedure::Unlocated)
    ));
    assert_eq!(
        caller
            .procedure(0x140)
            .and_then(super::locations::Procedure::locations)
            .map(|list| list
                .entries()
                .map(|(range, expression)| (range, expression.id()))
                .collect::<Vec<_>>()),
        Some(vec![
            (None, ExpressionId(0)),
            (Some(super::sample::range(0x1000, 0x1010)), ExpressionId(1))
        ])
    );
    assert!(caller.procedure(0x130).is_none());
    let second = read.expression(ExpressionId(1));
    assert_eq!((second.unit_offset(), read.unit_language(1)), (None, None));
    assert!(read.list(LocationListId(2)).is_empty());

    // The same contents are the same row, however their parts were
    // ordered; any difference is another.
    let unit = caller.unit();
    let encoding = caller.encoding();

    let again = pool
        .expression(
            &[0x98, 0x20, 0, 0x9f],
            unit,
            encoding,
            &[(1, 0x1800), (3, 0x2000)],
            &[(0x120, None), (0x140, Some(LocationListId(0)))],
        )
        .unwrap();
    assert_eq!(again, ExpressionId(2));
    let differing = [
        pool.expression(
            &[0x98, 0x20, 0, 0x9f],
            unit,
            ENCODING,
            &[(1, 0x1800), (3, 0x2000)],
            &[(0x120, None), (0x140, Some(LocationListId(0)))],
        ),
        pool.expression(
            &[0x98, 0x20, 0, 0x9f],
            unit,
            encoding,
            &[(1, 0x1800)],
            &[(0x120, None), (0x140, Some(LocationListId(0)))],
        ),
        pool.expression(
            &[0x98, 0x20, 0, 0x9f],
            unit,
            encoding,
            &[(1, 0x1800), (3, 0x2000)],
            &[(0x140, Some(LocationListId(0)))],
        ),
        pool.expression(
            &[0x98, 0x20, 0, 0x9f],
            1,
            encoding,
            &[(1, 0x1800), (3, 0x2000)],
            &[(0x120, None), (0x140, Some(LocationListId(0)))],
        ),
    ];
    assert_eq!(
        differing.map(Result::unwrap),
        [3, 4, 5, 6].map(ExpressionId)
    );
    assert_eq!(
        pool.list(&[
            (Some(super::sample::range(0x1000, 0x1004)), ExpressionId(2)),
            (None, ExpressionId(0))
        ]),
        Ok(LocationListId(1))
    );
    assert_eq!(
        pool.list(&[
            (None, ExpressionId(0)),
            (Some(super::sample::range(0x1000, 0x1004)), ExpressionId(2))
        ]),
        Ok(LocationListId(3))
    );
}

#[test]
#[expect(clippy::too_many_lines, reason = "one check for each field and query")]
fn validation_rejects_locations_that_disagree() {
    use super::locations::unit_flags;

    let unit =
        |change: fn(&mut [EvaluationUnitRecord])| tampered(TableKind::EvaluationUnits, change);
    let base = |change: fn(&mut [BaseTypeRecord])| tampered(TableKind::BaseTypes, change);
    let list = |change: fn(&mut [LocationListRecord])| tampered(TableKind::LocationLists, change);
    let entry =
        |change: fn(&mut [LocationEntryRecord])| tampered(TableKind::LocationEntries, change);
    let expression = |change: fn(&mut [ExpressionRecord])| tampered(TableKind::Expressions, change);
    let address =
        |change: fn(&mut [IndexedAddressRecord])| tampered(TableKind::IndexedAddresses, change);
    let procedure = |change: fn(&mut [ProcedureRecord])| tampered(TableKind::Procedures, change);
    let cases = [
        (
            "an unknown unit flag",
            unit(|u| u[0].flags |= 0x80),
            "evaluation unit",
        ),
        (
            "an offset not flagged",
            unit(|u| u[1].offset = 4.into()),
            "evaluation unit",
        ),
        (
            "a language not flagged",
            unit(|u| u[1].language = 4.into()),
            "evaluation unit",
        ),
        (
            "base types past the table",
            unit(|u| u[0].base_types.count = 3.into()),
            "evaluation unit",
        ),
        (
            "an unknown value type",
            base(|b| b[0].value_type = 11),
            "evaluation unit",
        ),
        (
            "no value type",
            base(|b| b[1].value_type = 0),
            "evaluation unit",
        ),
        (
            "base types out of order",
            base(|b| b.swap(0, 1)),
            "evaluation unit",
        ),
        (
            "entries past the table",
            list(|l| l[2].entries.count = 1.into()),
            "location list",
        ),
        (
            "an entry of no expression",
            entry(|e| e[0].expression = 3.into()),
            "location list",
        ),
        (
            "a default with a range",
            entry(|e| e[0].end = 1.into()),
            "location list",
        ),
        (
            "an empty range",
            entry(|e| e[1].end = e[1].start),
            "location list",
        ),
        (
            "an unknown entry flag",
            entry(|e| e[0].flags = 2),
            "location list",
        ),
        (
            "an expression of no unit",
            expression(|x| x[0].unit = 2.into()),
            "expression",
        ),
        (
            "an unknown version",
            expression(|x| x[0].version = 6.into()),
            "expression",
        ),
        (
            "an odd address size",
            expression(|x| x[0].address_size = 3),
            "expression",
        ),
        (
            "an odd format",
            expression(|x| x[0].dwarf64 = 2),
            "expression",
        ),
        (
            "bytes past the pool",
            expression(|x| x[2].length = 5.into()),
            "expression",
        ),
        (
            "addresses past the table",
            expression(|x| x[1].addresses.count = 4.into()),
            "expression",
        ),
        (
            "procedures past the table",
            expression(|x| x[2].procedures.count = 3.into()),
            "expression",
        ),
        (
            "addresses out of order",
            address(|a| a.swap(1, 2)),
            "expression",
        ),
        (
            "procedures out of order",
            procedure(|p| p.swap(0, 1)),
            "expression",
        ),
        (
            "a procedure of no list",
            procedure(|p| p[0].list = 3.into()),
            "expression",
        ),
    ];
    assert_eq!(unit_flags::ALL, 3);

    for (name, error, expected) in cases {
        assert!(
            matches!(&error, Err(ImageError::Malformed(why)) if why.contains(expected)),
            "{name}: {error:?}"
        );
    }
}

/// Data objects and functions read back as they were encoded: objects of
/// one scope share its row and code, and a later entry for an index's key
/// replaces an earlier one.
#[test]
fn variables_read_back_as_they_were_encoded() {
    use super::variables::{
        ConstantValue, Metadata, MetadataAbsence, Object, ObjectId, ReturnConvention,
        TypeResolution, ValueDescription, VariableFunctionId, VariableView,
    };

    let input = super::sample::sample_variables();
    let (tables, files) = sample();
    let image = reopen(seal(&tables, &files).unwrap().as_bytes()).unwrap();
    let view = VariableView::new(&image);

    // x, y, u, and n share a scope; p, g, and f each have their own, and
    // every scope but the global's shares its code with a function.
    assert_eq!(image.table::<ScopeRecord>().len(), 4);
    assert_eq!(image.table::<CodeRange>().len(), 5);

    for (index, expected) in input.objects.iter().enumerate() {
        let object = view.object(ObjectId(u32::try_from(index).unwrap()));
        assert_eq!(object.name(), &*expected.name);
        assert_eq!(object.kind(), expected.kind);
        assert_eq!(object.declaration(), expected.declaration);
        assert!(object.ranges().eq(expected.ranges.iter().copied()));
        let go_declaration = object.go_declaration();
        assert_eq!(go_declaration.as_ref(), expected.go_declaration.as_deref());
        assert_eq!(object.instance(), expected.instance);
        assert_eq!(object.lexical_depth(), expected.lexical_depth);
        assert_eq!(object.type_info(), expected.type_info);
        assert_eq!(object.escaped(), expected.escaped);
        assert_eq!(object.hidden(), expected.hidden);
        assert_eq!(object.coroutine(), expected.coroutine);
        assert_eq!(object.value(), expected.value);
        assert_eq!(object.frame_base(), expected.frame_base);
        assert_eq!(object.malformed(), expected.malformed);
        assert_eq!(object.debug_info_offset(), expected.debug_info_offset);
    }

    let x = view.object(ObjectId(0));
    assert!(x.in_scope(ImageAddress::new(0x100f)) && !x.in_scope(ImageAddress::new(0x1010)));
    assert_eq!(x.location(), Some(super::locations::LocationListId(0)));
    assert_eq!(view.object(ObjectId(4)).location(), None);
    assert_eq!(
        view.object(ObjectId(4)).value(),
        Metadata::Value(ValueDescription::Constant(Box::new(
            ConstantValue::Unsigned(u128::MAX - 1)
        )))
    );

    for (index, expected) in input.functions.iter().enumerate() {
        let function = view.function(VariableFunctionId(u32::try_from(index).unwrap()));
        assert!(
            function
                .objects()
                .map(|object| object.id().0)
                .eq(expected.objects.iter().copied())
        );
        assert_eq!(function.name(), expected.name.as_deref());
        assert_eq!(function.captures(), expected.captures);
        assert_eq!(function.returns(), expected.returns);
    }

    assert_eq!(
        view.function(VariableFunctionId(0)).returns(),
        Some(ReturnConvention::GoRegisters)
    );
    let found = |address| {
        view.function_at(ImageAddress::new(address))
            .map(|f| f.id().0)
    };

    assert_eq!(
        [
            0xfff, 0x1000, 0x1006, 0x100c, 0x100f, 0x1010, 0x2007, 0x2008, 0x3000
        ]
        .map(found),
        [
            None,
            Some(0),
            Some(1),
            Some(0),
            Some(0),
            None,
            Some(0),
            None,
            Some(3)
        ]
    );
    let go = |address| {
        view.go_function(ImageAddress::new(address))
            .map(|f| f.id().0)
    };

    assert_eq!([0x1000, 0x1004, 0x1008].map(go), [Some(1), Some(1), None]);
    assert!(view.globals().map(Object::name).eq(["g"]));
    assert_eq!(view.global(0).map(Object::id), Some(ObjectId(2)));
    assert!(view.global(1).is_none());
    let at = |offset| view.object_at_offset(offset).map(|object| object.id().0);
    assert_eq!(
        [0x30, 0x40, 0x50, 0x41].map(at),
        [Some(5), Some(0), Some(3), None]
    );
    assert_eq!(
        view.procedure(0x80),
        Some(Metadata::Absent(MetadataAbsence::NoLocation))
    );
    assert_eq!(view.procedure(0x70), Some(Metadata::Malformed("m".into())));
    assert_eq!(view.procedure(0x60), None);
    assert!(matches!(
        view.object(ObjectId(1)).type_info(),
        TypeResolution::Malformed(why) if &*why == "no type"
    ));
    assert_eq!(view.object(ObjectId(1)).type_id(), None);
}

proptest::proptest! {
    /// A function is found at an address as before the image held them:
    /// of the functions whose code begins at or before it, those beginning
    /// last first, the first of them whose code contains it.
    #[test]
    fn a_function_is_found_where_its_code_holds_the_address(
        functions in proptest::collection::vec(
            proptest::collection::vec((0_u64..64, 1_u64..24), 0..3),
            0..8,
        ),
        addresses in proptest::collection::vec(0_u64..96, 1..16),
    ) {
        use super::variables::{Function, Variables, VariableView, add_to};

        let functions = functions
            .into_iter()
            .map(|ranges| Function {
                ranges: ranges
                    .into_iter()
                    .map(|(start, length)| super::sample::range(start, start + length))
                    .collect(),
                objects: Vec::new(),
                name: None,
                captures: Ok(Vec::new()),
                returns: None,
            })
            .collect::<Vec<_>>();
        let mut starts = std::collections::BTreeMap::<u64, Vec<usize>>::new();

        for (index, function) in functions.iter().enumerate() {
            for range in function.ranges.iter() {
                starts.entry(range.start.get()).or_default().push(index);
            }
        }

        let input = Variables { functions: functions.clone(), ..Variables::default() };
        let index = input.function_index().unwrap();
        let mut builder = Builder::new(TARGET);
        let mut strings = StringsBuilder::default();
        add_to(&mut builder, &mut strings, &input).unwrap();
        builder.bytes(TableKind::Strings, strings.into_bytes());
        let image = builder.seal(Limits::default()).unwrap();
        let view = VariableView::new(&image);

        for address in addresses {
            let expected = starts
                .range(..=address)
                .rev()
                .flat_map(|(_, functions)| functions.iter().copied())
                .find(|index| {
                    functions[*index]
                        .ranges
                        .iter()
                        .any(|range| range.contains(ImageAddress::new(address)))
                });
            let found = view
                .function_at(ImageAddress::new(address))
                .map(|function| function.id().0 as usize);
            proptest::prop_assert_eq!(found, expected, "at {:#x}", address);

            // The loader finds functions as the image does, before sealing.
            let before = index.function_at(ImageAddress::new(address));
            proptest::prop_assert!(
                match (before, expected) {
                    (Some(before), Some(found)) => std::ptr::eq(before, std::ptr::from_ref(&input.functions[found])),
                    (before, found) => before.is_none() && found.is_none(),
                },
                "before sealing, at {:#x}",
                address
            );
        }
    }
}

/// A global reads back its object and the names it answers to beyond the
/// object's.
#[test]
fn globals_read_back_with_their_names() {
    use super::variables::{ObjectId, VariableView};

    let (tables, files) = sample();
    let image = reopen(seal(&tables, &files).unwrap().as_bytes()).unwrap();
    let view = VariableView::new(&image);
    assert_eq!(view.global_count(), 1);
    let global = view.global_entry(0).unwrap();
    assert_eq!(global.object.id(), ObjectId(2));
    assert_eq!(
        (global.qualified_name, global.linkage_name, global.external),
        ("ns::g", Some("_ZN2ns1gE"), true)
    );
    assert!(view.global_entry(1).is_none());
}

#[test]
#[expect(clippy::too_many_lines, reason = "one case for each check")]
fn validation_rejects_variables_that_disagree() {
    use super::variables::{function_flags, object_flags, returns, value_kinds};

    let ranges = |change: fn(&mut [CodeRange])| tampered(TableKind::ScopeRanges, change);
    let scope = |change: fn(&mut [ScopeRecord])| tampered(TableKind::Scopes, change);
    let object = |change: fn(&mut [ObjectRecord])| tampered(TableKind::DataObjects, change);
    let constant = |change: fn(&mut [ConstantRecord])| tampered(TableKind::Constants, change);
    let function =
        |change: fn(&mut [VariableFunctionRecord])| tampered(TableKind::VariableFunctions, change);
    let capture = |change: fn(&mut [CaptureRecord])| tampered(TableKind::Captures, change);
    let start =
        |change: fn(&mut [FunctionStartRecord])| tampered(TableKind::FunctionStarts, change);
    let keyed = |kind, change: fn(&mut [Keyed])| tampered(kind, change);
    let item = |kind, change: fn(&mut [Item])| tampered(kind, change);
    let global = |change: fn(&mut [GlobalRecord])| tampered(TableKind::Globals, change);
    let procedure =
        |change: fn(&mut [DwarfProcedureRecord])| tampered(TableKind::DwarfProcedures, change);
    let cases = [
        (
            "empty code",
            ranges(|r| r[0].end = r[0].start),
            "code is empty",
        ),
        (
            "code past the table",
            scope(|s| s[0].ranges.count = 9.into()),
            "scope",
        ),
        (
            "an unknown instance",
            scope(|s| s[0].instance = 4.into()),
            "scope",
        ),
        (
            "an unknown Go instance",
            scope(|s| s[0].go_instance = 4.into()),
            "scope",
        ),
        (
            "a constant frame base",
            scope(|s| s[0].frame_base_kind = value_kinds::CONSTANT),
            "scope",
        ),
        (
            "a frame base of no list",
            scope(|s| s[0].frame_base = 3.into()),
            "scope",
        ),
        (
            "an absence naming something",
            scope(|s| s[1].frame_base = 1.into()),
            "scope",
        ),
        (
            "an unknown value kind",
            scope(|s| s[0].frame_base_kind = 9),
            "scope",
        ),
        (
            "bytes past the pool",
            constant(|c| c[1].high = 4.into()),
            "constant",
        ),
        (
            "an unknown constant kind",
            constant(|c| c[0].kind = 4),
            "constant",
        ),
        (
            "a name not in the pool",
            object(|o| o[0].name = 0xffff_fff0.into()),
            "data object",
        ),
        (
            "an unknown file",
            object(|o| o[0].declaration.file = 2.into()),
            "data object",
        ),
        (
            "an unknown scope",
            object(|o| o[0].scope = 4.into()),
            "data object",
        ),
        ("an unknown kind", object(|o| o[0].kind = 5), "data object"),
        (
            "an unknown flag",
            object(|o| o[0].flags |= 0x80),
            "data object",
        ),
        (
            "an unknown type",
            object(|o| o[0].ty = 999.into()),
            "data object",
        ),
        (
            "a malformed type not in the pool",
            object(|o| o[1].ty = 0xffff_fff0.into()),
            "data object",
        ),
        (
            "an unknown escaped type",
            object(|o| o[0].escaped = 999.into()),
            "data object",
        ),
        (
            "an unknown coroutine",
            object(|o| o[0].coroutine = 999.into()),
            "data object",
        ),
        (
            "a reason not in the pool",
            object(|o| o[1].malformed = 0xffff_fff0.into()),
            "data object",
        ),
        (
            "an offset not flagged",
            object(|o| o[2].debug_info_offset = 1.into()),
            "data object",
        ),
        (
            "a Go declaration not declared",
            object(|o| o[0].declaration = o[1].declaration),
            "data object",
        ),
        (
            "an unknown constant",
            object(|o| o[1].value = 9.into()),
            "data object",
        ),
        (
            "a value of no list",
            object(|o| o[0].value = 3.into()),
            "data object",
        ),
        (
            "an unknown value",
            object(|o| o[0].value_kind = 9),
            "data object",
        ),
        (
            "a capture's name",
            capture(|c| c[0].name = 0xffff_fff0.into()),
            "capture",
        ),
        (
            "a capture's flag",
            capture(|c| c[0].malformed = 2),
            "capture",
        ),
        (
            "a capture's type",
            capture(|c| c[0].ty = 999.into()),
            "capture",
        ),
        (
            "an unknown object",
            item(TableKind::FunctionObjects, |i| i[0].value = 7.into()),
            "no data object",
        ),
        (
            "an unknown global",
            global(|g| g[0].object = 7.into()),
            "global",
        ),
        (
            "a global's qualified name",
            global(|g| g[0].qualified_name = 0xffff_fff0.into()),
            "global",
        ),
        (
            "a global's linkage name",
            global(|g| g[0].linkage_name = 0xffff_fff0.into()),
            "global",
        ),
        (
            "a global's visibility",
            global(|g| g[0].external = 2),
            "global",
        ),
        (
            "objects past the list",
            function(|f| f[0].objects.count = 8.into()),
            "function",
        ),
        (
            "a function's name",
            function(|f| f[0].name = 0xffff_fff0.into()),
            "function",
        ),
        (
            "captures past the table",
            function(|f| f[0].captures.count = 3.into()),
            "function",
        ),
        (
            "malformed captures with some",
            function(|f| f[1].captures.count = 1.into()),
            "function",
        ),
        (
            "an unknown flag",
            function(|f| f[0].flags |= 0x80),
            "function",
        ),
        (
            "an unknown convention",
            function(|f| f[0].returns = 3),
            "function",
        ),
        (
            "Go's with a language",
            function(|f| f[0].language = 1),
            "function",
        ),
        (
            "Go's rewritten",
            function(|f| f[0].flags |= function_flags::REWRITTEN),
            "function",
        ),
        (
            "an unknown returned type",
            function(|f| f[1].returned_type = 999.into()),
            "function",
        ),
        (
            "an unknown language",
            function(|f| f[2].language = 200),
            "function",
        ),
        (
            "System V unnamed",
            function(|f| f[1].returned_name = NONE.into()),
            "function",
        ),
        (
            "starts out of order",
            start(|s| {
                s.swap(0, 1);
                s[0].prefix_max_end = s[0].end;
                s[1].prefix_max_end = s[0].end.max(s[1].end);
            }),
            "function starts",
        ),
        (
            "a wrong prefix",
            start(|s| s[1].prefix_max_end = 0x100c.into()),
            "function starts",
        ),
        (
            "an empty start",
            start(|s| s[0].end = s[0].start),
            "function starts",
        ),
        (
            "an unknown function",
            start(|s| s[0].function = 4.into()),
            "function starts",
        ),
        (
            "Go entries out of order",
            keyed(TableKind::GoEntries, |k| k.swap(0, 1)),
            "index of variables",
        ),
        (
            "an entry of no function",
            keyed(TableKind::GoEntries, |k| k[0].value = 4.into()),
            "index of variables",
        ),
        (
            "an offset of no object",
            keyed(TableKind::ObjectOffsets, |k| k[0].value = 7.into()),
            "index of variables",
        ),
        (
            "procedures out of order",
            procedure(|p| p.swap(0, 1)),
            "DWARF procedure",
        ),
        (
            "a constant procedure",
            procedure(|p| p[0].location_kind = value_kinds::CONSTANT),
            "DWARF procedure",
        ),
    ];
    assert_eq!(object_flags::ALL, 0b1111);
    assert_eq!(returns::SYSTEM_V, 2);

    for (name, error, expected) in cases {
        assert!(
            matches!(&error, Err(ImageError::Malformed(why)) if why.contains(expected)),
            "{name}: {error:?}"
        );
    }
}

/// Calls read back as they were encoded, and the sites returning to an
/// address are all found, however many.
#[test]
fn calls_read_back_as_they_were_encoded() {
    use super::calls::{CallView, SiteId};

    let input = super::sample::sample_calls();
    let (tables, files) = sample();
    let image = reopen(seal(&tables, &files).unwrap().as_bytes()).unwrap();
    let view = CallView::new(&image);

    for (index, expected) in (0_u32..).zip(&input.functions) {
        let function = view.function(index);
        assert_eq!(function.name(), expected.name);
        assert_eq!(function.frame_base(), expected.frame_base);
        assert_eq!(
            function.tail_calls_described(),
            expected.tail_calls_described
        );
        assert!(
            function
                .tail_calls()
                .map(|site| site.0)
                .eq(expected.tail_calls.iter().copied())
        );
    }

    for (index, expected) in (0_u32..).zip(&input.sites) {
        let site = view.site(SiteId(index)).unwrap();
        assert_eq!(site.function(), expected.function);
        assert_eq!(site.return_address(), expected.return_address);
        assert_eq!(site.target(), expected.target);
        assert_eq!(site.enters(), expected.enters);
        assert_eq!(site.jump(), expected.jump);
        assert!(site.parameters().eq(expected.parameters.iter().copied()));
        assert_eq!(site.malformed(), expected.malformed);
    }

    assert!(view.site(SiteId(5)).is_none());
    let returning = |address| {
        view.returning_to(ImageAddress::new(address))
            .map(|site| site.0)
            .collect::<Vec<_>>()
    };

    assert_eq!(returning(0x1008), [0, 3]);
    assert_eq!(returning(0x1004), [4]);
    assert_eq!(returning(0x1005), [0; 0]);
    assert_eq!(returning(0x2000), [0; 0]);
}

#[test]
#[expect(clippy::too_many_lines, reason = "one check for each field and query")]
fn validation_rejects_calls_that_disagree() {
    use super::calls::{site_flags, targets};
    use super::variables::value_kinds;

    let function =
        |change: fn(&mut [CallingFunctionRecord])| tampered(TableKind::CallingFunctions, change);
    let site = |change: fn(&mut [CallSiteRecord])| tampered(TableKind::CallSites, change);
    let parameter =
        |change: fn(&mut [SiteParameterRecord])| tampered(TableKind::SiteParameters, change);
    let tail = |change: fn(&mut [Item])| tampered(TableKind::TailCalls, change);
    let returns = |change: fn(&mut [Keyed])| tampered(TableKind::CallReturns, change);
    let cases = [
        (
            "a name not in the pool",
            function(|f| f[0].name = 0xffff_fff0.into()),
            "calling",
        ),
        ("an unknown flag", function(|f| f[0].flags = 2), "calling"),
        (
            "a frame base of no list",
            function(|f| f[0].frame_base = 3.into()),
            "calling",
        ),
        (
            "a constant frame base",
            function(|f| f[0].frame_base_kind = value_kinds::CONSTANT),
            "calling",
        ),
        (
            "tail calls past the table",
            function(|f| f[2].tail_calls.count = 2.into()),
            "calling",
        ),
        (
            "a tail call of no site",
            tail(|t| t[0].value = 5.into()),
            "names no site",
        ),
        (
            "an unknown parameter flag",
            parameter(|p| p[0].flags = 4),
            "parameter",
        ),
        (
            "a register not flagged",
            parameter(|p| p[1].register = 3.into()),
            "parameter",
        ),
        (
            "a parameter not flagged",
            parameter(|p| p[0].parameter = 3.into()),
            "parameter",
        ),
        (
            "a value of no expression",
            parameter(|p| p[0].value = 3.into()),
            "parameter",
        ),
        (
            "a referent of no expression",
            parameter(|p| p[1].data_value = 3.into()),
            "parameter",
        ),
        (
            "a site of no function",
            site(|s| s[0].function = 3.into()),
            "call site is",
        ),
        (
            "entering no function",
            site(|s| s[1].enters = 3.into()),
            "call site is",
        ),
        (
            "an unknown flag",
            site(|s| s[0].flags |= 0x80),
            "call site is",
        ),
        (
            "a return not flagged",
            site(|s| s[1].return_address = 1.into()),
            "call site is",
        ),
        (
            "a jump not flagged",
            site(|s| s[0].jump_lookup = 1.into()),
            "call site is",
        ),
        (
            "parameters past the table",
            site(|s| s[0].parameters.count = 3.into()),
            "call site is",
        ),
        (
            "a reason not in the pool",
            site(|s| s[3].malformed = 0xffff_fff0.into()),
            "call site is",
        ),
        (
            "an unknown target",
            site(|s| s[0].target_kind = 5),
            "call site is",
        ),
        (
            "an unknown naming something",
            site(|s| s[4].target = 1.into()),
            "call site is",
        ),
        (
            "a symbol not in the pool",
            site(|s| s[1].target = 0xffff_fff0.into()),
            "call site is",
        ),
        (
            "a target of no list",
            site(|s| s[2].target = 3.into()),
            "call site is",
        ),
        (
            "a list past u32",
            site(|s| {
                s[2].target = (1_u64 << 40).into();
                s[2].target_kind = targets::COMPUTED;
            }),
            "call site is",
        ),
        (
            "returns out of order",
            returns(|r| r.swap(0, 1)),
            "where calls return",
        ),
        (
            "a return of no site",
            returns(|r| r[0].value = 5.into()),
            "where calls return",
        ),
        (
            "a return elsewhere",
            returns(|r| r[0].key = 0x1009.into()),
            "where calls return",
        ),
        (
            "a return left out",
            site(|s| {
                s[1].flags |= site_flags::RETURN_ADDRESS;
                s[1].return_address = 0x2000.into();
            }),
            "where calls return",
        ),
    ];

    for (name, error, expected) in cases {
        assert!(
            matches!(&error, Err(ImageError::Malformed(why)) if why.contains(expected)),
            "{name}: {error:?}"
        );
    }
}

/// Type facts read back by their keys, whatever order they were given in.
#[test]
fn type_facts_read_back_by_their_keys() {
    use super::locations::ExpressionId;
    use super::type_facts::{LayoutChild, TypeFactsView};

    let (tables, files) = sample();
    let image = reopen(seal(&tables, &files).unwrap().as_bytes()).unwrap();
    let view = TypeFactsView::new(&image);
    let ty = crate::TypeId::new;
    assert_eq!(
        [0, 2, 3, 5].map(|id| view.dictionary_index(ty(id))),
        [None, Some(1), None, Some(0)]
    );
    assert_eq!(
        [1, 2, 4].map(|id| view.passed_by_value(ty(id))),
        [Some(true), None, Some(false)]
    );
    assert_eq!(view.complex_part("float", 4), Some(ty(1)));
    assert_eq!(view.complex_part("double", 8), Some(ty(2)));
    assert_eq!(view.complex_part("float", 8), Some(ty(3)));
    assert_eq!(view.complex_part("float", 16), None);
    assert_eq!(view.complex_part("long double", 8), None);
    let layout = |aggregate, child| view.dynamic_layout(ty(aggregate), child);
    assert_eq!(layout(4, LayoutChild::Discriminant), Some(ExpressionId(0)));
    assert_eq!(layout(4, LayoutChild::Member(1)), Some(ExpressionId(2)));
    assert_eq!(layout(4, LayoutChild::Base(1)), None);
    assert_eq!(
        layout(
            1,
            LayoutChild::VariantMember {
                variant: 1,
                member: 0
            }
        ),
        Some(ExpressionId(1))
    );
    assert_eq!(
        layout(
            1,
            LayoutChild::VariantMember {
                variant: 0,
                member: 1
            }
        ),
        None
    );
}

#[test]
fn validation_rejects_type_facts_that_disagree() {
    let fact = |kind, change: fn(&mut [TypeFactRecord])| tampered(kind, change);
    let part = |change: fn(&mut [ComplexPartRecord])| tampered(TableKind::ComplexParts, change);
    let layout =
        |change: fn(&mut [DynamicLayoutRecord])| tampered(TableKind::DynamicLayouts, change);
    let cases = [
        (
            "indices out of order",
            fact(TableKind::DictionaryIndices, |f| f.swap(0, 1)),
            "type fact",
        ),
        (
            "an index of no type",
            fact(TableKind::DictionaryIndices, |f| f[1].ty = 999.into()),
            "type fact",
        ),
        (
            "a class twice",
            fact(TableKind::PassedByValue, |f| f[1].ty = f[0].ty),
            "type fact",
        ),
        (
            "passed neither way",
            fact(TableKind::PassedByValue, |f| f[0].value = 2.into()),
            "type fact",
        ),
        ("parts out of order", part(|p| p.swap(0, 1)), "complex"),
        (
            "a part's name",
            part(|p| p[0].name = 0xffff_fff0.into()),
            "complex",
        ),
        (
            "a part of no type",
            part(|p| p[0].ty = 999.into()),
            "complex",
        ),
        (
            "layouts out of order",
            layout(|l| l.swap(0, 1)),
            "run-time layout",
        ),
        (
            "an aggregate of no type",
            layout(|l| l[2].aggregate = 999.into()),
            "run-time layout",
        ),
        (
            "a layout of no expression",
            layout(|l| l[0].expression = 3.into()),
            "run-time layout",
        ),
        (
            "an unknown child",
            layout(|l| l[2].kind = 8),
            "run-time layout",
        ),
        (
            "a discriminant's index",
            layout(|l| l[2].first = 1.into()),
            "run-time layout",
        ),
        (
            "a member's second index",
            layout(|l| l[1].second = 1.into()),
            "run-time layout",
        ),
    ];

    for (name, error, expected) in cases {
        assert!(
            matches!(&error, Err(ImageError::Malformed(why)) if why.contains(expected)),
            "{name}: {error:?}"
        );
    }
}

/// Constants by name and vtables by address read back the last value given
/// for each, and producers once each in the order first named.
#[test]
fn declarations_read_back_by_name_and_address() {
    use super::declarations::DeclarationView;
    use crate::IntegerValue::{Signed, Unsigned};

    let (tables, files) = sample();
    let image = reopen(seal(&tables, &files).unwrap().as_bytes()).unwrap();
    let view = DeclarationView::new(&image);
    assert_eq!(view.constant("runtime._Grunning"), Some(Signed(-1)));
    assert_eq!(view.constant("max"), Some(Unsigned(u128::MAX)));
    assert_eq!(view.constant("min"), Some(Signed(i128::MIN)));
    assert_eq!(view.constant("runtime"), None);
    assert!(
        view.constants()
            .map(|(name, _)| name)
            .eq(["max", "min", "runtime._Grunning"])
    );
    let vtable = |address| {
        view.vtable(ImageAddress::new(address))
            .map(crate::TypeId::get)
    };

    assert_eq!(
        [0x5000, 0x6000, 0x5001].map(vtable),
        [Some(1), Some(0), None]
    );
    assert_eq!(view.vtables().len(), 2);
    assert!(view.producers().eq(["rustc", "clang"]));
}

#[test]
fn validation_rejects_declarations_that_disagree() {
    let constant =
        |change: fn(&mut [NamedConstantRecord])| tampered(TableKind::NamedConstants, change);
    let vtable = |change: fn(&mut [VtableRecord])| tampered(TableKind::Vtables, change);
    let producer = |change: fn(&mut [Item])| tampered(TableKind::Producers, change);
    let cases = [
        (
            "constants out of order",
            constant(|c| c.swap(0, 1)),
            "named constant",
        ),
        (
            "a name twice",
            constant(|c| c[1].name = c[0].name),
            "named constant",
        ),
        (
            "a constant's name",
            constant(|c| c[0].name = 0xffff_fff0.into()),
            "named constant",
        ),
        ("a sign", constant(|c| c[0].signed = 2), "named constant"),
        ("vtables out of order", vtable(|v| v.swap(0, 1)), "vtable"),
        (
            "a vtable of no type",
            vtable(|v| v[0].ty = 999.into()),
            "vtable",
        ),
        ("a producer twice", producer(|p| p[1] = p[0]), "producer"),
        (
            "a producer's name",
            producer(|p| p[0].value = 0xffff_fff0.into()),
            "producer",
        ),
    ];

    for (name, error, expected) in cases {
        assert!(
            matches!(&error, Err(ImageError::Malformed(why)) if why.contains(expected)),
            "{name}: {error:?}"
        );
    }
}

/// Resume points read back as the loader decoded them, by instance, and
/// held ranges by entry and state, the later of two for a key.
#[test]
fn resumes_read_back_by_instance_and_state() {
    use super::resumes::ResumeView;
    use super::sample::sample_resumes;

    let (tables, files) = sample();
    let image = reopen(seal(&tables, &files).unwrap().as_bytes()).unwrap();
    let view = ResumeView::new(&image);
    let given = sample_resumes();
    let instance = CodeInstanceId::new;
    assert_eq!(
        view.resume_points(instance(0)),
        Some(given.points[1].1.clone())
    );
    assert_eq!(
        view.resume_points(instance(3)),
        Some(Err("an indirect dispatch".into()))
    );
    assert_eq!(view.resume_points(instance(1)), None);
    assert_eq!(view.resume_points(instance(9)), None);
    assert_eq!(
        view.resume_code()
            .map(|(code, instance)| (code.start.get(), code.end.get(), instance.get()))
            .collect::<Vec<_>>(),
        [
            (0x1000, 0x1002, 0),
            (0x1002, 0x1004, 0),
            (0x1008, 0x100c, 0),
            (0x2000, 0x2004, 0)
        ],
        "empty code is left out, and an undecodable instance has none"
    );
    let held = |offset, state| {
        view.held(offset, state).map(|held| {
            held.map(|code| (code.start.get(), code.end.get()))
                .collect::<Vec<_>>()
        })
    };

    assert_eq!(held(0x40, 3), Some(vec![(0x100c, 0x1010)]));
    assert_eq!(
        held(0x40, 4),
        Some(vec![(0x100c, 0x1010), (0x2000, 0x2008)])
    );
    assert_eq!(held(0x20, 3), Some(vec![]));
    assert_eq!(held(0x20, 4), None);
    assert_eq!(held(0x40, 0), None);
}

#[test]
fn validation_rejects_resumes_that_disagree() {
    use super::variables::CodeRange;

    let resume = |change: fn(&mut [ResumeRecord])| tampered(TableKind::Resumes, change);
    let point = |change: fn(&mut [ResumePointRecord])| tampered(TableKind::ResumePoints, change);
    let held = |change: fn(&mut [HeldRecord])| tampered(TableKind::Held, change);
    let cases = [
        (
            "instances out of order",
            resume(|r| r.swap(0, 1)),
            "resume points",
        ),
        (
            "an instance of no code",
            resume(|r| r[1].instance = 99.into()),
            "resume points",
        ),
        (
            "points past the table",
            resume(|r| r[0].points.count = 4.into()),
            "resume points",
        ),
        (
            "dispatch past the ranges",
            resume(|r| r[0].dispatch.first = 0xffff_fff0.into()),
            "resume points",
        ),
        (
            "a reason of no string",
            resume(|r| r[1].malformed = 0xffff_fff0.into()),
            "resume points",
        ),
        (
            "an undecodable instance's points",
            resume(|r| r[1].points.count = 1.into()),
            "resume points",
        ),
        (
            "a resumption past the ranges",
            point(|p| p[1].resumption.count = 99.into()),
            "resume point is",
        ),
        ("held out of order", held(|h| h.swap(0, 1)), "held range"),
        ("a key twice", held(|h| h[1] = h[0]), "held range"),
        (
            "held past the ranges",
            held(|h| h[2].ranges.first = 99.into()),
            "held range",
        ),
    ];

    for (name, error, expected) in cases {
        assert!(
            matches!(&error, Err(ImageError::Malformed(why)) if why.contains(expected)),
            "{name}: {error:?}"
        );
    }

    // The ranges themselves are any addresses.
    tampered(TableKind::ResumeRanges, |r: &mut [CodeRange]| {
        r[0].end = 0.into();
    })
    .unwrap();
}

/// A table finds the coroutines its types hold as the loader does from the
/// whole graph, decoding only types named as coroutines: a record or a
/// variant so named is one, readable or not, and a pointer or a malformed
/// type so named is not.
#[test]
fn a_type_table_finds_the_coroutines_the_graph_holds() {
    use std::sync::Arc;

    use crate::{ModuleImageId, RecordKind, TypeId, TypeInfo, TypeKind, TypeNode, TypeReference};

    let image = ModuleImageId::new(0);
    let reference = |id| TypeReference {
        image,
        id: TypeId::new(id),
    };

    let node = |id, name: &str, kind| {
        TypeNode::Resolved(TypeInfo {
            reference: reference(id),
            name: name.into(),
            byte_size: Some(8),
            kind,
            identity: None,
        })
    };

    let record = TypeKind::Record {
        kind: RecordKind::Struct,
        members: [].into(),
        bases: [].into(),
        incomplete: false,
    };

    let nodes = vec![
        node(0, "{async_fn_env#0}", record.clone()),
        node(
            1,
            "{async_block_env#1}",
            TypeKind::Pointer {
                target: Some(reference(0)),
                address_class: 0,
            },
        ),
        TypeNode::Malformed {
            reference: reference(2),
            description: "{async_fn_env#2}".into(),
        },
        node(3, "plain", record.clone()),
        node(4, "{async_closure_env#0}", record),
    ];
    let mut builder = Builder::new(TARGET);
    let mut strings = StringsBuilder::default();

    super::types::add_to(
        &mut builder,
        &mut strings,
        &super::types::Types {
            nodes: &nodes,
            classes: &[0, 1, 2, 3, 4],
        },
    )
    .unwrap();
    builder.bytes(TableKind::Strings, strings.into_bytes());
    let table = TypeTable::new(Arc::new(builder.seal(Limits::default()).unwrap()), image);
    let expected = crate::debug_info::coroutines::normalize(&nodes);
    assert_eq!(
        expected.keys().copied().collect::<Vec<_>>(),
        [TypeId::new(0), TypeId::new(4)]
    );

    for id in 0..5 {
        let id = TypeId::new(id);
        assert_eq!(
            table
                .coroutine(id)
                .map(|found| found.cloned().map_err(Arc::clone)),
            expected.get(&id).cloned(),
            "{id:?}"
        );
    }
}

/// A Python runtime reads back as it was sealed, with its layout or the
/// reason it has none.
#[test]
fn a_python_runtime_reads_back_with_its_layout_or_its_problem() {
    use super::python::{
        self, Constant, FREE_THREADED, Field, PyVersion, PythonLayout, PythonRuntime, TARGET_TABLE,
    };

    let sealed = |runtime: Option<&PythonRuntime>| {
        let mut builder = Builder::new(TARGET);
        let mut strings = StringsBuilder::default();
        python::add_to(&mut builder, &mut strings, runtime).unwrap();
        builder.bytes(TableKind::Strings, strings.into_bytes());
        python::runtime(&builder.seal(Limits::default()).unwrap())
    };

    let mut layout = PythonLayout::default();
    layout.set_field(Field::ThreadNext, 8);
    layout.set_field(Field::FrameTlbcIndex, 0x1_0000_0000);
    layout.set_constant(Constant::OwnerEntry, 3);
    let runtime = PythonRuntime {
        version: PyVersion(0x030e_04f0),
        flags: TARGET_TABLE | FREE_THREADED,
        runtime: AddressRange {
            start: ImageAddress::new(0x1000),
            end: ImageAddress::new(0x2000),
        },
        layout: Ok(layout),
    };
    let read = sealed(Some(&runtime)).unwrap();
    assert_eq!(read, runtime);
    let layout = read.layout.unwrap();
    assert_eq!(layout.field(Field::ThreadNext), Some(8));
    assert_eq!(layout.field(Field::FrameTlbcIndex), None, "past a u32");
    assert_eq!(layout.field(Field::CodeTlbc), None);
    assert_eq!(layout.constant(Constant::OwnerEntry), 3);

    let refused = PythonRuntime {
        layout: Err("CPython 3.16.0 is not supported".into()),
        ..runtime
    };
    assert_eq!(sealed(Some(&refused)), Some(refused));
    assert_eq!(sealed(None), None);
}

#[test]
fn validation_rejects_a_python_runtime_that_names_nothing() {
    let record = |start: u64, end: u64, problem: u32| PythonRuntimeRecord {
        version: 0x030d_0df0.into(),
        flags: 0.into(),
        reserved: 0.into(),
        runtime_start: start.into(),
        runtime_end: end.into(),
        problem: problem.into(),
        offsets: [u32::MAX.into(); super::python::Field::COUNT],
        constants: [0; super::python::Constant::COUNT],
    };

    let refusal = |records: Vec<PythonRuntimeRecord>| {
        let mut builder = Builder::new(TARGET);
        builder.owned_table(records);
        builder.seal(Limits::default()).unwrap_err().to_string()
    };

    assert!(refusal(vec![record(2, 1, u32::MAX)]).contains("ends before it begins"));
    assert!(refusal(vec![record(1, 2, 7)]).contains("is no string"));
    assert!(refusal(vec![record(1, 2, u32::MAX); 2]).contains("2 Python runtimes"));
    let mut flagged = record(1, 2, u32::MAX);
    flagged.flags = 8.into();
    assert!(refusal(vec![flagged]).contains("flags it does not define"));
    let mut reserved = record(1, 2, u32::MAX);
    reserved.reserved = 1.into();
    assert!(refusal(vec![reserved]).contains("flags it does not define"));
}

/// A V8 runtime reads back as it was sealed, with its layout or the reason
/// it has none; a field or constant the layout lacks reads back absent.
#[test]
fn a_v8_runtime_reads_back_with_its_layout_or_its_problem() {
    use super::v8::{
        self, Constant, DYNAMIC_SYMBOLS, DispatchTable, EmbeddedBlob, Field, NodeRelease, V8Layout,
        V8Runtime, V8Version,
    };
    use crate::ImageAddress;

    let sealed = |runtime: Option<&V8Runtime>| {
        let mut builder = Builder::new(TARGET);
        let mut strings = StringsBuilder::default();
        v8::add_to(&mut builder, &mut strings, runtime).unwrap();
        builder.bytes(TableKind::Strings, strings.into_bytes());
        v8::runtime(&builder.seal(Limits::default()).unwrap())
    };

    let mut layout = V8Layout::default();
    layout.set_field(Field::JSFunctionSharedFunctionInfo, 32);
    layout.set_field(Field::ScriptSource, 0x1_0000_0000);
    layout.set_constant(Constant::OffFpFunction, -16);
    layout.set_constant(Constant::ScriptType, 166);
    let runtime = V8Runtime {
        version: V8Version {
            major: 14,
            minor: 6,
            build: 202,
            patch: 33,
        },
        flags: DYNAMIC_SYMBOLS,
        layout: Ok(layout),
        dispatch_table: Ok(Some(DispatchTable::Group {
            group: ImageAddress::new(0x6bb_c928),
            offset: 0x3270,
        })),
        embedded_blob: Ok(EmbeddedBlob {
            code: ImageAddress::new(0x10),
            code_size: ImageAddress::new(0x18),
            data: ImageAddress::new(0x20),
            data_size: ImageAddress::new(0x28),
        }),
        node: Ok(NodeRelease {
            major: 26,
            minor: 12,
            patch: 1,
        }),
    };
    let read = sealed(Some(&runtime)).unwrap();
    assert_eq!(read, runtime);
    assert_eq!(read.version.to_string(), "14.6.202.33");
    let layout = read.layout.unwrap();
    assert_eq!(layout.field(Field::JSFunctionSharedFunctionInfo), Some(32));
    assert_eq!(layout.field(Field::ScriptSource), None, "past a u32");
    assert_eq!(layout.field(Field::JSFunctionCode), None);
    assert_eq!(layout.constant(Constant::OffFpFunction), Some(-16));
    assert_eq!(layout.constant(Constant::ScriptType), Some(166));
    assert_eq!(layout.constant(Constant::FrameTurbofan), None);

    let refused = V8Runtime {
        layout: Err("V8 12.3 has no layout in this build of microscope".into()),
        dispatch_table: Err("names no js_dispatch_table_address".into()),
        embedded_blob: Err("names no GetEmbeddedCodeRange".into()),
        node: Err("names no napi_get_node_version".into()),
        ..runtime
    };
    assert_eq!(sealed(Some(&refused)), Some(refused));
    for dispatch_table in [Ok(None), Ok(Some(DispatchTable::Isolate { offset: 0x268 }))] {
        let other = V8Runtime {
            dispatch_table,
            ..runtime.clone()
        };
        assert_eq!(sealed(Some(&other)), Some(other));
    }
    assert_eq!(sealed(None), None);
}

#[test]
fn validation_rejects_a_v8_runtime_that_names_nothing() {
    let record = |problem: u32| V8RuntimeRecord {
        major: 14.into(),
        minor: 6.into(),
        build: 0.into(),
        patch: 0.into(),
        flags: 0.into(),
        reserved: 0.into(),
        problem: problem.into(),
        dispatch_kind: 0.into(),
        dispatch_reserved: 0.into(),
        dispatch_problem: u32::MAX.into(),
        dispatch_address: 0.into(),
        dispatch_offset: 0.into(),
        blob_problem: u32::MAX.into(),
        blob: [0.into(); 4],
        node_problem: u32::MAX.into(),
        node: [0.into(); 3],
        offsets: [u32::MAX.into(); super::v8::Field::COUNT],
        constants: [i64::MIN.into(); super::v8::Constant::COUNT],
    };

    let refusal = |records: Vec<V8RuntimeRecord>| {
        let mut builder = Builder::new(TARGET);
        builder.owned_table(records);
        builder.seal(Limits::default()).unwrap_err().to_string()
    };

    assert!(refusal(vec![record(7)]).contains("is no string"));
    assert!(refusal(vec![record(u32::MAX); 2]).contains("2 V8 runtimes"));
    let mut flagged = record(u32::MAX);
    flagged.flags = 2.into();
    assert!(refusal(vec![flagged]).contains("flags it does not define"));
    let mut reserved = record(u32::MAX);
    reserved.reserved = 1.into();
    assert!(refusal(vec![reserved]).contains("flags it does not define"));
    let mut kind = record(u32::MAX);
    kind.dispatch_kind = 3.into();
    assert!(refusal(vec![kind]).contains("dispatch table of kind 3"));
    let mut stray = record(u32::MAX);
    stray.dispatch_address = 8.into();
    assert!(refusal(vec![stray]).contains("dispatch table of kind 0"));
    let mut placed = record(u32::MAX);
    placed.blob_problem = 7.into();
    assert!(refusal(vec![placed]).contains("is no string"));
    let mut both = record(u32::MAX);
    both.blob = [8.into(); 4];
    both.blob_problem = 0.into();
    assert!(refusal(vec![both]).contains("is no string"));
    let mut kind = record(u32::MAX);
    kind.dispatch_kind = 3.into();
    assert!(refusal(vec![kind]).contains("dispatch table of kind 3"));
    let mut stray = record(u32::MAX);
    stray.dispatch_address = 8.into();
    assert!(refusal(vec![stray]).contains("dispatch table of kind 0"));
    let mut placed = record(u32::MAX);
    placed.blob_problem = 7.into();
    assert!(refusal(vec![placed]).contains("is no string"));

    // A blob placed beside the reason it cannot be.
    let mut strings = StringsBuilder::default();
    let problem = strings.push("no accessor").unwrap().0;
    let mut both = record(u32::MAX);
    both.blob = [8.into(); 4];
    both.blob_problem = problem.into();
    let mut builder = Builder::new(TARGET);
    builder.owned_table(vec![both]);
    builder.bytes(TableKind::Strings, strings.into_bytes());
    let refused = builder.seal(Limits::default()).unwrap_err().to_string();
    assert!(
        refused.contains("places its embedded blob and says it cannot"),
        "{refused}"
    );

    // A Node release given beside the reason it cannot be.
    let mut strings = StringsBuilder::default();
    let problem = strings.push("no napi_get_node_version").unwrap().0;
    let mut both = record(u32::MAX);
    both.node = [24.into(), 16.into(), 0.into()];
    both.node_problem = problem.into();
    let mut builder = Builder::new(TARGET);
    builder.owned_table(vec![both]);
    builder.bytes(TableKind::Strings, strings.into_bytes());
    let refused = builder.seal(Limits::default()).unwrap_err().to_string();
    assert!(
        refused.contains("gives a Node release and says it cannot"),
        "{refused}"
    );
}
