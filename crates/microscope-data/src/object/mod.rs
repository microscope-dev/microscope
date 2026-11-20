pub mod codec;
pub mod tables;
#[cfg(test)]
mod tests;

use serde::{Deserialize, Serialize};
use twox_hash::XxHash3_64;
use zerocopy::little_endian::{I64, U16, U32, U64};
use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout, Unaligned};

pub use codec::{
    Codec, Coding, ColumnEntry, GroupEntry, SectionHeader, SectionReader, SectionWriter,
};
pub use tables::{
    ChunkData, ChunkRow, DimensionRow, SanitizerCountRow, decode_flame_tables, decode_tables,
    encode_tables, merge_chunks, round_capture_times,
};

pub const OBJECT_MAGIC: [u8; 8] = *b"USCOPEOB";
pub const PART_LIST_MAGIC: [u8; 8] = *b"USCOPEPL";
pub const TRAILER_MAGIC: [u8; 8] = *b"USCOPEOE";
pub const OBJECT_VERSION: u32 = 1;

pub const OBJECT_HEADER_LEN: usize = 32;
pub const SECTION_HEADER_LEN: usize = 24;
pub const GROUP_ENTRY_LEN: usize = 8;
pub const COLUMN_ENTRY_LEN: usize = 12;
pub const PART_LIST_HEADER_LEN: usize = 16;
pub const PART_ENTRY_LEN: usize = 64;
pub const SECTION_ENTRY_LEN: usize = 40;
pub const TRAILER_LEN: usize = 24;

pub const MAX_OBJECT_BYTES: usize = 64 << 20;
pub const MAX_PARTS: usize = 16_384;
pub const MAX_SECTIONS: usize = 65_536;
pub const MAX_GROUPS: usize = 4_096;
pub const MAX_COLUMNS: usize = 16_384;
pub const MAX_ROWS: usize = 4_000_000;
pub const MAX_COLUMN_RAW_BYTES: usize = 32 << 20;
pub const MAX_DECOMPRESSED_OBJECT_BYTES: usize = 128 << 20;

