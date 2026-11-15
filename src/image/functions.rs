use rayon::slice::ParallelSliceMut as _;
use zerocopy::little_endian::{U16, U32, U64};
use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout, Unaligned};

use super::index::{self, Interval, NameEntry};
use super::strings::{StrId, Strings, StringsBuilder};
use super::symbols::{role, role_code, valid_role};
use super::{Builder, Image, NONE, Record, SharedRecord, Span, TableKind};
use crate::{
    AddressRange, BoundaryEvidence, BreakpointEntry, CodeInstanceId, CodeInstanceInfo,
    CodeInstanceKind, CodeRole, ColumnNumber, EntryProvenance, FunctionId, FunctionInfo,
    ImageAddress, LineNumber, SourceFileId, SourceLanguage, SourceLocation, TypeId,
};

#[repr(C)]
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, FromBytes, IntoBytes, KnownLayout, Immutable, Unaligned,
)]
pub struct LocationRecord {
    pub file: U32,
    pub line: U64,
    pub column: U64,
}

impl LocationRecord {
    const NONE: Self = Self {
        file: U32::new(NONE),
        line: U64::new(0),
        column: U64::new(0),
    };

    pub(super) fn of(location: Option<&SourceLocation>) -> Self {
        location.map_or(Self::NONE, |location| Self {
            file: location.file.get().into(),
            line: location.line.get().into(),
            column: location.column.map_or(0, ColumnNumber::get).into(),
        })
    }

    pub(super) fn get(&self) -> Option<SourceLocation> {
        if self.file.get() == NONE {
            return None;
        }

        Some(SourceLocation {
            file: SourceFileId::new(self.file.get()),
            line: LineNumber::new(self.line.get())?,
            column: ColumnNumber::new(self.column.get()),
        })
    }

    pub(super) fn valid(&self, files: usize) -> bool {
        if self.file.get() == NONE {
            *self == Self::NONE
        } else {
            (self.file.get() as usize) < files && self.line.get() != 0
        }
    }
}

#[repr(C)]
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, FromBytes, IntoBytes, KnownLayout, Immutable, Unaligned,
)]
pub struct FunctionRecord {
    pub name: U32,

    pub linkage_name: U32,
    pub declaration: LocationRecord,

    pub enclosing: U32,

    pub coroutine: U32,

    pub generics: Span<GenericRecord>,

    pub instances: Span<Member>,

    pub other_language: U16,
    pub language: u8,
    pub role: u8,

    pub main_subprogram: u8,
}

impl Record for FunctionRecord {
    const KIND: TableKind = TableKind::Functions;
}

#[repr(C)]
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, FromBytes, IntoBytes, KnownLayout, Immutable, Unaligned,
)]
pub struct GenericRecord {
    pub name: U32,
    pub argument: U32,
}

impl Record for GenericRecord {
    const KIND: TableKind = TableKind::Generics;
}

#[repr(C)]
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, FromBytes, IntoBytes, KnownLayout, Immutable, Unaligned,
)]
pub struct InstanceRecord {
    pub function: U32,

    pub parent: U32,

    pub call_site: LocationRecord,

    pub ranges: Span<RangeRecord>,

    pub entries: Span<EntryRecord>,

    pub entry: U64,
    pub provenance: u8,
    pub inline: u8,
}

impl Record for InstanceRecord {
    const KIND: TableKind = TableKind::CodeInstances;
}

#[repr(C)]
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, FromBytes, IntoBytes, KnownLayout, Immutable, Unaligned,
)]
pub struct RangeRecord {
    pub start: U64,
    pub end: U64,
}

impl Record for RangeRecord {
    const KIND: TableKind = TableKind::InstanceRanges;
}

impl RangeRecord {
    const fn get(&self) -> AddressRange<ImageAddress> {
        AddressRange {
            start: ImageAddress::new(self.start.get()),
            end: ImageAddress::new(self.end.get()),
        }
    }
}

