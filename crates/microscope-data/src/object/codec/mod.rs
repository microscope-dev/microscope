//! Paged, multi-frame columnar section encoder and decoder (`SectionWriter`,
//! `SectionReader`) with per-page key-range pruning, `zstd` compression, and
//! `DecodeBudget` memory accounting.

use std::borrow::Cow;
use std::cell::RefCell;
use std::ops::Range;

use foldhash::HashMap;

use zerocopy::little_endian::{I64, U16, U32};
use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout, Unaligned};

use super::{
    EncodedSection, GROUP_ENTRY_LEN, MAX_COLUMNS, MAX_GROUPS, MAX_PAGE_RAW_BYTES, MAX_PAGES,
    MAX_TOTAL_ROWS, ObjectError, PAGE_ENTRY_LEN, SECTION_HEADER_LEN, SectionKind, TimeUnit,
};

pub const WHOLE_SECTION_GROUP: u16 = u16::MAX;
pub const NO_KEY_COLUMN: u8 = u8::MAX;
pub const SECTION_FLAG_PAGED: u8 = 1;
pub(super) const DENSE_KEY_LIMIT: usize = 1 << 20;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u8)]
pub enum Coding {
    Constant = 0,
    Bytes = 1,
    Words = 2,
    VarintPlain = 3,
    VarintDelta = 4,
    VarintDeltaByKey = 5,
    VarintRelative = 6,
    DictVarint = 7,
    Binned2b = 8,
    BpTree = 9,
    SparseHistogram40 = 10,
    NonZeroBitmap = 11,
}

impl Coding {
    #[must_use]
    pub const fn from_number(value: u8) -> Option<Self> {
        match value {
            0 => Some(Self::Constant),
            1 => Some(Self::Bytes),
            2 => Some(Self::Words),
            3 => Some(Self::VarintPlain),
            4 => Some(Self::VarintDelta),
            5 => Some(Self::VarintDeltaByKey),
            6 => Some(Self::VarintRelative),
            7 => Some(Self::DictVarint),
            8 => Some(Self::Binned2b),
            9 => Some(Self::BpTree),
            10 => Some(Self::SparseHistogram40),
            11 => Some(Self::NonZeroBitmap),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u8)]
pub enum Codec {
    Raw = 0,
    Zstd = 1,
}

impl Codec {
    #[must_use]
    pub const fn from_number(value: u8) -> Option<Self> {
        match value {
            0 => Some(Self::Raw),
            1 => Some(Self::Zstd),
            _ => None,
        }
    }
}

#[repr(C)]
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, FromBytes, IntoBytes, KnownLayout, Immutable, Unaligned,
)]
pub struct SectionHeader {
    pub kind: U16,
    pub version: u8,
    pub time_unit: u8,
    pub flags: u8,
    pub reserved: u8,
    pub page_count: U16,
    pub rows: U32,
    pub uncompressed_len: U32,
    pub base_time: I64,
}

#[repr(C)]
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, FromBytes, IntoBytes, KnownLayout, Immutable, Unaligned,
)]
pub struct PageEntry {
    pub min_key: I64,
    pub max_key: I64,
    pub stored_len: U32,
    pub uncompressed_len: U32,
    pub rows: U32,
}

#[repr(C)]
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, FromBytes, IntoBytes, KnownLayout, Immutable, Unaligned,
)]
pub struct GroupEntry {
    pub abi: u8,
    pub argument: u8,
    pub number: U16,
    pub rows: U32,
}

const _: () = assert!(size_of::<SectionHeader>() == SECTION_HEADER_LEN);
const _: () = assert!(size_of::<PageEntry>() == PAGE_ENTRY_LEN);
const _: () = assert!(size_of::<GroupEntry>() == GROUP_ENTRY_LEN);

mod encodings;
pub use encodings::*;

pub fn compress_stream(raw: &[u8], zstd_level: i32) -> Result<(Codec, Vec<u8>), ObjectError> {
    if raw.len() < 32 {
        return Ok((Codec::Raw, raw.to_vec()));
    }

    let compressed = ZSTD_CCTX.with(|cell| {
        let mut borrow = cell.borrow_mut();
        let needs_new = borrow.as_ref().is_none_or(|(lvl, _)| *lvl != zstd_level);
        if needs_new {
            let c = zstd::bulk::Compressor::new(zstd_level)
                .map_err(|error| ObjectError::Compression(error.to_string()))?;
            *borrow = Some((zstd_level, c));
        }
        let Some((_, compressor)) = borrow.as_mut() else {
            return Err(ObjectError::Compression("missing zstd compressor".into()));
        };
        compressor
            .compress(raw)
            .map_err(|error| ObjectError::Compression(error.to_string()))
    })?;
    if compressed.len() < raw.len() {
        Ok((Codec::Zstd, compressed))
    } else {
        Ok((Codec::Raw, raw.to_vec()))
    }
}

/// Tracks live decompressed and materialized bytes across an object decode
/// (`release` frees page temporaries between pages) to bound peak memory
/// against crafted or zip-bomb inputs.
#[derive(Debug)]
pub struct DecodeBudget {
    limit: usize,
    remaining: std::cell::Cell<usize>,
}

impl DecodeBudget {
    #[must_use]
    pub const fn new(limit: usize) -> Self {
        Self {
            limit,
            remaining: std::cell::Cell::new(limit),
        }
    }

    #[must_use]
    pub const fn remaining(&self) -> usize {
        self.remaining.get()
    }

    pub fn take(&self, bytes: usize) -> Result<(), ObjectError> {
        let next = self
            .remaining
            .get()
            .checked_sub(bytes)
            .ok_or(ObjectError::Limit("object exceeds maximum decoded size"))?;
        self.remaining.set(next);
        Ok(())
    }

    pub fn release(&self, bytes: usize) {
        let next = self.remaining.get().saturating_add(bytes).min(self.limit);
        self.remaining.set(next);
    }
}