pub const DEFAULT_ZSTD_LEVEL: i32 = 3;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ObjectError {
    #[error("object is truncated")]
    Truncated,

    #[error("bad magic bytes")]
    BadMagic,

    #[error("unsupported object version {0}")]
    UnsupportedVersion(u32),

    #[error("unknown {what}: {value}")]
    UnknownValue { what: &'static str, value: u64 },

    #[error("checksum mismatch for {what}")]
    Checksum { what: &'static str },

    #[error("reserved field is non-zero in {0}")]
    NonZeroReserved(&'static str),

    #[error("limit exceeded: {0}")]
    Limit(&'static str),

    #[error("organization mismatch: expected {expected}, found {found}")]
    OrgMismatch { expected: u64, found: u64 },

    #[error("invalid layout: {0}")]
    Layout(String),

    #[error("compression error: {0}")]
    Compression(String),

    #[error("invalid capture: {0}")]
    Capture(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u8)]
pub enum ObjectKind {
    Post = 1,
    WriteAhead = 2,
    Sums = 3,
    Detail = 4,
}

impl ObjectKind {
    #[must_use]
    pub const fn from_number(value: u8) -> Option<Self> {
        match value {
            1 => Some(Self::Post),
            2 => Some(Self::WriteAhead),
            3 => Some(Self::Sums),
            4 => Some(Self::Detail),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u8)]
pub enum ObjectLevel {
    None = 0,
    Minute = 1,
    TenMinutes = 2,
    Hour = 3,
    Day = 4,
    Pin = 5,
}

impl ObjectLevel {
    #[must_use]
    pub const fn from_number(value: u8) -> Option<Self> {
        match value {
            0 => Some(Self::None),
            1 => Some(Self::Minute),
            2 => Some(Self::TenMinutes),
            3 => Some(Self::Hour),
            4 => Some(Self::Day),
            5 => Some(Self::Pin),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u8)]
pub enum Signal {
    Profile = 1,
}

impl Signal {
    #[must_use]
    pub const fn from_number(value: u8) -> Option<Self> {
        match value {
            1 => Some(Self::Profile),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u8)]
pub enum PartKind {
    Chunks = 1,
    Sums = 2,
    Detail = 3,
}

impl PartKind {
    #[must_use]
    pub const fn from_number(value: u8) -> Option<Self> {
        match value {
            1 => Some(Self::Chunks),
            2 => Some(Self::Sums),
            3 => Some(Self::Detail),
            _ => None,
        }
    }
}

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default, Serialize, Deserialize,
)]
#[repr(u8)]
pub enum TimeUnit {
    #[default]
    Ns = 0,
    Us = 3,
    Ms = 6,
}

impl TimeUnit {
    #[must_use]
    pub const fn from_number(value: u8) -> Option<Self> {
        match value {
            0 => Some(Self::Ns),
            3 => Some(Self::Us),
            6 => Some(Self::Ms),
            _ => None,
        }
    }

    #[must_use]
    pub const fn nanos(self) -> i64 {
        match self {
            Self::Ns => 1,
            Self::Us => 1_000,
            Self::Ms => 1_000_000,
        }
    }

    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Ns => "1ns",
            Self::Us => "1us",
            Self::Ms => "1ms",
        }
    }

    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        match text.trim() {
            "1ns" | "ns" => Some(Self::Ns),
            "1us" | "1µs" | "us" | "µs" => Some(Self::Us),
            "1ms" | "ms" => Some(Self::Ms),
            _ => None,
        }
    }

    #[must_use]
    pub const fn to_units(self, nanos: i64) -> i64 {
        nanos.div_euclid(self.nanos())
    }

    #[must_use]
    pub const fn to_nanos(self, units: i64) -> i64 {
        units.saturating_mul(self.nanos())
    }

    #[must_use]
    pub const fn round_down(self, nanos: i64) -> i64 {
        let unit = self.nanos();
        nanos.div_euclid(unit).saturating_mul(unit)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[repr(u16)]
pub enum SectionKind {
    Strings = 1,
    Settings = 2,
    Modules = 3,
    Processes = 4,
    Threads = 5,
    Mappings = 6,
    Frames = 7,
    Stacks = 8,
    Samples = 9,
    SampleRanges = 10,
    FrameSymbols = 11,
    Lines = 12,
    Functions = 13,
    Accounting = 14,
    OffCpu = 15,
    OffCpuRanges = 16,
    Sites = 17,
    Zones = 18,
    ZoneRanges = 19,
    Plots = 20,
    Markers = 21,
    Tasks = 22,
    TaskWaits = 23,
    Gaps = 24,
    Syscalls = 25,
    SyscallBlobs = 26,
    SyscallNotes = 27,
    SyscallStats = 28,
    SyscallErrnos = 29,
    Chunk = 30,
    Dimensions = 31,
    SanitizerCounts = 32,
    Groups = 64,
    Summary = 65,
    GroupTree = 66,
    FunctionIndex = 67,
    CallSums = 68,
    ZoneSums = 69,
    MachineIndex = 80,
    ProcessTree = 81,
}

impl SectionKind {
    #[must_use]
    pub const fn from_number(value: u16) -> Option<Self> {
        match value {
            1 => Some(Self::Strings),
            2 => Some(Self::Settings),
            3 => Some(Self::Modules),
            4 => Some(Self::Processes),
            5 => Some(Self::Threads),
            6 => Some(Self::Mappings),
            7 => Some(Self::Frames),
            8 => Some(Self::Stacks),
            9 => Some(Self::Samples),
            10 => Some(Self::SampleRanges),
            11 => Some(Self::FrameSymbols),
            12 => Some(Self::Lines),
            13 => Some(Self::Functions),
            14 => Some(Self::Accounting),
            15 => Some(Self::OffCpu),
            16 => Some(Self::OffCpuRanges),
            17 => Some(Self::Sites),
            18 => Some(Self::Zones),
            19 => Some(Self::ZoneRanges),
            20 => Some(Self::Plots),
            21 => Some(Self::Markers),
            22 => Some(Self::Tasks),
            23 => Some(Self::TaskWaits),
            24 => Some(Self::Gaps),
            25 => Some(Self::Syscalls),
            26 => Some(Self::SyscallBlobs),
            27 => Some(Self::SyscallNotes),
            28 => Some(Self::SyscallStats),
            29 => Some(Self::SyscallErrnos),
            30 => Some(Self::Chunk),
            31 => Some(Self::Dimensions),
            32 => Some(Self::SanitizerCounts),
            64 => Some(Self::Groups),
            65 => Some(Self::Summary),
            66 => Some(Self::GroupTree),
            67 => Some(Self::FunctionIndex),
            68 => Some(Self::CallSums),
            69 => Some(Self::ZoneSums),
            80 => Some(Self::MachineIndex),
            81 => Some(Self::ProcessTree),
            _ => None,
        }
    }

    #[must_use]
    pub const fn needed_for_flame(self) -> bool {
        matches!(
            self,
            Self::Strings
                | Self::Settings
                | Self::Modules
                | Self::Processes
                | Self::Threads
                | Self::Frames
                | Self::Stacks
                | Self::Samples
                | Self::SampleRanges
                | Self::FrameSymbols
                | Self::Lines
                | Self::Functions
                | Self::Chunk
                | Self::Dimensions
        )
    }
}

#[repr(C)]
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, FromBytes, IntoBytes, KnownLayout, Immutable, Unaligned,
)]
pub struct ObjectHeader {
    pub magic: [u8; 8],
    pub version: U32,
    pub kind: u8,
    pub level: u8,
    pub reserved: U16,
    pub created_at: I64,
    pub writer: U64,
}

#[repr(C)]
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, FromBytes, IntoBytes, KnownLayout, Immutable, Unaligned,
)]
pub struct PartListHeader {
    pub magic: [u8; 8],
    pub parts: U32,
    pub sections: U32,
}