#[repr(C)]
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, FromBytes, IntoBytes, KnownLayout, Immutable, Unaligned,
)]
pub struct EntryRecord {
    pub address: U64,
    pub provenance: u8,
}

impl Record for EntryRecord {
    const KIND: TableKind = TableKind::RecommendedEntries;
}

#[repr(C)]
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, FromBytes, IntoBytes, KnownLayout, Immutable, Unaligned,
)]
pub struct StartRecord {
    pub address: U64,
    pub evidence: u8,
}

impl Record for StartRecord {
    const KIND: TableKind = TableKind::InstructionStarts;
}

#[repr(C)]
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, FromBytes, IntoBytes, KnownLayout, Immutable, Unaligned,
)]
pub struct Member {
    pub value: U32,
}

impl SharedRecord for Member {
    const NAME: &'static str = "Member";
}

const _: () = assert!(size_of::<LocationRecord>() == 20);
const _: () = assert!(size_of::<FunctionRecord>() == 57);
const _: () = assert!(size_of::<InstanceRecord>() == 54);

pub const NONE_PROVENANCE: u8 = u8::MAX;

const PROVENANCES: [EntryProvenance; 5] = [
    EntryProvenance::Explicit,
    EntryProvenance::Statement,
    EntryProvenance::AnalyzedPrologue,
    EntryProvenance::RangeStart,
    EntryProvenance::CoroutineBody,
];
const EVIDENCE: [BoundaryEvidence; 5] = [
    BoundaryEvidence::ProgramCounter,
    BoundaryEvidence::FunctionRange,
    BoundaryEvidence::CodeSymbol,
    BoundaryEvidence::SectionStart,
    BoundaryEvidence::RangeEnd,
];

#[expect(
    clippy::indexing_slicing,
    reason = "function validation rejects every entry whose provenance `PROVENANCES` lacks"
)]
fn provenance(code: u8) -> EntryProvenance {
    PROVENANCES[usize::from(code)]
}

#[expect(
    clippy::indexing_slicing,
    reason = "function validation rejects every start whose evidence `EVIDENCE` lacks"
)]
fn evidence(code: u8) -> BoundaryEvidence {
    EVIDENCE[usize::from(code)]
}

const LANGUAGES: [SourceLanguage; 15] = [
    SourceLanguage::C,
    SourceLanguage::Cpp,
    SourceLanguage::Rust,
    SourceLanguage::Go,
    SourceLanguage::Zig,
    SourceLanguage::Other(0),
    SourceLanguage::Unknown,
    SourceLanguage::Odin,
    SourceLanguage::Fortran,
    SourceLanguage::D,
    SourceLanguage::Nim,
    SourceLanguage::Ada,
    SourceLanguage::Python,
    SourceLanguage::JavaScript,
    SourceLanguage::TypeScript,
];
const OTHER_LANGUAGE: u8 = 5;

pub(super) fn code_of<T: PartialEq>(known: &[T], value: &T) -> Option<u8> {
    known
        .iter()
        .position(|candidate| candidate == value)
        .and_then(|code| u8::try_from(code).ok())
}

pub(super) fn language_code(language: SourceLanguage) -> Option<(u8, u16)> {
    Some(match language {
        SourceLanguage::Other(code) => (OTHER_LANGUAGE, code),
        language => (code_of(&LANGUAGES, &language)?, 0),
    })
}

#[expect(
    clippy::indexing_slicing,
    reason = "function, variable, and type identity validation reject every code \
              `valid_language` refuses"
)]
pub(super) fn language_of(code: u8, other: u16) -> SourceLanguage {
    debug_assert!(
        valid_language(code, other),
        "language {code} ({other}) names no language"
    );
    match code {
        OTHER_LANGUAGE => SourceLanguage::Other(other),
        code => LANGUAGES[usize::from(code)],
    }
}

