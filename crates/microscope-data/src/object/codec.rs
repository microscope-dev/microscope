use std::collections::BTreeMap;

use foldhash::HashMap;
use zerocopy::little_endian::{I64, U16, U32};
use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout, Unaligned};

use super::{
    COLUMN_ENTRY_LEN, EncodedSection, GROUP_ENTRY_LEN, MAX_COLUMN_RAW_BYTES, MAX_COLUMNS,
    MAX_GROUPS, MAX_ROWS, ObjectError, SECTION_HEADER_LEN, SectionKind, TimeUnit,
};

pub const WHOLE_SECTION_GROUP: u16 = u16::MAX;
pub const NO_KEY_COLUMN: u8 = u8::MAX;
pub const FLAG_BIT_PACKED: u8 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u8)]
pub enum Coding {
    Plain = 0,
    Delta = 1,
    DeltaByKey = 2,
    Relative = 3,
    Dictionary = 4,
    String = 5,
    Bytes = 6,
    Words = 7,
    Order = 8,
}

impl Coding {
    #[must_use]
    pub const fn from_number(value: u8) -> Option<Self> {
        match value {
            0 => Some(Self::Plain),
            1 => Some(Self::Delta),
            2 => Some(Self::DeltaByKey),
            3 => Some(Self::Relative),
            4 => Some(Self::Dictionary),
            5 => Some(Self::String),
            6 => Some(Self::Bytes),
            7 => Some(Self::Words),
            8 => Some(Self::Order),
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
    pub rows: U32,
    pub groups: U16,
    pub columns: U16,
    pub reserved: U32,
    pub base_time: I64,
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

#[repr(C)]
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, FromBytes, IntoBytes, KnownLayout, Immutable, Unaligned,
)]
pub struct ColumnEntry {
    pub group: U16,
    pub column: u8,
    pub coding: u8,
    pub key: u8,
    pub flags: u8,
    pub codec: u8,
    pub reserved: u8,
    pub stored_len: U32,
}

const _: () = assert!(size_of::<SectionHeader>() == SECTION_HEADER_LEN);
const _: () = assert!(size_of::<GroupEntry>() == GROUP_ENTRY_LEN);
const _: () = assert!(size_of::<ColumnEntry>() == COLUMN_ENTRY_LEN);

#[must_use]
pub const fn zigzag_encode(value: i64) -> u64 {
    ((value << 1) ^ (value >> 63)).cast_unsigned()
}

#[must_use]
pub const fn zigzag_decode(value: u64) -> i64 {
    ((value >> 1).cast_signed()) ^ (-((value & 1).cast_signed()))
}

pub fn write_varint(mut value: u64, out: &mut Vec<u8>) {
    loop {
        let byte = (value & 0x7f) as u8;
        value >>= 7;
        if value == 0 {
            out.push(byte);
            return;
        }
        out.push(byte | 0x80);
    }
}

pub fn read_varint(bytes: &[u8], at: &mut usize) -> Result<u64, ObjectError> {
    let mut value = 0_u64;
    let mut shift = 0_u32;
    for _ in 0..10 {
        let byte = *bytes.get(*at).ok_or(ObjectError::Truncated)?;
        *at += 1;
        if shift == 63 && byte > 1 {
            return Err(ObjectError::Layout("varint overflow".into()));
        }
        value |= u64::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return Ok(value);
        }
        shift += 7;
    }

    Err(ObjectError::Layout("varint exceeds 10 bytes".into()))
}

#[must_use]
pub fn encode_varints(values: &[u64]) -> Vec<u8> {
    let mut out = Vec::with_capacity(values.len());
    for &value in values {
        write_varint(value, &mut out);
    }
    out
}

pub fn decode_varints(bytes: &[u8], count: usize) -> Result<Vec<u64>, ObjectError> {
    if count > MAX_ROWS {
        return Err(ObjectError::Limit("row count exceeds limit"));
    }

    let mut out = Vec::with_capacity(count);
    let mut at = 0;
    for _ in 0..count {
        out.push(read_varint(bytes, &mut at)?);
    }
    if at != bytes.len() {
        return Err(ObjectError::Layout(
            "trailing bytes in varint column".into(),
        ));
    }
    Ok(out)
}