#[repr(C)]
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, FromBytes, IntoBytes, KnownLayout, Immutable, Unaligned,
)]
pub struct PartEntry {
    pub org_id: U64,
    pub project_id: U64,
    pub starts_at: I64,
    pub ends_at: I64,
    pub byte_start: U64,
    pub byte_len: U64,
    pub first_section: U32,
    pub section_count: U32,
    pub signal: u8,
    pub part: u8,
    pub reserved: [u8; 6],
}

#[repr(C)]
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, FromBytes, IntoBytes, KnownLayout, Immutable, Unaligned,
)]
pub struct SectionEntry {
    pub offset: U64,
    pub stored_len: U32,
    pub rows: U32,
    pub key: U64,
    pub checksum: U64,
    pub kind: U16,
    pub reserved: [u8; 6],
}

#[repr(C)]
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, FromBytes, IntoBytes, KnownLayout, Immutable, Unaligned,
)]
pub struct ObjectTrailer {
    pub list_len: U64,
    pub list_checksum: U64,
    pub magic: [u8; 8],
}

const _: () = assert!(size_of::<ObjectHeader>() == OBJECT_HEADER_LEN);
const _: () = assert!(size_of::<PartListHeader>() == PART_LIST_HEADER_LEN);
const _: () = assert!(size_of::<PartEntry>() == PART_ENTRY_LEN);
const _: () = assert!(size_of::<SectionEntry>() == SECTION_ENTRY_LEN);
const _: () = assert!(size_of::<ObjectTrailer>() == TRAILER_LEN);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EncodedSection {
    pub kind: SectionKind,
    pub rows: u32,
    pub key: u64,
    pub bytes: Vec<u8>,
}

