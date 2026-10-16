//! Linker symbols, allocated sections, and the GOT slots the loader fills.
//!
//! Symbols carry their rank among the code symbols and among the data
//! symbols, by the preferences that choose one of several symbols naming
//! an address, so that a lookup compares numbers, not names.

use std::sync::Arc;

use zerocopy::little_endian::{U32, U64};
use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout, Unaligned};

use super::index::{self, Interval, NameEntry};
use super::strings::{StrId, Strings, StringsBuilder};
use super::{Builder, Image, NONE, Record, TableKind};
use crate::{
    AddressRange, CodeRole, GotSlot, GotTarget, ImageAddress, SectionId, SectionInfo,
    SymbolBinding, SymbolExtent, SymbolExtentProvenance, SymbolId, SymbolInfo, SymbolKind,
};

/// One linker symbol.
#[repr(C)]
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, FromBytes, IntoBytes, KnownLayout, Immutable, Unaligned,
)]
pub struct SymbolRecord {
    pub name: U32,
    pub address: U64,

    /// Where the symbol's extent or storage ends; its address when it has
    /// neither.
    pub end: U64,

    /// The symbol's place among code symbols by preference, or [`NONE`].
    pub extent_rank: U32,

    /// The symbol's place among data symbols by preference, or [`NONE`].
    pub storage_rank: U32,
    pub kind: u8,
    pub binding: u8,
    pub role: u8,
    pub flags: u8,
}

impl Record for SymbolRecord {
    const KIND: TableKind = TableKind::Symbols;
}

/// [`SymbolRecord::flags`].
pub mod symbol_flags {
    pub const EXPORTED: u8 = 1 << 0;

    pub const EXTENT: u8 = 1 << 1;

    /// The extent was inferred, not declared.
    pub const INFERRED: u8 = 1 << 2;

    pub const STORAGE: u8 = 1 << 3;

    pub const ALL: u8 = EXPORTED | EXTENT | INFERRED | STORAGE;
}

/// One allocated section.
#[repr(C)]
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, FromBytes, IntoBytes, KnownLayout, Immutable, Unaligned,
)]
pub struct SectionRecord {
    pub name: U32,
    pub start: U64,
    pub end: U64,
    pub flags: u8,
}

impl Record for SectionRecord {
    const KIND: TableKind = TableKind::Sections;
}

/// [`SectionRecord::flags`].
pub mod section_flags {
    pub const EXECUTABLE: u8 = 1 << 0;

    pub const WRITABLE: u8 = 1 << 1;

    pub const ALL: u8 = EXECUTABLE | WRITABLE;
}

/// One GOT slot: an import by name, or an indirect function's resolver.
#[repr(C)]
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, FromBytes, IntoBytes, KnownLayout, Immutable, Unaligned,
)]
pub struct GotRecord {
    pub address: U64,

    /// The resolver's address, for an indirect function.
    pub target: U64,

    /// The imported name, or [`NONE`].
    pub name: U32,
    pub kind: u8,
}

impl Record for GotRecord {
    const KIND: TableKind = TableKind::GotSlots;
}

const GOT_IMPORT: u8 = 0;
const GOT_INDIRECT: u8 = 1;

const _: () = assert!(size_of::<SymbolRecord>() == 32);
const _: () = assert!(size_of::<SectionRecord>() == 21);
const _: () = assert!(size_of::<GotRecord>() == 21);

const SYMBOL_KINDS: [SymbolKind; 4] = [
    SymbolKind::Function,
    SymbolKind::IndirectFunction,
    SymbolKind::Data,
    SymbolKind::Unknown,
];
const BINDINGS: [SymbolBinding; 3] = [
    SymbolBinding::Global,
    SymbolBinding::Weak,
    SymbolBinding::Local,
];
const ROLES: [CodeRole; 10] = [
    CodeRole::Ordinary,
    CodeRole::Wrapper,
    CodeRole::RuntimeInternal,
    CodeRole::Panic,
    CodeRole::StackSwitch,
    CodeRole::Outermost,
    CodeRole::TrapEntry,
    CodeRole::SignalTrampoline,
    CodeRole::Dispatch,
    CodeRole::Interpreter,
];