thread_local! {
    static ZSTD_DCTX: RefCell<Option<zstd::bulk::Decompressor<'static>>> = const { RefCell::new(None) };
    static ZSTD_CCTX: RefCell<Option<(i32, zstd::bulk::Compressor<'static>)>> = const { RefCell::new(None) };
}

fn decompress_cow_budgeted<'a>(
    codec: Codec,
    stored: &'a [u8],
    expected_len: usize,
    budget: &DecodeBudget,
) -> Result<Cow<'a, [u8]>, ObjectError> {
    if expected_len > MAX_PAGE_RAW_BYTES {
        return Err(ObjectError::Limit(
            "decompressed frame exceeds maximum size",
        ));
    }
    budget.take(expected_len)?;

    match codec {
        Codec::Raw => {
            if stored.len() != expected_len {
                return Err(ObjectError::Layout(
                    "raw frame length disagrees with uncompressed_len".into(),
                ));
            }
            Ok(Cow::Borrowed(stored))
        }
        Codec::Zstd => {
            let out = ZSTD_DCTX.with(|cell| {
                let mut borrow = cell.borrow_mut();
                let decompressor = if let Some(d) = borrow.as_mut() {
                    d
                } else {
                    let d = zstd::bulk::Decompressor::new()
                        .map_err(|e| ObjectError::Compression(e.to_string()))?;
                    borrow.insert(d)
                };
                decompressor
                    .decompress(stored, expected_len)
                    .map_err(|e| ObjectError::Compression(e.to_string()))
            })?;
            if out.len() != expected_len {
                return Err(ObjectError::Layout(
                    "zstd frame length disagrees with uncompressed_len".into(),
                ));
            }
            Ok(Cow::Owned(out))
        }
    }
}

pub fn decompress_exact_budgeted(
    codec: Codec,
    stored: &[u8],
    expected_len: usize,
    budget: &DecodeBudget,
) -> Result<Vec<u8>, ObjectError> {
    decompress_cow_budgeted(codec, stored, expected_len, budget).map(Cow::into_owned)
}

struct PendingColumn {
    group: u16,
    col_id: u8,
    coding: Coding,
    key_col: u8,
    constant_val: u64,
    payload: Vec<u8>,
    bypass: Vec<u8>,
}

struct PendingFrame {
    groups: Vec<GroupEntry>,
    columns: Vec<PendingColumn>,
}

struct FinishedPage {
    min_key: i64,
    max_key: i64,
    rows: u32,
    uncompressed_len: u32,
    bytes: Vec<u8>,
}

pub struct SectionWriter {
    kind: SectionKind,
    version: u8,
    time_unit: TimeUnit,
    rows: u32,
    base_time: i64,
    zstd_level: i32,
    current_frames: Vec<PendingFrame>,
    pages: Vec<FinishedPage>,
}

impl SectionWriter {
    #[must_use]
    pub fn new(
        kind: SectionKind,
        version: u8,
        time_unit: TimeUnit,
        rows: u32,
        base_time: i64,
        zstd_level: i32,
    ) -> Self {
        Self {
            kind,
            version,
            time_unit,
            rows,
            base_time,
            zstd_level,
            current_frames: vec![PendingFrame {
                groups: Vec::new(),
                columns: Vec::new(),
            }],
            pages: Vec::new(),
        }
    }

    /// Starts a new independently compressed column frame within the current
    /// page so readers using `SectionReader::read_page_frames` can decode core
    /// columns while skipping heavier columns (e.g. syscall arguments).
    pub fn next_frame(&mut self) -> Result<(), ObjectError> {
        if self.current_frames.len() >= 16 {
            return Err(ObjectError::Limit("too many column frames in page"));
        }
        self.current_frames.push(PendingFrame {
            groups: Vec::new(),
            columns: Vec::new(),
        });
        Ok(())
    }

    pub fn add_group(
        &mut self,
        abi: u8,
        argument: u8,
        number: u16,
        rows: u32,
    ) -> Result<u16, ObjectError> {
        let frame = self
            .current_frames
            .last_mut()
            .ok_or_else(|| ObjectError::Layout("missing frame".into()))?;
        let index =
            u16::try_from(frame.groups.len()).map_err(|_| ObjectError::Limit("too many groups"))?;
        if frame.groups.len() >= MAX_GROUPS {
            return Err(ObjectError::Limit("too many groups"));
        }

        frame.groups.push(GroupEntry {
            abi,
            argument,
            number: number.into(),
            rows: rows.into(),
        });
        Ok(index)
    }

    pub fn add_constant(&mut self, group: u16, column: u8, value: u64) -> Result<(), ObjectError> {
        let frame = self
            .current_frames
            .last_mut()
            .ok_or_else(|| ObjectError::Layout("missing frame".into()))?;
        if frame.columns.len() >= MAX_COLUMNS {
            return Err(ObjectError::Limit("too many columns"));
        }
        frame.columns.push(PendingColumn {
            group,
            col_id: column,
            coding: Coding::Constant,
            key_col: NO_KEY_COLUMN,
            constant_val: value,
            payload: Vec::new(),
            bypass: Vec::new(),
        });
        Ok(())
    }

    pub fn add_column(
        &mut self,
        group: u16,
        column: u8,
        coding: Coding,
        key: u8,
        raw: &[u8],
    ) -> Result<(), ObjectError> {
        let frame = self
            .current_frames
            .last_mut()
            .ok_or_else(|| ObjectError::Layout("missing frame".into()))?;
        if frame.columns.len() >= MAX_COLUMNS {
            return Err(ObjectError::Limit("too many columns"));
        }
        if coding == Coding::Constant {
            let bytes: [u8; 8] = raw.try_into().map_err(|_| {
                ObjectError::Layout("constant column requires an 8-byte payload".into())
            })?;
            return self.add_constant(group, column, u64::from_le_bytes(bytes));
        }
        frame.columns.push(PendingColumn {
            group,
            col_id: column,
            coding,
            key_col: key,
            constant_val: 0,
            payload: raw.to_vec(),
            bypass: Vec::new(),
        });
        Ok(())
    }

    pub fn add_column_owned(
        &mut self,
        group: u16,
        column: u8,
        coding: Coding,
        key: u8,
        payload: Vec<u8>,
    ) -> Result<(), ObjectError> {
        let frame = self
            .current_frames
            .last_mut()
            .ok_or_else(|| ObjectError::Layout("missing frame".into()))?;
        if frame.columns.len() >= MAX_COLUMNS {
            return Err(ObjectError::Limit("too many columns"));
        }
        if coding == Coding::Constant {
            let bytes: [u8; 8] = payload.as_slice().try_into().map_err(|_| {
                ObjectError::Layout("constant column requires an 8-byte payload".into())
            })?;
            return self.add_constant(group, column, u64::from_le_bytes(bytes));
        }
        frame.columns.push(PendingColumn {
            group,
            col_id: column,
            coding,
            key_col: key,
            constant_val: 0,
            payload,
            bypass: Vec::new(),
        });
        Ok(())
    }