impl EncodedSection {
    #[must_use]
    pub fn checksum(&self) -> u64 {
        XxHash3_64::oneshot(&self.bytes)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SectionMeta {
    pub kind: u16,
    pub key: u64,
    pub offset: u64,
    pub stored_len: u32,
    pub rows: u32,
    pub checksum: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PartMeta {
    pub org_id: u64,
    pub project_id: u64,
    pub starts_at: i64,
    pub ends_at: i64,
    pub byte_start: u64,
    pub byte_len: u64,
    pub signal: Signal,
    pub part: PartKind,
    pub sections: Vec<SectionMeta>,
}

#[derive(Debug, Clone, Copy)]
pub struct RawSection<'a> {
    pub kind: SectionKind,
    pub key: u64,
    pub rows: u32,
    pub checksum: u64,
    pub bytes: &'a [u8],
}

#[derive(Debug, Clone)]
pub struct PartSpec {
    pub org_id: u64,
    pub project_id: u64,
    pub starts_at: i64,
    pub ends_at: i64,
    pub signal: Signal,
    pub part: PartKind,
    pub sections: Vec<EncodedSection>,
}

pub struct ObjectWriter {
    kind: ObjectKind,
    level: ObjectLevel,
    created_at: i64,
    writer: u64,
    bytes: Vec<u8>,
    parts: Vec<PartEntry>,
    sections: Vec<SectionEntry>,
    part_metas: Vec<PartMeta>,
}

impl ObjectWriter {
    #[must_use]
    pub fn new(kind: ObjectKind, level: ObjectLevel, created_at: i64, writer: u64) -> Self {
        let header = ObjectHeader {
            magic: OBJECT_MAGIC,
            version: OBJECT_VERSION.into(),
            kind: kind as u8,
            level: level as u8,
            reserved: 0.into(),
            created_at: created_at.into(),
            writer: writer.into(),
        };
        let mut bytes = Vec::with_capacity(4096);
        bytes.extend_from_slice(header.as_bytes());
        Self {
            kind,
            level,
            created_at,
            writer,
            bytes,
            parts: Vec::new(),
            sections: Vec::new(),
            part_metas: Vec::new(),
        }
    }

    pub fn add_part(&mut self, spec: &PartSpec) -> Result<(), ObjectError> {
        if self.parts.len() >= MAX_PARTS {
            return Err(ObjectError::Limit("too many parts"));
        }

        let first_section = u32::try_from(self.sections.len())
            .map_err(|_| ObjectError::Limit("too many sections"))?;
        let byte_start = u64::try_from(self.bytes.len())
            .map_err(|_| ObjectError::Limit("object offset overflow"))?;
        let mut section_metas = Vec::with_capacity(spec.sections.len());

        for section in &spec.sections {
            if self.sections.len() >= MAX_SECTIONS {
                return Err(ObjectError::Limit("too many sections"));
            }

            let offset = u64::try_from(self.bytes.len())
                .map_err(|_| ObjectError::Limit("object offset overflow"))?;
            let stored_len = u32::try_from(section.bytes.len())
                .map_err(|_| ObjectError::Limit("section exceeds u32 length"))?;
            let checksum = section.checksum();
            self.bytes.extend_from_slice(&section.bytes);
            self.sections.push(SectionEntry {
                offset: offset.into(),
                stored_len: stored_len.into(),
                rows: section.rows.into(),
                key: section.key.into(),
                checksum: checksum.into(),
                kind: (section.kind as u16).into(),
                reserved: [0; 6],
            });
            section_metas.push(SectionMeta {
                kind: section.kind as u16,
                key: section.key,
                offset,
                stored_len,
                rows: section.rows,
                checksum,
            });
        }

        let byte_end = u64::try_from(self.bytes.len())
            .map_err(|_| ObjectError::Limit("object offset overflow"))?;
        let byte_len = byte_end - byte_start;
        let section_count = u32::try_from(spec.sections.len())
            .map_err(|_| ObjectError::Limit("too many sections"))?;

        self.parts.push(PartEntry {
            org_id: spec.org_id.into(),
            project_id: spec.project_id.into(),
            starts_at: spec.starts_at.into(),
            ends_at: spec.ends_at.into(),
            byte_start: byte_start.into(),
            byte_len: byte_len.into(),
            first_section: first_section.into(),
            section_count: section_count.into(),
            signal: spec.signal as u8,
            part: spec.part as u8,
            reserved: [0; 6],
        });
        self.part_metas.push(PartMeta {
            org_id: spec.org_id,
            project_id: spec.project_id,
            starts_at: spec.starts_at,
            ends_at: spec.ends_at,
            byte_start,
            byte_len,
            signal: spec.signal,
            part: spec.part,
            sections: section_metas,
        });
        Ok(())
    }

    pub fn finish(mut self) -> Result<(Vec<u8>, Vec<PartMeta>), ObjectError> {
        let list_start = self.bytes.len();
        let list_header = PartListHeader {
            magic: PART_LIST_MAGIC,
            parts: u32::try_from(self.parts.len())
                .map_err(|_| ObjectError::Limit("too many parts"))?
                .into(),
            sections: u32::try_from(self.sections.len())
                .map_err(|_| ObjectError::Limit("too many sections"))?
                .into(),
        };
        self.bytes.extend_from_slice(list_header.as_bytes());
        self.bytes.extend_from_slice(self.parts.as_bytes());
        self.bytes.extend_from_slice(self.sections.as_bytes());

        let list_bytes = self.bytes.get(list_start..).ok_or(ObjectError::Truncated)?;
        let list_len = u64::try_from(list_bytes.len())
            .map_err(|_| ObjectError::Limit("part list too large"))?;
        let list_checksum = XxHash3_64::oneshot(list_bytes);
        let trailer = ObjectTrailer {
            list_len: list_len.into(),
            list_checksum: list_checksum.into(),
            magic: TRAILER_MAGIC,
        };
        self.bytes.extend_from_slice(trailer.as_bytes());

        debug_assert_eq!(self.bytes.get(..8), Some(OBJECT_MAGIC.as_slice()));
        debug_assert_eq!(self.kind as u8, self.bytes[12]);
        debug_assert_eq!(self.level as u8, self.bytes[13]);
        debug_assert_eq!(
            i64::from_le_bytes(self.bytes[16..24].try_into().unwrap_or_default()),
            self.created_at
        );
        debug_assert_eq!(
            u64::from_le_bytes(self.bytes[24..32].try_into().unwrap_or_default()),
            self.writer
        );

        Ok((self.bytes, self.part_metas))
    }
}

#[derive(Debug)]
pub struct ObjectReader<'a> {
    bytes: &'a [u8],
    pub kind: ObjectKind,
    pub level: ObjectLevel,
    pub created_at: i64,
    pub writer: u64,
    parts: Vec<PartMeta>,
}

impl<'a> ObjectReader<'a> {
    pub fn open(bytes: &'a [u8]) -> Result<Self, ObjectError> {
        if bytes.len() > MAX_OBJECT_BYTES {
            return Err(ObjectError::Limit("object exceeds maximum size"));
        }

        if bytes.len() < OBJECT_HEADER_LEN + PART_LIST_HEADER_LEN + TRAILER_LEN {
            return Err(ObjectError::Truncated);
        }

        let header = ObjectHeader::read_from_bytes(
            bytes
                .get(..OBJECT_HEADER_LEN)
                .ok_or(ObjectError::Truncated)?,
        )
        .map_err(|_| ObjectError::Truncated)?;
        if header.magic != OBJECT_MAGIC {
            return Err(ObjectError::BadMagic);
        }

        let version = header.version.get();
        if version != OBJECT_VERSION {
            return Err(ObjectError::UnsupportedVersion(version));
        }

        if header.reserved.get() != 0 {
            return Err(ObjectError::NonZeroReserved("object header"));
        }

        let kind =
            ObjectKind::from_number(header.kind).ok_or_else(|| ObjectError::UnknownValue {
                what: "object kind",
                value: u64::from(header.kind),
            })?;
        let level =
            ObjectLevel::from_number(header.level).ok_or_else(|| ObjectError::UnknownValue {
                what: "object level",
                value: u64::from(header.level),
            })?;

        let trailer_start = bytes.len() - TRAILER_LEN;
        let trailer = ObjectTrailer::read_from_bytes(
            bytes.get(trailer_start..).ok_or(ObjectError::Truncated)?,
        )
        .map_err(|_| ObjectError::Truncated)?;
        if trailer.magic != TRAILER_MAGIC {
            return Err(ObjectError::BadMagic);
        }

        let list_len = usize::try_from(trailer.list_len.get())
            .map_err(|_| ObjectError::Limit("part list length overflow"))?;
        if list_len < PART_LIST_HEADER_LEN
            || list_len > trailer_start.saturating_sub(OBJECT_HEADER_LEN)
        {
            return Err(ObjectError::Truncated);
        }

        let list_start = trailer_start - list_len;
        let list_bytes = bytes
            .get(list_start..trailer_start)
            .ok_or(ObjectError::Truncated)?;
        if XxHash3_64::oneshot(list_bytes) != trailer.list_checksum.get() {
            return Err(ObjectError::Checksum { what: "part list" });
        }

        let parts = parse_part_list(list_bytes, list_start as u64)?;
        Ok(Self {
            bytes,
            kind,
            level,
            created_at: header.created_at.get(),
            writer: header.writer.get(),
            parts,
        })
    }

