use std::sync::Arc;

use crate::inspection::InspectionBudget;
use crate::model::{TextCompletion, TextSummary, ValueStorage};
use crate::{
    BaseTypeEncoding, GoKind, ImageAddress, InspectionExhaustion, RecordMember, RecordMemberLayout,
    SourceLanguage, TypeId, TypeKind, ValueAccessUnavailableReason, VariableUnavailableReason,
    VariableValue, VirtualAddress,
};

use super::codec::{decode_address, unsigned_value};
use super::evaluate::EvaluateError;
use super::shape::ValueShape;
use super::storage;
use super::{DwarfVariableInfo, VariableRuntime};
use crate::debug_info::TextLocation;

const PAGE_SIZE: u64 = 4096;

const MAX_STRING_DEPTH: usize = 6;

enum Stopped {
    Unreadable(VirtualAddress),

    Limited(InspectionExhaustion),
}

struct TextReader<'a> {
    runtime: &'a mut dyn VariableRuntime,
    budget: &'a mut InspectionBudget,
}

impl TextReader<'_> {
    fn read_exact(&mut self, address: VirtualAddress, size: usize) -> Result<Vec<u8>, Stopped> {
        if let Some(exhaustion) = self.budget.memory_shortfall(size as u64) {
            return Err(Stopped::Limited(exhaustion));
        }

        self.budget.consume_memory(size).map_err(Stopped::Limited)?;
        self.runtime
            .read_memory(address, size)
            .map(|bytes| bytes.to_vec())
            .map_err(|_| Stopped::Unreadable(address))
    }

    fn read_text(
        &mut self,
        address: VirtualAddress,
        limit: usize,
        width: usize,
        nul_terminated: bool,
    ) -> (Vec<u8>, bool, Option<Stopped>) {
        let mut bytes = Vec::new();

        while bytes.len() < limit {
            let Some(next) = address.get().checked_add(bytes.len() as u64) else {
                return (bytes, false, None);
            };

            let size = usize::try_from(PAGE_SIZE - next % PAGE_SIZE)
                .unwrap_or(usize::MAX)
                .min(limit - bytes.len());
            let granted = match self.budget.consume_memory_up_to(size) {
                Ok(granted) => granted,
                Err(exhaustion) => return (bytes, false, Some(Stopped::Limited(exhaustion))),
            };

            let next = VirtualAddress::new(next);
            let Ok(chunk) = self.runtime.read_memory(next, granted) else {
                return (bytes, false, Some(Stopped::Unreadable(next)));
            };

            let first = bytes.len() - bytes.len() % width;
            bytes.extend_from_slice(&chunk);

            if nul_terminated
                && let Some(position) = bytes.get(first..).and_then(|tail| {
                    tail.chunks_exact(width)
                        .position(|unit| unit.iter().all(|byte| *byte == 0))
                })
            {
                bytes.truncate(first + position * width);
                return (bytes, true, None);
            }
        }

        (bytes, false, None)
    }

    fn c_string(
        &mut self,
        address: VirtualAddress,
        width: usize,
        byte_order: crate::ByteOrder,
    ) -> TextSummary {
        let (bytes, terminated, stopped) =
            self.read_text(address, TextSummary::MAX_BYTES * width, width, true);

        let extent = (terminated && stopped.is_none()).then(|| (bytes.len() + width) as u64);
        let completion = match stopped {
            Some(stopped) => stopped.completion(None),
            None if terminated => TextCompletion::Complete,
            None => TextCompletion::Truncated { length: None },
        };

        TextSummary {
            extent,
            ..text(bytes, width, byte_order, completion)
        }
    }

    fn counted_text(
        &mut self,
        address: VirtualAddress,
        length: u64,
        width: usize,
        byte_order: crate::ByteOrder,
    ) -> TextSummary {
        let limit = usize::try_from(length)
            .unwrap_or(usize::MAX)
            .min(TextSummary::MAX_BYTES);
        let (bytes, _, stopped) = self.read_text(address, limit * width, width, false);
        let completion = match stopped {
            Some(stopped) => stopped.completion(Some(length)),
            None if length > limit as u64 => TextCompletion::Truncated {
                length: Some(length),
            },
            None => TextCompletion::Complete,
        };

        text(bytes, width, byte_order, completion)
    }

    fn storage_bytes(
        &mut self,
        storage: &ValueStorage,
        offset: u64,
        size: usize,
    ) -> Option<Result<Vec<u8>, Stopped>> {
        match storage {
            ValueStorage::Memory(address) => {
                let address = VirtualAddress::new(address.get().checked_add(offset)?);
                Some(self.read_exact(address, size))
            }

            ValueStorage::Bytes {
                raw, start, end, ..
            } => {
                let first = start.checked_add(usize::try_from(offset).ok()?)?;
                let last = first.checked_add(size)?;
                raw.get(first..last)
                    .filter(|_| last <= *end)
                    .map(|bytes| Ok(bytes.to_vec()))
            }
            ValueStorage::ImplicitPointer { .. } => None,
            ValueStorage::Composite(_) => {
                let selected =
                    storage::offset(storage.clone(), i64::try_from(offset).ok()?).ok()?;
                match storage::read(&selected, size, self.runtime, self.budget) {
                    Ok((_, raw)) => Some(Ok(raw.to_vec())),
                    Err(EvaluateError::Unavailable(
                        VariableUnavailableReason::InspectionLimit(exhaustion),
                    )) => Some(Err(Stopped::Limited(exhaustion))),
                    Err(_) => None,
                }
            }
        }
    }
}

