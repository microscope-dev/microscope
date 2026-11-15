use std::sync::Arc;

use crate::VirtualAddress;
use crate::image::v8::{Constant, Field, V8Layout, V8Version};
use crate::runtime_model::{RuntimeStop, StopMemory};

pub(super) type Problem = Arc<str>;

pub(super) const MAX_STRING: u64 = 4096;

const MAX_STRING_DEPTH: usize = 32;

#[derive(Clone, Copy)]
pub(super) struct Reader<'a> {
    pub(super) stop: &'a dyn RuntimeStop,
    pub(super) layout: &'a V8Layout,
    pub(super) version: V8Version,

    pub(super) charge: Option<&'a dyn Fn(usize) -> Result<(), Problem>>,

    pub(super) blocks: Option<&'a StopMemory>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(super) struct Object(pub(super) u64);

pub(super) fn address_at(address: u64, bytes: u64) -> Result<u64, Problem> {
    address.checked_add(bytes).ok_or_else(|| {
        format!("{bytes:#x} bytes past {address:#x} lies past the address space").into()
    })
}

impl Reader<'_> {
    pub(super) fn bytes_into(&self, address: u64, bytes: &mut [u8]) -> Result<(), Problem> {
        let unreadable = || format!("the memory at {address:#x} is unreadable").into();
        if let Some(blocks) = self.blocks
            && bytes.len() <= blocks.block()
            && address.checked_add(bytes.len() as u64).is_some()
        {
            let mut done = 0;
            while let Some(rest) = bytes.get_mut(done..)
                && !rest.is_empty()
            {
                let copied = blocks.copy(address + done as u64, rest, |start, block| {
                    if let Some(charge) = self.charge {
                        charge(block.len())?;
                    }
                    Ok::<_, Problem>(self.stop.read(start, block))
                })?;
                match copied {
                    Some(copied) if copied > 0 => done += copied,
                    _ => break,
                }
                debug_assert!(
                    done <= bytes.len(),
                    "the block cache copied {done} bytes into {} at {address:#x}",
                    bytes.len(),
                );
            }
            if done == bytes.len() {
                return Ok(());
            }
        }
        if let Some(charge) = self.charge {
            charge(bytes.len())?;
        }
        self.stop
            .read(VirtualAddress::new(address), bytes)
            .then_some(())
            .ok_or_else(unreadable)
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

    pub(super) fn smi(&self, word: u64) -> Result<Option<i64>, Problem> {
        let mask = self.constant(Constant::SmiTagMask)?.cast_unsigned();
        let tag = self.constant(Constant::SmiTag)?.cast_unsigned();
        let shift = u32::try_from(self.constant(Constant::SmiShiftSize)?)
            .map_err(|_| "the Smi shift is out of range")?
            + mask.count_ones();
        Ok((word & mask == tag).then(|| word.cast_signed().checked_shr(shift).unwrap_or(0)))
    }

    pub(super) fn smi_word(&self, value: i64) -> Result<Option<u64>, Problem> {
        let mask = self.constant(Constant::SmiTagMask)?.cast_unsigned();
        let tag = self.constant(Constant::SmiTag)?.cast_unsigned();
        let shift = u32::try_from(self.constant(Constant::SmiShiftSize)?)
            .map_err(|_| "the Smi shift is out of range")?
            + mask.count_ones();
        let word = value
            .checked_shl(shift)
            .map(|shifted| shifted.cast_unsigned() | tag);
        Ok(word.filter(|word| self.smi(*word).ok().flatten() == Some(value)))
    }

    pub(super) fn object(&self, word: u64) -> Result<Option<Object>, Problem> {
        let mask = self.constant(Constant::HeapObjectTagMask)?.cast_unsigned();
        let tag = self.constant(Constant::HeapObjectTag)?.cast_unsigned();
        Ok((word & mask == tag && word > tag).then_some(Object(word)))
    }

    pub(super) fn field_address(&self, object: Object, field: Field) -> Result<u64, Problem> {
        let tag = self.constant(Constant::HeapObjectTag)?.cast_unsigned();
        let offset = self.offset(field)?;
        object
            .0
            .checked_sub(tag)
            .and_then(|start| start.checked_add(offset))
            .ok_or_else(|| {
                format!(
                    "{} of {:#x} lies past the address space",
                    field.name(),
                    object.0
                )
                .into()
            })
    }

    pub(super) fn field(&self, object: Object, field: Field) -> Result<u64, Problem> {
        self.word(self.field_address(object, field)?)
    }

    pub(super) fn smi_field(&self, object: Object, field: Field) -> Result<i64, Problem> {
        let word = self.field(object, field)?;
        self.smi(word)?
            .ok_or_else(|| format!("{} of {:#x} is no Smi", field.name(), object.0).into())
    }

    pub(super) fn int32_field(&self, object: Object, field: Field) -> Result<i32, Problem> {
        let mut bytes = [0; 4];
        self.bytes_into(self.field_address(object, field)?, &mut bytes)?;
        Ok(i32::from_le_bytes(bytes))
    }

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

    pub(super) fn element(&self, array: Object, index: i64) -> Result<u64, Problem> {
        let kinds = [
            (
                Constant::FixedArrayType,
                Field::FixedArrayBaseLength,
                Field::FixedArrayObjects,
            ),
            (
                Constant::TrustedFixedArrayType,
                Field::TrustedFixedArrayLength,
                Field::TrustedFixedArrayObjects,
            ),
            (
                Constant::ProtectedFixedArrayType,
                Field::ProtectedFixedArrayLength,
                Field::ProtectedFixedArrayObjects,
            ),
            (
                Constant::NameDictionaryType,
                Field::FixedArrayBaseLength,
                Field::FixedArrayObjects,
            ),
            (
                Constant::NumberDictionaryType,
                Field::FixedArrayBaseLength,
                Field::FixedArrayObjects,
            ),
            (
                Constant::GlobalDictionaryType,
                Field::FixedArrayBaseLength,
                Field::FixedArrayObjects,
            ),
            (
                Constant::OrderedHashMapType,
                Field::FixedArrayBaseLength,
                Field::FixedArrayObjects,
            ),
            (
                Constant::OrderedHashSetType,
                Field::FixedArrayBaseLength,
                Field::FixedArrayObjects,
            ),
            (
                Constant::WeakFixedArrayType,
                Field::WeakFixedArrayLength,
                Field::WeakFixedArrayObjects,
            ),
            (
                Constant::TrustedWeakFixedArrayType,
                Field::TrustedWeakFixedArrayLength,
                Field::TrustedWeakFixedArrayObjects,
            ),
        ];
        let ty = self.instance_type(array)?;
        let (_, length, objects) = kinds
            .into_iter()
            .find(|(kind, length, _)| {
                self.layout.has(*length) && self.constant(*kind).ok() == Some(ty)
            })
            .ok_or_else(|| format!("{:#x} is no fixed array", array.0))?;
        if !(0..self.smi_field(array, length)?).contains(&index) {
            return Err(format!("index {index} is past the end of {:#x}", array.0).into());
        }
        let objects = self.field_address(array, objects)?;
        let address = index
            .cast_unsigned()
            .checked_mul(8)
            .and_then(|offset| objects.checked_add(offset))
            .ok_or_else(|| {
                format!(
                    "element {index} of {:#x} lies past the address space",
                    array.0
                )
            })?;
        self.word(address)
    }

    pub(super) fn map(&self, object: Object) -> Result<Object, Problem> {
        let map = self.field(object, Field::HeapObjectMap)?;
        self.object(map)?
            .ok_or_else(|| format!("the map of {:#x} is no object", object.0).into())
    }

    pub(super) fn byte_field(&self, object: Object, field: Field) -> Result<u8, Problem> {
        let mut byte = [0; 1];
        self.bytes_into(self.field_address(object, field)?, &mut byte)?;
        Ok(byte[0])
    }

    pub(super) fn instance_type(&self, object: Object) -> Result<i64, Problem> {
        let map = self.map(object)?;
        let mut bytes = [0; 2];
        self.bytes_into(self.field_address(map, Field::MapInstanceType)?, &mut bytes)?;
        Ok(i64::from(u16::from_le_bytes(bytes)))
    }

    pub(super) fn is(&self, object: Object, ty: Constant) -> Result<bool, Problem> {
        Ok(self.instance_type(object)? == self.constant(ty)?)
    }

    pub(super) fn string(&self, object: Object) -> Result<String, Problem> {
        let length = u64::try_from(self.int32_field(object, Field::StringLength)?)
            .map_err(|_| "a string has a negative length")?;
        if length > MAX_STRING {
            return Err(format!("the string exceeds the {MAX_STRING}-character read limit").into());
        }
        let mut text = Vec::new();
        self.string_into(object, 0, MAX_STRING, 0, &mut text)?;
        debug_assert!(
            text.len() as u64 <= MAX_STRING,
            "the string {:#x} read {} units past its limit of {MAX_STRING}",
            object.0,
            text.len(),
        );
        if text.len() as u64 != length {
            return Err("the string's contents end before its declared length".into());
        }
        Ok(String::from_utf16_lossy(&text))
    }

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
            if ty & self.constant(Constant::UncachedExternalStringMask)?
                == self.constant(Constant::UncachedExternalStringTag)?
            {
                return Err("uncached external strings are unsupported".into());
            }
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

    fn units(
        &self,
        address: u64,
        start: u64,
        end: u64,
        one_byte: bool,
        text: &mut Vec<u16>,
    ) -> Result<(), Problem> {
        debug_assert!(
            start < end,
            "reading the units {start}..{end} of the string at {address:#x}, which are none",
        );
        let width = if one_byte { 1 } else { 2 };
        let length = usize::try_from((end - start) * width).map_err(|_| "a string is too long")?;
        let mut bytes = vec![0; length];
        self.bytes_into(address_at(address, start * width)?, &mut bytes)?;
        let before = text.len();
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
        debug_assert_eq!(
            (text.len() - before) as u64,
            end - start,
            "the string at {address:#x} appended the wrong number of units for {start}..{end}",
        );
        Ok(())
    }
}

