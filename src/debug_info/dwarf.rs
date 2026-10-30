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

    /// An error and the part of the DWARF it was found in.
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

    /// More of something than an image numbers with its `u32` ids.
    #[error("the debug information has more {0} than an image can hold")]
    TooMany(&'static str),

    #[error("the debug information does not fit an image: {0}")]
    Seal(Box<dyn std::error::Error + Send + Sync>),
}

impl DwarfError {
    /// Says where in the DWARF the error was found, unless it already says,
    /// or is about the load as a whole.
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

/// An architecture's name, as its users know it.
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

/// Names a unit for a reader: the source file it compiles, when it names
/// one, and where it is.
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

/// Parses an ELF file, saying what is wrong with one that cannot be: that
/// it is not ELF, or is cut short of what its headers describe.
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

/// Where an ELF file's header, program headers, and section headers end,
/// as its header says; `None` when the header is malformed.
fn elf_headers_end(data: &[u8]) -> Option<u64> {
    let little = match data.get(5)? {
        1 => true,
        2 => false,
        _ => return None,
    };

    // The header's size and its fields' offsets differ by class.
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
        Some((0..size).fold(0, |value, index| {
            let byte = bytes[if little { size - 1 - index } else { index }];
            value << 8 | u64::from(byte)
        }))
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

/// Checks that an object is one whose code and DWARF a debugger can
/// describe as they are: a linked program or library, for a supported
/// architecture.
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

/// Whether DWARF sections have relocations, which only a linker applies.
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

/// An image's units, with where each `.debug_info` unit lies, so that a
/// reference across units finds its unit by a binary search. A dwz
/// supplementary file's units follow the file's own.
#[expect(
    clippy::struct_field_names,
    reason = "the units are what the catalog lists"
)]
struct Units<'data> {
    units: Vec<gimli::Unit<Reader<'data>>>,

    /// The `.debug_info` offset of each of the file's own units and its
    /// index, by offset. DWARF 4's type units are in `.debug_types`, which
    /// `.debug_info` references never name.
    starts: Vec<(usize, usize)>,

    /// The same for the supplementary file's units.
    supplementary_starts: Vec<(usize, usize)>,

    /// The index of the supplementary file's first unit.
    first_supplementary: usize,

    /// The language each unit without its own takes from the units that
    /// refer to it, when they agree; empty when no unit is partial.
    inherited_languages: Vec<Option<gimli::DwLang>>,
}

/// A depth-first walk of one unit's DIEs that decodes only the DIEs its
/// caller asks for.
///
/// Each DIE's depth, offset, and tag come from its abbreviation, without
/// its attributes. Most walks act on a few tags, while most DIEs in a Rust
/// or C++ program are members, parameters, and template arguments of
/// types: decoding every attribute of every DIE, as gimli's cursor does,
/// cost each of several walks over every unit far more than skipping them.
/// A decoded DIE is read into one entry the walk reuses.
pub(super) struct DieWalk<'a, 'data> {
    raw: gimli::EntriesRaw<'a, Reader<'data>>,
    entry: gimli::DebuggingInformationEntry<Reader<'data>>,

    /// The DIE [`Self::next`] returned, until it is decoded or skipped.
    pending: Option<(&'a gimli::Abbreviation, WalkedDie)>,
}

/// Where one DIE of a [`DieWalk`] is, and what it is.
#[derive(Debug, Clone, Copy)]
pub(super) struct WalkedDie {
    /// The DIE's depth below the unit's root, which is at zero.
    pub(super) depth: isize,
    pub(super) offset: gimli::UnitOffset,
    pub(super) tag: gimli::DwTag,
}

impl<'a, 'data> DieWalk<'a, 'data> {
    /// A walk of every DIE of `unit`, from its root.
    pub(super) fn new(unit: &'a gimli::Unit<Reader<'data>>) -> gimli::Result<Self> {
        Ok(Self {
            raw: unit.entries_raw(None)?,
            entry: gimli::DebuggingInformationEntry::null(),
            pending: None,
        })
    }

