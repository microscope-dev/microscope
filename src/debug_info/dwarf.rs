use std::borrow::Cow;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use foldhash::{HashMap, HashMapExt};
use gimli::{
    BaseAddresses, CfaRule, ColumnType, DebugFrame, DwarfSections, EhFrame, Encoding, EndianSlice,
    EvaluationResult, Location, RegisterRule, RunTimeEndian, UnwindContext, UnwindExpression,
    UnwindSection, Value,
};

use object::{Object, ObjectSection, ObjectSegment, ObjectSymbol};
use rayon::prelude::*;

use super::separate::Supplementary;
use super::{DebugInfo, UnwindInfo};
use crate::image::lines::{Files, LineTables, Row, StatementsByAddress};
use crate::model::{Binding, ModuleMetadata};
use crate::unwind::{DwarfRegister, FramePointerRule, MemoryReader, RegisterFile, UnwindStep};
use crate::{
    AddressRange, Architecture, BreakpointEntry, ByteOrder, CodeInstanceId, CodeInstanceInfo,
    CodeInstanceKind, ColumnNumber, EmbeddedSymbolTable, EntryProvenance, Error, FunctionId,
    FunctionInfo, ImageAddress, LineNumber, ModuleImage, PointerWidth, Result, SourceLanguage,
    SourceLocation, TargetDescription, UnwindTermination, VirtualAddress,
};

#[derive(Debug, thiserror::Error)]
enum DwarfError {
    #[error("failed to read debug information: {0}")]
    Io(#[from] std::io::Error),

    #[error("malformed ELF file: {0}")]
    Object(#[from] object::Error),

    #[error(transparent)]
    Section(#[from] super::compressed::SectionError),

    #[error("not an ELF file")]
    NotElf,

    #[error(
        "the file is truncated: its ELF headers describe {described} bytes, but it has {length}"
    )]
    Truncated { described: u64, length: u64 },

    #[error(
        "it is a relocatable object file, whose DWARF has relocations only a linker applies; \
         debug the executable or shared library it is linked into"
    )]
    Relocatable,

    #[error("it is a core dump, not a program; open it as a core dump")]
    CoreDump,

    #[error("malformed DWARF: {0}")]
    Dwarf(#[from] gimli::Error),

    #[error("its code is for {}, which this debugger does not support", architecture_name(*.0))]
    UnsupportedArchitecture(object::Architecture),

    #[error("{}", in_context(.error, .context))]
    Context { error: Box<Self>, context: String },

    #[error("DWARF references a supplementary file that was not read")]
    UnsupportedSupplementaryReference,

    #[error("DWARF reference {0:#x} is outside every unit of the supplementary file")]
    SupplementaryReferenceOutsideUnits(usize),

    #[error("{0}")]
    Supplementary(String),

    #[error(transparent)]
    LineTables(#[from] crate::image::lines::TooManyRows),

    #[error("DWARF entry depth cannot be represented")]
    InvalidEntryDepth,

    #[error("DWARF code range is reversed")]
    InvalidRange,

    #[error("DWARF debug-info reference {0:#x} is outside every loaded unit")]
    ReferenceOutsideUnits(usize),

    #[error("unsupported DWARF reference form")]
    UnsupportedReferenceForm,

    #[error("DWARF type signature {0:#018x} has no loaded definition")]
    TypeSignatureMissing(u64),

    #[error("DWARF type signature {0:#018x} has multiple definitions")]
    DuplicateTypeSignature(u64),

    #[error("DWARF reference targets an unsupported DIE at unit {unit}, offset {offset:#x}")]
    ReferencedFunctionMissing { unit: usize, offset: usize },

    #[error("DWARF reference cycle")]
    ReferenceCycle,

    #[error("internal error loading DWARF: {0}")]
    Internal(&'static str),

    #[error("malformed variable type metadata: {0}")]
    MalformedVariable(Arc<str>),

    #[error(
        "the debug information needs more than its load budget of {limit} bytes: {what} asked \
         for {requested} more"
    )]
    Budget {
        what: &'static str,
        limit: u64,
        requested: u64,
    },

    #[error(transparent)]
    Binding(crate::image::facts::Unbound),

    #[error("the debug information has more {0} than an image can hold")]
    TooMany(&'static str),

    #[error("the debug information does not fit an image: {0}")]
    Seal(Box<dyn std::error::Error + Send + Sync>),
}

impl DwarfError {
    fn within(self, context: impl FnOnce() -> String) -> Self {
        match self {
            Self::Context { .. } | Self::Budget { .. } => self,
            error => Self::Context {
                error: Box::new(error),
                context: context(),
            },
        }
    }
}

fn in_context(error: &DwarfError, context: &str) -> String {
    match error {
        DwarfError::Dwarf(error) => format!("malformed DWARF in {context}: {error}"),
        error => format!("{error}, in {context}"),
    }
}

fn architecture_name(architecture: object::Architecture) -> String {
    match architecture {
        object::Architecture::I386 => "32-bit x86".to_owned(),
        object::Architecture::X86_64_X32 => "the x32 ABI".to_owned(),
        object::Architecture::Arm => "32-bit Arm".to_owned(),
        object::Architecture::Aarch64 => "AArch64".to_owned(),
        object::Architecture::Riscv64 => "64-bit RISC-V".to_owned(),
        other => format!("{other:?}"),
    }
}

fn describe_unit(unit: &gimli::Unit<Reader<'_>>) -> String {
    let at = describe_unit_offset(&unit.header);
    unit.name.map_or_else(
        || format!("the unit at {at}"),
        |name| format!("the unit for {} at {at}", text(name)),
    )
}

fn describe_unit_offset(header: &gimli::UnitHeader<Reader<'_>>) -> String {
    format!("{}+{:#x}", header.section().name(), header.offset().0)
}

fn parse_object(data: &[u8]) -> std::result::Result<object::File<'_>, DwarfError> {
    if !data.starts_with(&object::elf::ELFMAG) {
        return Err(DwarfError::NotElf);
    }

    object::File::parse(data).map_err(|error| {
        let length = data.len() as u64;
        match elf_headers_end(data) {
            Some(described) if described > length => DwarfError::Truncated { described, length },
            _ => DwarfError::Object(error),
        }
    })
}

fn elf_headers_end(data: &[u8]) -> Option<u64> {
    let little = match data.get(5)? {
        1 => true,
        2 => false,
        _ => return None,
    };

    let (header, phoff, shoff, word, phentsize) = match data.get(4)? {
        1 => (52_usize, 0x1c, 0x20, 4, 0x2a),
        2 => (64, 0x20, 0x28, 8, 0x36),
        _ => return None,
    };

    if data.len() < header {
        return Some(header as u64);
    }

    let field = |at: usize, size: usize| -> Option<u64> {
        let bytes = data.get(at..at + size)?;
        let push = |value: u64, byte: &u8| value << 8 | u64::from(*byte);
        Some(if little {
            bytes.iter().rev().fold(0, push)
        } else {
            bytes.iter().fold(0, push)
        })
    };

    let table = |offset: usize, entry: usize| -> Option<u64> {
        let count = field(entry + 2, 2)?;
        if count == 0 {
            return Some(0);
        }

        field(offset, word)?.checked_add(field(entry, 2)?.checked_mul(count)?)
    };

    let program_headers = table(phoff, phentsize)?;
    let section_headers = table(shoff, phentsize + 4)?;
    Some((header as u64).max(program_headers).max(section_headers))
}

fn check_object(object: &object::File<'_>) -> std::result::Result<(), DwarfError> {
    match object.kind() {
        object::ObjectKind::Relocatable if has_debug_relocations(object) => {
            return Err(DwarfError::Relocatable);
        }
        object::ObjectKind::Core => return Err(DwarfError::CoreDump),
        _ => {}
    }

    target_description(object).map(drop)
}

fn has_debug_relocations(object: &object::File<'_>) -> bool {
    object.sections().any(|section| {
        section.name().is_ok_and(|name| name.starts_with(".debug_"))
            && section.relocations().next().is_some()
    })
}

type Reader<'data> = EndianSlice<'data, RunTimeEndian>;
type TypeSignatures = HashMap<gimli::DebugTypeSignature, DieKey>;

struct UnitCatalog<'data> {
    units: Units<'data>,
    type_signatures: TypeSignatures,
    code: CodeRanges,
}

#[expect(
    clippy::struct_field_names,
    reason = "the units are what the catalog lists"
)]
struct Units<'data> {
    units: Vec<gimli::Unit<Reader<'data>>>,

    starts: Vec<(usize, usize)>,

    supplementary_starts: Vec<(usize, usize)>,

    first_supplementary: usize,

    supplementary_programs: Vec<Option<Arc<LineProgram<'data>>>>,

    inherited_languages: Vec<Option<gimli::DwLang>>,
}

pub(super) struct DieWalk<'a, 'data> {
    raw: gimli::EntriesRaw<'a, Reader<'data>>,
    entry: gimli::DebuggingInformationEntry<Reader<'data>>,

    pending: Option<(&'a gimli::Abbreviation, WalkedDie)>,
}

#[derive(Debug, Clone, Copy)]
pub(super) struct WalkedDie {
    pub(super) depth: isize,
    pub(super) offset: gimli::UnitOffset,
    pub(super) tag: gimli::DwTag,
}

impl<'a, 'data> DieWalk<'a, 'data> {
    pub(super) fn new(unit: &'a gimli::Unit<Reader<'data>>) -> gimli::Result<Self> {
        Ok(Self {
            raw: unit.entries_raw(None)?,
            entry: gimli::DebuggingInformationEntry::null(),
            pending: None,
        })
    }

    pub(super) fn next(&mut self) -> gimli::Result<Option<WalkedDie>> {
        if let Some((abbreviation, _)) = self.pending.take() {
            self.raw.skip_attributes(abbreviation.attributes())?;
        }

        while !self.raw.is_empty() {
            let depth = self.raw.next_depth();
            let offset = self.raw.next_offset();
            if let Some(abbreviation) = self.raw.read_abbreviation()? {
                let die = WalkedDie {
                    depth,
                    offset,
                    tag: abbreviation.tag(),
                };

                self.pending = Some((abbreviation, die));
                return Ok(Some(die));
            }
        }

        Ok(None)
    }

    pub(super) fn decode(
        &mut self,
    ) -> gimli::Result<&gimli::DebuggingInformationEntry<Reader<'data>>> {
        let Some((abbreviation, die)) = self.pending.take() else {
            return Err(gimli::Error::NoEntryAtGivenOffset(
                self.entry.offset.0 as u64,
            ));
        };

        self.entry.tag = abbreviation.tag();
        self.entry.has_children = abbreviation.has_children();
        self.entry.offset = die.offset;
        self.entry.depth = die.depth;
        self.raw
            .read_attributes(abbreviation.attributes(), &mut self.entry.attrs)?;
        Ok(&self.entry)
    }
}

type LineProgram<'data> = gimli::IncompleteLineProgram<Reader<'data>>;

type ProgramKey<'data> = (usize, u8, Option<&'data [u8]>, Option<&'data [u8]>);

type Programs<'data> = std::sync::Mutex<HashMap<ProgramKey<'data>, Arc<LineProgram<'data>>>>;

fn shared_program<'data>(
    dwarf: &gimli::Dwarf<Reader<'data>>,
    unit: &gimli::Unit<Reader<'data>>,
    offset: gimli::DebugLineOffset,
    programs: &Programs<'data>,
) -> gimli::Result<Arc<LineProgram<'data>>> {
    let address_size = unit.header.address_size();
    let key = (
        offset.0,
        address_size,
        unit.comp_dir.map(|path| path.slice()),
        unit.name.map(|name| name.slice()),
    );
    let cached = programs
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(&key)
        .cloned();
    if let Some(program) = cached {
        return Ok(program);
    }

    let program = Arc::new(dwarf.debug_line.program(
        offset,
        address_size,
        unit.comp_dir,
        unit.name,
    )?);
    Ok(programs
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .entry(key)
        .or_insert(program)
        .clone())
}

fn supplementary_unit<'data>(
    dwarf: &gimli::Dwarf<Reader<'data>>,
    header: gimli::UnitHeader<Reader<'data>>,
    programs: &Programs<'data>,
) -> gimli::Result<(gimli::Unit<Reader<'data>>, Option<Arc<LineProgram<'data>>>)> {
    let abbreviations = dwarf.abbreviations(&header)?;
    let mut unit = gimli::Unit {
        abbreviations,
        name: None,
        comp_dir: None,
        low_pc: 0,
        str_offsets_base: gimli::DebugStrOffsetsBase::default_for_encoding_and_file(
            header.encoding(),
            dwarf.file_type,
        ),
        addr_base: gimli::DebugAddrBase(0),
        loclists_base: gimli::DebugLocListsBase::default_for_encoding_and_file(
            header.encoding(),
            dwarf.file_type,
        ),
        rnglists_base: gimli::DebugRngListsBase::default_for_encoding_and_file(
            header.encoding(),
            dwarf.file_type,
        ),
        line_program: None,
        dwo_id: match header.type_() {
            gimli::UnitType::Skeleton(dwo_id) | gimli::UnitType::SplitCompilation(dwo_id) => {
                Some(dwo_id)
            }
            _ => None,
        },
        header,
    };

    let mut name = None;
    let mut comp_dir = None;
    let mut line_program_offset = None;
    let mut low_pc = None;
    let mut entries = unit.header.entries(&unit.abbreviations);
    entries.next_dfs()?;
    let root = entries.current().ok_or(gimli::Error::MissingUnitDie)?;
    for attribute in root.attrs() {
        match (attribute.name(), attribute.value()) {
            (gimli::DW_AT_name, value) => name = Some(value),
            (gimli::DW_AT_comp_dir, value) => comp_dir = Some(value),
            (gimli::DW_AT_low_pc, value) => low_pc = Some(value),
            (gimli::DW_AT_stmt_list, gimli::AttributeValue::DebugLineRef(offset)) => {
                line_program_offset = Some(offset);
            }
            (gimli::DW_AT_str_offsets_base, gimli::AttributeValue::DebugStrOffsetsBase(base)) => {
                unit.str_offsets_base = base;
            }
            (
                gimli::DW_AT_addr_base | gimli::DW_AT_GNU_addr_base,
                gimli::AttributeValue::DebugAddrBase(base),
            ) => unit.addr_base = base,
            (gimli::DW_AT_loclists_base, gimli::AttributeValue::DebugLocListsBase(base)) => {
                unit.loclists_base = base;
            }
            (
                gimli::DW_AT_rnglists_base | gimli::DW_AT_GNU_ranges_base,
                gimli::AttributeValue::DebugRngListsBase(base),
            ) => unit.rnglists_base = base,
            (gimli::DW_AT_GNU_dwo_id, gimli::AttributeValue::DwoId(dwo_id))
                if unit.dwo_id.is_none() =>
            {
                unit.dwo_id = Some(dwo_id);
            }
            _ => {}
        }
    }

    unit.name = name.and_then(|value| dwarf.attr_string(&unit, value).ok());
    unit.comp_dir = comp_dir.and_then(|value| dwarf.attr_string(&unit, value).ok());
    let shared = line_program_offset
        .map(|offset| shared_program(dwarf, &unit, offset, programs))
        .transpose()?;

    if let Some(value) = low_pc
        && let Some(address) = dwarf.attr_address(&unit, value)?
    {
        unit.low_pc = address;
    }

    Ok((unit, shared))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Outward {
    Own(gimli::DebugInfoOffset),
    Supplementary(gimli::DebugInfoOffset),
}

impl<'data> Units<'data> {
    #[cfg(test)]
    fn new(units: Vec<gimli::Unit<Reader<'data>>>) -> Self {
        Self::catalog(units, 0)
    }

    fn load(
        dwarf: &DwarfSet<'data>,
        mut units: Vec<gimli::Unit<Reader<'data>>>,
    ) -> std::result::Result<Self, DwarfError> {
        let partial = units.iter().any(is_partial_unit);
        let Some(supplementary) = dwarf.sup() else {
            if !partial {
                let count = units.len();
                return Ok(Self::catalog(units, count));
            }

            let outward = first_error(units.par_iter().map(outward_references).collect())?;
            let count = units.len();
            let mut this = Self::catalog(units, count);
            this.inherit(&outward);
            return Ok(this);
        };

        let mut headers = Vec::new();
        let mut unit_headers = supplementary.units();
        while let Some(header) = unit_headers.next()? {
            headers.push(header);
        }

        let header_at = |offset: gimli::DebugInfoOffset| {
            let after = headers.partition_point(|header| {
                header
                    .debug_info_offset()
                    .is_some_and(|start| start.0 <= offset.0)
            });
            let index = after.checked_sub(1)?;
            let header = *headers.get(index)?;
            let start = header.debug_info_offset()?.0;
            (offset.0 - start < header.length_including_self()).then_some((index, header))
        };

        let mut outward = first_error(units.par_iter().map(outward_references).collect())?;
        let mut reached = vec![false; headers.len()];
        let mut wave = Vec::new();
        let mut reach = |references: &[Outward], within_supplementary: bool, wave: &mut Vec<_>| {
            for reference in references {
                let offset = match *reference {
                    Outward::Supplementary(offset) => offset,
                    Outward::Own(offset) if within_supplementary => offset,
                    Outward::Own(_) => continue,
                };

                if let Some((index, header)) = header_at(offset)
                    && reached
                        .get_mut(index)
                        .is_some_and(|seen| !std::mem::replace(seen, true))
                {
                    wave.push((index, header));
                }
            }
        };

        for references in &outward {
            reach(references, false, &mut wave);
        }

        let mut supplementary_units = Vec::new();
        let programs = std::sync::Mutex::new(HashMap::new());

        while !wave.is_empty() {
            let loaded = first_error(
                std::mem::take(&mut wave)
                    .into_par_iter()
                    .map(|(index, header)| {
                        let (unit, program) = supplementary_unit(supplementary, header, &programs)?;
                        let references = outward_references(&unit)?;
                        Ok::<_, DwarfError>((index, unit, program, references))
                    })
                    .collect(),
            )?;
            for (index, unit, program, references) in loaded {
                reach(&references, true, &mut wave);
                supplementary_units.push((index, unit, program, references));
            }
        }

        supplementary_units.sort_unstable_by_key(|(index, ..)| *index);
        let first_supplementary = units.len();
        let mut supplementary_programs = Vec::with_capacity(supplementary_units.len());

        for (_, unit, program, references) in supplementary_units {
            units.push(unit);
            supplementary_programs.push(program);

            outward.push(
                references
                    .into_iter()
                    .filter_map(|reference| match reference {
                        Outward::Own(offset) => Some(Outward::Supplementary(offset)),
                        Outward::Supplementary(_) => None,
                    })
                    .collect(),
            );
        }

        let mut this = Self::catalog(units, first_supplementary);
        this.supplementary_programs = supplementary_programs;
        this.inherit(&outward);
        Ok(this)
    }

    fn catalog(units: Vec<gimli::Unit<Reader<'data>>>, first_supplementary: usize) -> Self {
        let starts_of = |supplementary: bool| {
            let mut starts = units
                .iter()
                .enumerate()
                .filter(|(index, _)| (*index >= first_supplementary) == supplementary)
                .filter(|(_, unit)| unit.dwo_id.is_none())
                .filter_map(|(index, unit)| Some((unit.header.debug_info_offset()?.0, index)))
                .collect::<Vec<_>>();
            starts.sort_unstable();
            starts
        };

        Self {
            starts: starts_of(false),
            supplementary_starts: starts_of(true),
            units,
            first_supplementary,
            supplementary_programs: Vec::new(),
            inherited_languages: Vec::new(),
        }
    }

    #[expect(
        clippy::indexing_slicing,
        reason = "every index is a unit's: below `count`, the length of each table here"
    )]
    fn inherit(&mut self, outward: &[Vec<Outward>]) {
        let count = self.units.len();
        debug_assert_eq!(
            outward.len(),
            count,
            "the outward references are of {} units, but the catalog has {count}",
            outward.len(),
        );
        let mut referrers = vec![Vec::new(); count];
        for (referrer, references) in outward.iter().enumerate() {
            for &reference in references {
                let target = match reference {
                    Outward::Own(offset) => self.containing(offset, false),
                    Outward::Supplementary(offset) => self.containing(offset, true),
                };

                if let Some(target) = target
                    && target.unit != referrer
                    && referrers[target.unit].last() != Some(&referrer)
                {
                    referrers[target.unit].push(referrer);
                }
            }
        }

        let mut languages = self
            .units
            .iter()
            .map(|unit| {
                let mut entries = unit.entries();
                match entries
                    .next_dfs()
                    .ok()
                    .flatten()?
                    .attr_value(gimli::DW_AT_language)
                {
                    Some(gimli::AttributeValue::Language(language)) => Some(language),
                    _ => None,
                }
            })
            .collect::<Vec<_>>();
        let mut inherited = vec![None; count];
        let mut language_settled = languages.iter().map(Option::is_some).collect::<Vec<_>>();
        let mut directory_settled = self
            .units
            .iter()
            .map(|unit| unit.comp_dir.is_some())
            .collect::<Vec<_>>();

        loop {
            let mut round = Vec::new();
            for unit in 0..count {
                let settled =
                    |settled: &[bool]| referrers[unit].iter().any(|&referrer| settled[referrer]);
                let language = (!language_settled[unit] && settled(&language_settled)).then(|| {
                    agreed(
                        referrers[unit]
                            .iter()
                            .filter(|&&referrer| language_settled[referrer])
                            .map(|&referrer| languages[referrer]),
                    )
                });
                let directory =
                    (!directory_settled[unit] && settled(&directory_settled)).then(|| {
                        agreed(
                            referrers[unit]
                                .iter()
                                .filter(|&&referrer| directory_settled[referrer])
                                .map(|&referrer| self.units[referrer].comp_dir),
                        )
                    });
                if language.is_some() || directory.is_some() {
                    round.push((unit, language, directory));
                }
            }

            if round.is_empty() {
                break;
            }

            for (unit, language, directory) in round {
                if let Some(language) = language {
                    languages[unit] = language;
                    inherited[unit] = language;
                    language_settled[unit] = true;
                }

                if let Some(directory) = directory {
                    self.units[unit].comp_dir = directory;
                    directory_settled[unit] = true;
                }
            }
        }

        self.inherited_languages = inherited;
    }

    fn containing(&self, offset: gimli::DebugInfoOffset, supplementary: bool) -> Option<DieKey> {
        let starts = if supplementary {
            &self.supplementary_starts
        } else {
            &self.starts
        };

        let after = starts.partition_point(|(start, _)| *start <= offset.0);
        let (_, unit) = *starts.get(after.checked_sub(1)?)?;
        let within = offset.to_unit_offset(&self.units.get(unit)?.header)?;
        Some(DieKey {
            unit,
            offset: within.0,
        })
    }

    fn unit(&self, index: usize) -> std::result::Result<&gimli::Unit<Reader<'data>>, DwarfError> {
        self.units
            .get(index)
            .ok_or(DwarfError::Internal("a unit index is outside the catalog"))
    }

    fn line_program<'u>(
        &'u self,
        unit: &'u gimli::Unit<Reader<'data>>,
    ) -> Option<&'u LineProgram<'data>> {
        if let Some(program) = &unit.line_program {
            return Some(program);
        }

        let start = unit.header.debug_info_offset()?.0;
        let found = self
            .supplementary_starts
            .binary_search_by_key(&start, |(start, _)| *start)
            .ok()?;
        let (_, index) = *self.supplementary_starts.get(found)?;
        if !std::ptr::eq(self.units.get(index)?, unit) {
            return None;
        }

        self.supplementary_programs
            .get(index.checked_sub(self.first_supplementary)?)?
            .as_deref()
    }

    const fn is_supplementary(&self, index: usize) -> bool {
        index >= self.first_supplementary
    }

    #[expect(
        clippy::indexing_slicing,
        reason = "`parents` has one entry per unit, and holds only unit indexes"
    )]
    fn groups(&self) -> std::result::Result<Vec<usize>, DwarfError> {
        let outward = first_error(self.units.par_iter().map(outward_references).collect())?;
        let mut parents = (0..self.units.len()).collect::<Vec<_>>();
        let root = |parents: &mut Vec<usize>, mut unit: usize| {
            while parents[unit] != unit {
                parents[unit] = parents[parents[unit]];
                unit = parents[unit];
            }

            unit
        };

        for (unit, references) in outward.iter().enumerate() {
            for &reference in references {
                let target = match reference {
                    Outward::Own(offset) => self.containing(offset, self.is_supplementary(unit)),
                    Outward::Supplementary(offset) => self.containing(offset, true),
                };

                if let Some(target) = target {
                    let (from, to) = (root(&mut parents, unit), root(&mut parents, target.unit));
                    parents[from.max(to)] = from.min(to);
                }
            }
        }

        Ok((0..self.units.len())
            .map(|unit| root(&mut parents, unit))
            .collect())
    }

    fn inherited_language(&self, index: usize) -> Option<gimli::DwLang> {
        self.inherited_languages.get(index).copied().flatten()
    }

    fn debug_info_offset(&self, key: DieKey) -> Option<u64> {
        if self.is_supplementary(key.unit) {
            return None;
        }

        gimli::UnitOffset(key.offset)
            .to_debug_info_offset(&self.units.get(key.unit)?.header)
            .map(|offset| offset.0 as u64)
    }
}

fn is_partial_unit(unit: &gimli::Unit<Reader<'_>>) -> bool {
    matches!(unit.header.type_(), gimli::UnitType::Partial)
        || unit
            .entries_raw(None)
            .and_then(|mut entries| entries.read_abbreviation())
            .ok()
            .flatten()
            .is_some_and(|root| root.tag() == gimli::DW_TAG_partial_unit)
}