    pub fn add_binned_2b(
        &mut self,
        group: u16,
        column: u8,
        values: &[u64],
    ) -> Result<(), ObjectError> {
        let frame = self
            .current_frames
            .last_mut()
            .ok_or_else(|| ObjectError::Layout("missing frame".into()))?;
        if frame.columns.len() >= MAX_COLUMNS {
            return Err(ObjectError::Limit("too many columns"));
        }
        let (classes, mantissa) = encode_binned_2b(values);
        frame.columns.push(PendingColumn {
            group,
            col_id: column,
            coding: Coding::Binned2b,
            key_col: NO_KEY_COLUMN,
            constant_val: 0,
            payload: classes,
            bypass: mantissa,
        });
        Ok(())
    }

    pub fn add_u64_smart(
        &mut self,
        group: u16,
        column: u8,
        values: &[u64],
        delta: bool,
    ) -> Result<(), ObjectError> {
        if let Some(&first) = values.first()
            && values.iter().all(|&v| v == first)
        {
            return self.add_constant(group, column, first);
        }
        if delta {
            let encoded = encode_varints_delta(values);
            self.add_column_owned(group, column, Coding::VarintDelta, NO_KEY_COLUMN, encoded)
        } else {
            let encoded = encode_varints(values);
            self.add_column_owned(group, column, Coding::VarintPlain, NO_KEY_COLUMN, encoded)
        }
    }

    pub fn add_best_u64(
        &mut self,
        group: u16,
        column: u8,
        values: &[u64],
    ) -> Result<(), ObjectError> {
        let (coding, bytes) = pick_best_u64_coding(values);
        self.add_column_owned(group, column, coding, NO_KEY_COLUMN, bytes)
    }

    pub fn finish_page(
        &mut self,
        min_key: i64,
        max_key: i64,
        page_rows: u32,
    ) -> Result<(), ObjectError> {
        if self.pages.len() >= MAX_PAGES {
            return Err(ObjectError::Limit("too many pages in section"));
        }
        if min_key > max_key {
            return Err(ObjectError::Layout("page min_key exceeds max_key".into()));
        }

        let frames = std::mem::replace(
            &mut self.current_frames,
            vec![PendingFrame {
                groups: Vec::new(),
                columns: Vec::new(),
            }],
        );
        let (page_bytes, uncompressed_len) = encode_page_frames(&frames, self.zstd_level)?;
        self.pages.push(FinishedPage {
            min_key,
            max_key,
            rows: page_rows,
            uncompressed_len,
            bytes: page_bytes,
        });
        Ok(())
    }

    pub fn finish(self, key: u64) -> Result<EncodedSection, ObjectError> {
        if self.pages.is_empty() {
            let (page_bytes, uncompressed_len) =
                encode_page_frames(&self.current_frames, self.zstd_level)?;
            let header = SectionHeader {
                kind: (self.kind as u16).into(),
                version: self.version,
                time_unit: self.time_unit as u8,
                flags: 0,
                reserved: 0,
                page_count: 1_u16.into(),
                rows: self.rows.into(),
                uncompressed_len: uncompressed_len.into(),
                base_time: self.base_time.into(),
            };
            let mut bytes = Vec::with_capacity(SECTION_HEADER_LEN + page_bytes.len());
            bytes.extend_from_slice(header.as_bytes());
            bytes.extend_from_slice(&page_bytes);
            return Ok(EncodedSection {
                kind: self.kind,
                rows: self.rows,
                key,
                bytes,
            });
        }

        let has_trailing = self
            .current_frames
            .iter()
            .any(|f| !f.columns.is_empty() || !f.groups.is_empty());
        if has_trailing {
            return Err(ObjectError::Layout(
                "unflushed columns remaining after finish_page".into(),
            ));
        }

        self.finish_paged(key)
    }

    pub fn finish_paged(self, key: u64) -> Result<EncodedSection, ObjectError> {
        if self.pages.is_empty() {
            return Err(ObjectError::Layout("paged section has zero pages".into()));
        }
        let total_rows: u32 = self
            .pages
            .iter()
            .try_fold(0_u32, |acc, p| acc.checked_add(p.rows))
            .ok_or(ObjectError::Limit("section row count exceeds u32"))?;
        let page_count =
            u16::try_from(self.pages.len()).map_err(|_| ObjectError::Limit("too many pages"))?;

        if page_count == 1 && self.pages[0].min_key == i64::MIN && self.pages[0].max_key == i64::MAX
        {
            let page = &self.pages[0];
            let header = SectionHeader {
                kind: (self.kind as u16).into(),
                version: self.version,
                time_unit: self.time_unit as u8,
                flags: 0,
                reserved: 0,
                page_count: 1_u16.into(),
                rows: total_rows.into(),
                uncompressed_len: page.uncompressed_len.into(),
                base_time: self.base_time.into(),
            };
            let mut bytes = Vec::with_capacity(SECTION_HEADER_LEN + page.bytes.len());
            bytes.extend_from_slice(header.as_bytes());
            bytes.extend_from_slice(&page.bytes);
            return Ok(EncodedSection {
                kind: self.kind,
                rows: total_rows,
                key,
                bytes,
            });
        }

        let header = SectionHeader {
            kind: (self.kind as u16).into(),
            version: self.version,
            time_unit: self.time_unit as u8,
            flags: SECTION_FLAG_PAGED,
            reserved: 0,
            page_count: page_count.into(),
            rows: total_rows.into(),
            uncompressed_len: 0_u32.into(),
            base_time: self.base_time.into(),
        };

        let payload_len: usize = self.pages.iter().map(|p| p.bytes.len()).sum();
        let mut bytes = Vec::with_capacity(
            SECTION_HEADER_LEN + self.pages.len() * PAGE_ENTRY_LEN + payload_len,
        );
        bytes.extend_from_slice(header.as_bytes());
        for page in &self.pages {
            let stored_len = u32::try_from(page.bytes.len())
                .map_err(|_| ObjectError::Limit("page exceeds u32 length"))?;
            let entry = PageEntry {
                min_key: page.min_key.into(),
                max_key: page.max_key.into(),
                stored_len: stored_len.into(),
                uncompressed_len: page.uncompressed_len.into(),
                rows: page.rows.into(),
            };
            bytes.extend_from_slice(entry.as_bytes());
        }
        for page in self.pages {
            bytes.extend_from_slice(&page.bytes);
        }

        Ok(EncodedSection {
            kind: self.kind,
            rows: total_rows,
            key,
            bytes,
        })
    }
}

