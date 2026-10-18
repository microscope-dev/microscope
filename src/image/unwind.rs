//! What unwinding reads once a module is loaded: the call-frame sections
//! and an index of their entries by address, and Go's function table with
//! where each Go function saves its caller's frame pointer. These are a
//! provider's private tables; DWARF and Go interpret their bytes.

use zerocopy::little_endian::{U32, U64};
use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout, Unaligned};

use super::index::{self, Interval};
use super::strings::{StrId, Strings, StringsBuilder};
use super::{Builder, Image, NONE, Record, TableKind};
use crate::{AddressRange, ImageAddress};

/// Facts about the call-frame sections and Go's table. An image holds one,
/// or none when nothing describes how its code unwinds.
#[repr(C)]
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, FromBytes, IntoBytes, KnownLayout, Immutable, Unaligned,
)]
pub struct UnwindRecord {
    /// The addresses gimli resolves pointer encodings against.
    pub eh_frame_base: U64,
    pub text_base: U64,
    pub got_base: U64,

    /// Where each section's first malformed entry is, [`WHOLE_SECTION`]
    /// when the section cannot be enumerated, and why, or [`NONE`].
    pub eh_frame_error_offset: U64,
    pub debug_frame_error_offset: U64,
    pub eh_frame_error: U32,
    pub debug_frame_error: U32,

    /// Where Go's table is, where its code starts, and where `go:func.*`
    /// is, as Go's table parser reads them.
    pub go_table: U64,
    pub go_text: U64,
    pub go_func: U64,

    /// The Go release that built the image.
    pub go_major: U32,
    pub go_minor: U32,
    pub flags: u8,
    pub address_size: u8,
}

impl Record for UnwindRecord {
    const KIND: TableKind = TableKind::Unwind;
}

/// [`UnwindRecord::flags`].
pub mod unwind_flags {
    pub const EH_FRAME_BASE: u8 = 1 << 0;

    pub const TEXT_BASE: u8 = 1 << 1;

    pub const GOT_BASE: u8 = 1 << 2;

    pub const BIG_ENDIAN: u8 = 1 << 3;

    /// The image has Go's table, in [`super::TableKind::GoTable`].
    pub const GO_TABLE: u8 = 1 << 4;

    pub const GO_FUNC: u8 = 1 << 5;

    pub const GO_RELEASE: u8 = 1 << 6;

    pub const ALL: u8 = (1 << 7) - 1;
}

/// The most words a PLT stub is known to push before it jumps.
pub const MOST_STUB_PUSHES: u32 = 2;

/// The bits of every register a restored return may name: rbx, rbp, and
/// r12-r15.
pub const MOST_RESTORED_REGISTERS: u32 = 0x3f;

/// The error offset of a section that could not be enumerated at all.
pub const WHOLE_SECTION: u64 = u64::MAX;

/// Where one function of Go's table keeps its caller's frame pointer
/// saved, by the function's index in the table.
#[repr(C)]
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, FromBytes, IntoBytes, KnownLayout, Immutable, Unaligned,
)]
pub struct FrameSaveRecord {
    pub start: U64,
    pub end: U64,
    pub saved: u8,
}

impl Record for FrameSaveRecord {
    const KIND: TableKind = TableKind::GoFrameSaves;
}

const _: () = assert!(size_of::<UnwindRecord>() == 82);

/// One call-frame section's entries by the code they describe, and its
/// first malformed entry.
#[derive(Debug, Default)]
pub struct FdeIndex {
    /// Every entry that parses and covers code, as an interval valued by
    /// the entry's offset in its section.
    pub entries: Vec<Interval>,
    pub first_error: Option<(u64, String)>,
}

/// Where Go's table is and what reading it needs, as
/// `GoTable::parse` takes them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GoTableFacts {
    /// The table's image address.
    pub address: u64,

    /// The address function offsets are relative to.
    pub text: u64,

    /// The image address of `go:func.*`, when known.
    pub go_func: Option<u64>,

    /// The Go release that built the image, as (major, minor).
    pub release: Option<(u32, u32)>,
}

/// Go's table, as an image keeps it.
#[derive(Debug)]
pub struct GoTableData {
    pub bytes: std::sync::Arc<[u8]>,
    pub facts: GoTableFacts,

    /// Where each function keeps its caller's frame pointer saved, by the
    /// function's index in the table.
    pub frame_saves: Vec<Option<std::ops::Range<u64>>>,
}

/// The addresses gimli resolves pointer encodings against.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Bases {
    pub eh_frame: Option<u64>,
    pub text: Option<u64>,
    pub got: Option<u64>,
}