#[cfg(test)]
pub(super) mod tests {
    use std::collections::HashMap;

    use proptest::prelude::*;

    use super::{Object, Reader};
    use crate::image::v8::{Constant, Field, V8Layout, V8Version};
    use crate::runtime_model::RuntimeStop;
    use crate::{LoadBias, ThreadId, VirtualAddress};

    #[derive(Default)]
    pub(in super::super) struct Heap(HashMap<u64, u8>);

    impl Heap {
        pub(in super::super) fn word(&mut self, address: u64, word: u64) {
            for (index, byte) in (0..).zip(word.to_le_bytes()) {
                self.0.insert(address + index, byte);
            }
        }
    }

    impl RuntimeStop for Heap {
        fn read(&self, address: VirtualAddress, bytes: &mut [u8]) -> bool {
            for (index, byte) in (0..).zip(bytes.iter_mut()) {
                let Some(read) = address
                    .get()
                    .checked_add(index)
                    .and_then(|at| self.0.get(&at))
                else {
                    return false;
                };
                *byte = *read;
            }
            true
        }

        fn thread_pointer(&self, _thread: ThreadId) -> Option<u64> {
            None
        }

        fn instruction(&self, _thread: ThreadId) -> Option<VirtualAddress> {
            None
        }

        fn load_bias(&self) -> LoadBias {
            LoadBias::default()
        }