fn encode_page_frames(
    frames: &[PendingFrame],
    zstd_level: i32,
) -> Result<(Vec<u8>, u32), ObjectError> {
    let mut non_empty: Vec<&PendingFrame> = frames
        .iter()
        .filter(|f| !f.columns.is_empty() || !f.groups.is_empty())
        .collect();
    if non_empty.is_empty()
        && let Some(first) = frames.first()
    {
        non_empty.push(first);
    }

    let frame_count =
        u8::try_from(non_empty.len()).map_err(|_| ObjectError::Limit("too many column frames"))?;
    let mut out = Vec::with_capacity(256);
    out.push(frame_count);
    let mut total_uncompressed = 0_usize;

    for frame in non_empty {
        let mut raw_buf = Vec::new();
        write_varint(frame.groups.len() as u64, &mut raw_buf);
        raw_buf.extend_from_slice(frame.groups.as_bytes());
        write_varint(frame.columns.len() as u64, &mut raw_buf);

        // `bypass_buf` holds high-entropy `Binned2b` mantissa bits verbatim
        // outside zstd, avoiding wasted compression work on incompressible bits.
        let mut bypass_buf = Vec::new();
        for col in &frame.columns {
            let enc_group = if col.group == WHOLE_SECTION_GROUP {
                0_u64
            } else {
                u64::from(col.group) + 1
            };
            write_varint(enc_group, &mut raw_buf);
            raw_buf.push(col.col_id);
            raw_buf.push(col.coding as u8);
            raw_buf.push(col.key_col);
            if col.coding == Coding::Constant {
                write_varint(col.constant_val, &mut raw_buf);
            } else {
                write_varint(col.payload.len() as u64, &mut raw_buf);
                if col.coding == Coding::Binned2b {
                    write_varint(col.bypass.len() as u64, &mut raw_buf);
                }
            }
        }
        for col in &frame.columns {
            if col.coding != Coding::Constant {
                raw_buf.extend_from_slice(&col.payload);
                if col.coding == Coding::Binned2b {
                    bypass_buf.extend_from_slice(&col.bypass);
                }
            }
        }

        if raw_buf.len() > MAX_PAGE_RAW_BYTES || bypass_buf.len() > MAX_PAGE_RAW_BYTES {
            return Err(ObjectError::Limit(
                "encoded frame exceeds maximum page size",
            ));
        }
        let (codec, stored) = compress_stream(&raw_buf, zstd_level)?;
        total_uncompressed = total_uncompressed
            .checked_add(raw_buf.len())
            .and_then(|n| n.checked_add(bypass_buf.len()))
            .ok_or(ObjectError::Limit("page uncompressed size overflow"))?;

        out.push(codec as u8);
        write_varint(stored.len() as u64, &mut out);
        write_varint(raw_buf.len() as u64, &mut out);
        write_varint(bypass_buf.len() as u64, &mut out);
        out.extend_from_slice(&stored);
        out.extend_from_slice(&bypass_buf);
    }

    let total_uncompressed_u32 = u32::try_from(total_uncompressed)
        .map_err(|_| ObjectError::Limit("page uncompressed size exceeds u32"))?;
    Ok((out, total_uncompressed_u32))
}

#[derive(Debug, Clone, Copy)]
pub struct DecodedColumn<'a> {
    pub coding: Coding,
    pub key_col: u8,
    pub constant_val: u64,
    pub data: &'a [u8],
    pub bypass: &'a [u8],
}