pub(super) fn valid_language(code: u8, other: u16) -> bool {
    usize::from(code) < LANGUAGES.len() && (code == OTHER_LANGUAGE || other == 0)
}

fn number(count: usize) -> Result<u32, TooMany> {
    u32::try_from(count)
        .ok()
        .filter(|count| *count < NONE)
        .ok_or(TooMany)
}

fn optional(id: Option<u32>) -> U32 {
    id.unwrap_or(NONE).into()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("the functions, their names, or their instances do not fit an image")]
pub struct TooMany;

pub struct Code<'a> {
    pub instances: &'a [CodeInstanceInfo],

    pub prologue_ends: &'a [ImageAddress],

    pub instruction_starts: &'a [(ImageAddress, BoundaryEvidence)],
}

pub fn add_functions_to(
    builder: &mut Builder<'_>,
    strings: &mut StringsBuilder,
    functions: &[FunctionInfo],
    instances: &[CodeInstanceInfo],
) -> Result<(), TooMany> {
    add_functions(builder, strings, functions, instances)
}

pub fn add_code_to(builder: &mut Builder<'_>, code: &Code<'_>) -> Result<(), TooMany> {
    add_instances(builder, code.instances, code.prologue_ends)?;
    let mut starts = Vec::with_capacity(code.instruction_starts.len());
    for (address, evidence) in code.instruction_starts {
        starts.push(StartRecord {
            address: address.get().into(),
            evidence: code_of(&EVIDENCE, evidence).ok_or(TooMany)?,
        });
    }

    builder.owned_table(starts);
    Ok(())
}

const MAIN: &str = "main";

fn aliased(main_subprogram: bool, name: &[u8]) -> bool {
    main_subprogram && name != MAIN.as_bytes()
}

fn add_functions(
    builder: &mut Builder<'_>,
    strings: &mut StringsBuilder,
    functions: &[FunctionInfo],
    instances: &[CodeInstanceInfo],
) -> Result<(), TooMany> {
    let mut by_function = instances
        .iter()
        .map(|instance| (instance.function.get(), instance.id.get()))
        .collect::<Vec<_>>();
    by_function.par_sort_unstable();
    let members = by_function
        .iter()
        .map(|(_, instance)| Member {
            value: (*instance).into(),
        })
        .collect::<Vec<_>>();

    let mut records = Vec::with_capacity(functions.len());
    let mut names = Vec::with_capacity(functions.len());
    let mut generics = Vec::new();
    let mut first_instance = 0;

    for function in functions {
        let name = strings.push(&function.name).ok_or(TooMany)?;
        names.push((&*function.name, name, function.id.get()));
        if aliased(function.main_subprogram, function.name.as_bytes()) {
            let main = strings.push(MAIN).ok_or(TooMany)?;
            names.push((MAIN, main, function.id.get()));
        }

        let first_generic = number(generics.len())?;
        for (parameter, argument) in function.generics.iter() {
            generics.push(GenericRecord {
                name: strings.push(parameter).ok_or(TooMany)?.0.into(),
                argument: argument.get().into(),
            });
        }

        let instance_count = by_function
            .get(first_instance..)
            .unwrap_or_default()
            .iter()
            .take_while(|(owner, _)| *owner == function.id.get())
            .count();
        let (language, other_language) = language_code(function.language).ok_or(TooMany)?;
        records.push(FunctionRecord {
            name: name.0.into(),
            linkage_name: match &function.linkage_name {
                Some(name) => strings.push(name).ok_or(TooMany)?.0.into(),
                None => NONE.into(),
            },
            declaration: LocationRecord::of(function.declaration.as_ref()),
            enclosing: optional(function.enclosing.map(FunctionId::get)),
            coroutine: optional(function.coroutine.map(TypeId::get)),
            generics: Span::new(first_generic, number(function.generics.len())?),
            instances: Span::new(number(first_instance)?, number(instance_count)?),
            other_language: other_language.into(),
            language,
            role: role_code(function.role).ok_or(TooMany)?,
            main_subprogram: function.main_subprogram.into(),
        });
        first_instance += instance_count;
    }

    number(generics.len())?;
    builder
        .owned_table(records)
        .owned_shared(TableKind::FunctionNames, index::names(names))
        .owned_table(generics)
        .owned_shared(TableKind::FunctionInstances, members);
    Ok(())
}

