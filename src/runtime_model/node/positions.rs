//! Where in its script a frame of V8's bytecode is: the bytecode's source
//! position table maps its offset to a character offset of the script,
//! and the script's line ends map that to a line and column.
//!
//! A table is a sequence of entries, each a code offset and a source
//! position, both written as the difference from the entry before. V8
//! changed how it writes the code offset in 14.0:
//! before, a zigzag-coded signed integer whose sign marks a statement;
//! since, an unsigned integer whose two low bits mark a breakable position
//! and a statement. Both decode a table into plausible lines, so the
//! encoding is the version's, never a guess, and a table must decode to
//! its exact length.

use super::objects::{Object, Problem, Reader};
use crate::image::v8::{Constant, Field, V8Version};

/// How V8 writes a position table's code offsets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Encoding {
    /// A zigzag-coded signed integer, negative for an entry that is no
    /// statement: V8 before 14.0.
    SignedStatement,

    /// An unsigned integer whose low two bits are the breakable and
    /// statement flags: V8 14.0 and later.
    FlagBits,
}

impl Encoding {
    pub(super) const fn of(version: V8Version) -> Self {
        if version.major >= 14 {
            Self::FlagBits
        } else {
            Self::SignedStatement
        }
    }
}

/// One entry of a decoded table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Entry {
    pub(super) code_offset: i64,

    /// V8's `SourcePosition`, whose script offset and inlining id
    /// [`script_offset`] gives.
    pub(super) position: i64,
}

/// The code offset of a function's entry, before its first instruction.
pub(super) const FUNCTION_ENTRY: i64 = -1;

/// The entries of a table's bytes, or why the bytes are no table.
pub(super) fn decode(bytes: &[u8], encoding: Encoding) -> Result<Vec<Entry>, Problem> {
    let mut entries = Vec::new();
    let mut at = 0;
    // Offsets are deltas from the function's entry, which V8 places at
    // -1 (kFunctionEntryBytecodeOffset).
    let mut code_offset = FUNCTION_ENTRY;
    let mut position = 0_i64;
    while at < bytes.len() {
        let delta = match encoding {
            Encoding::SignedStatement => {
                let value = zigzag(unsigned(bytes, &mut at)?);
                if value >= 0 { value } else { -(value + 1) }
            }
            // V8 reads it as a 32-bit int, so a function's entry, at
            // offset -1, is a delta of -1.
            Encoding::FlagBits => {
                let value = u32::try_from(unsigned(bytes, &mut at)?)
                    .map_err(|_| "a position table's offset is too large")?;
                i64::from(value.cast_signed() >> 2)
            }
        };
        code_offset = code_offset
            .checked_add(delta)
            .ok_or("a position table's offsets overflow")?;
        position = position
            .checked_add(zigzag(unsigned(bytes, &mut at)?))
            .ok_or("a position table's positions overflow")?;
        entries.push(Entry {
            code_offset,
            position,
        });
    }
    Ok(entries)
}

/// The varint at `at`, seven bits a byte, low bits first, as V8 writes
/// its position and baseline offset tables.
pub(super) fn unsigned(bytes: &[u8], at: &mut usize) -> Result<u64, Problem> {
    let mut value = 0_u64;
    for shift in (0..64).step_by(7) {
        let byte = *bytes
            .get(*at)
            .ok_or("a position table ends inside an entry")?;
        *at += 1;
        // A group whose bits reach past the 64th names no value.
        let bits = u64::from(byte & 0x7f);
        if (bits << shift) >> shift != bits {
            return Err("a position table holds an integer too large".into());
        }

        value |= bits << shift;
        if byte & 0x80 == 0 {
            return Ok(value);
        }
    }
    Err("a position table holds an integer too long".into())
}

const fn zigzag(value: u64) -> i64 {
    (value >> 1).cast_signed() ^ -((value & 1).cast_signed())
}

/// The script offset a `SourcePosition` holds, or `None` for none.
pub(super) const fn script_offset(position: i64) -> Option<u64> {
    // Bit 0 marks an external position; bits 1 to 30 hold the offset + 1.
    if position & 1 != 0 {
        return None;
    }
    let stored = (position >> 1) & ((1 << 30) - 1);
    if stored == 0 {
        None
    } else {
        Some(stored.cast_unsigned() - 1)
    }
}