fn outward_references(
    unit: &gimli::Unit<Reader<'_>>,
) -> std::result::Result<Vec<Outward>, DwarfError> {
    let mut references = Vec::new();
    let mut entries = unit.entries();

    while let Some(entry) = entries.next_dfs()? {
        references.extend(
            entry
                .attrs()
                .iter()
                .filter_map(|attribute| match attribute.value() {
                    gimli::AttributeValue::DebugInfoRef(offset) => Some(Outward::Own(offset)),
                    gimli::AttributeValue::DebugInfoRefSup(offset) => {
                        Some(Outward::Supplementary(offset))
                    }
                    _ => None,
                }),
        );
    }

    references.sort_unstable();
    references.dedup();
    Ok(references)
}

fn unit_dwarf<'a, 'data>(
    dwarf: &'a DwarfSet<'data>,
    unit: &gimli::Unit<Reader<'data>>,
) -> &'a gimli::Dwarf<Reader<'data>> {
    use gimli::Section as _;
    if let Some(split) = dwarf.split_of(unit) {
        return split;
    }
    let dwarf = dwarf.main();
    let Some(supplementary) = dwarf.sup() else {
        return dwarf;
    };

    let section = supplementary.debug_info.reader().slice().as_ptr_range();
    let held = unit
        .header
        .range_from(unit.header.root_offset()..)
        .is_ok_and(|entries| section.contains(&entries.slice().as_ptr()));

    if held { supplementary } else { dwarf }
}

fn agreed<T: PartialEq>(values: impl Iterator<Item = Option<T>>) -> Option<T> {
    let mut agreed = None;
    for value in values.flatten() {
        match &agreed {
            None => agreed = Some(value),
            Some(known) if *known == value => {}
            Some(_) => return None,
        }
    }

    agreed
}

impl<'data> std::ops::Deref for Units<'data> {
    type Target = [gimli::Unit<Reader<'data>>];

    fn deref(&self) -> &Self::Target {
        &self.units
    }
}

struct CodeRanges(Vec<AddressRange<ImageAddress>>);

impl CodeRanges {
    fn contains(&self, range: AddressRange<ImageAddress>) -> bool {
        self.0
            .iter()
            .any(|code| code.start <= range.start && range.end <= code.end)
    }

    fn contains_address(&self, address: ImageAddress) -> bool {
        self.0.iter().any(|code| code.contains(address))
    }
}

fn die_code_ranges<'data>(
    dwarf: &DwarfSet<'data>,
    unit: &gimli::Unit<Reader<'data>>,
    entry: &gimli::DebuggingInformationEntry<Reader<'data>>,
    code: &CodeRanges,
) -> std::result::Result<Vec<AddressRange<ImageAddress>>, DwarfError> {
    let mut ranges = Vec::new();
    if entry.attr_value(gimli::DW_AT_ranges).is_some() {
        let mut list = unit_dwarf(dwarf, unit).die_ranges(unit, entry)?;
        while let Some(range) = list.next()? {
            ranges.push((range.begin, Some(range.end)));
        }
    } else if let (Some(low), Some(high)) = (
        entry.attr_value(gimli::DW_AT_low_pc),
        entry.attr_value(gimli::DW_AT_high_pc),
    ) && let Some(begin) = unit_dwarf(dwarf, unit).attr_address(unit, low)?
    {
        let end = unit_dwarf(dwarf, unit)
            .attr_address(unit, high)?
            .or_else(|| high.udata_value().and_then(|size| begin.checked_add(size)));
        ranges.push((begin, end));
    }

    Ok(ranges
        .into_iter()
        .filter_map(|(begin, end)| {
            let range = AddressRange {
                start: ImageAddress::new(begin),
                end: ImageAddress::new(end?),
            };

            (range.start < range.end && code.contains(range)).then_some(range)
        })
        .collect())
}

mod budget;
use budget::LoadLimits;
mod split;
pub(in crate::debug_info) use split::DwarfSet;
mod variables;

pub(in crate::debug_info) use variables::{PathStep, array_byte_offset};

#[cfg(feature = "fuzzing")]
pub(super) fn fuzz_expression(data: &[u8]) {
    variables::fuzz_expression(data);
}

#[cfg(all(feature = "fuzzing", feature = "tools"))]
mod fuzz;
#[cfg(all(feature = "fuzzing", feature = "tools"))]
pub(super) use fuzz::fuzz_load;

struct DwarfUnwindInfo {
    tables: Arc<crate::image::Image>,

    go: std::sync::OnceLock<Option<super::gopclntab::GoUnwind>>,

    rows: std::sync::Mutex<HashMap<(bool, u64), Arc<FrameRow>>>,
}

pub fn load(
    path: &Path,
    image_id: crate::ModuleImageId,
    search: &super::DebugFileSearch,
) -> Result<DebugInfo> {
    let _load = crate::span!("load", "{}", path.display());
    let phase = crate::span!("read");
    let (data, _) = crate::image::backing::read_input(path)?;
    crate::count!("input_bytes", data.len());
    drop(phase);

    load_bytes_on_pool(
        path,
        &data,
        image_id,
        search,
        crate::cache::current(),
        false,
    )
}

pub fn load_bytes(
    path: &Path,
    data: &[u8],
    image_id: crate::ModuleImageId,
    search: &super::DebugFileSearch,
) -> Result<DebugInfo> {
    let _load = crate::span!("load", "{}", path.display());
    load_bytes_on_pool(path, data, image_id, search, crate::cache::current(), false)
}

pub fn load_profile_bytes(
    path: &Path,
    data: &[u8],
    image_id: crate::ModuleImageId,
    search: &super::DebugFileSearch,
) -> Result<(DebugInfo, bool)> {
    let _load = crate::span!("load", "{}", path.display());
    let object = parse_object(data).map_err(Error::debug_info)?;
    let runtime = super::cpython::carries_runtime(&object)
        || super::cpython::is_asyncio(&object)
        || super::v8::carries_runtime(&object)
        || !matches!(super::gopclntab::load(&object), Ok(None));
    if runtime {
        return load_bytes(path, data, image_id, search).map(|info| (info, false));
    }
    let cache = crate::cache::current();
    alone(path, data, LoadKind::Profile, cache, || {
        crate::pool::install(|| {
            load_debug_info_kind(
                path,
                data,
                image_id,
                search,
                LoadLimits::default(),
                cache,
                LoadKind::Profile,
            )
        })
    })
    .map_err(Error::debug_info)?
    .map(|(info, _)| (info, true))
    .map_err(Error::debug_info)
}

pub fn load_transient_bytes(
    path: &Path,
    data: &[u8],
    image_id: crate::ModuleImageId,
) -> Result<DebugInfo> {
    let _load = crate::span!("load", "{}", path.display());
    let search = super::DebugFileSearch::default();
    load_bytes_on_pool(path, data, image_id, &search, None, true)
}

fn load_bytes_on_pool(
    path: &Path,
    data: &[u8],
    image_id: crate::ModuleImageId,
    search: &super::DebugFileSearch,
    cache: Option<&crate::cache::ImageCache>,
    transient: bool,
) -> Result<DebugInfo> {
    let load = || load_debug_info(path, data, image_id, search, LoadLimits::default(), cache);
    if transient {
        crate::pool::install_small(load)
    } else {
        alone(path, data, LoadKind::Full, cache, || {
            crate::pool::install(load)
        })
    }
    .map_err(Error::debug_info)?
    .map(|(info, _)| info)
    .map_err(|error| match error {
        DwarfError::Object(_)
        | DwarfError::NotElf
        | DwarfError::Truncated { .. }
        | DwarfError::Relocatable
        | DwarfError::CoreDump
        | DwarfError::UnsupportedArchitecture(_) => {
            Error::UnsupportedFile(error.to_string().into())
        }
        error => Error::debug_info(error),
    })
}

fn alone<T>(
    path: &Path,
    data: &[u8],
    kind: LoadKind,
    cache: Option<&crate::cache::ImageCache>,
    load: impl FnOnce() -> T,
) -> T {
    match cache {
        Some(cache) if rayon::current_thread_index().is_none() => {
            cache.load_alone(path, data.len() as u64, kind == LoadKind::Full, load)
        }
        _ => load(),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum CacheOutcome {
    Off,

    Hit,

    Miss,

    Corrupt,
}

fn load_debug_info(
    path: &Path,
    data: &[u8],
    image_id: crate::ModuleImageId,
    search: &super::DebugFileSearch,
    limits: LoadLimits,
    cache: Option<&crate::cache::ImageCache>,
) -> std::result::Result<(DebugInfo, CacheOutcome), DwarfError> {
    load_debug_info_kind(path, data, image_id, search, limits, cache, LoadKind::Full)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum LoadKind {
    Full,
    Profile,
}

fn load_debug_info_kind(
    path: &Path,
    data: &[u8],
    image_id: crate::ModuleImageId,
    search: &super::DebugFileSearch,
    limits: LoadLimits,
    cache: Option<&crate::cache::ImageCache>,
    kind: LoadKind,
) -> std::result::Result<(DebugInfo, CacheOutcome), DwarfError> {
    let object = parse_object(data)?;
    check_object(&object)?;
    let phase = crate::span!("separate_debug_file");
    let super::separate::Search {
        found: separate,
        rejected,
    } = search.find(path, &object);

    let supplementary = if kind == LoadKind::Profile {
        Supplementary::None
    } else {
        separate.as_ref().map_or_else(
            || search.supplementary(path, &object),
            |file| {
                object::File::parse(file.data.as_slice()).map_or(Supplementary::None, |debug| {
                    search.supplementary(&file.path, &debug)
                })
            },
        )
    };
    drop(phase);
    let binding = Binding {
        path: Arc::new(path.to_owned()),
        debug_path: separate
            .as_ref()
            .or_else(|| rejected.as_ref().map(|(file, _)| file))
            .map(|file| Arc::new(file.path.clone())),
        id: image_id,
    };

    let built =
        |tables, unkept| bind(&binding, Arc::new(tables), unkept).map_err(DwarfError::Binding);
    let read = |separate: Option<&super::separate::DebugFile>, supplementary: &Supplementary| {
        let rejected = rejected
            .as_ref()
            .map(|(file, reason)| (file, Arc::<str>::from(reason.as_str())));
        read_debug_info(
            path,
            data,
            separate,
            rejected.as_ref(),
            supplementary,
            limits,
            kind,
        )
    };

    let Some(cache) = cache else {
        let read = read(separate.as_ref(), &supplementary)?;

        drop((separate, supplementary, rejected));
        return Ok((built(read.seal()?, None)?, CacheOutcome::Off));
    };

    let phase = crate::span!("cache.read");
    let mut inputs = vec![data];
    if kind == LoadKind::Profile {
        inputs.push(b"native-recording-without-unit-dwarf-v1");
    }
    inputs.extend(separate.as_ref().map(|file| file.data.as_slice()));

    inputs.extend(rejected.as_ref().map(|(file, _)| file.data.as_slice()));
    if let Supplementary::Found(file) = &supplementary {
        inputs.push(file.data.as_slice());
    }

    let dwarf_data = separate.as_ref().map_or(data, |file| file.data.as_slice());
    let split = (kind == LoadKind::Full)
        .then(|| {
            object::File::parse(dwarf_data)
                .ok()
                .and_then(|object| split::fingerprint(&object, path))
        })
        .flatten();
    inputs.extend(split.as_deref());

    let key = crate::cache::Key::of(&inputs);
    let outcome = match cache.get(key) {
        crate::cache::Lookup::Hit { image, stamp } => match bind(&binding, image, None) {
            Ok(info) => {
                crate::count!("cache_hits", 1);
                return Ok((info, CacheOutcome::Hit));
            }
            Err(error) => {
                crate::cache::report(format_args!("{key} for {}: {error}", path.display()));
                cache.discard(key, &stamp);
                CacheOutcome::Corrupt
            }
        },
        crate::cache::Lookup::Corrupt(error) => {
            crate::cache::report(format_args!("{key} for {}: {error}", path.display()));
            CacheOutcome::Corrupt
        }
        crate::cache::Lookup::Miss => CacheOutcome::Miss,
    };

    drop(phase);
    crate::count!("cache_misses", 1);
    let read = read(separate.as_ref(), &supplementary)?;
    drop((separate, supplementary, rejected));
    let tables = read.seal()?;
    let phase = crate::span!("cache.write");
    let unkept = cache
        .put(key, &tables)
        .err()
        .map(|error| crate::cache::unwritten(path, &error));

    drop(phase);
    Ok((built(tables, unkept)?, outcome))
}

fn read_debug_info(
    program: &Path,
    data: &[u8],
    separate: Option<&super::separate::DebugFile>,
    rejected: Option<&(&super::separate::DebugFile, Arc<str>)>,
    supplementary: &Supplementary,
    limits: LoadLimits,
    kind: LoadKind,
) -> std::result::Result<Read, DwarfError> {
    let none_found = || {
        rejected.map_or(Separate::None, |(file, reason)| {
            Separate::Unusable(&file.path, Arc::clone(reason))
        })
    };

    let image = |separate, supplementary, dwarf| {
        load_image(program, data, separate, supplementary, dwarf, limits)
    };

    if kind == LoadKind::Profile {
        return image(
            separate.map_or_else(none_found, Separate::Used),
            None,
            DwarfUse::Deferred,
        );
    }

    let own = |separate: Separate<'_>, supplementary| {
        load_image(
            program,
            data,
            separate.clone(),
            supplementary,
            DwarfUse::Read,
            limits,
        )
        .or_else(|error| {
            load_image(
                program,
                data,
                separate,
                None,
                DwarfUse::Unusable(error.to_string().into()),
                limits,
            )
        })
    };

    match (separate, supplementary) {
        (None, Supplementary::Missing(reason)) => image(
            none_found(),
            None,
            DwarfUse::Unusable(reason.as_str().into()),
        ),
        (None, Supplementary::None) => own(none_found(), None),
        (None, Supplementary::Found(file)) => own(none_found(), Some(file)),
        (Some(separate), Supplementary::Missing(reason)) => own(
            Separate::Unusable(&separate.path, reason.as_str().into()),
            None,
        ),
        (Some(separate), supplementary) => {
            let supplementary = match supplementary {
                Supplementary::Found(file) => Some(file),
                Supplementary::None | Supplementary::Missing(_) => None,
            };

            image(Separate::Used(separate), supplementary, DwarfUse::Read).or_else(|error| {
                own(
                    Separate::Unusable(&separate.path, error.to_string().into()),
                    None,
                )
            })
        }
    }
}

enum DwarfUse {
    Read,

    Deferred,

    Unusable(Arc<str>),
}

type Sections<'data> = DwarfSections<Cow<'data, [u8]>>;

fn load_sections<'data>(
    object: &object::File<'data>,
    limits: LoadLimits,
) -> std::result::Result<(Sections<'data>, u64), DwarfError> {
    let mut wanted = Vec::new();
    DwarfSections::load(|id| {
        wanted.push(id);
        Ok::<_, DwarfError>(())
    })?;

    let found = wanted
        .iter()
        .map(|id| object.section_by_name(id.name()))
        .collect();
    let data = super::compressed::read(found, limits.most_decompressed)?;
    let input = data.iter().map(|data| data.len() as u64).sum();
    let mut loaded = wanted.into_iter().zip(data).collect::<HashMap<_, _>>();
    let sections = DwarfSections::load(|id| {
        Ok::<_, DwarfError>(loaded.remove(&id).unwrap_or(Cow::Borrowed(&[])))
    })?;

    Ok((sections, input))
}

#[derive(Clone)]
enum Separate<'a> {
    None,
    Used(&'a super::separate::DebugFile),

    Unusable(&'a Path, Arc<str>),
}

struct Read {
    target: TargetDescription,
    address_range: AddressRange<ImageAddress>,
    metadata: ModuleMetadata,
}

impl Read {
    fn seal(self) -> std::result::Result<crate::image::Image, DwarfError> {
        let _phase = crate::span!("image_indexes");
        crate::model::seal(self.target, self.address_range, self.metadata).map_err(DwarfError::Seal)
    }
}

#[expect(
    clippy::too_many_lines,
    reason = "one loader assembles every table of an image from its sources"
)]
fn load_image(
    program: &Path,
    data: &[u8],
    separate: Separate<'_>,
    supplementary: Option<&super::separate::DebugFile>,
    dwarf_use: DwarfUse,
    limits: LoadLimits,
) -> std::result::Result<Read, DwarfError> {
    let object = parse_object(data)?;
    let target = target_description(&object)?;
    let debug_object = match &separate {
        Separate::Used(file) => Some(object::File::parse(file.data.as_slice())?),
        Separate::None | Separate::Unusable(..) => None,
    };

    let dwarf_object = debug_object.as_ref().unwrap_or(&object);
    let supplementary_object = supplementary
        .map(|file| object::File::parse(file.data.as_slice()))
        .transpose()?;

    let phase = crate::span!("sections");
    let (sections, mut input) = match dwarf_use {
        DwarfUse::Read => load_sections(dwarf_object, limits)?,
        DwarfUse::Deferred | DwarfUse::Unusable(_) => (
            DwarfSections::load(|_| Ok::<_, DwarfError>(Cow::Borrowed(&[][..])))?,
            0,
        ),
    };

    let supplementary_sections = supplementary_object
        .as_ref()
        .map(|supplementary| {
            if supplementary.is_little_endian() != object.is_little_endian() {
                return Err(DwarfError::Supplementary(
                    "its dwz supplementary file has another byte order".to_owned(),
                ));
            }

            load_sections(supplementary, limits)
        })
        .transpose()?
        .map(|(sections, bytes)| {
            input += bytes;
            sections
        });
    crate::count!("debug_bytes", input);

    let endian = if object.is_little_endian() {
        RunTimeEndian::Little
    } else {
        RunTimeEndian::Big
    };

    let mut dwarf = sections.borrow_with_sup(supplementary_sections.as_ref(), |section| {
        EndianSlice::new(section, endian)
    });

    dwarf.populate_abbreviations_cache(gimli::AbbreviationsCacheStrategy::Duplicates);
    if let Some(supplementary) = dwarf.sup.as_mut().and_then(Arc::get_mut) {
        supplementary.populate_abbreviations_cache(gimli::AbbreviationsCacheStrategy::Duplicates);
    }
    let mut files = Files::default();
    let mut headers = Vec::new();

    let mut next = 0;
    let mut unit_headers = dwarf.units();

    while let Some(header) = unit_headers.next().map_err(|error| {
        DwarfError::from(error).within(|| format!("the unit header at .debug_info+{next:#x}"))
    })? {
        next = unit_end(&header);
        headers.push(header);
    }

    let mut next = 0;
    let mut type_unit_headers = dwarf.type_units();

    while let Some(header) = type_unit_headers.next().map_err(|error| {
        DwarfError::from(error).within(|| format!("the unit header at .debug_types+{next:#x}"))
    })? {
        next = unit_end(&header);
        headers.push(header);
    }

    let units = first_error(
        headers
            .into_par_iter()
            .map(|header| {
                dwarf.unit(header).map_err(|error| {
                    DwarfError::from(error).within(|| unit_failure(&dwarf, &header))
                })
            })
            .collect(),
    )?;

    let dwo_files: Vec<(usize, Vec<u8>)> = split::read_files(&dwarf, &units, program)
        .into_iter()
        .filter_map(|(index, found)| found.ok().map(|(_, bytes)| (index, bytes)))
        .collect();
    let dwo_bytes: u64 = dwo_files.iter().map(|(_, bytes)| bytes.len() as u64).sum();
    crate::count!("dwo_bytes", dwo_bytes);
    input += dwo_bytes;
    let dwo_sections: Vec<(usize, Sections<'_>)> = dwo_files
        .iter()
        .filter_map(|(index, bytes)| Some((*index, split::sections(bytes)?)))
        .collect();
    let (dwarf, units) = split::with_split_units(dwarf, units, &dwo_sections, endian);
    let units = Units::load(&dwarf, units)?;
    let catalog = UnitCatalog {
        type_signatures: type_signature_index(&units)?,
        units,
        code: CodeRanges(super::elf::executable_ranges(&object)),
    };

    crate::count!("units", catalog.units.len());
    drop(phase);

    let (mut go_table, mut runtime_function_table) = match super::gopclntab::load(&object) {
        Ok(Some(table)) => (Some(Arc::new(table)), EmbeddedSymbolTable::Loaded),
        Ok(None) => (None, EmbeddedSymbolTable::Absent),
        Err(error) => (None, unusable_table(&error)),
    };

    let (function_metadata, unit_lines) = rayon::join(
        || {
            let _phase = crate::span!("functions");
            load_function_metadata(&dwarf, &catalog, go_table.as_deref(), &mut files)
        },
        || {
            let _phase = crate::span!("lines");
            catalog
                .units
                .par_iter()
                .filter(|unit| !is_type_unit(unit))
                .map(|unit| {
                    let mut files = Files::default();
                    let mut tables = LineTables::default();
                    let program = catalog.units.line_program(unit);
                    load_lines(
                        &dwarf,
                        unit,
                        program,
                        &catalog.code,
                        &mut files,
                        &mut tables,
                    )
                    .map_err(|error| {
                        error.within(|| format!("the line program of {}", describe_unit(unit)))
                    })?;
                    Ok::<_, DwarfError>((files, tables))
                })
                .collect::<Vec<_>>()
        },
    );
    let mut function_metadata = function_metadata?;
    let unit_lines = first_error(unit_lines)?;
    let phase = crate::span!("lines.merge");

    let unit_lines = unit_lines
        .into_iter()
        .map(|(unit_files, tables)| {
            let ids = unit_files
                .paths()
                .iter()
                .map(|path| files.intern(path.clone()))
                .collect::<Vec<_>>();
            (tables, ids)
        })
        .collect::<Vec<_>>();
    let mut line_tables = LineTables::concat(&unit_lines)?;
    crate::model::free(unit_lines, line_tables.rows.len());

    crate::count!("line_rows", line_tables.rows.len());
    drop(phase);

    let phase = crate::span!("go_completion");
    let code = |address: u64, length: usize| {
        code_bytes(&object, address, address.checked_add(length as u64)?)
    };

    if let Some(table) = &go_table
        && let Err(error) = super::gopclntab::complete_metadata(
            table,
            code,
            &mut super::gopclntab::Catalog {
                functions: &mut function_metadata.functions,
                code_instances: &mut function_metadata.code_instances,
                lines: &mut line_tables,
                source_file: &mut |path| files.intern(path),
            },
        )
    {
        runtime_function_table = unusable_table(&error);
        go_table = None;
    }

    for (index, function) in function_metadata.functions.iter().enumerate() {
        debug_assert_eq!(
            function.id.index(),
            index,
            "function {:?} is not at its identifier's position",
            function.id
        );
    }

    for (index, instance) in function_metadata.code_instances.iter().enumerate() {
        debug_assert_eq!(
            instance.id.index(),
            index,
            "code instance {:?} is not at its identifier's position",
            instance.id
        );
    }

    drop(phase);
    let phase = crate::span!("prologues");

    let statements = line_tables.statements_by_address();
    super::roles::link_loop_bodies(&mut function_metadata.functions);
    refine_proved_prologue_entries(
        &object,
        target,
        &statements,
        &mut function_metadata.code_instances,
    );

    drop(phase);

    let (variables, unwind_and_symbols) = rayon::join(
        || {
            let _phase = crate::span!("variables");
            let read = |files: &mut Files, locals| {
                variables::load_variable_info(
                    &dwarf,
                    &catalog,
                    target,
                    crate::ModuleImageId::new(0),
                    variables::CodeMetadata {
                        instance_ids: &function_metadata.instance_ids,
                    },
                    files,
                    locals,
                    limits.budget(input),
                    limits.walk_ahead,
                )
            };

            let known = files.paths().len();

            match read(&mut files, variables::Locals::Read) {
                Err(error @ DwarfError::Budget { .. }) => {
                    let _phase = crate::span!("variables.without_locals");
                    files.truncate(known);
                    read(&mut files, variables::Locals::LeftOut)
                        .map(|variables| (variables, Some(error)))
                }
                read => read.map(|variables| (variables, None)),
            }
        },
        || {
            let _phase = crate::span!("unwind_and_symbols");
            let go_code = go_code_ranges(&dwarf, &catalog)?;
            let go = go_table.as_ref().map(|table| {
                let (bytes, facts) = table.source();
                crate::image::unwind::GoTableData {
                    bytes: Arc::clone(bytes),
                    facts,
                    frame_saves: super::gopclntab::frame_saves(table, code),
                }
            });
            let unwind = load_unwind_info(
                &object,
                debug_object.as_ref(),
                target,
                go_code,
                go,
                limits.most_decompressed,
            )?;
            let symbols =
                super::elf::load_symbols(&object, debug_object.as_ref(), &unwind.function_ranges());

            Ok::<_, DwarfError>((unwind, symbols))
        },
    );

    let (mut variables, locals_left_out) =
        variables.map_err(|error| error.within(|| "its variables and types".to_owned()))?;
    let locals_left_out = locals_left_out.map(|error| -> Arc<str> {
        format!("its locals and parameters were left out, as {error}").into()
    });
    let (unwind, mut symbols) = unwind_and_symbols?;

    for (instance, generics) in std::mem::take(&mut variables.function_generics) {
        if let Some(function) = function_metadata
            .code_instances
            .get(instance.index())
            .and_then(|instance| {
                function_metadata
                    .functions
                    .get_mut(instance.function.index())
            })
        {
            function.generics = generics;
        }
    }

    let phase = crate::span!("resume_points");
    let coroutines = std::mem::take(&mut variables.coroutines);

    for (instance, ty) in &variables.coroutine_bodies {
        if let Some(Ok(_)) = coroutines.get(ty)
            && let Some(function) = function_metadata
                .code_instances
                .get(instance.index())
                .and_then(|instance| {
                    function_metadata
                        .functions
                        .get_mut(instance.function.index())
                })
        {
            function.coroutine = Some(*ty);
        }
    }

    link_bodies_by_name(
        &mut function_metadata.functions,
        &function_metadata.body_futures,
        &variables.types,
        &coroutines,
    );

    #[cfg(target_arch = "x86_64")]
    let resume_points = if target.architecture == Architecture::X86_64 {
        decode_resume_points(
            &object,
            &statements,
            &function_metadata.functions,
            &mut function_metadata.code_instances,
            &coroutines,
        )
    } else {
        Vec::new()
    };

    #[cfg(not(target_arch = "x86_64"))]
    let resume_points = Vec::new();
    drop(statements);

    #[cfg(target_arch = "x86_64")]
    let held = variables::coroutine::held_ranges(
        &ObjectCode(&object),
        &variables.variables,
        &function_metadata.code_instances,
        &resume_points,
    );
    #[cfg(not(target_arch = "x86_64"))]
    let held = Vec::new();
    drop(phase);
    symbols.sources.runtime_function_table = runtime_function_table;
    if let Some(table) = &go_table {
        assign_go_symbol_roles(table, &mut symbols.symbols);
    }

    if super::cpython::carries_runtime(&object) {
        super::cpython::assign_roles(
            &mut function_metadata.functions,
            &mut symbols.symbols,
            super::roles::cpython_role,
        );
    }
    if super::v8::carries_runtime(&object) {
        super::cpython::assign_roles(
            &mut function_metadata.functions,
            &mut symbols.symbols,
            super::roles::v8_role,
        );
    }
    if super::cpython::is_asyncio(&object) {
        super::cpython::assign_roles(
            &mut function_metadata.functions,
            &mut symbols.symbols,
            super::roles::asyncio_role,
        );
    }

    Ok(Read {
        target,
        address_range: image_address_range(&object)?,
        metadata: ModuleMetadata {
            functions: function_metadata.functions,
            code_instances: function_metadata.code_instances,
            symbols: symbols.symbols,
            symbol_sources: symbols.sources,
            got_slots: symbols.got_slots,
            types: variables.types,
            locations: variables.locations,
            variables: variables.variables,
            calls: variables.calls,
            type_facts: variables.type_facts,
            resumes: crate::image::resumes::Resumes {
                points: resume_points,
                held,
            },
            declarations: crate::image::declarations::Declarations {
                producers: unit_producers(&dwarf, &catalog)?,
                ..variables.declarations
            },
            packages: go_packages(&dwarf, &catalog)?,
            files,
            lines: line_tables,
            unwind: Some(unwind),
            sections: super::elf::load_sections(&object),
            thread_local_storage: super::elf::has_thread_local_storage(&object),
            thread_locals: {
                let mut thread_locals = super::elf::load_thread_locals(&object);
                super::v8::add_thread_locals(&object, &mut thread_locals);
                thread_locals
            },
            python: super::cpython::runtime(&object),
            v8: super::v8::runtime(&object),
            debug_information: match dwarf_use {
                DwarfUse::Deferred => crate::DebugInformation::Incomplete {
                    reason: "unit DWARF deferred until collection ends".into(),
                },
                DwarfUse::Unusable(reason) => crate::DebugInformation::Unusable { reason },
                DwarfUse::Read => match (split_units(&dwarf, &catalog)?, &locals_left_out) {
                    (Some(split), Some(locals)) => crate::DebugInformation::Incomplete {
                        reason: format!("{locals}; and {split}").into(),
                    },
                    (Some(reason), None) => crate::DebugInformation::Incomplete { reason },
                    (None, Some(reason)) => crate::DebugInformation::Incomplete {
                        reason: Arc::clone(reason),
                    },
                    (None, None) if catalog.units.is_empty() => crate::DebugInformation::Absent,
                    (None, None) => crate::DebugInformation::Loaded,
                },
            },
            locals_left_out,
            debug_file: match separate {
                Separate::None => None,
                Separate::Used(file) => Some(crate::DebugFile::Used(Arc::new(file.path.clone()))),
                Separate::Unusable(path, reason) => Some(crate::DebugFile::Unusable {
                    path: Arc::new(path.to_path_buf()),
                    reason,
                }),
            },
            embedded_views: embedded_views(dwarf_object, limits.most_decompressed)?,
        },
    })
}

fn bind(
    binding: &Binding,
    tables: Arc<crate::image::Image>,
    unkept: Option<crate::cache::Unkept>,
) -> std::result::Result<DebugInfo, crate::image::facts::Unbound> {
    let mut image = ModuleImage::bind(binding, tables)?;
    image.unkept = unkept;
    let image = Arc::new(image);
    Ok(DebugInfo {
        unwind: Arc::new(DwarfUnwindInfo {
            tables: Arc::clone(image.tables()),
            go: std::sync::OnceLock::new(),
            rows: std::sync::Mutex::default(),
        }),
        variables: Arc::new(variables::DwarfVariableInfo::new(&image)),
        image,
    })
}

pub(super) fn no_variables(image: &ModuleImage) -> Arc<dyn super::VariableInfo> {
    Arc::new(variables::DwarfVariableInfo::new(image))
}

fn embedded_views(
    object: &object::File<'_>,
    most: u64,
) -> std::result::Result<Vec<u8>, DwarfError> {
    let section = object.section_by_name(crate::view::embedded::SECTION);
    Ok(super::compressed::read_one(section, most)?.into_owned())
}

fn unit_failure(
    dwarf: &gimli::Dwarf<Reader<'_>>,
    header: &gimli::UnitHeader<Reader<'_>>,
) -> String {
    let at = describe_unit_offset(header);
    let Ok(abbreviations) = dwarf.abbreviations(header) else {
        return format!(
            "the abbreviations at .debug_abbrev+{:#x} of the unit at {at}",
            header.debug_abbrev_offset().0
        );
    };

    let mut entries = header.entries(&abbreviations);
    let line_program = match entries.next_dfs() {
        Ok(Some(root)) => root.attr_value(gimli::DW_AT_stmt_list),
        _ => return format!("the unit at {at}"),
    };

    match line_program {
        Some(gimli::AttributeValue::DebugLineRef(offset)) => {
            format!(
                "the line program at .debug_line+{:#x} of the unit at {at}",
                offset.0
            )
        }
        _ => format!("the unit at {at}"),
    }
}

fn unit_end(header: &gimli::UnitHeader<Reader<'_>>) -> usize {
    header
        .offset()
        .0
        .saturating_add(header.length_including_self())
}

fn split_units<'data>(
    dwarf: &DwarfSet<'data>,
    catalog: &UnitCatalog<'data>,
) -> std::result::Result<Option<Arc<str>>, DwarfError> {
    let (mut compile_units, mut unread, mut first) = (0, 0, None);
    for unit in catalog.units.iter().filter(|unit| !is_type_unit(unit)) {
        compile_units += 1;
        if split::is_skeleton(unit) && dwarf.split_of(unit).is_none() {
            unread += 1;
            first.get_or_insert(unit);
        }
    }

    let Some(first) = first else {
        return Ok(None);
    };

    let file = match first.dwo_name()? {
        Some(name) => Some(text(unit_dwarf(dwarf, first).attr_string(first, name)?).into_owned()),
        None => None,
    };

    let file = |such_as| file.map_or_else(String::new, |file| format!(", {such_as}{file}"));
    let reason = if compile_units == 1 {
        format!(
            "its compile unit is described in a split DWARF file{} that was not found or could \
             not be read; its functions are known only by their symbols, without inlines, \
             variables, or types",
            file("")
        )
    } else {
        format!(
            "{unread} of its {compile_units} compile units are described in split DWARF files{} \
             that were not found or could not be read; their functions are known only by their \
             symbols, without inlines, variables, or types",
            file("such as ")
        )
    };

    Ok(Some(reason.into()))
}