/// A code role's number in a table.
pub(super) fn role_code(role: CodeRole) -> Option<u8> {
    ROLES
        .iter()
        .position(|known| *known == role)
        .and_then(|code| u8::try_from(code).ok())
}

/// The code role a number names; validation checked it names one.
pub(super) fn role(code: u8) -> CodeRole {
    ROLES[usize::from(code)]
}

/// Whether `code` names a code role.
pub(super) const fn valid_role(code: u8) -> bool {
    (code as usize) < ROLES.len()
}

fn code_of<T: PartialEq>(known: &[T], value: &T) -> Option<u8> {
    known
        .iter()
        .position(|candidate| candidate == value)
        .and_then(|code| u8::try_from(code).ok())
}

/// Why symbols could not be encoded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("a symbol's or section's name holds a NUL, or the names do not fit")]
pub struct BadName;

/// Adds `symbols`, `sections`, and `slots`, with their indexes, to
/// `builder`, pooling their names in `strings`.
pub fn add_to(
    builder: &mut Builder<'_>,
    strings: &mut StringsBuilder,
    symbols: &[SymbolInfo],
    sections: &[SectionInfo],
    slots: &[GotSlot],
) -> Result<(), BadName> {
    add_symbols(builder, strings, symbols)?;
    add_sections(builder, strings, sections)?;
    add_got(builder, strings, slots)
}

fn add_symbols(
    builder: &mut Builder<'_>,
    strings: &mut StringsBuilder,
    symbols: &[SymbolInfo],
) -> Result<(), BadName> {
    let extent_ranks = ranks(symbols, |symbol| {
        symbol.extent.map(|extent| {
            (
                extent.provenance,
                std::cmp::Reverse(extent.range.start),
                extent.range.end.get() - extent.range.start.get(),
                symbol.kind,
                symbol.binding,
                !symbol.exported,
                leading_underscores(&symbol.name),
                Arc::clone(&symbol.name),
                symbol.id,
            )
        })
    });
    let storage_ranks = ranks(symbols, |symbol| {
        symbol.storage.map(|storage| {
            (
                std::cmp::Reverse(storage.start),
                storage.end.get() - storage.start.get(),
                symbol.binding,
                !symbol.exported,
                leading_underscores(&symbol.name),
                Arc::clone(&symbol.name),
                symbol.id,
            )
        })
    });
    let mut records = Vec::with_capacity(symbols.len());
    let mut names = Vec::with_capacity(symbols.len());
    for (index, symbol) in symbols.iter().enumerate() {
        let name = strings.push(&symbol.name).ok_or(BadName)?;
        names.push((&*symbol.name, name, symbol.id.get()));
        let mut flags = 0;
        if symbol.exported {
            flags |= symbol_flags::EXPORTED;
        }

        let end = match (symbol.extent, symbol.storage) {
            (Some(extent), _) => {
                flags |= symbol_flags::EXTENT;
                if extent.provenance == SymbolExtentProvenance::Inferred {
                    flags |= symbol_flags::INFERRED;
                }

                extent.range.end
            }
            (None, Some(storage)) => {
                flags |= symbol_flags::STORAGE;
                storage.end
            }
            (None, None) => symbol.address,
        };

        records.push(SymbolRecord {
            name: name.0.into(),
            address: symbol.address.get().into(),
            end: end.get().into(),
            extent_rank: extent_ranks[index].into(),
            storage_rank: storage_ranks[index].into(),
            kind: code_of(&SYMBOL_KINDS, &symbol.kind).ok_or(BadName)?,
            binding: code_of(&BINDINGS, &symbol.binding).ok_or(BadName)?,
            role: role_code(symbol.role).ok_or(BadName)?,
            flags,
        });
    }

    let names = index::names(names);
    let extents = index::intervals(
        symbols
            .iter()
            .filter_map(|symbol| Some((symbol.extent?.range, symbol.id.get()))),
    );
    let storage = index::intervals(
        symbols
            .iter()
            .filter_map(|symbol| Some((symbol.storage?, symbol.id.get()))),
    );
    let unsized_data = index::intervals(symbols.iter().filter_map(|symbol| {
        let storage = symbol.storage?;
        (storage.start == storage.end).then_some((
            AddressRange {
                start: storage.start,
                end: ImageAddress::new(storage.start.get().checked_add(1)?),
            },
            symbol.id.get(),
        ))
    }));
    builder
        .owned_table(records)
        .owned_shared(TableKind::SymbolNames, names)
        .owned_shared(TableKind::SymbolExtents, extents)
        .owned_shared(TableKind::SymbolStorage, storage)
        .owned_shared(TableKind::UnsizedData, unsized_data);
    Ok(())
}