fn add_instances(
    builder: &mut Builder<'_>,
    instances: &[CodeInstanceInfo],
    prologue_ends: &[ImageAddress],
) -> Result<(), TooMany> {
    let code_ranges = index::intervals(instances.iter().flat_map(|instance| {
        instance
            .ranges
            .iter()
            .map(|range| (*range, instance.id.get()))
    }));
    let entries = recommended_entries(instances, &code_ranges, prologue_ends);
    let mut instance_records = Vec::with_capacity(instances.len());
    let mut ranges = Vec::new();
    let mut entry_records = Vec::new();

    for (instance, recommended) in instances.iter().zip(&entries) {
        let (call_site, inline) = match &instance.kind {
            CodeInstanceKind::OutOfLine => (None, 0),
            CodeInstanceKind::Inline { call_site } => (call_site.as_ref(), 1),
        };

        instance_records.push(InstanceRecord {
            function: instance.function.get().into(),
            parent: optional(instance.parent.map(CodeInstanceId::get)),
            call_site: LocationRecord::of(call_site),
            ranges: Span::new(number(ranges.len())?, number(instance.ranges.len())?),
            entries: Span::new(number(entry_records.len())?, number(recommended.len())?),
            entry: instance
                .breakpoint_entry
                .map_or(0, |entry| entry.address.get())
                .into(),
            provenance: match instance.breakpoint_entry {
                Some(entry) => code_of(&PROVENANCES, &entry.provenance).ok_or(TooMany)?,
                None => NONE_PROVENANCE,
            },
            inline,
        });
        ranges.extend(instance.ranges.iter().map(|range| RangeRecord {
            start: range.start.get().into(),
            end: range.end.get().into(),
        }));
        for entry in recommended {
            entry_records.push(EntryRecord {
                address: entry.address.get().into(),
                provenance: code_of(&PROVENANCES, &entry.provenance).ok_or(TooMany)?,
            });
        }
    }

    number(ranges.len())?;
    number(entry_records.len())?;
    builder
        .owned_table(instance_records)
        .owned_table(ranges)
        .owned_shared(TableKind::CodeRanges, code_ranges)
        .owned_table(entry_records);
    Ok(())
}

fn recommended_entries(
    instances: &[CodeInstanceInfo],
    code_ranges: &[Interval],
    prologue_ends: &[ImageAddress],
) -> Vec<Vec<BreakpointEntry>> {
    let mut prologues = vec![Vec::new(); instances.len()];
    for address in prologue_ends {
        for id in index::containing(code_ranges, *address) {
            let (Some(entries), Some(instance)) =
                (prologues.get_mut(id as usize), instances.get(id as usize))
            else {
                continue;
            };

            if matches!(instance.kind, CodeInstanceKind::OutOfLine)
                && !entries
                    .iter()
                    .any(|entry: &BreakpointEntry| entry.address == *address)
            {
                entries.push(BreakpointEntry {
                    address: *address,
                    provenance: EntryProvenance::Statement,
                });
            }
        }
    }

    instances
        .iter()
        .zip(prologues)
        .map(|(instance, prologues)| match instance.breakpoint_entry {
            Some(entry) if entry.provenance == EntryProvenance::CoroutineBody => vec![entry],
            _ if !prologues.is_empty() => prologues,
            entry => entry.into_iter().collect(),
        })
        .collect()
}