fn unit_producers<'data>(
    dwarf: &DwarfSet<'data>,
    catalog: &UnitCatalog<'data>,
) -> std::result::Result<Vec<Arc<str>>, DwarfError> {
    let mut producers = Vec::<Arc<str>>::new();
    for unit in catalog.units.iter() {
        let mut entries = unit.entries();
        let Some(root) = entries.next_dfs()? else {
            continue;
        };

        if let Some(producer) = string_attribute(dwarf, unit, root, gimli::DW_AT_producer)? {
            producers.push(producer);
        }
    }

    Ok(producers)
}

const DW_AT_GO_PACKAGE_NAME: gimli::DwAt = gimli::DwAt(0x2905);

fn go_packages(
    dwarf: &DwarfSet<'_>,
    catalog: &UnitCatalog<'_>,
) -> std::result::Result<Vec<crate::model::PackageInfo>, DwarfError> {
    let mut packages = Vec::new();
    for unit in catalog.units.iter().filter(|unit| !is_type_unit(unit)) {
        let mut entries = unit.entries();
        let Some(root) = entries.next_dfs()? else {
            continue;
        };

        if let (Some(path), Some(name)) = (
            string_attribute(dwarf, unit, root, gimli::DW_AT_name)?,
            string_attribute(dwarf, unit, root, DW_AT_GO_PACKAGE_NAME)?,
        ) {
            packages.push(crate::model::PackageInfo { path, name });
        }
    }

    Ok(packages)
}

fn unusable_table(error: &super::gopclntab::PclntabError) -> EmbeddedSymbolTable {
    EmbeddedSymbolTable::Unusable {
        reason: error.to_string().into(),
    }
}

fn assign_go_symbol_roles(table: &super::gopclntab::GoTable, symbols: &mut [crate::SymbolInfo]) {
    for symbol in symbols {
        let Some(function) = table
            .function_containing(symbol.address.get())
            .filter(|function| function.entry == symbol.address.get() && function.is_go())
        else {
            continue;
        };

        if let Ok(name) = table.name(function) {
            symbol.role = super::roles::go_role(&name, Some(function.facts), false);
        }
    }
}

fn go_code_ranges<'data>(
    dwarf: &DwarfSet<'data>,
    catalog: &UnitCatalog<'data>,
) -> std::result::Result<Vec<AddressRange<ImageAddress>>, DwarfError> {
    let mut ranges = Vec::new();
    for unit in catalog.units.iter().filter(|unit| !is_type_unit(unit)) {
        let mut entries = unit.entries();
        let Some(root) = entries.next_dfs()? else {
            continue;
        };

        if matches!(
            root.attr_value(gimli::DW_AT_language),
            Some(gimli::AttributeValue::Language(gimli::DW_LANG_Go))
        ) {
            ranges.extend(die_code_ranges(dwarf, unit, root, &catalog.code)?);
        }
    }

    ranges.sort_unstable_by_key(|range| range.start);
    let mut merged: Vec<AddressRange<ImageAddress>> = Vec::with_capacity(ranges.len());
    for range in ranges {
        match merged.last_mut() {
            Some(last) if range.start <= last.end => last.end = last.end.max(range.end),
            _ => merged.push(range),
        }
    }

    Ok(merged)
}

fn first_error<T, E>(results: Vec<std::result::Result<T, E>>) -> std::result::Result<Vec<T>, E> {
    results.into_iter().collect()
}

fn type_signature_index(
    units: &[gimli::Unit<Reader<'_>>],
) -> std::result::Result<TypeSignatures, DwarfError> {
    let mut signatures = HashMap::new();
    for (unit_index, unit) in units.iter().enumerate() {
        let (gimli::UnitType::Type {
            type_signature,
            type_offset,
        }
        | gimli::UnitType::SplitType {
            type_signature,
            type_offset,
        }) = unit.header.type_()
        else {
            continue;
        };

        if signatures
            .insert(
                type_signature,
                DieKey {
                    unit: unit_index,
                    offset: type_offset.0,
                },
            )
            .is_some()
        {
            return Err(DwarfError::DuplicateTypeSignature(type_signature.0));
        }
    }

    Ok(signatures)
}

fn is_type_unit(unit: &gimli::Unit<Reader<'_>>) -> bool {
    matches!(
        unit.header.type_(),
        gimli::UnitType::Type { .. } | gimli::UnitType::SplitType { .. }
    )
}

fn image_address_range(
    object: &object::File<'_>,
) -> std::result::Result<AddressRange<ImageAddress>, DwarfError> {
    let mut start = u64::MAX;
    let mut end = 0;

    for segment in object.segments() {
        start = start.min(segment.address());
        end = end.max(
            segment
                .address()
                .checked_add(segment.size())
                .ok_or(gimli::Error::AddressOverflow)?,
        );
    }

    if start == u64::MAX
        && let Some(allocated) = super::gdb_jit::allocated_range(object)?
    {
        (start, end) = (allocated.start, allocated.end);
    }

    if start == u64::MAX {
        start = 0;
    }

    Ok(AddressRange {
        start: ImageAddress::new(start),
        end: ImageAddress::new(end),
    })
}

fn load_unwind_info(
    object: &object::File<'_>,
    debug_object: Option<&object::File<'_>>,
    target: TargetDescription,
    go_code: Vec<AddressRange<ImageAddress>>,
    go: Option<crate::image::unwind::GoTableData>,
    most: u64,
) -> std::result::Result<crate::image::unwind::Unwind, DwarfError> {
    let data_in = |object: &object::File<'_>, name| -> std::result::Result<Arc<[u8]>, DwarfError> {
        Ok(super::compressed::read_one(object.section_by_name(name), most)?.into())
    };

    let section_data = |name| data_in(object, name);

    let debug_frame = match section_data(".debug_frame")? {
        frame if frame.is_empty() => match debug_object {
            Some(debug_object) => data_in(debug_object, ".debug_frame")?,
            None => frame,
        },
        frame => frame,
    };

    let address = |name| {
        object
            .section_by_name(name)
            .map(|section| section.address())
    };

    let mut unwind = crate::image::unwind::Unwind {
        eh_frame: section_data(".eh_frame")?,
        debug_frame,
        bases: crate::image::unwind::Bases {
            eh_frame: address(".eh_frame"),
            text: address(".text"),
            got: address(".got"),
        },
        big_endian: target.byte_order == ByteOrder::Big,
        address_size: target.pointer_width.bytes(),
        go_code,
        go,
        ..Default::default()
    };

    let sections = UnwindSections::of(&unwind);
    let indexes = (
        fde_index(&sections.eh_frame(), &sections.bases),
        fde_index(&sections.debug_frame(), &sections.bases),
    );
    (unwind.eh_frame_index, unwind.debug_frame_index) = indexes;
    unwind.stub_frames = super::elf::plt_frames(object, |address| {
        [&unwind.eh_frame_index, &unwind.debug_frame_index]
            .iter()
            .all(|index| {
                crate::image::index::containing(&index.entries, address)
                    .next()
                    .is_none()
            })
    });

    if target.architecture == Architecture::X86_64 {
        let code: Vec<(u64, &[u8])> = object
            .sections()
            .filter(|section| section.kind() == object::SectionKind::Text)
            .filter_map(|section| Some((section.address(), section.data().ok()?)))
            .collect();
        let sections = UnwindSections::of(&unwind);
        let mut restored = restored_returns(&sections.eh_frame(), &sections.bases, &code);
        restored.extend(restored_returns(
            &sections.debug_frame(),
            &sections.bases,
            &code,
        ));
        unwind.restored = restored;
        let mut functions = Vec::new();
        unwind.proved_frames = proved_frames(object, debug_object, &unwind, &mut functions);
        unwind.proved_functions = functions;
    }

    Ok(unwind)
}

#[cfg(target_arch = "x86_64")]
fn proved_frames(
    object: &object::File<'_>,
    debug_object: Option<&object::File<'_>>,
    unwind: &crate::image::unwind::Unwind,
    functions: &mut Vec<AddressRange<ImageAddress>>,
) -> Vec<(AddressRange<ImageAddress>, u32)> {
    let mut builtins = super::v8::builtins(object);
    if builtins.is_empty()
        && let Some(debug_object) = debug_object
    {
        builtins = super::v8::builtins(debug_object)
            .into_iter()
            .filter_map(|(address, code)| {
                Some((
                    address,
                    code_bytes(object, address, address + code.len() as u64)?,
                ))
            })
            .collect();
    }

    let described = |address: u64| {
        [&unwind.eh_frame_index, &unwind.debug_frame_index]
            .iter()
            .any(|index| {
                crate::image::index::containing(&index.entries, ImageAddress::new(address))
                    .next()
                    .is_some()
            })
    };
    let mut frames = Vec::new();
    let mut starts: std::collections::BTreeSet<u64> =
        builtins.iter().map(|(address, _)| *address).collect();
    if !builtins.is_empty() {
        v8_frames(
            object,
            debug_object,
            builtins,
            &described,
            &mut starts,
            &mut frames,
        );
    }

    let entry = object.entry();
    if !described(entry)
        && let Some(range) = object
            .sections()
            .filter(|section| section.kind() == object::SectionKind::Text)
            .find_map(|section| {
                let code = section.data().ok()?;
                let offset = usize::try_from(entry.checked_sub(section.address())?).ok()?;
                super::x86_64::frames::entry_frame(code.get(offset..)?, entry)
            })
        && let Some(value) = super::x86_64::frames::CodeFrame::Outermost.encode()
    {
        frames.push((range, value));
        starts.insert(entry);
    }

    let scan = scanned(object, unwind);
    undescribed_frames(
        (object, debug_object),
        unwind,
        &described,
        &starts,
        scan,
        (&mut frames, functions),
    );

    let clones = clone_returns(object, unwind, &described);
    if let Some(value) = super::x86_64::frames::CodeFrame::Clone.encode() {
        frames.retain(|(range, _)| {
            clones
                .iter()
                .all(|clone| range.end <= clone.start || clone.end <= range.start)
        });
        frames.extend(clones.into_iter().map(|range| (range, value)));
    }
    frames
}

#[cfg(target_arch = "x86_64")]
fn clone_returns(
    object: &object::File<'_>,
    unwind: &crate::image::unwind::Unwind,
    described: &dyn Fn(u64) -> bool,
) -> Vec<AddressRange<ImageAddress>> {
    const LOAD: u64 = 5;

    let sections: Vec<(u64, &[u8])> = object
        .sections()
        .filter(|section| section.kind() == object::SectionKind::Text)
        .filter_map(|section| Some((section.address(), section.data().ok()?)))
        .collect();
    [&unwind.eh_frame_index, &unwind.debug_frame_index]
        .iter()
        .flat_map(|index| index.entries.iter())
        .filter_map(|entry| {
            let from = entry.end.get().checked_sub(LOAD)?;
            let (start, code) = sections
                .iter()
                .find(|(start, code)| *start <= from && from - start < code.len() as u64)?;
            let code = code.get(usize::try_from(from - start).ok()?..)?;
            super::x86_64::frames::clone_return(code, from)
        })
        .filter(|range| !described(range.start.get()))
        .collect()
}

#[cfg(target_arch = "x86_64")]
fn v8_frames(
    object: &object::File<'_>,
    debug_object: Option<&object::File<'_>>,
    builtins: Vec<(u64, &[u8])>,
    described: &dyn Fn(u64) -> bool,
    starts: &mut std::collections::BTreeSet<u64>,
    frames: &mut Vec<(AddressRange<ImageAddress>, u32)>,
) {
    for (address, code) in builtins {
        if !described(address) {
            super::x86_64::frames::runtime_frames(code, address, true, frames);
        }
    }

    let symbols = debug_object
        .filter(|_| object.symbols().next().is_none())
        .unwrap_or(object);
    let mut functions: Vec<(u64, u64)> = symbols
        .symbols()
        .filter(|symbol| symbol.kind() == object::SymbolKind::Text)
        .map(|symbol| (symbol.address(), symbol.size()))
        .collect();
    functions.sort_unstable();
    functions.dedup_by_key(|(address, _)| *address);
    for (at, &(address, size)) in functions.iter().enumerate() {
        if starts.contains(&address) || described(address) {
            continue;
        }
        let end = match size {
            0 => match functions.get(at + 1) {
                Some((next, _)) => *next,
                None => continue,
            },
            size => match address.checked_add(size) {
                Some(end) => end,
                None => continue,
            },
        };
        if let Some(code) = code_bytes(object, address, end) {
            super::x86_64::frames::runtime_frames(code, address, false, frames);
            starts.insert(address);
        }
    }
}

#[cfg(target_arch = "x86_64")]
type ProvedFrames = Vec<(AddressRange<ImageAddress>, u32)>;

#[cfg(target_arch = "x86_64")]
const SCANNED_CODE: u64 = 64 * 1024;

#[cfg(target_arch = "x86_64")]
fn scanned(object: &object::File<'_>, unwind: &crate::image::unwind::Unwind) -> bool {
    let code = object
        .sections()
        .filter(|section| section.kind() == object::SectionKind::Text)
        .map(|section| section.size())
        .fold(0, u64::saturating_add);

    if code <= SCANNED_CODE {
        return true;
    }

    let described = [&unwind.eh_frame_index, &unwind.debug_frame_index]
        .iter()
        .flat_map(|index| index.entries.iter())
        .map(|entry| entry.end.get().saturating_sub(entry.start.get()))
        .fold(0, u64::saturating_add);
    described.saturating_mul(2) < code
}

#[cfg(target_arch = "x86_64")]
fn function_entries(
    (object, debug_object): (&object::File<'_>, Option<&object::File<'_>>),
    sections: &[(u64, &[u8])],
    scan: bool,
) -> Vec<u64> {
    let mut entries: Vec<u64> = if scan {
        sections
            .par_iter()
            .flat_map_iter(|(address, code)| super::x86_64::frames::code_entries(code, *address))
            .collect()
    } else {
        Vec::new()
    };
    entries.extend(
        object
            .symbols()
            .chain(object.dynamic_symbols())
            .filter(|symbol| symbol.kind() == object::SymbolKind::Text)
            .map(|symbol| symbol.address()),
    );
    if let Some(debug_object) = debug_object {
        entries.extend(
            debug_object
                .symbols()
                .filter(|symbol| symbol.kind() == object::SymbolKind::Text)
                .map(|symbol| symbol.address()),
        );
    }
    entries.extend(
        object
            .sections()
            .filter(|section| matches!(section.name(), Ok(".init" | ".fini")))
            .map(|section| section.address()),
    );
    entries.extend(
        object
            .dynamic_relocations()
            .into_iter()
            .flatten()
            .filter_map(|(_, relocation)| {
                matches!(
                    relocation.flags(),
                    object::RelocationFlags::Elf {
                        r_type: object::elf::R_X86_64_RELATIVE
                    }
                )
                .then(|| relocation.addend().cast_unsigned())
            }),
    );
    entries.retain(|&address| {
        sections
            .iter()
            .any(|(start, code)| *start <= address && address - start < code.len() as u64)
    });
    entries
}

#[cfg(target_arch = "x86_64")]
fn undescribed_frames(
    (object, debug_object): (&object::File<'_>, Option<&object::File<'_>>),
    unwind: &crate::image::unwind::Unwind,
    described: &(dyn Fn(u64) -> bool + Sync),
    starts: &std::collections::BTreeSet<u64>,
    scan: bool,
    (frames, functions): (&mut ProvedFrames, &mut Vec<AddressRange<ImageAddress>>),
) {
    let sections: Vec<(u64, &[u8])> = object
        .sections()
        .filter(|section| section.kind() == object::SectionKind::Text)
        .filter_map(|section| Some((section.address(), section.data().ok()?)))
        .collect();
    let mut entries = function_entries((object, debug_object), &sections, scan);

    let mut bounds = entries.clone();
    bounds.extend(
        [&unwind.eh_frame_index, &unwind.debug_frame_index]
            .iter()
            .flat_map(|index| index.entries.iter())
            .map(|entry| entry.start.get()),
    );
    bounds.extend(starts.iter().copied());
    bounds.sort_unstable();
    bounds.dedup();
    entries.sort_unstable();
    entries.dedup();

    let found: Vec<((u64, u64), ProvedFrames)> = entries
        .par_iter()
        .filter(|&&address| !starts.contains(&address))
        .filter_map(|&address| {
            let (start, code) = sections
                .iter()
                .find(|(start, code)| *start <= address && address - start < code.len() as u64)?;
            let section_end = start + code.len() as u64;
            let next = bounds.partition_point(|&bound| bound <= address);
            let end = bounds
                .get(next)
                .copied()
                .unwrap_or(section_end)
                .min(section_end);
            if described(address) && (!scan || described(end.saturating_sub(1))) {
                return None;
            }
            let offset = usize::try_from(address - start).ok()?;
            let length = usize::try_from(end - address).ok()?;
            let code = code.get(offset..offset.checked_add(length)?)?;
            if let Some(frames) = super::x86_64::frames::musl_clone(code, address) {
                return Some(((address, end), frames));
            }
            let convention = code.starts_with(&super::x86_64::frames::PROLOGUE);
            let mut frames = Vec::new();
            super::x86_64::frames::runtime_frames(code, address, convention, &mut frames);
            Some(((address, end), frames))
        })
        .collect();
    functions.extend(found.iter().map(|&((start, end), _)| AddressRange {
        start: ImageAddress::new(start),
        end: ImageAddress::new(end),
    }));
    frames.extend(found.into_iter().flat_map(|(_, found)| found));
}

