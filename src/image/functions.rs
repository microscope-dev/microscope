//! Source-level functions, the code instances that place them in the
//! image, and the indexes that find them: by name, by function, and by
//! address. Each instance carries the entries a function breakpoint
//! prefers, and the image the addresses where instructions are known to
//! begin.

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

/// A source location: a file, or [`NONE`] for none, and its line and
/// column, each zero when absent.
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

/// One source-level function.
#[repr(C)]
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, FromBytes, IntoBytes, KnownLayout, Immutable, Unaligned,
)]
pub struct FunctionRecord {
    pub name: U32,

    /// The linkage name, or [`NONE`].
    pub linkage_name: U32,
    pub declaration: LocationRecord,

    /// The function whose loop this one is the body of, or [`NONE`].
    pub enclosing: U32,

    /// The type of the coroutine the function runs, or [`NONE`].
    pub coroutine: U32,

    /// The function's first type argument in [`TableKind::Generics`].
    pub generics: Span<GenericRecord>,

    /// The function's first instance in [`TableKind::FunctionInstances`].
    pub instances: Span<Member>,

    /// A language that has no code of its own, by its DWARF code.
    pub other_language: U16,
    pub language: u8,
    pub role: u8,

    /// 1 for the program's main subprogram, else 0.
    pub main_subprogram: u8,
}

impl Record for FunctionRecord {
    const KIND: TableKind = TableKind::Functions;
}

/// One type argument of a generic function, with its parameter's name.
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

/// One concrete placement of a function in the image.
#[repr(C)]
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, FromBytes, IntoBytes, KnownLayout, Immutable, Unaligned,
)]
pub struct InstanceRecord {
    pub function: U32,

    /// The nearest instance containing an inline expansion, or [`NONE`].
    pub parent: U32,

    /// An inline expansion's call, when described.
    pub call_site: LocationRecord,

    /// The instance's first range in [`TableKind::InstanceRanges`].
    pub ranges: Span<RangeRecord>,

    /// The instance's first recommended entry in
    /// [`TableKind::RecommendedEntries`].
    pub entries: Span<EntryRecord>,

    /// The preferred function breakpoint location, when `provenance` is
    /// not [`NONE_PROVENANCE`].
    pub entry: U64,
    pub provenance: u8,
    pub inline: u8,
}

impl Record for InstanceRecord {
    const KIND: TableKind = TableKind::CodeInstances;
}

/// An address range of an instance.
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

/// A breakpoint location, and how it was chosen.
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

/// An address where an instruction is known to begin, and the best
/// evidence of it.
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

/// A record's number, as one index lists them.
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

/// [`InstanceRecord::provenance`] for an instance without an entry.
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
const LANGUAGES: [SourceLanguage; 13] = [
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

/// The language [`language_code`] encodes, once validated.
pub(super) fn language_of(code: u8, other: u16) -> SourceLanguage {
    match code {
        OTHER_LANGUAGE => SourceLanguage::Other(other),
        code => LANGUAGES[usize::from(code)],
    }
}

/// Whether a language code and the code of another language are one
/// [`language_code`] makes.
pub(super) fn valid_language(code: u8, other: u16) -> bool {
    usize::from(code) < LANGUAGES.len() && (code == OTHER_LANGUAGE || other == 0)
}

/// A count or index of records, which the builder keeps below [`NONE`].
fn number(count: usize) -> Result<u32, TooMany> {
    u32::try_from(count)
        .ok()
        .filter(|count| *count < NONE)
        .ok_or(TooMany)
}

fn optional(id: Option<u32>) -> U32 {
    id.unwrap_or(NONE).into()
}

/// Why functions could not be encoded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("the functions, their names, or their instances do not fit an image")]
pub struct TooMany;

/// What [`add_code_to`] encodes.
pub struct Code<'a> {
    pub instances: &'a [CodeInstanceInfo],

    /// Where the line tables end prologues, in line-table order, which
    /// orders each instance's recommended entries.
    pub prologue_ends: &'a [ImageAddress],

    /// Where instructions are known to begin, by address, once each.
    pub instruction_starts: &'a [(ImageAddress, BoundaryEvidence)],
}

/// Adds the functions, their names, and their name index to `builder`,
/// pooling names in `strings`, which must be pooled in a fixed order.
pub fn add_functions_to(
    builder: &mut Builder<'_>,
    strings: &mut StringsBuilder,
    functions: &[FunctionInfo],
    instances: &[CodeInstanceInfo],
) -> Result<(), TooMany> {
    add_functions(builder, strings, functions, instances)
}

