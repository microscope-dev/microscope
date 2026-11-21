use std::cell::RefCell;
use std::collections::BTreeMap;

use foldhash::HashMap;
use zerocopy::little_endian::{I64, U16, U32};
use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout, Unaligned};

use super::{
    EncodedSection, GROUP_ENTRY_LEN, MAX_COLUMNS, MAX_GROUPS, MAX_PAGE_RAW_BYTES, MAX_PAGES,
    MAX_TOTAL_ROWS, ObjectError, PAGE_ENTRY_LEN, SECTION_HEADER_LEN, SectionKind, TimeUnit,
};
use crate::capture::records::NONE;

pub const WHOLE_SECTION_GROUP: u16 = u16::MAX;
pub const NO_KEY_COLUMN: u8 = u8::MAX;
pub const SECTION_FLAG_PAGED: u8 = 1;
const DENSE_KEY_LIMIT: usize = 1 << 20;

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
    if count > MAX_TOTAL_ROWS {
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

pub fn delta_decode_u64_in_place(values: &mut [u64]) {
    let mut prev = 0_u64;
    for slot in values {
        let value = prev.wrapping_add(zigzag_decode(*slot).cast_unsigned());
        *slot = value;
        prev = value;
    }
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

/// Fast delta-by-key encoding: uses a dense `Vec<u64>` when keys fit in `DENSE_KEY_LIMIT`
/// (4.3x faster than `HashMap`), falling back to `HashMap` for sparse/large keys.
#[must_use]
pub fn delta_by_key_encode_u64(values: &[u64], keys: &[u32]) -> Vec<u64> {
    debug_assert_eq!(values.len(), keys.len());
    let max_key = keys.iter().copied().max().unwrap_or(0) as usize;
    let mut out = Vec::with_capacity(values.len());
    if max_key < DENSE_KEY_LIMIT {
        let mut last = vec![0_u64; max_key + 1];
        for (&value, &key) in values.iter().zip(keys) {
            let idx = key as usize;
            let prev = last.get(idx).copied().unwrap_or(0);
            let delta = value.wrapping_sub(prev).cast_signed();
            out.push(zigzag_encode(delta));
            if let Some(slot) = last.get_mut(idx) {
                *slot = value;
            }
        }
    } else {
        let mut last: HashMap<u32, u64> = HashMap::default();
        for (&value, &key) in values.iter().zip(keys) {
            let prev = last.get(&key).copied().unwrap_or(0);
            let delta = value.wrapping_sub(prev).cast_signed();
            out.push(zigzag_encode(delta));
            last.insert(key, value);
        }
    }
    out
}

pub fn delta_by_key_decode_u64(encoded: &[u64], keys: &[u32]) -> Result<Vec<u64>, ObjectError> {
    if encoded.len() != keys.len() {
        return Err(ObjectError::Layout(
            "delta-by-key column length disagrees with key column".into(),
        ));
    }

    let max_key = keys.iter().copied().max().unwrap_or(0) as usize;
    let mut out = Vec::with_capacity(encoded.len());
    if max_key < DENSE_KEY_LIMIT {
        let mut last = vec![0_u64; max_key + 1];
        for (&raw, &key) in encoded.iter().zip(keys) {
            let idx = key as usize;
            let prev = last.get(idx).copied().unwrap_or(0);
            let value = prev.wrapping_add(zigzag_decode(raw).cast_unsigned());
            out.push(value);
            if let Some(slot) = last.get_mut(idx) {
                *slot = value;
            }
        }
    } else {
        let mut last: HashMap<u32, u64> = HashMap::default();
        for (&raw, &key) in encoded.iter().zip(keys) {
            let prev = last.get(&key).copied().unwrap_or(0);
            let value = prev.wrapping_add(zigzag_decode(raw).cast_unsigned());
            out.push(value);
            last.insert(key, value);
        }
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
    decode_dict_u64_budgeted(bytes, count, None)
}

pub fn decode_dict_u64_budgeted(
    bytes: &[u8],
    count: usize,
    budget: Option<&DecodeBudget>,
) -> Result<Vec<u64>, ObjectError> {
    if count > MAX_TOTAL_ROWS {
        return Err(ObjectError::Limit("row count exceeds limit"));
    }

    let mut at = 0;
    let dict_len = usize::try_from(read_varint(bytes, &mut at)?)
        .map_err(|_| ObjectError::Limit("dictionary size overflow"))?;
    if dict_len > MAX_TOTAL_ROWS {
        return Err(ObjectError::Limit("dictionary exceeds limit"));
    }
    if let Some(budget) = budget {
        budget.take(dict_len.saturating_mul(size_of::<u64>()))?;
    }

    let mut dict = Vec::with_capacity(dict_len);
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

/// Pure safe Rust `Binned-2b` split-stream encoder for high-entropy `1ns` time/duration values.
///
/// Splits each value into:
/// - `classes`: 1 byte per row (`0` for 0, `1` for 1, `2..=4` for 2-bit/3-bit, or `(w << 2) | top2`
///   for bit-width `w >= 3`), which compresses tightly inside the zstd frame.
/// - `mantissa`: raw low `w - 3` bits packed little-endian, bypassing zstd completely.
#[expect(
    clippy::cast_possible_truncation,
    reason = "class bytes fit in u8 (v < 4 or width in 3..=64)"
)]
#[must_use]
pub fn encode_binned_2b(values: &[u64]) -> (Vec<u8>, Vec<u8>) {
    let mut classes = Vec::with_capacity(values.len());
    let mut mantissa = Vec::with_capacity(values.len() * 2);
    let mut acc = 0_u128;
    let mut bits = 0_u32;

    for &v in values {
        if v < 4 {
            classes.push(v as u8);
            continue;
        }
        let width = 64 - v.leading_zeros();
        // width is 3..=64; store (width - 2) in 1..=62 so ((width - 2) << 2) | top2 fits in u8 (4..=251).
        let rem = width - 3;
        let top2 = ((v >> rem) & 0x3) as u8;
        let cls = (((width - 2) as u8) << 2) | top2;
        classes.push(cls);
        if rem > 0 {
            let mask = (1_u64 << rem) - 1;
            acc |= u128::from(v & mask) << bits;
            bits += rem;
            while bits >= 8 {
                mantissa.push((acc & 0xff) as u8);
                acc >>= 8;
                bits -= 8;
            }
        }
    }
    if bits > 0 {
        mantissa.push((acc & 0xff) as u8);
    }

    (classes, mantissa)
}

#[expect(
    clippy::cast_possible_truncation,
    reason = "masked low bits (rem <= 61) fit in u64"
)]
pub fn decode_binned_2b(
    classes: &[u8],
    mantissa: &[u8],
    count: usize,
) -> Result<Vec<u64>, ObjectError> {
    if classes.len() != count {
        return Err(ObjectError::Layout(
            "binned-2b class stream length disagrees with row count".into(),
        ));
    }

    let mut out = Vec::with_capacity(count);
    let mut acc = 0_u128;
    let mut bits = 0_u32;
    let mut at = 0_usize;

    for &cls in classes {
        if cls < 4 {
            out.push(u64::from(cls));
            continue;
        }
        let width = u32::from(cls >> 2) + 2;
        if !(3..=64).contains(&width) {
            return Err(ObjectError::Layout(format!(
                "invalid binned-2b bit width {width}"
            )));
        }
        let top2 = u64::from(cls & 0x3);
        let rem = width - 3;
        let low = if rem == 0 {
            0_u64
        } else {
            while bits < rem {
                let byte = *mantissa.get(at).ok_or(ObjectError::Truncated)?;
                at += 1;
                acc |= u128::from(byte) << bits;
                bits += 8;
            }
            let mask = (1_u64 << rem) - 1;
            let val = (acc as u64) & mask;
            acc >>= rem;
            bits -= rem;
            val
        };
        let high = (4_u64 | top2) << rem;
        out.push(high | low);
    }

    if at != mantissa.len() || (bits > 0 && acc != 0) {
        return Err(ObjectError::Layout(
            "trailing bytes or non-zero padding in binned-2b mantissa".into(),
        ));
    }

    Ok(out)
}