fn text(
    bytes: Vec<u8>,
    width: usize,
    byte_order: crate::ByteOrder,
    completion: TextCompletion,
) -> TextSummary {
    if width == 1 {
        TextSummary {
            extent: None,
            bytes: bytes.into(),
            completion,
        }
    } else {
        TextSummary::from_units(&bytes, width, byte_order, completion)
    }
}

impl Stopped {
    const fn completion(self, length: Option<u64>) -> TextCompletion {
        match self {
            Self::Unreadable(address) => TextCompletion::Unreadable { address },
            Self::Limited(exhaustion) => TextCompletion::Limited { length, exhaustion },
        }
    }

    fn summary(self, length: Option<u64>) -> TextSummary {
        TextSummary {
            extent: None,
            bytes: Arc::from([]),
            completion: self.completion(length),
        }
    }
}

impl DwarfVariableInfo {
    #[expect(
        clippy::too_many_arguments,
        reason = "a value's type, shape, place, and the frame reading it"
    )]
    pub(super) fn text_summary(
        &self,
        type_id: TypeId,
        shape: &ValueShape,
        value: &VariableValue,
        storage: &ValueStorage,
        context_address: Option<ImageAddress>,
        runtime: &mut dyn VariableRuntime,
        budget: &mut InspectionBudget,
    ) -> Option<TextSummary> {
        let mut reader = TextReader { runtime, budget };

        match (shape, value) {
            (
                ValueShape::Indirection {
                    target: Some(target),
                    ..
                },
                VariableValue::Address(address),
            ) if address.address.get() != 0 && self.is_sentinel_text(type_id) => {
                Some(reader.c_string(address.address, 1, self.target.byte_order))
            }

            (
                ValueShape::Indirection {
                    target: Some(target),
                    ..
                },
                VariableValue::Address(address),
            ) if address.address.get() != 0 && self.character_width(*target).is_some() => {
                let width = self.character_width(*target)?;
                Some(reader.c_string(address.address, width, self.target.byte_order))
            }

            (
                ValueShape::Array {
                    element,
                    dimensions,
                    ..
                },
                _,
            ) if dimensions.len() == 1 && self.character_width(*element).is_some() => {
                let width = self.character_width(*element)?;
                let count = usize::try_from(dimensions.first()?.count).ok()?;
                let shown = count.min(TextSummary::MAX_BYTES);
                let mut bytes = match reader.storage_bytes(storage, 0, shown.checked_mul(width)?)? {
                    Ok(bytes) => bytes,
                    Err(stopped) => return Some(stopped.summary(None)),
                };

                let terminated = bytes
                    .chunks_exact(width)
                    .position(|unit| unit.iter().all(|byte| *byte == 0));
                let end = terminated.map_or(bytes.len(), |units| units * width);

                let extent = terminated.map(|units| ((units + 1) * width) as u64);
                bytes.truncate(end);
                Some(TextSummary {
                    extent,
                    ..text(
                        bytes,
                        width,
                        self.target.byte_order,
                        if terminated.is_none() && count > TextSummary::MAX_BYTES {
                            TextCompletion::Truncated { length: None }
                        } else {
                            TextCompletion::Complete
                        },
                    )
                })
            }
            (ValueShape::Slice { text: true, .. }, VariableValue::Slice { length, .. }) => {
                self.counted_slice_text(&mut reader, storage, *length, shape)
            }

            (
                ValueShape::Record {
                    record, members, ..
                },
                _,
            ) => self.record_text(*record, members, storage, &mut reader),
            (
                ValueShape::Indirection {
                    target: Some(target),
                    ..
                },
                VariableValue::Address(address),
            ) if address.address.get() != 0 => {
                let storage = ValueStorage::Memory(address.address);
                match self.value_shape(*target).ok()? {
                    ValueShape::Record {
                        record, members, ..
                    } => self.record_text(record, &members, &storage, &mut reader),
                    shape @ ValueShape::Slice {
                        text: true, words, ..
                    } => {
                        let length =
                            match self.read_word(&storage, self.word(words.length), &mut reader)? {
                                Ok(length) => length,
                                Err(stopped) => return Some(stopped.summary(None)),
                            };
                        self.counted_slice_text(&mut reader, &storage, length, &shape)
                    }
                    shape @ ValueShape::RuntimeArray { .. } => self.runtime_array_text(
                        *target,
                        &shape,
                        &storage,
                        context_address,
                        reader.runtime,
                        reader.budget,
                    ),
                    _ => None,
                }
            }
            _ => None,
        }
    }

    pub(super) fn text_location(
        &self,
        type_id: TypeId,
        shape: &ValueShape,
        value: &VariableValue,
        storage: &ValueStorage,
        runtime: &mut dyn VariableRuntime,
        budget: &mut InspectionBudget,
    ) -> std::result::Result<Option<TextLocation>, VariableUnavailableReason> {
        let mut reader = TextReader { runtime, budget };

        let found = match (shape, value) {
            (
                ValueShape::Indirection {
                    target: Some(target),
                    ..
                },
                VariableValue::Address(address),
            ) if self.is_character(*target) || self.is_sentinel_text(type_id) => {
                if address.address.get() == 0 {
                    return Err(VariableUnavailableReason::ValueAccess(
                        ValueAccessUnavailableReason::NullPointer,
                    ));
                }

                Some(Ok((address.address, None)))
            }

            (
                ValueShape::Slice {
                    text: true, words, ..
                },
                VariableValue::Slice { length, .. },
            ) if self.slice_width(shape) == 1 => self
                .read_pointer(storage, self.word(words.data), &mut reader)
                .map(|address| address.map(|address| (address, Some(*length)))),
            (
                ValueShape::Record {
                    record, members, ..
                },
                _,
            ) => self
                .string_parts(*record, members)
                .and_then(|(pointer, length)| {
                    let address = match self.read_pointer(storage, pointer, &mut reader)? {
                        Ok(address) => address,
                        Err(stopped) => return Some(Err(stopped)),
                    };

                    Some(
                        self.read_word(storage, length, &mut reader)?
                            .map(|length| (address, Some(length))),
                    )
                }),
            _ => None,
        };

        match found {
            None => Ok(None),
            Some(Ok((address, length))) => Ok(Some(TextLocation { address, length })),
            Some(Err(Stopped::Unreadable(address))) => {
                Err(VariableUnavailableReason::MemoryInaccessible {
                    address,
                    requested: 1,
                    completed: 0,
                    next_address: address,
                })
            }
            Some(Err(Stopped::Limited(exhaustion))) => {
                Err(VariableUnavailableReason::InspectionLimit(exhaustion))
            }
        }
    }

    fn record_text(
        &self,
        record: TypeId,
        members: &[RecordMember],
        storage: &ValueStorage,
        reader: &mut TextReader<'_>,
    ) -> Option<TextSummary> {
        let (pointer, length) = self.string_parts(record, members)?;
        let address = match self.read_pointer(storage, pointer, reader)? {
            Ok(address) => address,
            Err(stopped) => return Some(stopped.summary(None)),
        };

        let length = match self.read_word(storage, length, reader)? {
            Ok(length) => length,
            Err(stopped) => return Some(stopped.summary(None)),
        };

        Some(reader.counted_text(address, length, 1, self.target.byte_order))
    }

    fn slice_width(&self, slice: &ValueShape) -> usize {
        let ValueShape::Slice { element, .. } = slice else {
            return 1;
        };

        self.value_shape(*element)
            .ok()
            .and_then(|shape| shape.scalar().map(|base| base.byte_size))
            .filter(|size| matches!(size, 2 | 4))
            .and_then(|size| usize::try_from(size).ok())
            .unwrap_or(1)
    }

    fn counted_slice_text(
        &self,
        reader: &mut TextReader<'_>,
        storage: &ValueStorage,
        length: u64,
        slice: &ValueShape,
    ) -> Option<TextSummary> {
        let ValueShape::Slice { words, .. } = slice else {
            return None;
        };

        let address = match self.read_pointer(storage, self.word(words.data), reader)? {
            Ok(address) => address,
            Err(stopped) => return Some(stopped.summary(Some(length))),
        };

        let text = reader.counted_text(
            address,
            length,
            self.slice_width(slice),
            self.target.byte_order,
        );
        Some(self.slice_text(text, slice))
    }

    fn slice_text(&self, mut text: TextSummary, slice: &ValueShape) -> TextSummary {
        let ValueShape::Slice { element, .. } = slice else {
            return text;
        };

        let c_characters = self.value_shape(*element).is_ok_and(|shape| {
            shape.scalar().is_some_and(|base| {
                base.byte_size == 1
                    && matches!(
                        base.encoding,
                        BaseTypeEncoding::Signed | BaseTypeEncoding::SignedCharacter
                    )
            })
        });
        if c_characters
            && text.completion == TextCompletion::Complete
            && let Some((0, rest)) = text.bytes.split_last()
        {
            text.bytes = rest.into();
        }

        text
    }

    fn is_character(&self, id: TypeId) -> bool {
        self.character_width(id) == Some(1)
    }

    fn character_width(&self, id: TypeId) -> Option<usize> {
        let base = self.value_shape(id).ok()?.scalar()?.clone();
        let width = usize::try_from(base.byte_size).ok()?;
        if matches!(
            base.encoding,
            BaseTypeEncoding::SignedCharacter | BaseTypeEncoding::UnsignedCharacter
        ) {
            return matches!(width, 1 | 2 | 4).then_some(width);
        }

        if !matches!(
            base.encoding,
            BaseTypeEncoding::Signed | BaseTypeEncoding::Unsigned
        ) {
            return None;
        }

        let mut current = id;

        for _ in 0..MAX_STRING_DEPTH {
            let info = self.type_info(current).ok()?;
            match &info.kind {
                TypeKind::Named {
                    target: Some(target),
                    ..
                } => {
                    let named = match info.name.as_ref() {
                        "wchar_t" | "char32_t" => 4,
                        "char16_t" => 2,
                        _ => 0,
                    };

                    if named != 0 {
                        return (named == width).then_some(width);
                    }

                    current = target.id;
                }
                TypeKind::Modified { target, .. } => current = target.id,
                _ => return None,
            }
        }

        None
    }

    fn is_sentinel_text(&self, id: TypeId) -> bool {
        let mut current = id;

        for _ in 0..MAX_STRING_DEPTH {
            let Ok(info) = self.type_info(current) else {
                return false;
            };

            match &info.kind {
                TypeKind::Modified { target, .. }
                | TypeKind::Named {
                    target: Some(target),
                    ..
                } => current = target.id,
                TypeKind::Pointer {
                    target: Some(target),
                    ..
                } => {
                    let named = match info.identity.as_ref().map(|identity| identity.language) {
                        Some(SourceLanguage::Zig) => info.name.starts_with("[*:0]"),
                        Some(SourceLanguage::Odin) => info.name.as_ref() == "cstring",
                        _ => false,
                    };

                    return named
                        && self.value_shape(target.id).is_ok_and(|shape| {
                            shape.scalar().is_some_and(|base| {
                                base.byte_size == 1 && base.encoding == BaseTypeEncoding::Unsigned
                            })
                        });
                }
                _ => return false,
            }
        }

        false
    }

    fn string_parts(&self, record: TypeId, members: &[RecordMember]) -> Option<(u64, u64)> {
        let go = self.type_info(record).ok()?.identity.as_ref()?.go?;
        if go.kind != GoKind::String {
            return None;
        }

        Some((
            member_offset(members, "str")?,
            member_offset(members, "len")?,
        ))
    }

    pub(super) const fn pointer_bytes(&self) -> usize {
        self.target.pointer_width.bytes() as usize
    }

    fn word(&self, index: u8) -> u64 {
        u64::from(index) * self.pointer_bytes() as u64
    }

    fn read_pointer(
        &self,
        storage: &ValueStorage,
        offset: u64,
        reader: &mut TextReader<'_>,
    ) -> Option<Result<VirtualAddress, Stopped>> {
        let size = self.pointer_bytes();
        Some(match reader.storage_bytes(storage, offset, size)? {
            Ok(raw) => Ok(decode_address(&raw, size as u64, self.target).ok()?),
            Err(stopped) => Err(stopped),
        })
    }

    fn read_word(
        &self,
        storage: &ValueStorage,
        offset: u64,
        reader: &mut TextReader<'_>,
    ) -> Option<Result<u64, Stopped>> {
        Some(
            match reader.storage_bytes(storage, offset, self.pointer_bytes())? {
                Ok(raw) => {
                    Ok(u64::try_from(unsigned_value(&raw, self.target.byte_order).ok()?).ok()?)
                }
                Err(stopped) => Err(stopped),
            },
        )
    }
}

