//! Self-describing, checksummed columnar object container (`USCOPEOB` v2) used
//! for local capture files, upload posts, WAL batches, and rolled-up objects.

pub mod codec;
pub mod filter;
pub mod tables;
#[cfg(test)]
mod tests;

use serde::{Deserialize, Serialize};
use twox_hash::XxHash3_64;
use zerocopy::little_endian::{I64, U16, U32, U64};
use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout, Unaligned};

pub use codec::{
    Codec, Coding, DecodeBudget, GroupEntry, PageEntry, PageReader, SectionHeader, SectionReader,
    SectionWriter,
};
pub use filter::{
    BinaryFuse8, filter_key_dimension, filter_key_frame_addr, filter_key_function_name,
    filter_key_process_name,
};
pub use tables::{
    ChunkData, ChunkRow, DimensionRow, FlameBundle, SanitizerCountRow, SharedStringIndex,
    SharedSymbols, StringDecodeMap, StringEncodeMap, TABLE_VERSION, decode_event_bundle,
    decode_event_bundle_budgeted, decode_flame_bundle, decode_flame_tables,
    decode_flame_tables_budgeted, decode_tables, decode_tables_budgeted, encode_event_bundle,
    encode_event_bundle_with_index, encode_tables, merge_chunks, round_capture_times,
    shift_capture_times, verify_chunk_decode_budget,
};

pub const OBJECT_MAGIC: [u8; 8] = *b"USCOPEOB";
pub const PART_LIST_MAGIC: [u8; 8] = *b"USCOPEPL";
pub const TRAILER_MAGIC: [u8; 8] = *b"USCOPEOE";
pub const CHECKPOINT_MAGIC: [u8; 8] = *b"USCOPEOC";
pub const OBJECT_VERSION: u32 = 2;

pub const OBJECT_HEADER_LEN: usize = 48;
pub const SECTION_HEADER_LEN: usize = 24;
pub const PAGE_ENTRY_LEN: usize = 28;
pub const GROUP_ENTRY_LEN: usize = 8;
pub const SECTION_CHECKSUM_LEN: usize = 8;
pub const PART_LIST_HEADER_LEN: usize = 16;
pub const PART_ENTRY_LEN: usize = 64;
pub const SECTION_ENTRY_LEN: usize = 40;
pub const TRAILER_LEN: usize = 40;

pub const MAX_OBJECT_BYTES: usize = 256 << 20;
pub const MAX_PARTS: usize = 16_384;
pub const MAX_SECTIONS: usize = 65_536;
pub const MAX_PAGES: usize = 4_096;
pub const MAX_GROUPS: usize = 4_096;
pub const MAX_COLUMNS: usize = 16_384;
pub const MAX_ROWS_PER_PAGE: usize = 262_144;
pub const MAX_TOTAL_ROWS: usize = 64_000_000;
pub const MAX_PAGE_RAW_BYTES: usize = 32 << 20;
pub const MAX_PART_LIST_RAW_BYTES: usize = 32 << 20;
pub const MAX_DECOMPRESSED_OBJECT_BYTES: usize = 256 << 20;

pub const DEFAULT_ZSTD_LEVEL: i32 = 3;
pub const DEFAULT_PAGE_ROWS: usize = 131_072;

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

    #[error("project mismatch: expected {expected}, found {found}")]
    ProjectMismatch { expected: u64, found: u64 },

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
    Capture = 5,
}

impl ObjectKind {
    #[must_use]
    pub const fn from_number(value: u8) -> Option<Self> {
        match value {
            1 => Some(Self::Post),
            2 => Some(Self::WriteAhead),
            3 => Some(Self::Sums),
            4 => Some(Self::Detail),
            5 => Some(Self::Capture),
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
    SanitizerSums = 70,
    MachineIndex = 80,
    ProcessTree = 81,
    ProcessDimensions = 82,
    AgentDirectory = 83,
    Filter = 90,
    WalBatch = 91,
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
            70 => Some(Self::SanitizerSums),
            80 => Some(Self::MachineIndex),
            81 => Some(Self::ProcessTree),
            82 => Some(Self::ProcessDimensions),
            83 => Some(Self::AgentDirectory),
            90 => Some(Self::Filter),
            91 => Some(Self::WalBatch),
            _ => None,
        }
    }