impl DecodedColumn<'_> {
    pub fn decode_u64(
        &self,
        count: usize,
        budget: Option<&DecodeBudget>,
    ) -> Result<Vec<u64>, ObjectError> {
        if count > MAX_TOTAL_ROWS {
            return Err(ObjectError::Limit("row count exceeds limit"));
        }
        if let Some(b) = budget {
            b.take(count.saturating_mul(size_of::<u64>()))?;
        }
        match self.coding {
            Coding::Constant => Ok(vec![self.constant_val; count]),
            Coding::VarintPlain | Coding::VarintDeltaByKey | Coding::VarintRelative => {
                decode_varints(self.data, count)
            }
            Coding::VarintDelta => {
                let mut ints = decode_varints(self.data, count)?;
                delta_decode_u64_in_place(&mut ints);
                Ok(ints)
            }
            Coding::DictVarint => decode_dict_u64_budgeted(self.data, count, budget),
            Coding::Binned2b => decode_binned_2b(self.data, self.bypass, count),
            Coding::NonZeroBitmap => decode_nonzero_u64(self.data, count),
            _ => Err(ObjectError::Layout(format!(
                "unexpected coding {:?} for u64 column",
                self.coding
            ))),
        }
    }

    pub fn decode_raw_u64(
        &self,
        count: usize,
        budget: Option<&DecodeBudget>,
    ) -> Result<Vec<u64>, ObjectError> {
        if count > MAX_TOTAL_ROWS {
            return Err(ObjectError::Limit("row count exceeds limit"));
        }
        if let Some(b) = budget {
            b.take(count.saturating_mul(size_of::<u64>()))?;
        }
        match self.coding {
            Coding::Constant => Ok(vec![self.constant_val; count]),
            Coding::VarintPlain
            | Coding::VarintDelta
            | Coding::VarintDeltaByKey
            | Coding::VarintRelative => decode_varints(self.data, count),
            Coding::DictVarint => decode_dict_u64_budgeted(self.data, count, budget),
            Coding::Binned2b => decode_binned_2b(self.data, self.bypass, count),
            Coding::NonZeroBitmap => decode_nonzero_u64(self.data, count),
            _ => Err(ObjectError::Layout(format!(
                "unexpected coding {:?} for raw u64 column",
                self.coding
            ))),
        }
    }

    pub fn decode_u32(
        &self,
        count: usize,
        budget: Option<&DecodeBudget>,
    ) -> Result<Vec<u32>, ObjectError> {
        if count > MAX_TOTAL_ROWS {
            return Err(ObjectError::Limit("row count exceeds limit"));
        }
        if let Some(b) = budget {
            b.take(count.saturating_mul(size_of::<u32>()))?;
        }
        match self.coding {
            Coding::Constant => {
                let v = u32::try_from(self.constant_val)
                    .map_err(|_| ObjectError::Layout("column value exceeds u32".into()))?;
                Ok(vec![v; count])
            }
            Coding::VarintPlain | Coding::VarintDeltaByKey | Coding::VarintRelative => {
                if count > self.data.len() {
                    return Err(ObjectError::Truncated);
                }
                let mut out = Vec::with_capacity(count);
                let mut at = 0;
                for _ in 0..count {
                    let v = read_varint(self.data, &mut at)?;
                    out.push(
                        u32::try_from(v)
                            .map_err(|_| ObjectError::Layout("column value exceeds u32".into()))?,
                    );
                }
                if at != self.data.len() {
                    return Err(ObjectError::Layout(
                        "trailing bytes in varint column".into(),
                    ));
                }
                Ok(out)
            }
            Coding::VarintDelta => {
                if count > self.data.len() {
                    return Err(ObjectError::Truncated);
                }
                let mut out = Vec::with_capacity(count);
                let mut at = 0;
                let mut prev = 0_u64;
                for _ in 0..count {
                    let raw = read_varint(self.data, &mut at)?;
                    let v = prev.wrapping_add(zigzag_decode(raw).cast_unsigned());
                    prev = v;
                    out.push(
                        u32::try_from(v)
                            .map_err(|_| ObjectError::Layout("column value exceeds u32".into()))?,
                    );
                }
                if at != self.data.len() {
                    return Err(ObjectError::Layout(
                        "trailing bytes in varint column".into(),
                    ));
                }
                Ok(out)
            }
            _ => {
                let vals = self.decode_u64(count, budget)?;
                vals.into_iter()
                    .map(|v| {
                        u32::try_from(v)
                            .map_err(|_| ObjectError::Layout("column value exceeds u32".into()))
                    })
                    .collect()
            }
        }
    }

    pub fn decode_u8(
        &self,
        count: usize,
        budget: Option<&DecodeBudget>,
    ) -> Result<Vec<u8>, ObjectError> {
        if count > MAX_TOTAL_ROWS {
            return Err(ObjectError::Limit("row count exceeds limit"));
        }
        if let Some(b) = budget {
            b.take(count)?;
        }
        match self.coding {
            Coding::Constant => {
                let v = u8::try_from(self.constant_val)
                    .map_err(|_| ObjectError::Layout("column value exceeds u8".into()))?;
                Ok(vec![v; count])
            }
            Coding::VarintPlain | Coding::VarintDeltaByKey | Coding::VarintRelative => {
                if count > self.data.len() {
                    return Err(ObjectError::Truncated);
                }
                let mut out = Vec::with_capacity(count);
                let mut at = 0;
                for _ in 0..count {
                    let v = read_varint(self.data, &mut at)?;
                    out.push(
                        u8::try_from(v)
                            .map_err(|_| ObjectError::Layout("column value exceeds u8".into()))?,
                    );
                }
                if at != self.data.len() {
                    return Err(ObjectError::Layout(
                        "trailing bytes in varint column".into(),
                    ));
                }
                Ok(out)
            }
            Coding::VarintDelta => {
                if count > self.data.len() {
                    return Err(ObjectError::Truncated);
                }
                let mut out = Vec::with_capacity(count);
                let mut at = 0;
                let mut prev = 0_u64;
                for _ in 0..count {
                    let raw = read_varint(self.data, &mut at)?;
                    let v = prev.wrapping_add(zigzag_decode(raw).cast_unsigned());
                    prev = v;
                    out.push(
                        u8::try_from(v)
                            .map_err(|_| ObjectError::Layout("column value exceeds u8".into()))?,
                    );
                }
                if at != self.data.len() {
                    return Err(ObjectError::Layout(
                        "trailing bytes in varint column".into(),
                    ));
                }
                Ok(out)
            }
            _ => {
                let vals = self.decode_u64(count, budget)?;
                vals.into_iter()
                    .map(|v| {
                        u8::try_from(v)
                            .map_err(|_| ObjectError::Layout("column value exceeds u8".into()))
                    })
                    .collect()
            }
        }
    }
}

#[derive(Debug, Clone)]
struct ColumnSlot {
    frame_idx: usize,
    coding: Coding,
    key_col: u8,
    constant_val: u64,
    data_range: Range<usize>,
    bypass_range: Range<usize>,
}

#[derive(Debug)]
enum ColumnMap {
    Small(Vec<(u16, u8, ColumnSlot)>),
    Large(HashMap<(u16, u8), ColumnSlot>),
}

impl ColumnMap {
    const SMALL_LIMIT: usize = 32;

    const fn new() -> Self {
        Self::Small(Vec::new())
    }

    fn len(&self) -> usize {
        match self {
            Self::Small(v) => v.len(),
            Self::Large(m) => m.len(),
        }
    }

    fn reserve(&mut self, additional: usize) {
        match self {
            Self::Small(v) => {
                if v.len().saturating_add(additional) > Self::SMALL_LIMIT {
                    let mut map = HashMap::with_capacity_and_hasher(
                        v.len().saturating_add(additional),
                        foldhash::fast::RandomState::default(),
                    );
                    for (g, c, s) in v.drain(..) {
                        map.insert((g, c), s);
                    }
                    *self = Self::Large(map);
                } else {
                    v.reserve(additional);
                }
            }
            Self::Large(m) => m.reserve(additional),
        }
    }

    fn get(&self, group: u16, col_id: u8) -> Option<&ColumnSlot> {
        match self {
            Self::Small(v) => v
                .iter()
                .find(|(g, c, _)| *g == group && *c == col_id)
                .map(|(_, _, s)| s),
            Self::Large(m) => m.get(&(group, col_id)),
        }
    }