#[must_use]
pub fn encode_bitpacked(values: &[u64]) -> Vec<u8> {
    let max = values.iter().copied().max().unwrap_or(0);
    let width = if max == 0 {
        0_u8
    } else {
        u8::try_from(64 - max.leading_zeros()).unwrap_or(64)
    };
    let total_bits = values.len().saturating_mul(usize::from(width));
    let total_bytes = total_bits.div_ceil(8);
    let mut out = Vec::with_capacity(1 + total_bytes);
    out.push(width);
    if width == 0 {
        return out;
    }

    let mut acc = 0_u128;
    let mut bits = 0_u32;
    let width_u32 = u32::from(width);
    for &value in values {
        acc |= u128::from(value) << bits;
        bits += width_u32;
        while bits >= 8 {
            out.push((acc & 0xff) as u8);
            acc >>= 8;
            bits -= 8;
        }
    }
    if bits > 0 {
        out.push((acc & 0xff) as u8);
    }

    debug_assert_eq!(out.len(), 1 + total_bytes);
    out
}

pub fn decode_bitpacked(bytes: &[u8], count: usize) -> Result<Vec<u64>, ObjectError> {
    if count > MAX_ROWS {
        return Err(ObjectError::Limit("row count exceeds limit"));
    }

    let (&width, rest) = bytes.split_first().ok_or(ObjectError::Truncated)?;
    if width > 64 {
        return Err(ObjectError::Layout(format!(
            "invalid bit-pack width {width}"
        )));
    }

    let total_bits = count
        .checked_mul(usize::from(width))
        .ok_or(ObjectError::Limit("bit-packed size overflow"))?;
    let total_bytes = total_bits.div_ceil(8);
    if rest.len() != total_bytes {
        return Err(ObjectError::Layout(
            "bit-packed column length disagrees with row count".into(),
        ));
    }

    if width == 0 {
        return Ok(vec![0; count]);
    }

    let mask = if width == 64 {
        u64::MAX
    } else {
        (1_u64 << width) - 1
    };
    let width_u32 = u32::from(width);
    let mut out = Vec::with_capacity(count);
    let mut acc = 0_u128;
    let mut bits = 0_u32;
    let mut at = 0;

    for _ in 0..count {
        while bits < width_u32 {
            let byte = *rest.get(at).ok_or(ObjectError::Truncated)?;
            at += 1;
            acc |= u128::from(byte) << bits;
            bits += 8;
        }

        out.push(u64::try_from(acc & u128::from(mask)).unwrap_or(0));
        acc >>= width_u32;
        bits -= width_u32;
    }

    Ok(out)
}

#[must_use]
pub fn encode_ints(values: &[u64], bit_packed: bool) -> Vec<u8> {
    if bit_packed {
        encode_bitpacked(values)
    } else {
        encode_varints(values)
    }
}

pub fn decode_ints(bytes: &[u8], count: usize, bit_packed: bool) -> Result<Vec<u64>, ObjectError> {
    if bit_packed {
        decode_bitpacked(bytes, count)
    } else {
        decode_varints(bytes, count)
    }
}

#[must_use]
pub fn delta_encode_u64(values: &[u64]) -> Vec<u64> {
    let mut out = Vec::with_capacity(values.len());
    let mut prev = 0_u64;
    for &value in values {
        let delta = value.wrapping_sub(prev).cast_signed();
        out.push(zigzag_encode(delta));
        prev = value;
    }
    out
}

#[must_use]
pub fn delta_decode_u64(encoded: &[u64]) -> Vec<u64> {
    let mut out = Vec::with_capacity(encoded.len());
    let mut prev = 0_u64;
    for &raw in encoded {
        let value = prev.wrapping_add(zigzag_decode(raw).cast_unsigned());
        out.push(value);
        prev = value;
    }
    out
}

#[must_use]
pub fn delta_encode_i64(values: &[i64], base: i64) -> Vec<u64> {
    let mut out = Vec::with_capacity(values.len());
    let mut prev = base;
    for &value in values {
        let delta = value.wrapping_sub(prev);
        out.push(zigzag_encode(delta));
        prev = value;
    }
    out
}

#[must_use]
pub fn delta_decode_i64(encoded: &[u64], base: i64) -> Vec<i64> {
    let mut out = Vec::with_capacity(encoded.len());
    let mut prev = base;
    for &raw in encoded {
        let value = prev.wrapping_add(zigzag_decode(raw));
        out.push(value);
        prev = value;
    }
    out
}