    #[must_use]
    pub const fn is_chunk_table(self) -> bool {
        (self as u16) <= 32
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
    pub flags: U16,
    pub created_at: I64,
    pub writer: U64,
    pub key_seed: U64,
    pub reserved: U64,
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
    pub list_off: U64,
    pub list_len: U64,
    pub list_uncompressed_len: U64,
    pub list_checksum: U64,
    pub magic: [u8; 8],
}

const _: () = assert!(size_of::<ObjectHeader>() == OBJECT_HEADER_LEN);
const _: () = assert!(size_of::<PartListHeader>() == PART_LIST_HEADER_LEN);
const _: () = assert!(size_of::<PartEntry>() == PART_ENTRY_LEN);
const _: () = assert!(size_of::<SectionEntry>() == SECTION_ENTRY_LEN);
const _: () = assert!(size_of::<ObjectTrailer>() == TRAILER_LEN);

#[must_use]
pub fn derive_key_seed(kind: ObjectKind, level: ObjectLevel, created_at: i64, writer: u64) -> u64 {
    let mut buf = [0_u8; 26];
    buf[0..8].copy_from_slice(&OBJECT_MAGIC);
    buf[8] = kind as u8;
    buf[9] = level as u8;
    buf[10..18].copy_from_slice(&created_at.to_le_bytes());
    buf[18..26].copy_from_slice(&writer.to_le_bytes());
    XxHash3_64::oneshot(&buf)
}

/// Computes a domain-separated `xxHash3` checksum over `section_body`.
///
/// Binds the hash to tenant IDs, `part_index`, `kind`, and `section_key` so
/// sections cannot be spliced across parts, tenants, or objects.
#[must_use]
pub fn section_checksum(
    key_seed: u64,
    org_id: u64,
    project_id: u64,
    part_index: u32,
    kind: u16,
    section_key: u64,
    section_body: &[u8],
) -> u64 {
    let mut ctx = [0_u8; 38];
    ctx[0..8].copy_from_slice(&key_seed.to_le_bytes());
    ctx[8..16].copy_from_slice(&org_id.to_le_bytes());
    ctx[16..24].copy_from_slice(&project_id.to_le_bytes());
    ctx[24..28].copy_from_slice(&part_index.to_le_bytes());
    ctx[28..30].copy_from_slice(&kind.to_le_bytes());
    ctx[30..38].copy_from_slice(&section_key.to_le_bytes());
    let derived_seed = XxHash3_64::oneshot(&ctx);
    XxHash3_64::oneshot_with_seed(derived_seed, section_body)
}

#[must_use]
pub fn part_list_checksum(header: &ObjectHeader, stored_list: &[u8]) -> u64 {
    let header_hash = XxHash3_64::oneshot(header.as_bytes());
    XxHash3_64::oneshot_with_seed(header_hash ^ 0x504c_5f43_484b_0002, stored_list)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EncodedSection {
    pub kind: SectionKind,
    pub rows: u32,
    pub key: u64,
    pub bytes: Vec<u8>,
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
    pub part_index: u32,
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

#[must_use]
pub fn encode_section_index(part_index: u32, sections: &[SectionMeta]) -> Vec<u8> {
    let mut out = Vec::with_capacity(8 + sections.len() * 24);
    out.push(1_u8);
    codec::write_varint(u64::from(part_index), &mut out);
    codec::write_varint(sections.len() as u64, &mut out);
    let mut prev_offset = 0_u64;
    for sec in sections {
        out.extend_from_slice(&sec.kind.to_le_bytes());
        codec::write_varint(sec.key, &mut out);
        let delta = sec.offset.wrapping_sub(prev_offset);
        codec::write_varint(codec::zigzag_encode(delta.cast_signed()), &mut out);
        prev_offset = sec.offset;
        codec::write_varint(u64::from(sec.stored_len), &mut out);
        codec::write_varint(u64::from(sec.rows), &mut out);
        out.extend_from_slice(&sec.checksum.to_le_bytes());
    }
    out
}

pub fn decode_section_index(bytes: &[u8]) -> Result<(u32, Vec<SectionMeta>), ObjectError> {
    let mut at = 0_usize;
    let version = *bytes.get(at).ok_or(ObjectError::Truncated)?;
    at += 1;
    if version != 1 {
        return Err(ObjectError::UnknownValue {
            what: "section_index version",
            value: u64::from(version),
        });
    }
    let part_index = u32::try_from(codec::read_varint(bytes, &mut at)?)
        .map_err(|_| ObjectError::Limit("part_index exceeds u32"))?;
    let count = usize::try_from(codec::read_varint(bytes, &mut at)?)
        .map_err(|_| ObjectError::Limit("section_index count overflow"))?;
    if count > MAX_SECTIONS {
        return Err(ObjectError::Limit("too many sections in section_index"));
    }

    let remaining = bytes.len().saturating_sub(at);
    let mut sections = Vec::with_capacity(count.min(remaining / 14));
    let mut prev_offset = 0_u64;
    for _ in 0..count {
        let kind_bytes: [u8; 2] = bytes
            .get(at..at + 2)
            .and_then(|s| s.try_into().ok())
            .ok_or(ObjectError::Truncated)?;
        at += 2;
        let kind = u16::from_le_bytes(kind_bytes);
        let key = codec::read_varint(bytes, &mut at)?;
        let delta_raw = codec::read_varint(bytes, &mut at)?;
        let offset = prev_offset.wrapping_add(codec::zigzag_decode(delta_raw).cast_unsigned());
        prev_offset = offset;
        let stored_len = u32::try_from(codec::read_varint(bytes, &mut at)?)
            .map_err(|_| ObjectError::Limit("stored_len exceeds u32"))?;
        let rows = u32::try_from(codec::read_varint(bytes, &mut at)?)
            .map_err(|_| ObjectError::Limit("rows exceeds u32"))?;
        let chk_bytes: [u8; 8] = bytes
            .get(at..at + 8)
            .and_then(|s| s.try_into().ok())
            .ok_or(ObjectError::Truncated)?;
        at += 8;
        let checksum = u64::from_le_bytes(chk_bytes);
        sections.push(SectionMeta {
            kind,
            key,
            offset,
            stored_len,
            rows,
            checksum,
        });
    }

    if at != bytes.len() {
        return Err(ObjectError::Layout(
            "trailing bytes in section_index".into(),
        ));
    }

    Ok((part_index, sections))
}

pub struct ObjectWriter {
    header: ObjectHeader,
    kind: ObjectKind,
    level: ObjectLevel,
    created_at: i64,
    writer: u64,
    key_seed: u64,
    flushed_len: u64,
    bytes: Vec<u8>,
    parts: Vec<PartEntry>,
    sections: Vec<SectionEntry>,
    part_metas: Vec<PartMeta>,
}

impl ObjectWriter {
    #[must_use]
    pub fn new(kind: ObjectKind, level: ObjectLevel, created_at: i64, writer: u64) -> Self {
        let key_seed = derive_key_seed(kind, level, created_at, writer);
        Self::with_key_seed(kind, level, created_at, writer, key_seed)
    }

    #[must_use]
    pub fn with_key_seed(
        kind: ObjectKind,
        level: ObjectLevel,
        created_at: i64,
        writer: u64,
        key_seed: u64,
    ) -> Self {
        let header = ObjectHeader {
            magic: OBJECT_MAGIC,
            version: OBJECT_VERSION.into(),
            kind: kind as u8,
            level: level as u8,
            flags: 0.into(),
            created_at: created_at.into(),
            writer: writer.into(),
            key_seed: key_seed.into(),
            reserved: 0.into(),
        };
        let mut bytes = Vec::with_capacity(4096);
        bytes.extend_from_slice(header.as_bytes());
        Self {
            header,
            kind,
            level,
            created_at,
            writer,
            key_seed,
            flushed_len: 0,
            bytes,
            parts: Vec::new(),
            sections: Vec::new(),
            part_metas: Vec::new(),
        }
    }

    #[must_use]
    pub const fn key_seed(&self) -> u64 {
        self.key_seed
    }

    fn current_offset(&self) -> Result<u64, ObjectError> {
        let pending = u64::try_from(self.bytes.len())
            .map_err(|_| ObjectError::Limit("object offset overflow"))?;
        self.flushed_len
            .checked_add(pending)
            .ok_or(ObjectError::Limit("object offset overflow"))
    }

    #[must_use]
    pub const fn part_count(&self) -> usize {
        self.parts.len()
    }

    #[must_use]
    pub const fn written_bytes(&self) -> u64 {
        self.flushed_len.saturating_add(self.bytes.len() as u64)
    }

    pub fn write_checkpoint(&mut self) -> Result<(), ObjectError> {
        let list_off = self.current_offset()?;
        let footer = self.build_footer_with_magic(list_off, CHECKPOINT_MAGIC)?;
        self.bytes.extend_from_slice(&footer);
        Ok(())
    }

    pub fn write_final_footer(&mut self) -> Result<(), ObjectError> {
        let list_off = self.current_offset()?;
        let footer = self.build_footer_with_magic(list_off, TRAILER_MAGIC)?;
        self.bytes.extend_from_slice(&footer);
        Ok(())
    }

    pub fn take_bytes(&mut self) -> Result<Vec<u8>, ObjectError> {
        let next = self.current_offset()?;
        self.flushed_len = next;
        Ok(std::mem::take(&mut self.bytes))
    }

    pub fn add_part(&mut self, spec: &PartSpec) -> Result<(), ObjectError> {
        if self.parts.len() >= MAX_PARTS {
            return Err(ObjectError::Limit("too many parts"));
        }
        if self
            .sections
            .len()
            .checked_add(spec.sections.len())
            .is_none_or(|n| n > MAX_SECTIONS)
        {
            return Err(ObjectError::Limit("too many sections"));
        }
        if spec.starts_at > spec.ends_at {
            return Err(ObjectError::Layout("part starts_at exceeds ends_at".into()));
        }

        let part_index =
            u32::try_from(self.parts.len()).map_err(|_| ObjectError::Limit("too many parts"))?;
        let first_section = u32::try_from(self.sections.len())
            .map_err(|_| ObjectError::Limit("too many sections"))?;
        let byte_start = self.current_offset()?;
        let mut section_metas = Vec::with_capacity(spec.sections.len());

        for section in &spec.sections {
            let offset = self.current_offset()?;
            let checksum = section_checksum(
                self.key_seed,
                spec.org_id,
                spec.project_id,
                part_index,
                section.kind as u16,
                section.key,
                &section.bytes,
            );
            let total_len = section
                .bytes
                .len()
                .checked_add(SECTION_CHECKSUM_LEN)
                .ok_or(ObjectError::Limit("section exceeds length"))?;
            let stored_len = u32::try_from(total_len)
                .map_err(|_| ObjectError::Limit("section exceeds u32 length"))?;
            self.bytes.extend_from_slice(&section.bytes);
            self.bytes.extend_from_slice(&checksum.to_le_bytes());
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

        let byte_end = self.current_offset()?;
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
            part_index,
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

    pub fn build_footer(&self, list_off: u64) -> Result<Vec<u8>, ObjectError> {
        self.build_footer_with_magic(list_off, TRAILER_MAGIC)
    }

    fn build_footer_with_magic(
        &self,
        list_off: u64,
        magic: [u8; 8],
    ) -> Result<Vec<u8>, ObjectError> {
        let list_header = PartListHeader {
            magic: PART_LIST_MAGIC,
            parts: u32::try_from(self.parts.len())
                .map_err(|_| ObjectError::Limit("too many parts"))?
                .into(),
            sections: u32::try_from(self.sections.len())
                .map_err(|_| ObjectError::Limit("too many sections"))?
                .into(),
        };
        let mut raw_list = Vec::with_capacity(
            PART_LIST_HEADER_LEN
                + self.parts.len() * PART_ENTRY_LEN
                + self.sections.len() * SECTION_ENTRY_LEN,
        );
        raw_list.extend_from_slice(list_header.as_bytes());
        raw_list.extend_from_slice(self.parts.as_bytes());
        raw_list.extend_from_slice(self.sections.as_bytes());

        let (codec, stored_list) = codec::compress_stream(&raw_list, DEFAULT_ZSTD_LEVEL)?;
        let list_uncompressed_len = match codec {
            Codec::Raw => 0_u64,
            Codec::Zstd => u64::try_from(raw_list.len())
                .map_err(|_| ObjectError::Limit("part list too large"))?,
        };
        let list_len = u64::try_from(stored_list.len())
            .map_err(|_| ObjectError::Limit("part list too large"))?;
        let list_checksum = part_list_checksum(&self.header, &stored_list);
        let trailer = ObjectTrailer {
            list_off: list_off.into(),
            list_len: list_len.into(),
            list_uncompressed_len: list_uncompressed_len.into(),
            list_checksum: list_checksum.into(),
            magic,
        };

        let mut footer = Vec::with_capacity(stored_list.len() + TRAILER_LEN);
        footer.extend_from_slice(&stored_list);
        footer.extend_from_slice(trailer.as_bytes());
        let final_len = list_off
            .checked_add(
                u64::try_from(footer.len()).map_err(|_| ObjectError::Limit("footer overflow"))?,
            )
            .ok_or(ObjectError::Limit("object exceeds maximum size"))?;
        if final_len > MAX_OBJECT_BYTES as u64 {
            return Err(ObjectError::Limit("object exceeds maximum size"));
        }
        Ok(footer)
    }

    pub fn finish(mut self) -> Result<(Vec<u8>, Vec<PartMeta>), ObjectError> {
        let list_off = self.current_offset()?;
        let footer = self.build_footer(list_off)?;
        self.bytes.extend_from_slice(&footer);

        if self.flushed_len == 0 {
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
            debug_assert_eq!(
                u64::from_le_bytes(self.bytes[32..40].try_into().unwrap_or_default()),
                self.key_seed
            );
        }

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
    pub key_seed: u64,
    parts: Vec<PartMeta>,
    pub truncated: bool,
}

impl<'a> ObjectReader<'a> {
    pub fn open(bytes: &'a [u8]) -> Result<Self, ObjectError> {
        Self::open_inner(bytes, false, true)
    }

    /// Opens an object and, if the final `TRAILER_MAGIC` is missing, scans
    /// backward for the last valid `CHECKPOINT_MAGIC` whose part list and
    /// section checksums all verify.
    pub fn open_recoverable(bytes: &'a [u8]) -> Result<Self, ObjectError> {
        Self::open_inner(bytes, true, true)
    }

    #[cfg(any(test, feature = "fuzzing"))]
    pub fn open_unverified(bytes: &'a [u8]) -> Result<Self, ObjectError> {
        Self::open_inner(bytes, true, false)
    }

    fn open_inner(
        bytes: &'a [u8],
        allow_recovery: bool,
        verify_checksum: bool,
    ) -> Result<Self, ObjectError> {
        if bytes.len() > MAX_OBJECT_BYTES {
            return Err(ObjectError::Limit("object exceeds maximum size"));
        }

        if bytes.len() < OBJECT_HEADER_LEN {
            return Err(ObjectError::Truncated);
        }

        let header = parse_object_header(bytes)?;
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
        let key_seed = header.key_seed.get();

        if bytes.len() < OBJECT_HEADER_LEN + PART_LIST_HEADER_LEN + TRAILER_LEN {
            return Err(ObjectError::Truncated);
        }

        let trailer_start = bytes.len() - TRAILER_LEN;
        let eof_trailer = ObjectTrailer::read_from_bytes(
            bytes.get(trailer_start..).ok_or(ObjectError::Truncated)?,
        )
        .map_err(|_| ObjectError::Truncated)?;

        let recovery_budget = DecodeBudget::new(MAX_PART_LIST_RAW_BYTES);
        let (parts, truncated) = if eof_trailer.magic == TRAILER_MAGIC {
            let parts = decode_trailer_parts(
                bytes,
                trailer_start,
                &eof_trailer,
                &header,
                kind,
                verify_checksum,
                &recovery_budget,
            )?;
            (parts, false)
        } else if allow_recovery {
            let parts =
                recover_last_checkpoint(bytes, &header, kind, verify_checksum, &recovery_budget)?;
            (parts, true)
        } else {
            return Err(ObjectError::BadMagic);
        };

        Ok(Self {
            bytes,
            kind,
            level,
            created_at: header.created_at.get(),
            writer: header.writer.get(),
            key_seed,
            parts,
            truncated,
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
        self.part_sections_checked(part_index, expected_org, None)
    }

    pub fn part_sections_checked(
        &self,
        part_index: usize,
        expected_org: Option<u64>,
        expected_project: Option<u64>,
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
        if let Some(expected) = expected_project
            && part.project_id != expected
        {
            return Err(ObjectError::ProjectMismatch {
                expected,
                found: part.project_id,
            });
        }

        let mut out = Vec::with_capacity(part.sections.len());
        for entry in &part.sections {
            out.push(verify_section_slice(
                self.bytes,
                0,
                entry,
                self.key_seed,
                part.org_id,
                part.project_id,
                part.part_index,
            )?);
        }

        Ok(out)
    }

    #[cfg(any(test, feature = "fuzzing"))]
    pub fn part_sections_unverified(
        &self,
        part_index: usize,
    ) -> Result<Vec<RawSection<'a>>, ObjectError> {
        let part = self
            .parts
            .get(part_index)
            .ok_or_else(|| ObjectError::Layout(format!("part {part_index} is out of bounds")))?;
        let mut out = Vec::with_capacity(part.sections.len());
        for entry in &part.sections {
            out.push(extract_section_slice_unverified(self.bytes, 0, entry)?);
        }
        Ok(out)
    }

    pub fn read_section(
        &self,
        part: &PartMeta,
        entry: &SectionMeta,
    ) -> Result<RawSection<'a>, ObjectError> {
        verify_section_slice(
            self.bytes,
            0,
            entry,
            self.key_seed,
            part.org_id,
            part.project_id,
            part.part_index,
        )
    }
}

const MAX_RECOVERY_ATTEMPTS: usize = 64;

fn decode_trailer_parts(
    bytes: &[u8],
    trailer_start: usize,
    trailer: &ObjectTrailer,
    header: &ObjectHeader,
    kind: ObjectKind,
    verify_checksum: bool,
    budget: &DecodeBudget,
) -> Result<Vec<PartMeta>, ObjectError> {
    let list_off = usize::try_from(trailer.list_off.get())
        .map_err(|_| ObjectError::Limit("part list offset overflow"))?;
    let list_len = usize::try_from(trailer.list_len.get())
        .map_err(|_| ObjectError::Limit("part list length overflow"))?;
    let list_end = list_off
        .checked_add(list_len)
        .ok_or(ObjectError::Limit("part list end overflow"))?;
    if list_off < OBJECT_HEADER_LEN || list_end != trailer_start {
        return Err(ObjectError::Truncated);
    }

    let stored_list = bytes
        .get(list_off..list_end)
        .ok_or(ObjectError::Truncated)?;
    let parts = parse_stored_part_list_inner(
        stored_list,
        trailer.list_uncompressed_len.get(),
        trailer.list_checksum.get(),
        header,
        list_off as u64,
        MAX_PARTS,
        verify_checksum,
        budget,
    )?;
    if kind == ObjectKind::Post && parts.is_empty() {
        return Err(ObjectError::Layout(
            "post object must have at least 1 part".into(),
        ));
    }
    Ok(parts)
}

fn recover_last_checkpoint(
    bytes: &[u8],
    header: &ObjectHeader,
    kind: ObjectKind,
    verify_checksum: bool,
    budget: &DecodeBudget,
) -> Result<Vec<PartMeta>, ObjectError> {
    let key_seed = header.key_seed.get();
    let mut search_end = bytes.len();
    let mut attempts = 0_usize;
    while search_end >= OBJECT_HEADER_LEN + TRAILER_LEN && attempts < MAX_RECOVERY_ATTEMPTS {
        let Some(pos) = memchr::memmem::rfind(&bytes[..search_end], &CHECKPOINT_MAGIC) else {
            break;
        };
        attempts += 1;
        if let Some(trailer_start) = pos.checked_sub(TRAILER_LEN - TRAILER_MAGIC.len())
            && trailer_start >= OBJECT_HEADER_LEN
            && let Some(trailer_slice) = bytes.get(trailer_start..trailer_start + TRAILER_LEN)
            && let Ok(trailer) = ObjectTrailer::read_from_bytes(trailer_slice)
            && let Ok(parts) = decode_trailer_parts(
                bytes,
                trailer_start,
                &trailer,
                header,
                kind,
                verify_checksum,
                budget,
            )
        {
            let mut all_ok = true;
            for part in &parts {
                for sec in &part.sections {
                    let ok = if verify_checksum {
                        verify_section_slice(
                            bytes,
                            0,
                            sec,
                            key_seed,
                            part.org_id,
                            part.project_id,
                            part.part_index,
                        )
                        .is_ok()
                    } else {
                        extract_section_slice_unverified(bytes, 0, sec).is_ok()
                    };
                    if !ok {
                        all_ok = false;
                        break;
                    }
                }
                if !all_ok {
                    break;
                }
            }
            if all_ok {
                return Ok(parts);
            }
        }
        search_end = pos;
    }

    Err(ObjectError::Truncated)
}

fn extract_section_slice_unverified<'a>(
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
    let full = slice.get(start..end).ok_or(ObjectError::Truncated)?;
    if full.len() < SECTION_HEADER_LEN + SECTION_CHECKSUM_LEN {
        return Err(ObjectError::Truncated);
    }

    let body_len = full.len() - SECTION_CHECKSUM_LEN;
    let body = full.get(..body_len).ok_or(ObjectError::Truncated)?;
    Ok(RawSection {
        kind,
        key: entry.key,
        rows: entry.rows,
        checksum: entry.checksum,
        bytes: body,
    })
}

pub fn verify_section_slice<'a>(
    slice: &'a [u8],
    slice_base_offset: u64,
    entry: &SectionMeta,
    key_seed: u64,
    org_id: u64,
    project_id: u64,
    part_index: u32,
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
    let full = slice.get(start..end).ok_or(ObjectError::Truncated)?;
    if full.len() < SECTION_HEADER_LEN + SECTION_CHECKSUM_LEN {
        return Err(ObjectError::Truncated);
    }

    let body_len = full.len() - SECTION_CHECKSUM_LEN;
    let body = full.get(..body_len).ok_or(ObjectError::Truncated)?;
    let trailer_bytes: [u8; 8] = full
        .get(body_len..)
        .and_then(|s| s.try_into().ok())
        .ok_or(ObjectError::Truncated)?;
    let trailer_checksum = u64::from_le_bytes(trailer_bytes);

    let computed = section_checksum(
        key_seed, org_id, project_id, part_index, entry.kind, entry.key, body,
    );
    if computed != trailer_checksum || computed != entry.checksum {
        return Err(ObjectError::Checksum { what: "section" });
    }

    Ok(RawSection {
        kind,
        key: entry.key,
        rows: entry.rows,
        checksum: computed,
        bytes: body,
    })
}

pub fn parse_object_header(bytes: &[u8]) -> Result<ObjectHeader, ObjectError> {
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
    if header.flags.get() != 0 || header.reserved.get() != 0 {
        return Err(ObjectError::NonZeroReserved("object header"));
    }
    if ObjectKind::from_number(header.kind).is_none() {
        return Err(ObjectError::UnknownValue {
            what: "object kind",
            value: u64::from(header.kind),
        });
    }
    if ObjectLevel::from_number(header.level).is_none() {
        return Err(ObjectError::UnknownValue {
            what: "object level",
            value: u64::from(header.level),
        });
    }
    Ok(header)
}

pub fn parse_trailer(bytes: &[u8]) -> Result<ObjectTrailer, ObjectError> {
    if bytes.len() < TRAILER_LEN {
        return Err(ObjectError::Truncated);
    }
    let trailer_start = bytes.len() - TRAILER_LEN;
    let trailer =
        ObjectTrailer::read_from_bytes(bytes.get(trailer_start..).ok_or(ObjectError::Truncated)?)
            .map_err(|_| ObjectError::Truncated)?;
    if trailer.magic != TRAILER_MAGIC {
        return Err(ObjectError::BadMagic);
    }
    Ok(trailer)
}

pub fn parse_stored_part_list(
    stored_list: &[u8],
    uncompressed_len: u64,
    expected_checksum: u64,
    header: &ObjectHeader,
    data_end: u64,
    max_parts: usize,
) -> Result<Vec<PartMeta>, ObjectError> {
    let budget = DecodeBudget::new(MAX_PART_LIST_RAW_BYTES);
    parse_stored_part_list_inner(
        stored_list,
        uncompressed_len,
        expected_checksum,
        header,
        data_end,
        max_parts,
        true,
        &budget,
    )
}

#[expect(
    clippy::too_many_arguments,
    reason = "internal helper sharing validation and recovery decode budget across part-list readers"
)]
fn parse_stored_part_list_inner(
    stored_list: &[u8],
    uncompressed_len: u64,
    expected_checksum: u64,
    header: &ObjectHeader,
    data_end: u64,
    max_parts: usize,
    verify_checksum: bool,
    budget: &DecodeBudget,
) -> Result<Vec<PartMeta>, ObjectError> {
    if verify_checksum && part_list_checksum(header, stored_list) != expected_checksum {
        return Err(ObjectError::Checksum { what: "part list" });
    }

    let raw = if uncompressed_len == 0 {
        if stored_list.len() > MAX_PART_LIST_RAW_BYTES {
            return Err(ObjectError::Limit("part list exceeds maximum size"));
        }
        budget.take(stored_list.len())?;
        std::borrow::Cow::Borrowed(stored_list)
    } else {
        let expected = usize::try_from(uncompressed_len)
            .map_err(|_| ObjectError::Limit("part list uncompressed length overflow"))?;
        if expected > MAX_PART_LIST_RAW_BYTES {
            return Err(ObjectError::Limit("part list exceeds maximum size"));
        }
        let decompressed =
            codec::decompress_exact_budgeted(Codec::Zstd, stored_list, expected, budget)?;
        std::borrow::Cow::Owned(decompressed)
    };

    parse_part_list_bounded(&raw, data_end, max_parts)
}

#[expect(
    clippy::too_many_lines,
    reason = "validates part entries and section descriptors together"
)]
fn parse_part_list_bounded(
    list_bytes: &[u8],
    data_end: u64,
    max_parts: usize,
) -> Result<Vec<PartMeta>, ObjectError> {
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
    if part_count > max_parts {
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
    let mut prev_end_sec = 0_usize;
    let mut prev_part_end = OBJECT_HEADER_LEN as u64;
    for (idx, part) in part_entries.iter().enumerate() {
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
        let starts_at = part.starts_at.get();
        let ends_at = part.ends_at.get();
        if starts_at > ends_at {
            return Err(ObjectError::Layout("part starts_at exceeds ends_at".into()));
        }

        let byte_start = part.byte_start.get();
        let byte_len = part.byte_len.get();
        let byte_end = byte_start
            .checked_add(byte_len)
            .ok_or(ObjectError::Limit("part byte range overflow"))?;
        if byte_start < prev_part_end || byte_end > data_end {
            return Err(ObjectError::Layout(
                "part byte range is out of bounds or overlaps previous part".into(),
            ));
        }
        prev_part_end = byte_end;

        let first = part.first_section.get() as usize;
        let count = part.section_count.get() as usize;
        if first < prev_end_sec {
            return Err(ObjectError::Layout(
                "part section ranges overlap or are out of order".into(),
            ));
        }
        let end_sec = first
            .checked_add(count)
            .ok_or(ObjectError::Limit("section range overflow"))?;
        let slice = section_entries
            .get(first..end_sec)
            .ok_or_else(|| ObjectError::Layout("part section indices are out of bounds".into()))?;
        prev_end_sec = end_sec;

        let mut sections = Vec::with_capacity(count);
        let mut prev_sec_end = byte_start;
        for sec in slice {
            if sec.reserved != [0; 6] {
                return Err(ObjectError::NonZeroReserved("section entry"));
            }

            let sec_offset = sec.offset.get();
            let sec_len = u64::from(sec.stored_len.get());
            let sec_end = sec_offset
                .checked_add(sec_len)
                .ok_or(ObjectError::Limit("section byte range overflow"))?;
            if sec_offset < prev_sec_end || sec_end > byte_end {
                return Err(ObjectError::Layout(
                    "section byte range lies outside its part or overlaps previous section".into(),
                ));
            }
            prev_sec_end = sec_end;

            sections.push(SectionMeta {
                kind: sec.kind.get(),
                key: sec.key.get(),
                offset: sec_offset,
                stored_len: sec.stored_len.get(),
                rows: sec.rows.get(),
                checksum: sec.checksum.get(),
            });
        }

        let part_index =
            u32::try_from(idx).map_err(|_| ObjectError::Limit("part index overflow"))?;
        out.push(PartMeta {
            part_index,
            org_id: part.org_id.get(),
            project_id: part.project_id.get(),
            starts_at,
            ends_at,
            byte_start,
            byte_len,
            signal,
            part: part_kind,
            sections,
        });
    }

    Ok(out)
}