    fn insert(&mut self, group: u16, col_id: u8, slot: ColumnSlot) -> bool {
        match self {
            Self::Small(v) => {
                if v.iter().any(|(g, c, _)| *g == group && *c == col_id) {
                    return true;
                }
                if v.len() >= Self::SMALL_LIMIT {
                    let mut map = HashMap::with_capacity_and_hasher(
                        v.len() + 1,
                        foldhash::fast::RandomState::default(),
                    );
                    for (g, c, s) in v.drain(..) {
                        map.insert((g, c), s);
                    }
                    let dup = map.insert((group, col_id), slot).is_some();
                    *self = Self::Large(map);
                    dup
                } else {
                    v.push((group, col_id, slot));
                    false
                }
            }
            Self::Large(m) => m.insert((group, col_id), slot).is_some(),
        }
    }
}

#[derive(Debug)]
pub struct PageReader<'a> {
    pub rows: u32,
    pub groups: Vec<GroupEntry>,
    raw_frames: Vec<Cow<'a, [u8]>>,
    bypass_frames: Vec<&'a [u8]>,
    columns: ColumnMap,
    budget: Option<&'a DecodeBudget>,
}

impl<'a> PageReader<'a> {
    fn materialize(&self, slot: &ColumnSlot) -> DecodedColumn<'_> {
        DecodedColumn {
            coding: slot.coding,
            key_col: slot.key_col,
            constant_val: slot.constant_val,
            data: &self.raw_frames[slot.frame_idx][slot.data_range.clone()],
            bypass: &self.bypass_frames[slot.frame_idx][slot.bypass_range.clone()],
        }
    }

    pub fn column(&self, group: u16, col_id: u8) -> Result<DecodedColumn<'_>, ObjectError> {
        self.columns
            .get(group, col_id)
            .map(|slot| self.materialize(slot))
            .ok_or_else(|| {
                ObjectError::Layout(format!("missing column ({group}, {col_id}) in page"))
            })
    }

    #[must_use]
    pub fn optional_column(&self, group: u16, col_id: u8) -> Option<DecodedColumn<'_>> {
        self.columns
            .get(group, col_id)
            .map(|slot| self.materialize(slot))
    }

    pub fn charge(&self, bytes: usize) -> Result<(), ObjectError> {
        if let Some(budget) = self.budget {
            budget.take(bytes)?;
        }
        Ok(())
    }

    #[must_use]
    pub const fn budget(&self) -> Option<&'a DecodeBudget> {
        self.budget
    }
}

#[derive(Debug)]
pub struct SectionReader<'a, 'b> {
    pub kind: SectionKind,
    pub version: u8,
    pub time_unit: TimeUnit,
    pub rows: u32,
    pub uncompressed_len: u32,
    pub base_time: i64,
    pages: Vec<PageEntry>,
    page_slices: Vec<&'a [u8]>,
    budget: Option<&'b DecodeBudget>,
}

impl<'a> SectionReader<'a, 'static> {
    pub fn open(bytes: &'a [u8], expected_kind: SectionKind) -> Result<Self, ObjectError> {
        Self::open_inner(bytes, expected_kind, None)
    }
}

impl<'a, 'b> SectionReader<'a, 'b> {
    pub fn open_budgeted(
        bytes: &'a [u8],
        expected_kind: SectionKind,
        budget: &'b DecodeBudget,
    ) -> Result<Self, ObjectError> {
        Self::open_inner(bytes, expected_kind, Some(budget))
    }