fn restored_returns<'data, S>(
    section: &S,
    bases: &BaseAddresses,
    code: &[(u64, &[u8])],
) -> Vec<(AddressRange<ImageAddress>, u32)>
where
    S: UnwindSection<Reader<'data>>,
{
    let bytes = |start: u64, end: u64| {
        code.iter().find_map(|&(address, data)| {
            let from = usize::try_from(start.checked_sub(address)?).ok()?;
            let to = usize::try_from(end.checked_sub(address)?).ok()?;
            data.get(from..to)
        })
    };

    let mut restored = Vec::new();
    let mut context = UnwindContext::new();
    let mut epilogues = super::x86_64::Epilogues::new();
    let mut walk = section.entries(bases);

    while let Ok(Some(entry)) = walk.next() {
        let gimli::CieOrFde::Fde(partial) = entry else {
            continue;
        };
        let Ok(fde) = partial.parse(S::cie_from_offset) else {
            continue;
        };
        let Ok(mut rows) = fde.rows(section, bases, &mut context) else {
            continue;
        };

        let mut before: Option<(u64, bool)> = None;

        while let Ok(Some(row)) = rows.next_row() {
            let (start, end) = (row.start_address(), row.end_address());
            let gone = matches!(
                row.cfa(),
                CfaRule::RegisterAndOffset { register, offset: 8 } if register.0 == crate::unwind::x86_64::RSP.number()
            );
            let (mut slotted, mut below) = (0_u32, 0_u32);
            for (register, rule) in row.registers() {
                let RegisterRule::Offset(offset) = rule else {
                    continue;
                };
                if let Some(index) = crate::unwind::x86_64::CALLEE_SAVED_REGISTERS
                    .iter()
                    .position(|saved| *saved == DwarfRegister::from(*register))
                {
                    slotted |= 1 << index;
                    if *offset == -16 {
                        below |= 1 << index;
                    }
                }
            }

            let proved = (gone && slotted != 0)
                .then(|| bytes(start, end))
                .flatten()
                .and_then(|code| {
                    if let Some(end) = epilogues.runs_to_return(code, start) {
                        return Some((end, super::x86_64::ALL_CALLEE_SAVED));
                    }

                    let (from, _) = before.filter(|(_, gone)| !gone)?;
                    let popped = super::x86_64::restored_by(bytes(from, start)?, from)?;
                    (below & popped != 0).then(|| (epilogues.keeps(code, start, popped), popped))
                });

            if let Some((end, registers)) = proved.filter(|(end, _)| *end > start) {
                let range = AddressRange {
                    start: ImageAddress::new(start),
                    end: ImageAddress::new(end),
                };
                restored.push((range, registers));
            }

            before = Some((start, gone));
        }
    }

    restored
}

struct UnwindSections<'data> {
    eh_frame: &'data [u8],
    debug_frame: &'data [u8],
    endian: RunTimeEndian,
    address_size: u8,
    bases: BaseAddresses,
}

impl<'data> UnwindSections<'data> {
    fn of(unwind: &'data crate::image::unwind::Unwind) -> Self {
        Self::new(
            &unwind.eh_frame,
            &unwind.debug_frame,
            unwind.big_endian,
            unwind.address_size,
            unwind.bases,
        )
    }

    fn in_image(view: crate::image::unwind::UnwindView<'data>) -> Self {
        Self::new(
            view.eh_frame(),
            view.debug_frame(),
            view.big_endian(),
            view.address_size(),
            crate::image::unwind::Bases {
                eh_frame: view.eh_frame_base(),
                text: view.text_base(),
                got: view.got_base(),
            },
        )
    }

    fn new(
        eh_frame: &'data [u8],
        debug_frame: &'data [u8],
        big_endian: bool,
        address_size: u8,
        bases: crate::image::unwind::Bases,
    ) -> Self {
        let mut gimli_bases = BaseAddresses::default();
        if let Some(address) = bases.eh_frame {
            gimli_bases = gimli_bases.set_eh_frame(address);
        }

        if let Some(address) = bases.text {
            gimli_bases = gimli_bases.set_text(address);
        }

        if let Some(address) = bases.got {
            gimli_bases = gimli_bases.set_got(address);
        }

        Self {
            eh_frame,
            debug_frame,
            endian: if big_endian {
                RunTimeEndian::Big
            } else {
                RunTimeEndian::Little
            },
            address_size,
            bases: gimli_bases,
        }
    }

    fn eh_frame(&self) -> EhFrame<Reader<'data>> {
        let mut section = EhFrame::new(self.eh_frame, self.endian);
        section.set_address_size(self.address_size);
        section
    }

    fn debug_frame(&self) -> DebugFrame<Reader<'data>> {
        let mut section = DebugFrame::new(self.debug_frame, self.endian);
        section.set_address_size(self.address_size);
        section
    }
}

fn fde_index<'data, S>(section: &S, bases: &BaseAddresses) -> crate::image::unwind::FdeIndex
where
    S: UnwindSection<Reader<'data>>,
{
    let mut first_error = None;
    let mut entries = Vec::new();
    let mut walk = section.entries(bases);

    loop {
        let partial = match walk.next() {
            Ok(Some(gimli::CieOrFde::Fde(partial))) => partial,
            Ok(Some(gimli::CieOrFde::Cie(_))) => continue,
            Ok(None) => break,
            Err(error) => {
                first_error.get_or_insert((crate::image::unwind::WHOLE_SECTION, error));
                break;
            }
        };

        match partial.parse(S::cie_from_offset) {
            Ok(fde) => entries.push((
                AddressRange {
                    start: ImageAddress::new(fde.initial_address()),
                    end: ImageAddress::new(fde.end_address()),
                },
                u32::try_from(fde.offset()).unwrap_or(u32::MAX),
            )),
            Err(error) => {
                first_error.get_or_insert_with(|| (partial.offset() as u64, error));
            }
        }
    }

    crate::image::unwind::FdeIndex {
        entries: crate::image::index::intervals(entries),
        first_error: first_error.map(|(offset, error)| (offset, error.to_string())),
    }
}

impl crate::image::unwind::Unwind {
    fn function_ranges(&self) -> Vec<AddressRange<ImageAddress>> {
        self.eh_frame_index
            .entries
            .iter()
            .chain(&self.debug_frame_index.entries)
            .map(|fde| AddressRange {
                start: ImageAddress::new(fde.start.get()),
                end: ImageAddress::new(fde.end.get()),
            })
            .collect()
    }
}

impl DwarfUnwindInfo {
    fn view(&self) -> crate::image::unwind::UnwindView<'_> {
        crate::image::unwind::UnwindView::new(&self.tables)
    }

    fn go(&self) -> Option<&super::gopclntab::GoUnwind> {
        self.go
            .get_or_init(|| {
                let view = self.view();
                let (bytes, facts) = view.go()?;

                let table = super::gopclntab::GoTable::reparse(bytes.into(), facts).ok()?;
                Some(super::gopclntab::GoUnwind::new(
                    Arc::new(table),
                    view.frame_saves().collect(),
                ))
            })
            .as_ref()
    }

    fn row(
        &self,
        debug: bool,
        address: ImageAddress,
        find: impl FnOnce() -> std::result::Result<FrameRow, UnwindTermination>,
    ) -> std::result::Result<Arc<FrameRow>, UnwindTermination> {
        let key = (debug, address.get());
        let rows = || {
            self.rows
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
        };
        if let Some(row) = rows().get(&key) {
            return Ok(Arc::clone(row));
        }

        let row = Arc::new(find()?);
        let mut rows = rows();
        if rows.len() < MOST_FRAME_ROWS {
            rows.insert(key, Arc::clone(&row));
        }
        drop(rows);
        Ok(row)
    }

    fn call_clobbered_registers(&self, address: ImageAddress) -> &'static [DwarfRegister] {
        if self.view().is_go_code(address) || self.go().is_some_and(|go| go.is_go(address)) {
            &crate::unwind::GO_CALL_CLOBBERED_REGISTERS
        } else {
            &crate::unwind::CALL_CLOBBERED_REGISTERS
        }
    }
}

impl UnwindInfo for DwarfUnwindInfo {
    fn cfa(
        &self,
        address: ImageAddress,
        registers: &RegisterFile,
        memory: &mut dyn MemoryReader,
    ) -> std::result::Result<VirtualAddress, UnwindTermination> {
        let view = self.view();
        let sections = UnwindSections::in_image(view);
        let result = cfa_from_section(
            &sections.eh_frame(),
            view.eh_frame_index(),
            &sections.bases,
            address,
            registers,
            memory,
        );
        if !matches!(result, Err(UnwindTermination::NoUnwindInfo { .. })) {
            return result;
        }

        let result = cfa_from_section(
            &sections.debug_frame(),
            view.debug_frame_index(),
            &sections.bases,
            address,
            registers,
            memory,
        );
        if !matches!(result, Err(UnwindTermination::NoUnwindInfo { .. })) {
            return result;
        }

        if let Some(pushed) = view.stub_pushes(address) {
            return stub_cfa(pushed, registers);
        }

        if let Some(frame) = proved_frame(view, address) {
            return super::x86_64::frames::frame_cfa(frame, registers);
        }

        self.go()
            .and_then(|go| go.cfa(address, registers))
            .unwrap_or(result)
    }

    fn unwind(
        &self,
        address: ImageAddress,
        registers: &RegisterFile,
        memory: &mut dyn MemoryReader,
    ) -> std::result::Result<UnwindStep, UnwindTermination> {
        let view = self.view();
        let sections = UnwindSections::in_image(view);
        let clobbered = self.call_clobbered_registers(address);
        let restored = || view.restored(address);
        let eh_frame = sections.eh_frame();
        let result = self
            .row(false, address, || {
                frame_row(&eh_frame, view.eh_frame_index(), &sections.bases, address)
            })
            .and_then(|row| {
                unwind_by_row(&eh_frame, &row, registers, (clobbered, &restored), memory)
            });
        let result = if matches!(result, Err(UnwindTermination::NoUnwindInfo { .. })) {
            let debug_frame = sections.debug_frame();
            self.row(true, address, || {
                frame_row(
                    &debug_frame,
                    view.debug_frame_index(),
                    &sections.bases,
                    address,
                )
            })
            .and_then(|row| {
                unwind_by_row(
                    &debug_frame,
                    &row,
                    registers,
                    (clobbered, &restored),
                    memory,
                )
            })
        } else {
            result
        };

        let result = match result {
            Err(UnwindTermination::NoUnwindInfo { .. }) => {
                view.stub_pushes(address).map_or(result, |pushed| {
                    unwind_stub(pushed, registers, clobbered, memory)
                })
            }
            result => result,
        };

        let result = match result {
            Err(UnwindTermination::NoUnwindInfo { .. }) => proved_frame(view, address)
                .map_or(result, |frame| {
                    super::x86_64::frames::frame_caller(frame, registers, memory)
                }),
            result => result,
        };

        let Some(go) = self.go() else {
            return result;
        };

        let result = match result {
            Err(UnwindTermination::NoUnwindInfo { .. }) => go
                .unwind(address, registers, clobbered, memory)
                .unwrap_or(result),
            result => result,
        };

        result.map(|mut step| {
            go.recover_frame_pointer(address, registers, &mut step, memory);
            step
        })
    }

    fn frame_pointer_rule(&self, address: ImageAddress) -> FramePointerRule {
        self.go()
            .and_then(|go| go.frame_pointer_rule(address))
            .unwrap_or_else(|| crate::unwind::frame_pointer_rule(self, address))
    }

    fn task_register(&self, address: ImageAddress) -> Option<DwarfRegister> {
        const R14: DwarfRegister = DwarfRegister::new(14);
        self.go()
            .is_some_and(|go| go.compiled(address))
            .then_some(R14)
    }
}

fn proved_frame(
    view: crate::image::unwind::UnwindView<'_>,
    address: ImageAddress,
) -> Option<super::x86_64::frames::CodeFrame> {
    view.proved_frame(address)
        .and_then(super::x86_64::frames::CodeFrame::decode)
}

fn cfa_from_section<'data, S>(
    section: &S,
    index: crate::image::unwind::FdeLookup<'_>,
    bases: &BaseAddresses,
    address: ImageAddress,
    registers: &RegisterFile,
    memory: &mut dyn MemoryReader,
) -> std::result::Result<VirtualAddress, UnwindTermination>
where
    S: UnwindSection<Reader<'data>>,
{
    let mut context = UnwindContext::new();
    let (fde, row) = unwind_row(section, index, bases, address, &mut context)?;
    cfa_from_rule(row.cfa(), registers, section, fde.cie().encoding(), memory)
}

fn unwind_row<'data, 'context, S>(
    section: &S,
    index: crate::image::unwind::FdeLookup<'_>,
    bases: &BaseAddresses,
    address: ImageAddress,
    context: &'context mut UnwindContext<usize>,
) -> std::result::Result<
    (
        gimli::FrameDescriptionEntry<Reader<'data>>,
        &'context gimli::UnwindTableRow<usize>,
    ),
    UnwindTermination,
>
where
    S: UnwindSection<Reader<'data>>,
{
    let offset = index.lookup(address.get()).map_err(|miss| match miss {
        crate::image::unwind::FdeMiss::NoEntry => UnwindTermination::NoUnwindInfo {
            address: VirtualAddress::new(address.get()),
        },
        crate::image::unwind::FdeMiss::Malformed(description) => {
            UnwindTermination::CorruptUnwindInfo {
                description: description.into(),
            }
        }
    })?;
    let fde = section
        .fde_from_offset(bases, offset.into(), S::cie_from_offset)
        .map_err(|error| cfi_error(error, address))?;
    let row = fde
        .unwind_info_for_address(section, bases, context, address.get())
        .map_err(|error| cfi_error(error, address))?;
    Ok((fde, row))
}

#[derive(Debug)]
struct FrameRow {
    cfa: CfaRule<usize>,
    rules: Box<[(gimli::Register, RegisterRule<usize>)]>,
    return_register: DwarfRegister,
    encoding: Encoding,
    signal_frame: bool,
}

const MOST_FRAME_ROWS: usize = 1 << 16;

fn frame_row<'data, S>(
    section: &S,
    index: crate::image::unwind::FdeLookup<'_>,
    bases: &BaseAddresses,
    address: ImageAddress,
) -> std::result::Result<FrameRow, UnwindTermination>
where
    S: UnwindSection<Reader<'data>>,
{
    let mut context = UnwindContext::new();
    let (fde, row) = unwind_row(section, index, bases, address, &mut context)?;
    Ok(FrameRow {
        cfa: row.cfa().clone(),
        rules: row.registers().cloned().collect(),
        return_register: DwarfRegister::from(fde.cie().return_address_register()),
        encoding: fde.cie().encoding(),
        signal_frame: fde.cie().is_signal_trampoline(),
    })
}

#[cfg(test)]
fn unwind_from_section<'data, S>(
    section: &S,
    index: crate::image::unwind::FdeLookup<'_>,
    bases: &BaseAddresses,
    address: ImageAddress,
    registers: &RegisterFile,
    (clobbered, restored): (&[DwarfRegister], u32),
    memory: &mut dyn MemoryReader,
) -> std::result::Result<UnwindStep, UnwindTermination>
where
    S: UnwindSection<Reader<'data>>,
{
    let row = frame_row(section, index, bases, address)?;
    unwind_by_row(section, &row, registers, (clobbered, &|| restored), memory)
}

fn unwind_by_row<'data, S>(
    section: &S,
    row: &FrameRow,
    registers: &RegisterFile,
    (clobbered, restored): (&[DwarfRegister], &dyn Fn() -> u32),
    memory: &mut dyn MemoryReader,
) -> std::result::Result<UnwindStep, UnwindTermination>
where
    S: UnwindSection<Reader<'data>>,
{
    let return_register = row.return_register;
    let cfa = cfa_from_rule(&row.cfa, registers, section, row.encoding, memory)?;
    let mut caller = registers.clone();

    let clobbered_bits = low_register_bits(clobbered.iter().copied());
    let described_bits = low_register_bits(
        row.rules
            .iter()
            .map(|(register, _)| DwarfRegister::from(*register)),
    );
    caller.retain(|register| {
        if register.number() < 64 {
            let bit = 1 << register.number();
            return clobbered_bits & bit == 0 || described_bits & bit != 0;
        }

        !clobbered.contains(&register)
            || row
                .rules
                .iter()
                .any(|(described, _)| DwarfRegister::from(*described) == register)
    });

    let rules = RuleContext {
        current: registers,
        cfa,
        section,
        encoding: row.encoding,
    };

    for &(register, ref rule) in &row.rules {
        let register = DwarfRegister::from(register);
        match rules.apply(&mut caller, memory, register, rule) {
            Err(UnwindTermination::MemoryReadFailed { .. }) if register != return_register => {
                let proved = crate::unwind::CALLEE_SAVED_REGISTERS
                    .iter()
                    .position(|saved| *saved == register)
                    .is_some_and(|index| restored() & (1 << index) != 0);
                if !proved || clobbered.contains(&register) {
                    caller.remove(register);
                }
            }
            result => result?,
        }
    }

    caller.set(crate::unwind::STACK_POINTER, cfa.get());

    let Some(return_address) = caller.get(return_register) else {
        return Err(UnwindTermination::Complete);
    };
    caller.set(crate::unwind::PROGRAM_COUNTER, return_address);

    Ok(UnwindStep {
        registers: caller,
        cfa,
        signal_frame: row.signal_frame,
    })
}

fn low_register_bits(registers: impl Iterator<Item = DwarfRegister>) -> u64 {
    registers
        .filter(|register| register.number() < 64)
        .fold(0, |bits, register| bits | 1 << register.number())
}

fn stub_cfa(
    pushed: u32,
    registers: &RegisterFile,
) -> std::result::Result<VirtualAddress, UnwindTermination> {
    let stack = registers
        .get(crate::unwind::STACK_POINTER)
        .ok_or_else(|| register_unavailable(crate::unwind::STACK_POINTER))?;
    let cfa = (u64::from(pushed) + 1)
        .checked_mul(8)
        .and_then(|bytes| stack.checked_add(bytes))
        .ok_or_else(|| UnwindTermination::InvalidCaller {
            description: "a PLT stub's frame ends past the address space".into(),
        })?;
    Ok(VirtualAddress::new(cfa))
}

fn unwind_stub(
    pushed: u32,
    registers: &RegisterFile,
    clobbered: &[DwarfRegister],
    memory: &mut dyn MemoryReader,
) -> std::result::Result<UnwindStep, UnwindTermination> {
    let cfa = stub_cfa(pushed, registers)?;
    let slot = VirtualAddress::new(cfa.get() - 8);
    let return_address = memory
        .read_u64(slot)
        .ok_or(UnwindTermination::MemoryReadFailed { address: slot })?;
    let mut caller = registers.clone();
    for &register in clobbered {
        caller.remove(register);
    }

    caller.set(crate::unwind::PROGRAM_COUNTER, return_address);
    caller.set(crate::unwind::STACK_POINTER, cfa.get());
    Ok(UnwindStep {
        registers: caller,
        cfa,
        signal_frame: false,
    })
}

fn cfa_from_rule<'data, S>(
    rule: &CfaRule<usize>,
    registers: &RegisterFile,
    section: &S,
    encoding: Encoding,
    memory: &mut dyn MemoryReader,
) -> std::result::Result<VirtualAddress, UnwindTermination>
where
    S: UnwindSection<Reader<'data>>,
{
    match rule {
        CfaRule::RegisterAndOffset { register, offset } => {
            let register = DwarfRegister::from(*register);
            let value = registers
                .get(register)
                .ok_or_else(|| register_unavailable(register))?;
            Ok(VirtualAddress::new(
                checked_add(value, *offset).ok_or_else(|| UnwindTermination::InvalidCaller {
                    description: "CFA arithmetic overflow".into(),
                })?,
            ))
        }
        CfaRule::Expression(expression) => {
            match evaluate_unwind_expression(
                expression, section, encoding, registers, memory, None, "CFA",
            )? {
                Evaluated::Address(address) => Ok(VirtualAddress::new(address)),
                Evaluated::Value(_) => Err(UnwindTermination::UnsupportedUnwindInfo {
                    feature: "CFA expression: non-address result".into(),
                }),
            }
        }
    }
}

const MAX_UNWIND_EXPRESSION_ITERATIONS: u32 = 10_000;

enum Evaluated {
    Address(u64),
    Value(u64),
}

fn evaluate_unwind_expression<'data, S>(
    expression: &UnwindExpression<usize>,
    section: &S,
    encoding: Encoding,
    registers: &RegisterFile,
    memory: &mut dyn MemoryReader,
    initial: Option<u64>,
    rule: &str,
) -> std::result::Result<Evaluated, UnwindTermination>
where
    S: UnwindSection<Reader<'data>>,
{
    let unsupported = |feature: &str| UnwindTermination::UnsupportedUnwindInfo {
        feature: format!("{rule} expression: {feature}").into(),
    };

    let corrupt = |error: gimli::Error| UnwindTermination::CorruptUnwindInfo {
        description: format!("{rule} expression: {error}").into(),
    };

    let expression = expression.get(section).map_err(corrupt)?;
    let mut evaluation = expression.evaluation(encoding);
    evaluation.set_max_iterations(MAX_UNWIND_EXPRESSION_ITERATIONS);
    if let Some(initial) = initial {
        evaluation.set_initial_value(initial);
    }

    let mut result = evaluation.evaluate().map_err(corrupt)?;

    loop {
        result = match result {
            EvaluationResult::Complete => break,
            EvaluationResult::RequiresRegister { register, .. } => {
                let register = DwarfRegister::from(register);
                let value = registers
                    .get(register)
                    .ok_or_else(|| register_unavailable(register))?;
                evaluation
                    .resume_with_register(Value::Generic(value))
                    .map_err(corrupt)?
            }
            EvaluationResult::RequiresMemory { space: Some(_), .. } => {
                return Err(unsupported("non-default memory address space"));
            }
            EvaluationResult::RequiresMemory { address, size, .. } => {
                if size == 0 || u32::from(size) > 8 {
                    return Err(unsupported("unsupported memory operand size"));
                }

                let address = VirtualAddress::new(address);
                let word = memory
                    .read_u64(address)
                    .ok_or(UnwindTermination::MemoryReadFailed { address })?;
                let bits = u32::from(size) * 8;
                let value = if bits == 64 {
                    word
                } else {
                    word & ((1 << bits) - 1)
                };

                evaluation
                    .resume_with_memory(Value::Generic(value))
                    .map_err(corrupt)?
            }
            EvaluationResult::RequiresFrameBase => return Err(unsupported("frame base")),
            EvaluationResult::RequiresTls(_) => return Err(unsupported("TLS")),
            EvaluationResult::RequiresCallFrameCfa => {
                return Err(unsupported("recursive CFA"));
            }
            _ => return Err(unsupported("unsupported expression operation")),
        };
    }

    let pieces = evaluation.result();
    let [piece] = pieces.as_slice() else {
        return Err(unsupported("compound location"));
    };

    match piece.location {
        Location::Address { address } => Ok(Evaluated::Address(address)),
        Location::Value {
            value: Value::Generic(value),
        } => Ok(Evaluated::Value(value)),
        _ => Err(unsupported(
            "a result that is neither an address nor a word",
        )),
    }
}

struct RuleContext<'a, S> {
    current: &'a RegisterFile,
    cfa: VirtualAddress,
    section: &'a S,
    encoding: Encoding,
}

impl<'data, S: UnwindSection<Reader<'data>>> RuleContext<'_, S> {
    fn apply(
        &self,
        caller: &mut RegisterFile,
        memory: &mut dyn MemoryReader,
        register: DwarfRegister,
        rule: &RegisterRule<usize>,
    ) -> std::result::Result<(), UnwindTermination> {
        let cfa = self.cfa;
        let value = match rule {
            RegisterRule::Undefined => {
                caller.remove(register);
                return Ok(());
            }
            RegisterRule::SameValue => self
                .current
                .get(register)
                .ok_or_else(|| register_unavailable(register))?,
            RegisterRule::Offset(offset) => {
                let address =
                    VirtualAddress::new(checked_add(cfa.get(), *offset).ok_or_else(|| {
                        UnwindTermination::InvalidCaller {
                            description: "saved-register address overflow".into(),
                        }
                    })?);
                memory
                    .read_u64(address)
                    .ok_or(UnwindTermination::MemoryReadFailed { address })?
            }
            RegisterRule::ValOffset(offset) => {
                checked_add(cfa.get(), *offset).ok_or_else(|| UnwindTermination::InvalidCaller {
                    description: "register value overflow".into(),
                })?
            }
            RegisterRule::Register(source) => self
                .current
                .get(DwarfRegister::from(*source))
                .ok_or_else(|| register_unavailable(DwarfRegister::from(*source)))?,
            RegisterRule::Constant(value) => *value,
            RegisterRule::Expression(expression) => match self.evaluate(expression, memory)? {
                Evaluated::Address(address) => {
                    let address = VirtualAddress::new(address);
                    memory
                        .read_u64(address)
                        .ok_or(UnwindTermination::MemoryReadFailed { address })?
                }
                Evaluated::Value(_) => {
                    return Err(UnwindTermination::CorruptUnwindInfo {
                        description: "register expression: a value where a saved \
                                          register's address belongs"
                            .into(),
                    });
                }
            },
            RegisterRule::ValExpression(expression) => match self.evaluate(expression, memory)? {
                Evaluated::Address(value) | Evaluated::Value(value) => value,
            },
            RegisterRule::Architectural => {
                return Err(UnwindTermination::UnsupportedUnwindInfo {
                    feature: "architectural register rule".into(),
                });
            }
        };

        caller.set(register, value);
        Ok(())
    }

    fn evaluate(
        &self,
        expression: &UnwindExpression<usize>,
        memory: &mut dyn MemoryReader,
    ) -> std::result::Result<Evaluated, UnwindTermination> {
        evaluate_unwind_expression(
            expression,
            self.section,
            self.encoding,
            self.current,
            memory,
            Some(self.cfa.get()),
            "register",
        )
    }
}