    #[must_use]
    pub fn parts(&self) -> &[PartMeta] {
        &self.parts
    }

    pub fn part_sections(
        &self,
        part_index: usize,
        expected_org: Option<u64>,
    ) -> Result<Vec<RawSection<'a>>, ObjectError> {
        let part = self
            .parts
            .get(part_index)
            .ok_or_else(|| ObjectError::Layout(format!("part {part_index} is out of bounds")))?;
        if let Some(expected) = expected_org
            && part.org_id != expected
        {
            return Err(ObjectError::OrgMismatch {
                expected,
                found: part.org_id,
            });
        }

        let mut out = Vec::with_capacity(part.sections.len());
        for entry in &part.sections {
            out.push(self.read_section(entry)?);
        }

        Ok(out)
    }

    pub fn read_section(&self, entry: &SectionMeta) -> Result<RawSection<'a>, ObjectError> {
        read_section_from_slice(self.bytes, 0, entry)
    }
}

pub fn read_section_from_slice<'a>(
    slice: &'a [u8],
    slice_base_offset: u64,
    entry: &SectionMeta,
) -> Result<RawSection<'a>, ObjectError> {
    let kind = SectionKind::from_number(entry.kind).ok_or_else(|| ObjectError::UnknownValue {
        what: "section kind",
        value: u64::from(entry.kind),
    })?;
    let rel_start = entry
        .offset
        .checked_sub(slice_base_offset)
        .ok_or_else(|| ObjectError::Layout("section offset precedes slice".into()))?;
    let start =
        usize::try_from(rel_start).map_err(|_| ObjectError::Limit("section offset overflow"))?;
    let end = start
        .checked_add(entry.stored_len as usize)
        .ok_or(ObjectError::Limit("section end overflow"))?;
    let bytes = slice.get(start..end).ok_or(ObjectError::Truncated)?;
    if XxHash3_64::oneshot(bytes) != entry.checksum {
        return Err(ObjectError::Checksum { what: "section" });
    }

    Ok(RawSection {
        kind,
        key: entry.key,
        rows: entry.rows,
        checksum: entry.checksum,
        bytes,
    })
}