    #[expect(
        clippy::too_many_lines,
        reason = "validates section header and optional page directory in one pass"
    )]
    fn open_inner(
        bytes: &'a [u8],
        expected_kind: SectionKind,
        budget: Option<&'b DecodeBudget>,
    ) -> Result<Self, ObjectError> {
        let header = SectionHeader::read_from_bytes(
            bytes
                .get(..SECTION_HEADER_LEN)
                .ok_or(ObjectError::Truncated)?,
        )
        .map_err(|_| ObjectError::Truncated)?;

        let kind = SectionKind::from_number(header.kind.get()).ok_or_else(|| {
            ObjectError::UnknownValue {
                what: "section kind",
                value: u64::from(header.kind.get()),
            }
        })?;
        if kind != expected_kind {
            return Err(ObjectError::Layout(format!(
                "expected section {expected_kind:?}, got {kind:?}"
            )));
        }

        if header.reserved != 0 || (header.flags & !SECTION_FLAG_PAGED) != 0 {
            return Err(ObjectError::NonZeroReserved("section header"));
        }

        let time_unit =
            TimeUnit::from_number(header.time_unit).ok_or_else(|| ObjectError::UnknownValue {
                what: "time unit",
                value: u64::from(header.time_unit),
            })?;
        let page_count = header.page_count.get() as usize;
        if page_count == 0 || page_count > MAX_PAGES {
            return Err(ObjectError::Limit("invalid page count in section"));
        }

        let rows = header.rows.get();
        if (rows as usize) > MAX_TOTAL_ROWS {
            return Err(ObjectError::Limit("section row count exceeds limit"));
        }

        let is_paged = header.flags & SECTION_FLAG_PAGED != 0;
        if !is_paged {
            if page_count != 1 {
                return Err(ObjectError::Layout(
                    "unpaged section must have page_count == 1".into(),
                ));
            }
            if (header.uncompressed_len.get() as usize) > MAX_PAGE_RAW_BYTES {
                return Err(ObjectError::Limit(
                    "section uncompressed_len exceeds maximum page size",
                ));
            }
            let payload = bytes
                .get(SECTION_HEADER_LEN..)
                .ok_or(ObjectError::Truncated)?;
            let stored_len =
                u32::try_from(payload.len()).map_err(|_| ObjectError::Limit("page exceeds u32"))?;
            let entry = PageEntry {
                min_key: i64::MIN.into(),
                max_key: i64::MAX.into(),
                stored_len: stored_len.into(),
                uncompressed_len: header.uncompressed_len,
                rows: header.rows,
            };
            return Ok(Self {
                kind,
                version: header.version,
                time_unit,
                rows,
                uncompressed_len: header.uncompressed_len.get(),
                base_time: header.base_time.get(),
                pages: vec![entry],
                page_slices: vec![payload],
                budget,
            });
        }

        let dir_len = page_count
            .checked_mul(PAGE_ENTRY_LEN)
            .ok_or(ObjectError::Limit("page directory overflow"))?;
        let dir_end = SECTION_HEADER_LEN
            .checked_add(dir_len)
            .ok_or(ObjectError::Limit("page directory overflow"))?;
        let dir_bytes = bytes
            .get(SECTION_HEADER_LEN..dir_end)
            .ok_or(ObjectError::Truncated)?;
        let entries =
            <[PageEntry]>::ref_from_bytes(dir_bytes).map_err(|_| ObjectError::Truncated)?;

        let mut at = dir_end;
        let mut page_slices = Vec::with_capacity(page_count);
        let mut sum_rows = 0_u32;
        for entry in entries {
            if entry.min_key.get() > entry.max_key.get() {
                return Err(ObjectError::Layout(
                    "page entry min_key exceeds max_key".into(),
                ));
            }
            if (entry.uncompressed_len.get() as usize) > MAX_PAGE_RAW_BYTES {
                return Err(ObjectError::Limit(
                    "page uncompressed_len exceeds maximum page size",
                ));
            }
            let plen = entry.stored_len.get() as usize;
            let next = at
                .checked_add(plen)
                .ok_or(ObjectError::Limit("page slice overflow"))?;
            let slice = bytes.get(at..next).ok_or(ObjectError::Truncated)?;
            page_slices.push(slice);
            at = next;
            sum_rows = sum_rows
                .checked_add(entry.rows.get())
                .ok_or(ObjectError::Limit("section row count overflow"))?;
        }
        if at != bytes.len() {
            return Err(ObjectError::Layout(
                "trailing bytes after paged section payloads".into(),
            ));
        }
        if sum_rows != rows {
            return Err(ObjectError::Layout(
                "sum of page row counts disagrees with section header".into(),
            ));
        }

        Ok(Self {
            kind,
            version: header.version,
            time_unit,
            rows,
            uncompressed_len: header.uncompressed_len.get(),
            base_time: header.base_time.get(),
            pages: entries.to_vec(),
            page_slices,
            budget,
        })
    }

    #[must_use]
    pub fn page_entry(&self, index: usize) -> Option<&PageEntry> {
        self.pages.get(index)
    }

    #[must_use]
    pub const fn page_count(&self) -> usize {
        self.pages.len()
    }

    #[must_use]
    pub fn pages(&self) -> &[PageEntry] {
        &self.pages
    }

    #[must_use]
    pub fn pages_overlapping(&self, query_min: i64, query_max: i64) -> Vec<usize> {
        self.pages
            .iter()
            .enumerate()
            .filter_map(|(idx, p)| {
                (p.max_key.get() >= query_min && p.min_key.get() <= query_max).then_some(idx)
            })
            .collect()
    }

    pub fn read_page(&self, page_index: usize) -> Result<PageReader<'_>, ObjectError> {
        self.read_page_frames(page_index, usize::MAX)
    }

    pub fn read_single_page(&self) -> Result<PageReader<'_>, ObjectError> {
        if self.pages.len() != 1 {
            return Err(ObjectError::Layout(format!(
                "section {:?} expected 1 page, found {}",
                self.kind,
                self.pages.len()
            )));
        }
        let page = self.read_page(0)?;
        if page.rows != self.rows {
            return Err(ObjectError::Layout(format!(
                "section {:?} page row count disagrees with section header",
                self.kind
            )));
        }
        Ok(page)
    }

    pub fn read_page_frames(
        &self,
        page_index: usize,
        max_frames: usize,
    ) -> Result<PageReader<'_>, ObjectError> {
        let entry = self
            .pages
            .get(page_index)
            .ok_or_else(|| ObjectError::Layout(format!("page {page_index} out of bounds")))?;
        let slice = *self
            .page_slices
            .get(page_index)
            .ok_or_else(|| ObjectError::Layout(format!("page {page_index} out of bounds")))?;

        let local_budget = DecodeBudget::new(MAX_PAGE_RAW_BYTES);
        let budget = self.budget.unwrap_or(&local_budget);

        let mut at = 0_usize;
        let frame_count = usize::from(*slice.get(at).ok_or(ObjectError::Truncated)?);
        at += 1;
        if frame_count == 0 || frame_count > 16 {
            return Err(ObjectError::Limit("invalid frame count in page"));
        }

        let mut groups: Vec<GroupEntry> = Vec::new();
        let frames_to_decode = frame_count.min(max_frames);
        let mut raw_frames: Vec<Cow<'_, [u8]>> = Vec::with_capacity(frames_to_decode);
        let mut bypass_frames: Vec<&[u8]> = Vec::with_capacity(frames_to_decode);
        let mut columns = ColumnMap::new();
        let mut total_uncomp = 0_usize;

        for frame_idx in 0..frame_count {
            let codec_raw = *slice.get(at).ok_or(ObjectError::Truncated)?;
            at += 1;
            let codec = Codec::from_number(codec_raw).ok_or_else(|| ObjectError::UnknownValue {
                what: "frame codec",
                value: u64::from(codec_raw),
            })?;
            let stored_len = usize::try_from(read_varint(slice, &mut at)?)
                .map_err(|_| ObjectError::Limit("frame stored_len overflow"))?;
            let uncomp_len = usize::try_from(read_varint(slice, &mut at)?)
                .map_err(|_| ObjectError::Limit("frame uncompressed_len overflow"))?;
            let bypass_len = usize::try_from(read_varint(slice, &mut at)?)
                .map_err(|_| ObjectError::Limit("frame bypass_len overflow"))?;
            total_uncomp = total_uncomp
                .checked_add(uncomp_len)
                .and_then(|n| n.checked_add(bypass_len))
                .ok_or(ObjectError::Limit("page uncompressed size overflow"))?;

            let stored_end = at
                .checked_add(stored_len)
                .ok_or(ObjectError::Limit("frame end overflow"))?;
            let bypass_end = stored_end
                .checked_add(bypass_len)
                .ok_or(ObjectError::Limit("frame bypass end overflow"))?;
            let stored_bytes = slice.get(at..stored_end).ok_or(ObjectError::Truncated)?;
            let bypass_bytes = slice
                .get(stored_end..bypass_end)
                .ok_or(ObjectError::Truncated)?;
            at = bypass_end;

            if frame_idx >= frames_to_decode {
                continue;
            }

            if bypass_len > MAX_PAGE_RAW_BYTES {
                return Err(ObjectError::Limit("bypass stream exceeds maximum size"));
            }
            budget.take(bypass_len)?;
            let raw = decompress_cow_budgeted(codec, stored_bytes, uncomp_len, budget)?;
            decode_frame_into(frame_idx, &raw, bypass_bytes, &mut groups, &mut columns)?;
            raw_frames.push(raw);
            bypass_frames.push(bypass_bytes);
        }

        if at != slice.len() {
            return Err(ObjectError::Layout(
                "trailing bytes after page frames".into(),
            ));
        }
        if total_uncomp != entry.uncompressed_len.get() as usize {
            return Err(ObjectError::Layout(
                "page uncompressed_len disagrees with frame headers".into(),
            ));
        }

        Ok(PageReader {
            rows: entry.rows.get(),
            groups,
            raw_frames,
            bypass_frames,
            columns,
            budget: self.budget,
        })
    }

    pub fn charge(&self, bytes: usize) -> Result<(), ObjectError> {
        if let Some(budget) = self.budget {
            budget.take(bytes)?;
        }
        Ok(())
    }

    #[must_use]
    pub const fn budget(&self) -> Option<&'b DecodeBudget> {
        self.budget
    }
}