fn register_unavailable(register: DwarfRegister) -> UnwindTermination {
    UnwindTermination::RegisterUnavailable {
        register: format!("DWARF register {register}").into(),
    }
}

const fn checked_add(value: u64, offset: i64) -> Option<u64> {
    if offset < 0 {
        value.checked_sub(offset.unsigned_abs())
    } else {
        value.checked_add(offset.unsigned_abs())
    }
}

fn cfi_error(error: gimli::Error, address: ImageAddress) -> UnwindTermination {
    if error == gimli::Error::NoUnwindInfoForAddress {
        UnwindTermination::NoUnwindInfo {
            address: VirtualAddress::new(address.get()),
        }
    } else {
        UnwindTermination::CorruptUnwindInfo {
            description: error.to_string().into(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct DieKey {
    unit: usize,
    offset: usize,
}

#[derive(Clone)]
struct DieMap<V> {
    first: usize,
    units: Vec<HashMap<usize, V>>,
}

impl<V> Default for DieMap<V> {
    fn default() -> Self {
        Self {
            first: 0,
            units: Vec::new(),
        }
    }
}

impl<V> DieMap<V> {
    fn unit(&self, unit: usize) -> Option<&HashMap<usize, V>> {
        self.units.get(unit.checked_sub(self.first)?)
    }

    #[expect(
        clippy::indexing_slicing,
        reason = "the maps were just extended to hold `index`"
    )]
    fn unit_mut(&mut self, unit: usize) -> &mut HashMap<usize, V> {
        if self.units.is_empty() {
            self.first = unit;
        } else if unit < self.first {
            self.units.splice(
                0..0,
                std::iter::repeat_with(HashMap::new).take(self.first - unit),
            );
            self.first = unit;
        }

        let index = unit - self.first;
        if index >= self.units.len() {
            self.units.resize_with(index + 1, HashMap::new);
        }

        &mut self.units[index]
    }

    fn get(&self, key: &DieKey) -> Option<&V> {
        self.unit(key.unit)?.get(&key.offset)
    }

    fn contains_key(&self, key: &DieKey) -> bool {
        self.get(key).is_some()
    }

    fn insert(&mut self, key: DieKey, value: V) -> Option<V> {
        self.unit_mut(key.unit).insert(key.offset, value)
    }

    fn is_empty(&self) -> bool {
        self.units.iter().all(HashMap::is_empty)
    }

    fn remove(&mut self, key: &DieKey) -> Option<V> {
        let index = key.unit.checked_sub(self.first)?;
        self.units.get_mut(index)?.remove(&key.offset)
    }

    fn into_entries(self) -> impl Iterator<Item = (DieKey, V)> {
        let first = self.first;
        self.units
            .into_iter()
            .enumerate()
            .flat_map(move |(index, entries)| {
                entries.into_iter().map(move |(offset, value)| {
                    (
                        DieKey {
                            unit: first + index,
                            offset,
                        },
                        value,
                    )
                })
            })
    }

    fn extend(&mut self, other: Self) {
        let first = other.first;
        for (index, entries) in other.units.into_iter().enumerate() {
            if entries.is_empty() {
                continue;
            }

            let unit = self.unit_mut(first + index);
            if unit.is_empty() {
                *unit = entries;
            } else {
                unit.extend(entries);
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RawFunctionKind {
    Subprogram,
    Inline,
}

struct RawFunction {
    key: DieKey,

    language: SourceLanguage,
    kind: RawFunctionKind,
    parent: Option<DieKey>,
    abstract_origin: Option<DieKey>,
    specification: Option<DieKey>,
    name: Option<Arc<str>>,
    linkage_name: Option<Arc<str>>,

    trampoline: bool,

    main_subprogram: bool,
    declaration: Option<SourceLocation>,
    call_site: Option<SourceLocation>,

    ranges: Arc<[AddressRange<ImageAddress>]>,
    entry: Option<ImageAddress>,

    namespace: Option<Arc<str>>,

    returns: Option<DieKey>,
}

struct FunctionMetadata {
    functions: Vec<FunctionInfo>,
    code_instances: Vec<CodeInstanceInfo>,

    instance_ids: DieMap<CodeInstanceId>,

    body_futures: Vec<(FunctionId, Arc<str>)>,
}

#[expect(
    clippy::indexing_slicing,
    reason = "each definition is an index into `raw`, from `definition_of`, and each table \
              indexed by one is `raw.len()` long"
)]
fn load_function_metadata<'data>(
    dwarf: &DwarfSet<'data>,
    catalog: &UnitCatalog<'data>,
    go_table: Option<&super::gopclntab::GoTable>,
    files: &mut Files,
) -> std::result::Result<FunctionMetadata, DwarfError> {
    let phase = crate::span!("functions.collect");
    let (raw, futures) = collect_function_dies(dwarf, catalog, files)?;
    crate::count!("function_dies", raw.len());
    drop(phase);
    let _phase = crate::span!("functions.resolve");

    let definitions = first_error(
        raw.par_iter()
            .map(|function| definition_of(function.key, &raw))
            .collect(),
    )?;

    let mut concrete = vec![false; raw.len()];
    for (function, definition) in raw.iter().zip(&definitions) {
        if !function.ranges.is_empty() {
            concrete[*definition] = true;
        }
    }

    let mut seen = vec![false; raw.len()];
    let order = definitions
        .iter()
        .copied()
        .filter(|definition| !std::mem::replace(&mut seen[*definition], true))
        .collect::<Vec<_>>();
    let named = order
        .par_iter()
        .map(|definition| {
            let origin = &raw[*definition];

            function_name(origin).map(|name| {
                let role = origin_role(origin, &name, &futures);
                (name, role)
            })
        })
        .collect::<Vec<_>>();

    let mut function_ids = vec![None; raw.len()];
    let mut nameless = vec![false; raw.len()];
    let mut named_definitions = Vec::new();
    for (definition, named) in order.iter().zip(&named) {
        if named.is_some() {
            let id = u32::try_from(named_definitions.len())
                .map_err(|_| gimli::Error::UnsupportedOffset)?;
            function_ids[*definition] = Some(FunctionId::new(id));
            named_definitions.push(*definition);
        } else {
            nameless[*definition] = concrete[*definition];
        }
    }

    let named = named.into_iter().flatten().collect::<Vec<_>>();
    let mut functions = named_definitions
        .par_iter()
        .zip(named)
        .enumerate()
        .map(|(id, (definition, (name, role)))| {
            let origin = &raw[*definition];
            FunctionInfo {
                #[expect(clippy::cast_possible_truncation, reason = "numbered as u32 above")]
                id: FunctionId::new(id as u32),
                name,
                linkage_name: origin.linkage_name.clone(),
                declaration: origin.declaration.clone(),
                language: origin.language,
                role,
                enclosing: None,
                coroutine: None,
                generics: Arc::from([]),
                main_subprogram: origin.main_subprogram,
            }
        })
        .collect::<Vec<_>>();
    let trampolines = named_definitions
        .par_iter()
        .map(|definition| raw[*definition].trampoline)
        .collect::<Vec<_>>();
    let body_futures = body_futures(&raw, &named_definitions, &function_ids);

    let (code_instances, instance_ids) =
        code_instances(&raw, &definitions, &function_ids, &nameless)?;
    crate::count!("functions", functions.len());
    crate::count!("code_instances", code_instances.len());
    let roles = crate::span!("functions.go_roles");
    assign_go_function_roles(
        &mut functions,
        &trampolines,
        files.paths(),
        &code_instances,
        go_table,
    );
    drop(roles);
    let records = raw.len();
    crate::model::free(raw, records);

    Ok(FunctionMetadata {
        functions,
        code_instances,
        instance_ids,
        body_futures,
    })
}

fn body_futures(
    raw: &RawFunctions,
    definitions: &[usize],
    function_ids: &[Option<FunctionId>],
) -> Vec<(FunctionId, Arc<str>)> {
    definitions
        .par_iter()
        .filter_map(|definition| {
            let origin = &raw[*definition];
            if origin.language != SourceLanguage::Rust {
                return None;
            }

            let future = origin
                .name
                .as_deref()
                .zip(origin.namespace.as_deref())
                .and_then(|(name, namespace)| super::coroutines::body_future(name, namespace))?;
            Some((
                function_ids.get(*definition).copied().flatten()?,
                Arc::from(future),
            ))
        })
        .collect()
}

#[expect(
    clippy::indexing_slicing,
    reason = "definitions and function indexes are into `raw`, and each table indexed by one \
              is `raw.len()` long"
)]
fn code_instances(
    raw: &RawFunctions,
    definitions: &[usize],
    function_ids: &[Option<FunctionId>],
    nameless: &[bool],
) -> std::result::Result<(Vec<CodeInstanceInfo>, DieMap<CodeInstanceId>), DwarfError> {
    debug_assert!(
        nameless.len() == raw.len() && function_ids.len() == raw.len(),
        "{} functions, but {} nameless flags and {} function ids",
        raw.len(),
        nameless.len(),
        function_ids.len(),
    );

    let has_code = raw
        .par_iter()
        .zip(definitions)
        .map(|(function, definition)| !function.ranges.is_empty() && !nameless[*definition])
        .collect::<Vec<_>>();
    let mut instances = vec![None; raw.len()];
    let mut instance_functions = Vec::new();
    for ((index, has_code), definition) in has_code.into_iter().enumerate().zip(definitions) {
        if has_code {
            let id = CodeInstanceId::new(
                u32::try_from(instance_functions.len())
                    .map_err(|_| gimli::Error::UnsupportedOffset)?,
            );
            let Some(of) = function_ids[*definition] else {
                let key = raw[index].key;
                return Err(DwarfError::ReferencedFunctionMissing {
                    unit: key.unit,
                    offset: key.offset,
                });
            };
            instances[index] = Some(id);
            instance_functions.push((index, of));
        }
    }

    let instance_ids = DieMap {
        first: 0,
        units: raw
            .units
            .par_iter()
            .zip(&raw.starts)
            .map(|(unit, start)| {
                (*start..)
                    .zip(unit)
                    .filter_map(|(index, function)| Some((function.key.offset, instances[index]?)))
                    .collect()
            })
            .collect(),
    };

    let code_instances = instance_functions
        .par_iter()
        .enumerate()
        .map(|(id, (index, of))| {
            let function = &raw[*index];
            CodeInstanceInfo {
                #[expect(clippy::cast_possible_truncation, reason = "numbered as u32 above")]
                id: CodeInstanceId::new(id as u32),
                function: *of,
                parent: match function.kind {
                    RawFunctionKind::Inline => {
                        containing_instance(function.parent, raw, &instances)
                    }
                    RawFunctionKind::Subprogram => None,
                },
                kind: match function.kind {
                    RawFunctionKind::Subprogram => CodeInstanceKind::OutOfLine,
                    RawFunctionKind::Inline => CodeInstanceKind::Inline {
                        call_site: function.call_site.clone(),
                    },
                },
                ranges: Arc::clone(&function.ranges),
                breakpoint_entry: breakpoint_entry(function),
            }
        })
        .collect::<Vec<_>>();

    Ok((code_instances, instance_ids))
}

fn origin_role(origin: &RawFunction, name: &str, futures: &Futures) -> crate::CodeRole {
    let role = super::roles::function_role(
        origin.linkage_name.as_deref().unwrap_or(name),
        origin.trampoline || builds_future(origin, futures),
    );

    match (&origin.namespace, &origin.name) {
        (Some(namespace), Some(own))
            if origin.language == SourceLanguage::Rust && role != crate::CodeRole::Wrapper =>
        {
            super::roles::rust_role(namespace, own).unwrap_or(role)
        }
        _ => role,
    }
}

fn written_name(language: SourceLanguage, name: &str) -> Option<String> {
    match language {
        SourceLanguage::Nim => crate::demangle::qualified_name(name),
        SourceLanguage::Ada => crate::demangle::ada_name(name),
        _ => None,
    }
}

pub(super) fn own_name(language: SourceLanguage, name: &str) -> Option<String> {
    let written = written_name(language, name);
    let qualified = written.as_deref().unwrap_or(name);
    let own = match language {
        SourceLanguage::Nim | SourceLanguage::Odin => qualified.rsplit("::").next(),
        SourceLanguage::Ada => qualified.rsplit('.').next(),
        _ => None,
    };

    own.map(str::to_owned).or(written)
}

fn function_name(function: &RawFunction) -> Option<Arc<str>> {
    if let Some(written) = function
        .name
        .as_deref()
        .and_then(|name| written_name(function.language, name))
    {
        return Some(written.into());
    }

    let name = function.name.clone().or_else(|| {
        function
            .linkage_name
            .as_deref()
            .and_then(crate::demangle::demangle)
            .map(Arc::from)
    });

    match (&name, &function.namespace) {
        (Some(raw), Some(namespace)) if function.language == SourceLanguage::Rust => {
            super::coroutines::body_name(raw, namespace).or(name)
        }
        _ => name,
    }
}

fn link_bodies_by_name(
    functions: &mut [FunctionInfo],
    body_futures: &[(FunctionId, Arc<str>)],
    types: &[crate::TypeNode],
    coroutines: &BTreeMap<crate::TypeId, std::result::Result<crate::CoroutineInfo, Arc<str>>>,
) {
    if body_futures.iter().all(|(function, _)| {
        functions
            .get(function.index())
            .is_none_or(|function| function.coroutine.is_some())
    }) {
        return;
    }

    let mut named: HashMap<String, Vec<crate::TypeId>> = HashMap::new();
    for ty in coroutines
        .iter()
        .filter_map(|(ty, info)| info.is_ok().then_some(*ty))
    {
        let Some(crate::TypeNode::Resolved(info)) = types.get(ty.index()) else {
            continue;
        };

        let Some(identity) = info.identity.as_deref() else {
            continue;
        };

        let mut name = identity.path.join("::");
        name.push_str("::");
        name.push_str(&info.name);
        named.entry(name).or_default().push(ty);
    }

    for (function, future) in body_futures {
        let Some(function) = functions.get_mut(function.index()) else {
            continue;
        };
        if function.coroutine.is_none()
            && let Some(ty) = named.get(&**future).and_then(|types| types.first())
        {
            function.coroutine = Some(*ty);
        }
    }
}

fn builds_future(function: &RawFunction, futures: &Futures) -> bool {
    function.language == SourceLanguage::Rust
        && function
            .returns
            .and_then(|returns| futures.get(&returns))
            .zip(
                function
                    .name
                    .as_deref()
                    .and_then(super::coroutines::without_arguments),
            )
            .is_some_and(|(of, own)| **of == *own)
}

fn breakpoint_entry(function: &RawFunction) -> Option<BreakpointEntry> {
    function
        .entry
        .filter(|entry| function.ranges.iter().any(|range| range.contains(*entry)))
        .map(|address| BreakpointEntry {
            address,
            provenance: EntryProvenance::Explicit,
        })
        .or_else(|| {
            function.ranges.first().map(|range| BreakpointEntry {
                address: range.start,
                provenance: EntryProvenance::RangeStart,
            })
        })
}

fn assign_go_function_roles(
    functions: &mut [FunctionInfo],
    trampolines: &[bool],
    source_files: &[PathBuf],
    instances: &[CodeInstanceInfo],
    go_table: Option<&super::gopclntab::GoTable>,
) {
    let cgo = super::roles::cgo_generated(functions, source_files);
    let runtime_c = super::roles::cgo_runtime(functions, source_files);
    let generated = super::roles::abi_wrappers(functions, source_files)
        .into_iter()
        .zip(trampolines)
        .zip(&cgo)
        .map(|((abi_wrapper, trampoline), cgo)| abi_wrapper || *trampoline || *cgo)
        .collect::<Vec<_>>();
    let mut facts = vec![None; functions.len()];

    if let Some(table) = go_table {
        for instance in instances
            .iter()
            .filter(|instance| matches!(instance.kind, CodeInstanceKind::OutOfLine))
        {
            let Some(entry) = instance.ranges.first().map(|range| range.start.get()) else {
                continue;
            };

            if let Some(facts) = facts.get_mut(instance.function.index()) {
                *facts = table
                    .function_containing(entry)
                    .filter(|function| function.entry == entry)
                    .map(|function| function.facts);
            }
        }
    }

    for ((function, (facts, generated)), runtime_c) in functions
        .iter_mut()
        .zip(facts.into_iter().zip(generated))
        .zip(runtime_c)
    {
        if function.language == SourceLanguage::Go {
            function.role = super::roles::go_role(&function.name, facts, generated);
        } else if function.role == crate::CodeRole::Ordinary {
            if generated {
                function.role = crate::CodeRole::Wrapper;
            } else if runtime_c && go_table.is_some() {
                function.role = crate::CodeRole::RuntimeInternal;
            }
        }
    }
}

#[derive(Default)]
struct UnitFunctions {
    functions: Vec<RawFunction>,
    futures: Futures,
    files: UnitFiles,
}

#[derive(Default)]
struct UnitFiles {
    files: Files,

    by_index: HashMap<u64, Option<crate::SourceFileId>>,
}

struct RawFunctions {
    units: Vec<Vec<RawFunction>>,

    starts: Vec<usize>,

    unit_of: Vec<u32>,
}

impl RawFunctions {
    fn len(&self) -> usize {
        self.starts.last().copied().unwrap_or(0)
    }

    fn iter(&self) -> impl Iterator<Item = &RawFunction> {
        self.units.iter().flatten()
    }

    fn par_iter(&self) -> impl IndexedParallelIterator<Item = &RawFunction> {
        (0..self.len()).into_par_iter().map(|index| &self[index])
    }

    fn at(&self, key: DieKey) -> std::result::Result<usize, DwarfError> {
        self.units
            .get(key.unit)
            .and_then(|unit| {
                unit.binary_search_by_key(&key.offset, |function| function.key.offset)
                    .ok()
            })
            .and_then(|index| Some(self.starts.get(key.unit)? + index))
            .ok_or(DwarfError::ReferencedFunctionMissing {
                unit: key.unit,
                offset: key.offset,
            })
    }
}

impl std::ops::Index<usize> for RawFunctions {
    type Output = RawFunction;

    #[expect(
        clippy::indexing_slicing,
        reason = "callers index below `len()`, `unit_of` names the unit of each function, and \
                  `starts` has one more entry than `units`"
    )]
    fn index(&self, index: usize) -> &RawFunction {
        debug_assert!(index < self.len(), "function {index} of {}", self.len());
        let unit = self.unit_of[index] as usize;
        &self.units[unit][index - self.starts[unit]]
    }
}

fn collect_function_dies<'data>(
    dwarf: &DwarfSet<'data>,
    catalog: &UnitCatalog<'data>,
    files: &mut Files,
) -> std::result::Result<(RawFunctions, Futures), DwarfError> {
    let batches = first_error(
        catalog
            .units
            .par_iter()
            .enumerate()
            .map(|(index, unit)| {
                collect_unit_functions(dwarf, catalog, index)
                    .map_err(|error| error.within(|| describe_unit(unit)))
            })
            .collect(),
    )?;
    let _merge = crate::span!("functions.merge");
    let mut futures = Futures::new();
    let mut starts = vec![0];
    let mut renumbered = Vec::with_capacity(batches.len());
    for batch in batches {
        let ids = batch
            .files
            .files
            .paths()
            .iter()
            .map(|path| files.intern(path.clone()))
            .collect::<Vec<_>>();
        starts.push(starts.last().copied().unwrap_or(0) + batch.functions.len());
        futures.extend(batch.futures);
        renumbered.push((batch.functions, ids));
    }

    #[expect(
        clippy::indexing_slicing,
        reason = "a unit's functions name only its own files, which `ids` maps one to one"
    )]
    let units: Vec<Vec<RawFunction>> = renumbered
        .into_par_iter()
        .map(|(mut functions, ids)| {
            for function in &mut functions {
                for location in [&mut function.declaration, &mut function.call_site]
                    .into_iter()
                    .flatten()
                {
                    location.file = ids[location.file.index()];
                }
            }

            functions
        })
        .collect();

    let unit_of = (0..units.len())
        .into_par_iter()
        .flat_map_iter(|unit: usize| {
            #[expect(clippy::cast_possible_truncation, reason = "units are numbered by u32")]
            std::iter::repeat_n(unit as u32, units.get(unit).map_or(0, Vec::len))
        })
        .collect();

    Ok((
        RawFunctions {
            units,
            starts,
            unit_of,
        },
        futures,
    ))
}

fn collect_unit_functions<'data>(
    dwarf: &DwarfSet<'data>,
    catalog: &UnitCatalog<'data>,
    unit_index: usize,
) -> std::result::Result<UnitFunctions, DwarfError> {
    let units = &catalog.units;
    let unit = units.unit(unit_index)?;
    let mut batch = UnitFunctions::default();
    if is_type_unit(unit) {
        return Ok(batch);
    }

    let language = unit_language(dwarf, units, unit_index)?;

    let wanted = |tag| match tag {
        gimli::DW_TAG_subprogram | gimli::DW_TAG_inlined_subroutine | gimli::DW_TAG_namespace => {
            true
        }
        gimli::DW_TAG_structure_type => language == SourceLanguage::Rust,
        _ => false,
    };

    let mut entries = unit.entries_raw(None)?;
    let mut entry = gimli::DebuggingInformationEntry::null();

    let mut levels = Vec::<(Option<DieKey>, Option<usize>)>::new();
    let mut paths = Vec::<Arc<str>>::new();

    while !entries.is_empty() {
        let depth = entries.next_depth();
        let mut next = entries.clone();

        let Some(abbreviation) = next.read_abbreviation()? else {
            entries = next;
            continue;
        };

        let depth = usize::try_from(depth).map_err(|_| DwarfError::InvalidEntryDepth)?;
        levels.truncate(depth);
        let (parent, namespace) = levels.last().copied().unwrap_or_default();
        if !wanted(abbreviation.tag()) {
            next.skip_attributes(abbreviation.attributes())?;
            entries = next;
            levels.push((parent, namespace));
            continue;
        }

        entries.read_entry(&mut entry)?;
        let entry = &entry;
        let kind = match entry.tag() {
            gimli::DW_TAG_subprogram => Some(RawFunctionKind::Subprogram),
            gimli::DW_TAG_inlined_subroutine => Some(RawFunctionKind::Inline),
            _ => None,
        };

        let key = DieKey {
            unit: unit_index,
            offset: entry.offset().0,
        };

        #[expect(
            clippy::indexing_slicing,
            reason = "namespace indexes are positions this walk pushed to `paths`, which only grows"
        )]
        let inner = if entry.tag() == gimli::DW_TAG_namespace {
            string_attribute(dwarf, unit, entry, gimli::DW_AT_name)?.map(|name| {
                paths.push(match namespace {
                    Some(outer) => format!("{}::{name}", paths[outer]).into(),
                    None => name,
                });
                paths.len() - 1
            })
        } else {
            namespace
        };

        levels.push((if kind.is_some() { Some(key) } else { parent }, inner));
        #[expect(
            clippy::indexing_slicing,
            reason = "namespace indexes are positions this walk pushed to `paths`, which only grows"
        )]
        let namespace = namespace.map(|index| &paths[index]);
        if language == SourceLanguage::Rust
            && let Some(function) = future_of(dwarf, unit, entry, namespace.map(|path| &**path))?
        {
            batch.futures.insert(key, function);
        }

        let Some(kind) = kind else {
            continue;
        };

        batch.functions.push(raw_function(
            dwarf,
            catalog,
            entry,
            (key, kind, parent, language),
            namespace.cloned(),
            &mut batch.files,
        )?);
    }

    Ok(batch)
}

fn raw_function<'data>(
    dwarf: &DwarfSet<'data>,
    catalog: &UnitCatalog<'data>,
    entry: &gimli::DebuggingInformationEntry<Reader<'data>>,
    (key, kind, parent, language): (DieKey, RawFunctionKind, Option<DieKey>, SourceLanguage),
    namespace: Option<Arc<str>>,
    files: &mut UnitFiles,
) -> std::result::Result<RawFunction, DwarfError> {
    let units = &catalog.units;
    let unit = units.unit(key.unit)?;
    Ok(RawFunction {
        key,
        language,
        kind,
        parent,
        abstract_origin: die_reference(
            entry.attr_value(gimli::DW_AT_abstract_origin),
            key.unit,
            units,
        )?,
        specification: die_reference(
            entry.attr_value(gimli::DW_AT_specification),
            key.unit,
            units,
        )?,
        name: string_attribute(dwarf, unit, entry, gimli::DW_AT_name)?,
        linkage_name: string_attribute(dwarf, unit, entry, gimli::DW_AT_linkage_name)?,
        trampoline: entry
            .attr_value(gimli::DW_AT_trampoline)
            .is_some_and(|value| value != gimli::AttributeValue::Flag(false)),
        main_subprogram: entry
            .attr_value(gimli::DW_AT_main_subprogram)
            .is_some_and(|value| value != gimli::AttributeValue::Flag(false)),
        declaration: entry_source_location(
            dwarf,
            units,
            unit,
            entry,
            [
                gimli::DW_AT_decl_file,
                gimli::DW_AT_decl_line,
                gimli::DW_AT_decl_column,
            ],
            files,
        )?,
        call_site: entry_source_location(
            dwarf,
            units,
            unit,
            entry,
            [
                gimli::DW_AT_call_file,
                gimli::DW_AT_call_line,
                gimli::DW_AT_call_column,
            ],
            files,
        )?,
        ranges: die_code_ranges(dwarf, unit, entry, &catalog.code)?.into(),
        entry: entry
            .attr(gimli::DW_AT_entry_pc)
            .map(|attribute| unit_dwarf(dwarf, unit).attr_address(unit, attribute.value()))
            .transpose()?
            .flatten()
            .map(ImageAddress::new),
        namespace,
        returns: die_reference(entry.attr_value(gimli::DW_AT_type), key.unit, units)?,
    })
}