fn add_sections(
    builder: &mut Builder<'_>,
    strings: &mut StringsBuilder,
    sections: &[SectionInfo],
) -> Result<(), BadName> {
    let mut section_records = Vec::with_capacity(sections.len());
    for section in sections {
        let mut flags = 0;
        if section.executable {
            flags |= section_flags::EXECUTABLE;
        }

        if section.writable {
            flags |= section_flags::WRITABLE;
        }

        section_records.push(SectionRecord {
            name: strings.push(&section.name).ok_or(BadName)?.0.into(),
            start: section.range.start.get().into(),
            end: section.range.end.get().into(),
            flags,
        });
    }

    let section_ranges = index::intervals(
        sections
            .iter()
            .map(|section| (section.range, section.id.get())),
    );
    builder
        .owned_table(section_records)
        .owned_shared(TableKind::SectionRanges, section_ranges);
    Ok(())
}

fn add_got(
    builder: &mut Builder<'_>,
    strings: &mut StringsBuilder,
    slots: &[GotSlot],
) -> Result<(), BadName> {
    let mut got = Vec::with_capacity(slots.len());
    for slot in slots {
        got.push(match &slot.target {
            GotTarget::Import(name) => GotRecord {
                address: slot.address.get().into(),
                target: 0.into(),
                name: strings.push(name).ok_or(BadName)?.0.into(),
                kind: GOT_IMPORT,
            },
            GotTarget::Indirect(resolver) => GotRecord {
                address: slot.address.get().into(),
                target: resolver.get().into(),
                name: NONE.into(),
                kind: GOT_INDIRECT,
            },
        });
    }

    builder.owned_table(got);
    Ok(())
}

fn leading_underscores(name: &str) -> usize {
    name.bytes().take_while(|byte| *byte == b'_').count()
}

/// Each symbol's place in the order `key` gives the symbols that have
/// one, or [`NONE`].
fn ranks<K: Ord>(symbols: &[SymbolInfo], key: impl Fn(&SymbolInfo) -> Option<K>) -> Vec<u32> {
    let mut keyed = symbols
        .iter()
        .enumerate()
        .filter_map(|(index, symbol)| Some((key(symbol)?, index)))
        .collect::<Vec<_>>();
    keyed.sort_unstable();
    let mut ranks = vec![NONE; symbols.len()];
    for (rank, (_, index)) in (0u32..).zip(keyed) {
        ranks[index] = rank;
    }

    ranks
}

/// One linker symbol of a validated image.
#[derive(Clone, Copy)]
pub struct Symbol<'a> {
    strings: Strings<'a>,
    id: SymbolId,
    record: &'a SymbolRecord,
}

impl std::fmt::Debug for Symbol<'_> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.info().fmt(formatter)
    }
}

impl PartialEq for Symbol<'_> {
    fn eq(&self, other: &Self) -> bool {
        std::ptr::eq(self.record, other.record)
    }
}

impl Eq for Symbol<'_> {}

impl<'a> Symbol<'a> {
    #[must_use]
    pub const fn id(self) -> SymbolId {
        self.id
    }

