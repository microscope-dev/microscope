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
        value |= u64::from(byte & 0x7f) << shift;
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

    /// Entries as (code offset, script offset, statement), as each
    /// version writes them.
    fn encode(entries: &[(i64, i64, bool)], encoding: Encoding) -> Vec<u8> {
        let mut bytes = Vec::new();
        let (mut code, mut source) = (FUNCTION_ENTRY, 0);
        for &(at, offset, statement) in entries {
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
            varint(zig(position(offset) - source), &mut bytes);
            (code, source) = (at, position(offset));
        }
        bytes
    }

    /// Each version's table decodes to its own offsets, and read as the
    /// other version's, to others: the encoding must be the version's.
    #[test]
    fn each_encoding_decodes_its_own_tables_and_not_the_others() {
        let written = [
            (FUNCTION_ENTRY, 4, true),
            (0, 10, true),
            (3, 25, false),
            (9, 7, true),
            (14, 40, false),
        ];
        for (encoding, other) in [
            (Encoding::SignedStatement, Encoding::FlagBits),
            (Encoding::FlagBits, Encoding::SignedStatement),
        ] {
            let bytes = encode(&written, encoding);
            let read: Vec<_> = decode(&bytes, encoding)
                .expect("a table")
                .iter()
                .map(|entry| (entry.code_offset, script_offset(entry.position)))
                .collect();
            let expected: Vec<_> = written
                .iter()
                .map(|&(at, offset, _)| (at, Some(offset.cast_unsigned())))
                .collect();
            assert_eq!(read, expected, "{encoding:?}");

            let misread: Vec<_> = decode(&bytes, other)
                .map(|entries| entries.iter().map(|entry| entry.code_offset).collect())
                .unwrap_or_default();
            assert_ne!(
                misread,
                written.iter().map(|entry| entry.0).collect::<Vec<_>>(),
                "{encoding:?} read as {other:?}"
            );
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
    fn a_table_cut_inside_an_entry_is_no_table() {
        let mut bytes = encode(&[(0, 10, true), (300, 20, true)], Encoding::FlagBits);
        bytes.pop();
        assert!(decode(&bytes, Encoding::FlagBits).is_err());
    }

    #[test]
    fn positions_hold_their_script_offset() {
        assert_eq!(script_offset(0), None, "no position");
        assert_eq!(script_offset(position(0)), Some(0));
        assert_eq!(script_offset(1 | (5 << 1)), None, "an external line");
        assert_eq!(script_offset(position(42) | (3 << 31)), Some(42), "inlined");
    }
}