type Futures = HashMap<DieKey, Arc<str>>;

fn future_of(
    dwarf: &DwarfSet<'_>,
    unit: &gimli::Unit<Reader<'_>>,
    entry: &gimli::DebuggingInformationEntry<Reader<'_>>,
    namespace: Option<&str>,
) -> std::result::Result<Option<Arc<str>>, DwarfError> {
    if entry.tag() != gimli::DW_TAG_structure_type {
        return Ok(None);
    }

    let Some(name) = entry.attr_value(gimli::DW_AT_name) else {
        return Ok(None);
    };

    let name = unit_dwarf(dwarf, unit).attr_string(unit, name)?;
    Ok((super::coroutines::coroutine_kind(&text(name))
        == Some(crate::CoroutineKind::AsyncFunction))
    .then(|| namespace.and_then(|namespace| namespace.rsplit("::").next()))
    .flatten()
    .map(Arc::from))
}

fn unit_language(
    dwarf: &DwarfSet<'_>,
    units: &Units<'_>,
    unit_index: usize,
) -> std::result::Result<SourceLanguage, DwarfError> {
    let unit = units.unit(unit_index)?;
    let mut entries = unit.entries();
    let Some(root) = entries.next_dfs()? else {
        return Ok(SourceLanguage::Unknown);
    };

    let language = match root.attr_value(gimli::DW_AT_language) {
        Some(gimli::AttributeValue::Language(language)) => Some(language),
        _ => units.inherited_language(unit_index),
    };

    let producer = string_attribute(dwarf, unit, root, gimli::DW_AT_producer)?;
    let name = str_attribute(dwarf, unit, root, gimli::DW_AT_name)?;
    Ok(variables::source_language(
        language,
        variables::produced_language(producer.as_deref(), name.as_deref()),
    ))
}

fn string_attribute(
    dwarf: &DwarfSet<'_>,
    unit: &gimli::Unit<Reader<'_>>,
    entry: &gimli::DebuggingInformationEntry<Reader<'_>>,
    attribute: gimli::DwAt,
) -> std::result::Result<Option<Arc<str>>, DwarfError> {
    entry
        .attr_value(attribute)
        .map(|value| unit_dwarf(dwarf, unit).attr_string(unit, value))
        .transpose()
        .map_err(DwarfError::from)
        .map(|value| value.map(|value| Arc::<str>::from(text(value).as_ref())))
}

fn str_attribute<'data>(
    dwarf: &DwarfSet<'data>,
    unit: &gimli::Unit<Reader<'data>>,
    entry: &gimli::DebuggingInformationEntry<Reader<'data>>,
    attribute: gimli::DwAt,
) -> std::result::Result<Option<Cow<'data, str>>, DwarfError> {
    entry
        .attr_value(attribute)
        .map(|value| unit_dwarf(dwarf, unit).attr_string(unit, value))
        .transpose()
        .map_err(DwarfError::from)
        .map(|value| value.map(text))
}

fn text(value: Reader<'_>) -> Cow<'_, str> {
    std::str::from_utf8(value.slice())
        .map_or_else(|_| String::from_utf8_lossy(value.slice()), Cow::Borrowed)
}

fn die_reference(
    value: Option<gimli::AttributeValue<Reader<'_>>>,
    unit_index: usize,
    units: &Units<'_>,
) -> std::result::Result<Option<DieKey>, DwarfError> {
    let Some(value) = value else {
        return Ok(None);
    };

    match value {
        gimli::AttributeValue::UnitRef(offset) => Ok(Some(DieKey {
            unit: unit_index,
            offset: offset.0,
        })),
        gimli::AttributeValue::DebugInfoRef(offset) => units
            .containing(offset, units.is_supplementary(unit_index))
            .map(Some)
            .ok_or(DwarfError::ReferenceOutsideUnits(offset.0)),
        gimli::AttributeValue::DebugInfoRefSup(offset) => {
            if units.is_supplementary(unit_index) || units.supplementary_starts.is_empty() {
                return Err(DwarfError::UnsupportedSupplementaryReference);
            }

            units
                .containing(offset, true)
                .map(Some)
                .ok_or(DwarfError::SupplementaryReferenceOutsideUnits(offset.0))
        }
        _ => Err(DwarfError::UnsupportedReferenceForm),
    }
}

fn die_reference_with_signatures(
    value: Option<gimli::AttributeValue<Reader<'_>>>,
    unit_index: usize,
    units: &Units<'_>,
    signatures: &TypeSignatures,
) -> std::result::Result<Option<DieKey>, DwarfError> {
    match value {
        Some(gimli::AttributeValue::DebugTypesRef(signature)) => signatures
            .get(&signature)
            .copied()
            .map(Some)
            .ok_or(DwarfError::TypeSignatureMissing(signature.0)),
        value => die_reference(value, unit_index, units),
    }
}

fn definition_of(start: DieKey, raw: &RawFunctions) -> std::result::Result<usize, DwarfError> {
    let mut index = raw.at(start)?;

    for _ in 0..=raw.len() {
        let function = &raw[index];
        let Some(next) = function.abstract_origin.or(function.specification) else {
            return Ok(index);
        };

        index = raw.at(next)?;
    }

    Err(DwarfError::ReferenceCycle)
}

fn containing_instance(
    mut key: Option<DieKey>,
    raw: &RawFunctions,
    instances: &[Option<CodeInstanceId>],
) -> Option<CodeInstanceId> {
    while let Some(current) = key {
        let index = raw.at(current).ok()?;
        if let Some(instance) = *instances.get(index)? {
            return Some(instance);
        }

        key = raw[index].parent;
    }

    None
}

fn entry_source_location<'data>(
    dwarf: &DwarfSet<'data>,
    units: &Units<'data>,
    unit: &gimli::Unit<Reader<'data>>,
    entry: &gimli::DebuggingInformationEntry<Reader<'data>>,
    [file_attribute, line_attribute, column_attribute]: [gimli::DwAt; 3],
    files: &mut UnitFiles,
) -> std::result::Result<Option<SourceLocation>, DwarfError> {
    let Some(file_index) = entry
        .attr(file_attribute)
        .and_then(gimli::Attribute::udata_value)
    else {
        return Ok(None);
    };

    let Some(line) = entry
        .attr(line_attribute)
        .and_then(gimli::Attribute::udata_value)
        .and_then(LineNumber::new)
    else {
        return Ok(None);
    };

    let file = if let Some(file) = files.by_index.get(&file_index) {
        *file
    } else {
        let header = units
            .line_program(unit)
            .map(gimli::IncompleteLineProgram::header);
        let named = header.and_then(|header| Some((header, header.file(file_index)?)));
        let file = match named {
            Some((header, file)) => {
                Some(files.files.intern(source_path(dwarf, unit, header, file)?))
            }
            None => None,
        };

        files.by_index.insert(file_index, file);
        file
    };

    let Some(file) = file else {
        return Ok(None);
    };

    Ok(Some(SourceLocation {
        file,
        line,
        column: entry
            .attr(column_attribute)
            .and_then(gimli::Attribute::udata_value)
            .and_then(ColumnNumber::new),
    }))
}

fn load_lines<'data>(
    dwarf: &DwarfSet<'data>,
    unit: &gimli::Unit<Reader<'data>>,
    program: Option<&LineProgram<'data>>,
    code: &CodeRanges,
    files: &mut Files,
    tables: &mut LineTables,
) -> std::result::Result<(), DwarfError> {
    let Some(program) = program else {
        return Ok(());
    };

    if program.header().raw_program_buf().is_empty() {
        return Ok(());
    }

    let program = program.clone();

    let mut rows = program.rows();
    let mut sequence = Vec::new();
    let mut keep = None;
    let mut file_ids = HashMap::new();

    while let Some((_, row)) = rows.next_row()? {
        let kept = *keep.get_or_insert_with(|| {
            let start = if row.end_sequence() { 0 } else { row.address() };
            code.contains_address(ImageAddress::new(start))
        });

        if kept {
            sequence.push(*row);
        }

        if !row.end_sequence() {
            continue;
        }

        if kept {
            add_sequence(
                dwarf,
                unit,
                rows.header(),
                &sequence,
                files,
                tables,
                &mut file_ids,
            )?;
        }

        sequence.clear();
        keep = None;
    }

    Ok(())
}

fn add_sequence(
    dwarf: &DwarfSet<'_>,
    unit: &gimli::Unit<Reader<'_>>,
    header: &gimli::LineProgramHeader<Reader<'_>>,
    sequence: &[gimli::LineRow],
    files: &mut Files,
    tables: &mut LineTables,
    file_ids: &mut HashMap<u64, crate::SourceFileId>,
) -> std::result::Result<(), DwarfError> {
    tables.begin_sequence()?;
    let mut open: Option<(u64, u32, bool)> = None;

    for row in sequence {
        let line = row.line().map_or(0, std::num::NonZeroU64::get);

        let file = match row.file(header) {
            Some(file) if line != 0 => Some(if let Some(&id) = file_ids.get(&row.file_index()) {
                id
            } else {
                let id = files.intern(source_path(dwarf, unit, header, file)?);
                file_ids.insert(row.file_index(), id);
                id
            }),
            _ => None,
        };

        let decoded = Row {
            address: row.address(),
            file,
            line: if file.is_some() { line } else { 0 },
            column: match row.column() {
                ColumnType::LeftEdge => 0,
                ColumnType::Column(column) => column.get(),
            },
            operation_index: row.op_index(),
            discriminator: row.discriminator(),
            isa: row.isa(),
            statement: row.is_stmt(),
            prologue_end: row.prologue_end(),
            epilogue_begin: row.epilogue_begin(),
            end_sequence: row.end_sequence(),
        };

        let at = tables.push_row(&decoded)?;

        if decoded.end_sequence || decoded.location().is_none() {
            close_line_range(&mut open, row.address(), tables)?;
            continue;
        }

        if let Some((start, _, true)) = open
            && start == row.address()
            && !row.is_stmt()
        {
            continue;
        }

        let statement = row.is_stmt()
            || open.is_some_and(|(start, _, statement)| start == row.address() && statement);
        close_line_range(&mut open, row.address(), tables)?;
        open = Some((row.address(), at, statement));
    }

    Ok(())
}

fn source_path(
    dwarf: &DwarfSet<'_>,
    unit: &gimli::Unit<Reader<'_>>,
    header: &gimli::LineProgramHeader<Reader<'_>>,
    file: &gimli::FileEntry<Reader<'_>>,
) -> std::result::Result<PathBuf, DwarfError> {
    let dwarf = unit_dwarf(dwarf, unit);
    let file_name = text(dwarf.attr_string(unit, file.path_name())?).into_owned();
    let file_name = PathBuf::from(file_name);
    if file_name.is_absolute() {
        return Ok(file_name);
    }

    let directory = file
        .directory(header)
        .map(|directory| dwarf.attr_string(unit, directory))
        .transpose()?
        .map(|directory| PathBuf::from(text(directory).into_owned()));
    let compilation_directory = unit
        .comp_dir
        .map(|directory| PathBuf::from(text(directory).into_owned()));
    let mut path = PathBuf::new();

    if let Some(directory) = directory {
        if !directory.is_absolute()
            && let Some(compilation_directory) = compilation_directory
        {
            path.push(compilation_directory);
        }

        path.push(directory);
    } else if let Some(compilation_directory) = compilation_directory {
        path.push(compilation_directory);
    }

    path.push(file_name);

    Ok(path)
}

fn close_line_range(
    open: &mut Option<(u64, u32, bool)>,
    end: u64,
    tables: &mut LineTables,
) -> std::result::Result<(), DwarfError> {
    if let Some((start, row, statement)) = open.take()
        && start < end
    {
        tables.push_range(
            AddressRange {
                start: ImageAddress::new(start),
                end: ImageAddress::new(end),
            },
            row,
            statement,
        )?;
    }

    Ok(())
}

fn refine_proved_prologue_entries(
    object: &object::File<'_>,
    target: TargetDescription,
    rows: &StatementsByAddress<'_>,
    instances: &mut [CodeInstanceInfo],
) {
    if target.architecture != Architecture::X86_64 {
        return;
    }

    instances.par_iter_mut().for_each(|instance| {
        if !matches!(instance.kind, CodeInstanceKind::OutOfLine)
            || instance
                .ranges
                .iter()
                .any(|range| rows.within(*range).any(|row| row.flags.prologue_end()))
        {
            return;
        }

        let Some(raw_entry) = instance.breakpoint_entry.map(|entry| entry.address) else {
            return;
        };

        let Some(entry_range) = instance
            .ranges
            .iter()
            .find(|range| range.contains(raw_entry))
        else {
            return;
        };

        let Some(candidate) = first_distinct_source_statement(rows, *entry_range, raw_entry) else {
            return;
        };

        let Some(bytes) = code_bytes(object, raw_entry.get(), candidate.get()) else {
            return;
        };

        #[cfg(target_arch = "x86_64")]
        if super::x86_64::prove_prologue_prefix(bytes, raw_entry.get()).is_ok() {
            instance.breakpoint_entry = Some(BreakpointEntry {
                address: candidate,
                provenance: EntryProvenance::AnalyzedPrologue,
            });
        }
    });
}

#[cfg(target_arch = "x86_64")]
struct ObjectCode<'a, 'data>(&'a object::File<'data>);

#[cfg(target_arch = "x86_64")]
impl ObjectCode<'_, '_> {
    fn section_bytes(&self, address: u64, kinds: &[object::SectionKind]) -> Option<(&[u8], usize)> {
        let section = self.0.sections().find(|section| {
            kinds.contains(&section.kind())
                && section.address() <= address
                && address < section.address().saturating_add(section.size())
        })?;
        let data = section.data().ok()?;
        Some((data, usize::try_from(address - section.address()).ok()?))
    }
}

#[cfg(target_arch = "x86_64")]
impl super::dispatch::DispatchImage for ObjectCode<'_, '_> {
    fn code(&self, address: u64, length: usize) -> Option<&[u8]> {
        let (data, at) = self.section_bytes(address, &[object::SectionKind::Text])?;
        let bytes = data.get(at..)?;
        bytes.get(..bytes.len().min(length))
    }

    fn data(&self, address: u64, length: usize) -> Option<&[u8]> {
        let (data, at) = self.section_bytes(
            address,
            &[
                object::SectionKind::ReadOnlyData,
                object::SectionKind::ReadOnlyString,
                object::SectionKind::Text,
            ],
        )?;
        data.get(at..at.checked_add(length)?)
    }
}

#[cfg(target_arch = "x86_64")]
fn decode_resume_points(
    object: &object::File<'_>,
    rows: &StatementsByAddress<'_>,
    functions: &[FunctionInfo],
    instances: &mut [CodeInstanceInfo],
    coroutines: &BTreeMap<crate::TypeId, std::result::Result<crate::CoroutineInfo, Arc<str>>>,
) -> Vec<(
    CodeInstanceId,
    std::result::Result<crate::ResumePoints, Arc<str>>,
)> {
    let image = ObjectCode(object);
    let mut decoded = Vec::new();
    for instance in instances.iter_mut() {
        let Some(function) = functions.get(instance.function.index()) else {
            continue;
        };
        let Some(Ok(coroutine)) = function.coroutine.and_then(|ty| coroutines.get(&ty)) else {
            continue;
        };

        let header = function.declaration.as_ref().map(|location| location.line);
        let ranges = Arc::clone(&instance.ranges);
        let in_function = |address: u64| {
            let address = ImageAddress::new(address);
            ranges.iter().any(|range| range.contains(address))
        };

        let leading = |address: u64| {
            in_function(address)
                && rows
                    .line_at(ImageAddress::new(address))
                    .is_none_or(|line| line.get() == 0 || Some(line) == header)
        };

        let body_after = |start: ImageAddress| {
            super::dispatch::first_beyond(&image, start, &leading)
                .filter(|body| in_function(body.get()))
        };

        if !matches!(instance.kind, CodeInstanceKind::OutOfLine) {
            if let Some(body) = instance
                .breakpoint_entry
                .and_then(|entry| body_after(entry.address))
            {
                instance.breakpoint_entry = Some(BreakpointEntry {
                    address: body,
                    provenance: EntryProvenance::CoroutineBody,
                });
            }

            continue;
        }

        let Some(entry) = ranges.iter().map(|range| range.start).min() else {
            continue;
        };

        let states = coroutine
            .states
            .iter()
            .map(|state| state.value)
            .collect::<Vec<_>>();
        let points = super::dispatch::decode(&image, &ranges, entry, coroutine.state, &states).map(
            |mut points| {
                let arrival = points
                    .points
                    .iter()
                    .find(|point| {
                        coroutine
                            .state(point.state)
                            .is_some_and(|state| state.kind == crate::CoroutineStateKind::Unresumed)
                    })
                    .map(|point| super::dispatch::flood_all(&image, point.address, &in_function))
                    .filter(|(_, complete)| *complete)
                    .map(|(code, _)| code);
                let mut moved = points.points.to_vec();

                for point in &mut moved {
                    let Some(state) = coroutine.state(point.state) else {
                        continue;
                    };

                    if state.kind == crate::CoroutineStateKind::Unresumed {
                        let body = body_after(point.address).unwrap_or(point.address);
                        point.resumption = [AddressRange {
                            start: point.address,
                            end: body.max(point.address),
                        }]
                        .into();
                        instance.breakpoint_entry = Some(BreakpointEntry {
                            address: body,
                            provenance: EntryProvenance::CoroutineBody,
                        });
                        continue;
                    }

                    let own = state.location.as_ref().map(|location| location.line);
                    let within = |address: u64| {
                        in_function(address)
                            && (rows.line_at(ImageAddress::new(address)).is_none_or(|line| {
                                line.get() == 0 || Some(line) == own || Some(line) == header
                            }) || arrival.as_ref().is_some_and(|arrival| {
                                let address = ImageAddress::new(address);
                                !arrival.iter().any(|range| range.contains(address))
                            }))
                    };

                    point.resumption = super::dispatch::flood(&image, point.address, &within);
                }

                points.points = moved.into();
                points
            },
        );

        decoded.push((instance.id, points));
    }

    decoded
}

fn first_distinct_source_statement(
    rows: &StatementsByAddress<'_>,
    range: AddressRange<ImageAddress>,
    raw_entry: ImageAddress,
) -> Option<ImageAddress> {
    let entry_end = ImageAddress::new(raw_entry.get().checked_add(1)?);
    let mut entry_rows = rows
        .within(AddressRange {
            start: raw_entry,
            end: entry_end,
        })
        .filter(|row| row.location.is_some());
    let entry_row = entry_rows.next_back()?;
    if entry_rows.any(|row| row.sequence != entry_row.sequence) {
        return None;
    }

    let entry_location = entry_row.location.as_ref()?;
    rows.within(AddressRange {
        start: entry_end,
        end: range.end,
    })
    .filter(|row| row.sequence == entry_row.sequence && row.flags.is_statement())
    .find(|row| {
        row.location.as_ref().is_some_and(|location| {
            location.file != entry_location.file || location.line != entry_location.line
        })
    })
    .map(|row| row.address)
}

pub(super) fn code_bytes<'data>(
    object: &'data object::File<'data>,
    start: u64,
    end: u64,
) -> Option<&'data [u8]> {
    let length = usize::try_from(end.checked_sub(start)?).ok()?;
    let section = object.sections().find(|section| {
        section.kind() == object::SectionKind::Text
            && section.address() <= start
            && section
                .address()
                .checked_add(section.size())
                .is_some_and(|section_end| end <= section_end)
    })?;
    let offset = usize::try_from(start - section.address()).ok()?;
    section
        .data()
        .ok()?
        .get(offset..offset.checked_add(length)?)
}

fn target_description(
    object: &object::File<'_>,
) -> std::result::Result<TargetDescription, DwarfError> {
    let architecture = match object.architecture() {
        object::Architecture::X86_64 => Architecture::X86_64,
        object::Architecture::Aarch64 => Architecture::Aarch64,
        other => return Err(DwarfError::UnsupportedArchitecture(other)),
    };

    Ok(TargetDescription {
        architecture,
        byte_order: if object.is_little_endian() {
            ByteOrder::Little
        } else {
            ByteOrder::Big
        },
        pointer_width: if object.is_64() {
            PointerWidth::Bits64
        } else {
            PointerWidth::Bits32
        },
    })
}