#[must_use]
pub fn delta_by_key_encode_u64(values: &[u64], keys: &[u32]) -> Vec<u64> {
    debug_assert_eq!(values.len(), keys.len());
    let mut last: HashMap<u32, u64> = HashMap::default();
    let mut out = Vec::with_capacity(values.len());
    for (&value, &key) in values.iter().zip(keys) {
        let prev = last.get(&key).copied().unwrap_or(0);
        let delta = value.wrapping_sub(prev).cast_signed();
        out.push(zigzag_encode(delta));
        last.insert(key, value);
    }
    out
}

pub fn delta_by_key_decode_u64(encoded: &[u64], keys: &[u32]) -> Result<Vec<u64>, ObjectError> {
    if encoded.len() != keys.len() {
        return Err(ObjectError::Layout(
            "delta-by-key column length disagrees with key column".into(),
        ));
    }

    let mut last: HashMap<u32, u64> = HashMap::default();
    let mut out = Vec::with_capacity(encoded.len());
    for (&raw, &key) in encoded.iter().zip(keys) {
        let prev = last.get(&key).copied().unwrap_or(0);
        let value = prev.wrapping_add(zigzag_decode(raw).cast_unsigned());
        out.push(value);
        last.insert(key, value);
    }
    Ok(out)
}

#[must_use]
pub fn relative_encode_i64(values: &[i64], base: &[i64]) -> Vec<u64> {
    debug_assert_eq!(values.len(), base.len());
    values
        .iter()
        .zip(base)
        .map(|(&value, &anchor)| zigzag_encode(value.wrapping_sub(anchor)))
        .collect()
}

pub fn relative_decode_i64(encoded: &[u64], base: &[i64]) -> Result<Vec<i64>, ObjectError> {
    if encoded.len() != base.len() {
        return Err(ObjectError::Layout(
            "relative column length disagrees with anchor column".into(),
        ));
    }

    Ok(encoded
        .iter()
        .zip(base)
        .map(|(&raw, &anchor)| anchor.wrapping_add(zigzag_decode(raw)))
        .collect())
}

#[must_use]
pub fn relative_encode_u64(values: &[u64], base: &[u64]) -> Vec<u64> {
    debug_assert_eq!(values.len(), base.len());
    values
        .iter()
        .zip(base)
        .map(|(&value, &anchor)| zigzag_encode(value.wrapping_sub(anchor).cast_signed()))
        .collect()
}

pub fn relative_decode_u64(encoded: &[u64], base: &[u64]) -> Result<Vec<u64>, ObjectError> {
    if encoded.len() != base.len() {
        return Err(ObjectError::Layout(
            "relative column length disagrees with anchor column".into(),
        ));
    }

    Ok(encoded
        .iter()
        .zip(base)
        .map(|(&raw, &anchor)| anchor.wrapping_add(zigzag_decode(raw).cast_unsigned()))
        .collect())
}

#[must_use]
pub fn encode_dict_u64(values: &[u64]) -> Vec<u8> {
    let mut dict: Vec<u64> = Vec::new();
    let mut lookup: HashMap<u64, u64> = HashMap::default();
    let mut indices = Vec::with_capacity(values.len());

    for &value in values {
        let index = if let Some(&idx) = lookup.get(&value) {
            idx
        } else {
            let idx = dict.len() as u64;
            dict.push(value);
            lookup.insert(value, idx);
            idx
        };
        indices.push(index);
    }

    let mut out = Vec::new();
    write_varint(dict.len() as u64, &mut out);
    for &entry in &dict {
        write_varint(entry, &mut out);
    }
    for &index in &indices {
        write_varint(index, &mut out);
    }
    out
}