        fn threads(&self) -> Vec<ThreadId> {
            Vec::new()
        }
    }

    pub(in super::super) fn layout() -> V8Layout {
        let mut layout = crate::debug_info::v8::generated_layout(14, 6).expect("a layout");
        for (constant, value) in [
            (Constant::SmiTag, 0),
            (Constant::SmiTagMask, 1),
            (Constant::SmiShiftSize, 31),
            (Constant::HeapObjectTag, 1),
            (Constant::HeapObjectTagMask, 3),
        ] {
            layout.set_constant(constant, value);
        }
        layout
    }

    pub(in super::super) fn reader<'a>(heap: &'a Heap, layout: &'a V8Layout) -> Reader<'a> {
        Reader {
            stop: heap,
            layout,
            version: V8Version {
                major: 14,
                minor: 6,
                ..V8Version::default()
            },
            charge: None,
            blocks: None,
        }
    }

    #[derive(Debug, Clone)]
    enum Text {
        Seq {
            units: Vec<u16>,
            one_byte: bool,
        },
        Cons(Box<Self>, Box<Self>),
        Sliced {
            parent: Box<Self>,
            offset: u64,
            length: u64,
        },
        Thin(Box<Self>),
    }

    impl Text {
        fn units(&self) -> Vec<u16> {
            match self {
                Self::Seq { units, .. } => units.clone(),
                Self::Cons(first, second) => [first.units(), second.units()].concat(),
                Self::Sliced {
                    parent,
                    offset,
                    length,
                } => {
                    let units = parent.units();
                    let start = usize::try_from(*offset).unwrap();
                    units[start..start + usize::try_from(*length).unwrap()].to_vec()
                }
                Self::Thin(actual) => actual.units(),
            }
        }
    }

