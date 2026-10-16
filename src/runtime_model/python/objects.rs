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

/// The longest C string read, in bytes, as a type's name.
const MAX_C_STRING: u64 = 256;

/// How many bytes of an object are read at once, before any of its
/// fields: enough for the header and fixed fields of each built-in object
/// the debugger presents, and the text of a short string.
const HEAD: usize = 64;

/// A problem reading the process, as one line.
pub(super) type Problem = Arc<str>;

/// What pays for each read a reader makes.
pub(super) trait Charge {
    /// Pays for one read of `bytes`, or says why it cannot be paid for.
    fn charge(&self, bytes: usize) -> Result<(), Problem>;
}

/// The memory of a stopped process, read by a layout, and what pays for
/// the reads, if anything does.
#[derive(Clone, Copy)]
pub(super) struct Reader<'a> {
    pub(super) stop: &'a dyn RuntimeStop,
    pub(super) layout: &'a PythonLayout,
    pub(super) charge: Option<&'a dyn Charge>,
}

impl<'a> Reader<'a> {
    pub(super) fn bytes(&self, address: VirtualAddress, length: u64) -> Result<Vec<u8>, Problem> {
        let mut bytes = vec![0; usize::try_from(length).map_err(|_| unreadable(address))?];
        self.read(address, &mut bytes)?;
        Ok(bytes)
    }

    /// The first bytes of the object at `address`, read at once, or none
    /// where they cannot all be read, as near the end of a mapping.
    pub(super) fn head(&self, address: VirtualAddress) -> Result<Head<'_, 'a>, Problem> {
        let mut bytes = vec![0; HEAD];
        if let Some(charge) = self.charge {
            charge.charge(HEAD)?;
        }

        if !self.stop.read(address, &mut bytes) {
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
        if let Some(charge) = self.charge {
            charge.charge(bytes.len())?;
        }

        self.stop
            .read(address, bytes)
            .then_some(())
            .ok_or_else(|| unreadable(address))
    }

    /// The NUL-terminated string at `address`, of at most
    /// [`MAX_C_STRING`] bytes, read a few aligned bytes at a time so that
    /// no read reaches past a page its string ends in.
    pub(super) fn c_string(&self, address: VirtualAddress) -> Result<Arc<str>, Problem> {
        const CHUNK: u64 = 16;
        let mut text = Vec::new();
        let mut at = address;
        while (text.len() as u64) < MAX_C_STRING {
            let length = CHUNK - at.get() % CHUNK;
            let chunk = self.bytes(at, length)?;
            if let Some(end) = chunk.iter().position(|byte| *byte == 0) {
                text.extend_from_slice(&chunk[..end]);
                return String::from_utf8(text)
                    .map(Arc::from)
                    .map_err(|_| malformed(address, "C string"));
            }

            text.extend_from_slice(&chunk);
            at = at.wrapping_add(length);
        }

        Err(format!("the C string at {address:#x} is longer than {MAX_C_STRING} bytes").into())
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

    /// The `str` at `object`, of at most [`MAX_STRING`] characters, as a
    /// name or a path is: one that holds a lone surrogate is malformed.
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

/// The first bytes of an object, read at once, so a small object costs
/// one read. What lies past them, or in an object whose head could not be
/// read whole, is read where it is.
pub(super) struct Head<'r, 'a> {
    reader: &'r Reader<'a>,
    address: VirtualAddress,
    bytes: Vec<u8>,
}

impl Head<'_, '_> {
    /// The address of the object the head is of.
    pub(super) const fn address(&self) -> VirtualAddress {
        self.address
    }

    /// The `length` bytes at `at`, from the head where it holds them all.
    pub(super) fn bytes(&self, at: VirtualAddress, length: u64) -> Result<Vec<u8>, Problem> {
        let held = at.get().checked_sub(self.address.get()).and_then(|start| {
            let end = start.checked_add(length)?;
            self.bytes
                .get(usize::try_from(start).ok()?..usize::try_from(end).ok()?)
        });
        held.map_or_else(|| self.reader.bytes(at, length), |bytes| Ok(bytes.to_vec()))
    }

    /// The `length` bytes at the object's `field`.
    pub(super) fn field_bytes(&self, field: Field, length: u64) -> Result<Vec<u8>, Problem> {
        self.bytes(
            self.address.wrapping_add(self.reader.offset(field)?),
            length,
        )
    }

    /// The word at the object's `field`.
    pub(super) fn field_word(&self, field: Field) -> Result<u64, Problem> {
        let bytes = self.field_bytes(field, 8)?;
        read_le(&bytes, 0, 8).ok_or_else(|| unreadable(self.address))
    }

    /// The first byte at the object's `field`.
    pub(super) fn field_byte(&self, field: Field) -> Result<u8, Problem> {
        let bytes = self.field_bytes(field, 1)?;
        bytes
            .first()
            .copied()
            .ok_or_else(|| unreadable(self.address))
    }

    /// The first `limit` code points of the `str` the head is of, as
    /// CPython keeps them, and how many it holds.
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

        // A 4-byte kind holds no code point past Unicode's last.
        if points.iter().any(|point| *point > u32::from(char::MAX)) {
            return Err(malformed(object, "str"));
        }

        Ok((points, length))
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