pub fn decode_dict_u64(bytes: &[u8], count: usize) -> Result<Vec<u64>, ObjectError> {
    if count > MAX_ROWS {
        return Err(ObjectError::Limit("row count exceeds limit"));
    }

    let mut at = 0;
    let dict_len = usize::try_from(read_varint(bytes, &mut at)?)
        .map_err(|_| ObjectError::Limit("dictionary size overflow"))?;
    if dict_len > count.max(1) && dict_len > MAX_ROWS {
        return Err(ObjectError::Limit("dictionary exceeds limit"));
    }

    let mut dict = Vec::with_capacity(dict_len.min(MAX_ROWS));
    for _ in 0..dict_len {
        dict.push(read_varint(bytes, &mut at)?);
    }

    let mut out = Vec::with_capacity(count);
    for _ in 0..count {
        let idx = usize::try_from(read_varint(bytes, &mut at)?)
            .map_err(|_| ObjectError::Layout("dictionary index overflow".into()))?;
        let &value = dict
            .get(idx)
            .ok_or_else(|| ObjectError::Layout("dictionary index out of bounds".into()))?;
        out.push(value);
    }

    if at != bytes.len() {
        return Err(ObjectError::Layout(
            "trailing bytes in dictionary column".into(),
        ));
    }

    Ok(out)
}

#[must_use]
pub fn encode_words(records: &[&[u8]], record_len: usize) -> Vec<u8> {
    debug_assert!(record_len > 0 && record_len.is_multiple_of(8));
    let words_per = record_len / 8;
    let mut out = Vec::with_capacity(records.len() * record_len);
    for word in 0..words_per {
        let start = word * 8;
        for record in records {
            if let Some(slice) = record.get(start..start + 8) {
                out.extend_from_slice(slice);
            }
        }
    }
    out
}

pub fn decode_words(bytes: &[u8], count: usize, record_len: usize) -> Result<Vec<u8>, ObjectError> {
    if record_len == 0 || !record_len.is_multiple_of(8) {
        return Err(ObjectError::Layout(
            "word-coded records must be a non-zero multiple of 8 bytes".into(),
        ));
    }

    let expected = count
        .checked_mul(record_len)
        .ok_or(ObjectError::Limit("word column size overflow"))?;
    if bytes.len() != expected {
        return Err(ObjectError::Layout(
            "word column length disagrees with count and stride".into(),
        ));
    }

    let words_per = record_len / 8;
    let mut out = vec![0_u8; expected];
    for word in 0..words_per {
        let col_offset = word * count * 8;
        for row in 0..count {
            let src_start = col_offset + row * 8;
            let dst_start = row * record_len + word * 8;
            let src = bytes
                .get(src_start..src_start + 8)
                .ok_or(ObjectError::Truncated)?;
            let dst = out
                .get_mut(dst_start..dst_start + 8)
                .ok_or(ObjectError::Truncated)?;
            dst.copy_from_slice(src);
        }
    }

    Ok(out)
}

pub fn compress_stream(raw: &[u8], zstd_level: i32) -> Result<(Codec, Vec<u8>), ObjectError> {
    if raw.len() < 32 {
        return Ok((Codec::Raw, raw.to_vec()));
    }

    let compressed = zstd::bulk::compress(raw, zstd_level)
        .map_err(|error| ObjectError::Compression(error.to_string()))?;
    if compressed.len() < raw.len() {
        Ok((Codec::Zstd, compressed))
    } else {
        Ok((Codec::Raw, raw.to_vec()))
    }
}

pub fn decompress_stream(codec: Codec, stored: &[u8]) -> Result<Vec<u8>, ObjectError> {
    match codec {
        Codec::Raw => {
            if stored.len() > MAX_COLUMN_RAW_BYTES {
                return Err(ObjectError::Limit("raw column exceeds maximum size"));
            }
            Ok(stored.to_vec())
        }
        Codec::Zstd => {
            let mut decoder = zstd::stream::read::Decoder::new(stored)
                .map_err(|error| ObjectError::Compression(error.to_string()))?;
            let mut out = Vec::new();
            let mut chunk = [0_u8; 8192];
            loop {
                let read = std::io::Read::read(&mut decoder, &mut chunk)
                    .map_err(|error| ObjectError::Compression(error.to_string()))?;
                if read == 0 {
                    break;
                }
                if out.len().saturating_add(read) > MAX_COLUMN_RAW_BYTES {
                    return Err(ObjectError::Limit(
                        "decompressed column exceeds maximum size",
                    ));
                }
                out.extend_from_slice(chunk.get(..read).ok_or(ObjectError::Truncated)?);
            }
            Ok(out)
        }
    }
}

pub struct SectionWriter {
    kind: SectionKind,
    version: u8,
    time_unit: TimeUnit,
    rows: u32,
    base_time: i64,
    zstd_level: i32,
    groups: Vec<GroupEntry>,
    columns: Vec<ColumnEntry>,
    streams: Vec<Vec<u8>>,
}