/// Adds the instances, their ranges and entries, and the instruction
/// starts to `builder`. None names a string, so another thread can build
/// them while strings are pooled.
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

/// The name a program's main subprogram also answers to.
const MAIN: &str = "main";

/// Whether the function index lists a function under [`MAIN`] as well as
/// its own name: it is the program's main subprogram, named otherwise.
fn aliased(main_subprogram: bool, name: &[u8]) -> bool {
    main_subprogram && name != MAIN.as_bytes()
}

fn add_functions(
    builder: &mut Builder<'_>,
    strings: &mut StringsBuilder,
    functions: &[FunctionInfo],
    instances: &[CodeInstanceInfo],
) -> Result<(), TooMany> {
    // Each function's instances, in order.
    let mut by_function = instances
        .iter()
        .map(|instance| (instance.function.get(), instance.id.get()))
        .collect::<Vec<_>>();
    by_function.sort_unstable();
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

        let instance_count =
            by_function[first_instance..].partition_point(|(owner, _)| *owner == function.id.get());
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

/// The entries a breakpoint on each instance prefers: an out-of-line
/// instance's prologue ends, or else its own entry. A coroutine's body
/// begins past its dispatch, wherever its prologue ends.
fn recommended_entries(
    instances: &[CodeInstanceInfo],
    code_ranges: &[Interval],
    prologue_ends: &[ImageAddress],
) -> Vec<Vec<BreakpointEntry>> {
    let mut prologues = vec![Vec::new(); instances.len()];
    for address in prologue_ends {
        for id in index::containing(code_ranges, *address) {
            let entries = &mut prologues[id as usize];
            if matches!(instances[id as usize].kind, CodeInstanceKind::OutOfLine)
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

/// The functions and instances of a validated image.
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

    /// The functions named `name`, in identifier order.
    pub fn named(self, name: &str) -> impl Iterator<Item = Function<'a>> + 'a {
        let found = |name| {
            index::named(self.strings, self.names, name)
                .filter_map(move |id| self.indexed_function(id))
        };

        // `main` names a program's main subprogram rather than the C
        // `main` its runtime calls it from.
        let program = name == MAIN && found(name).any(Function::main_subprogram);
        found(name).filter(move |function| !program || function.main_subprogram())
    }

    /// The functions whose own name is `name`, in identifier order: unlike
    /// [`Self::named`], `main` names only functions called that.
    pub fn called(self, name: &str) -> impl Iterator<Item = Function<'a>> + 'a {
        let mut ids = index::named(self.strings, self.names, name).collect::<Vec<_>>();
        ids.sort_unstable();
        ids.dedup();
        let name = name.to_owned();
        ids.into_iter()
            .filter_map(move |id| self.indexed_function(id))
            .filter(move |function| function.name() == name)
    }

    /// The function a validated index names by `id`.
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

    /// The instances whose code contains `address`, latest start first.
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

    /// The known instruction starts within `range`, in order.
    pub fn instruction_starts(
        self,
        range: AddressRange<ImageAddress>,
    ) -> impl Iterator<Item = (ImageAddress, BoundaryEvidence)> + 'a {
        let first = self
            .starts
            .partition_point(|start| start.address.get() < range.start.get());
        self.starts[first..]
            .iter()
            .take_while(move |start| start.address.get() < range.end.get())
            .map(|start| {
                (
                    ImageAddress::new(start.address.get()),
                    EVIDENCE[usize::from(start.evidence)],
                )
            })
    }
}

/// One source-level function of a validated image.
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

    /// The source-level function name.
    #[must_use]
    pub fn name(self) -> &'a str {
        self.view.strings.get(StrId(self.record.name.get()))
    }

    /// The linker-visible function name, when known.
    #[must_use]
    pub fn linkage_name(self) -> Option<&'a str> {
        let name = self.record.linkage_name.get();
        (name != NONE).then(|| self.view.strings.get(StrId(name)))
    }

    /// The function's declaration location, when known.
    #[must_use]
    pub fn declaration(self) -> Option<SourceLocation> {
        self.record.declaration.get()
    }

    /// The language of the unit that defines the function.
    #[must_use]
    pub fn language(self) -> SourceLanguage {
        language_of(self.record.language, self.record.other_language.get())
    }

    /// Whether the function is the program's main subprogram, as
    /// [`FunctionInfo::main_subprogram`] describes.
    #[must_use]
    pub const fn main_subprogram(self) -> bool {
        self.record.main_subprogram == 1
    }

    /// What the function is to unwinding and stepping.
    #[must_use]
    pub fn role(self) -> CodeRole {
        role(self.record.role)
    }

    /// The function whose loop this one is the body of, as
    /// [`FunctionInfo::enclosing`] describes.
    #[must_use]
    pub fn enclosing(self) -> Option<FunctionId> {
        let id = self.record.enclosing.get();
        (id != NONE).then(|| FunctionId::new(id))
    }

    /// The type of the coroutine the function runs, as
    /// [`FunctionInfo::coroutine`] describes.
    #[must_use]
    pub fn coroutine(self) -> Option<TypeId> {
        let id = self.record.coroutine.get();
        (id != NONE).then(|| TypeId::new(id))
    }

    /// A generic function's type arguments, each with its parameter's
    /// name.
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

    /// The function's concrete instances, in order.
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

    /// The function as a record of its own.
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