#[cfg(all(test, feature = "tools"))]
mod cache_tests;

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use foldhash::HashMap;

    use gimli::write::{
        Address, Dwarf as WriteDwarf, EndianVec, LineProgram, LineString, Sections, Unit,
    };

    use gimli::{Encoding, Format, LineEncoding, LittleEndian, Register};

    use super::*;
    use crate::{LineSequenceId, SourceFileId, StatementFlags, StatementRow};
    use std::fs;

    fn elf_with_dwarf(
        address: u64,
        size: usize,
        sections: &Sections<EndianVec<LittleEndian>>,
    ) -> Vec<u8> {
        let mut debug = Vec::new();
        sections
            .for_each(|id, data| {
                if !data.slice().is_empty() {
                    debug.push((id.name(), data.slice().to_vec(), 0));
                }
                Ok::<_, gimli::Error>(())
            })
            .expect("the sections");

        elf_with_sections(address, size, &debug)
    }

    fn elf_with_sections(address: u64, size: usize, debug: &[(&str, Vec<u8>, u64)]) -> Vec<u8> {
        let mut bytes = Vec::new();
        let mut writer =
            object::write::elf::Writer::new(object::Endianness::Little, true, &mut bytes);
        let text = writer.add_section_name(b".text");
        let names: Vec<_> = debug
            .iter()
            .map(|(name, ..)| writer.add_section_name(name.as_bytes()))
            .collect();
        writer.reserve_file_header();
        writer.reserve_null_section_index();
        writer.reserve_section_index();
        for _ in debug {
            writer.reserve_section_index();
        }
        let code = writer.reserve(size, 16);
        let offsets: Vec<_> = debug
            .iter()
            .map(|(_, data, _)| writer.reserve(data.len(), 1))
            .collect();
        writer.reserve_shstrtab_section_index();
        writer.reserve_shstrtab();
        writer.reserve_section_headers();

        writer
            .write_file_header(&object::write::elf::FileHeader {
                os_abi: 0,
                abi_version: 0,
                e_type: object::elf::ET_DYN,
                e_machine: object::elf::EM_X86_64,
                e_entry: 0,
                e_flags: 0,
            })
            .expect("a header");
        writer.write_align(16);
        writer.write(&vec![0xc3; size]);
        for (_, data, _) in debug {
            writer.write(data);
        }
        writer.write_shstrtab();
        writer.write_null_section_header();
        writer.write_section_header(&object::write::elf::SectionHeader {
            name: Some(text),
            sh_type: object::elf::SHT_PROGBITS,
            sh_flags: u64::from(object::elf::SHF_ALLOC | object::elf::SHF_EXECINSTR),
            sh_addr: address,
            sh_offset: code as u64,
            sh_size: size as u64,
            sh_link: 0,
            sh_info: 0,
            sh_addralign: 16,
            sh_entsize: 0,
        });
        for ((name, (_, data, sh_flags)), offset) in names.into_iter().zip(debug).zip(offsets) {
            writer.write_section_header(&object::write::elf::SectionHeader {
                name: Some(name),
                sh_type: object::elf::SHT_PROGBITS,
                sh_flags: *sh_flags,
                sh_addr: 0,
                sh_offset: offset as u64,
                sh_size: data.len() as u64,
                sh_link: 0,
                sh_info: 0,
                sh_addralign: 1,
                sh_entsize: 0,
            });
        }
        writer.write_shstrtab_section_header();
        bytes
    }

    #[test]
    fn a_compressed_section_is_read_only_within_its_claim_and_our_limits() {
        use crate::test_memory::memory_cap::{start_totals, stop_totals, totals};

        let compressed = |kind: u32, claim: u64, payload: &[u8]| {
            let mut data = Vec::new();
            data.extend_from_slice(&kind.to_le_bytes());
            data.extend_from_slice(&0_u32.to_le_bytes());
            data.extend_from_slice(&claim.to_le_bytes());
            data.extend_from_slice(&1_u64.to_le_bytes());
            data.extend_from_slice(payload);
            let flags = u64::from(object::elf::SHF_COMPRESSED);
            elf_with_sections(0x1000, 0x10, &[(".debug_info", data, flags)])
        };

        let zlib = |data: &[u8]| {
            use std::io::Write as _;
            let mut encoder =
                flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::best());
            encoder.write_all(data).expect("compress");
            encoder.finish().expect("compress")
        };

        let mut state = 1_u64;
        let noise: Vec<u8> = (0..2 << 20)
            .map(|_| {
                state = state
                    .wrapping_mul(6_364_136_223_846_793_005)
                    .wrapping_add(1_442_695_040_888_963_407);
                state.to_le_bytes()[7]
            })
            .collect();

        let cases = [
            (
                "a zlib claim of 64 GiB",
                compressed(object::elf::ELFCOMPRESS_ZLIB, 64 << 30, &zlib(&[0; 4096])),
                LoadLimits::default(),
                "its .debug_info section claims to decompress to 68719476736 bytes from ",
            ),
            (
                "an honest claim past the file's limit",
                compressed(object::elf::ELFCOMPRESS_ZLIB, 2 << 20, &zlib(&noise)),
                LoadLimits {
                    most_decompressed: 1 << 20,
                    ..LoadLimits::default()
                },
                "its compressed sections claim to decompress to 2097152 bytes, more than the \
                 1048576 one file may",
            ),
        ];

        for (case, elf, limits, expected) in cases {
            start_totals();
            let before = totals();
            let loaded = load_debug_info(
                Path::new("bomb"),
                &elf,
                crate::ModuleImageId::new(0),
                &super::super::DebugFileSearch::default(),
                limits,
                None,
            );
            let used = totals().since(&before).allocated.bytes;
            stop_totals();

            let (info, _) = loaded.expect(case);
            let crate::DebugInformation::Unusable { reason } = info.image.debug_information()
            else {
                panic!("{case}: {:?}", info.image.debug_information());
            };
            assert!(reason.starts_with(expected), "{case}: {reason}");
            assert!(used < 8 << 20, "{case}: {used} bytes");
        }
    }

    #[test]
    fn code_sections_claiming_more_bytes_than_a_number_holds_still_load() {
        let code = u64::from(object::elf::SHF_ALLOC | object::elf::SHF_EXECINSTR);
        let mut elf = elf_with_sections(
            0x1000,
            0x10,
            &[
                (".text.a", vec![0xc3; 0x10], code),
                (".text.b", vec![0xc3; 0x10], code),
            ],
        );
        let headers =
            usize::try_from(u64::from_le_bytes(elf[0x28..0x30].try_into().unwrap())).unwrap();
        for section in [2, 3] {
            let size = headers + section * 64 + 0x20;
            elf[size..size + 8].copy_from_slice(&(1_u64 << 63).to_le_bytes());
        }

        load_debug_info(
            Path::new("claims"),
            &elf,
            crate::ModuleImageId::new(0),
            &super::super::DebugFileSearch::default(),
            LoadLimits::default(),
            None,
        )
        .expect("a load");
    }

    #[test]
    fn a_v8_function_sized_past_the_address_space_still_loads() {
        use object::write;

        let mut object = write::Object::new(
            object::BinaryFormat::Elf,
            object::Architecture::X86_64,
            object::Endianness::Little,
        );
        let text = object.add_section(Vec::new(), b".text".to_vec(), object::SectionKind::Text);
        object.append_section_data(text, &[0xc3; 0x100], 16);
        for (name, value, size) in [
            (b"v8_Default_embedded_blob_code_".as_slice(), 0, 0),
            (b"Builtins_Abort", 0x10, 0x10),
            (b"PushAllRegistersAndIterateStack", 0x40, u64::MAX - 0x20),
        ] {
            object.add_symbol(write::Symbol {
                name: name.to_vec(),
                value,
                size,
                kind: object::SymbolKind::Text,
                scope: object::SymbolScope::Dynamic,
                weak: false,
                section: write::SymbolSection::Section(text),
                flags: object::SymbolFlags::None,
            });
        }

        load_debug_info(
            Path::new("v8"),
            &object.write().unwrap(),
            crate::ModuleImageId::new(0),
            &super::super::DebugFileSearch::default(),
            LoadLimits::default(),
            None,
        )
        .expect("a load");
    }

    #[test]
    fn a_function_with_code_and_no_name_leaves_the_rest_loaded() {
        use gimli::write::AttributeValue;

        let encoding = Encoding {
            format: Format::Dwarf32,
            version: 4,
            address_size: 8,
        };
        let mut written = WriteDwarf::new();
        let id = written.units.add(Unit::new(encoding, LineProgram::none()));
        let unit = written.units.get_mut(id);
        let root = unit.root();
        unit.get_mut(root)
            .set(gimli::DW_AT_name, AttributeValue::String(b"a.c".to_vec()));
        unit.get_mut(root).set(
            gimli::DW_AT_language,
            AttributeValue::Language(gimli::DW_LANG_C99),
        );
        for (name, start) in [(None, 0x1000), (Some("named"), 0x1010)] {
            let die = unit.add(root, gimli::DW_TAG_subprogram);
            let entry = unit.get_mut(die);
            if let Some(name) = name {
                entry.set(gimli::DW_AT_name, AttributeValue::String(name.into()));
            }
            entry.set(
                gimli::DW_AT_low_pc,
                AttributeValue::Address(Address::Constant(start)),
            );
            entry.set(gimli::DW_AT_high_pc, AttributeValue::Udata(0x10));
        }
        let mut sections = Sections::new(EndianVec::new(LittleEndian));
        written.write(&mut sections).expect("write test DWARF");

        let elf = elf_with_dwarf(0x1000, 0x20, &sections);
        let info = load_transient_bytes(Path::new("nameless"), &elf, crate::ModuleImageId::new(0))
            .expect("load");
        assert_eq!(
            info.image.debug_information(),
            crate::DebugInformation::Loaded
        );
        let named = info.image.function_named("named").expect("named");
        assert_eq!(
            info.image
                .locate(ImageAddress::new(0x1018))
                .physical_instance
                .and_then(|instance| info.image.code_instance(instance))
                .map(crate::image::functions::CodeInstance::function),
            Some(named.id())
        );
        assert!(
            info.image
                .locate(ImageAddress::new(0x1008))
                .physical_instance
                .is_none()
        );
    }

    #[test]
    fn a_slice_of_a_type_reached_through_a_pointer_to_it_names_its_element() {
        use gimli::write::{AttributeValue, Expression};

        let encoding = Encoding {
            format: Format::Dwarf32,
            version: 4,
            address_size: 8,
        };
        let mut written = WriteDwarf::new();
        let id = written.units.add(Unit::new(encoding, LineProgram::none()));
        let unit = written.units.get_mut(id);
        let root = unit.root();
        unit.get_mut(root)
            .set(gimli::DW_AT_name, AttributeValue::String(b"a.rs".to_vec()));
        unit.get_mut(root).set(
            gimli::DW_AT_language,
            AttributeValue::Language(gimli::DW_LANG_Rust),
        );

        let pointer = unit.add(root, gimli::DW_TAG_pointer_type);
        let remote = unit.add(root, gimli::DW_TAG_structure_type);
        let slice = unit.add(root, gimli::DW_TAG_structure_type);
        let usize = unit.add(root, gimli::DW_TAG_base_type);

        let entry = unit.get_mut(pointer);
        entry.set(
            gimli::DW_AT_name,
            AttributeValue::String(b"*const Remote".to_vec()),
        );
        entry.set(gimli::DW_AT_type, AttributeValue::UnitRef(remote));
        entry.set(gimli::DW_AT_byte_size, AttributeValue::Udata(8));

        let entry = unit.get_mut(usize);
        entry.set(gimli::DW_AT_name, AttributeValue::String(b"usize".to_vec()));
        entry.set(
            gimli::DW_AT_encoding,
            AttributeValue::Encoding(gimli::DW_ATE_unsigned),
        );
        entry.set(gimli::DW_AT_byte_size, AttributeValue::Udata(8));

        for (record, name, members) in [
            (remote, "Remote", &[("remotes", slice)][..]),
            (
                slice,
                "&[Remote]",
                &[("data_ptr", pointer), ("length", usize)][..],
            ),
        ] {
            let entry = unit.get_mut(record);
            entry.set(gimli::DW_AT_name, AttributeValue::String(name.into()));
            entry.set(gimli::DW_AT_byte_size, AttributeValue::Udata(16));
            for (offset, (member, ty)) in (0..).step_by(8).zip(members) {
                let die = unit.add(record, gimli::DW_TAG_member);
                let entry = unit.get_mut(die);
                entry.set(gimli::DW_AT_name, AttributeValue::String((*member).into()));
                entry.set(gimli::DW_AT_type, AttributeValue::UnitRef(*ty));
                entry.set(
                    gimli::DW_AT_data_member_location,
                    AttributeValue::Udata(offset),
                );
            }
        }

        let global = unit.add(root, gimli::DW_TAG_variable);
        let mut location = Expression::new();
        location.op_addr(Address::Constant(0x1000));
        let entry = unit.get_mut(global);
        entry.set(gimli::DW_AT_name, AttributeValue::String(b"FIRST".to_vec()));
        entry.set(gimli::DW_AT_type, AttributeValue::UnitRef(pointer));
        entry.set(gimli::DW_AT_location, AttributeValue::Exprloc(location));

        let mut sections = Sections::new(EndianVec::new(LittleEndian));
        written.write(&mut sections).expect("write test DWARF");

        let elf = elf_with_dwarf(0x1000, 0x20, &sections);
        let info = load_transient_bytes(Path::new("remotes"), &elf, crate::ModuleImageId::new(0))
            .expect("load");
        let named = |name: &str| {
            info.image.types().find_map(|node| match node {
                crate::TypeNode::Resolved(info) if info.name.as_ref() == name => Some(info),
                _ => None,
            })
        };

        let Some(crate::TypeInfo {
            kind: crate::TypeKind::Slice { element, .. },
            ..
        }) = named("&[Remote]")
        else {
            panic!("{:?}", info.image.types().collect::<Vec<_>>());
        };
        assert_eq!(
            Some(element.id),
            named("Remote").map(|remote| remote.reference.id)
        );
    }

    #[test]
    fn type_signature_references_resolve_only_indexed_primary_dies() {
        let signature = gimli::DebugTypeSignature(0x1234_5678_9abc_def0);
        let key = DieKey {
            unit: 3,
            offset: 0x40,
        };

        let signatures = HashMap::from_iter([(signature, key)]);
        let units = Units::new(Vec::new());

        assert_eq!(
            die_reference_with_signatures(
                Some(gimli::AttributeValue::DebugTypesRef(signature)),
                0,
                &units,
                &signatures,
            )
            .expect("indexed signature"),
            Some(key)
        );
        assert!(matches!(
            die_reference_with_signatures(
                Some(gimli::AttributeValue::DebugTypesRef(
                    gimli::DebugTypeSignature(7)
                )),
                0,
                &units,
                &signatures,
            ),
            Err(DwarfError::TypeSignatureMissing(7))
        ));
    }

    #[test]
    fn relative_type_unit_source_paths_coalesce_only_with_a_unique_absolute_suffix() {
        let mut files = Files::default();
        let absolute = files.intern_suffix(PathBuf::from("/work/project/src/types.cpp"));
        assert_eq!(
            files.intern_suffix(PathBuf::from("src/types.cpp")),
            absolute
        );
        assert_eq!(files.paths().len(), 1);

        files.intern_suffix(PathBuf::from("/other/project/src/types.cpp"));
        let ambiguous_relative = files.intern_suffix(PathBuf::from("src/types.cpp"));
        assert_ne!(ambiguous_relative, absolute);
        assert_eq!(files.paths().len(), 3);
    }

    struct TestMemory {
        values: BTreeMap<VirtualAddress, u64>,
    }

    #[test]
    fn line_loader_retains_unattributed_control_boundaries_and_equal_address_order() {
        let encoding = Encoding {
            format: Format::Dwarf32,
            version: 4,
            address_size: 8,
        };

        let mut program = LineProgram::new(
            encoding,
            LineEncoding::default(),
            LineString::String(b"/test".to_vec()),
            None,
            LineString::String(b"boundary.c".to_vec()),
            None,
        );
        let file = program.add_file(
            LineString::String(b"boundary.c".to_vec()),
            program.default_directory(),
            None,
        );
        program.begin_sequence(Some(Address::Constant(0x100)));
        program.row().file = file;
        program.row().line = 0;
        program.row().is_statement = false;
        program.row().prologue_end = true;
        program.generate_row();
        program.row().file = file;
        program.row().line = 10;
        program.row().is_statement = true;
        program.row().epilogue_begin = true;
        program.generate_row();
        program.end_sequence(4);

        let mut written = WriteDwarf::new();
        written.units.add(Unit::new(encoding, program));
        let mut sections = Sections::new(EndianVec::new(LittleEndian));
        written.write(&mut sections).expect("write test DWARF");
        let dwarf = gimli::Dwarf::load(|id| {
            let bytes = sections.get(id).map(EndianVec::slice).unwrap_or_default();
            Ok::<_, gimli::Error>(EndianSlice::new(bytes, RunTimeEndian::Little))
        })
        .expect("read test DWARF");
        let dwarf = DwarfSet::from(dwarf);
        let mut headers = dwarf.units();
        let header = headers.next().unwrap().expect("one test unit");
        let unit = dwarf.unit(header).expect("read test unit");
        let mut files = Files::default();
        let mut tables = LineTables::default();

        let code = |start, end| {
            CodeRanges(vec![AddressRange {
                start: ImageAddress::new(start),
                end: ImageAddress::new(end),
            }])
        };

        load_lines(
            &dwarf,
            &unit,
            unit.line_program.as_ref(),
            &code(0x200, 0x300),
            &mut files,
            &mut tables,
        )
        .expect("load test line program");
        assert!(tables.rows.is_empty() && tables.ranges.is_empty());

        load_lines(
            &dwarf,
            &unit,
            unit.line_program.as_ref(),
            &code(0x100, 0x200),
            &mut files,
            &mut tables,
        )
        .expect("load test line program");
        let statements = tables.statement_rows();
        let lines = tables.line_entries();

        assert_eq!(statements.len(), 2);
        assert_eq!(statements[0].address, ImageAddress::new(0x100));
        assert_eq!(statements[0].ordinal, 0);
        assert!(statements[0].location.is_none());
        assert!(statements[0].flags.prologue_end());
        assert_eq!(statements[1].address, ImageAddress::new(0x100));
        assert_eq!(statements[1].ordinal, 1);
        assert_eq!(
            statements[1]
                .location
                .as_ref()
                .map(|location| location.line),
            LineNumber::new(10)
        );
        assert!(statements[1].flags.epilogue_begin());
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].range.start, ImageAddress::new(0x100));
        assert_eq!(lines[0].range.end, ImageAddress::new(0x104));
    }

    #[derive(Debug, Clone, Copy)]
    struct ModelRow {
        advance: u64,
        line: u64,
        statement: bool,
    }

    #[derive(Debug, Clone)]
    struct ModelSequence {
        rows: Vec<ModelRow>,
        tail: u64,
        in_code: bool,
    }

    fn model_sequence_start(index: usize) -> u64 {
        0x1000 * (index as u64 + 1)
    }

    fn load_model_lines(version: u16, sequences: &[ModelSequence]) -> LineTables {
        let encoding = Encoding {
            format: Format::Dwarf32,
            version,
            address_size: 8,
        };

        let mut program = LineProgram::new(
            encoding,
            LineEncoding::default(),
            LineString::String(b"/test".to_vec()),
            None,
            LineString::String(b"model.c".to_vec()),
            None,
        );
        let file = program.add_file(
            LineString::String(b"model.c".to_vec()),
            program.default_directory(),
            None,
        );

        let mut column = 0;
        for (index, sequence) in sequences.iter().enumerate() {
            program.begin_sequence(Some(Address::Constant(model_sequence_start(index))));
            let mut offset = 0;
            for row in &sequence.rows {
                offset += row.advance;
                column += 1;
                program.row().address_offset = offset;
                program.row().file = file;
                program.row().line = row.line;
                program.row().column = column;
                program.row().is_statement = row.statement;
                program.generate_row();
            }
            program.end_sequence(offset + sequence.tail);
        }

        let mut written = WriteDwarf::new();
        written.units.add(Unit::new(encoding, program));
        let mut sections = Sections::new(EndianVec::new(LittleEndian));
        written.write(&mut sections).expect("write test DWARF");
        let dwarf = gimli::Dwarf::load(|id| {
            let bytes = sections.get(id).map(EndianVec::slice).unwrap_or_default();
            Ok::<_, gimli::Error>(EndianSlice::new(bytes, RunTimeEndian::Little))
        })
        .expect("read test DWARF");
        let dwarf = DwarfSet::from(dwarf);
        let header = dwarf.units().next().unwrap().expect("one test unit");
        let unit = dwarf.unit(header).expect("read test unit");

        let code = CodeRanges(
            sequences
                .iter()
                .enumerate()
                .filter(|(_, sequence)| sequence.in_code)
                .map(|(index, sequence)| {
                    let start = model_sequence_start(index);
                    let length = sequence.rows.iter().map(|row| row.advance).sum::<u64>();
                    AddressRange {
                        start: ImageAddress::new(start),
                        end: ImageAddress::new(start + length + sequence.tail + 1),
                    }
                })
                .collect(),
        );

        let mut tables = LineTables::default();
        load_lines(
            &dwarf,
            &unit,
            unit.line_program.as_ref(),
            &code,
            &mut Files::default(),
            &mut tables,
        )
        .expect("load test line program");
        tables
    }

    fn loaded_ranges(tables: &LineTables) -> Vec<(u64, u64, u64, bool)> {
        tables
            .ranges
            .iter()
            .map(|range| {
                let row = tables.row(range.row.get() as usize);
                (
                    range.start.get(),
                    range.end.get(),
                    row.column,
                    range.statement != 0,
                )
            })
            .collect()
    }

    fn model_ranges(sequences: &[ModelSequence]) -> Vec<(u64, u64, u64, bool)> {
        let mut ranges = Vec::new();
        let mut column = 0;
        for (index, sequence) in sequences.iter().enumerate() {
            let start = model_sequence_start(index);
            let mut rows = Vec::new();
            let mut offset = 0;
            for row in &sequence.rows {
                offset += row.advance;
                column += 1;
                rows.push((start + offset, column, *row));
            }

            let end = start + offset + sequence.tail;
            if !sequence.in_code {
                continue;
            }

            let groups = rows
                .chunk_by(|left, right| left.0 == right.0)
                .collect::<Vec<_>>();
            for (at, group) in groups.iter().enumerate() {
                let address = group[0].0;
                let next = groups.get(at + 1).map_or(end, |next| next[0].0);
                let after = group
                    .iter()
                    .rposition(|(_, _, row)| row.line == 0)
                    .map_or(0, |last| last + 1);
                let counted = &group[after..];
                let Some(last) = counted.last() else {
                    continue;
                };

                if address < next {
                    let location = counted
                        .iter()
                        .rev()
                        .find(|(_, _, row)| row.statement)
                        .unwrap_or(last);
                    let statement = counted.iter().any(|(_, _, row)| row.statement);
                    ranges.push((address, next, location.1, statement));
                }
            }
        }

        ranges
    }

    #[test]
    fn a_line_0_row_after_a_statement_at_one_address_leaves_it_without_a_line() {
        let row = |advance, line, statement| ModelRow {
            advance,
            line,
            statement,
        };
        let sequence = ModelSequence {
            rows: vec![row(0, 7, true), row(0, 0, false), row(4, 8, true)],
            tail: 4,
            in_code: true,
        };

        let tables = load_model_lines(5, std::slice::from_ref(&sequence));
        assert_eq!(loaded_ranges(&tables), [(0x1004, 0x1008, 3, true)]);
    }

    fn model_sequence() -> impl proptest::strategy::Strategy<Value = ModelSequence> {
        use proptest::prelude::*;

        let row = (
            prop_oneof![3 => Just(0_u64), 2 => 1_u64..5],
            prop_oneof![1 => Just(0_u64), 3 => 1_u64..6],
            any::<bool>(),
        )
            .prop_map(|(advance, line, statement)| ModelRow {
                advance,
                line,
                statement,
            });

        (
            proptest::collection::vec(row, 1..12),
            prop_oneof![1 => Just(0_u64), 3 => 1_u64..5],
            prop_oneof![1 => Just(false), 4 => Just(true)],
        )
            .prop_map(|(rows, tail, in_code)| ModelSequence {
                rows,
                tail,
                in_code,
            })
    }

    proptest::proptest! {
        #![proptest_config(proptest::prelude::ProptestConfig::with_cases(256))]

        /// Line ranges are what the model above says, they never overlap,
        /// and a sequence outside the image's code leaves no rows.
        #[test]
        fn line_ranges_follow_the_rows_at_each_address(
            version in proptest::sample::select(vec![4_u16, 5]),
            sequences in proptest::collection::vec(model_sequence(), 1..4),
        ) {
            let tables = load_model_lines(version, &sequences);
            let ranges = loaded_ranges(&tables);
            proptest::prop_assert_eq!(&ranges, &model_ranges(&sequences));

            let mut sorted = ranges;
            sorted.sort_unstable();
            for pair in sorted.windows(2) {
                proptest::prop_assert!(pair[0].1 <= pair[1].0, "{:x?} overlap", pair);
            }

            let rows = sequences
                .iter()
                .filter(|sequence| sequence.in_code)
                .map(|sequence| sequence.rows.len() + 1)
                .sum::<usize>();
            proptest::prop_assert_eq!(tables.rows.len(), rows);
        }
    }

    fn analyzed_entry_row(address: u64, line: u64, sequence: u32, ordinal: u32) -> StatementRow {
        StatementRow {
            address: ImageAddress::new(address),
            operation_index: 0,
            location: Some(SourceLocation {
                file: SourceFileId::new(0),
                line: LineNumber::new(line).expect("nonzero test line"),
                column: None,
            }),
            discriminator: 0,
            flags: StatementFlags::empty().with_statement(true),
            isa: 0,
            sequence: LineSequenceId::new(sequence),
            ordinal,
        }
    }

    #[test]
    fn analyzed_entry_skips_the_signature_line_within_one_sequence() {
        let row = analyzed_entry_row;
        for (rows, expected) in [
            (
                [
                    row(0x100, 10, 0, 0),
                    row(0x110, 10, 0, 1),
                    row(0x120, 11, 0, 2),
                ],
                Some(0x120),
            ),
            (
                [
                    row(0x100, 10, 0, 0),
                    row(0x100, 50, 1, 0),
                    row(0x120, 11, 0, 1),
                ],
                None,
            ),
            (
                [
                    row(0x100, 10, 0, 0),
                    row(0x110, 50, 1, 0),
                    row(0x120, 11, 0, 1),
                ],
                Some(0x120),
            ),
        ] {
            let mut rows = rows;
            rows.sort_by_key(|row| (row.sequence, row.ordinal));
            let tables = crate::image::lines::from_statement_rows(&rows);
            assert_eq!(
                first_distinct_source_statement(
                    &tables.statements_by_address(),
                    AddressRange {
                        start: ImageAddress::new(0x100),
                        end: ImageAddress::new(0x130),
                    },
                    ImageAddress::new(0x100),
                ),
                expected.map(ImageAddress::new)
            );
        }
    }

    impl MemoryReader for TestMemory {
        fn read_u64(&mut self, address: VirtualAddress) -> Option<u64> {
            self.values.get(&address).copied()
        }
    }

    const TEST_ENCODING: Encoding = Encoding {
        format: Format::Dwarf32,
        version: 4,
        address_size: 8,
    };

    #[test]
    fn register_rules_distinguish_locations_values_and_frozen_registers() {
        let dwarf = DwarfRegister::new;
        let current = RegisterFile::new([(dwarf(1), 100), (dwarf(2), 200), (dwarf(3), 300)]);
        let mut caller = current.clone();
        let mut memory = TestMemory {
            values: [(0xff8, 0xfeed), (0x64, 0xbeef)]
                .into_iter()
                .map(|(address, value)| (VirtualAddress::new(address), value))
                .collect(),
        };

        let expressions: [&[u8]; 5] = [
            &[0x38, 0x1c],
            &[0x40, 0x22],
            &[0x31, 0x22, 0x9f],
            &[0x71, 0x00],
            &[0x9f],
        ];
        let bytes = expressions.concat();
        let section = EhFrame::new(&bytes, RunTimeEndian::Little);
        let mut offset = 0;
        let [below, above, stack_value, through_register, malformed] = expressions.map(|bytes| {
            let expression = UnwindExpression {
                offset,
                length: bytes.len(),
            };

            offset += bytes.len();
            expression
        });
        let mut apply = |caller: &mut RegisterFile, cfa, register, rule| {
            RuleContext {
                current: &current,
                cfa: VirtualAddress::new(cfa),
                section: &section,
                encoding: TEST_ENCODING,
            }
            .apply(caller, &mut memory, dwarf(register), &rule)
        };

        for (register, rule) in [
            (1, RegisterRule::Constant(999)),
            (4, RegisterRule::Register(Register(1))),
            (5, RegisterRule::Offset(-8)),
            (6, RegisterRule::ValOffset(-8)),
            (3, RegisterRule::Undefined),
            (7, RegisterRule::Expression(below)),
            (8, RegisterRule::ValExpression(above)),
            (9, RegisterRule::ValExpression(stack_value)),
            (10, RegisterRule::Expression(through_register)),
        ] {
            apply(&mut caller, 0x1000, register, rule).unwrap();
        }

        assert_eq!(caller.get(dwarf(1)), Some(999));
        assert_eq!(
            caller.get(dwarf(4)),
            Some(100),
            "rule read mutated caller state"
        );
        assert_eq!(caller.get(dwarf(5)), Some(0xfeed));
        assert_eq!(caller.get(dwarf(6)), Some(0xff8));
        assert_eq!(caller.get(dwarf(3)), None);
        assert_eq!(caller.get(dwarf(7)), Some(0xfeed));
        assert_eq!(caller.get(dwarf(8)), Some(0x1010));
        assert_eq!(caller.get(dwarf(9)), Some(0x1001));
        assert_eq!(
            caller.get(dwarf(10)),
            Some(0xbeef),
            "expressions read the callee's registers"
        );
        assert_eq!(
            apply(&mut caller, 0x1000, 11, RegisterRule::Expression(malformed)),
            Err(UnwindTermination::CorruptUnwindInfo {
                description: "register expression: a value where a saved register's address \
                              belongs"
                    .into(),
            })
        );

        for (cfa, rule, failure) in [
            (
                0,
                RegisterRule::Offset(-1),
                UnwindTermination::InvalidCaller {
                    description: "saved-register address overflow".into(),
                },
            ),
            (
                0x1000,
                RegisterRule::Offset(0),
                UnwindTermination::MemoryReadFailed {
                    address: VirtualAddress::new(0x1000),
                },
            ),
            (
                0,
                RegisterRule::Register(Register(9)),
                UnwindTermination::RegisterUnavailable {
                    register: "DWARF register 9".into(),
                },
            ),
        ] {
            assert_eq!(apply(&mut caller, cfa, 1, rule), Err(failure));
        }
    }

    #[test]
    fn unwind_expressions_reject_non_default_address_spaces() {
        let bytes = [0x30, 0x31, 0x18];
        let section = EhFrame::new(&bytes, RunTimeEndian::Little);
        let expression = UnwindExpression {
            offset: 0usize,
            length: bytes.len(),
        };

        let registers = RegisterFile::new([]);
        let mut memory = TestMemory {
            values: BTreeMap::new(),
        };

        assert!(matches!(
            evaluate_unwind_expression(
                &expression,
                &section,
                TEST_ENCODING,
                &registers,
                &mut memory,
                None,
                "CFA",
            ),
            Err(UnwindTermination::UnsupportedUnwindInfo { feature })
                if &*feature == "CFA expression: non-default memory address space"
        ));
    }

    fn check_fde_index<'data, S: UnwindSection<Reader<'data>>>(
        section: &S,
        bases: &BaseAddresses,
        index: crate::image::unwind::FdeLookup<'_>,
        extra: &[u64],
    ) {
        use crate::image::unwind::FdeMiss;

        let mut probes = vec![0, u64::MAX];
        probes.extend(extra);
        for fde in index.entries {
            let (start, end) = (fde.start.get(), fde.end.get());
            probes.extend([start.wrapping_sub(1), start, start + 1, end - 1, end]);
        }

        let mut open = probes
            .iter()
            .copied()
            .collect::<std::collections::BTreeSet<_>>();
        let mut found = std::collections::BTreeMap::new();
        let mut entries = section.entries(bases);
        let failure = loop {
            let partial = match entries.next() {
                Ok(Some(gimli::CieOrFde::Fde(partial))) => partial,
                Ok(Some(gimli::CieOrFde::Cie(_))) => continue,
                Ok(None) => break None,
                Err(error) => break Some(error),
            };

            let fde = match partial.parse(S::cie_from_offset) {
                Ok(fde) => fde,
                Err(error) => break Some(error),
            };

            let contained = open
                .range(fde.initial_address()..)
                .take_while(|&&address| fde.contains(address))
                .copied()
                .collect::<Vec<_>>();
            for address in contained {
                open.remove(&address);
                found.insert(address, fde.offset());
            }
        };

        for address in probes {
            let walk = found.get(&address).copied().ok_or_else(|| {
                failure
                    .filter(|&error| error != gimli::Error::NoUnwindInfoForAddress)
                    .map(|error| error.to_string())
            });
            let indexed = index.lookup(address).map_err(|miss| match miss {
                FdeMiss::NoEntry => None,
                FdeMiss::Malformed(error) => Some(error.to_owned()),
            });
            assert_eq!(indexed, walk, "address {address:#x}");
        }
    }

    #[test]
    fn every_number_of_workers_loads_the_same_image() {
        let mut rewalked = 0;

        for fixture in [
            "containers-cpp-clang-o2",
            "containers-rust-o2",
            "tokio-server-1.51-o3",
            "values-go-o0",
            "containers-zig-self-hosted",
            "callers-go-stripped",
        ] {
            let path = Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("build/test-programs")
                .join(fixture);
            let data = fs::read(&path).expect("run `just build-test-programs`");
            let load_with = |jobs, limits| {
                let pool = rayon::ThreadPoolBuilder::new()
                    .num_threads(jobs)
                    .build()
                    .expect("a pool");
                let (info, _) = pool
                    .install(|| {
                        load_debug_info(
                            &path,
                            &data,
                            crate::ModuleImageId::new(0),
                            &super::super::DebugFileSearch::default(),
                            limits,
                            None,
                        )
                    })
                    .expect("load the fixture");

                info.image.image_bytes().to_vec()
            };

            let load = |jobs| load_with(jobs, LoadLimits::default());
            let serial = load(1);

            let waiting = LoadLimits {
                walk_ahead: 1,
                ..LoadLimits::default()
            };

            assert!(
                load_with(8, waiting) == serial,
                "{fixture} differs walking one unit ahead"
            );
            assert!(!serial.is_empty(), "{fixture}");
            let recording = crate::timings::Recording::start(crate::timings::Options::default())
                .expect("no other recording");
            let apart = load(8);
            let report = recording.finish().unwrap();
            assert!(apart == serial, "{fixture} differs with 8 workers");
            assert_eq!(
                report.counters.get("units_walked_in_order"),
                None,
                "{fixture} walks its units apart with 8 workers"
            );
            rewalked += report
                .counters
                .get("units_rewalked_in_order")
                .copied()
                .unwrap_or(0);
        }

        assert!(
            rewalked > 0,
            "some fixture walks a unit in order after its types differ"
        );
    }

    #[test]
    fn plt_stubs_unwind_to_their_callers_without_call_frame_information() {
        for (fixture, stub, offsets) in [
            ("disassembly-gcc-mold", "printf@plt", &[0, 6][..]),
            ("disassembly-gcc-mold", "__cxa_finalize@plt", &[0]),
        ] {
            let path = Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("build/test-programs")
                .join(fixture);
            let data = fs::read(&path).expect("run `just build-test-programs`");
            let (info, _) = load_debug_info(
                &path,
                &data,
                crate::ModuleImageId::new(0),
                &super::super::DebugFileSearch::default(),
                LoadLimits::default(),
                None,
            )
            .expect("the fixture loads");
            let start = info
                .image
                .symbols_named(stub)
                .next()
                .unwrap_or_else(|| panic!("{fixture}'s {stub}"))
                .address()
                .get();
            let stack = 0x7fff_0000;
            let mut memory = TestMemory {
                values: std::iter::once((VirtualAddress::new(stack), 0x1234)).collect(),
            };

            for offset in offsets {
                let address = ImageAddress::new(start + offset);
                let registers = RegisterFile::new([
                    (crate::unwind::STACK_POINTER, stack),
                    (crate::unwind::PROGRAM_COUNTER, address.get()),
                    (DwarfRegister::new(3), 0xb),
                    (DwarfRegister::new(0), 0xa),
                ]);
                let step = info
                    .unwind
                    .unwind(address, &registers, &mut memory)
                    .unwrap_or_else(|end| panic!("{fixture}: {stub}+{offset}: {end}"));
                assert_eq!(step.cfa, VirtualAddress::new(stack + 8), "{stub}+{offset}");
                assert_eq!(
                    info.unwind.cfa(address, &registers, &mut memory),
                    Ok(step.cfa)
                );
                assert_eq!(
                    step.registers.get(crate::unwind::PROGRAM_COUNTER),
                    Some(0x1234)
                );
                assert_eq!(
                    step.registers.get(crate::unwind::STACK_POINTER),
                    Some(stack + 8)
                );
                assert_eq!(
                    step.registers.get(DwarfRegister::new(3)),
                    Some(0xb),
                    "rbx survives a call"
                );
                assert_eq!(
                    step.registers.get(DwarfRegister::new(0)),
                    None,
                    "rax need not"
                );
            }

            let header = info
                .image
                .sections()
                .iter()
                .find(|section| &*section.name == ".plt")
                .expect("a .plt")
                .range
                .start;
            let registers = RegisterFile::new([
                (crate::unwind::STACK_POINTER, stack),
                (crate::unwind::PROGRAM_COUNTER, header.get()),
            ]);
            assert!(
                matches!(
                    info.unwind.unwind(header, &registers, &mut memory),
                    Err(UnwindTermination::NoUnwindInfo { .. })
                ),
                "{fixture}'s .plt header"
            );
        }
    }

    #[test]
    #[expect(
        clippy::too_many_lines,
        reason = "a budget too small for anything, then one for the locals alone"
    )]
    fn a_load_past_its_budget_leaves_its_locals_out_and_keeps_the_rest() {
        let path =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("build/test-programs/tokio-std-async-o0");
        let data = fs::read(&path).expect("run `just build-test-programs`");
        let load = |floor| {
            load_debug_info(
                &path,
                &data,
                crate::ModuleImageId::new(0),
                &super::super::DebugFileSearch::default(),
                LoadLimits {
                    per_input_byte: 0,
                    floor,
                    ..LoadLimits::default()
                },
                None,
            )
            .expect("a load")
            .0
        };

        let full = load(u64::MAX);
        assert_eq!(
            full.image.debug_information(),
            crate::DebugInformation::Loaded
        );

        let over = load(64 << 10);
        let crate::DebugInformation::Unusable { reason } = over.image.debug_information() else {
            panic!("{:?}", over.image.debug_information());
        };
        assert!(
            ["types", "data objects", "symbolic names"]
                .iter()
                .any(|what| reason.starts_with(&format!(
                    "the debug information needs more than its load budget of {} bytes: {what} \
                     asked for ",
                    64 << 10
                ))),
            "{reason}"
        );
        assert_eq!(over.image.functions().len(), 0);
        assert_ne!(over.image.symbols().count(), 0, "its symbols describe it");

        let (mut small, mut large) = (64_u64 << 10, 1 << 32);
        let partial = loop {
            assert!(small + 1 < large, "no budget leaves out only the locals");
            let floor = (small.isqrt() * large.isqrt()).clamp(small + 1, large - 1);
            let info = load(floor);
            match info.image.debug_information() {
                crate::DebugInformation::Loaded => large = floor,
                crate::DebugInformation::Unusable { .. } => small = floor,
                _ => break info,
            }
        };

        let crate::DebugInformation::Incomplete { reason } = partial.image.debug_information()
        else {
            panic!("{:?}", partial.image.debug_information());
        };

        assert!(
            reason.starts_with("its locals and parameters were left out, as the debug information needs more than its load budget of "),
            "{reason}"
        );
        assert_eq!(partial.image.locals_left_out(), Some(reason.clone()));
        assert_eq!(full.image.locals_left_out(), None);
        let coroutines = |info: &DebugInfo| {
            info.image
                .functions()
                .filter(|function| function.coroutine().is_some())
                .count()
        };

        assert_ne!(coroutines(&full), 0);
        assert_eq!(coroutines(&partial), coroutines(&full));
        assert_eq!(
            partial.image.functions().len(),
            full.image.functions().len()
        );
        assert_eq!(
            partial.image.statement_rows().count(),
            full.image.statement_rows().count()
        );
        assert_ne!(partial.image.globals().count(), 0);
        assert_eq!(
            partial.image.globals().count(),
            full.image.globals().count()
        );

        let run = full
            .image
            .functions_named("run")
            .find(|function| {
                function
                    .linkage_name()
                    .is_some_and(|name| name.contains("std_async"))
            })
            .and_then(|function| function.instances().next())
            .expect("the fixture's run");
        let address = run.ranges().next().expect("run's code").start;
        let line = |info: &DebugInfo| info.image.source_location(address).map(|at| at.line);
        assert!(line(&full).is_some());
        assert_eq!(line(&partial), line(&full));
        let visible = |info: &DebugInfo| info.variables.visible_object(address, None, "tasks");
        assert!(visible(&full).is_ok(), "{:?}", visible(&full).err());
        assert!(
            matches!(
                visible(&partial),
                Err(crate::Error::LocalsLeftOut(ref why)) if *why == reason
            ),
            "{:?}",
            visible(&partial)
        );
    }

    #[test]
    fn indexed_fde_lookups_match_a_walk_of_real_sections() {
        let mut indexed = 0;
        let mut truncations = 0;
        for fixture in ["basic", "containers-cpp-gcc-o2", "callers-go"] {
            let path = Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("build/test-programs")
                .join(fixture);
            let data = fs::read(&path).expect("run `just build-test-programs`");
            let object = object::File::parse(&*data).expect("ELF");
            let target = target_description(&object).expect("target");
            let unwind = load_unwind_info(
                &object,
                None,
                target,
                Vec::new(),
                None,
                LoadLimits::default().most_decompressed,
            )
            .expect("CFI");
            let sections = UnwindSections::of(&unwind);
            check_fde_index(
                &sections.eh_frame(),
                &sections.bases,
                unwind.eh_frame_index.lookup(),
                &[],
            );
            check_fde_index(
                &sections.debug_frame(),
                &sections.bases,
                unwind.debug_frame_index.lookup(),
                &[],
            );
            indexed += unwind.eh_frame_index.entries.len() + unwind.debug_frame_index.entries.len();

            let info = load_bytes(
                &path,
                &data,
                crate::ModuleImageId::new(0),
                &super::super::DebugFileSearch::default(),
            )
            .expect("load the fixture");
            let view = crate::image::unwind::UnwindView::new(info.image.tables());
            let kept = UnwindSections::in_image(view);
            check_fde_index(&kept.eh_frame(), &kept.bases, view.eh_frame_index(), &[]);
            check_fde_index(
                &kept.debug_frame(),
                &kept.bases,
                view.debug_frame_index(),
                &[],
            );
            assert_eq!(view.eh_frame_index().entries, unwind.eh_frame_index.entries);

            let probes: Vec<u64> = unwind
                .function_ranges()
                .iter()
                .map(|range| range.start.get())
                .collect();
            let mut eh_frame = EhFrame::new(
                &unwind.eh_frame[..unwind.eh_frame.len() / 2],
                RunTimeEndian::Little,
            );
            eh_frame.set_address_size(8);
            let debug_frame = DebugFrame::new(
                &unwind.debug_frame[..unwind.debug_frame.len() / 2],
                RunTimeEndian::Little,
            );
            let index = fde_index(&eh_frame, &sections.bases);
            truncations += usize::from(index.first_error.is_some());
            check_fde_index(&eh_frame, &sections.bases, index.lookup(), &probes);
            let index = fde_index(&debug_frame, &sections.bases);
            truncations += usize::from(index.first_error.is_some());
            check_fde_index(&debug_frame, &sections.bases, index.lookup(), &probes);
        }

        assert!(indexed > 1000, "the fixtures describe {indexed} functions");
        assert!(
            truncations >= 2,
            "only {truncations} sections ended mid-entry"
        );
    }

    #[test]
    fn an_epilogue_that_restored_its_callers_registers_unwinds_from_a_samples_copy() {
        use gimli::write::CallFrameInstruction;

        let mut code = vec![
            0x55, 0x48, 0x89, 0xe5, 0x53, 0x48, 0x8b, 0x5d, 0xf8, 0xc9, 0x31, 0xd2, 0x31, 0xc9,
            0xc3,
        ];
        let handler = [
            0x55, 0x48, 0x89, 0xe5, 0x48, 0x83, 0xec, 0x30, 0x48, 0x83, 0xc4, 0x30, 0x5d, 0x49,
            0x81, 0x3c, 0xc0, 0x40, 0x1d, 0xa0, 0x01, 0x0f, 0x84, 0x00, 0x10, 0x00, 0x00, 0x41,
            0xff, 0x24, 0xc0,
        ];
        let (start, dispatch) = (0x1000, 0x2000);
        let mut cie = gimli::write::CommonInformationEntry::new(TEST_ENCODING, 1, -8, Register(16));
        cie.add_instruction(CallFrameInstruction::Cfa(Register(7), 8));
        cie.add_instruction(CallFrameInstruction::Offset(Register(16), -8));
        let mut table = gimli::write::FrameTable::default();
        let cie = table.add_cie(cie);
        let frame = |at: u64, length: usize, instructions: &[(u32, CallFrameInstruction)]| {
            let length = u32::try_from(length).expect("short");
            let mut fde = gimli::write::FrameDescriptionEntry::new(Address::Constant(at), length);
            for (offset, instruction) in instructions {
                fde.add_instruction(*offset, instruction.clone());
            }
            fde
        };
        let set_up = [
            (1, CallFrameInstruction::CfaOffset(16)),
            (1, CallFrameInstruction::Offset(Register(6), -16)),
            (4, CallFrameInstruction::CfaRegister(Register(6))),
        ];
        let mut malloc = set_up.to_vec();
        malloc.push((5, CallFrameInstruction::Offset(Register(3), -24)));
        malloc.push((10, CallFrameInstruction::Cfa(Register(7), 8)));
        table.add_fde(cie, frame(start, code.len(), &malloc));
        let mut freed = set_up.to_vec();
        freed.push((13, CallFrameInstruction::Cfa(Register(7), 8)));
        table.add_fde(cie, frame(dispatch, handler.len(), &freed));
        let mut written = gimli::write::DebugFrame(EndianVec::new(LittleEndian));
        table.write_debug_frame(&mut written).expect("write");
        let bytes = written.0.into_vec();
        let section = DebugFrame::new(&bytes, RunTimeEndian::Little);
        let bases = BaseAddresses::default();

        let range = |from, to| AddressRange {
            start: ImageAddress::new(from),
            end: ImageAddress::new(to),
        };
        assert_eq!(
            restored_returns(&section, &bases, &[(start, &code), (dispatch, &handler)]),
            [
                (range(start + 10, start + 15), 0x3f),
                (range(dispatch + 13, dispatch + 27), 2),
            ]
        );

        let stack = 0x7fff_0000;
        let registers = RegisterFile::new([
            (crate::unwind::STACK_POINTER, stack),
            (crate::unwind::PROGRAM_COUNTER, start + 12),
            (DwarfRegister::new(6), 0xf00d),
            (DwarfRegister::new(3), 0xb0b),
        ]);
        let copy = || TestMemory {
            values: std::iter::once((VirtualAddress::new(stack), 0x4242)).collect(),
        };
        let index = fde_index(&section, &bases);
        let unwind = |restored, memory: &mut TestMemory| {
            unwind_from_section(
                &section,
                index.lookup(),
                &bases,
                ImageAddress::new(start + 12),
                &registers,
                (&crate::unwind::CALL_CLOBBERED_REGISTERS, restored),
                memory,
            )
            .expect("a caller")
        };
        let kept = |step: &UnwindStep| {
            [6, 3, 16].map(|register| step.registers.get(DwarfRegister::new(register)))
        };

        let step = unwind(0x3f, &mut copy());
        assert_eq!(step.cfa, VirtualAddress::new(stack + 8));
        assert_eq!(kept(&step), [Some(0xf00d), Some(0xb0b), Some(0x4242)]);
        assert_eq!(
            kept(&unwind(2, &mut copy())),
            [Some(0xf00d), None, Some(0x4242)]
        );
        assert_eq!(kept(&unwind(0, &mut copy())), [None, None, Some(0x4242)]);

        let mut live = copy();
        live.values.insert(VirtualAddress::new(stack - 8), 0xbeef);
        live.values.insert(VirtualAddress::new(stack - 16), 0xcafe);
        assert_eq!(
            kept(&unwind(0x3f, &mut live)),
            [Some(0xbeef), Some(0xcafe), Some(0x4242)]
        );

        code[13] = 0xdb;
        assert_eq!(
            restored_returns(&section, &bases, &[(start, &code)]),
            [(range(start + 10, start + 14), 2)]
        );
    }

    #[test]
    fn indexed_fde_lookups_follow_section_order() {
        let mut table = gimli::write::FrameTable::default();
        let cie = table.add_cie(gimli::write::CommonInformationEntry::new(
            Encoding {
                format: Format::Dwarf32,
                version: 1,
                address_size: 8,
            },
            1,
            -8,
            Register(16),
        ));
        for (start, length) in [
            (0x1000, 0x100),
            (0x1080, 0x180),
            (0x0f00, 0x1100),
            (0x1100, 0),
            (0x1040, 0x10),
            (0x3000, 0x100),
        ] {
            table.add_fde(
                cie,
                gimli::write::FrameDescriptionEntry::new(Address::Constant(start), length),
            );
        }

        let mut written = gimli::write::DebugFrame(EndianVec::new(LittleEndian));
        table.write_debug_frame(&mut written).expect("write");
        let mut bytes = written.0.into_vec();
        let bases = BaseAddresses::default();
        let section = DebugFrame::new(&bytes, RunTimeEndian::Little);
        let index = fde_index(&section, &bases);
        assert_eq!(index.entries.len(), 5);
        check_fde_index(&section, &bases, index.lookup(), &[0x1050, 0x1150, 0x1fff]);

        let mut offsets: Vec<usize> = index
            .entries
            .iter()
            .map(|fde| fde.value.get() as usize)
            .collect();
        offsets.sort_unstable();
        let second = offsets[1];
        bytes[second + 4..second + 8].copy_from_slice(&0x7fff_0000_u32.to_le_bytes());
        let section = DebugFrame::new(&bytes, RunTimeEndian::Little);
        let index = fde_index(&section, &bases);
        assert!(index.first_error.is_some());
        check_fde_index(
            &section,
            &bases,
            index.lookup(),
            &[0x1050, 0x1150, 0x1fff, 0x3050],
        );
    }

    #[test]
    fn loading_a_large_program_allocates_in_proportion_to_its_debug_information() {
        use crate::test_memory::memory_cap::{start_totals, stop_totals, totals};
        use object::{Object, ObjectSection};

        for fixture in ["gofmt-go-o0", "gofmt-go-o2"] {
            let path = Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("build/test-programs")
                .join(fixture);
            let data = fs::read(&path).expect("run `just build-test-programs`");
            let object = object::File::parse(&*data).expect("ELF");
            let debug = object
                .sections()
                .filter(|section| section.name().is_ok_and(|name| name.starts_with(".debug_")))
                .map(|section| {
                    section
                        .compressed_data()
                        .expect("a section")
                        .uncompressed_size
                })
                .sum::<u64>();

            start_totals();
            let before = totals();
            let info = load_bytes(
                &path,
                &data,
                crate::ModuleImageId::new(0),
                &super::super::DebugFileSearch::default(),
            )
            .expect("load");
            let used = totals().since(&before).allocated;
            stop_totals();
            let (blocks, bytes) = (used.blocks, used.bytes);
            assert!(info.image.functions().len() > 4000, "{fixture}");
            let work = format!("{fixture}: {debug} debug bytes: {blocks} blocks, {bytes} bytes");
            assert!(blocks <= debug * 7 / 10, "{work}");
            assert!(bytes <= debug * 150, "{work}");
        }
    }

    #[test]
    fn async_bodies_link_by_name_to_the_one_coroutine_they_run() {
        let function = |id: u32, coroutine: Option<u32>| FunctionInfo {
            id: FunctionId::new(id),
            name: "body".into(),
            linkage_name: None,
            declaration: None,
            language: SourceLanguage::Rust,
            role: crate::CodeRole::Ordinary,
            enclosing: None,
            coroutine: coroutine.map(crate::TypeId::new),
            generics: Arc::from([]),
            main_subprogram: false,
        };
        let coroutine_type = |id: u32, path: &[&str], name: &str| {
            crate::TypeNode::Resolved(crate::TypeInfo {
                reference: crate::TypeReference {
                    image: crate::ModuleImageId::new(0),
                    id: crate::TypeId::new(id),
                },
                name: name.into(),
                byte_size: Some(16),
                kind: crate::TypeKind::Opaque {
                    description: "a coroutine".into(),
                },
                identity: Some(Arc::new(crate::TypeIdentity {
                    language: SourceLanguage::Rust,
                    path: path.iter().map(|&part| part.into()).collect(),
                    inline_namespaces: Arc::new([]),
                    base: name.into(),
                    arguments: Arc::new([]),
                    pack: None,
                    origin: crate::ArgumentOrigin::Dwarf,
                    go: None,
                })),
            })
        };
        let info = || crate::CoroutineInfo {
            kind: crate::CoroutineKind::AsyncFunction,
            state: crate::StateMember { offset: 0, size: 1 },
            states: Arc::new([]),
            captures: Arc::new([]),
        };

        let types = vec![
            coroutine_type(0, &["steps", "leaf"], "{async_fn_env#0}"),
            coroutine_type(1, &["steps", "leaf"], "{async_fn_env#0}<u32>"),
            coroutine_type(2, &["steps", "twice"], "{async_fn_env#0}"),
            coroutine_type(3, &["steps", "twice"], "{async_fn_env#0}"),
            coroutine_type(4, &["steps", "broken"], "{async_fn_env#0}"),
            coroutine_type(5, &["steps", "linked"], "{async_fn_env#0}"),
        ];
        let coroutines = BTreeMap::from_iter([
            (crate::TypeId::new(0), Ok(info())),
            (crate::TypeId::new(1), Ok(info())),
            (crate::TypeId::new(2), Ok(info())),
            (crate::TypeId::new(3), Ok(info())),
            (
                crate::TypeId::new(4),
                Err(Arc::from("its layout is damaged")),
            ),
            (crate::TypeId::new(5), Ok(info())),
        ]);
        let mut functions = (0..6).map(|id| function(id, None)).collect::<Vec<_>>();
        functions[5].coroutine = Some(crate::TypeId::new(0));
        let futures = [
            (FunctionId::new(0), "steps::leaf::{async_fn_env#0}"),
            (FunctionId::new(1), "steps::leaf::{async_fn_env#0}<u32>"),
            (FunctionId::new(2), "steps::twice::{async_fn_env#0}"),
            (FunctionId::new(3), "steps::broken::{async_fn_env#0}"),
            (FunctionId::new(4), "steps::absent::{async_fn_env#0}"),
            (FunctionId::new(5), "steps::linked::{async_fn_env#0}"),
        ]
        .map(|(function, name)| (function, Arc::<str>::from(name)));

        link_bodies_by_name(&mut functions, &futures, &types, &coroutines);
        let linked = functions
            .iter()
            .map(|function| function.coroutine.map(crate::TypeId::get))
            .collect::<Vec<_>>();
        assert_eq!(linked, [Some(0), Some(1), Some(2), None, None, Some(0)]);
    }
}