    fn sequential() -> impl Strategy<Value = Text> {
        prop_oneof![
            proptest::collection::vec(0..0x100_u16, 0..12).prop_map(|units| Text::Seq {
                units,
                one_byte: true
            }),
            proptest::collection::vec(any::<u16>(), 0..12).prop_map(|units| Text::Seq {
                units,
                one_byte: false
            }),
        ]
    }

    fn text() -> impl Strategy<Value = Text> {
        let sliced = (
            sequential(),
            any::<prop::sample::Index>(),
            any::<prop::sample::Index>(),
        )
            .prop_map(|(parent, start, length)| {
                let total = parent.units().len();
                let offset = start.index(total + 1);
                let length = length.index(total - offset + 1);
                Text::Sliced {
                    parent: Box::new(parent),
                    offset: offset as u64,
                    length: length as u64,
                }
            });
        prop_oneof![sequential(), sliced].prop_recursive(4, 16, 2, |inner| {
            prop_oneof![
                (inner.clone(), inner.clone())
                    .prop_map(|(first, second)| Text::Cons(Box::new(first), Box::new(second))),
                inner.prop_map(|actual| Text::Thin(Box::new(actual))),
            ]
        })
    }

    struct Strings<'a> {
        heap: Heap,
        layout: &'a V8Layout,
        next: u64,
        maps: HashMap<i64, u64>,