    /// The next DIE, skipping the attributes of the one before unless it
    /// was decoded, or `None` after the last.
    pub(super) fn next(&mut self) -> gimli::Result<Option<WalkedDie>> {
        if let Some((abbreviation, _)) = self.pending.take() {
            self.raw.skip_attributes(abbreviation.attributes())?;
        }

        // Null entries end children, and pad the unit after its root's.
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

    /// Decodes the DIE [`Self::next`] last returned.
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

/// A DIE reference that leaves its unit: to a `.debug_info` offset in the
/// unit's own file, or in the supplementary file.
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

    /// A file's units, then the units of its dwz supplementary file that
    /// they reach: through the references their DIEs make, and the
    /// references those units make in turn. A supplementary file serves
    /// every file of a package, most of which the file never uses.
    ///
    /// dwz moves what units share into partial units. A partial unit
    /// without a compilation directory or a language takes those of the
    /// units referring to it, when they agree: dwz merges what names a file
    /// by a relative path only among units that share their directory.
    fn load(
        dwarf: &gimli::Dwarf<Reader<'data>>,
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

        // Where each supplementary unit lies, in offset order.
        let header_at = |offset: gimli::DebugInfoOffset| {
            let after = headers.partition_point(|header| {
                header
                    .debug_info_offset()
                    .is_some_and(|start| start.0 <= offset.0)
            });
            let index = after.checked_sub(1)?;
            let header = &headers[index];
            let start = header.debug_info_offset()?.0;
            (offset.0 - start < header.length_including_self()).then_some(index)
        };

        let mut outward = first_error(units.par_iter().map(outward_references).collect())?;
        let mut reached = vec![false; headers.len()];
        let mut wave = Vec::new();
        let mut reach =
            |references: &[Outward], within_supplementary: bool, wave: &mut Vec<usize>| {
                for reference in references {
                    let offset = match *reference {
                        Outward::Supplementary(offset) => offset,
                        Outward::Own(offset) if within_supplementary => offset,
                        Outward::Own(_) => continue,
                    };

                    if let Some(index) = header_at(offset)
                        && !std::mem::replace(&mut reached[index], true)
                    {
                        wave.push(index);
                    }
                }
            };

        for references in &outward {
            reach(references, false, &mut wave);
        }

        let mut supplementary_units = Vec::new();

        while !wave.is_empty() {
            let loaded = first_error(
                std::mem::take(&mut wave)
                    .into_par_iter()
                    .map(|index| {
                        let unit = supplementary.unit(headers[index])?;
                        let references = outward_references(&unit)?;
                        Ok::<_, DwarfError>((index, unit, references))
                    })
                    .collect(),
            )?;
            for (index, unit, references) in loaded {
                reach(&references, true, &mut wave);
                supplementary_units.push((index, unit, references));
            }
        }

        supplementary_units.sort_unstable_by_key(|(index, ..)| *index);
        let first_supplementary = units.len();

        for (_, unit, references) in supplementary_units {
            units.push(unit);

            // The supplementary file's references stay within it, and name
            // nothing in another file.
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
        this.inherit(&outward);
        Ok(this)
    }

    /// The units of a file followed by those of its supplementary file,
    /// which begin at `first_supplementary`.
    fn catalog(units: Vec<gimli::Unit<Reader<'data>>>, first_supplementary: usize) -> Self {
        let starts_of = |units: &[gimli::Unit<Reader<'data>>], first: usize| {
            let mut starts = units
                .iter()
                .enumerate()
                .filter_map(|(index, unit)| {
                    Some((unit.header.debug_info_offset()?.0, first + index))
                })
                .collect::<Vec<_>>();
            starts.sort_unstable();
            starts
        };

        Self {
            starts: starts_of(&units[..first_supplementary], 0),
            supplementary_starts: starts_of(&units[first_supplementary..], first_supplementary),
            units,
            first_supplementary,
            inherited_languages: Vec::new(),
        }
    }

    /// Gives each unit without a compilation directory or language those
    /// of the units referring to it, given each unit's `outward`
    /// references, in which a supplementary unit's name only its own file,
    /// when they agree. Units take them in rounds outward from
    /// the units that have their own, each from referrers settled in
    /// earlier rounds.
    fn inherit(&mut self, outward: &[Vec<Outward>]) {
        let count = self.units.len();
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

    /// The unit whose DIEs `offset` lies among, in the supplementary file's
    /// `.debug_info` or the file's own, and the offset within it.
    fn containing(&self, offset: gimli::DebugInfoOffset, supplementary: bool) -> Option<DieKey> {
        let starts = if supplementary {
            &self.supplementary_starts
        } else {
            &self.starts
        };

        let after = starts.partition_point(|(start, _)| *start <= offset.0);
        let (_, unit) = *starts.get(after.checked_sub(1)?)?;
        let within = offset.to_unit_offset(&self.units[unit].header)?;
        Some(DieKey {
            unit,
            offset: within.0,
        })
    }

    /// Whether the unit at `index` is the supplementary file's.
    const fn is_supplementary(&self, index: usize) -> bool {
        index >= self.first_supplementary
    }

    /// Each unit's group, named by its first unit: units whose DIEs refer
    /// to one another's, directly or through other units, share one, so a
    /// unit reaches only its group's DIEs.
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

    /// The language the unit at `index` takes from the units referring to
    /// it, having none of its own.
    fn inherited_language(&self, index: usize) -> Option<gimli::DwLang> {
        self.inherited_languages.get(index).copied().flatten()
    }

    /// The `.debug_info` offset of the DIE `key`, which expressions and
    /// call sites name DIEs by. The supplementary file's DIEs have none:
    /// their offsets are in another section.
    fn debug_info_offset(&self, key: DieKey) -> Option<u64> {
        if self.is_supplementary(key.unit) {
            return None;
        }

        gimli::UnitOffset(key.offset)
            .to_debug_info_offset(&self.units[key.unit].header)
            .map(|offset| offset.0 as u64)
    }
}

/// Whether dwz made the unit to hold what other units share, by its root's
/// tag alone, whose attributes every load reads later.
fn is_partial_unit(unit: &gimli::Unit<Reader<'_>>) -> bool {
    matches!(unit.header.type_(), gimli::UnitType::Partial)
        || unit
            .entries_raw(None)
            .and_then(|mut entries| entries.read_abbreviation())
            .ok()
            .flatten()
            .is_some_and(|root| root.tag() == gimli::DW_TAG_partial_unit)
}

/// The references a unit's DIEs make outside it, each once, in order.
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

/// The DWARF whose sections a unit's attributes name: the supplementary
/// file's for a unit that file holds, else the file's own. A unit holds
/// its DIEs' bytes, which lie in one file's `.debug_info`.
fn unit_dwarf<'a, 'data>(
    dwarf: &'a gimli::Dwarf<Reader<'data>>,
    unit: &gimli::Unit<Reader<'data>>,
) -> &'a gimli::Dwarf<Reader<'data>> {
    use gimli::Section as _;
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

/// The value every one of `values` that has one agrees on.
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

/// The executable address ranges of an image.
///
/// When a linker discards a function, through `--gc-sections` or by merging
/// duplicate template instances, it points the function's debug information
/// at address 0 or a tombstone near `u64::MAX`. Those ranges lie outside
/// every executable section, which is how they are told apart from code.
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

/// Reads the code ranges a DIE covers, dropping empty ranges and the stubs
/// of discarded functions that lie outside the image's code.
fn die_code_ranges<'data>(
    dwarf: &gimli::Dwarf<Reader<'data>>,
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
        // A constant high_pc is an offset from low_pc. gimli would add it
        // unchecked, which overflows for a tombstone low_pc.
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
mod variables;

pub(in crate::debug_info) use variables::{PathStep, array_byte_offset};

#[cfg(feature = "fuzzing")]
pub(super) fn fuzz_expression(data: &[u8]) {
    variables::fuzz_expression(data);
}

/// Unwinds by the call-frame sections and Go's table an image keeps.
struct DwarfUnwindInfo {
    tables: Arc<crate::image::Image>,

    /// Go's function table, which unwinds Go code no call-frame information
    /// describes and says where Go frames saved the frame pointer, parsed
    /// on the first unwind that needs it.
    go: std::sync::OnceLock<Option<super::gopclntab::GoUnwind>>,

    /// The row found at each address unwound, by whether it is
    /// `.debug_frame`'s. Finding a row runs its entry's instructions from
    /// the start, and a tracer or sampler unwinds the same few addresses
    /// over and over.
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

/// Loads the debug information of an image that lives only as long as a
/// process keeps it in memory, as a JIT compiler's objects do, which no
/// other session would read from the cache.
pub fn load_transient_bytes(
    path: &Path,
    data: &[u8],
    image_id: crate::ModuleImageId,
) -> Result<DebugInfo> {
    let _load = crate::span!("load", "{}", path.display());
    let search = super::DebugFileSearch::default();
    load_bytes_on_pool(path, data, image_id, &search, None, true)
}

/// Loads an image's debug information on the loader's workers. A
/// `transient` image, as a JIT compiler's object, is small and loads on one
/// worker: a load on the shared pool wakes all of its workers.
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
        crate::pool::install(load)
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

/// What a load found in the cache.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum CacheOutcome {
    /// The load had no cache.
    Off,

    /// The cache held the image.
    Hit,

    /// The cache held no image, so the load built one and wrote it.
    Miss,

    /// The cache held an unusable image, which the load removed before
    /// building and writing another.
    Corrupt,
}

/// Loads an image's debug information. A file without DWARF of its own may
/// have a separate debug file, whose DWARF, symbols, and call-frame
/// information describe the code here. One that cannot be loaded leaves
/// the image as its own file describes it, and DWARF that cannot be read
/// leaves it as its symbols describe it, with the reason recorded. Only a
/// file that is no linked ELF image of a supported architecture fails.
fn load_debug_info(
    path: &Path,
    data: &[u8],
    image_id: crate::ModuleImageId,
    search: &super::DebugFileSearch,
    limits: LoadLimits,
    cache: Option<&crate::cache::ImageCache>,
) -> std::result::Result<(DebugInfo, CacheOutcome), DwarfError> {
    let object = parse_object(data)?;
    check_object(&object)?;
    let phase = crate::span!("separate_debug_file");
    let super::separate::Search {
        found: separate,
        rejected,
    } = search.find(path, &object);

    // The supplementary file is named by whichever file holds the DWARF.
    let supplementary = separate.as_ref().map_or_else(
        || search.supplementary(path, &object),
        |file| {
            object::File::parse(file.data.as_slice()).map_or(Supplementary::None, |debug| {
                search.supplementary(&file.path, &debug)
            })
        },
    );
    drop(phase);
    let binding = Binding {
        path: Arc::new(path.to_owned()),
        debug_path: separate
            .as_ref()
            .or_else(|| rejected.as_ref().map(|(file, _)| file))
            .map(|file| Arc::new(file.path.clone())),
        id: image_id,
    };

    let built = |tables| bind(&binding, Arc::new(tables)).map_err(DwarfError::Binding);
    let read = |separate: Option<&super::separate::DebugFile>, supplementary: &Supplementary| {
        let rejected = rejected
            .as_ref()
            .map(|(file, reason)| (file, Arc::<str>::from(reason.as_str())));
        read_debug_info(data, separate, rejected.as_ref(), supplementary, limits)
    };

    let Some(cache) = cache else {
        let read = read(separate.as_ref(), &supplementary)?;

        // What the image was read from is not needed to seal it.
        drop((separate, supplementary, rejected));
        return Ok((built(read.seal()?)?, CacheOutcome::Off));
    };

    let phase = crate::span!("cache.read");
    let mut inputs = vec![data];
    inputs.extend(separate.as_ref().map(|file| file.data.as_slice()));

    // A rejected file's reason is recorded, so its bytes key the image too.
    inputs.extend(rejected.as_ref().map(|(file, _)| file.data.as_slice()));
    if let Supplementary::Found(file) = &supplementary {
        inputs.push(file.data.as_slice());
    }

    let key = crate::cache::Key::of(&inputs);
    let outcome = match cache.get(key) {
        crate::cache::Lookup::Hit { image, stamp } => match bind(&binding, Arc::new(*image)) {
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
    if let Err(error) = cache.put(key, &tables) {
        crate::cache::report(format_args!("{error}"));
    }

    drop(phase);
    Ok((built(tables)?, outcome))
}

/// Reads what an image of a file's debug information holds, from the
/// separate debug file `separate` when one was found, with the dwz supplementary file its
/// DWARF shares. A separate debug file that cannot be loaded, or whose
/// supplementary file was not found, leaves the image as its own file
/// describes it, and DWARF of its own that cannot be read leaves it as its
/// symbols and call-frame information describe it, with the reasons
/// recorded. Without one, the file `rejected` names that describes another
/// module is recorded with why.
fn read_debug_info(
    data: &[u8],
    separate: Option<&super::separate::DebugFile>,
    rejected: Option<&(&super::separate::DebugFile, Arc<str>)>,
    supplementary: &Supplementary,
    limits: LoadLimits,
) -> std::result::Result<Read, DwarfError> {
    let none_found = || {
        rejected.map_or(Separate::None, |(file, reason)| {
            Separate::Unusable(&file.path, Arc::clone(reason))
        })
    };

    let image =
        |separate, supplementary, dwarf| load_image(data, separate, supplementary, dwarf, limits);

    // The file's own DWARF, or, when it cannot be read, none.
    let own = |separate: Separate<'_>, supplementary| {
        load_image(
            data,
            separate.clone(),
            supplementary,
            DwarfUse::Read,
            limits,
        )
        .or_else(|error| {
            load_image(
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

/// Whether a load reads the DWARF of the file that holds it.
enum DwarfUse {
    Read,

    /// It could not be read, for the reason given, so the image is loaded
    /// without it.
    Unusable(Arc<str>),
}

type Sections<'data> = DwarfSections<Cow<'data, [u8]>>;

/// An object's DWARF sections, those compressed decompressed in parallel.
/// Also returns their uncompressed bytes, which the load's budget follows.
fn load_sections<'data>(
    object: &object::File<'data>,
) -> std::result::Result<(Sections<'data>, u64), DwarfError> {
    // gimli names the sections it reads by asking for each in turn.
    let mut wanted = Vec::new();
    DwarfSections::load(|id| {
        wanted.push(id);
        Ok::<_, DwarfError>(())
    })?;
    let loaded = wanted
        .into_par_iter()
        .map(|id| {
            let data = match object.section_by_name(id.name()) {
                Some(section) => section.uncompressed_data()?,
                None => Cow::Borrowed(&[][..]),
            };

            Ok((id, data))
        })
        .collect::<Vec<std::result::Result<_, DwarfError>>>();
    let loaded = first_error(loaded)?;
    let input = loaded.iter().map(|(_, data)| data.len() as u64).sum();
    let mut loaded = loaded.into_iter().collect::<HashMap<_, _>>();
    let sections = DwarfSections::load(|id| {
        Ok::<_, DwarfError>(loaded.remove(&id).unwrap_or(Cow::Borrowed(&[])))
    })?;

    Ok((sections, input))
}

/// What a separate debug file contributes to an image.
#[derive(Clone)]
enum Separate<'a> {
    None,
    Used(&'a super::separate::DebugFile),

    /// One found but unusable, for the reason given.
    Unusable(&'a Path, Arc<str>),
}

/// What an image holds, read from its sources, which need not outlive it.
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
        DwarfUse::Read => load_sections(dwarf_object)?,
        DwarfUse::Unusable(_) => (
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

            load_sections(supplementary)
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

    // Units a compiler emitted together share their abbreviations: each
    // shared table is read and held once rather than for every unit, which
    // was half a gigabyte for 14,000 units of 2,800 ThinLTO modules.
    dwarf.populate_abbreviations_cache(gimli::AbbreviationsCacheStrategy::Duplicates);
    let mut files = Files::default();
    let mut line_tables = LineTables::default();
    let mut headers = Vec::new();

    // Where the next unit's header is, which a malformed one names.
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

    // Each unit's abbreviations, root DIE, and line program header, in
    // parallel and in order.
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
    let units = Units::load(&dwarf, units)?;
    let catalog = UnitCatalog {
        type_signatures: type_signature_index(&units)?,
        units,
        code: CodeRanges(super::elf::executable_ranges(&object)),
    };

    crate::count!("units", catalog.units.len());
    drop(phase);

    // Go's own function table, which the runtime reads and stripping keeps.
    let (mut go_table, mut runtime_function_table) = match super::gopclntab::load(&object) {
        Ok(Some(table)) => (Some(Arc::new(table)), EmbeddedSymbolTable::Loaded),
        Ok(None) => (None, EmbeddedSymbolTable::Absent),
        Err(error) => (None, unusable_table(&error)),
    };

    let phase = crate::span!("functions");
    let mut function_metadata =
        load_function_metadata(&dwarf, &catalog, go_table.as_deref(), &mut files)?;

    drop(phase);
    let phase = crate::span!("lines");

    // Each unit's line program, decoded in parallel with files of its own,
    // and appended in unit order: interning a unit's files in the order it
    // first names them gives every file the identifier a serial load would.
    let unit_lines = catalog
        .units
        .par_iter()
        .filter(|unit| !is_type_unit(unit))
        .map(|unit| {
            let mut files = Files::default();
            let mut tables = LineTables::default();
            load_lines(&dwarf, unit, &catalog.code, &mut files, &mut tables).map_err(|error| {
                error.within(|| format!("the line program of {}", describe_unit(unit)))
            })?;
            Ok::<_, DwarfError>((files, tables))
        })
        .collect::<Vec<_>>();
    let unit_lines = first_error(unit_lines)?;

    for (unit_files, tables) in unit_lines {
        let ids = unit_files
            .paths()
            .iter()
            .map(|path| files.intern(path.clone()))
            .collect::<Vec<_>>();
        line_tables.append(&tables, |file| ids[file.index()])?;
    }

    crate::count!("line_rows", line_tables.rows.len());
    drop(phase);

    // Code no DWARF describes, such as a stripped image's, gets functions
    // and lines from the function table.
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

    // Function and code instance identifiers index their tables.
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

    // The prologue and coroutine analyses below find statements by address.
    let statements = line_tables.statements_by_address();
    super::roles::link_loop_bodies(&mut function_metadata.functions);
    refine_proved_prologue_entries(
        &object,
        target,
        &statements,
        &mut function_metadata.code_instances,
    );

    drop(phase);

    // Unwinding and symbols need nothing the variables produce, and most of
    // the variables' work runs on one thread, so they are read beside it.
    let (variables, unwind_and_symbols) = rayon::join(
        || {
            let _phase = crate::span!("variables");
            let read = |files: &mut Files, locals| {
                variables::load_variable_info(
                    &dwarf,
                    &catalog,
                    target,
                    // The module's identifier is its binding's, not the image's.
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
                // Locals are most of what a large program's debug
                // information describes, and the rest may fit without them.
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
            let unwind = load_unwind_info(&object, debug_object.as_ref(), target, go_code, go)?;
            let symbols =
                super::elf::load_symbols(&object, debug_object.as_ref(), &unwind.function_ranges());

            Ok::<_, DwarfError>((unwind, symbols))
        },
    );

    // The variables' failure is reported first, as when they were read first.
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
            .map(|instance| instance.function)
        {
            function_metadata.functions[function.index()].generics = generics;
        }
    }

    let phase = crate::span!("resume_points");
    let coroutines = std::mem::take(&mut variables.coroutines);

    for (instance, ty) in &variables.coroutine_bodies {
        if let Some(Ok(_)) = coroutines.get(ty)
            && let Some(function) = function_metadata
                .code_instances
                .get(instance.index())
                .map(|instance| instance.function)
        {
            function_metadata.functions[function.index()].coroutine = Some(*ty);
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

    // Which variables an await holds is asked of the module's code.
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
            embedded_views: embedded_views(dwarf_object)?,
        },
    })
}

/// The debug information `tables` describe for the module `binding`
/// names: what a load sealed, or the same bytes read back from a cache.
fn bind(
    binding: &Binding,
    tables: Arc<crate::image::Image>,
) -> std::result::Result<DebugInfo, crate::image::facts::Unbound> {
    let image = Arc::new(ModuleImage::bind(binding, tables)?);
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

/// The variables of an image no DWARF describes: it has none, and says
/// so as a module stripped of its DWARF does.
pub(super) fn no_variables(image: &ModuleImage) -> Arc<dyn super::VariableInfo> {
    Arc::new(variables::DwarfVariableInfo::new(image))
}

/// The bytes of the views a module carries for its own types.
fn embedded_views(object: &object::File<'_>) -> std::result::Result<Vec<u8>, DwarfError> {
    Ok(object
        .section_by_name(crate::view::embedded::SECTION)
        .map(|section| section.uncompressed_data())
        .transpose()?
        .map(std::borrow::Cow::into_owned)
        .unwrap_or_default())
}

/// Which part of the unit `header` begins could not be read: its
/// abbreviations, its root entry, or its line program's header.
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

/// Where the unit after the one `header` begins.
fn unit_end(header: &gimli::UnitHeader<Reader<'_>>) -> usize {
    header
        .offset()
        .0
        .saturating_add(header.length_including_self())
}

/// Why the image's DWARF is incomplete when compile units keep it in split
/// DWARF files, which the debugger does not read: how many do, and the
/// first one's file.
fn split_units<'data>(
    dwarf: &gimli::Dwarf<Reader<'data>>,
    catalog: &UnitCatalog<'data>,
) -> std::result::Result<Option<Arc<str>>, DwarfError> {
    let (mut compile_units, mut split, mut first) = (0, 0, None);
    for unit in catalog.units.iter().filter(|unit| !is_type_unit(unit)) {
        compile_units += 1;
        if unit.dwo_id.is_some() {
            split += 1;
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
            "its compile unit is described in a split DWARF file{}, which this debugger does not \
             read; its functions are known only by their symbols, without variables or types",
            file("")
        )
    } else {
        format!(
            "{split} of its {compile_units} compile units are described in split DWARF files{}, which this \
             debugger does not read; their functions are known only by their symbols, without \
             variables or types",
            file("such as ")
        )
    };

    Ok(Some(reason.into()))
}

/// The producers the units name, in unit order.
fn unit_producers<'data>(
    dwarf: &gimli::Dwarf<Reader<'data>>,
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

/// Go's attribute naming the package a unit compiles, which its
/// `DW_AT_name` names by import path.
const DW_AT_GO_PACKAGE_NAME: gimli::DwAt = gimli::DwAt(0x2905);

/// The Go packages the image has units for, with the names their code
/// declares.
fn go_packages(
    dwarf: &gimli::Dwarf<Reader<'_>>,
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

/// Gives each symbol naming a Go function's entry the role the function
/// table records for it. Its name is the table's, without the ELF symbol's
/// ABI suffix.
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

/// Returns the code ranges of every unit written in Go, merged and sorted
/// by start address. A Go unit's ranges also cover the assembly functions
/// of its package, which follow the same calling convention.
fn go_code_ranges<'data>(
    dwarf: &gimli::Dwarf<Reader<'data>>,
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

    // Merging lets a lookup check only the last range starting at or before
    // an address.
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

/// The results' values, or the first one's error: what a serial run
/// would have stopped at, whichever order parallel work finished in.
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

    // An image with no program headers, as a JIT compiler writes one to
    // memory, lies where its allocated sections say.
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
) -> std::result::Result<crate::image::unwind::Unwind, DwarfError> {
    let data_in = |object: &object::File<'_>, name| -> std::result::Result<Arc<[u8]>, DwarfError> {
        Ok(object
            .section_by_name(name)
            .as_ref()
            .map(ObjectSection::uncompressed_data)
            .transpose()?
            .unwrap_or_default()
            .into())
    };

    let section_data = |name| data_in(object, name);

    // A separate debug file may hold the `.debug_frame` the module's file
    // was stripped of.
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
        unwind.proved_frames = proved_frames(object, debug_object, &unwind);
    }

    Ok(unwind)
}

/// The frames of code no call-frame information describes, as its
/// instructions prove them: V8's builtins, by V8's conventions too, and,
/// in a module whose call-frame information leaves most of its code
/// undescribed, as Google's build of Chrome does, every function its own
/// code or relocations reach. A separate debug file may name the builtins
/// its module's file was stripped of.
#[cfg(target_arch = "x86_64")]
fn proved_frames(
    object: &object::File<'_>,
    debug_object: Option<&object::File<'_>>,
    unwind: &crate::image::unwind::Unwind,
) -> Vec<(AddressRange<ImageAddress>, u32)> {
    let mut builtins = super::v8::builtins(object);
    if builtins.is_empty()
        && let Some(debug_object) = debug_object
    {
        // The debug file holds the symbols; the module's file, the code.
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

    if sparsely_described(object, unwind) {
        undescribed_frames(object, unwind, &described, &starts, &mut frames);
    }
    frames
}

/// The frames of V8's builtins and of V8's other hand-written code, whose
/// starts are added to `starts`.
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

    // V8's hand-written code that is not a builtin, as the heap's scan of
    // the stack (`PushAllRegistersAndIterateStack`), has no call-frame
    // information either, and every collection samples under it. Its
    // instructions alone prove its frames: it keeps no convention.
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
        // A symbol of no size, as Node 22's build gives the scan, runs at
        // most to the next; its paths end at its return.
        let end = match size {
            0 => match functions.get(at + 1) {
                Some((next, _)) => *next,
                None => continue,
            },
            size => address + size,
        };
        if let Some(code) = code_bytes(object, address, end) {
            super::x86_64::frames::runtime_frames(code, address, false, frames);
            starts.insert(address);
        }
    }
}

/// Whether call-frame information describes less than half of the code
/// of `object`'s executable sections.
#[cfg(target_arch = "x86_64")]
fn sparsely_described(object: &object::File<'_>, unwind: &crate::image::unwind::Unwind) -> bool {
    let code: u64 = object
        .sections()
        .filter(|section| section.kind() == object::SectionKind::Text)
        .map(|section| section.size())
        .sum();
    let described: u64 = [&unwind.eh_frame_index, &unwind.debug_frame_index]
        .iter()
        .flat_map(|index| index.entries.iter())
        .map(|entry| entry.end.get().saturating_sub(entry.start.get()))
        .sum();
    described.saturating_mul(2) < code
}

/// The frames of the functions of `object` that no call-frame information
/// describes and that `starts` does not already hold, each found at an
/// address its code calls or loads, a relocation holds, or a symbol
/// names, and analyzed up to the next function's start.
#[cfg(target_arch = "x86_64")]
fn undescribed_frames(
    object: &object::File<'_>,
    unwind: &crate::image::unwind::Unwind,
    described: &(dyn Fn(u64) -> bool + Sync),
    starts: &std::collections::BTreeSet<u64>,
    frames: &mut Vec<(AddressRange<ImageAddress>, u32)>,
) {
    let sections: Vec<(u64, &[u8])> = object
        .sections()
        .filter(|section| section.kind() == object::SectionKind::Text)
        .filter_map(|section| Some((section.address(), section.data().ok()?)))
        .collect();
    let in_code = |address: u64| {
        sections
            .iter()
            .any(|(start, code)| *start <= address && address - start < code.len() as u64)
    };

    let mut entries: Vec<u64> = sections
        .par_iter()
        .flat_map_iter(|(address, code)| super::x86_64::frames::code_entries(code, *address))
        .collect();
    entries.extend(
        object
            .symbols()
            .chain(object.dynamic_symbols())
            .filter(|symbol| symbol.kind() == object::SymbolKind::Text)
            .map(|symbol| symbol.address()),
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
    entries.retain(|&address| in_code(address));

    // Where each function ends: at the next start of any function,
    // described or not.
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

    let found: Vec<Vec<(AddressRange<ImageAddress>, u32)>> = entries
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
            // Google's Chrome describes only the prologue of many functions,
            // in an entry of a few bytes; the rest of such a function is
            // proved, and unwinding prefers an entry wherever one exists.
            if described(address) && described(end.saturating_sub(1)) {
                return None;
            }
            let offset = usize::try_from(address - start).ok()?;
            let length = usize::try_from(end - address).ok()?;
            let code = code.get(offset..offset.checked_add(length)?)?;
            // A function that begins by linking its frame was built to keep
            // the chain, so each call its paths do not reach, as the cases
            // of a jump table, keeps it too.
            let convention = code.starts_with(&super::x86_64::frames::PROLOGUE);
            let mut frames = Vec::new();
            super::x86_64::frames::runtime_frames(code, address, convention, &mut frames);
            Some(frames)
        })
        .collect();
    frames.extend(found.into_iter().flatten());
}

/// The instructions of `section`'s functions at which registers their
/// callers keep are already restored, though the rules still name the
/// slots they were saved in, each with the registers as bits of
/// [`X86_64_CALLEE_SAVED_REGISTERS`]: GCC's epilogues pop them and leave
/// those rules, and a sample's copy of the stack begins above the slots.
/// Only a row whose frame is gone, its return address at the stack
/// pointer, that still names such a slot is read, from its first
/// instruction, which a row always begins at. Code that runs straight to
/// its return holds every one; otherwise the `pop` or `leave` that freed
/// the frame, the last instruction of the row before, restored the
/// register it loaded from the slot the row names, until code writes it
/// or may go elsewhere.
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

        // The row before: where it starts, and whether its frame is gone.
        let mut before: Option<(u64, bool)> = None;

        while let Ok(Some(row)) = rows.next_row() {
            let (start, end) = (row.start_address(), row.end_address());
            let gone = matches!(
                row.cfa(),
                CfaRule::RegisterAndOffset { register, offset: 8 } if register.0 == X86_64_RSP
            );
            // The registers the row names slots of, and those whose slot
            // is just below the return address.
            let (mut slotted, mut below) = (0_u32, 0_u32);
            for (register, rule) in row.registers() {
                let RegisterRule::Offset(offset) = rule else {
                    continue;
                };
                if let Some(index) = X86_64_CALLEE_SAVED_REGISTERS
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

                    // A pop of the slot just below the return address.
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

/// The call-frame sections as gimli reads them.
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

/// Indexes the frame description entries of one call-frame section by
/// address, so that finding an address's entry is a binary search rather
/// than a walk of the section. Lookups agree exactly with gimli's walk
/// (`UnwindSection::fde_for_address`), which returns the first entry in
/// section order that contains the address, or the first error before it:
/// the index keeps the offset of the first entry that fails to parse, or
/// [`WHOLE_SECTION`](crate::image::unwind::WHOLE_SECTION) when the section
/// itself is malformed and enumeration stops.
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
                // A section too large for the image fails when sealed.
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
    /// Returns the code range of every function the call-frame information
    /// describes. Enumeration stops at the first malformed entry, so the
    /// result is evidence of function boundaries rather than a complete map.
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

                // The loader parsed these same bytes.
                let table = super::gopclntab::GoTable::reparse(bytes.into(), facts).ok()?;
                Some(super::gopclntab::GoUnwind::new(
                    Arc::new(table),
                    view.frame_saves().collect(),
                ))
            })
            .as_ref()
    }

    /// The row at `address` of `.debug_frame` if `debug`, else of
    /// `.eh_frame`, as `find` finds it, found once.
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

    /// Returns the registers the function at `address` may overwrite
    /// without saving them, by the calling convention it follows. Go's
    /// lets a callee overwrite registers the System V ABI preserves.
    fn call_clobbered_registers(&self, address: ImageAddress) -> &'static [DwarfRegister] {
        if self.view().is_go_code(address) || self.go().is_some_and(|go| go.is_go(address)) {
            &X86_64_GO_CALL_CLOBBERED_REGISTERS
        } else {
            &X86_64_SYSV_CALL_CLOBBERED_REGISTERS
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
        // Only an epilogue whose saved registers lie below the copy needs
        // this, so it is looked up only then.
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
        // Go's rules find the frame by SP, but its prologue keeps the chain.
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

/// The frame proved for the instruction at `address` of runtime code no
/// call-frame information describes.
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

/// The call-frame row in effect at `address`, and the entry holding it.
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

/// What unwinding a frame at one address needs of the call-frame
/// information: the row in effect there, with only the rules it has, and
/// the facts of the entry that holds it.
#[derive(Debug)]
struct FrameRow {
    cfa: CfaRule<usize>,
    rules: Box<[(gimli::Register, RegisterRule<usize>)]>,
    return_register: DwarfRegister,
    encoding: Encoding,
    signal_frame: bool,
}

/// The rows an image's call-frame information was asked for most, past
/// which a row is found again each time it is needed.
const MOST_FRAME_ROWS: usize = 1 << 16;

/// The row of `section` in effect at `address`.
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

/// The caller of the frame whose `registers` `row` describes.
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

    // A callee may overwrite every register its calling convention does not
    // preserve across calls, so the caller's value survives only where the
    // row says where it was saved. Keeping the callee's value would present
    // it as the caller's.
    caller.retain(|register| {
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
            // A register saved where memory cannot be read is unknown in
            // the caller, as an undefined one is: an epilogue's rules may
            // still name a slot it popped, below a sample's copy. Where the
            // code proves it restored the registers its caller keeps, the
            // caller's value is the one the register holds. Only the
            // return address, without which there is no caller, ends the
            // unwind.
            Err(UnwindTermination::MemoryReadFailed { .. }) if register != return_register => {
                let proved = X86_64_CALLEE_SAVED_REGISTERS
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

    caller.set(crate::unwind::RSP, cfa.get());

    if caller.get(return_register).is_none() {
        return Err(UnwindTermination::Complete);
    }

    Ok(UnwindStep {
        registers: caller,
        cfa,
        signal_frame: row.signal_frame,
    })
}

/// The canonical frame address of a PLT stub that has pushed `pushed`
/// words since its call, which pushed the return address.
fn stub_cfa(
    pushed: u32,
    registers: &RegisterFile,
) -> std::result::Result<VirtualAddress, UnwindTermination> {
    let stack = registers
        .get(crate::unwind::RSP)
        .ok_or_else(|| register_unavailable(crate::unwind::RSP))?;
    let cfa = (u64::from(pushed) + 1)
        .checked_mul(8)
        .and_then(|bytes| stack.checked_add(bytes))
        .ok_or_else(|| UnwindTermination::InvalidCaller {
            description: "a PLT stub's frame ends past the address space".into(),
        })?;
    Ok(VirtualAddress::new(cfa))
}

/// The caller of a PLT stub that has pushed `pushed` words since its call.
/// The stub changes no register its caller keeps, but the call may change
/// any it does not.
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

    caller.set(crate::unwind::RETURN_ADDRESS, return_address);
    caller.set(crate::unwind::RSP, cfa.get());
    Ok(UnwindStep {
        registers: caller,
        cfa,
        signal_frame: false,
    })
}

/// The DWARF numbers of the registers the x86-64 System V ABI lets a callee
/// overwrite: rax, rdx, rcx, rsi, rdi, r8-r11, rflags, the SSE registers,
/// and the x87 stack.
const X86_64_SYSV_CALL_CLOBBERED_REGISTERS: [DwarfRegister; 34] = DwarfRegister::each([
    0, 1, 2, 4, 5, 8, 9, 10, 11, 49, 17, 18, 19, 20, 21, 22, 23, 24, 25, 26, 27, 28, 29, 30, 31,
    32, 33, 34, 35, 36, 37, 38, 39, 40,
]);

/// The DWARF numbers of the registers besides rsp the x86-64 System V ABI
/// has a function return with as its caller left them: rbx, rbp, and
/// r12-r15, in the order whose bits a restored return names them by.
const X86_64_CALLEE_SAVED_REGISTERS: [DwarfRegister; 6] =
    DwarfRegister::each([3, 6, 12, 13, 14, 15]);

/// The DWARF number of x86-64's stack pointer.
const X86_64_RSP: u16 = 7;

/// The DWARF numbers of the registers Go code may overwrite: every System V
/// call-clobbered register, and rbx, rbp, and r12-r15 too. Go's register ABI
/// preserves none of them across calls, assembly functions may overwrite the
/// goroutine pointer in r14, and Go's call-frame information does not
/// describe the frame pointer a prologue saves.
const X86_64_GO_CALL_CLOBBERED_REGISTERS: [DwarfRegister; 40] = DwarfRegister::each([
    0, 1, 2, 3, 4, 5, 6, 8, 9, 10, 11, 12, 13, 14, 15, 49, 17, 18, 19, 20, 21, 22, 23, 24, 25, 26,
    27, 28, 29, 30, 31, 32, 33, 34, 35, 36, 37, 38, 39, 40,
]);

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

// Bounds unwind-expression evaluation so a malformed expression with a
// backward branch cannot hang the controller thread.
const MAX_UNWIND_EXPRESSION_ITERATIONS: u32 = 10_000;

/// What an unwind expression computed: an address, or with
/// `DW_OP_stack_value`, a value.
enum Evaluated {
    Address(u64),
    Value(u64),
}

/// Evaluates a CFA or register rule's expression. A register rule's
/// expression starts with the CFA on its stack, as `initial`.
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

/// What a row's register rules are applied with.
struct RuleContext<'a, S> {
    /// The registers of the frame being unwound.
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
            // The register was saved where the expression computes.
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
            // The register's value is what the expression computes.
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

/// A map from DIEs, kept per unit.
///
/// The variables walk reads one unit at a time, and nearly every reference
/// stays in its unit, so the map of the unit being read stays in cache,
/// where one map of every DIE in a large program misses on most lookups.
/// A map holds the units from the least it has a DIE of to the greatest:
/// one unit's walk names few, and a map for each of 14,000 units cost
/// every walk's maps megabytes of empty ones.
#[derive(Clone)]
struct DieMap<V> {
    /// The unit of the first map.
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

    /// The map of `unit`, made with those between it and the others.
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

    /// Replaces each value with `map` of it.
    fn map_values(&mut self, mut map: impl FnMut(V) -> V)
    where
        V: Copy,
    {
        for unit in &mut self.units {
            #[expect(
                clippy::disallowed_methods,
                clippy::iter_over_hash_type,
                reason = "each value is replaced on its own, whatever the order"
            )]
            for value in unit.values_mut() {
                *value = map(*value);
            }
        }
    }

    fn remove(&mut self, key: &DieKey) -> Option<V> {
        let index = key.unit.checked_sub(self.first)?;
        self.units.get_mut(index)?.remove(&key.offset)
    }

    /// Every entry, in no order.
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

    /// Adds every entry of `other`, whose entries replace these.
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

    /// The language of the unit holding the DIE.
    language: SourceLanguage,
    kind: RawFunctionKind,
    parent: Option<DieKey>,
    abstract_origin: Option<DieKey>,
    specification: Option<DieKey>,
    name: Option<Arc<str>>,
    linkage_name: Option<Arc<str>>,

    /// Whether the DIE says the code only forwards to another function.
    trampoline: bool,

    /// Whether the DIE says it is the program's main subprogram.
    main_subprogram: bool,
    declaration: Option<SourceLocation>,
    call_site: Option<SourceLocation>,

    /// Its code, held once for its code instance too.
    ranges: Arc<[AddressRange<ImageAddress>]>,
    entry: Option<ImageAddress>,

    /// The names of the namespaces enclosing the DIE, outermost first,
    /// joined by `::`, as Rust's debug information nests its functions.
    namespace: Option<Arc<str>>,

    /// The type the function returns.
    returns: Option<DieKey>,
}

struct FunctionMetadata {
    functions: Vec<FunctionInfo>,
    code_instances: Vec<CodeInstanceInfo>,

    /// Maps each concrete function DIE to its code instance so the variable
    /// catalog can attribute scopes to logical frames.
    instance_ids: DieMap<CodeInstanceId>,

    /// Each Rust async body, with the full name of the coroutine type that
    /// rustc's naming says it runs (see [`super::coroutines::body_future`]):
    /// how a body is linked to its coroutine where its debug information
    /// does not otherwise say.
    body_futures: Vec<(FunctionId, Arc<str>)>,
}

fn load_function_metadata<'data>(
    dwarf: &gimli::Dwarf<Reader<'data>>,
    catalog: &UnitCatalog<'data>,
    go_table: Option<&super::gopclntab::GoTable>,
    files: &mut Files,
) -> std::result::Result<FunctionMetadata, DwarfError> {
    let phase = crate::span!("functions.collect");
    let (raw, futures) = collect_function_dies(dwarf, catalog, files)?;
    crate::count!("function_dies", raw.len());
    drop(phase);
    let _phase = crate::span!("functions.resolve");

    // Each function's definition: where its origin and specification
    // chains end, as an index into `raw`.
    let definitions = first_error(
        raw.par_iter()
            .map(|function| definition_of(function.key, &raw))
            .collect(),
    )?;

    // The definitions code belongs to. Clang also emits subprograms with
    // no code and no name, only to scope a function's local types.
    let mut concrete = vec![false; raw.len()];
    for (function, definition) in raw.iter().zip(&definitions) {
        if !function.ranges.is_empty() {
            concrete[*definition] = true;
        }
    }

    // Each definition once, in the order DIEs first name it.
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

            // Clang names the thunks a multiply inherited virtual function
            // needs only by their linkage names.
            function_name(origin).map(|name| {
                let role = origin_role(origin, &name, &futures);
                (name, role)
            })
        })
        .collect::<Vec<_>>();
    let mut functions = Vec::new();
    let mut trampolines = Vec::new();
    let mut body_futures = Vec::new();
    let mut function_ids = vec![None; raw.len()];
    let mut nameless = vec![false; raw.len()];
    for (definition, named) in order.into_iter().zip(named) {
        let Some((name, role)) = named else {
            // Clang also emits code with no name of any kind, as some
            // subprograms Chromium's split DWARF has: its code keeps its
            // symbol's name, and what it inlined, theirs.
            nameless[definition] = concrete[definition];
            continue;
        };

        let origin = &raw[definition];
        let id = FunctionId::new(
            u32::try_from(functions.len()).map_err(|_| gimli::Error::UnsupportedOffset)?,
        );
        trampolines.push(origin.trampoline);
        if origin.language == SourceLanguage::Rust
            && let Some(future) = origin
                .name
                .as_deref()
                .zip(origin.namespace.as_deref())
                .and_then(|(name, namespace)| super::coroutines::body_future(name, namespace))
        {
            body_futures.push((id, Arc::from(future)));
        }

        functions.push(FunctionInfo {
            id,
            name,
            linkage_name: origin.linkage_name.clone(),
            declaration: origin.declaration.clone(),
            language: origin.language,
            role,
            enclosing: None,
            coroutine: None,
            generics: Arc::from([]),
            main_subprogram: origin.main_subprogram,
        });
        function_ids[definition] = Some(id);
    }

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

    Ok(FunctionMetadata {
        functions,
        code_instances,
        instance_ids,
        body_futures,
    })
}

/// Each function DIE with code as an instance, numbered in DIE order, of
/// its definition's function; and the instance each such DIE is. Code of a
/// definition with no name is no instance, and keeps its symbol's.
fn code_instances(
    raw: &RawFunctions,
    definitions: &[usize],
    function_ids: &[Option<FunctionId>],
    nameless: &[bool],
) -> std::result::Result<(Vec<CodeInstanceInfo>, DieMap<CodeInstanceId>), DwarfError> {
    let mut instances = vec![None; raw.len()];
    let mut instance_ids = DieMap::default();
    let mut count = 0;
    for ((index, function), definition) in raw.iter().enumerate().zip(definitions) {
        if !function.ranges.is_empty() && !nameless[*definition] {
            let id = CodeInstanceId::new(
                u32::try_from(count).map_err(|_| gimli::Error::UnsupportedOffset)?,
            );
            instances[index] = Some(id);
            instance_ids.insert(function.key, id);
            count += 1;
        }
    }

    if let Some(((function, _), _)) = raw
        .iter()
        .zip(&instances)
        .zip(definitions)
        .find(|((_, id), definition)| id.is_some() && function_ids[**definition].is_none())
    {
        return Err(DwarfError::ReferencedFunctionMissing {
            unit: function.key.unit,
            offset: function.key.offset,
        });
    }

    let code_instances = raw
        .par_iter()
        .zip(&instances)
        .zip(definitions)
        .filter_map(|((function, id), definition)| {
            Some(CodeInstanceInfo {
                id: (*id)?,
                // Every instance's definition has an ID, as checked above.
                function: function_ids[*definition]?,
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
            })
        })
        .collect::<Vec<_>>();

    Ok((code_instances, instance_ids))
}

/// What a function is to unwinding and stepping.
fn origin_role(origin: &RawFunction, name: &str, futures: &Futures) -> crate::CodeRole {
    let role = super::roles::function_role(
        origin.linkage_name.as_deref().unwrap_or(name),
        origin.trampoline || builds_future(origin, futures),
    );

    // What builds a future only wraps, even a runtime's: a step that
    // enters the runtime goes on into the future's body.
    match (&origin.namespace, &origin.name) {
        (Some(namespace), Some(own))
            if origin.language == SourceLanguage::Rust && role != crate::CodeRole::Wrapper =>
        {
            super::roles::rust_role(namespace, own).unwrap_or(role)
        }
        _ => role,
    }
}

/// What a function's language writes for a function whose DWARF name is
/// an encoding: Nim names its procedures' C functions by their mangled
/// names, and GNAT its subprograms by their scopes, encoded.
fn written_name(language: SourceLanguage, name: &str) -> Option<String> {
    match language {
        SourceLanguage::Nim => crate::demangle::qualified_name(name),
        SourceLanguage::Ada => crate::demangle::ada_name(name),
        _ => None,
    }
}

/// A function's own name, without the scopes its language qualifies it
/// with, as C names a function: Nim's `values::add`, Odin's, and Ada's
/// `values.add` are `add`. Other languages' DWARF names are their own.
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

/// The name a function shows. Clang names the thunks a multiply inherited
/// virtual function needs only by their linkage names, and the body of a
/// Rust `async fn` or block shows as the function its programmer wrote.
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

/// Links each async body whose parameters name no coroutine to the
/// coroutine type rustc's naming says it runs, as `body_futures` lists
/// them: what a Rust older than 1.93 leaves to its names alone.
///
/// A type is described once in every unit that uses it, as Rust 1.88's
/// coroutines spawned through tokio are, so one name may answer several
/// type ids. Those are one type to the image, which classes types by the
/// same identity (see `ModuleImage::same_type`), and the body is linked to
/// the first.
fn link_bodies_by_name(
    functions: &mut [FunctionInfo],
    body_futures: &[(FunctionId, Arc<str>)],
    types: &[crate::TypeNode],
    coroutines: &BTreeMap<crate::TypeId, std::result::Result<crate::CoroutineInfo, Arc<str>>>,
) {
    if body_futures
        .iter()
        .all(|(function, _)| functions[function.index()].coroutine.is_some())
    {
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
        let function = &mut functions[function.index()];
        if function.coroutine.is_none()
            && let Some(ty) = named.get(&**future).and_then(|types| types.first())
        {
            function.coroutine = Some(*ty);
        }
    }
}

/// Whether a function is an `async fn` as rustc compiles it apart from its
/// body: code that only builds the future, which returns the coroutine of
/// the `async fn` of its own name, less a generic function's arguments.
/// `futures` names the function each `async fn`'s coroutine type belongs
/// to.
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

/// Where a breakpoint on a code instance goes: the entry its DIE names,
/// when that lies in its code, or else where its code begins.
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

/// Gives each Go function its role, from its name, whether the compiler
/// generated it as a trampoline or an ABI wrapper, and what the function
/// table records at its entry. An ABI wrapper shares its function's DWARF
/// name but has its own entry.
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

            facts[instance.function.index()] = table
                .function_containing(entry)
                .filter(|function| function.entry == entry)
                .map(|function| function.facts);
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
                // cgo's C, such as the code that Go's calls to C enter.
                function.role = crate::CodeRole::Wrapper;
            } else if runtime_c && go_table.is_some() {
                function.role = crate::CodeRole::RuntimeInternal;
            }
        }
    }
}

