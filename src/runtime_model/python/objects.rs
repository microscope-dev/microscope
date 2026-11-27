//! Budgeted, block-cached memory reader for `CPython` structs and primitive objects.
//!
//! Handles PEP 393 compact and legacy `str` decoding as well as `Py_STACKREF` tagged pointers
//! introduced in `CPython` 3.14.

use std::sync::Arc;

use crate::VirtualAddress;
use crate::image::python::{Constant, Field, PythonLayout};
use crate::runtime_model::{RuntimeStop, StopMemory};

pub(super) const MAX_STRING: u64 = 4096;

pub(super) const MAX_ITEMS: u64 = 1 << 16;

const MAX_C_STRING: u64 = 256;

const HEAD: usize = 64;

pub(super) const BLOCK: usize = StopMemory::BLOCK;

pub(super) type Problem = Arc<str>;

pub(super) trait Charge {
    fn charge(&self, bytes: usize) -> Result<(), Problem>;
}

#[derive(Clone, Copy)]
pub(super) struct Reader<'a> {
    pub(super) stop: &'a dyn RuntimeStop,
    pub(super) layout: &'a PythonLayout,
    pub(super) charge: Option<&'a dyn Charge>,

    pub(super) blocks: Option<&'a StopMemory>,
}

impl<'a> Reader<'a> {
    pub(super) fn bytes(&self, address: VirtualAddress, length: u64) -> Result<Vec<u8>, Problem> {
        let mut bytes = vec![0; usize::try_from(length).map_err(|_| unreadable(address))?];
        self.read(address, &mut bytes)?;
        Ok(bytes)
    }

    pub(super) fn head(&self, address: VirtualAddress) -> Result<Head<'_, 'a>, Problem> {
        let mut bytes = vec![0; HEAD];
        if !self.read_some(address, &mut bytes)? {
            bytes.clear();
        }

        Ok(Head {
            reader: self,
            address,
            bytes,
        })
    }

    pub(super) fn word(&self, address: VirtualAddress) -> Result<u64, Problem> {
        let mut bytes = [0; 8];
        self.read(address, &mut bytes)?;
        Ok(u64::from_le_bytes(bytes))
    }

    fn read(&self, address: VirtualAddress, bytes: &mut [u8]) -> Result<(), Problem> {
        self.read_some(address, bytes)?
            .then_some(())
            .ok_or_else(|| unreadable(address))
    }

    fn read_some(&self, address: VirtualAddress, bytes: &mut [u8]) -> Result<bool, Problem> {
        let length = bytes.len() as u64;
        if let Some(blocks) = self.blocks
            && length <= BLOCK as u64
            && address.get().checked_add(length).is_some()
        {
            let mut done = 0;

            while let Some(rest) = bytes.get_mut(done..)
                && !rest.is_empty()
            {
                match self.copy_block(blocks, address.get() + done as u64, rest)? {
                    Some(copied) => {
                        debug_assert!(
                            copied > 0,
                            "the block cache copied nothing at virtual {:#x}",
                            address.get() + done as u64,
                        );
                        debug_assert!(
                            copied <= rest.len(),
                            "the block cache copied {copied} bytes into {} at virtual {address:#x}",
                            rest.len(),
                        );
                        done += copied;
                    }
                    None => break,
                }
            }

            if done == bytes.len() {
                return Ok(true);
            }
        }

        if let Some(charge) = self.charge {
            charge.charge(bytes.len())?;
        }

        Ok(self.stop.read(address, bytes))
    }

    fn copy_block(
        &self,
        blocks: &StopMemory,
        at: u64,
        into: &mut [u8],
    ) -> Result<Option<usize>, Problem> {
        blocks.copy(at, into, |start, bytes| {
            if let Some(charge) = self.charge {
                charge.charge(bytes.len())?;
            }

            Ok(self.stop.read(start, bytes))
        })
    }

    pub(super) fn c_string(&self, address: VirtualAddress) -> Result<Arc<str>, Problem> {
        const CHUNK: u64 = 16;
        let mut text = Vec::new();
        let mut at = address;

        while (text.len() as u64) < MAX_C_STRING {
            let length = CHUNK - at.get() % CHUNK;
            let chunk = self.bytes(at, length)?;
            if let Some((head, _)) = (chunk.iter().position(|byte| *byte == 0))
                .and_then(|end| chunk.split_at_checked(end))
            {
                text.extend_from_slice(head);
                return String::from_utf8(text)
                    .map(Arc::from)
                    .map_err(|_| malformed(address, "C string"));
            }

            text.extend_from_slice(&chunk);
            at = at.wrapping_add(length);
            debug_assert!(
                at.get().is_multiple_of(CHUNK),
                "the C string at virtual {address:#x} reads on from unaligned {at:#x}",
            );
        }

        Err(format!("the C string at {address:#x} is longer than {MAX_C_STRING} bytes").into())
    }