#[derive(Clone, Copy)]
pub struct FunctionView<'a> {
    strings: Strings<'a>,
    functions: &'a [FunctionRecord],
    names: &'a [NameEntry],
    generics: &'a [GenericRecord],
    members: &'a [Member],
    instances: &'a [InstanceRecord],
    ranges: &'a [RangeRecord],
    code_ranges: &'a [Interval],
    entries: &'a [EntryRecord],
    starts: &'a [StartRecord],
}

impl<'a> FunctionView<'a> {
    pub fn new(image: &'a Image) -> Self {
        Self {
            strings: image.strings(),
            functions: image.table(),
            names: image.shared(TableKind::FunctionNames),
            generics: image.table(),
            members: image.shared(TableKind::FunctionInstances),
            instances: image.table(),
            ranges: image.table(),
            code_ranges: image.shared(TableKind::CodeRanges),
            entries: image.table(),
            starts: image.table(),
        }
    }

    pub fn function(self, id: FunctionId) -> Option<Function<'a>> {
        Some(Function {
            view: self,
            id,
            record: self.functions.get(id.index())?,
        })
    }

    pub fn functions(self) -> impl ExactSizeIterator<Item = Function<'a>> + DoubleEndedIterator {
        self.functions
            .iter()
            .zip(super::ids(self.functions.len()))
            .map(move |(record, id)| Function {
                view: self,
                id: FunctionId::new(id),
                record,
            })
    }

    pub fn named(self, name: &str) -> impl Iterator<Item = Function<'a>> + 'a {
        let found = |name| {
            index::named(self.strings, self.names, name)
                .filter_map(move |id| self.indexed_function(id))
        };

        let program = name == MAIN && found(name).any(Function::main_subprogram);
        found(name).filter(move |function| !program || function.main_subprogram())
    }

    pub fn called(self, name: &str) -> impl Iterator<Item = Function<'a>> + 'a {
        let mut ids = index::named(self.strings, self.names, name).collect::<Vec<_>>();
        ids.sort_unstable();
        ids.dedup();
        let name = name.to_owned();
        ids.into_iter()
            .filter_map(move |id| self.indexed_function(id))
            .filter(move |function| function.name() == name)
    }

    fn indexed_function(self, id: u32) -> Option<Function<'a>> {
        let function = self.function(FunctionId::new(id));
        debug_assert!(
            function.is_some(),
            "validation checked indexed function {id}"
        );
        function
    }

    pub fn instance(self, id: CodeInstanceId) -> Option<CodeInstance<'a>> {
        Some(CodeInstance {
            view: self,
            id,
            record: self.instances.get(id.index())?,
        })
    }

    pub fn instances(
        self,
    ) -> impl ExactSizeIterator<Item = CodeInstance<'a>> + DoubleEndedIterator {
        self.instances
            .iter()
            .zip(super::ids(self.instances.len()))
            .map(move |(record, id)| CodeInstance {
                view: self,
                id: CodeInstanceId::new(id),
                record,
            })
    }

    pub fn instances_containing(
        self,
        address: ImageAddress,
    ) -> impl Iterator<Item = CodeInstance<'a>> + 'a {
        index::containing(self.code_ranges, address).filter_map(move |id| {
            let instance = self.instance(CodeInstanceId::new(id));
            debug_assert!(
                instance.is_some(),
                "validation checked code range instance {id}"
            );
            instance
        })
    }

    pub fn instruction_starts(
        self,
        range: AddressRange<ImageAddress>,
    ) -> impl Iterator<Item = (ImageAddress, BoundaryEvidence)> + 'a {
        let first = self
            .starts
            .partition_point(|start| start.address.get() < range.start.get());
        self.starts
            .get(first..)
            .unwrap_or_default()
            .iter()
            .take_while(move |start| start.address.get() < range.end.get())
            .map(|start| {
                (
                    ImageAddress::new(start.address.get()),
                    evidence(start.evidence),
                )
            })
    }
}

#[derive(Clone, Copy)]
pub struct Function<'a> {
    view: FunctionView<'a>,
    id: FunctionId,
    record: &'a FunctionRecord,
}