fn member_offset(members: &[RecordMember], name: &str) -> Option<u64> {
    let member = members
        .iter()
        .find(|member| member.name.as_deref() == Some(name))?;
    match member.layout {
        RecordMemberLayout::ByteOffset(offset) => Some(offset),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::debug_info::{VariableRegister, VariableRuntimeError};
    use crate::{ImageAddress, VariableUnavailableReason};

    struct Filled(&'static [u8]);

    impl VariableRuntime for Filled {
        fn register(
            &mut self,
            _: crate::unwind::DwarfRegister,
        ) -> Result<VariableRegister, VariableRuntimeError> {
            Err(VariableRuntimeError::Fatal("no registers".into()))
        }

        fn call_frame_cfa(&self) -> Result<VirtualAddress, VariableRuntimeError> {
            Err(VariableRuntimeError::Fatal("no frame".into()))
        }

        fn tls_address(&mut self, _: u64) -> Result<VirtualAddress, VariableUnavailableReason> {
            Err(crate::UnsupportedVariableFeature::Tls.into())
        }

        fn relocate(&self, address: ImageAddress) -> Result<VirtualAddress, Arc<str>> {
            Ok(VirtualAddress::new(address.get()))
        }

        fn image_address(&self, _: VirtualAddress) -> Option<ImageAddress> {
            None
        }

        fn read_memory(
            &mut self,
            address: VirtualAddress,
            size: usize,
        ) -> Result<Arc<[u8]>, VariableRuntimeError> {
            let start = usize::try_from(address.get()).unwrap_or(usize::MAX);
            Ok((0..size)
                .map(|offset| {
                    start
                        .checked_add(offset)
                        .and_then(|at| self.0.get(at))
                        .copied()
                        .unwrap_or(b'x')
                })
                .collect())
        }

        fn entry_value(
            &mut self,
            _: crate::debug_info::EntryParameter,
            _: &mut InspectionBudget,
        ) -> Result<u64, VariableRuntimeError> {
            Err(VariableRuntimeError::Fatal("no caller".into()))
        }
    }

    #[test]
    fn text_reaching_the_end_of_the_address_space_is_truncated() {
        let mut budget = InspectionBudget::default();
        let mut reader = TextReader {
            runtime: &mut Filled(b""),
            budget: &mut budget,
        };

        let text = reader.c_string(
            VirtualAddress::new(u64::MAX - 2),
            1,
            crate::ByteOrder::Little,
        );
        assert_eq!(&*text.bytes, b"xxx");
        assert_eq!(text.completion, TextCompletion::Truncated { length: None });
        assert_eq!(text.extent, None);
    }

    #[test]
    fn a_c_string_extends_through_its_terminator() {
        let mut budget = InspectionBudget::default();
        let mut reader = TextReader {
            runtime: &mut Filled(b"h\0\xe9\0!\0\0\0tail"),
            budget: &mut budget,
        };

        let wide = reader.c_string(VirtualAddress::new(0), 2, crate::ByteOrder::Little);
        assert_eq!(&*wide.bytes, "hé!".as_bytes());
        assert_eq!(wide.completion, TextCompletion::Complete);
        assert_eq!(wide.extent, Some(8));

        let narrow = reader.c_string(VirtualAddress::new(0), 1, crate::ByteOrder::Little);
        assert_eq!(&*narrow.bytes, b"h");
        assert_eq!(narrow.extent, Some(2));
    }
}
