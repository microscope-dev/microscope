//! Facts about an image as a whole: the addresses it spans, which symbol
//! tables it provided, what became of its DWARF and of the separate debug
//! file found for it, and where each thread's copy of its thread-local
//! variables is.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use zerocopy::little_endian::{U32, U64};
use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout, Unaligned};

use super::strings::{StrId, Strings, StringsBuilder};
use super::{Builder, Image, NONE, Record, TableKind};
use crate::model::ThreadLocal;
use crate::{
    AddressRange, DebugFile, DebugInformation, EmbeddedSymbolTable, ImageAddress,
    SymbolTableSources,
};

/// The image's facts. An image holds at most one; none means it spans no
/// addresses, every table is absent, and it has no thread-local storage.
#[repr(C)]
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, FromBytes, IntoBytes, KnownLayout, Immutable, Unaligned,
)]
pub struct FactsRecord {
    /// Why the embedded symbol table and the runtime's function table
    /// could not be read, when [`TABLE_UNUSABLE`].
    pub embedded_reason: U32,
    pub runtime_reason: U32,

    /// [`TABLE_ABSENT`], [`TABLE_LOADED`], or [`TABLE_UNUSABLE`].
    pub embedded_table: u8,
    pub runtime_table: u8,
    pub flags: u8,

    /// Why the separate debug file could not be used, or [`NONE`]. Its
    /// path is where it was found, which binding the image supplies.
    pub debug_reason: U32,

    /// [`DEBUG_FILE_NONE`], [`DEBUG_FILE_USED`], or
    /// [`DEBUG_FILE_UNUSABLE`].
    pub debug_file: u8,

    /// The addresses the image's segments span.
    pub address_start: U64,
    pub address_end: U64,

    /// Why the DWARF is incomplete or unusable, or [`NONE`].
    pub dwarf_reason: U32,

    /// [`DWARF_ABSENT`], [`DWARF_LOADED`], [`DWARF_INCOMPLETE`], or
    /// [`DWARF_UNUSABLE`].
    pub dwarf: u8,

    /// Why the DWARF's locals were left out, when
    /// [`fact_flags::LOCALS_LEFT_OUT`]; otherwise [`NONE`].
    pub locals_reason: U32,
}

impl Record for FactsRecord {
    const KIND: TableKind = TableKind::Facts;
}

pub const TABLE_ABSENT: u8 = 0;

pub const TABLE_LOADED: u8 = 1;

pub const TABLE_UNUSABLE: u8 = 2;

pub const DEBUG_FILE_NONE: u8 = 0;

pub const DEBUG_FILE_USED: u8 = 1;

pub const DEBUG_FILE_UNUSABLE: u8 = 2;

pub const DWARF_ABSENT: u8 = 0;

pub const DWARF_LOADED: u8 = 1;

pub const DWARF_INCOMPLETE: u8 = 2;

pub const DWARF_UNUSABLE: u8 = 3;

/// [`FactsRecord::flags`].
pub mod fact_flags {
    pub const STATIC_TABLE: u8 = 1 << 0;

    pub const DYNAMIC_TABLE: u8 = 1 << 1;

    pub const THREAD_LOCAL_STORAGE: u8 = 1 << 2;

    pub const LOCALS_LEFT_OUT: u8 = 1 << 3;

    pub const ALL: u8 = STATIC_TABLE | DYNAMIC_TABLE | THREAD_LOCAL_STORAGE | LOCALS_LEFT_OUT;
}

/// One thread-local variable, ordered by its name's bytes.
#[repr(C)]
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, FromBytes, IntoBytes, KnownLayout, Immutable, Unaligned,
)]
pub struct ThreadLocalRecord {
    pub name: U32,

    /// The offset's two's-complement bits, or the slot's address.
    pub value: U64,

    /// Why the place is unknown, when [`PLACE_UNKNOWN`].
    pub reason: U32,

    /// [`PLACE_OFFSET`], [`PLACE_SLOT`], or [`PLACE_UNKNOWN`].
    pub place: u8,
}