/// What [`add_to`] encodes.
#[derive(Debug, Default)]
pub struct Unwind {
    pub eh_frame: std::sync::Arc<[u8]>,
    pub debug_frame: std::sync::Arc<[u8]>,
    pub eh_frame_index: FdeIndex,
    pub debug_frame_index: FdeIndex,
    pub bases: Bases,
    pub big_endian: bool,
    pub address_size: u8,

    /// Code the Go toolchain compiled.
    pub go_code: Vec<AddressRange<ImageAddress>>,
    pub go: Option<GoTableData>,

    /// Each PLT stub instruction, by how many words the stub has pushed
    /// before it.
    pub stub_frames: Vec<(AddressRange<ImageAddress>, u32)>,

    /// The instructions at which registers a function's caller keeps are
    /// already restored, though its call-frame information still names
    /// the slots it saved them in, each with the registers as bits of
    /// rbx, rbp, and r12-r15.
    pub restored: Vec<(AddressRange<ImageAddress>, u32)>,
}

/// Why unwinding information could not be encoded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("the call-frame information does not fit an image")]
pub struct TooLarge;

/// Adds `unwind` to `builder`, pooling error descriptions in `strings`.
pub fn add_to<'a>(
    builder: &mut Builder<'a>,
    strings: &mut StringsBuilder,
    unwind: &'a Unwind,
) -> Result<(), TooLarge> {
    let mut error = |index: &FdeIndex| -> Result<(u64, u32), TooLarge> {
        Ok(match &index.first_error {
            Some((offset, text)) => (*offset, strings.push(text).ok_or(TooLarge)?.0),
            None => (0, NONE),
        })
    };

    // Each entry's offset is an interval's 32-bit value.
    if [&unwind.eh_frame, &unwind.debug_frame]
        .iter()
        .any(|section| u32::try_from(section.len()).is_err())
    {
        return Err(TooLarge);
    }

    let (eh_frame_error_offset, eh_frame_error) = error(&unwind.eh_frame_index)?;
    let (debug_frame_error_offset, debug_frame_error) = error(&unwind.debug_frame_index)?;
    let mut flags = 0;
    for (present, flag) in [
        (unwind.bases.eh_frame.is_some(), unwind_flags::EH_FRAME_BASE),
        (unwind.bases.text.is_some(), unwind_flags::TEXT_BASE),
        (unwind.bases.got.is_some(), unwind_flags::GOT_BASE),
        (unwind.big_endian, unwind_flags::BIG_ENDIAN),
        (unwind.go.is_some(), unwind_flags::GO_TABLE),
        (
            unwind
                .go
                .as_ref()
                .is_some_and(|go| go.facts.go_func.is_some()),
            unwind_flags::GO_FUNC,
        ),
        (
            unwind
                .go
                .as_ref()
                .is_some_and(|go| go.facts.release.is_some()),
            unwind_flags::GO_RELEASE,
        ),
    ] {
        if present {
            flags |= flag;
        }
    }

    let go = unwind.go.as_ref();
    let facts = go.map(|go| go.facts);
    let release = facts.and_then(|facts| facts.release).unwrap_or_default();
    let record = UnwindRecord {
        eh_frame_base: unwind.bases.eh_frame.unwrap_or(0).into(),
        text_base: unwind.bases.text.unwrap_or(0).into(),
        got_base: unwind.bases.got.unwrap_or(0).into(),
        eh_frame_error_offset: eh_frame_error_offset.into(),
        debug_frame_error_offset: debug_frame_error_offset.into(),
        eh_frame_error: eh_frame_error.into(),
        debug_frame_error: debug_frame_error.into(),
        go_table: facts.map_or(0, |facts| facts.address).into(),
        go_text: facts.map_or(0, |facts| facts.text).into(),
        go_func: facts.and_then(|facts| facts.go_func).unwrap_or(0).into(),
        go_major: release.0.into(),
        go_minor: release.1.into(),
        flags,
        address_size: unwind.address_size,
    };

    let go_code = index::intervals(unwind.go_code.iter().map(|range| (*range, 0)));
    builder
        .owned_table(vec![record])
        .bytes(TableKind::EhFrame, unwind.eh_frame.to_vec())
        .bytes(TableKind::DebugFrame, unwind.debug_frame.to_vec())
        .shared(TableKind::EhFrameIndex, &unwind.eh_frame_index.entries)
        .shared(
            TableKind::DebugFrameIndex,
            &unwind.debug_frame_index.entries,
        )
        .owned_shared(TableKind::GoCode, go_code)
        .owned_shared(
            TableKind::StubFrames,
            index::intervals(unwind.stub_frames.iter().copied()),
        )
        .owned_shared(
            TableKind::RestoredFrames,
            index::intervals(unwind.restored.iter().copied()),
        );
    if let Some(go) = go {
        let saves = go
            .frame_saves
            .iter()
            .map(|save| {
                let (start, end, saved) = save
                    .as_ref()
                    .map_or((0, 0, 0), |range| (range.start, range.end, 1));
                FrameSaveRecord {
                    start: start.into(),
                    end: end.into(),
                    saved,
                }
            })
            .collect::<Vec<_>>();
        builder
            .bytes(TableKind::GoTable, go.bytes.to_vec())
            .owned_table(saves);
    }

    Ok(())
}