pub fn list_range(bytes: &[u8]) -> Result<(u64, u64), ObjectError> {
    if bytes.len() < OBJECT_HEADER_LEN + TRAILER_LEN {
        return Err(ObjectError::Truncated);
    }
    let trailer_start = bytes.len() - TRAILER_LEN;
    let trailer =
        ObjectTrailer::read_from_bytes(bytes.get(trailer_start..).ok_or(ObjectError::Truncated)?)
            .map_err(|_| ObjectError::Truncated)?;
    if trailer.magic != TRAILER_MAGIC {
        return Err(ObjectError::BadMagic);
    }
    let list_len = trailer.list_len.get();
    let list_len_usize =
        usize::try_from(list_len).map_err(|_| ObjectError::Limit("part list length overflow"))?;
    let list_start = trailer_start
        .checked_sub(list_len_usize)
        .ok_or(ObjectError::Truncated)?;
    if list_start < OBJECT_HEADER_LEN {
        return Err(ObjectError::Truncated);
    }
    Ok((list_start as u64, list_len))
}

pub fn verify_part_list_org(
    list_bytes: &[u8],
    expected_org: u64,
    expected_byte_start: u64,
    expected_byte_len: u64,
) -> Result<PartMeta, ObjectError> {
    let parts = parse_part_list(list_bytes, u64::MAX)?;
    parts
        .into_iter()
        .find(|part| {
            part.org_id == expected_org
                && part.byte_start == expected_byte_start
                && part.byte_len == expected_byte_len
        })
        .ok_or(ObjectError::OrgMismatch {
            expected: expected_org,
            found: 0,
        })
}