impl std::fmt::Debug for Function<'_> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.info().fmt(formatter)
    }
}

impl PartialEq for Function<'_> {
    fn eq(&self, other: &Self) -> bool {
        std::ptr::eq(self.record, other.record)
    }
}

impl Eq for Function<'_> {}

impl<'a> Function<'a> {
    #[must_use]
    pub const fn id(self) -> FunctionId {
        self.id
    }

    #[must_use]
    pub fn name(self) -> &'a str {
        self.view.strings.get(StrId(self.record.name.get()))
    }

    #[must_use]
    pub fn linkage_name(self) -> Option<&'a str> {
        let name = self.record.linkage_name.get();
        (name != NONE).then(|| self.view.strings.get(StrId(name)))
    }

    #[must_use]
    pub fn declaration(self) -> Option<SourceLocation> {
        self.record.declaration.get()
    }

    #[must_use]
    pub fn language(self) -> SourceLanguage {
        language_of(self.record.language, self.record.other_language.get())
    }

    #[must_use]
    pub const fn main_subprogram(self) -> bool {
        self.record.main_subprogram == 1
    }

    #[must_use]
    pub fn role(self) -> CodeRole {
        role(self.record.role)
    }

    #[must_use]
    pub fn enclosing(self) -> Option<FunctionId> {
        let id = self.record.enclosing.get();
        (id != NONE).then(|| FunctionId::new(id))
    }

    #[must_use]
    pub fn coroutine(self) -> Option<TypeId> {
        let id = self.record.coroutine.get();
        (id != NONE).then(|| TypeId::new(id))
    }

    #[must_use]
    pub fn generics(self) -> impl ExactSizeIterator<Item = (&'a str, TypeId)> + 'a {
        let strings = self.view.strings;
        self.record
            .generics
            .rows(self.view.generics)
            .iter()
            .map(move |generic| {
                (
                    strings.get(StrId(generic.name.get())),
                    TypeId::new(generic.argument.get()),
                )
            })
    }

    pub fn instances(self) -> impl Iterator<Item = CodeInstance<'a>> + 'a {
        let view = self.view;
        self.record
            .instances
            .rows(self.view.members)
            .iter()
            .filter_map(move |member| {
                let instance = view.instance(CodeInstanceId::new(member.value.get()));
                debug_assert!(instance.is_some(), "validation checked the instance index");
                instance
            })
    }

    #[must_use]
    pub fn info(self) -> FunctionInfo {
        FunctionInfo {
            id: self.id,
            name: self.name().into(),
            linkage_name: self.linkage_name().map(Into::into),
            declaration: self.declaration(),
            language: self.language(),
            role: self.role(),
            enclosing: self.enclosing(),
            coroutine: self.coroutine(),
            generics: self
                .generics()
                .map(|(name, argument)| (name.into(), argument))
                .collect(),
            main_subprogram: self.main_subprogram(),
        }
    }
}

#[derive(Clone, Copy)]
pub struct CodeInstance<'a> {
    view: FunctionView<'a>,
    id: CodeInstanceId,
    record: &'a InstanceRecord,
}

impl std::fmt::Debug for CodeInstance<'_> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.info().fmt(formatter)
    }
}

impl PartialEq for CodeInstance<'_> {
    fn eq(&self, other: &Self) -> bool {
        std::ptr::eq(self.record, other.record)
    }
}

impl Eq for CodeInstance<'_> {}

impl<'a> CodeInstance<'a> {
    #[must_use]
    pub const fn id(self) -> CodeInstanceId {
        self.id
    }

    #[must_use]
    pub const fn function(self) -> FunctionId {
        FunctionId::new(self.record.function.get())
    }

    #[must_use]
    pub fn parent(self) -> Option<CodeInstanceId> {
        let id = self.record.parent.get();
        (id != NONE).then(|| CodeInstanceId::new(id))
    }