/// One unit's function DIEs, decoded on a worker of its own, with the
/// files they name numbered within the unit.
#[derive(Default)]
struct UnitFunctions {
    functions: Vec<RawFunction>,
    futures: Futures,
    files: UnitFiles,
}

/// The files one unit's DIEs name, each path resolved once.
#[derive(Default)]
struct UnitFiles {
    files: Files,

    /// The file each of the line program's file indexes names.
    by_index: HashMap<u64, Option<crate::SourceFileId>>,
}

/// Every unit's function DIEs, in DIE order: by unit, then by offset.
/// Each unit's stay where its worker decoded them, and an index counts
/// through them all.
struct RawFunctions {
    units: Vec<Vec<RawFunction>>,

    /// The index of each unit's first function, and then of the end.
    starts: Vec<usize>,
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

    /// Where `key`'s function is.
    fn at(&self, key: DieKey) -> std::result::Result<usize, DwarfError> {
        self.units
            .get(key.unit)
            .and_then(|unit| {
                unit.binary_search_by_key(&key.offset, |function| function.key.offset)
                    .ok()
            })
            .map(|index| self.starts[key.unit] + index)
            .ok_or(DwarfError::ReferencedFunctionMissing {
                unit: key.unit,
                offset: key.offset,
            })
    }
}