    /// The linker-visible symbol name.
    #[must_use]
    pub fn name(self) -> &'a str {
        self.strings.get(StrId(self.record.name.get()))
    }

    /// The symbol's image address.
    #[must_use]
    pub const fn address(self) -> ImageAddress {
        ImageAddress::new(self.record.address.get())
    }

    /// What the symbol names.
    #[must_use]
    pub fn kind(self) -> SymbolKind {
        SYMBOL_KINDS[usize::from(self.record.kind)]
    }

    /// The symbol's linkage visibility.
    #[must_use]
    pub fn binding(self) -> SymbolBinding {
        BINDINGS[usize::from(self.record.binding)]
    }

    /// Whether the module's dynamic symbol table exports the symbol.
    #[must_use]
    pub const fn exported(self) -> bool {
        self.record.flags & symbol_flags::EXPORTED != 0
    }

    /// The code the symbol names, for a code symbol whose extent lies
    /// within one executable section.
    #[must_use]
    pub const fn extent(self) -> Option<SymbolExtent> {
        if self.record.flags & symbol_flags::EXTENT == 0 {
            return None;
        }

        Some(SymbolExtent {
            range: self.range(),
            provenance: if self.record.flags & symbol_flags::INFERRED == 0 {
                SymbolExtentProvenance::Declared
            } else {
                SymbolExtentProvenance::Inferred
            },
        })
    }

    /// The storage a data symbol names; empty for an unsized symbol.
    #[must_use]
    pub const fn storage(self) -> Option<AddressRange<ImageAddress>> {
        if self.record.flags & symbol_flags::STORAGE == 0 {
            return None;
        }

        Some(self.range())
    }

    /// Where the symbol's extent or storage ends; its address when it has
    /// neither.
    #[cfg(any(test, feature = "fuzzing"))]
    pub(super) const fn range_end(self) -> ImageAddress {
        self.range().end
    }

    const fn range(self) -> AddressRange<ImageAddress> {
        AddressRange {
            start: ImageAddress::new(self.record.address.get()),
            end: ImageAddress::new(self.record.end.get()),
        }
    }

    /// What the code the symbol names is to unwinding and stepping.
    #[must_use]
    pub fn role(self) -> CodeRole {
        role(self.record.role)
    }

    pub(crate) const fn extent_rank(self) -> u32 {
        self.record.extent_rank.get()
    }

    pub(crate) const fn storage_rank(self) -> u32 {
        self.record.storage_rank.get()
    }

    /// The source-level spelling of a Rust, C++, or D mangled name, or `None`
    /// when the name is not mangled in a recognized scheme.
    #[must_use]
    pub fn demangled_name(self) -> Option<String> {
        crate::demangle::demangle(self.name())
    }

    /// Whether a name a person writes names the symbol: its linker name,
    /// that name without its version, or its demangled spelling, with or
    /// without a C++ function's parameters, as `shapes::scale` names
    /// `_ZN6shapes5scaleEd`, `shapes::scale(double)`.
    #[must_use]
    pub fn answers_to(self, name: &str) -> bool {
        self.name() == name
            || self.unversioned_name() == name
            || crate::demangle::spells(self.name(), name)
    }

    /// The name without the version that tells it apart from other
    /// definitions of the name, as `memcpy` for `memcpy@GLIBC_2.2.5`.
    #[must_use]
    pub fn unversioned_name(self) -> &'a str {
        let name = self.name();
        match name.split_once('@') {
            Some((unversioned, version)) if !unversioned.is_empty() && version != "plt" => {
                unversioned
            }
            _ => name,
        }
    }

    /// The symbol as a record of its own.
    #[must_use]
    pub fn info(self) -> SymbolInfo {
        SymbolInfo {
            id: self.id,
            name: self.name().into(),
            address: self.address(),
            kind: self.kind(),
            binding: self.binding(),
            exported: self.exported(),
            extent: self.extent(),
            storage: self.storage(),
            role: self.role(),
        }
    }
}

/// The symbol tables of a validated image.
#[derive(Clone, Copy)]
pub struct SymbolView<'a> {
    strings: Strings<'a>,
    symbols: &'a [SymbolRecord],
    names: &'a [NameEntry],
    extents: &'a [Interval],
    storage: &'a [Interval],
    unsized_data: &'a [Interval],
}

impl<'a> SymbolView<'a> {
    pub fn new(image: &'a Image) -> Self {
        Self {
            strings: image.strings(),
            symbols: image.table(),
            names: image.shared(TableKind::SymbolNames),
            extents: image.shared(TableKind::SymbolExtents),
            storage: image.shared(TableKind::SymbolStorage),
            unsized_data: image.shared(TableKind::UnsizedData),
        }
    }