    #[must_use]
    pub fn kind(self) -> CodeInstanceKind {
        if self.record.inline == 0 {
            CodeInstanceKind::OutOfLine
        } else {
            CodeInstanceKind::Inline {
                call_site: self.record.call_site.get(),
            }
        }
    }

    #[must_use]
    pub const fn is_out_of_line(self) -> bool {
        self.record.inline == 0
    }

    pub fn ranges(
        self,
    ) -> impl ExactSizeIterator<Item = AddressRange<ImageAddress>> + DoubleEndedIterator + Clone + 'a
    {
        self.record
            .ranges
            .rows(self.view.ranges)
            .iter()
            .map(RangeRecord::get)
    }

    #[must_use]
    pub fn contains(self, address: ImageAddress) -> bool {
        self.ranges().any(|range| range.contains(address))
    }

    #[must_use]
    pub fn breakpoint_entry(self) -> Option<BreakpointEntry> {
        PROVENANCES
            .get(usize::from(self.record.provenance))
            .map(|provenance| BreakpointEntry {
                address: ImageAddress::new(self.record.entry.get()),
                provenance: *provenance,
            })
    }

    #[must_use]
    pub fn recommended_entries(self) -> impl ExactSizeIterator<Item = BreakpointEntry> + 'a {
        self.record
            .entries
            .rows(self.view.entries)
            .iter()
            .map(|entry| BreakpointEntry {
                address: ImageAddress::new(entry.address.get()),
                provenance: provenance(entry.provenance),
            })
    }

    #[must_use]
    pub fn info(self) -> CodeInstanceInfo {
        CodeInstanceInfo {
            id: self.id,
            function: self.function(),
            parent: self.parent(),
            kind: self.kind(),
            ranges: self.ranges().collect(),
            breakpoint_entry: self.breakpoint_entry(),
        }
    }
}

pub(super) fn validate(image: &Image) -> Result<(), String> {
    validate_functions(image)?;
    validate_instances(image)
}

fn validate_functions(image: &Image) -> Result<(), String> {
    let strings = image.strings();
    let files = image.table::<super::lines::FileRecord>().len();
    let functions = image.table::<FunctionRecord>();
    let generics = image.table::<GenericRecord>();
    let members = image.shared::<Member>(TableKind::FunctionInstances);
    let instances = image.table::<InstanceRecord>();

    let mut next_member = 0;
    let mut next_generic = 0;

    for (index, function) in functions.iter().enumerate() {
        if !strings.contains(StrId(function.name.get()))
            || (function.linkage_name.get() != NONE
                && !strings.contains(StrId(function.linkage_name.get())))
            || !function.declaration.valid(files)
            || (function.enclosing.get() != NONE
                && function.enclosing.get() as usize >= functions.len())
            || !valid_language(function.language, function.other_language.get())
            || !valid_role(function.role)
            || function.main_subprogram > 1
            || function.generics.first() != next_generic
            || !function.generics.within(generics.len())
            || function.instances.first() as usize != next_member
            || !function.instances.within(members.len())
        {
            return Err("a function is malformed".into());
        }

        next_generic += function.generics.count();
        next_member += function.instances.count() as usize;
        let own = function.instances.rows(members);
        if !own.iter().all(|member| {
            instances
                .get(member.value.get() as usize)
                .is_some_and(|instance| instance.function.get() as usize == index)
        }) || !own.is_sorted_by(|earlier, later| earlier.value.get() < later.value.get())
        {
            return Err("the instance index disagrees with the instances".into());
        }
    }

    if next_generic as usize != generics.len()
        || next_member != members.len()
        || members.len() != instances.len()
        || !generics
            .iter()
            .all(|generic| strings.contains(StrId(generic.name.get())))
    {
        return Err("the function indexes do not cover their tables".into());
    }

    let names = image.shared::<NameEntry>(TableKind::FunctionNames);
    let main = |function: &FunctionRecord| {
        aliased(
            function.main_subprogram == 1,
            strings.bytes(StrId(function.name.get())),
        )
    };

    let aliases = functions.iter().filter(|function| main(function)).count();
    if names.len() != functions.len() + aliases
        || !index::valid_names(&strings, names, functions.len())
        || !names.iter().all(|entry| {
            functions
                .get(entry.value.get() as usize)
                .is_some_and(|function| {
                    function.name == entry.name
                        || (main(function)
                            && strings.bytes(StrId(entry.name.get())) == MAIN.as_bytes())
                })
        })
    {
        return Err("the function name index disagrees with the functions".into());
    }

    Ok(())
}