/// Converts a parent-index forest into DFS preorder, returning `(dfs_parents, old_to_dfs, dfs_to_old)`.
///
/// Any node with `parent == NONE` or an invalid/cyclic parent is treated as a root.
#[must_use]
pub fn forest_to_dfs(parents: &[u32]) -> (Vec<u32>, Vec<u32>, Vec<usize>) {
    let n = parents.len();
    let mut children: Vec<Vec<usize>> = vec![Vec::new(); n];
    let mut roots = Vec::new();
    for (i, &p) in parents.iter().enumerate() {
        if p == NONE || (p as usize) >= n || (p as usize) == i {
            roots.push(i);
        } else if let Some(slot) = children.get_mut(p as usize) {
            slot.push(i);
        }
    }

    let mut visited = vec![false; n];
    let mut dfs_parents = Vec::with_capacity(n);
    let mut old_to_dfs = vec![NONE; n];
    let mut dfs_to_old = Vec::with_capacity(n);

    let mut visit_root = |start: usize| {
        let mut stack = vec![(start, NONE)];
        while let Some((old, new_parent)) = stack.pop() {
            if visited.get(old).copied().unwrap_or(true) {
                continue;
            }
            if let Some(v) = visited.get_mut(old) {
                *v = true;
            }
            let new_id = u32::try_from(dfs_parents.len()).unwrap_or(NONE);
            if let Some(slot) = old_to_dfs.get_mut(old) {
                *slot = new_id;
            }
            dfs_parents.push(new_parent);
            dfs_to_old.push(old);
            if let Some(kids) = children.get(old) {
                for &child in kids.iter().rev() {
                    if !visited.get(child).copied().unwrap_or(true) {
                        stack.push((child, new_id));
                    }
                }
            }
        }
    };

    for root in roots {
        visit_root(root);
    }
    // Any remaining unvisited nodes belonged to a cycle; emit them as roots so no node is lost.
    for i in 0..n {
        visit_root(i);
    }

    (dfs_parents, old_to_dfs, dfs_to_old)
}