impl SectionWriter {
    #[must_use]
    pub const fn new(
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
            groups: Vec::new(),
            columns: Vec::new(),
            streams: Vec::new(),
        }
    }

    pub fn add_group(
        &mut self,
        abi: u8,
        argument: u8,
        number: u16,
        rows: u32,
    ) -> Result<u16, ObjectError> {
        let index =
            u16::try_from(self.groups.len()).map_err(|_| ObjectError::Limit("too many groups"))?;
        if self.groups.len() >= MAX_GROUPS {
            return Err(ObjectError::Limit("too many groups"));
        }

        self.groups.push(GroupEntry {
            abi,
            argument,
            number: number.into(),
            rows: rows.into(),
        });
        Ok(index)
    }

    pub fn add_column(
        &mut self,
        group: u16,
        column: u8,
        coding: Coding,
        key: u8,
        flags: u8,
        raw: &[u8],
    ) -> Result<(), ObjectError> {
        if self.columns.len() >= MAX_COLUMNS {
            return Err(ObjectError::Limit("too many columns"));
        }

        let (codec, stored) = compress_stream(raw, self.zstd_level)?;
        let stored_len = u32::try_from(stored.len())
            .map_err(|_| ObjectError::Limit("column stream exceeds u32"))?;
        self.columns.push(ColumnEntry {
            group: group.into(),
            column,
            coding: coding as u8,
            key,
            flags,
            codec: codec as u8,
            reserved: 0,
            stored_len: stored_len.into(),
        });
        self.streams.push(stored);
        Ok(())
    }

    pub fn finish(self, key: u64) -> Result<EncodedSection, ObjectError> {
        let groups =
            u16::try_from(self.groups.len()).map_err(|_| ObjectError::Limit("too many groups"))?;
        let columns = u16::try_from(self.columns.len())
            .map_err(|_| ObjectError::Limit("too many columns"))?;
        let header = SectionHeader {
            kind: (self.kind as u16).into(),
            version: self.version,
            time_unit: self.time_unit as u8,
            rows: self.rows.into(),
            groups: groups.into(),
            columns: columns.into(),
            reserved: 0.into(),
            base_time: self.base_time.into(),
        };

        let streams_len: usize = self.streams.iter().map(Vec::len).sum();
        let mut bytes = Vec::with_capacity(
            SECTION_HEADER_LEN
                + self.groups.len() * GROUP_ENTRY_LEN
                + self.columns.len() * COLUMN_ENTRY_LEN
                + streams_len,
        );
        bytes.extend_from_slice(header.as_bytes());
        bytes.extend_from_slice(self.groups.as_bytes());
        bytes.extend_from_slice(self.columns.as_bytes());
        for stream in self.streams {
            bytes.extend_from_slice(&stream);
        }

        Ok(EncodedSection {
            kind: self.kind,
            rows: self.rows,
            key,
            bytes,
        })
    }
}

#[derive(Debug, Clone, Copy)]
pub struct ColumnSlice<'a> {
    pub entry: ColumnEntry,
    pub coding: Coding,
    pub codec: Codec,
    pub data: &'a [u8],
}

impl ColumnSlice<'_> {
    #[must_use]
    pub const fn bit_packed(&self) -> bool {
        self.entry.flags & FLAG_BIT_PACKED != 0
    }

    pub fn decompress(&self) -> Result<Vec<u8>, ObjectError> {
        decompress_stream(self.codec, self.data)
    }
}

#[derive(Debug)]
pub struct SectionReader<'a> {
    pub kind: SectionKind,
    pub version: u8,
    pub time_unit: TimeUnit,
    pub rows: u32,
    pub base_time: i64,
    pub groups: &'a [GroupEntry],
    columns: BTreeMap<(u16, u8), ColumnSlice<'a>>,
}

