//! Reading V8's heap at a stop, with the offsets the image's layout gives.
//!
//! Every value is a tagged word: a Smi, whose low bit is clear and whose
//! integer is its upper half, or a pointer to a heap object, whose low bit
//! is set. An object is what its map's instance type says it is, and the
//! reader follows a pointer only as the type it was proven to be. Memory
//! that cannot be read, or holds the wrong type, is a problem that names
//! it, never a guess.

use std::sync::Arc;

use crate::VirtualAddress;
use crate::image::v8::{Constant, Field, V8Layout};
use crate::runtime_model::{RuntimeStop, StopMemory};

/// A problem reading the process, as one line.
pub(super) type Problem = Arc<str>;

/// The most characters of a string read: names and paths are far shorter.
pub(super) const MAX_STRING: u64 = 4096;

/// How deep a string's representation may nest, as a cons string of cons
/// strings does.
const MAX_STRING_DEPTH: usize = 32;

/// The memory of a stopped process, read by a layout.
#[derive(Clone, Copy)]
pub(super) struct Reader<'a> {
    pub(super) stop: &'a dyn RuntimeStop,
    pub(super) layout: &'a V8Layout,
}

/// A tagged word that points to a heap object: its address with the tag.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(super) struct Object(pub(super) u64);

impl Reader<'_> {
    pub(super) fn bytes_into(&self, address: u64, bytes: &mut [u8]) -> Result<(), Problem> {
        let read = match self.stop.memory() {
            Some(memory) if bytes.len() <= StopMemory::BLOCK => {
                copy_through(memory, self.stop, address, bytes)
            }
            _ => self.stop.read(VirtualAddress::new(address), bytes),
        };
        read.then_some(())
            .ok_or_else(|| format!("the memory at {address:#x} is unreadable").into())
    }

    pub(super) fn word(&self, address: u64) -> Result<u64, Problem> {
        let mut bytes = [0; 8];
        self.bytes_into(address, &mut bytes)?;
        Ok(u64::from_le_bytes(bytes))
    }

    pub(super) fn constant(&self, constant: Constant) -> Result<i64, Problem> {
        self.layout
            .constant(constant)
            .ok_or_else(|| format!("the layout has no {}", constant.name()).into())
    }

    pub(super) fn offset(&self, field: Field) -> Result<u64, Problem> {
        self.layout
            .field(field)
            .ok_or_else(|| format!("the layout has no {}", field.name()).into())
    }

    /// The integer a Smi holds, or `None` for a word that is no Smi.
    pub(super) fn smi(&self, word: u64) -> Result<Option<i64>, Problem> {
        let mask = self.constant(Constant::SmiTagMask)?.cast_unsigned();
        let tag = self.constant(Constant::SmiTag)?.cast_unsigned();
        let shift = self.constant(Constant::SmiValueShift)?;
        let shift = u32::try_from(shift).map_err(|_| "the Smi shift is out of range")?;
        Ok((word & mask == tag).then(|| word.cast_signed().checked_shr(shift).unwrap_or(0)))
    }

    /// The object a word points to, or `None` for a word that is no pointer.
    pub(super) fn object(&self, word: u64) -> Result<Option<Object>, Problem> {
        let mask = self.constant(Constant::HeapObjectTagMask)?.cast_unsigned();
        let tag = self.constant(Constant::HeapObjectTag)?.cast_unsigned();
        Ok((word & mask == tag && word > tag).then_some(Object(word)))
    }

    /// Where `field` of `object` is.
    pub(super) fn field_address(&self, object: Object, field: Field) -> Result<u64, Problem> {
        let tag = self.constant(Constant::HeapObjectTag)?.cast_unsigned();
        Ok(object.0 - tag + self.offset(field)?)
    }

    /// The tagged word `field` of `object` holds.
    pub(super) fn field(&self, object: Object, field: Field) -> Result<u64, Problem> {
        self.word(self.field_address(object, field)?)
    }

    /// The Smi `field` of `object` holds.
    pub(super) fn smi_field(&self, object: Object, field: Field) -> Result<i64, Problem> {
        let word = self.field(object, field)?;
        self.smi(word)?
            .ok_or_else(|| format!("{} of {:#x} is no Smi", field.name(), object.0).into())
    }

    /// The 32-bit integer `field` of `object` holds, untagged.
    pub(super) fn int32_field(&self, object: Object, field: Field) -> Result<i32, Problem> {
        let mut bytes = [0; 4];
        self.bytes_into(self.field_address(object, field)?, &mut bytes)?;
        Ok(i32::from_le_bytes(bytes))
    }

    /// The object `field` of `object` points to, proven of type `ty`.
    pub(super) fn typed_field(
        &self,
        object: Object,
        field: Field,
        ty: Constant,
    ) -> Result<Object, Problem> {
        let word = self.field(object, field)?;
        self.object(word)?
            .filter(|target| self.instance_type(*target).ok() == self.constant(ty).ok())
            .ok_or_else(|| {
                format!(
                    "{} of {:#x} is no {}",
                    field.name(),
                    object.0,
                    ty.name().trim_end_matches("_TYPE")
                )
                .into()
            })
    }

    /// The instance type of `object`, from its map.
    pub(super) fn instance_type(&self, object: Object) -> Result<i64, Problem> {
        let map = self.field(object, Field::HeapObjectMap)?;
        let map = self
            .object(map)?
            .ok_or_else(|| format!("the map of {:#x} is no object", object.0))?;
        let mut bytes = [0; 2];
        self.bytes_into(self.field_address(map, Field::MapInstanceType)?, &mut bytes)?;
        Ok(i64::from(u16::from_le_bytes(bytes)))
    }

    /// Whether `object` is of type `ty`.
    pub(super) fn is(&self, object: Object, ty: Constant) -> Result<bool, Problem> {
        Ok(self.instance_type(object)? == self.constant(ty)?)
    }

    /// The text of the string `object`, of at most [`MAX_STRING`]
    /// characters, or the problem that it is no string or cannot be read.
    pub(super) fn string(&self, object: Object) -> Result<String, Problem> {
        let mut text = Vec::new();
        self.string_into(object, 0, MAX_STRING, 0, &mut text)?;
        Ok(String::from_utf16_lossy(&text))
    }

    /// Appends to `text` the UTF-16 units `start` to `start + limit` of the
    /// string `object`, whose representation is `depth` deep.
    pub(super) fn string_into(
        &self,
        object: Object,
        start: u64,
        limit: u64,
        depth: usize,
        text: &mut Vec<u16>,
    ) -> Result<(), Problem> {
        if depth > MAX_STRING_DEPTH {
            return Err(format!("the string {:#x} nests too deeply", object.0).into());
        }
        let ty = self.instance_type(object)?;
        if ty >= self.constant(Constant::FirstNonstringType)? {
            return Err(format!("{:#x} is no string", object.0).into());
        }
        let length = u64::try_from(self.int32_field(object, Field::StringLength)?)
            .map_err(|_| format!("the string {:#x} has a negative length", object.0))?;
        let end = length.min(start.saturating_add(limit));
        if start >= end {
            return Ok(());
        }

        let representation = ty & self.constant(Constant::StringRepresentationMask)?;
        let one_byte = ty & self.constant(Constant::StringEncodingMask)?
            == self.constant(Constant::OneByteStringTag)?;
        let tagged = |field| -> Result<Object, Problem> {
            let word = self.field(object, field)?;
            self.object(word)?.ok_or_else(|| {
                format!("{} of {:#x} is no object", Field::name(field), object.0).into()
            })
        };

        if representation == self.constant(Constant::SeqStringTag)? {
            let chars = if one_byte {
                Field::SeqOneByteStringChars
            } else {
                Field::SeqTwoByteStringChars
            };
            self.units(
                self.field_address(object, chars)?,
                start,
                end,
                one_byte,
                text,
            )
        } else if representation == self.constant(Constant::ExternalStringTag)? {
            let data = self.word(self.field_address(object, Field::ExternalStringResourceData)?)?;
            if data == 0 {
                return Err(format!("the external string {:#x} keeps no data", object.0).into());
            }
            self.units(data, start, end, one_byte, text)
        } else if representation == self.constant(Constant::ConsStringTag)? {
            let first = tagged(Field::ConsStringFirst)?;
            let first_length = u64::try_from(self.int32_field(first, Field::StringLength)?)
                .map_err(|_| "a string has a negative length")?;
            if start < first_length {
                self.string_into(first, start, end - start, depth + 1, text)?;
            }
            let second = tagged(Field::ConsStringSecond)?;
            let from = start.saturating_sub(first_length);
            let wanted = end - start - (first_length.min(end).saturating_sub(start));
            self.string_into(second, from, wanted, depth + 1, text)
        } else if representation == self.constant(Constant::SlicedStringTag)? {
            let offset = u64::try_from(self.smi_field(object, Field::SlicedStringOffset)?)
                .map_err(|_| "a sliced string has a negative offset")?;
            let parent = tagged(Field::SlicedStringParent)?;
            self.string_into(parent, offset + start, end - start, depth + 1, text)
        } else if representation == self.constant(Constant::ThinStringTag)? {
            let actual = tagged(Field::ThinStringActual)?;
            self.string_into(actual, start, end - start, depth + 1, text)
        } else {
            Err(format!("the string {:#x} has an unknown representation", object.0).into())
        }
    }

    /// Appends the units `start..end` of characters at `address`.
    fn units(
        &self,
        address: u64,
        start: u64,
        end: u64,
        one_byte: bool,
        text: &mut Vec<u16>,
    ) -> Result<(), Problem> {
        let width = if one_byte { 1 } else { 2 };
        let length = usize::try_from((end - start) * width).map_err(|_| "a string is too long")?;
        let mut bytes = vec![0; length];
        self.stop
            .read(VirtualAddress::new(address + start * width), &mut bytes)
            .then_some(())
            .ok_or_else(|| format!("the characters at {address:#x} are unreadable"))?;
        if one_byte {
            text.extend(bytes.iter().map(|byte| u16::from(*byte)));
        } else {
            text.extend(
                bytes
                    .as_chunks::<2>()
                    .0
                    .iter()
                    .map(|pair| u16::from_le_bytes(*pair)),
            );
        }
        Ok(())
    }
}

/// Reads `bytes` at `address` through the stop's kept blocks.
fn copy_through(
    memory: &StopMemory,
    stop: &dyn RuntimeStop,
    address: u64,
    bytes: &mut [u8],
) -> bool {
    let mut done = 0;
    while done < bytes.len() {
        let copied =
            memory.copy::<()>(address + done as u64, &mut bytes[done..], |start, block| {
                Ok(stop.read(start, block))
            });
        match copied {
            Ok(Some(copied)) if copied > 0 => done += copied,
            _ => return stop.read(VirtualAddress::new(address), bytes),
        }
    }
    true
}