impl Record for ThreadLocalRecord {
    const KIND: TableKind = TableKind::ThreadLocals;
}

pub const PLACE_OFFSET: u8 = 0;

pub const PLACE_SLOT: u8 = 1;

pub const PLACE_UNKNOWN: u8 = 2;

/// Why facts could not be encoded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("the image's facts do not fit an image")]
pub struct TooLarge;

/// Why an image cannot be bound: it was built with a separate debug file
/// and binding names none, or the other way round.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("the image was built with another separate debug file")]
pub struct Unbound;

/// What [`add_to`] encodes.
#[derive(Debug)]
pub struct Facts<'a> {
    pub address_range: AddressRange<ImageAddress>,
    pub symbol_sources: &'a SymbolTableSources,
    pub thread_local_storage: bool,
    pub thread_locals: &'a BTreeMap<Arc<str>, Result<ThreadLocal, Arc<str>>>,

    /// The separate debug file found for the image, whose path is left to
    /// binding.
    pub debug_file: Option<&'a DebugFile>,
    pub debug_information: &'a DebugInformation,
    pub locals_left_out: Option<&'a str>,
}

/// Adds `facts` to `builder`, pooling names and reasons in `strings`.
pub fn add_to(
    builder: &mut Builder<'_>,
    strings: &mut StringsBuilder,
    facts: &Facts<'_>,
) -> Result<(), TooLarge> {
    let (debug_file, debug_reason) = match facts.debug_file {
        None => (DEBUG_FILE_NONE, NONE),
        Some(DebugFile::Used(_)) => (DEBUG_FILE_USED, NONE),
        Some(DebugFile::Unusable { reason, .. }) => {
            (DEBUG_FILE_UNUSABLE, strings.push(reason).ok_or(TooLarge)?.0)
        }
    };

    let mut push = |text: &str| strings.push(text).map(|id| id.0).ok_or(TooLarge);
    let (dwarf, dwarf_reason) = match facts.debug_information {
        DebugInformation::Absent => (DWARF_ABSENT, NONE),
        DebugInformation::Loaded => (DWARF_LOADED, NONE),
        DebugInformation::Incomplete { reason } => (DWARF_INCOMPLETE, push(reason)?),
        DebugInformation::Unusable { reason } => (DWARF_UNUSABLE, push(reason)?),
    };

    let locals_reason = facts.locals_left_out.map(&mut push).transpose()?;
    let mut table = |table: &EmbeddedSymbolTable| -> Result<(u8, u32), TooLarge> {
        Ok(match table {
            EmbeddedSymbolTable::Absent => (TABLE_ABSENT, NONE),
            EmbeddedSymbolTable::Loaded => (TABLE_LOADED, NONE),
            EmbeddedSymbolTable::Unusable { reason } => (TABLE_UNUSABLE, push(reason)?),
        })
    };

    let sources = facts.symbol_sources;
    let (embedded_table, embedded_reason) = table(&sources.embedded_table)?;
    let (runtime_table, runtime_reason) = table(&sources.runtime_function_table)?;
    let mut flags = 0;
    for (present, flag) in [
        (sources.static_table, fact_flags::STATIC_TABLE),
        (sources.dynamic_table, fact_flags::DYNAMIC_TABLE),
        (facts.thread_local_storage, fact_flags::THREAD_LOCAL_STORAGE),
        (locals_reason.is_some(), fact_flags::LOCALS_LEFT_OUT),
    ] {
        if present {
            flags |= flag;
        }
    }

    let record = FactsRecord {
        embedded_reason: embedded_reason.into(),
        runtime_reason: runtime_reason.into(),
        embedded_table,
        runtime_table,
        flags,
        debug_reason: debug_reason.into(),
        debug_file,
        address_start: facts.address_range.start.get().into(),
        address_end: facts.address_range.end.get().into(),
        dwarf_reason: dwarf_reason.into(),
        dwarf,
        locals_reason: locals_reason.unwrap_or(NONE).into(),
    };

    // A map's order is its names' byte order, which lookups search.
    let thread_locals = facts
        .thread_locals
        .iter()
        .map(|(name, place)| {
            let (place, value, reason) = match place {
                Ok(ThreadLocal::Offset(offset)) => (PLACE_OFFSET, offset.cast_unsigned(), NONE),
                Ok(ThreadLocal::Slot(address)) => (PLACE_SLOT, address.get(), NONE),
                Err(reason) => (PLACE_UNKNOWN, 0, push(reason)?),
            };

            Ok(ThreadLocalRecord {
                name: push(name)?.into(),
                value: value.into(),
                reason: reason.into(),
                place,
            })
        })
        .collect::<Result<Vec<_>, TooLarge>>()?;

    builder.owned_table(vec![record]).owned_table(thread_locals);
    Ok(())
}