#[expect(
    clippy::too_many_lines,
    reason = "validates part entries and section descriptors together"
)]
pub fn parse_part_list(list_bytes: &[u8], data_end: u64) -> Result<Vec<PartMeta>, ObjectError> {
    let header = PartListHeader::read_from_bytes(
        list_bytes
            .get(..PART_LIST_HEADER_LEN)
            .ok_or(ObjectError::Truncated)?,
    )
    .map_err(|_| ObjectError::Truncated)?;
    if header.magic != PART_LIST_MAGIC {
        return Err(ObjectError::BadMagic);
    }

    let part_count = header.parts.get() as usize;
    let section_count = header.sections.get() as usize;
    if part_count > MAX_PARTS {
        return Err(ObjectError::Limit("too many parts"));
    }

    if section_count > MAX_SECTIONS {
        return Err(ObjectError::Limit("too many sections"));
    }

    let parts_bytes_len = part_count
        .checked_mul(PART_ENTRY_LEN)
        .ok_or(ObjectError::Limit("part list size overflow"))?;
    let sections_bytes_len = section_count
        .checked_mul(SECTION_ENTRY_LEN)
        .ok_or(ObjectError::Limit("section list size overflow"))?;
    let expected_len = PART_LIST_HEADER_LEN
        .checked_add(parts_bytes_len)
        .and_then(|n| n.checked_add(sections_bytes_len))
        .ok_or(ObjectError::Limit("part list size overflow"))?;
    if list_bytes.len() != expected_len {
        return Err(ObjectError::Layout(
            "part list length disagrees with entry counts".into(),
        ));
    }

    let part_entries = <[PartEntry]>::ref_from_bytes(
        list_bytes
            .get(PART_LIST_HEADER_LEN..PART_LIST_HEADER_LEN + parts_bytes_len)
            .ok_or(ObjectError::Truncated)?,
    )
    .map_err(|_| ObjectError::Truncated)?;
    let section_entries = <[SectionEntry]>::ref_from_bytes(
        list_bytes
            .get(PART_LIST_HEADER_LEN + parts_bytes_len..)
            .ok_or(ObjectError::Truncated)?,
    )
    .map_err(|_| ObjectError::Truncated)?;

    let mut out = Vec::with_capacity(part_count);
    for part in part_entries {
        if part.reserved != [0; 6] {
            return Err(ObjectError::NonZeroReserved("part entry"));
        }

        let signal = Signal::from_number(part.signal).ok_or_else(|| ObjectError::UnknownValue {
            what: "signal",
            value: u64::from(part.signal),
        })?;
        let part_kind =
            PartKind::from_number(part.part).ok_or_else(|| ObjectError::UnknownValue {
                what: "part kind",
                value: u64::from(part.part),
            })?;
        let byte_start = part.byte_start.get();
        let byte_len = part.byte_len.get();
        let byte_end = byte_start
            .checked_add(byte_len)
            .ok_or(ObjectError::Limit("part byte range overflow"))?;
        if byte_start < OBJECT_HEADER_LEN as u64 || byte_end > data_end {
            return Err(ObjectError::Layout(
                "part byte range is out of bounds".into(),
            ));
        }

        let first = part.first_section.get() as usize;
        let count = part.section_count.get() as usize;
        let end_sec = first
            .checked_add(count)
            .ok_or(ObjectError::Limit("section range overflow"))?;
        let slice = section_entries
            .get(first..end_sec)
            .ok_or_else(|| ObjectError::Layout("part section indices are out of bounds".into()))?;

        let mut sections = Vec::with_capacity(count);
        for sec in slice {
            if sec.reserved != [0; 6] {
                return Err(ObjectError::NonZeroReserved("section entry"));
            }

            let sec_offset = sec.offset.get();
            let sec_len = u64::from(sec.stored_len.get());
            let sec_end = sec_offset
                .checked_add(sec_len)
                .ok_or(ObjectError::Limit("section byte range overflow"))?;
            if sec_offset < byte_start || sec_end > byte_end {
                return Err(ObjectError::Layout(
                    "section byte range lies outside its part".into(),
                ));
            }

            sections.push(SectionMeta {
                kind: sec.kind.get(),
                key: sec.key.get(),
                offset: sec_offset,
                stored_len: sec.stored_len.get(),
                rows: sec.rows.get(),
                checksum: sec.checksum.get(),
            });
        }

        out.push(PartMeta {
            org_id: part.org_id.get(),
            project_id: part.project_id.get(),
            starts_at: part.starts_at.get(),
            ends_at: part.ends_at.get(),
            byte_start,
            byte_len,
            signal,
            part: part_kind,
            sections,
        });
    }

    Ok(out)
}