    pub fn get(self, id: SymbolId) -> Option<Symbol<'a>> {
        Some(Symbol {
            strings: self.strings,
            id,
            record: self.symbols.get(id.index())?,
        })
    }

    pub fn all(self) -> impl ExactSizeIterator<Item = Symbol<'a>> + DoubleEndedIterator {
        self.symbols
            .iter()
            .zip(super::ids(self.symbols.len()))
            .map(move |(record, id)| Symbol {
                strings: self.strings,
                id: SymbolId::new(id),
                record,
            })
    }

    /// The symbols named `name`, in identifier order.
    pub fn named(self, name: &str) -> impl Iterator<Item = Symbol<'a>> + 'a {
        index::named(self.strings, self.names, name).filter_map(move |id| {
            let symbol = self.get(SymbolId::new(id));
            debug_assert!(symbol.is_some(), "validation checked indexed symbol {id}");
            symbol
        })
    }

    /// The preferred code symbol whose extent contains `address`.
    pub fn code_at(self, address: ImageAddress) -> Option<Symbol<'a>> {
        self.preferred(self.extents, address, Symbol::extent_rank)
    }

    /// The preferred data symbol whose storage contains `address`, or else
    /// an unsized one at `address`.
    pub fn data_at(self, address: ImageAddress) -> Option<Symbol<'a>> {
        self.preferred(self.storage, address, Symbol::storage_rank)
            .or_else(|| self.preferred(self.unsized_data, address, Symbol::storage_rank))
    }

    fn preferred(
        self,
        intervals: &'a [Interval],
        address: ImageAddress,
        rank: fn(Symbol<'a>) -> u32,
    ) -> Option<Symbol<'a>> {
        index::containing(intervals, address)
            .filter_map(|id| self.get(SymbolId::new(id)))
            .min_by_key(|symbol| rank(*symbol))
    }
}

/// The sections of a validated image.
pub fn sections(image: &Image) -> Vec<SectionInfo> {
    let strings = image.strings();
    image
        .table::<SectionRecord>()
        .iter()
        .zip(0u32..)
        .map(|(record, index)| SectionInfo {
            id: SectionId::new(index),
            name: strings.get(StrId(record.name.get())).into(),
            range: AddressRange {
                start: ImageAddress::new(record.start.get()),
                end: ImageAddress::new(record.end.get()),
            },
            executable: record.flags & section_flags::EXECUTABLE != 0,
            writable: record.flags & section_flags::WRITABLE != 0,
        })
        .collect()
}

/// The GOT slots of a validated image.
pub fn got_slots(image: &Image) -> Vec<GotSlot> {
    let strings = image.strings();
    image
        .table::<GotRecord>()
        .iter()
        .map(|record| GotSlot {
            address: ImageAddress::new(record.address.get()),
            target: if record.kind == GOT_IMPORT {
                GotTarget::Import(strings.get(StrId(record.name.get())).into())
            } else {
                GotTarget::Indirect(ImageAddress::new(record.target.get()))
            },
        })
        .collect()
}

/// Checks the symbol, section, and GOT tables.
pub(super) fn validate(image: &Image) -> Result<(), String> {
    validate_symbols(image)?;
    validate_sections(image)
}

