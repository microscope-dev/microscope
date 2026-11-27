//! 8-bit Binary Fuse filter (`BinaryFuse8`, Mueller-Lemire) for fast negative
//! membership lookups with ~1/256 false-positive rate and ~1.13 bytes/key.

use std::hash::Hasher;

use twox_hash::XxHash3_64;

use super::ObjectError;

const FILTER_VERSION: u8 = 1;
const FILTER_HEADER_LEN: usize = 14;
const MAX_SEGMENT_LENGTH_LOG2: u8 = 18;
pub const MAX_FILTER_BYTES: usize = 16 << 20;

const DOMAIN_FRAME_ADDR: u64 = 0x4652_414d_455f_4144;
const DOMAIN_FUNCTION_NAME: u64 = 0x4655_4e43_5f4e_414d;
const DOMAIN_DIMENSION: u64 = 0x4449_4d5f_4b56_0001;
const DOMAIN_PROCESS_NAME: u64 = 0x5052_4f43_5f4e_414d;

#[must_use]
pub fn filter_key_frame_addr(build_id: &str, address: u64) -> u64 {
    let mut hasher = XxHash3_64::with_seed(DOMAIN_FRAME_ADDR);
    hasher.write(build_id.as_bytes());
    hasher.write(&[0]);
    hasher.write(&address.to_le_bytes());
    hasher.finish()
}

#[must_use]
pub fn filter_key_function_name(name: &str) -> u64 {
    let mut hasher = XxHash3_64::with_seed(DOMAIN_FUNCTION_NAME);
    hasher.write(name.as_bytes());
    hasher.finish()
}

#[must_use]
pub fn filter_key_dimension(name: &str, value: &str) -> u64 {
    let mut hasher = XxHash3_64::with_seed(DOMAIN_DIMENSION);
    hasher.write(name.as_bytes());
    hasher.write(&[0]);
    hasher.write(value.as_bytes());
    hasher.finish()
}

#[must_use]
pub fn filter_key_process_name(name: &str) -> u64 {
    let mut hasher = XxHash3_64::with_seed(DOMAIN_PROCESS_NAME);
    hasher.write(name.as_bytes());
    hasher.finish()
}

#[inline]
const fn mix64(mut x: u64) -> u64 {
    x ^= x >> 33;
    x = x.wrapping_mul(0xff51_afd7_ed55_8ccd);
    x ^= x >> 33;
    x = x.wrapping_mul(0xc4ce_b9fe_1a85_ec53);
    x ^= x >> 33;
    x
}

#[expect(
    clippy::cast_possible_truncation,
    reason = "8-bit fingerprint intentionally truncates mixed 64-bit hash"
)]
#[inline]
const fn fingerprint(hash: u64) -> u8 {
    (hash ^ (hash >> 32)) as u8
}

#[expect(
    clippy::cast_possible_truncation,
    reason = "binary fuse slot hashes intentionally extract 32-bit slices"
)]
#[inline]
fn hashes_from_mix(
    hash: u64,
    segment_length: u32,
    segment_length_mask: u32,
    segment_count_length: u32,
) -> (usize, usize, usize) {
    let hi = ((u128::from(hash) * u128::from(segment_count_length)) >> 64) as u32;
    let h0 = hi;
    let mut h1 = h0.wrapping_add(segment_length);
    let mut h2 = h1.wrapping_add(segment_length);
    h1 ^= ((hash >> 18) as u32) & segment_length_mask;
    h2 ^= (hash as u32) & segment_length_mask;
    (h0 as usize, h1 as usize, h2 as usize)
}