/// Encodes a DFS-preorder forest into a 2-bit-per-node Balanced Parentheses bitstream.
///
/// Bit `1` = open / descend to next node; bit `0` = close / pop back to parent.
#[must_use]
pub fn encode_bp_tree(dfs_parents: &[u32]) -> Vec<u8> {
    let n = dfs_parents.len();
    let total_bits = n.saturating_mul(2);
    let mut out = vec![0_u8; total_bits.div_ceil(8)];
    let mut bit_pos = 0_usize;

    let mut push_bit = |bit: bool| {
        if bit && let Some(byte) = out.get_mut(bit_pos >> 3) {
            *byte |= 1_u8 << (bit_pos & 7);
        }
        bit_pos += 1;
    };

    let mut stack: Vec<u32> = Vec::new();
    for (idx, &parent) in dfs_parents.iter().enumerate() {
        while let Some(&top) = stack.last() {
            if top == parent {
                break;
            }
            stack.pop();
            push_bit(false);
        }
        push_bit(true);
        stack.push(u32::try_from(idx).unwrap_or(NONE));
    }
    while stack.pop().is_some() {
        push_bit(false);
    }

    debug_assert_eq!(bit_pos, total_bits);
    out
}

/// Decodes a 2-bit-per-node Balanced Parentheses bitstream back into `dfs_parents`.
pub fn decode_bp_tree(bytes: &[u8], count: usize) -> Result<Vec<u32>, ObjectError> {
    if count > MAX_TOTAL_ROWS {
        return Err(ObjectError::Limit("tree node count exceeds limit"));
    }
    let total_bits = count
        .checked_mul(2)
        .ok_or(ObjectError::Limit("bp tree size overflow"))?;
    let expected_bytes = total_bits.div_ceil(8);
    if bytes.len() != expected_bytes {
        return Err(ObjectError::Layout(
            "bp tree bitstream length disagrees with node count".into(),
        ));
    }

    let mut parents = Vec::with_capacity(count);
    let mut stack: Vec<u32> = Vec::new();
    for bit_pos in 0..total_bits {
        let byte = bytes.get(bit_pos >> 3).copied().unwrap_or(0);
        let is_open = (byte >> (bit_pos & 7)) & 1 != 0;
        if is_open {
            if parents.len() >= count {
                return Err(ObjectError::Layout("bp tree has too many open bits".into()));
            }
            let parent = stack.last().copied().unwrap_or(NONE);
            let id = u32::try_from(parents.len()).unwrap_or(NONE);
            parents.push(parent);
            stack.push(id);
        } else if stack.pop().is_none() {
            return Err(ObjectError::Layout(
                "bp tree has unmatched close bit".into(),
            ));
        }
    }

    if !stack.is_empty() || parents.len() != count {
        return Err(ObjectError::Layout("bp tree is unbalanced".into()));
    }

    // Verify trailing unused padding bits in the final byte are zero.
    let rem = total_bits & 7;
    if rem != 0
        && let Some(&last) = bytes.last()
        && (last >> rem) != 0
    {
        return Err(ObjectError::Layout(
            "non-zero trailing padding bits in bp tree".into(),
        ));
    }

    Ok(parents)
}