        words: Vec<(u64, usize, u64)>,
    }

    impl Strings<'_> {
        fn constant(&self, constant: Constant) -> i64 {
            self.layout
                .constant(constant)
                .expect("the layout has the constant")
        }

        fn offset(&self, field: Field) -> u64 {
            self.layout.field(field).expect("the layout has the field")
        }

        fn write(&mut self, at: u64, width: usize, value: u64) {
            for (address, byte) in (at..).zip(value.to_le_bytes().into_iter().take(width)) {
                self.heap.0.insert(address, byte);
            }
            self.words.push((at, width, value));
        }

        fn map(&mut self, ty: i64) -> u64 {
            if let Some(map) = self.maps.get(&ty) {
                return *map;
            }
            let map = self.next;
            self.next += 0x100;
            self.heap.word(map, map + 1);
            let at = map + self.offset(Field::MapInstanceType);
            for (address, byte) in (at..).zip(ty.to_le_bytes().into_iter().take(2)) {
                self.heap.0.insert(address, byte);
            }
            self.maps.insert(ty, map + 1);
            map + 1
        }

        fn string(&mut self, text: &Text) -> u64 {
            let at = self.next;
            self.next += 0x100;
            let one_byte = |text: &Text| {
                matches!(text, Text::Seq { one_byte: true, .. } | Text::Sliced { .. })
                    && match text {
                        Text::Sliced { parent, .. } => {
                            matches!(**parent, Text::Seq { one_byte: true, .. })
                        }
                        _ => true,
                    }
            };
            let encoding = if one_byte(text) {
                self.constant(Constant::OneByteStringTag)
            } else {
                self.constant(Constant::TwoByteStringTag)
            };
            let representation = self.constant(match text {
                Text::Seq { .. } => Constant::SeqStringTag,
                Text::Cons(..) => Constant::ConsStringTag,
                Text::Sliced { .. } => Constant::SlicedStringTag,
                Text::Thin(_) => Constant::ThinStringTag,
            });
            let map = self.map(representation | encoding);
            self.write(at + self.offset(Field::HeapObjectMap), 8, map);
            let length = text.units().len() as u64;
            self.write(at + self.offset(Field::StringLength), 4, length);

            match text {
                Text::Seq { units, one_byte } => {
                    let (chars, width) = if *one_byte {
                        (Field::SeqOneByteStringChars, 1)
                    } else {
                        (Field::SeqTwoByteStringChars, 2)
                    };
                    let chars = at + self.offset(chars);
                    for (index, unit) in (0..).zip(units) {
                        self.write(
                            chars + index * width,
                            usize::try_from(width).unwrap(),
                            u64::from(*unit),
                        );
                    }
                }
                Text::Cons(first, second) => {
                    let (first, second) = (self.string(first), self.string(second));
                    self.write(at + self.offset(Field::ConsStringFirst), 8, first);
                    self.write(at + self.offset(Field::ConsStringSecond), 8, second);
                }
                Text::Sliced { parent, offset, .. } => {
                    let parent = self.string(parent);
                    self.write(at + self.offset(Field::SlicedStringParent), 8, parent);
                    self.write(at + self.offset(Field::SlicedStringOffset), 8, offset << 32);
                }
                Text::Thin(actual) => {
                    let actual = self.string(actual);
                    self.write(at + self.offset(Field::ThinStringActual), 8, actual);
                }
            }

            at + 1
        }
    }

    proptest! {
        /// A string of any representation reads as its text, and wherever
        /// one of its words leads instead, as a text or a reason.
        #[test]
        fn a_string_reads_as_its_text_wherever_its_words_lead(
            text in text(),
            damage in proptest::option::of((
                any::<prop::sample::Index>(),
                prop_oneof![
                    crate::test_strategies::hostile_address(vec![0x1_0000, 0x1_0001]),
                    crate::test_strategies::u64_edges(),
                ],
            )),
        ) {
            let mut layout = layout();
            for (constant, value) in [
                (Constant::StringRepresentationMask, 7),
                (Constant::SeqStringTag, 0),
                (Constant::ConsStringTag, 1),
                (Constant::ExternalStringTag, 2),
                (Constant::SlicedStringTag, 3),
                (Constant::ThinStringTag, 5),
                (Constant::StringEncodingMask, 8),
                (Constant::TwoByteStringTag, 0),
                (Constant::OneByteStringTag, 8),
                (Constant::FirstNonstringType, 128),
            ] {
                layout.set_constant(constant, value);
            }
            let mut strings = Strings {
                heap: Heap::default(),
                layout: &layout,
                next: 0x1_0000,
                maps: HashMap::new(),
                words: Vec::new(),
            };
            let root = strings.string(&text);
            let damaged = damage.is_some_and(|(index, value)| {
                let (at, width, before) = strings.words[index.index(strings.words.len())];
                for (address, byte) in (at..).zip(value.to_le_bytes().into_iter().take(width)) {
                    strings.heap.0.insert(address, byte);
                }
                value & (u64::MAX >> (64 - 8 * width)) != before
            });

            let reader = reader(&strings.heap, &layout);
            let object = reader.object(root).unwrap().expect("a tagged word");
            let read = reader.string(object);
            if !damaged {
                prop_assert_eq!(read, Ok(String::from_utf16_lossy(&text.units())));
            }
        }
    }

    #[test]
    fn fields_past_the_end_of_the_address_space_are_problems() {
        let layout = layout();
        let mut heap = Heap::default();
        let pointer = reader(&heap, &layout)
            .object(0xffff_ffff_ffff_fffd)
            .unwrap()
            .expect("a tagged word");
        let problem = reader(&heap, &layout)
            .string(pointer)
            .expect_err("a string's length lies past the address space");
        assert!(problem.contains("address space"), "{problem}");

        let map = 0x1000;
        let fixed = layout.constant(Constant::FixedArrayType).unwrap();
        heap.word(map, map + 1);
        heap.word(
            map + layout.field(Field::MapInstanceType).unwrap(),
            fixed.cast_unsigned(),
        );
        let array = 0xffff_ffff_ffff_ff00;
        heap.word(array, map + 1);
        heap.word(
            array + layout.field(Field::FixedArrayBaseLength).unwrap(),
            1 << 62,
        );
        let problem = reader(&heap, &layout)
            .element(Object(array + 1), 1 << 28)
            .expect_err("the element lies past the address space");
        assert!(problem.contains("address space"), "{problem}");
    }
}