    pub(super) fn offset(&self, field: Field) -> Result<u64, Problem> {
        self.layout
            .field(field)
            .ok_or_else(|| format!("CPython's layout has no {}", field.name()).into())
    }

    pub(super) fn record(
        &self,
        address: VirtualAddress,
        size: Field,
    ) -> Result<Record<'a>, Problem> {
        Ok(Record {
            address,
            bytes: self.bytes(address, self.offset(size)?)?,
            layout: self.layout,
        })
    }

    pub(super) fn field_bytes(
        &self,
        object: VirtualAddress,
        field: Field,
        length: u64,
    ) -> Result<Vec<u8>, Problem> {
        self.bytes(object.wrapping_add(self.offset(field)?), length)
    }

    pub(super) fn field_word(&self, object: VirtualAddress, field: Field) -> Result<u64, Problem> {
        self.word(object.wrapping_add(self.offset(field)?))
    }

    pub(super) fn string(&self, object: VirtualAddress) -> Result<Arc<str>, Problem> {
        let (points, length) = self.head(object)?.code_points(MAX_STRING)?;
        if length > MAX_STRING {
            return Err(format!("the str at {object:#x} is {length} characters long").into());
        }

        points
            .into_iter()
            .map(char::from_u32)
            .collect::<Option<String>>()
            .map(Arc::from)
            .ok_or_else(|| malformed(object, "str"))
    }

    pub(super) fn bytes_object(&self, object: VirtualAddress) -> Result<Vec<u8>, Problem> {
        let length = self.field_word(object, Field::BytesSize)?;
        if length > MAX_ITEMS {
            return Err(format!("the bytes at {object:#x} are {length} long").into());
        }

        self.field_bytes(object, Field::BytesData, length)
    }

    pub(super) fn tuple(&self, object: VirtualAddress) -> Result<Vec<VirtualAddress>, Problem> {
        let length = self.field_word(object, Field::TupleSize)?;
        if length > MAX_ITEMS {
            return Err(format!("the tuple at {object:#x} holds {length} items").into());
        }

        let items = self.field_bytes(object, Field::TupleItems, length * 8)?;
        let items: Vec<VirtualAddress> = items
            .as_chunks::<8>()
            .0
            .iter()
            .map(|item| VirtualAddress::new(u64::from_le_bytes(*item)))
            .collect();
        debug_assert_eq!(
            items.len() as u64,
            length,
            "the items read of the tuple at virtual {object:#x} are not its {length}",
        );
        Ok(items)
    }
}

pub(super) struct Head<'r, 'a> {
    reader: &'r Reader<'a>,
    address: VirtualAddress,
    bytes: Vec<u8>,
}

impl Head<'_, '_> {
    pub(super) const fn address(&self) -> VirtualAddress {
        self.address
    }

    pub(super) fn bytes(&self, at: VirtualAddress, length: u64) -> Result<Vec<u8>, Problem> {
        let held = at.get().checked_sub(self.address.get()).and_then(|start| {
            let end = start.checked_add(length)?;
            self.bytes
                .get(usize::try_from(start).ok()?..usize::try_from(end).ok()?)
        });
        held.map_or_else(|| self.reader.bytes(at, length), |bytes| Ok(bytes.to_vec()))
    }

    pub(super) fn field_bytes(&self, field: Field, length: u64) -> Result<Vec<u8>, Problem> {
        self.bytes(
            self.address.wrapping_add(self.reader.offset(field)?),
            length,
        )
    }

    pub(super) fn field_word(&self, field: Field) -> Result<u64, Problem> {
        let bytes = self.field_bytes(field, 8)?;
        read_le(&bytes, 0, 8).ok_or_else(|| unreadable(self.address))
    }

    pub(super) fn field_byte(&self, field: Field) -> Result<u8, Problem> {
        let bytes = self.field_bytes(field, 1)?;
        bytes
            .first()
            .copied()
            .ok_or_else(|| unreadable(self.address))
    }