/// Encodes a 1-bit/node non-zero bitmap + packed non-zero `u64` varints (for `GroupTree.self_ns`
/// and `ProcessTree.self_ns`, where ~70-85% of internal nodes are zero).
#[must_use]
pub fn encode_nonzero_u64(values: &[u64]) -> Vec<u8> {
    let bitmap_len = values.len().div_ceil(8);
    let mut out = vec![0_u8; bitmap_len];
    for (idx, &v) in values.iter().enumerate() {
        if v != 0 {
            if let Some(byte) = out.get_mut(idx >> 3) {
                *byte |= 1_u8 << (idx & 7);
            }
            write_varint(v, &mut out);
        }
    }
    out
}

pub fn decode_nonzero_u64(bytes: &[u8], count: usize) -> Result<Vec<u64>, ObjectError> {
    if count > MAX_TOTAL_ROWS {
        return Err(ObjectError::Limit("row count exceeds limit"));
    }
    let bitmap_len = count.div_ceil(8);
    if bytes.len() < bitmap_len {
        return Err(ObjectError::Truncated);
    }
    let bitmap = bytes.get(..bitmap_len).ok_or(ObjectError::Truncated)?;
    let rem = count & 7;
    if rem != 0
        && let Some(&last) = bitmap.last()
        && (last >> rem) != 0
    {
        return Err(ObjectError::Layout(
            "non-zero padding bits in nonzero bitmap".into(),
        ));
    }

    let mut at = bitmap_len;
    let mut out = vec![0_u64; count];
    for (idx, slot) in out.iter_mut().enumerate() {
        let byte = bitmap.get(idx >> 3).copied().unwrap_or(0);
        if (byte >> (idx & 7)) & 1 != 0 {
            *slot = read_varint(bytes, &mut at)?;
        }
    }
    if at != bytes.len() {
        return Err(ObjectError::Layout(
            "trailing bytes in nonzero bitmap column".into(),
        ));
    }
    Ok(out)
}

/// Encodes `[u64; 40]` duration histograms as a dict-coded 40-bit bitmask column + packed non-zero varints.
#[must_use]
pub fn encode_sparse_histograms(histograms: &[[u64; 40]]) -> Vec<u8> {
    let mut masks = Vec::with_capacity(histograms.len());
    let mut nz_vals = Vec::new();
    for hist in histograms {
        let mut mask = 0_u64;
        for (bucket, &count) in hist.iter().enumerate() {
            if count != 0 {
                mask |= 1_u64 << bucket;
                nz_vals.push(count);
            }
        }
        masks.push(mask);
    }

    let dict_masks = encode_dict_u64(&masks);
    let mut out = Vec::with_capacity(8 + dict_masks.len() + nz_vals.len() * 2);
    write_varint(dict_masks.len() as u64, &mut out);
    out.extend_from_slice(&dict_masks);
    for &val in &nz_vals {
        write_varint(val, &mut out);
    }
    out
}