#[expect(
    clippy::too_many_lines,
    reason = "parses page frame header, group table, and column descriptors"
)]
fn decode_frame_into(
    frame_idx: usize,
    raw: &[u8],
    bypass: &[u8],
    groups: &mut Vec<GroupEntry>,
    columns: &mut ColumnMap,
) -> Result<(), ObjectError> {
    let mut at = 0_usize;
    let group_count = usize::try_from(read_varint(raw, &mut at)?)
        .map_err(|_| ObjectError::Limit("group count overflow"))?;
    if groups.len().saturating_add(group_count) > MAX_GROUPS {
        return Err(ObjectError::Limit("too many groups"));
    }
    let groups_bytes_len = group_count
        .checked_mul(GROUP_ENTRY_LEN)
        .ok_or(ObjectError::Limit("group bytes overflow"))?;
    let groups_end = at
        .checked_add(groups_bytes_len)
        .ok_or(ObjectError::Limit("group end overflow"))?;
    let group_entries =
        <[GroupEntry]>::ref_from_bytes(raw.get(at..groups_end).ok_or(ObjectError::Truncated)?)
            .map_err(|_| ObjectError::Truncated)?;
    let group_base = groups.len();
    groups.extend_from_slice(group_entries);
    at = groups_end;

    let col_count = usize::try_from(read_varint(raw, &mut at)?)
        .map_err(|_| ObjectError::Limit("column count overflow"))?;
    if columns.len().saturating_add(col_count) > MAX_COLUMNS {
        return Err(ObjectError::Limit("too many columns"));
    }
    columns.reserve(col_count);

    let mut specs = Vec::with_capacity(col_count);
    for _ in 0..col_count {
        let enc_group = read_varint(raw, &mut at)?;
        let group = if enc_group == 0 {
            WHOLE_SECTION_GROUP
        } else {
            let local = usize::try_from(enc_group - 1)
                .map_err(|_| ObjectError::Layout("group index overflow".into()))?;
            let resolved = if group_count > 0 {
                if local >= group_count {
                    return Err(ObjectError::Layout("column group out of bounds".into()));
                }
                group_base + local
            } else {
                if local >= groups.len() {
                    return Err(ObjectError::Layout("column group out of bounds".into()));
                }
                local
            };
            u16::try_from(resolved).map_err(|_| ObjectError::Limit("group index exceeds u16"))?
        };
        let col_id = *raw.get(at).ok_or(ObjectError::Truncated)?;
        let coding_raw = *raw.get(at + 1).ok_or(ObjectError::Truncated)?;
        let key_col = *raw.get(at + 2).ok_or(ObjectError::Truncated)?;
        at += 3;
        let coding = Coding::from_number(coding_raw).ok_or_else(|| ObjectError::UnknownValue {
            what: "column coding",
            value: u64::from(coding_raw),
        })?;
        let (constant_val, data_len, bypass_len) = if coding == Coding::Constant {
            (read_varint(raw, &mut at)?, 0_usize, 0_usize)
        } else {
            let dlen = usize::try_from(read_varint(raw, &mut at)?)
                .map_err(|_| ObjectError::Limit("column payload len overflow"))?;
            let blen = if coding == Coding::Binned2b {
                usize::try_from(read_varint(raw, &mut at)?)
                    .map_err(|_| ObjectError::Limit("column bypass len overflow"))?
            } else {
                0_usize
            };
            (0_u64, dlen, blen)
        };
        specs.push((
            group,
            col_id,
            coding,
            key_col,
            constant_val,
            data_len,
            bypass_len,
        ));
    }

    let mut b_at = 0_usize;
    for (group, col_id, coding, key_col, constant_val, data_len, bypass_len) in specs {
        let d_end = at
            .checked_add(data_len)
            .ok_or(ObjectError::Limit("column payload end overflow"))?;
        if d_end > raw.len() {
            return Err(ObjectError::Truncated);
        }
        let data_range = at..d_end;
        at = d_end;

        let b_end = b_at
            .checked_add(bypass_len)
            .ok_or(ObjectError::Limit("column bypass end overflow"))?;
        if b_end > bypass.len() {
            return Err(ObjectError::Truncated);
        }
        let bypass_range = b_at..b_end;
        b_at = b_end;

        if columns.insert(
            group,
            col_id,
            ColumnSlot {
                frame_idx,
                coding,
                key_col,
                constant_val,
                data_range,
                bypass_range,
            },
        ) {
            return Err(ObjectError::Layout(format!(
                "duplicate column ({group}, {col_id})"
            )));
        }
    }

    if at != raw.len() || b_at != bypass.len() {
        return Err(ObjectError::Layout(
            "trailing bytes in decoded page frame".into(),
        ));
    }

    Ok(())
}
