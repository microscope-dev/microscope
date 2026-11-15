use super::inspect::{ElementType, Kind, Value, ViewRange};
use super::objects::{Object, Problem, Reader};
use super::own::elements_kind;
use crate::image::v8::{Constant, Field};

pub(super) const SHOWN: u64 = 100;

pub(super) fn element_type(reader: &Reader<'_>, kind: i64) -> Option<ElementType> {
    use Constant as C;
    use ElementType as E;
    const KINDS: [(Constant, Constant, ElementType); 12] = [
        (C::ElementsKindUint8, C::ElementsKindRabGsabUint8, E::Uint8),
        (C::ElementsKindInt8, C::ElementsKindRabGsabInt8, E::Int8),
        (
            C::ElementsKindUint16,
            C::ElementsKindRabGsabUint16,
            E::Uint16,
        ),
        (C::ElementsKindInt16, C::ElementsKindRabGsabInt16, E::Int16),
        (
            C::ElementsKindUint32,
            C::ElementsKindRabGsabUint32,
            E::Uint32,
        ),
        (C::ElementsKindInt32, C::ElementsKindRabGsabInt32, E::Int32),
        (
            C::ElementsKindBigUint64,
            C::ElementsKindRabGsabBigUint64,
            E::BigUint64,
        ),
        (
            C::ElementsKindBigInt64,
            C::ElementsKindRabGsabBigInt64,
            E::BigInt64,
        ),
        (
            C::ElementsKindUint8Clamped,
            C::ElementsKindRabGsabUint8Clamped,
            E::Uint8Clamped,
        ),
        (
            C::ElementsKindFloat32,
            C::ElementsKindRabGsabFloat32,
            E::Float32,
        ),
        (
            C::ElementsKindFloat64,
            C::ElementsKindRabGsabFloat64,
            E::Float64,
        ),
        (
            C::ElementsKindFloat16,
            C::ElementsKindRabGsabFloat16,
            E::Float16,
        ),
    ];
    KINDS.iter().find_map(|(fixed, resizable, element)| {
        [fixed, resizable]
            .iter()
            .any(|constant| reader.constant(**constant).ok() == Some(kind))
            .then_some(*element)
    })
}

pub(super) const fn size(element: ElementType) -> u64 {
    match element {
        ElementType::Int8 | ElementType::Uint8 | ElementType::Uint8Clamped => 1,
        ElementType::Int16 | ElementType::Uint16 | ElementType::Float16 => 2,
        ElementType::Int32 | ElementType::Uint32 | ElementType::Float32 => 4,
        ElementType::Float64 | ElementType::BigInt64 | ElementType::BigUint64 => 8,
    }
}

#[expect(
    clippy::indexing_slicing,
    reason = "a compile-time check: an index out of bounds fails the build"
)]
const _: () = {
    use ElementType as E;
    let all = [
        E::Int8,
        E::Uint8,
        E::Uint8Clamped,
        E::Int16,
        E::Uint16,
        E::Int32,
        E::Uint32,
        E::Float16,
        E::Float32,
        E::Float64,
        E::BigInt64,
        E::BigUint64,
    ];
    let mut at = 0;
    while at < all.len() {
        assert!(size(all[at]) == all[at].size() as u64);
        at += 1;
    }
};

pub(super) const fn constructor_name(element: ElementType) -> &'static str {
    match element {
        ElementType::Int8 => "Int8Array",
        ElementType::Uint8 => "Uint8Array",
        ElementType::Uint8Clamped => "Uint8ClampedArray",
        ElementType::Int16 => "Int16Array",
        ElementType::Uint16 => "Uint16Array",
        ElementType::Int32 => "Int32Array",
        ElementType::Uint32 => "Uint32Array",
        ElementType::Float16 => "Float16Array",
        ElementType::Float32 => "Float32Array",
        ElementType::Float64 => "Float64Array",
        ElementType::BigInt64 => "BigInt64Array",
        ElementType::BigUint64 => "BigUint64Array",
    }
}

struct Buffer {
    byte_length: u64,
    detached: bool,
    shared: bool,
    data: u64,
}

fn bits(reader: &Reader<'_>, word: u32, shift: Constant, mask: Constant) -> Result<u32, Problem> {
    let mask = u32::try_from(reader.constant(mask)?).map_err(|_| "a mask is out of range")?;
    let shift = u32::try_from(reader.constant(shift)?).map_err(|_| "a shift is out of range")?;
    Ok((word & mask) >> shift)
}

fn word_field(reader: &Reader<'_>, object: Object, field: Field) -> Result<u64, Problem> {
    reader.word(reader.field_address(object, field)?)
}

fn u32_field(reader: &Reader<'_>, object: Object, field: Field) -> Result<u32, Problem> {
    Ok(reader.int32_field(object, field)?.cast_unsigned())
}