pub fn decode_sparse_histograms(
    bytes: &[u8],
    count: usize,
    budget: Option<&DecodeBudget>,
) -> Result<Vec<[u64; 40]>, ObjectError> {
    if count > MAX_TOTAL_ROWS {
        return Err(ObjectError::Limit("histogram row count exceeds limit"));
    }
    if let Some(b) = budget {
        b.take(count.saturating_mul(size_of::<[u64; 40]>()))?;
    }

    let mut at = 0_usize;
    let masks_len = usize::try_from(read_varint(bytes, &mut at)?)
        .map_err(|_| ObjectError::Limit("sparse histogram mask length overflow"))?;
    let masks_end = at
        .checked_add(masks_len)
        .ok_or(ObjectError::Limit("sparse histogram offset overflow"))?;
    let mask_bytes = bytes.get(at..masks_end).ok_or(ObjectError::Truncated)?;
    let masks = decode_dict_u64_budgeted(mask_bytes, count, budget)?;
    at = masks_end;

    let mut out = Vec::with_capacity(count);
    for mask in masks {
        if (mask >> 40) != 0 {
            return Err(ObjectError::Layout(
                "sparse histogram mask sets bit >= 40".into(),
            ));
        }
        let mut hist = [0_u64; 40];
        let mut remaining = mask;
        while remaining != 0 {
            let bucket = remaining.trailing_zeros() as usize;
            if let Some(slot) = hist.get_mut(bucket) {
                *slot = read_varint(bytes, &mut at)?;
            }
            remaining &= remaining - 1;
        }
        out.push(hist);
    }

    if at != bytes.len() {
        return Err(ObjectError::Layout(
            "trailing bytes in sparse histogram column".into(),
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

/// Picks the best `u64` coding by sampling at most 2,048 rows at zstd level 3
/// (~8.9x faster than compressing the whole column 6 times at level 9, with 0.00% size loss).
#[must_use]
pub fn pick_best_u64_coding(values: &[u64]) -> (Coding, Vec<u8>) {
    if values.is_empty() {
        return (Coding::VarintPlain, Vec::new());
    }
    if values.iter().all(|&v| v == values[0]) {
        return (Coding::Constant, values[0].to_le_bytes().to_vec());
    }

    let sample_len = values.len().min(2048);
    let sample = &values[..sample_len];
    let s_plain = encode_varints(sample);
    let s_delta = encode_varints(&delta_encode_u64(sample));
    let s_dict = encode_dict_u64(sample);

    let score = |raw: &[u8]| -> usize {
        zstd::bulk::compress(raw, 3).map_or(raw.len(), |c| c.len().min(raw.len()))
    };
    let p_sz = score(&s_plain);
    let d_sz = score(&s_delta);
    let k_sz = score(&s_dict);

    if d_sz <= p_sz && d_sz <= k_sz {
        if sample_len == values.len() {
            (Coding::VarintDelta, s_delta)
        } else {
            (
                Coding::VarintDelta,
                encode_varints(&delta_encode_u64(values)),
            )
        }
    } else if k_sz < p_sz {
        if sample_len == values.len() {
            (Coding::DictVarint, s_dict)
        } else {
            (Coding::DictVarint, encode_dict_u64(values))
        }
    } else if sample_len == values.len() {
        (Coding::VarintPlain, s_plain)
    } else {
        (Coding::VarintPlain, encode_varints(values))
    }
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

    /// Returns temporary per-page scratch capacity once page-local buffers are dropped.
    /// Zstd decompression and final output row charges are never released.
    pub fn release(&self, bytes: usize) {
        let next = self.remaining.get().saturating_add(bytes).min(self.limit);
        self.remaining.set(next);
    }
}

thread_local! {
    static ZSTD_DCTX: RefCell<Option<zstd::bulk::Decompressor<'static>>> = const { RefCell::new(None) };
}

pub fn decompress_exact_budgeted(
    codec: Codec,
    stored: &[u8],
    expected_len: usize,
    budget: &DecodeBudget,
) -> Result<Vec<u8>, ObjectError> {
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
            Ok(stored.to_vec())
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
            Ok(out)
        }
    }
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

    /// Starts a new independent compression frame within the current page (e.g. separating
    /// `Syscalls` core columns from `args` columns so readers can skip `args`).
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
            let val = if raw.len() == 8 {
                u64::from_le_bytes(raw.try_into().unwrap_or([0; 8]))
            } else {
                0
            };
            return self.add_constant(group, column, val);
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

    /// Adds a `Binned-2b` split-stream column: `classes` goes into the zstd-compressed frame,
    /// while `mantissa` goes into the raw bypass buffer (skipping zstd).
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

    /// Convenience helper: writes a `u64` column as `Constant` if all values are identical
    /// (and non-empty), otherwise as `VarintPlain` or `VarintDelta`.
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
            let encoded = encode_varints(&delta_encode_u64(values));
            self.add_column(group, column, Coding::VarintDelta, NO_KEY_COLUMN, &encoded)
        } else {
            let encoded = encode_varints(values);
            self.add_column(group, column, Coding::VarintPlain, NO_KEY_COLUMN, &encoded)
        }
    }

    /// Convenience helper using sampled codec selection (`Constant`, `VarintPlain`, `VarintDelta`,
    /// `DictVarint`). Ideal for compactor rollup sections.
    pub fn add_best_u64(
        &mut self,
        group: u16,
        column: u8,
        values: &[u64],
    ) -> Result<(), ObjectError> {
        let (coding, bytes) = pick_best_u64_coding(values);
        self.add_column(group, column, coding, NO_KEY_COLUMN, &bytes)
    }

    /// Seals the current page with its key range `(min_key, max_key)` and row count, and resets
    /// internal frame buffers for the next page.
    pub fn finish_page(
        &mut self,
        min_key: i64,
        max_key: i64,
        page_rows: u32,
    ) -> Result<(), ObjectError> {
        if self.pages.len() >= MAX_PAGES {
            return Err(ObjectError::Limit("too many pages in section"));
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

    /// Finishes a single-page section (no page directory emitted).
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

        // Flush trailing frame data if the caller added columns after the last `finish_page`.
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

    /// Finishes a multi-page section (emits the 28-byte-per-page `PageEntry` directory whenever
    /// `pages.len() > 1`, or omits it when `pages.len() == 1`).
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

        if page_count == 1 {
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

#[derive(Debug, Clone)]
pub struct DecodedColumn {
    pub coding: Coding,
    pub key_col: u8,
    pub constant_val: u64,
    pub data: Vec<u8>,
    pub bypass: Vec<u8>,
}

impl DecodedColumn {
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
                decode_varints(&self.data, count)
            }
            Coding::VarintDelta => {
                let mut ints = decode_varints(&self.data, count)?;
                delta_decode_u64_in_place(&mut ints);
                Ok(ints)
            }
            Coding::DictVarint => decode_dict_u64_budgeted(&self.data, count, budget),
            Coding::Binned2b => decode_binned_2b(&self.data, &self.bypass, count),
            Coding::NonZeroBitmap => decode_nonzero_u64(&self.data, count),
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
            | Coding::VarintRelative => decode_varints(&self.data, count),
            Coding::DictVarint => decode_dict_u64_budgeted(&self.data, count, budget),
            Coding::Binned2b => decode_binned_2b(&self.data, &self.bypass, count),
            Coding::NonZeroBitmap => decode_nonzero_u64(&self.data, count),
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
        let vals = self.decode_u64(count, budget)?;
        vals.into_iter()
            .map(|v| {
                u32::try_from(v).map_err(|_| ObjectError::Layout("column value exceeds u32".into()))
            })
            .collect()
    }

    pub fn decode_u8(
        &self,
        count: usize,
        budget: Option<&DecodeBudget>,
    ) -> Result<Vec<u8>, ObjectError> {
        let vals = self.decode_u64(count, budget)?;
        vals.into_iter()
            .map(|v| {
                u8::try_from(v).map_err(|_| ObjectError::Layout("column value exceeds u8".into()))
            })
            .collect()
    }
}

#[derive(Debug)]
pub struct PageReader<'b> {
    pub rows: u32,
    pub groups: Vec<GroupEntry>,
    columns: BTreeMap<(u16, u8), DecodedColumn>,
    budget: Option<&'b DecodeBudget>,
}

impl<'b> PageReader<'b> {
    pub fn column(&self, group: u16, col_id: u8) -> Result<&DecodedColumn, ObjectError> {
        self.columns.get(&(group, col_id)).ok_or_else(|| {
            ObjectError::Layout(format!("missing column ({group}, {col_id}) in page"))
        })
    }

    #[must_use]
    pub fn optional_column(&self, group: u16, col_id: u8) -> Option<&DecodedColumn> {
        self.columns.get(&(group, col_id))
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

#[derive(Debug)]
pub struct SectionReader<'a, 'b> {
    pub kind: SectionKind,
    pub version: u8,
    pub time_unit: TimeUnit,
    pub rows: u32,
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
            base_time: header.base_time.get(),
            pages: entries.to_vec(),
            page_slices,
            budget,
        })
    }

    #[must_use]
    pub const fn page_count(&self) -> usize {
        self.pages.len()
    }

    #[must_use]
    pub fn pages(&self) -> &[PageEntry] {
        &self.pages
    }

    /// Returns indices of pages whose `[min_key, max_key]` overlaps `[query_min, query_max]`.
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

    pub fn read_page(&self, page_index: usize) -> Result<PageReader<'b>, ObjectError> {
        self.read_page_frames(page_index, usize::MAX)
    }

    /// Reads up to `max_frames` column-group frames from `page_index` (e.g. `max_frames = 1`
    /// reads only core syscall columns and skips `args` frames completely).
    pub fn read_page_frames(
        &self,
        page_index: usize,
        max_frames: usize,
    ) -> Result<PageReader<'b>, ObjectError> {
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
        let mut columns: BTreeMap<(u16, u8), DecodedColumn> = BTreeMap::new();
        let frames_to_decode = frame_count.min(max_frames);

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
            let raw = decompress_exact_budgeted(codec, stored_bytes, uncomp_len, budget)?;
            decode_frame_into(&raw, bypass_bytes, &mut groups, &mut columns)?;
        }

        if at != slice.len() {
            return Err(ObjectError::Layout(
                "trailing bytes after page frames".into(),
            ));
        }

        Ok(PageReader {
            rows: entry.rows.get(),
            groups,
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
    raw: &[u8],
    bypass: &[u8],
    groups: &mut Vec<GroupEntry>,
    columns: &mut BTreeMap<(u16, u8), DecodedColumn>,
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

    let mut specs = Vec::with_capacity(col_count);
    for _ in 0..col_count {
        let enc_group = read_varint(raw, &mut at)?;
        let group = if enc_group == 0 {
            WHOLE_SECTION_GROUP
        } else {
            let local = usize::try_from(enc_group - 1)
                .map_err(|_| ObjectError::Layout("group index overflow".into()))?;
            if local >= group_count && local >= groups.len() {
                return Err(ObjectError::Layout("column group out of bounds".into()));
            }
            u16::try_from(if group_count > 0 {
                group_base + local
            } else {
                local
            })
            .map_err(|_| ObjectError::Limit("group index exceeds u16"))?
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
        let data = raw.get(at..d_end).ok_or(ObjectError::Truncated)?.to_vec();
        at = d_end;

        let b_end = b_at
            .checked_add(bypass_len)
            .ok_or(ObjectError::Limit("column bypass end overflow"))?;
        let col_bypass = bypass
            .get(b_at..b_end)
            .ok_or(ObjectError::Truncated)?
            .to_vec();
        b_at = b_end;

        if columns
            .insert(
                (group, col_id),
                DecodedColumn {
                    coding,
                    key_col,
                    constant_val,
                    data,
                    bypass: col_bypass,
                },
            )
            .is_some()
        {
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