impl std::ops::Index<usize> for RawFunctions {
    type Output = RawFunction;

    fn index(&self, index: usize) -> &RawFunction {
        let unit = self.starts.partition_point(|start| *start <= index) - 1;
        &self.units[unit][index - self.starts[unit]]
    }
}

/// Every unit's function DIEs, decoded in parallel, with the files they
/// name numbered as one walk of every unit in order would number them.
fn collect_function_dies<'data>(
    dwarf: &gimli::Dwarf<Reader<'data>>,
    catalog: &UnitCatalog<'data>,
    files: &mut Files,
) -> std::result::Result<(RawFunctions, Futures), DwarfError> {
    let batches = first_error(
        (0..catalog.units.len())
            .into_par_iter()
            .map(|unit| {
                collect_unit_functions(dwarf, catalog, unit)
                    .map_err(|error| error.within(|| describe_unit(&catalog.units[unit])))
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

    let units = renumbered
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

    Ok((RawFunctions { units, starts }, futures))
}

/// One unit's function DIEs, and the coroutine types of its `async fn`s.
fn collect_unit_functions<'data>(
    dwarf: &gimli::Dwarf<Reader<'data>>,
    catalog: &UnitCatalog<'data>,
    unit_index: usize,
) -> std::result::Result<UnitFunctions, DwarfError> {
    let units = &catalog.units;
    let unit = &units[unit_index];
    let mut batch = UnitFunctions::default();
    if is_type_unit(unit) {
        return Ok(batch);
    }

    let language = unit_language(dwarf, units, unit_index)?;

    // Only these DIEs are read; every other one's attributes are skipped.
    let wanted = |tag| match tag {
        gimli::DW_TAG_subprogram | gimli::DW_TAG_inlined_subroutine | gimli::DW_TAG_namespace => {
            true
        }
        gimli::DW_TAG_structure_type => language == SourceLanguage::Rust,
        _ => false,
    };

    let mut entries = unit.entries_raw(None)?;
    let mut entry = gimli::DebuggingInformationEntry::null();

    // What the DIEs at each depth are within: the nearest function, and
    // the namespace path, an index into `paths`.
    let mut levels = Vec::<(Option<DieKey>, Option<usize>)>::new();
    let mut paths = Vec::<Arc<str>>::new();

    while !entries.is_empty() {
        let depth = entries.next_depth();
        let mut next = entries.clone();

        // Null entries end children, and pad the unit after its root's.
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

        // A namespace's children are within its path; an unnamed one's
        // within none.
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

/// What a function DIE says of its function.
fn raw_function<'data>(
    dwarf: &gimli::Dwarf<Reader<'data>>,
    catalog: &UnitCatalog<'data>,
    entry: &gimli::DebuggingInformationEntry<Reader<'data>>,
    (key, kind, parent, language): (DieKey, RawFunctionKind, Option<DieKey>, SourceLanguage),
    namespace: Option<Arc<str>>,
    files: &mut UnitFiles,
) -> std::result::Result<RawFunction, DwarfError> {
    let units = &catalog.units;
    let unit = &units[key.unit];
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

/// The coroutine type of each `async fn`, and that function's name.
type Futures = HashMap<DieKey, Arc<str>>;

/// The name of the `async fn` whose coroutine type a DIE is, which rustc
/// nests in the function's namespace.
fn future_of(
    dwarf: &gimli::Dwarf<Reader<'_>>,
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

/// The language a unit is written in, by its root DIE or its importers.
fn unit_language(
    dwarf: &gimli::Dwarf<Reader<'_>>,
    units: &Units<'_>,
    unit_index: usize,
) -> std::result::Result<SourceLanguage, DwarfError> {
    let unit = &units[unit_index];
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
    dwarf: &gimli::Dwarf<Reader<'_>>,
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

/// A string attribute as the section holds it, without copying it: for
/// callers that only inspect a name, which need not allocate one.
fn str_attribute<'data>(
    dwarf: &gimli::Dwarf<Reader<'data>>,
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

/// A DWARF string as text, replacing what is not UTF-8. Checking that it
/// is first is far faster than gimli's lossy conversion of valid text.
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
        // A supplementary file's references stay within it.
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

/// Where the function `start` names is defined: the end of its origin and
/// specification chain, as an index into `raw`.
fn definition_of(start: DieKey, raw: &RawFunctions) -> std::result::Result<usize, DwarfError> {
    let mut index = raw.at(start)?;

    // A chain longer than the functions repeats one.
    for _ in 0..=raw.len() {
        let function = &raw[index];
        let Some(next) = function.abstract_origin.or(function.specification) else {
            return Ok(index);
        };

        index = raw.at(next)?;
    }

    Err(DwarfError::ReferenceCycle)
}

/// The nearest code instance enclosing the DIE `key`.
fn containing_instance(
    mut key: Option<DieKey>,
    raw: &RawFunctions,
    instances: &[Option<CodeInstanceId>],
) -> Option<CodeInstanceId> {
    while let Some(current) = key {
        let index = raw.at(current).ok()?;
        if let Some(instance) = instances[index] {
            return Some(instance);
        }

        key = raw[index].parent;
    }

    None
}

/// The source location a DIE's file, line, and column attributes name.
fn entry_source_location(
    dwarf: &gimli::Dwarf<Reader<'_>>,
    unit: &gimli::Unit<Reader<'_>>,
    entry: &gimli::DebuggingInformationEntry<Reader<'_>>,
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
        let header = unit
            .line_program
            .as_ref()
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

/// Appends a unit's line program to `tables`: every row of each sequence
/// in the image's code, and the range of code each located row describes.
fn load_lines(
    dwarf: &gimli::Dwarf<Reader<'_>>,
    unit: &gimli::Unit<Reader<'_>>,
    code: &CodeRanges,
    files: &mut Files,
    tables: &mut LineTables,
) -> std::result::Result<(), DwarfError> {
    let Some(program) = unit.line_program.clone() else {
        return Ok(());
    };

    let (program, sequences) = program.sequences()?;

    // Rows name files by index into the program header; resolving a path
    // allocates, so each index is resolved once.
    let mut file_ids = HashMap::new();

    for sequence in sequences {
        // A discarded function's sequence starts outside the image's code.
        if !code.contains_address(ImageAddress::new(sequence.start)) {
            continue;
        }

        tables.begin_sequence()?;
        let mut rows = program.resume_from(&sequence);

        // The line range being described: its start, the row whose location
        // it has, and whether it begins at a statement.
        let mut open: Option<(u64, u32, bool)> = None;

        while let Some((header, row)) = rows.next_row()? {
            let line = row.line().map_or(0, std::num::NonZeroU64::get);

            // Only a row with a line names a file: one with line 0 is
            // compiler-generated code, whose file is never shown.
            let file = match row.file(header) {
                Some(file) if line != 0 => {
                    Some(if let Some(&id) = file_ids.get(&row.file_index()) {
                        id
                    } else {
                        let id = files.intern(source_path(dwarf, unit, header, file)?);
                        file_ids.insert(row.file_index(), id);
                        id
                    })
                }
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

            // A row without a location still ends the previous range, so the
            // gap is not attributed to a neighboring line.
            if decoded.end_sequence || decoded.location().is_none() {
                close_line_range(&mut open, row.address(), tables)?;
                continue;
            }

            // Rows at one address collapse into a single range, a statement
            // boundary if any collapsed row recommends it. Its location is
            // the last statement row's, as gdb presents it: a later row that
            // is no statement, such as the line an inlined call came from,
            // does not describe where execution stands.
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
    }

    Ok(())
}

fn source_path(
    dwarf: &gimli::Dwarf<Reader<'_>>,
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

/// Moves an out-of-line function's breakpoint entry past a prologue that
/// x86-64 instruction analysis proves only sets up the frame, when the line
/// table marks no `prologue_end`. A heuristic: anything unproven keeps the
/// raw entry, and no failure here fails the module.
fn refine_proved_prologue_entries(
    object: &object::File<'_>,
    target: TargetDescription,
    rows: &StatementsByAddress<'_>,
    instances: &mut [CodeInstanceInfo],
) {
    if target.architecture != Architecture::X86_64 {
        return;
    }

    for instance in instances {
        if !matches!(instance.kind, CodeInstanceKind::OutOfLine)
            || instance
                .ranges
                .iter()
                .any(|range| rows.within(*range).any(|row| row.flags.prologue_end()))
        {
            continue;
        }

        let Some(raw_entry) = instance.breakpoint_entry.map(|entry| entry.address) else {
            continue;
        };

        let Some(entry_range) = instance
            .ranges
            .iter()
            .find(|range| range.contains(raw_entry))
        else {
            continue;
        };

        let Some(candidate) = first_distinct_source_statement(rows, *entry_range, raw_entry) else {
            continue;
        };

        let Some(bytes) = code_bytes(object, raw_entry.get(), candidate.get()) else {
            continue;
        };

        #[cfg(target_arch = "x86_64")]
        if super::x86_64::prove_prologue_prefix(bytes, raw_entry.get()).is_ok() {
            instance.breakpoint_entry = Some(BreakpointEntry {
                address: candidate,
                provenance: EntryProvenance::AnalyzedPrologue,
            });
        }
    }
}

/// The bytes of an object file a coroutine's dispatch is decoded from.
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
        Some(&bytes[..bytes.len().min(length)])
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

/// Decodes where each out-of-line function that runs a coroutine goes for
/// each state, and moves the breakpoint entry of every instance of one past
/// what leads into its body.
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
        let function = &functions[instance.function.index()];
        let Some(Ok(coroutine)) = function.coroutine.and_then(|ty| coroutines.get(&ty)) else {
            continue;
        };

        let header = function.declaration.as_ref().map(|location| location.line);
        let ranges = Arc::clone(&instance.ranges);
        let in_function = |address: u64| {
            let address = ImageAddress::new(address);
            ranges.iter().any(|range| range.contains(address))
        };

        // What leads into the body before its first statement carries the
        // header's line, which the dispatch and the argument moves have,
        // or none.
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
            // A body inlined into its awaiter is entered straight from the
            // awaiter's code.
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
                // The code a first poll can run. Resuming runs code of its
                // own until it joins that, such as the rest of a line
                // whose awaited call is inlined.
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

                    // Resuming runs the code of the state's own line, of the
                    // function's header, and of no line, which includes the
                    // loop polling the awaited future that arriving at the
                    // await runs too, and so is not where the await's line
                    // begins. It also runs code no first poll runs before
                    // it joins one's path, such as the rest of a line whose
                    // awaited body is inlined.
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
    // Overlapping line programs (COMDAT folding, duplicated metadata) can
    // attribute the same image address from unrelated sequences. Prologue
    // reasoning is only sound within the single sequence that describes the
    // entry, so an ambiguous entry attribution keeps the raw entry.
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

    // Line programs collapse equal-address rows by taking the final source
    // attribution. Mirror that rule here, and do not mistake a later row for
    // the same signature line for proof that argument homing has completed.
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

/// Returns the bytes of `[start, end)` from an executable section.
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

    /// A shared object whose `.text` at `address` holds `size` bytes, with
    /// the DWARF sections `sections` holds.
    fn elf_with_dwarf(
        address: u64,
        size: usize,
        sections: &Sections<EndianVec<LittleEndian>>,
    ) -> Vec<u8> {
        let mut debug = Vec::new();
        sections
            .for_each(|id, data| {
                if !data.slice().is_empty() {
                    debug.push((id.name(), data.slice().to_vec()));
                }
                Ok::<_, gimli::Error>(())
            })
            .expect("the sections");

        let mut bytes = Vec::new();
        let mut writer =
            object::write::elf::Writer::new(object::Endianness::Little, true, &mut bytes);
        let text = writer.add_section_name(b".text");
        let names: Vec<_> = debug
            .iter()
            .map(|(name, _)| writer.add_section_name(name.as_bytes()))
            .collect();
        writer.reserve_file_header();
        writer.reserve_null_section_index();
        writer.reserve_section_index();
        for _ in &debug {
            writer.reserve_section_index();
        }
        let code = writer.reserve(size, 16);
        let offsets: Vec<_> = debug
            .iter()
            .map(|(_, data)| writer.reserve(data.len(), 1))
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
        for (_, data) in &debug {
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
        for ((name, (_, data)), offset) in names.into_iter().zip(&debug).zip(offsets) {
            writer.write_section_header(&object::write::elf::SectionHeader {
                name: Some(name),
                sh_type: object::elf::SHT_PROGBITS,
                sh_flags: 0,
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

    /// Clang leaves some functions with code and no name of any kind, as
    /// in Chromium's split DWARF. Such a function is none the image names,
    /// and the rest of its DWARF still loads.
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

        // A sequence outside the image's code belongs to a discarded function.
        load_lines(&dwarf, &unit, &code(0x200, 0x300), &mut files, &mut tables)
            .expect("load test line program");
        assert!(tables.rows.is_empty() && tables.ranges.is_empty());

        load_lines(&dwarf, &unit, &code(0x100, 0x200), &mut files, &mut tables)
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
            // A foreign sequence at the entry address makes attribution
            // ambiguous.
            (
                [
                    row(0x100, 10, 0, 0),
                    row(0x100, 50, 1, 0),
                    row(0x120, 11, 0, 1),
                ],
                None,
            ),
            // A foreign sequence within the body supplies no candidate.
            (
                [
                    row(0x100, 10, 0, 0),
                    row(0x110, 50, 1, 0),
                    row(0x120, 11, 0, 1),
                ],
                Some(0x120),
            ),
        ] {
            // A line program keeps each sequence's rows together.
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

        // Each expression starts with the CFA on its stack.
        let expressions: [&[u8]; 5] = [
            &[0x38, 0x1c],       // DW_OP_lit8, DW_OP_minus: saved at CFA - 8
            &[0x40, 0x22],       // DW_OP_lit16, DW_OP_plus: CFA + 16 is the value
            &[0x31, 0x22, 0x9f], // DW_OP_lit1, DW_OP_plus, DW_OP_stack_value
            &[0x71, 0x00],       // DW_OP_breg1 0: saved where register 1 points
            &[0x9f],             // DW_OP_stack_value: no address at all
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
        // DW_OP_lit0, DW_OP_lit1, DW_OP_xderef: dereference address 0 in
        // address space 1. The evaluator must reject the non-default space
        // instead of silently reading the default inferior address space.
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

    /// Checks that looking an address up in a section's index finds the
    /// entry gimli's walk of the whole section finds, or fails as it does,
    /// at every entry's edges and at `extra`.
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

        // The walk `fde_for_address` makes for each address, made once:
        // the first entry in section order that contains an address
        // answers it, and an entry that does not parse answers every
        // address no entry before it contains.
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

    /// Loading on one worker, two, or eight builds a byte-identical image.
    /// One worker walks the units in order; more walk them apart and add
    /// what each built in order. Units that reach each other's types, as
    /// Go's and Zig's do, are walked in order with each other, apart from
    /// the rest.
    #[test]
    fn every_number_of_workers_loads_the_same_image() {
        for fixture in [
            "containers-cpp-clang-o2",
            "containers-rust-o2",
            "tokio-server-1.52-o0",
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

            // Walks that may run only one unit past those added wait on the
            // adding, and build the same image.
            let waiting = LoadLimits {
                walk_ahead: 1,
                ..LoadLimits::default()
            };

            assert!(
                load_with(8, waiting) == serial,
                "{fixture} differs walking one unit ahead"
            );
            assert!(!serial.is_empty(), "{fixture}");
            for jobs in [2, 8] {
                let recording =
                    crate::timings::Recording::start(crate::timings::Options::default())
                        .expect("no other recording");
                let apart = load(jobs);
                let report = recording.finish().unwrap();
                assert!(apart == serial, "{fixture} differs with {jobs} workers");
                assert_eq!(
                    report.counters.get("units_walked_in_order"),
                    None,
                    "{fixture} walks its units apart with {jobs} workers"
                );
            }
        }
    }

    /// mold and lld describe no PLT stub with call-frame information. A
    /// stub's caller is where its call left the return address, past the
    /// words the stub pushed, at its first instruction and past it. The
    /// `.plt` header has no frame of its own: what is pushed when it runs
    /// depends on the stub that jumped to it.
    #[test]
    fn plt_stubs_unwind_to_their_callers_without_call_frame_information() {
        // `printf@plt` is `mov $0, %r11d` (6 bytes), then the jump through
        // the GOT; `memcpy@plt`, in `.plt.got`, is the jump alone, where the
        // tokio suite once caught a thread.
        for (fixture, stub, offsets) in [
            ("disassembly-gcc-mold", "printf@plt", &[0, 6][..]),
            ("tokio-steps-o0", "memcpy@plt", &[0]),
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
                    (crate::unwind::RSP, stack),
                    (crate::unwind::RETURN_ADDRESS, address.get()),
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
                    step.registers.get(crate::unwind::RETURN_ADDRESS),
                    Some(0x1234)
                );
                assert_eq!(step.registers.get(crate::unwind::RSP), Some(stack + 8));
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
                (crate::unwind::RSP, stack),
                (crate::unwind::RETURN_ADDRESS, header.get()),
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

    /// Debug information that would build more than its budget is left
    /// out, with the budget's error naming what asked, and loads with the
    /// default.
    #[test]
    fn a_load_past_its_budget_leaves_the_dwarf_out_naming_what_asked() {
        let path =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("build/test-programs/containers-rust-o2");
        let data = fs::read(&path).expect("run `just build-test-programs`");
        let load = |limits| {
            load_debug_info(
                &path,
                &data,
                crate::ModuleImageId::new(0),
                &super::super::DebugFileSearch::default(),
                limits,
                None,
            )
        };

        let small = LoadLimits {
            per_input_byte: 0,
            floor: 64 << 10,
            ..LoadLimits::default()
        };

        let (over, _) = load(small).expect("a load past its budget");
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
        let (within, _) = load(LoadLimits::default()).expect("the default budget");
        assert_eq!(
            within.image.debug_information(),
            crate::DebugInformation::Loaded
        );
    }

    /// Debug information whose locals would take it past its budget loads
    /// without them, keeping its functions, lines, globals, and the
    /// coroutines async bodies run, and says why it has no locals.
    #[test]
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

        // A budget too small for the locals but large enough for the rest.
        // Those budgets differ by a factor rather than a margin, which
        // halving the factor between the bounds finds in a few loads.
        let (mut small, mut large) = (1_u64, 1 << 32);
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

        // A frame says why it shows no locals, rather than seeming to have
        // none.
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

    /// FDE lookups in real images agree with a walk of the section, and so
    /// do lookups in a section cut short mid-entry, and in the loaded image.
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
            let unwind = load_unwind_info(&object, None, target, Vec::new(), None).expect("CFI");
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

            // The loaded image keeps the same sections and indexes.
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

            // Cut short mid-entry, a section's walk fails where it ends.
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

    /// Functions shaped as GCC builds CPython's `_PyObject_Malloc` and
    /// 3.15's handlers: after its `leave` or `pop rbp`, a function's rules
    /// still name the slots that saved rbp and rbx, below the stack pointer
    /// and so below a sample's copy of the stack. Where the code proves it
    /// restored them, running straight to its return, the caller has the
    /// values they hold; where it proves the `pop` restored rbp, until it
    /// jumps, rbp only; without a proof, they are unknown.
    #[test]
    fn an_epilogue_that_restored_its_callers_registers_unwinds_from_a_samples_copy() {
        use gimli::write::CallFrameInstruction;

        // push rbp; mov rbp, rsp; push rbx; mov rbx, [rbp-8]; leave;
        // xor edx, edx; xor ecx, ecx; ret
        let mut code = vec![
            0x55, 0x48, 0x89, 0xe5, 0x53, 0x48, 0x8b, 0x5d, 0xf8, 0xc9, 0x31, 0xd2, 0x31, 0xc9,
            0xc3,
        ];
        // push rbp; mov rbp, rsp; sub rsp, 0x30; add rsp, 0x30; pop rbp;
        // cmp qword [r8+rax*8], 0x1a01d40; je elsewhere; jmp [r8+rax*8]
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
            (crate::unwind::RSP, stack),
            (crate::unwind::RETURN_ADDRESS, start + 12),
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
                (&X86_64_SYSV_CALL_CLOBBERED_REGISTERS, restored),
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

        // Memory that still holds the slots is read, as a live unwind's is.
        let mut live = copy();
        live.values.insert(VirtualAddress::new(stack - 8), 0xbeef);
        live.values.insert(VirtualAddress::new(stack - 16), 0xcafe);
        assert_eq!(
            kept(&unwind(0x3f, &mut live)),
            [Some(0xbeef), Some(0xcafe), Some(0x4242)]
        );

        // Code that writes rbx on its way to the return proves only rbp,
        // which its `leave` loaded.
        code[13] = 0xdb;
        assert_eq!(
            restored_returns(&section, &bases, &[(start, &code)]),
            [(range(start + 10, start + 14), 2)]
        );
    }

    /// Where entries overlap, the first in section order wins, and an entry
    /// that does not parse hides every entry after it, as in a walk.
    #[test]
    fn indexed_fde_lookups_follow_section_order() {
        // Overlapping entries, in an order the section's walk must respect.
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

        // An entry whose CIE pointer leads nowhere stops the walk there, so
        // later entries never match.
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

    /// Loading a large real program, gofmt, does work in proportion to its
    /// debug information, counted as what the loading thread allocates: a
    /// regression bound that a loader doing far more than it did fails.
    /// Loading allocates about 100 bytes, in 0.47 blocks, for each byte of
    /// gofmt's; the bounds are half again as much.
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
                .map(|section| section.uncompressed_data().expect("a section").len() as u64)
                .sum::<u64>();

            // Totals count every thread's blocks, so that work spread
            // over worker threads counts too.
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

    /// Before Rust 1.93 an async body names the coroutine it runs only by
    /// rustc's naming. Each body is linked to the coroutine type of its
    /// future's full name, the first of its copies when several units
    /// describe it; a coroutine whose layout did not load, a name no type
    /// answers, and a body already linked by its parameter are left as
    /// they are.
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
            // One type, described in two units.
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