/// Why no entry describes an address.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FdeMiss<'a> {
    /// No entry covers it.
    NoEntry,

    /// The section is malformed before any entry covering it: the
    /// description of the first problem.
    Malformed(&'a str),
}

/// An index of one section's entries, as a lookup reads it.
#[derive(Debug, Clone, Copy)]
pub struct FdeLookup<'a> {
    pub entries: &'a [Interval],
    pub first_error: Option<(u64, &'a str)>,
}

impl<'a> FdeLookup<'a> {
    /// The section offset of the first entry describing `address`, unless
    /// the section is malformed before it.
    pub fn lookup(self, address: u64) -> Result<usize, FdeMiss<'a>> {
        let first = index::containing(self.entries, ImageAddress::new(address)).min();
        match (first, self.first_error) {
            (Some(offset), Some((error_offset, _))) if u64::from(offset) < error_offset => {
                Ok(offset as usize)
            }
            (Some(offset), None) => Ok(offset as usize),
            (_, Some((_, error))) => Err(FdeMiss::Malformed(error)),
            (None, None) => Err(FdeMiss::NoEntry),
        }
    }
}

impl FdeIndex {
    /// The index as a lookup reads it, before the image is sealed.
    #[cfg(test)]
    pub fn lookup(&self) -> FdeLookup<'_> {
        FdeLookup {
            entries: &self.entries,
            first_error: self
                .first_error
                .as_ref()
                .map(|(offset, error)| (*offset, error.as_str())),
        }
    }
}

/// The unwinding tables of a validated image.
#[derive(Debug, Clone, Copy)]
pub struct UnwindView<'a> {
    strings: Strings<'a>,
    record: Option<&'a UnwindRecord>,
    image: &'a Image,
}

impl<'a> UnwindView<'a> {
    pub fn new(image: &'a Image) -> Self {
        Self {
            strings: image.strings(),
            record: image.table::<UnwindRecord>().first(),
            image,
        }
    }

    fn flag(self, flag: u8) -> bool {
        self.record.is_some_and(|record| record.flags & flag != 0)
    }

    fn base(self, flag: u8, value: impl Fn(&UnwindRecord) -> U64) -> Option<u64> {
        self.flag(flag)
            .then(|| self.record.map(|record| value(record).get()))
            .flatten()
    }

    pub fn eh_frame_base(self) -> Option<u64> {
        self.base(unwind_flags::EH_FRAME_BASE, |record| record.eh_frame_base)
    }

    pub fn text_base(self) -> Option<u64> {
        self.base(unwind_flags::TEXT_BASE, |record| record.text_base)
    }

    pub fn got_base(self) -> Option<u64> {
        self.base(unwind_flags::GOT_BASE, |record| record.got_base)
    }

    pub fn big_endian(self) -> bool {
        self.flag(unwind_flags::BIG_ENDIAN)
    }

    pub fn address_size(self) -> u8 {
        self.record.map_or(8, |record| record.address_size)
    }

    pub fn eh_frame(self) -> &'a [u8] {
        self.image.bytes(TableKind::EhFrame)
    }

    pub fn debug_frame(self) -> &'a [u8] {
        self.image.bytes(TableKind::DebugFrame)
    }

    fn error(self, offset: u64, text: u32) -> Option<(u64, &'a str)> {
        (text != NONE).then(|| (offset, self.strings.get(StrId(text))))
    }

    pub fn eh_frame_index(self) -> FdeLookup<'a> {
        FdeLookup {
            entries: self.image.shared(TableKind::EhFrameIndex),
            first_error: self.record.and_then(|record| {
                self.error(
                    record.eh_frame_error_offset.get(),
                    record.eh_frame_error.get(),
                )
            }),
        }
    }

    pub fn debug_frame_index(self) -> FdeLookup<'a> {
        FdeLookup {
            entries: self.image.shared(TableKind::DebugFrameIndex),
            first_error: self.record.and_then(|record| {
                self.error(
                    record.debug_frame_error_offset.get(),
                    record.debug_frame_error.get(),
                )
            }),
        }
    }

    /// Whether the Go toolchain compiled the code at `address`, by its
    /// debug information.
    pub fn is_go_code(self, address: ImageAddress) -> bool {
        index::containing(self.image.shared(TableKind::GoCode), address)
            .next()
            .is_some()
    }

    /// How many words the PLT stub running `address` has pushed since
    /// its call, when `address` is one of a stub's instructions.
    pub fn stub_pushes(self, address: ImageAddress) -> Option<u32> {
        index::containing(self.image.shared(TableKind::StubFrames), address).next()
    }

    /// The registers its caller keeps that the function running `address`
    /// has already restored there, as bits of rbx, rbp, and r12-r15.
    pub fn restored(self, address: ImageAddress) -> u32 {
        index::containing(self.image.shared(TableKind::RestoredFrames), address)
            .fold(0, |registers, more| registers | more)
    }

    /// Go's table, when the image has it.
    pub fn go(self) -> Option<(&'a [u8], GoTableFacts)> {
        let record = self.record?;
        self.flag(unwind_flags::GO_TABLE).then(|| {
            (
                self.image.bytes(TableKind::GoTable),
                GoTableFacts {
                    address: record.go_table.get(),
                    text: record.go_text.get(),
                    go_func: self
                        .flag(unwind_flags::GO_FUNC)
                        .then(|| record.go_func.get()),
                    release: self
                        .flag(unwind_flags::GO_RELEASE)
                        .then(|| (record.go_major.get(), record.go_minor.get())),
                },
            )
        })
    }

    /// Where each function of Go's table keeps its caller's frame pointer
    /// saved.
    pub fn frame_saves(self) -> impl ExactSizeIterator<Item = Option<std::ops::Range<u64>>> + 'a {
        self.image
            .table::<FrameSaveRecord>()
            .iter()
            .map(|save| (save.saved != 0).then(|| save.start.get()..save.end.get()))
    }
}