    pub(super) fn code_points(&self, limit: u64) -> Result<(Vec<u32>, u64), Problem> {
        let (object, reader) = (self.address, self.reader);
        let compact = reader.offset(Field::UnicodeCompactSize)?;
        let header = self.bytes(object, compact)?;
        let at = |field| -> Result<u64, Problem> {
            let offset = reader.offset(field)?;
            read_le(&header, offset, 8).ok_or_else(|| malformed(object, "str"))
        };

        let length = at(Field::UnicodeLength)?;
        if length.cast_signed() < 0 {
            return Err(malformed(object, "str"));
        }

        let state = {
            let offset = reader.offset(Field::UnicodeState)?;
            read_le(&header, offset, 4).ok_or_else(|| malformed(object, "str"))?
        };

        let bit = |shift: Constant| (state >> reader.layout.constant(shift)) & 1 != 0;
        let kind = (state >> reader.layout.constant(Constant::UnicodeKindShift)) & 7;
        let data = match (
            bit(Constant::UnicodeCompactShift),
            bit(Constant::UnicodeAsciiShift),
        ) {
            (true, true) => object.wrapping_add(reader.offset(Field::UnicodeAsciiSize)?),
            (true, false) => object.wrapping_add(compact),
            (false, _) => VirtualAddress::new(self.field_word(Field::UnicodeData)?),
        };

        let width = match kind {
            1 | 2 | 4 => kind,
            _ => return Err(malformed(object, "str")),
        };

        let shown = length.min(limit);
        let bytes = self.bytes(data, shown * width)?;

        let points: Vec<u32> = match width {
            1 => bytes.iter().map(|&byte| u32::from(byte)).collect(),
            2 => bytes
                .as_chunks::<2>()
                .0
                .iter()
                .map(|unit| u32::from(u16::from_le_bytes(*unit)))
                .collect(),
            _ => bytes
                .as_chunks::<4>()
                .0
                .iter()
                .map(|unit| u32::from_le_bytes(*unit))
                .collect(),
        };

        debug_assert_eq!(
            points.len() as u64,
            shown,
            "the code points read of the str at virtual {object:#x} are not the {shown} shown",
        );

        if points.iter().any(|point| *point > u32::from(char::MAX)) {
            return Err(malformed(object, "str"));
        }

        Ok((points, length))
    }
}

pub(super) struct Record<'a> {
    pub(super) address: VirtualAddress,
    bytes: Vec<u8>,
    layout: &'a PythonLayout,
}

impl Record<'_> {
    pub(super) fn number(&self, field: Field, width: u64) -> Result<u64, Problem> {
        let offset = self.layout.field(field).ok_or_else(|| -> Problem {
            format!("CPython's layout has no {}", field.name()).into()
        })?;
        read_le(&self.bytes, offset, width)
            .ok_or_else(|| format!("{} lies past its structure", field.name()).into())
    }

    pub(super) fn word(&self, field: Field) -> Result<u64, Problem> {
        self.number(field, 8)
    }

    pub(super) fn pointer(&self, field: Field) -> Result<Option<VirtualAddress>, Problem> {
        Ok(Some(self.word(field)?)
            .filter(|word| *word != 0)
            .map(VirtualAddress::new))
    }

    pub(super) fn int(&self, field: Field) -> Result<i32, Problem> {
        let number = self.number(field, 4)?;
        u32::try_from(number)
            .map(u32::cast_signed)
            .map_err(|_| format!("{} is no int", field.name()).into())
    }
}

/// Untags a `CPython` 3.14+ `_PyStackRef` word, clearing the low deferred-refcount bit and
/// rejecting tagged small integers or invalid tag bits.
pub(super) fn stack_reference(bits: u64) -> Option<VirtualAddress> {
    match bits & 3 {
        0 | 1 => Some(bits & !1)
            .filter(|bits| *bits != 0)
            .map(VirtualAddress::new),
        _ => None,
    }
}

pub(super) fn read_le(bytes: &[u8], offset: u64, width: u64) -> Option<u64> {
    debug_assert!(width <= 8, "a {width}-byte number is wider than a word");
    let start = usize::try_from(offset).ok()?;
    let end = start.checked_add(usize::try_from(width).ok()?)?;
    let bytes = bytes.get(start..end)?;
    let mut word = [0; 8];
    word.get_mut(..bytes.len())?.copy_from_slice(bytes);
    Some(u64::from_le_bytes(word))
}

pub(super) fn unreadable(address: VirtualAddress) -> Problem {
    format!("the memory at {address:#x} is unreadable").into()
}

pub(super) fn malformed(address: VirtualAddress, what: &str) -> Problem {
    format!("the {what} at {address:#x} is malformed").into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_stack_reference_is_a_pointer_without_its_counting_bit_or_nothing() {
        assert_eq!(stack_reference(0x1000), Some(VirtualAddress::new(0x1000)));
        assert_eq!(stack_reference(0x1001), Some(VirtualAddress::new(0x1000)));
        assert_eq!(stack_reference(1), None, "NULL");
        assert_eq!(stack_reference((5 << 2) | 3), None, "a tagged int");
        assert_eq!(stack_reference(0x1002), None, "an invalid tag");
    }
}