#[expect(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss,
    reason = "segment sizing follows the standard Mueller-Lemire binary fuse formula"
)]
fn initial_geometry(n: usize) -> (u8, u32) {
    let log2 = if n <= 16 {
        2_u8
    } else {
        let log_n = (n as f64).log(3.33_f64) + 2.25;
        let bits = (log_n.floor() as u32).clamp(2, u32::from(MAX_SEGMENT_LENGTH_LOG2));
        bits as u8
    };
    let seg_len = 1_u32 << log2;
    let base_factor = if n <= 2 {
        2.0
    } else {
        let ratio = (1_000_000_f64).log(n as f64);
        0.25_f64.mul_add(ratio.max(1.0), 0.875).max(1.125)
    };
    let factor = if n < 128 {
        base_factor.max(1.35)
    } else {
        base_factor
    };
    let capacity = ((n as f64) * factor).ceil() as u32;
    let seg_count = capacity.div_ceil(seg_len).max(3);
    (log2, seg_count)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BinaryFuse8 {
    seed: u64,
    segment_length_log2: u8,
    segment_length: u32,
    segment_length_mask: u32,
    segment_count: u32,
    segment_count_length: u32,
    fingerprints: Vec<u8>,
}

impl BinaryFuse8 {
    #[must_use]
    pub const fn empty() -> Self {
        Self {
            seed: 0,
            segment_length_log2: 0,
            segment_length: 0,
            segment_length_mask: 0,
            segment_count: 0,
            segment_count_length: 0,
            fingerprints: Vec::new(),
        }
    }

    /// Builds a filter via 3-way hypergraph peeling and reverse XOR assignment,
    /// retrying with new seeds and widening `segment_count` after 16 failed
    /// seeds at the current size.
    #[must_use]
    #[expect(
        clippy::too_many_lines,
        reason = "peeling loop and reverse assignment pass share filter geometry state"
    )]
    pub fn build(keys: &[u64]) -> Self {
        let mut unique = keys.to_vec();
        unique.sort_unstable();
        unique.dedup();
        if unique.is_empty() {
            return Self::empty();
        }

        let n = unique.len();
        let (seg_log2, mut seg_count) = initial_geometry(n);
        let seg_len = 1_u32 << seg_log2;
        let seg_mask = seg_len - 1;
        let mut seed = 0x9e37_79b9_7f4a_7c15_u64;

        let mut attempts_at_size = 0_u32;
        loop {
            let Some(seg_count_len) = seg_count
                .checked_sub(2)
                .and_then(|c| c.checked_mul(seg_len))
            else {
                return Self::empty();
            };
            let Some(array_len) = (seg_count as usize).checked_mul(seg_len as usize) else {
                return Self::empty();
            };
            if array_len > MAX_FILTER_BYTES {
                return Self::empty();
            }

            let mut t2count = vec![0_u32; array_len];
            let mut t2hash = vec![0_u64; array_len];

            for &key in &unique {
                let h = mix64(key.wrapping_add(seed));
                let (h0, h1, h2) = hashes_from_mix(h, seg_len, seg_mask, seg_count_len);
                if let Some(c) = t2count.get_mut(h0) {
                    *c += 1;
                }
                if let Some(v) = t2hash.get_mut(h0) {
                    *v ^= h;
                }
                if let Some(c) = t2count.get_mut(h1) {
                    *c += 1;
                }
                if let Some(v) = t2hash.get_mut(h1) {
                    *v ^= h;
                }
                if let Some(c) = t2count.get_mut(h2) {
                    *c += 1;
                }
                if let Some(v) = t2hash.get_mut(h2) {
                    *v ^= h;
                }
            }

            let mut alone = Vec::with_capacity(array_len / 2);
            for (idx, &count) in t2count.iter().enumerate() {
                if count == 1 {
                    alone.push(idx);
                }
            }

            let mut reverse_order: Vec<(u64, usize)> = Vec::with_capacity(n);
            while let Some(index) = alone.pop() {
                if t2count.get(index).copied().unwrap_or(0) != 1 {
                    continue;
                }
                let h = t2hash.get(index).copied().unwrap_or(0);
                reverse_order.push((h, index));
                let slots =
                    <[usize; 3]>::from(hashes_from_mix(h, seg_len, seg_mask, seg_count_len));
                for slot in slots {
                    if let Some(c) = t2count.get_mut(slot) {
                        *c = c.saturating_sub(1);
                        if *c == 1 {
                            alone.push(slot);
                        }
                    }
                    if let Some(v) = t2hash.get_mut(slot) {
                        *v ^= h;
                    }
                }
            }

            if reverse_order.len() == n {
                let mut fingerprints = vec![0_u8; array_len];
                for &(h, found) in reverse_order.iter().rev() {
                    let (h0, h1, h2) = hashes_from_mix(h, seg_len, seg_mask, seg_count_len);
                    let f0 = fingerprints.get(h0).copied().unwrap_or(0);
                    let f1 = fingerprints.get(h1).copied().unwrap_or(0);
                    let f2 = fingerprints.get(h2).copied().unwrap_or(0);
                    if let Some(slot) = fingerprints.get_mut(found) {
                        *slot = fingerprint(h) ^ f0 ^ f1 ^ f2;
                    }
                }
                return Self {
                    seed,
                    segment_length_log2: seg_log2,
                    segment_length: seg_len,
                    segment_length_mask: seg_mask,
                    segment_count: seg_count,
                    segment_count_length: seg_count_len,
                    fingerprints,
                };
            }

            seed = seed.wrapping_add(0x517c_c1b7_2722_0a95);
            attempts_at_size += 1;
            if attempts_at_size >= 16 {
                seg_count = seg_count.saturating_add(1);
                attempts_at_size = 0;
            }
        }
    }

    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.fingerprints.is_empty()
    }

    #[must_use]
    pub const fn byte_len(&self) -> usize {
        FILTER_HEADER_LEN + self.fingerprints.len()
    }

    #[must_use]
    pub fn contains(&self, key: u64) -> bool {
        if self.fingerprints.is_empty() {
            return false;
        }
        let h = mix64(key.wrapping_add(self.seed));
        let (h0, h1, h2) = hashes_from_mix(
            h,
            self.segment_length,
            self.segment_length_mask,
            self.segment_count_length,
        );
        let f0 = self.fingerprints.get(h0).copied().unwrap_or(0);
        let f1 = self.fingerprints.get(h1).copied().unwrap_or(0);
        let f2 = self.fingerprints.get(h2).copied().unwrap_or(0);
        (f0 ^ f1 ^ f2) == fingerprint(h)
    }

    #[must_use]
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.byte_len());
        out.push(FILTER_VERSION);
        out.extend_from_slice(&self.seed.to_le_bytes());
        out.push(self.segment_length_log2);
        out.extend_from_slice(&self.segment_count.to_le_bytes());
        out.extend_from_slice(&self.fingerprints);
        out
    }

    pub fn from_bytes(bytes: &[u8]) -> Result<Self, ObjectError> {
        if bytes.len() < FILTER_HEADER_LEN {
            return Err(ObjectError::Truncated);
        }
        let version = *bytes.first().ok_or(ObjectError::Truncated)?;
        if version != FILTER_VERSION {
            return Err(ObjectError::UnknownValue {
                what: "filter version",
                value: u64::from(version),
            });
        }
        let seed_bytes: [u8; 8] = bytes
            .get(1..9)
            .and_then(|s| s.try_into().ok())
            .ok_or(ObjectError::Truncated)?;
        let seed = u64::from_le_bytes(seed_bytes);
        let seg_log2 = *bytes.get(9).ok_or(ObjectError::Truncated)?;
        let count_bytes: [u8; 4] = bytes
            .get(10..14)
            .and_then(|s| s.try_into().ok())
            .ok_or(ObjectError::Truncated)?;
        let seg_count = u32::from_le_bytes(count_bytes);
        let payload = bytes
            .get(FILTER_HEADER_LEN..)
            .ok_or(ObjectError::Truncated)?;

        if seg_count == 0 {
            if seed != 0 || seg_log2 != 0 || !payload.is_empty() {
                return Err(ObjectError::Layout(
                    "empty filter must have zero seed, geometry, and payload".into(),
                ));
            }
            return Ok(Self::empty());
        }

        if seg_log2 == 0 || seg_log2 > MAX_SEGMENT_LENGTH_LOG2 || seg_count < 3 {
            return Err(ObjectError::Layout(
                "invalid binary fuse filter geometry".into(),
            ));
        }

        let seg_len = 1_u32 << seg_log2;
        let array_len = (seg_count as usize)
            .checked_mul(seg_len as usize)
            .ok_or(ObjectError::Limit("binary fuse filter size overflow"))?;
        if array_len > MAX_FILTER_BYTES {
            return Err(ObjectError::Limit(
                "binary fuse filter exceeds maximum size",
            ));
        }
        if payload.len() != array_len {
            return Err(ObjectError::Layout(
                "binary fuse filter payload length mismatch".into(),
            ));
        }

        Ok(Self {
            seed,
            segment_length_log2: seg_log2,
            segment_length: seg_len,
            segment_length_mask: seg_len - 1,
            segment_count: seg_count,
            segment_count_length: (seg_count - 2) * seg_len,
            fingerprints: payload.to_vec(),
        })
    }
}