/// Checks the unwinding tables.
pub(super) fn validate(image: &Image) -> Result<(), String> {
    let strings = image.strings();
    let records = image.table::<UnwindRecord>();
    let Some(record) = records.first() else {
        return if [
            TableKind::EhFrame,
            TableKind::DebugFrame,
            TableKind::GoTable,
        ]
        .iter()
        .all(|kind| image.bytes(*kind).is_empty())
            && image.table::<FrameSaveRecord>().is_empty()
        {
            Ok(())
        } else {
            Err("unwinding tables without their facts".into())
        };
    };

    let flag = |flag| record.flags & flag != 0;
    let error = |offset: U64, text: U32, section: usize| {
        if text.get() == NONE {
            offset.get() == 0
        } else {
            strings.contains(StrId(text.get()))
                && (offset.get() == WHOLE_SECTION || offset.get() < section as u64)
        }
    };

    let unset = |present: bool, value: U64| present || value.get() == 0;
    let eh_frame = image.bytes(TableKind::EhFrame).len();
    let debug_frame = image.bytes(TableKind::DebugFrame).len();
    let go = flag(unwind_flags::GO_TABLE);
    if records.len() != 1
        || record.flags & !unwind_flags::ALL != 0
        || !matches!(record.address_size, 4 | 8)
        || !error(
            record.eh_frame_error_offset,
            record.eh_frame_error,
            eh_frame,
        )
        || !error(
            record.debug_frame_error_offset,
            record.debug_frame_error,
            debug_frame,
        )
        || !unset(flag(unwind_flags::EH_FRAME_BASE), record.eh_frame_base)
        || !unset(flag(unwind_flags::TEXT_BASE), record.text_base)
        || !unset(flag(unwind_flags::GOT_BASE), record.got_base)
        || !unset(go, record.go_table)
        || !unset(go, record.go_text)
        || !unset(flag(unwind_flags::GO_FUNC), record.go_func)
        || (!flag(unwind_flags::GO_RELEASE)
            && (record.go_major.get() != 0 || record.go_minor.get() != 0))
        || (!go
            && (flag(unwind_flags::GO_FUNC)
                || flag(unwind_flags::GO_RELEASE)
                || !image.bytes(TableKind::GoTable).is_empty()
                || !image.table::<FrameSaveRecord>().is_empty()))
    {
        return Err("the unwinding facts are malformed".into());
    }

    if !index::valid_intervals(image.shared(TableKind::EhFrameIndex), eh_frame)
        || !index::valid_intervals(image.shared(TableKind::DebugFrameIndex), debug_frame)
        || !index::valid_intervals(image.shared(TableKind::GoCode), 1)
        || !index::valid_intervals(
            image.shared(TableKind::StubFrames),
            MOST_STUB_PUSHES as usize + 1,
        )
        || !index::valid_intervals(
            image.shared(TableKind::RestoredFrames),
            MOST_RESTORED_REGISTERS as usize + 1,
        )
    {
        return Err("an unwinding index is malformed".into());
    }

    if !image
        .table::<FrameSaveRecord>()
        .iter()
        .all(|save| match save.saved {
            0 => save.start.get() == 0 && save.end.get() == 0,
            1 => save.start.get() <= save.end.get(),
            _ => false,
        })
    {
        return Err("a Go frame save is malformed".into());
    }

    Ok(())
}