/// The source position of the bytecode at `offset`, or of the function's
/// entry at [`FUNCTION_ENTRY`]: that of the last entry at or before it in
/// the script, as V8's `SourcePosition` finds it, the function's entry
/// included.
pub(super) fn position_at(entries: &[Entry], offset: i64) -> Option<i64> {
    entries
        .iter()
        .filter(|entry| script_offset(entry.position).is_some())
        .take_while(|entry| entry.code_offset <= offset)
        .last()
        .map(|entry| entry.position)
}

/// The bytes of the position table of `bytecode`.
pub(super) fn table_bytes(reader: &Reader<'_>, bytecode: Object) -> Result<Vec<u8>, Problem> {
    let word = reader.field(bytecode, Field::BytecodeArraySourcePositionTable)?;
    let table = reader
        .object(word)?
        .ok_or("a bytecode array's position table is no object")?;
    byte_array(reader, table)
}

/// The bytes of the byte array `array`, trusted or not.
pub(super) fn byte_array(reader: &Reader<'_>, array: Object) -> Result<Vec<u8>, Problem> {
    let (length, values) = if reader.is(array, Constant::TrustedByteArrayType)? {
        (Field::TrustedByteArrayLength, Field::TrustedByteArrayValues)
    } else if reader.is(array, Constant::ByteArrayType)? {
        (Field::FixedArrayBaseLength, Field::ByteArrayValues)
    } else {
        return Err(format!("{:#x} is no byte array", array.0).into());
    };
    let length = usize::try_from(reader.smi_field(array, length)?)
        .map_err(|_| "a byte array has a negative length")?;
    if length > 1 << 24 {
        return Err("a byte array is implausibly long".into());
    }
    let mut bytes = vec![0; length];
    let address = reader.field_address(array, values)?;
    let unreadable = || format!("the byte array at {:#x} is unreadable", array.0).into();
    // One that fits a block is read with the blocks the reader keeps, as
    // its own object is.
    if reader.blocks.is_some_and(|blocks| length <= blocks.block()) {
        reader.bytes_into(address, &mut bytes)?;
        return Ok(bytes);
    }
    reader
        .stop
        .read(crate::VirtualAddress::new(address), &mut bytes)
        .then_some(bytes)
        .ok_or_else(unreadable)
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    fn varint(mut value: u64, out: &mut Vec<u8>) {
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

    fn zig(value: i64) -> u64 {
        ((value << 1) ^ (value >> 63)).cast_unsigned()
    }

    /// A source position for `offset`, not inlined.
    fn position(offset: i64) -> i64 {
        (offset + 1) << 1
    }

    /// Entries as (code offset, source position, statement), as each
    /// version writes them.
    fn encode_positions(entries: &[(i64, i64, bool)], encoding: Encoding) -> Vec<u8> {
        let mut bytes = Vec::new();
        let (mut code, mut source) = (FUNCTION_ENTRY, 0);
        for &(at, position, statement) in entries {
            let delta = at - code;
            match encoding {
                Encoding::SignedStatement => {
                    varint(zig(if statement { delta } else { -delta - 1 }), &mut bytes);
                }
                Encoding::FlagBits => {
                    varint(
                        (delta.cast_unsigned() << 2) | u64::from(statement),
                        &mut bytes,
                    );
                }
            }
            varint(zig(position - source), &mut bytes);
            (code, source) = (at, position);
        }
        bytes
    }

    /// The varint at `at` as the format defines it, read into 128 bits:
    /// at most ten groups, and `None` for one cut short, longer, or past
    /// 64 bits.
    fn reference_unsigned(bytes: &[u8], at: &mut usize) -> Option<u64> {
        let mut value = 0_u128;
        for group in 0..10 {
            let byte = *bytes.get(*at)?;
            *at += 1;
            value |= u128::from(byte & 0x7f) << (7 * group);
            if byte & 0x80 == 0 {
                return u64::try_from(value).ok();
            }
        }
        None
    }

    /// A table's entries as the format defines them, with every sum in
    /// 128 bits, or `None` where the bytes are no table or a value does
    /// not fit.
    fn reference_decode(bytes: &[u8], encoding: Encoding) -> Option<Vec<Entry>> {
        let mut entries = Vec::new();
        let mut at = 0;
        let (mut code_offset, mut position) = (i128::from(FUNCTION_ENTRY), 0_i128);
        while at < bytes.len() {
            let value = i128::from(reference_unsigned(bytes, &mut at)?);
            let delta = match encoding {
                Encoding::SignedStatement => {
                    let signed = if value % 2 == 0 {
                        value / 2
                    } else {
                        -(value + 1) / 2
                    };
                    if signed >= 0 { signed } else { -signed - 1 }
                }
                Encoding::FlagBits => {
                    let bits = u32::try_from(value).ok()?;
                    (i128::from(bits) - if bits >> 31 == 1 { 1 << 32 } else { 0 }) >> 2
                }
            };
            let value = i128::from(reference_unsigned(bytes, &mut at)?);
            let change = if value % 2 == 0 {
                value / 2
            } else {
                -(value + 1) / 2
            };
            code_offset += delta;
            position += change;
            entries.push(Entry {
                code_offset: i64::try_from(code_offset).ok()?,
                position: i64::try_from(position).ok()?,
            });
        }
        Some(entries)
    }

    fn encoding() -> impl Strategy<Value = Encoding> {
        prop_oneof![Just(Encoding::SignedStatement), Just(Encoding::FlagBits)]
    }

    /// A varint's group past the 64th bit is no value, not one with its
    /// high bits dropped.
    #[test]
    fn a_varint_past_64_bits_is_too_long() {
        let mut wide = vec![0x80; 9];
        wide.push(0x02);
        assert!(unsigned(&wide, &mut 0).is_err());

        // The table proptest shrank the case to.
        let mut table = vec![0x00];
        table.extend(wide);
        assert!(decode(&table, Encoding::SignedStatement).is_err());

        let mut widest = vec![0xff; 9];
        widest.push(0x01);
        assert_eq!(unsigned(&widest, &mut 0), Ok(u64::MAX));
    }

    proptest! {
        /// Entries whose offsets rise from the function's entry decode as
        /// they were written, in each version's encoding.
        #[test]
        fn written_tables_decode_to_their_entries(
            encoding in encoding(),
            written in proptest::collection::vec(
                (0_i64..1 << 20, -(1_i64 << 40)..1 << 40, any::<bool>()),
                0..24,
            ),
        ) {
            let mut code = FUNCTION_ENTRY;
            let written: Vec<_> = written
                .into_iter()
                .map(|(delta, position, statement)| {
                    code += delta;
                    (code, position, statement)
                })
                .collect();

            let decoded = decode(&encode_positions(&written, encoding), encoding);
            let expected: Vec<_> = written
                .iter()
                .map(|&(code_offset, position, _)| Entry { code_offset, position })
                .collect();
            prop_assert_eq!(decoded, Ok(expected));
        }

        /// Any bytes are a table exactly when the format reads them as
        /// one whose values fit, and then the same table.
        #[test]
        fn any_bytes_decode_as_the_format_reads_them(
            encoding in encoding(),
            varints in proptest::collection::vec(
                (
                    0..12_usize,
                    prop_oneof![Just(0x80_u8), Just(0xff), any::<u8>().prop_map(|byte| byte | 0x80)],
                    any::<u8>(),
                ),
                0..8,
            ),
        ) {
            // Runs of continuation bytes, each ended by any byte, reach
            // every length of varint.
            let bytes: Vec<u8> = varints
                .into_iter()
                .flat_map(|(groups, fill, last)| {
                    std::iter::repeat_n(fill, groups).chain([last])
                })
                .collect();
            prop_assert_eq!(decode(&bytes, encoding).ok(), reference_decode(&bytes, encoding));
        }
    }

    #[test]
    fn the_position_of_an_offset_is_the_last_entry_at_or_before_it() {
        let entries =
            [(FUNCTION_ENTRY, 4), (0, 10), (3, 25), (9, 7)].map(|(code_offset, offset)| Entry {
                code_offset,
                position: position(offset),
            });
        let at = |offset| position_at(&entries, offset).and_then(script_offset);
        assert_eq!(at(0), Some(10));
        assert_eq!(at(2), Some(10));
        assert_eq!(at(3), Some(25));
        assert_eq!(at(8), Some(25));
        assert_eq!(at(100), Some(7));
        assert_eq!(at(FUNCTION_ENTRY), Some(4), "the entry is kept");
        assert_eq!(at(FUNCTION_ENTRY - 1), None);
    }

    #[test]
    fn positions_hold_their_script_offset() {
        assert_eq!(script_offset(0), None, "no position");
        assert_eq!(script_offset(position(0)), Some(0));
        assert_eq!(script_offset(1 | (5 << 1)), None, "an external line");
        assert_eq!(script_offset(position(42) | (3 << 31)), Some(42), "inlined");
    }
}