fn distinct_ranges(ranges: &[RangeRecord]) -> usize {
    const IN_PLACE: usize = 16;
    let key = |range: &RangeRecord| (range.start.get(), range.end.get());
    if ranges.len() <= IN_PLACE {
        return ranges
            .iter()
            .enumerate()
            .filter(|(index, range)| {
                !ranges
                    .iter()
                    .take(*index)
                    .any(|earlier| key(earlier) == key(range))
            })
            .count();
    }

    let mut keys = ranges.iter().map(key).collect::<Vec<_>>();
    keys.sort_unstable();
    keys.dedup();
    keys.len()
}

fn validate_instances(image: &Image) -> Result<(), String> {
    let files = image.table::<super::lines::FileRecord>().len();
    let functions = image.table::<FunctionRecord>();
    let instances = image.table::<InstanceRecord>();
    let ranges = image.table::<RangeRecord>();
    let entries = image.table::<EntryRecord>();
    let mut next_range = 0;
    let mut next_entry = 0;

    for (index, instance) in instances.iter().enumerate() {
        let inline = match instance.inline {
            0 => false,
            1 => true,
            _ => return Err("an instance is malformed".into()),
        };

        if instance.function.get() as usize >= functions.len()
            || (instance.parent.get() != NONE && instance.parent.get() as usize >= index)
            || !instance.call_site.valid(files)
            || (!inline && instance.call_site != LocationRecord::NONE)
            || instance.ranges.first() != next_range
            || !instance.ranges.within(ranges.len())
            || instance.entries.first() != next_entry
            || !instance.entries.within(entries.len())
            || (instance.provenance == NONE_PROVENANCE && instance.entry.get() != 0)
            || (instance.provenance != NONE_PROVENANCE
                && usize::from(instance.provenance) >= PROVENANCES.len())
        {
            return Err("an instance is malformed".into());
        }

        next_range += instance.ranges.count();
        next_entry += instance.entries.count();
    }

    if next_range as usize != ranges.len()
        || next_entry as usize != entries.len()
        || !ranges
            .iter()
            .all(|range| range.start.get() < range.end.get())
        || !entries
            .iter()
            .all(|entry| usize::from(entry.provenance) < PROVENANCES.len())
    {
        return Err("an instance's ranges or entries are malformed".into());
    }

    let code_ranges = image.shared::<Interval>(TableKind::CodeRanges);
    let own = |instance: &InstanceRecord| instance.ranges.rows(ranges);

    if !index::valid_intervals(code_ranges, instances.len())
        || !code_ranges.iter().all(|interval| {
            instances
                .get(interval.value.get() as usize)
                .is_some_and(|instance| {
                    own(instance)
                        .iter()
                        .any(|range| range.start == interval.start && range.end == interval.end)
                })
        })
        || instances
            .iter()
            .map(|instance| distinct_ranges(own(instance)))
            .sum::<usize>()
            != code_ranges.len()
    {
        return Err("the code range index disagrees with the instances".into());
    }

    let starts = image.table::<StartRecord>();
    if !starts
        .iter()
        .all(|start| usize::from(start.evidence) < EVIDENCE.len())
        || !starts.is_sorted_by(|earlier, later| earlier.address.get() < later.address.get())
    {
        return Err("the instruction starts are malformed".into());
    }

    Ok(())
}
