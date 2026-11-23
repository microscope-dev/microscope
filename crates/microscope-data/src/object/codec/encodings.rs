use foldhash::HashMap;

use super::{Coding, DENSE_KEY_LIMIT, DecodeBudget};
use crate::capture::records::NONE;
use crate::object::{MAX_TOTAL_ROWS, ObjectError};

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
    if count > bytes.len() {
        return Err(ObjectError::Truncated);
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

#[must_use]
pub fn delta_by_key_encode_u64(values: &[u64], keys: &[u32]) -> Vec<u64> {
    debug_assert_eq!(values.len(), keys.len());
    let max_key = keys.iter().copied().max().unwrap_or(0) as usize;
    let mut out = Vec::with_capacity(values.len());
    if max_key < DENSE_KEY_LIMIT && max_key <= values.len().saturating_mul(4) {
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
    if max_key < DENSE_KEY_LIMIT && max_key <= encoded.len().saturating_mul(4) {
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
    if dict_len > bytes.len().saturating_sub(at) {
        return Err(ObjectError::Truncated);
    }
    if let Some(budget) = budget {
        budget.take(dict_len.saturating_mul(size_of::<u64>()))?;
    }

    let mut dict = Vec::with_capacity(dict_len);
    for _ in 0..dict_len {
        dict.push(read_varint(bytes, &mut at)?);
    }

    if count > bytes.len().saturating_sub(at) {
        return Err(ObjectError::Truncated);
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
    for i in 0..n {
        visit_root(i);
    }

    (dfs_parents, old_to_dfs, dfs_to_old)
}

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

pub fn decode_bp_tree(bytes: &[u8], count: usize) -> Result<Vec<u32>, ObjectError> {
    decode_bp_tree_budgeted(bytes, count, None)
}

pub fn decode_bp_tree_budgeted(
    bytes: &[u8],
    count: usize,
    budget: Option<&DecodeBudget>,
) -> Result<Vec<u32>, ObjectError> {
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
    if let Some(b) = budget {
        b.take(count.saturating_mul(size_of::<u32>()))?;
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
    let set_bits: usize = bitmap.iter().map(|b| b.count_ones() as usize).sum();
    if set_bits > bytes.len().saturating_sub(at) {
        return Err(ObjectError::Truncated);
    }
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

    let mut total_nz = 0_usize;
    for &mask in &masks {
        if (mask >> 40) != 0 {
            return Err(ObjectError::Layout(
                "sparse histogram mask sets bit >= 40".into(),
            ));
        }
        total_nz = total_nz.saturating_add(mask.count_ones() as usize);
    }
    if total_nz > bytes.len().saturating_sub(at) {
        return Err(ObjectError::Truncated);
    }

    let mut out = Vec::with_capacity(count);
    for mask in masks {
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