/// The facts of a validated image.
#[derive(Debug, Clone, Copy)]
pub struct FactsView<'a> {
    strings: Strings<'a>,
    record: Option<&'a FactsRecord>,
    thread_locals: &'a [ThreadLocalRecord],
}

impl<'a> FactsView<'a> {
    pub fn new(image: &'a Image) -> Self {
        Self {
            strings: image.strings(),
            record: image.table::<FactsRecord>().first(),
            thread_locals: image.table(),
        }
    }

    fn flag(self, flag: u8) -> bool {
        self.record.is_some_and(|record| record.flags & flag != 0)
    }

    /// The addresses the image's segments span.
    pub fn address_range(self) -> AddressRange<ImageAddress> {
        let (start, end) = self.record.map_or((0, 0), |record| {
            (record.address_start.get(), record.address_end.get())
        });
        AddressRange {
            start: ImageAddress::new(start),
            end: ImageAddress::new(end),
        }
    }

    pub fn symbol_sources(self) -> SymbolTableSources {
        let Some(record) = self.record else {
            return SymbolTableSources::default();
        };

        let table = |state, reason: U32| match state {
            TABLE_LOADED => EmbeddedSymbolTable::Loaded,
            TABLE_UNUSABLE => EmbeddedSymbolTable::Unusable {
                reason: self.strings.get(StrId(reason.get())).into(),
            },
            _ => EmbeddedSymbolTable::Absent,
        };

        SymbolTableSources {
            static_table: self.flag(fact_flags::STATIC_TABLE),
            dynamic_table: self.flag(fact_flags::DYNAMIC_TABLE),
            embedded_table: table(record.embedded_table, record.embedded_reason),
            runtime_function_table: table(record.runtime_table, record.runtime_reason),
        }
    }

    /// The separate debug file found for the image at `path`, whether it
    /// was used or could not be; an error when the image was built with a
    /// debug file and `path` names none, or the other way round.
    pub fn debug_file(self, path: Option<Arc<PathBuf>>) -> Result<Option<DebugFile>, Unbound> {
        let found = self
            .record
            .map_or(DEBUG_FILE_NONE, |record| record.debug_file);
        match (found, path) {
            (DEBUG_FILE_NONE, None) => Ok(None),
            (DEBUG_FILE_USED, Some(path)) => Ok(Some(DebugFile::Used(path))),
            (DEBUG_FILE_UNUSABLE, Some(path)) => Ok(Some(DebugFile::Unusable {
                path,
                reason: self
                    .strings
                    .get(StrId(self.record.ok_or(Unbound)?.debug_reason.get()))
                    .into(),
            })),
            _ => Err(Unbound),
        }
    }

    /// What became of the image's DWARF.
    pub fn debug_information(self) -> DebugInformation {
        let Some(record) = self.record else {
            return DebugInformation::Absent;
        };

        let reason = || self.strings.get(StrId(record.dwarf_reason.get())).into();
        match record.dwarf {
            DWARF_LOADED => DebugInformation::Loaded,
            DWARF_INCOMPLETE => DebugInformation::Incomplete { reason: reason() },
            DWARF_UNUSABLE => DebugInformation::Unusable { reason: reason() },
            _ => DebugInformation::Absent,
        }
    }