fn buffer(reader: &Reader<'_>, buffer: Object) -> Result<Buffer, Problem> {
    if !reader.is(buffer, Constant::JSArrayBufferType)? {
        return Err(format!("{:#x} is not an ArrayBuffer", buffer.0).into());
    }
    let flags = u32_field(reader, buffer, Field::JSArrayBufferBitField)?;
    let shared = bits(
        reader,
        flags,
        Constant::JSArrayBufferIsSharedShift,
        Constant::JSArrayBufferIsSharedMask,
    )? != 0;
    let resizable = bits(
        reader,
        flags,
        Constant::JSArrayBufferIsResizableByJsShift,
        Constant::JSArrayBufferIsResizableByJsMask,
    )? != 0;
    if shared && resizable {
        return Err("a growable SharedArrayBuffer stores its length outside the heap".into());
    }
    Ok(Buffer {
        byte_length: word_field(reader, buffer, Field::JSArrayBufferRawByteLength)?,
        detached: bits(
            reader,
            flags,
            Constant::JSArrayBufferWasDetachedShift,
            Constant::JSArrayBufferWasDetachedMask,
        )? != 0,
        shared,
        data: word_field(reader, buffer, Field::JSArrayBufferBackingStore)?,
    })
}

fn shown(reader: &Reader<'_>, data: u64, count: u64, full: bool) -> Result<Vec<u8>, Problem> {
    if !full {
        return Ok(Vec::new());
    }
    let mut bytes = vec![0; usize::try_from(count).map_err(|_| "too many bytes")?];
    if !bytes.is_empty() {
        reader.bytes_into(data, &mut bytes)?;
    }
    Ok(bytes)
}

pub(super) fn array_buffer(
    reader: &Reader<'_>,
    object: Object,
    full: bool,
) -> Result<Kind, Problem> {
    let buffer = buffer(reader, object)?;
    let bytes = if buffer.detached {
        None
    } else {
        Some(shown(
            reader,
            buffer.data,
            buffer.byte_length.min(SHOWN),
            full,
        )?)
    };
    debug_assert!(
        bytes
            .as_ref()
            .is_none_or(|bytes| bytes.len() as u64 <= SHOWN),
        "the buffer {:#x} shows more than {SHOWN} bytes",
        object.0,
    );
    Ok(Kind::ArrayBuffer {
        shared: buffer.shared,
        byte_length: if buffer.detached {
            0
        } else {
            buffer.byte_length
        },
        bytes,
    })
}

fn view_range(
    reader: &Reader<'_>,
    view: Object,
    buffer: &Buffer,
) -> Result<Option<ViewRange>, Problem> {
    if buffer.detached {
        return Ok(None);
    }
    let flags = u32_field(reader, view, Field::JSArrayBufferViewBitField)?;
    let tracking = bits(
        reader,
        flags,
        Constant::JSArrayBufferViewIsLengthTrackingShift,
        Constant::JSArrayBufferViewIsLengthTrackingMask,
    )? != 0;
    let resizable = bits(
        reader,
        flags,
        Constant::JSArrayBufferViewIsBackedByRabShift,
        Constant::JSArrayBufferViewIsBackedByRabMask,
    )? != 0;
    let byte_offset = word_field(reader, view, Field::JSArrayBufferViewRawByteOffset)?;
    let raw = word_field(reader, view, Field::JSArrayBufferViewRawByteLength)?;
    if !resizable && !tracking {
        return Ok(Some(ViewRange {
            byte_length: raw,
            byte_offset,
        }));
    }
    let Some(rest) = buffer.byte_length.checked_sub(byte_offset) else {
        return Ok(None);
    };
    Ok(if tracking {
        Some(ViewRange {
            byte_length: rest,
            byte_offset,
        })
    } else {
        (raw <= rest).then_some(ViewRange {
            byte_length: raw,
            byte_offset,
        })
    })
}

fn viewed(reader: &Reader<'_>, view: Object) -> Result<(u64, Buffer), Problem> {
    let word = reader.field(view, Field::JSArrayBufferViewBuffer)?;
    let object = reader
        .object(word)?
        .ok_or_else(|| format!("the buffer of {:#x} is not an object", view.0))?;
    Ok((word, buffer(reader, object)?))
}

pub(super) fn typed_array(
    reader: &Reader<'_>,
    object: Object,
    full: bool,
) -> Result<Kind, Problem> {
    let (element, length, data) = extent(reader, object)?;
    let bytes = shown(
        reader,
        data,
        length.min(SHOWN).saturating_mul(size(element)),
        full,
    )?;
    debug_assert!(
        bytes.len() as u64 <= SHOWN * size(element),
        "the typed array {:#x} shows {} bytes, past {SHOWN} elements",
        object.0,
        bytes.len(),
    );
    Ok(Kind::TypedArray {
        element,
        length,
        bytes,
    })
}

pub(super) fn extent(
    reader: &Reader<'_>,
    object: Object,
) -> Result<(ElementType, u64, u64), Problem> {
    let map = reader.map(object)?;
    let kind = elements_kind(reader, map)?;
    let element = element_type(reader, kind)
        .ok_or_else(|| format!("a typed array has elements kind {kind}"))?;
    let (_, buffer) = viewed(reader, object)?;
    let width = size(element);
    let length = view_range(reader, object, &buffer)?.map_or(0, |range| range.byte_length / width);
    let base = word_field(reader, object, Field::JSTypedArrayBasePointer)?;
    let external = word_field(reader, object, Field::JSTypedArrayExternalPointer)?;
    Ok((element, length, base.wrapping_add(external)))
}

pub(super) fn data_view(
    reader: &Reader<'_>,
    object: Object,
    buffer_value: impl FnOnce(u64) -> Value,
) -> Result<Kind, Problem> {
    let (word, buffer) = viewed(reader, object)?;
    Ok(Kind::DataView {
        view: view_range(reader, object, &buffer)?,
        buffer: buffer_value(word),
    })
}