fn validate_symbols(image: &Image) -> Result<(), String> {
    let strings = image.strings();
    let symbols = image.table::<SymbolRecord>();
    let count = |flag| {
        symbols
            .iter()
            .filter(|symbol| symbol.flags & flag != 0)
            .count()
    };

    // Each of the symbols with a rank holds a different one below their
    // count, so together they hold every rank.
    let mut extent_ranks = vec![false; count(symbol_flags::EXTENT)];
    let mut storage_ranks = vec![false; count(symbol_flags::STORAGE)];
    for symbol in symbols {
        let (start, end) = (symbol.address.get(), symbol.end.get());
        let flags = symbol.flags;
        let extent = flags & symbol_flags::EXTENT != 0;
        let storage = flags & symbol_flags::STORAGE != 0;
        let kind = SYMBOL_KINDS.get(usize::from(symbol.kind));
        let shape = match (extent, storage) {
            (true, false) => {
                matches!(
                    kind,
                    Some(SymbolKind::Function | SymbolKind::IndirectFunction)
                ) && start < end
            }
            (false, true) => kind == Some(&SymbolKind::Data) && start <= end,
            (false, false) => kind.is_some() && end == start,
            (true, true) => false,
        };

        if !shape
            || flags & !symbol_flags::ALL != 0
            || (!extent && flags & symbol_flags::INFERRED != 0)
            || usize::from(symbol.binding) >= BINDINGS.len()
            || !valid_role(symbol.role)
            || !strings.contains(StrId(symbol.name.get()))
        {
            return Err("a symbol is malformed".into());
        }

        for (has, rank, seen) in [
            (extent, symbol.extent_rank.get(), &mut extent_ranks),
            (storage, symbol.storage_rank.get(), &mut storage_ranks),
        ] {
            if has {
                let Some(slot) = seen.get_mut(rank as usize).filter(|slot| !**slot) else {
                    return Err("a symbol's rank is out of range or repeated".into());
                };
                *slot = true;
            } else if rank != NONE {
                return Err("a symbol has a rank it cannot".into());
            }
        }
    }

    // Each entry is its symbol's own interval, so with as many entries as
    // indexed symbols, and no entry twice, each symbol is indexed once.
    let index = |kind, flag: fn(&SymbolRecord) -> bool| {
        let intervals = image.shared::<Interval>(kind);
        index::valid_intervals(intervals, symbols.len())
            && intervals.len() == symbols.iter().filter(|symbol| flag(symbol)).count()
            && intervals.iter().all(|interval| {
                let symbol = &symbols[interval.value.get() as usize];
                let end = if symbol.address == symbol.end {
                    symbol.end.get().checked_add(1)
                } else {
                    Some(symbol.end.get())
                };

                flag(symbol) && interval.start == symbol.address && Some(interval.end.get()) == end
            })
    };

    if !index(TableKind::SymbolExtents, |symbol| {
        symbol.flags & symbol_flags::EXTENT != 0
    }) || !index(TableKind::SymbolStorage, |symbol| {
        symbol.flags & symbol_flags::STORAGE != 0 && symbol.address != symbol.end
    }) || !index(TableKind::UnsizedData, |symbol| {
        symbol.flags & symbol_flags::STORAGE != 0
            && symbol.address == symbol.end
            && symbol.address.get() != u64::MAX
    }) {
        return Err("the symbol indexes disagree with the symbols".into());
    }

    let names = image.shared::<NameEntry>(TableKind::SymbolNames);
    if names.len() != symbols.len()
        || !index::valid_names(&strings, names, symbols.len())
        || !names
            .iter()
            .all(|entry| symbols[entry.value.get() as usize].name == entry.name)
    {
        return Err("the symbol name index disagrees with the symbols".into());
    }

    Ok(())
}

fn validate_sections(image: &Image) -> Result<(), String> {
    let strings = image.strings();
    let sections = image.table::<SectionRecord>();
    if !sections.iter().all(|section| {
        section.start.get() < section.end.get()
            && section.flags & !section_flags::ALL == 0
            && strings.contains(StrId(section.name.get()))
    }) {
        return Err("a section is malformed".into());
    }

    let ranges = image.shared::<Interval>(TableKind::SectionRanges);
    if ranges.len() != sections.len()
        || !index::valid_intervals(ranges, sections.len())
        || !ranges.iter().all(|interval| {
            let section = &sections[interval.value.get() as usize];
            interval.start == section.start && interval.end == section.end
        })
    {
        return Err("the section index disagrees with the sections".into());
    }

    if !image
        .table::<GotRecord>()
        .iter()
        .all(|slot| match slot.kind {
            GOT_IMPORT => slot.target.get() == 0 && strings.contains(StrId(slot.name.get())),
            GOT_INDIRECT => slot.name.get() == NONE,
            _ => false,
        })
    {
        return Err("a GOT slot is malformed".into());
    }

    Ok(())
}