    /// Why the DWARF's locals were left out, when they were.
    pub fn locals_left_out(self) -> Option<Arc<str>> {
        let record = self.record?;
        self.flag(fact_flags::LOCALS_LEFT_OUT)
            .then(|| self.strings.get(StrId(record.locals_reason.get())).into())
    }

    pub fn thread_local_storage(self) -> bool {
        self.flag(fact_flags::THREAD_LOCAL_STORAGE)
    }

    /// Every thread-local variable, by name.
    pub fn thread_locals(
        self,
    ) -> impl ExactSizeIterator<Item = (&'a str, Result<ThreadLocal, Arc<str>>)> + 'a {
        self.thread_locals.iter().map(move |record| {
            (
                self.strings.get(StrId(record.name.get())),
                self.place(record),
            )
        })
    }

    /// The thread-local variable `name` names.
    pub fn thread_local(self, name: &str) -> Option<Result<ThreadLocal, Arc<str>>> {
        let index = self
            .thread_locals
            .binary_search_by(|record| {
                self.strings
                    .bytes(StrId(record.name.get()))
                    .cmp(name.as_bytes())
            })
            .ok()?;
        Some(self.place(&self.thread_locals[index]))
    }

    fn place(self, record: &ThreadLocalRecord) -> Result<ThreadLocal, Arc<str>> {
        match record.place {
            PLACE_OFFSET => Ok(ThreadLocal::Offset(record.value.get().cast_signed())),
            PLACE_SLOT => Ok(ThreadLocal::Slot(ImageAddress::new(record.value.get()))),
            _ => Err(self.strings.get(StrId(record.reason.get())).into()),
        }
    }
}

/// Checks the facts and thread-local variables.
pub(super) fn validate(image: &Image) -> Result<(), String> {
    let strings = image.strings();
    let records = image.table::<FactsRecord>();
    let debug_file = |record: &FactsRecord| match record.debug_file {
        DEBUG_FILE_NONE | DEBUG_FILE_USED => record.debug_reason.get() == NONE,
        DEBUG_FILE_UNUSABLE => strings.contains(StrId(record.debug_reason.get())),
        _ => false,
    };

    let dwarf = |record: &FactsRecord| match record.dwarf {
        DWARF_ABSENT | DWARF_LOADED => record.dwarf_reason.get() == NONE,
        DWARF_INCOMPLETE | DWARF_UNUSABLE => strings.contains(StrId(record.dwarf_reason.get())),
        _ => false,
    };

    let locals = |record: &FactsRecord| {
        if record.flags & fact_flags::LOCALS_LEFT_OUT == 0 {
            record.locals_reason.get() == NONE
        } else {
            strings.contains(StrId(record.locals_reason.get()))
        }
    };

    let reason = |state: u8, reason: U32| match state {
        TABLE_ABSENT | TABLE_LOADED => reason.get() == NONE,
        TABLE_UNUSABLE => strings.contains(StrId(reason.get())),
        _ => false,
    };

    if records.len() > 1
        || records.iter().any(|record| {
            record.flags & !fact_flags::ALL != 0
                || !reason(record.embedded_table, record.embedded_reason)
                || !reason(record.runtime_table, record.runtime_reason)
                || !debug_file(record)
                || !dwarf(record)
                || !locals(record)
                || record.address_start.get() > record.address_end.get()
        })
    {
        return Err("the image's facts are malformed".into());
    }

    let thread_locals = image.table::<ThreadLocalRecord>();
    if !thread_locals.iter().all(|record| {
        strings.contains(StrId(record.name.get()))
            && match record.place {
                PLACE_OFFSET | PLACE_SLOT => record.reason.get() == NONE,
                PLACE_UNKNOWN => {
                    record.value.get() == 0 && strings.contains(StrId(record.reason.get()))
                }
                _ => false,
            }
    }) || !thread_locals.is_sorted_by(|earlier, later| {
        strings.bytes(StrId(earlier.name.get())) < strings.bytes(StrId(later.name.get()))
    }) {
        return Err("a thread-local variable is malformed or out of order".into());
    }

    Ok(())
}