impl<'a> SectionReader<'a> {
    #[expect(
        clippy::too_many_lines,
        reason = "validates header, groups, and column slices in order"
    )]
    pub fn open(bytes: &'a [u8], expected_kind: SectionKind) -> Result<Self, ObjectError> {
        let header = SectionHeader::read_from_bytes(
            bytes
                .get(..SECTION_HEADER_LEN)
                .ok_or(ObjectError::Truncated)?,
        )
        .map_err(|_| ObjectError::Truncated)?;
        if header.reserved.get() != 0 {
            return Err(ObjectError::NonZeroReserved("section header"));
        }

        let kind = SectionKind::from_number(header.kind.get()).ok_or_else(|| {
            ObjectError::UnknownValue {
                what: "section kind",
                value: u64::from(header.kind.get()),
            }
        })?;
        if kind != expected_kind {
            return Err(ObjectError::Layout(format!(
                "section header kind {kind:?} disagrees with entry {expected_kind:?}"
            )));
        }

        let time_unit =
            TimeUnit::from_number(header.time_unit).ok_or_else(|| ObjectError::UnknownValue {
                what: "time unit",
                value: u64::from(header.time_unit),
            })?;
        let rows = header.rows.get();
        if rows as usize > MAX_ROWS {
            return Err(ObjectError::Limit("section row count exceeds limit"));
        }

        let group_count = usize::from(header.groups.get());
        let column_count = usize::from(header.columns.get());
        if group_count > MAX_GROUPS {
            return Err(ObjectError::Limit("too many groups"));
        }

        if column_count > MAX_COLUMNS {
            return Err(ObjectError::Limit("too many columns"));
        }

        let groups_end = SECTION_HEADER_LEN
            .checked_add(group_count * GROUP_ENTRY_LEN)
            .ok_or(ObjectError::Limit("group table overflow"))?;
        let cols_end = groups_end
            .checked_add(column_count * COLUMN_ENTRY_LEN)
            .ok_or(ObjectError::Limit("column table overflow"))?;

        let groups = <[GroupEntry]>::ref_from_bytes(
            bytes
                .get(SECTION_HEADER_LEN..groups_end)
                .ok_or(ObjectError::Truncated)?,
        )
        .map_err(|_| ObjectError::Truncated)?;
        let col_entries = <[ColumnEntry]>::ref_from_bytes(
            bytes
                .get(groups_end..cols_end)
                .ok_or(ObjectError::Truncated)?,
        )
        .map_err(|_| ObjectError::Truncated)?;

        let mut at = cols_end;
        let mut columns = BTreeMap::new();
        for &entry in col_entries {
            if entry.reserved != 0 {
                return Err(ObjectError::NonZeroReserved("column entry"));
            }

            if entry.flags & !FLAG_BIT_PACKED != 0 {
                return Err(ObjectError::UnknownValue {
                    what: "column flags",
                    value: u64::from(entry.flags),
                });
            }

            let group = entry.group.get();
            if group != WHOLE_SECTION_GROUP && usize::from(group) >= group_count {
                return Err(ObjectError::Layout("column refers to missing group".into()));
            }

            let coding =
                Coding::from_number(entry.coding).ok_or_else(|| ObjectError::UnknownValue {
                    what: "column coding",
                    value: u64::from(entry.coding),
                })?;
            let codec =
                Codec::from_number(entry.codec).ok_or_else(|| ObjectError::UnknownValue {
                    what: "column codec",
                    value: u64::from(entry.codec),
                })?;
            let end = at
                .checked_add(entry.stored_len.get() as usize)
                .ok_or(ObjectError::Limit("column stream offset overflow"))?;
            let data = bytes.get(at..end).ok_or(ObjectError::Truncated)?;
            at = end;
            if columns
                .insert(
                    (group, entry.column),
                    ColumnSlice {
                        entry,
                        coding,
                        codec,
                        data,
                    },
                )
                .is_some()
            {
                return Err(ObjectError::Layout("duplicate column entry".into()));
            }
        }

        if at != bytes.len() {
            return Err(ObjectError::Layout("trailing bytes in section".into()));
        }

        Ok(Self {
            kind,
            version: header.version,
            time_unit,
            rows,
            base_time: header.base_time.get(),
            groups,
            columns,
        })
    }

    pub fn column(&self, group: u16, column: u8) -> Result<ColumnSlice<'a>, ObjectError> {
        self.columns
            .get(&(group, column))
            .copied()
            .ok_or_else(|| ObjectError::Layout(format!("missing column ({group}, {column})")))
    }

    #[must_use]
    pub fn optional_column(&self, group: u16, column: u8) -> Option<ColumnSlice<'a>> {
        self.columns.get(&(group, column)).copied()
    }
}
