//! Reading CPython's structures and the objects they point to, with the
//! offsets the module's layout gives. Every read is bounded, and memory
//! that cannot be read or is not what the layout says is an error that
//! names it, never a guess.

use std::sync::Arc;

use crate::VirtualAddress;
use crate::image::python::{Constant, Field, PythonLayout};
use crate::runtime_model::RuntimeStop;

/// The longest string read, in characters: names and paths are far
/// shorter.
pub(super) const MAX_STRING: u64 = 4096;

/// The largest sequence read, in items or bytes.
pub(super) const MAX_ITEMS: u64 = 1 << 16;

/// A problem reading the process, as one line.
pub(super) type Problem = Arc<str>;

/// The memory of a stopped process, read by a layout.
#[derive(Clone, Copy)]
pub(super) struct Reader<'a> {
    pub(super) stop: &'a dyn RuntimeStop,
    pub(super) layout: &'a PythonLayout,
}

impl<'a> Reader<'a> {
    pub(super) fn bytes(&self, address: VirtualAddress, length: u64) -> Result<Vec<u8>, Problem> {
        let mut bytes = vec![0; usize::try_from(length).map_err(|_| unreadable(address))?];
        self.stop
            .read(address, &mut bytes)
            .then_some(bytes)
            .ok_or_else(|| unreadable(address))
    }

    pub(super) fn word(&self, address: VirtualAddress) -> Result<u64, Problem> {
        let mut bytes = [0; 8];
        self.stop
            .read(address, &mut bytes)
            .then(|| u64::from_le_bytes(bytes))
            .ok_or_else(|| unreadable(address))
    }

    /// The offset of `field`, which the layout must have.
    pub(super) fn offset(&self, field: Field) -> Result<u64, Problem> {
        self.layout
            .field(field)
            .ok_or_else(|| format!("CPython's layout has no {}", field.name()).into())
    }

    /// The structure at `address` whose size is the layout's `size`, read
    /// whole.
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

    /// The `length` bytes at `field` of the object at `object`.
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

    /// The `str` at `object`, of at most [`MAX_STRING`] characters.
    pub(super) fn string(&self, object: VirtualAddress) -> Result<Arc<str>, Problem> {
        let compact = self.offset(Field::UnicodeCompactSize)?;
        let header = self.bytes(object, compact)?;
        let at = |field| -> Result<u64, Problem> {
            let offset = self.offset(field)?;
            read_le(&header, offset, 8).ok_or_else(|| malformed(object, "str"))
        };

        let length = at(Field::UnicodeLength)?;
        if length > MAX_STRING {
            return Err(format!("the str at {object:#x} is {length} characters long").into());
        }

        let state = {
            let offset = self.offset(Field::UnicodeState)?;
            read_le(&header, offset, 4).ok_or_else(|| malformed(object, "str"))?
        };
        let bit = |shift: Constant| (state >> self.layout.constant(shift)) & 1 != 0;
        let kind = (state >> self.layout.constant(Constant::UnicodeKindShift)) & 7;
        let data = match (
            bit(Constant::UnicodeCompactShift),
            bit(Constant::UnicodeAsciiShift),
        ) {
            (true, true) => object.wrapping_add(self.offset(Field::UnicodeAsciiSize)?),
            (true, false) => object.wrapping_add(compact),
            (false, _) => VirtualAddress::new(self.field_word(object, Field::UnicodeData)?),
        };

        let width = match kind {
            1 | 2 | 4 => kind,
            _ => return Err(malformed(object, "str")),
        };

        let bytes = self.bytes(data, length * width)?;
        let text: Option<String> = match width {
            1 => Some(bytes.iter().map(|&byte| char::from(byte)).collect()),
            2 => char::decode_utf16(
                bytes
                    .as_chunks::<2>()
                    .0
                    .iter()
                    .map(|unit| u16::from_le_bytes(*unit)),
            )
            .collect::<Result<_, _>>()
            .ok(),
            _ => bytes
                .as_chunks::<4>()
                .0
                .iter()
                .map(|unit| char::from_u32(u32::from_le_bytes(*unit)))
                .collect(),
        };

        text.map(Arc::from).ok_or_else(|| malformed(object, "str"))
    }

    /// The contents of the `bytes` at `object`, of at most [`MAX_ITEMS`].
    pub(super) fn bytes_object(&self, object: VirtualAddress) -> Result<Vec<u8>, Problem> {
        let length = self.field_word(object, Field::BytesSize)?;
        if length > MAX_ITEMS {
            return Err(format!("the bytes at {object:#x} are {length} long").into());
        }

        self.field_bytes(object, Field::BytesData, length)
    }

    /// The items of the `tuple` at `object`, of at most [`MAX_ITEMS`].
    pub(super) fn tuple(&self, object: VirtualAddress) -> Result<Vec<VirtualAddress>, Problem> {
        let length = self.field_word(object, Field::TupleSize)?;
        if length > MAX_ITEMS {
            return Err(format!("the tuple at {object:#x} holds {length} items").into());
        }

        let items = self.field_bytes(object, Field::TupleItems, length * 8)?;
        Ok(items
            .as_chunks::<8>()
            .0
            .iter()
            .map(|item| VirtualAddress::new(u64::from_le_bytes(*item)))
            .collect())
    }
}

/// A structure read whole.
pub(super) struct Record<'a> {
    pub(super) address: VirtualAddress,
    bytes: Vec<u8>,
    layout: &'a PythonLayout,
}

impl Record<'_> {
    /// The `width` bytes at `field`, as an unsigned number.
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

    /// A C `int` at `field`.
    pub(super) fn int(&self, field: Field) -> Result<i32, Problem> {
        let number = self.number(field, 4)?;
        u32::try_from(number)
            .map(u32::cast_signed)
            .map_err(|_| format!("{} is no int", field.name()).into())
    }
}

/// What a stack reference at a frame's field holds, from 3.14: a pointer
/// with a reference-counting bit, or a tagged integer.
pub(super) fn stack_reference(bits: u64) -> Option<VirtualAddress> {
    match bits & 3 {
        0 | 1 => Some(bits & !1)
            .filter(|bits| *bits != 0)
            .map(VirtualAddress::new),
        _ => None,
    }
}

/// The `width`-byte little-endian number at `offset` of `bytes`.
pub(super) fn read_le(bytes: &[u8], offset: u64, width: u64) -> Option<u64> {
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