/// One concrete placement of a function in a validated image.
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

    /// The source-level function the instance places.
    #[must_use]
    pub const fn function(self) -> FunctionId {
        FunctionId::new(self.record.function.get())
    }

    /// The nearest containing instance, for an inline expansion.
    #[must_use]
    pub fn parent(self) -> Option<CodeInstanceId> {
        let id = self.record.parent.get();
        (id != NONE).then(|| CodeInstanceId::new(id))
    }

    /// Whether the instance is physical or inlined.
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

    /// Whether the instance is a physical, independently callable body.
    #[must_use]
    pub const fn is_out_of_line(self) -> bool {
        self.record.inline == 0
    }

    /// Every image-address range the instance occupies.
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

    /// Whether the instance contains an image address.
    #[must_use]
    pub fn contains(self, address: ImageAddress) -> bool {
        self.ranges().any(|range| range.contains(address))
    }

    /// The preferred location for a function breakpoint, when one exists.
    #[must_use]
    pub fn breakpoint_entry(self) -> Option<BreakpointEntry> {
        PROVENANCES
            .get(usize::from(self.record.provenance))
            .map(|provenance| BreakpointEntry {
                address: ImageAddress::new(self.record.entry.get()),
                provenance: *provenance,
            })
    }

    /// Where a function breakpoint stops in the instance: an out-of-line
    /// instance's every prologue end, or otherwise its breakpoint entry.
    #[must_use]
    pub fn recommended_entries(self) -> impl ExactSizeIterator<Item = BreakpointEntry> + 'a {
        self.record
            .entries
            .rows(self.view.entries)
            .iter()
            .map(|entry| BreakpointEntry {
                address: ImageAddress::new(entry.address.get()),
                provenance: PROVENANCES[usize::from(entry.provenance)],
            })
    }

    /// The instance as a record of its own.
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

/// Checks the function and instance tables and their indexes.
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

    // Each function's instances follow the last one's, so together they
    // list every member once.
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
            let function = &functions[entry.value.get() as usize];
            function.name == entry.name
                || (main(function) && strings.bytes(StrId(entry.name.get())) == MAIN.as_bytes())
        })
    {
        return Err("the function name index disagrees with the functions".into());
    }

    Ok(())
}

/// How many distinct ranges `ranges` holds. An instance has a few, which
/// are compared in place; a long list is sorted instead, so that no image
/// makes this quadratic.
fn distinct_ranges(ranges: &[RangeRecord]) -> usize {
    const IN_PLACE: usize = 16;
    let key = |range: &RangeRecord| (range.start.get(), range.end.get());
    if ranges.len() <= IN_PLACE {
        return ranges
            .iter()
            .enumerate()
            .filter(|(index, range)| {
                !ranges[..*index]
                    .iter()
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

    // Each entry is one of its instance's ranges, and each range has one.
    let code_ranges = image.shared::<Interval>(TableKind::CodeRanges);
    let own = |instance: &InstanceRecord| instance.ranges.rows(ranges);

    // The index is strictly ordered, so it names each (range, instance)
    // once, and each of its intervals is one of its instance's ranges.
    // Then it holds every instance's every range exactly when it has as
    // many intervals as instances have distinct ranges: counting them is
    // linear, where finding each range in the index searched it once per
    // range, most of the time a cached image took to validate.
    if !index::valid_intervals(code_ranges, instances.len())
        || !code_ranges.iter().all(|interval| {
            own(&instances[interval.value.get() as usize])
                .iter()
                .any(|range| range.start == interval.start && range.end == interval.end)
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
