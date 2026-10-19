//! Decoding and encoding target integers, floats, addresses, and bit-fields.

use std::sync::Arc;

use gimli::{Reader as _, RunTimeEndian, Value};

use crate::debug_info::VariableRuntime;
use crate::debug_info::dwarf::Reader;
use crate::model::FloatLayout;
use crate::{
    Architecture, BaseType, BaseTypeEncoding, ByteOrder, FloatValue, IntegerValue, ScalarValue,
    TargetDescription, VariableInvalidReason, VariableUnavailableReason, VirtualAddress,
};

use super::evaluate::EvaluateError;
use super::inspect::ScalarDecodeError;

pub(super) fn integer_bit_width(base: &BaseType) -> std::result::Result<u32, Arc<str>> {
    let storage_bits = base
        .byte_size
        .checked_mul(8)
        .ok_or_else(|| Arc::from("integer storage bit width overflows"))?;
    let bits = base.bit_size.unwrap_or(storage_bits);
    if bits == 0 || bits > storage_bits || bits > 128 {
        return Err("integer bit width is outside its storage representation".into());
    }

    u32::try_from(bits).map_err(|_| Arc::from("integer bit width exceeds u32"))
}

pub(super) fn checked_integer_value(
    value: IntegerValue,
    base: &BaseType,
) -> std::result::Result<IntegerValue, Arc<str>> {
    let bits = integer_bit_width(base)?;
    match (base.encoding, value) {
        (
            BaseTypeEncoding::Signed | BaseTypeEncoding::SignedCharacter,
            IntegerValue::Signed(value),
        ) => {
            if !fits_signed(value, bits) {
                return Err("signed enumerator does not fit its representation".into());
            }

            Ok(IntegerValue::Signed(value))
        }

        (
            BaseTypeEncoding::Signed | BaseTypeEncoding::SignedCharacter,
            IntegerValue::Unsigned(value),
        ) => {
            let value = i128::try_from(value)
                .map_err(|_| Arc::from("enumerator does not fit its signed representation"))?;
            checked_integer_value(IntegerValue::Signed(value), base)
        }

        (
            BaseTypeEncoding::Boolean
            | BaseTypeEncoding::Unsigned
            | BaseTypeEncoding::UnsignedCharacter,
            IntegerValue::Unsigned(value),
        ) => {
            if bits < 128 && value >= 1_u128 << bits {
                return Err("unsigned enumerator does not fit its representation".into());
            }

            if matches!(base.encoding, BaseTypeEncoding::Boolean) && value > 1 {
                return Err("boolean enumerator is neither zero nor one".into());
            }

            Ok(IntegerValue::Unsigned(value))
        }

        (
            BaseTypeEncoding::Boolean
            | BaseTypeEncoding::Unsigned
            | BaseTypeEncoding::UnsignedCharacter,
            IntegerValue::Signed(value),
        ) => {
            let value = u128::try_from(value)
                .map_err(|_| Arc::from("negative enumerator has an unsigned representation"))?;
            checked_integer_value(IntegerValue::Unsigned(value), base)
        }
        (BaseTypeEncoding::Floating | BaseTypeEncoding::ComplexFloating, _) => {
            Err("enumerator representation is floating-point".into())
        }
    }
}

pub(super) fn decode_integer_value(
    base: &BaseType,
    bytes: &[u8],
    byte_order: ByteOrder,
) -> std::result::Result<IntegerValue, Arc<str>> {
    let expected = usize::try_from(base.byte_size)
        .map_err(|_| Arc::from("integer byte size does not fit host usize"))?;
    if bytes.len() != expected {
        return Err("integer storage size mismatch".into());
    }

    let bits = integer_bit_width(base)?;
    let raw = unsigned_value(bytes, byte_order).map_err(|reason| Arc::from(reason.to_string()))?
        & low_bits_mask(bits as usize);
    let value = match base.encoding {
        BaseTypeEncoding::Signed | BaseTypeEncoding::SignedCharacter => {
            let signed = if bits == 128 {
                raw.cast_signed()
            } else {
                let shift = 128 - bits;
                (raw << shift).cast_signed() >> shift
            };

            IntegerValue::Signed(signed)
        }

        BaseTypeEncoding::Boolean
        | BaseTypeEncoding::Unsigned
        | BaseTypeEncoding::UnsignedCharacter => IntegerValue::Unsigned(raw),
        BaseTypeEncoding::Floating | BaseTypeEncoding::ComplexFloating => {
            return Err("integer representation is floating-point".into());
        }
    };

    checked_integer_value(value, base)
}

pub(super) fn enumeration_constant(
    value: gimli::AttributeValue<Reader<'_>>,
    base: &BaseType,
    byte_order: ByteOrder,
) -> std::result::Result<IntegerValue, Arc<str>> {
    let signed = matches!(
        base.encoding,
        BaseTypeEncoding::Signed | BaseTypeEncoding::SignedCharacter
    );
    let value = match value {
        gimli::AttributeValue::Data1(value) if signed => {
            IntegerValue::Signed(i128::from(value.cast_signed()))
        }
        gimli::AttributeValue::Data2(value) if signed => {
            IntegerValue::Signed(i128::from(value.cast_signed()))
        }
        gimli::AttributeValue::Data4(value) if signed => {
            IntegerValue::Signed(i128::from(value.cast_signed()))
        }
        gimli::AttributeValue::Data8(value) if signed => {
            IntegerValue::Signed(i128::from(value.cast_signed()))
        }
        gimli::AttributeValue::Data16(value) if signed => IntegerValue::Signed(value.cast_signed()),
        gimli::AttributeValue::Data1(value) => IntegerValue::Unsigned(u128::from(value)),
        gimli::AttributeValue::Data2(value) => IntegerValue::Unsigned(u128::from(value)),
        gimli::AttributeValue::Data4(value) => IntegerValue::Unsigned(u128::from(value)),
        gimli::AttributeValue::Data8(value) | gimli::AttributeValue::Udata(value) => {
            IntegerValue::Unsigned(u128::from(value))
        }
        gimli::AttributeValue::Data16(value) => IntegerValue::Unsigned(value),
        gimli::AttributeValue::Sdata(value) => IntegerValue::Signed(i128::from(value)),
        gimli::AttributeValue::Block(value) => {
            let bytes = value
                .to_slice()
                .map_err(|error| Arc::from(error.to_string()))?;
            return decode_integer_value(base, bytes.as_ref(), byte_order);
        }
        _ => return Err("enumerator constant has an unsupported form".into()),
    };

    checked_integer_value(value, base)
}

pub(super) fn read_uleb128_u128(
    bytes: &[u8],
    cursor: &mut usize,
) -> std::result::Result<u128, Arc<str>> {
    let mut value = 0_u128;
    let mut shift = 0_u32;

    loop {
        let byte = *bytes
            .get(*cursor)
            .ok_or_else(|| Arc::from("truncated unsigned discriminant LEB128"))?;
        *cursor = cursor
            .checked_add(1)
            .ok_or_else(|| Arc::from("discriminant-list cursor overflows"))?;
        let payload = u128::from(byte & 0x7f);
        if shift >= 128 {
            if payload != 0 {
                return Err("unsigned discriminant LEB128 exceeds 128 bits".into());
            }
        } else {
            let available = 128 - shift;
            if available < 7 && payload >= 1_u128 << available {
                return Err("unsigned discriminant LEB128 exceeds 128 bits".into());
            }

            value |= payload << shift;
        }

        if byte & 0x80 == 0 {
            return Ok(value);
        }

        shift = shift
            .checked_add(7)
            .ok_or_else(|| Arc::from("unsigned discriminant LEB128 shift overflows"))?;
        if shift > 133 {
            return Err("unsigned discriminant LEB128 is overlong".into());
        }
    }
}

pub(super) fn read_sleb128_i128(
    bytes: &[u8],
    cursor: &mut usize,
) -> std::result::Result<i128, Arc<str>> {
    let mut value = 0_u128;
    let mut shift = 0_u32;

    loop {
        let byte = *bytes
            .get(*cursor)
            .ok_or_else(|| Arc::from("truncated signed discriminant LEB128"))?;
        *cursor = cursor
            .checked_add(1)
            .ok_or_else(|| Arc::from("discriminant-list cursor overflows"))?;
        let payload = u128::from(byte & 0x7f);
        if shift < 128 {
            let available = 128 - shift;
            value |= (payload & low_bits_mask(available.min(7) as usize)) << shift;
        }

        if byte & 0x80 == 0 {
            let negative = byte & 0x40 != 0;
            if shift >= 128 {
                let canonical = if negative {
                    payload == 0x7f
                } else {
                    payload == 0
                };

                if !canonical {
                    return Err("signed discriminant LEB128 exceeds 128 bits".into());
                }
            } else {
                let consumed = shift + 7;
                if negative && consumed < 128 {
                    value |= u128::MAX << consumed;
                } else if consumed > 128 {
                    let used = 128 - shift;
                    let high = payload >> used;
                    let expected = if negative {
                        low_bits_mask((7 - used) as usize)
                    } else {
                        0
                    };

                    if high != expected {
                        return Err("signed discriminant LEB128 exceeds 128 bits".into());
                    }
                }
            }

            return Ok(value.cast_signed());
        }

        shift = shift
            .checked_add(7)
            .ok_or_else(|| Arc::from("signed discriminant LEB128 shift overflows"))?;
        if shift > 133 {
            return Err("signed discriminant LEB128 is overlong".into());
        }
    }
}

pub(super) fn compare_integer_values(
    left: IntegerValue,
    right: IntegerValue,
) -> std::result::Result<std::cmp::Ordering, Arc<str>> {
    match (left, right) {
        (IntegerValue::Signed(left), IntegerValue::Signed(right)) => Ok(left.cmp(&right)),
        (IntegerValue::Unsigned(left), IntegerValue::Unsigned(right)) => Ok(left.cmp(&right)),
        _ => Err("variant selectors mix signed and unsigned values".into()),
    }
}

pub(super) fn extract_bit_field(
    bytes: &[u8],
    bit_offset: u64,
    bit_size: u64,
    byte_order: ByteOrder,
) -> std::result::Result<u128, Arc<str>> {
    let end = bit_offset
        .checked_add(bit_size)
        .ok_or_else(|| Arc::from("bit-field range overflows"))?;
    let available = u64::try_from(bytes.len())
        .ok()
        .and_then(|length| length.checked_mul(8))
        .ok_or_else(|| Arc::from("record storage size overflows"))?;
    if bit_size == 0 || bit_size > 128 || end > available {
        return Err("bit-field range is outside its containing object".into());
    }

    let mut value = 0_u128;

    for field_bit in 0..bit_size {
        let source = bit_offset + field_bit;
        let byte = usize::try_from(source / 8)
            .ok()
            .and_then(|index| bytes.get(index).copied())
            .ok_or_else(|| Arc::from("bit-field range is outside its containing object"))?;
        let within = source % 8;
        let bit = match byte_order {
            ByteOrder::Little => (byte >> within) & 1,
            ByteOrder::Big => (byte >> (7 - within)) & 1,
        };

        match byte_order {
            ByteOrder::Little => value |= u128::from(bit) << field_bit,
            ByteOrder::Big => value = (value << 1) | u128::from(bit),
        }
    }

    Ok(value)
}

pub(super) fn wrapping_integer_bytes(
    value: u128,
    size: usize,
    target: TargetDescription,
) -> std::result::Result<Arc<[u8]>, EvaluateError> {
    if size == 0 || size > 16 {
        return Err(crate::UnsupportedVariableFeature::ScalarRepresentation.into());
    }

    integer_bytes(value & low_bits_mask(size * 8), size, target)
}

pub(super) fn dwarf_address_bytes(
    value: Value,
    size: usize,
    target: TargetDescription,
) -> std::result::Result<Arc<[u8]>, EvaluateError> {
    let address = match value {
        Value::Generic(value) | Value::U64(value) => value,
        Value::U8(value) => u64::from(value),
        Value::U16(value) => u64::from(value),
        Value::U32(value) => u64::from(value),
        Value::I8(value) => i64::from(value).cast_unsigned(),
        Value::I16(value) => i64::from(value).cast_unsigned(),
        Value::I32(value) => i64::from(value).cast_unsigned(),
        Value::I64(value) => value.cast_unsigned(),
        Value::F32(_) | Value::F64(_) => {
            return Err(EvaluateError::Malformed(
                "floating-point value cannot represent an address".into(),
            ));
        }
    };

    wrapping_integer_bytes(u128::from(address), size, target)
}

pub(super) fn decode_address(
    raw: &[u8],
    byte_size: u64,
    target: TargetDescription,
) -> std::result::Result<VirtualAddress, EvaluateError> {
    let size =
        usize::try_from(byte_size).map_err(|_| VariableUnavailableReason::EvaluationLimit)?;
    if size == 0 || size > 8 {
        return Err(crate::UnsupportedVariableFeature::ScalarRepresentation.into());
    }

    if raw.len() != size {
        return Err(EvaluateError::Malformed(
            "pointer storage size does not match its declared type".into(),
        ));
    }

    let value = unsigned_value(raw, target.byte_order).map_err(EvaluateError::Malformed)?;
    Ok(VirtualAddress::new(u64::try_from(value).map_err(|_| {
        EvaluateError::Malformed("pointer value exceeds 64 bits".into())
    })?))
}

pub(super) fn integer_bytes(
    value: u128,
    size: usize,
    target: TargetDescription,
) -> std::result::Result<Arc<[u8]>, EvaluateError> {
    if size == 0 || size > 16 || (size < 16 && value >= (1_u128 << (size * 8))) {
        return Err(EvaluateError::Malformed(
            "constant value does not fit its scalar type".into(),
        ));
    }

    let bytes = match target.byte_order {
        ByteOrder::Little => value.to_le_bytes()[..size].to_vec(),
        ByteOrder::Big => value.to_be_bytes()[16 - size..].to_vec(),
    };

    Ok(bytes.into())
}

pub(super) fn signed_integer_bytes(
    value: i128,
    size: usize,
    target: TargetDescription,
) -> std::result::Result<Arc<[u8]>, EvaluateError> {
    if size == 0 || size > 16 {
        return Err(crate::UnsupportedVariableFeature::ScalarRepresentation.into());
    }

    let bits = size * 8;
    let signed_bits =
        u32::try_from(bits).map_err(|_| crate::UnsupportedVariableFeature::ScalarRepresentation)?;
    if !fits_signed(value, signed_bits) {
        return Err(EvaluateError::Malformed(
            "signed constant value does not fit its scalar type".into(),
        ));
    }

    integer_bytes(value.cast_unsigned() & low_bits_mask(bits), size, target)
}

/// Whether `value` fits a two's-complement integer `bits` wide.
const fn fits_signed(value: i128, bits: u32) -> bool {
    if bits >= 128 {
        return true;
    }

    let half = 1_i128 << (bits - 1);
    -half <= value && value < half
}

const fn low_bits_mask(bits: usize) -> u128 {
    if bits == 128 {
        u128::MAX
    } else {
        (1_u128 << bits) - 1
    }
}

pub(super) fn register_u64(
    runtime: &mut dyn VariableRuntime,
    register: crate::unwind::DwarfRegister,
    endian: RunTimeEndian,
) -> std::result::Result<u64, EvaluateError> {
    let value = runtime.register(register)?;
    bytes_to_u64(&value.bytes, endian)
}

pub(super) fn bytes_to_u64(
    bytes: &[u8],
    endian: RunTimeEndian,
) -> std::result::Result<u64, EvaluateError> {
    if bytes.len() > 8 {
        return Err(EvaluateError::Malformed(
            "DWARF expression requested more than one word".into(),
        ));
    }

    let mut word = [0_u8; 8];
    match endian {
        RunTimeEndian::Little => word[..bytes.len()].copy_from_slice(bytes),
        RunTimeEndian::Big => word[8 - bytes.len()..].copy_from_slice(bytes),
    }

    Ok(match endian {
        RunTimeEndian::Little => u64::from_le_bytes(word),
        RunTimeEndian::Big => u64::from_be_bytes(word),
    })
}

pub(super) fn decode_scalar(
    type_info: &BaseType,
    bytes: &[u8],
    target: TargetDescription,
) -> std::result::Result<ScalarValue, ScalarDecodeError> {
    let expected = usize::try_from(type_info.byte_size).map_err(|_| {
        ScalarDecodeError::Malformed("scalar byte size does not fit host usize".into())
    })?;
    if bytes.len() != expected {
        return Err(ScalarDecodeError::Malformed(
            "scalar storage size mismatch".into(),
        ));
    }

    match type_info.encoding {
        BaseTypeEncoding::Boolean => {
            let bits = integer_bit_width(type_info).map_err(ScalarDecodeError::Malformed)?;
            let value = unsigned_value(bytes, target.byte_order)
                .map_err(ScalarDecodeError::Malformed)?
                & low_bits_mask(bits as usize);

            match value {
                0 => Ok(ScalarValue::Boolean(false)),
                1 => Ok(ScalarValue::Boolean(true)),
                value => Err(ScalarDecodeError::Invalid(
                    VariableInvalidReason::BooleanRepresentation(value),
                )),
            }
        }
        BaseTypeEncoding::Signed | BaseTypeEncoding::SignedCharacter => {
            let IntegerValue::Signed(value) =
                decode_integer_value(type_info, bytes, target.byte_order)
                    .map_err(ScalarDecodeError::Malformed)?
            else {
                return Err(ScalarDecodeError::Malformed(
                    "signed encoding decoded as an unsigned integer".into(),
                ));
            };

            Ok(ScalarValue::Signed(value))
        }
        BaseTypeEncoding::Unsigned | BaseTypeEncoding::UnsignedCharacter => {
            let IntegerValue::Unsigned(value) =
                decode_integer_value(type_info, bytes, target.byte_order)
                    .map_err(ScalarDecodeError::Malformed)?
            else {
                return Err(ScalarDecodeError::Malformed(
                    "unsigned encoding decoded as a signed integer".into(),
                ));
            };

            Ok(ScalarValue::Unsigned(value))
        }
        BaseTypeEncoding::Floating => {
            decode_float(type_info, bytes, target).map(ScalarValue::Floating)
        }
        BaseTypeEncoding::ComplexFloating => {
            let part = complex_part(type_info);
            let (real, imaginary) = bytes.split_at(bytes.len() / 2);
            Ok(ScalarValue::Complex {
                real: decode_float(&part, real, target)?,
                imaginary: decode_float(&part, imaginary, target)?,
            })
        }
    }
}

/// The float type each part of a complex type is: Go's `complex128` is two
/// `float64`, C's `complex double` two `double`, and Clang's `complex` two
/// floats of its size.
pub(super) fn complex_part(complex: &BaseType) -> BaseType {
    let byte_size = complex.byte_size / 2;
    let name: Arc<str> = match complex.base_name.as_ref() {
        "complex64" => "float32".into(),
        "complex128" => "float64".into(),
        name => name
            .strip_prefix("complex ")
            .or_else(|| name.strip_prefix("_Complex "))
            .unwrap_or(match byte_size {
                4 => "float",
                8 => "double",
                _ => "long double",
            })
            .into(),
    };

    BaseType {
        name: Arc::clone(&name),
        base_name: name,
        encoding: BaseTypeEncoding::Floating,
        byte_size,
        bit_size: None,
    }
}

pub(super) fn unsigned_value(
    bytes: &[u8],
    byte_order: ByteOrder,
) -> std::result::Result<u128, Arc<str>> {
    if bytes.is_empty() || bytes.len() > 16 {
        return Err("unsupported integer storage size".into());
    }

    let mut value = [0_u8; 16];

    match byte_order {
        ByteOrder::Little => value[..bytes.len()].copy_from_slice(bytes),
        ByteOrder::Big => value[16 - bytes.len()..].copy_from_slice(bytes),
    }

    Ok(match byte_order {
        ByteOrder::Little => u128::from_le_bytes(value),
        ByteOrder::Big => u128::from_be_bytes(value),
    })
}

/// Whether a 16-byte float is x87 extended precision, padded, rather than
/// the IEEE binary128 of `__float128` and `f128`; only the name tells them
/// apart.
fn is_x87_extended(type_info: &BaseType, target: TargetDescription) -> bool {
    type_info.byte_size == 16
        && type_info.float_layout() == Some(FloatLayout::X87Extended)
        && target.architecture == Architecture::X86_64
        && target.byte_order == ByteOrder::Little
}

/// How many leading bytes of a scalar's storage hold its value: all of
/// them, but for x87 extended precision, whose last six are padding.
pub(super) fn significant_bytes(type_info: &BaseType, target: TargetDescription) -> u64 {
    if is_x87_extended(type_info, target) {
        10
    } else {
        type_info.byte_size
    }
}

fn decode_float(
    type_info: &BaseType,
    bytes: &[u8],
    target: TargetDescription,
) -> std::result::Result<FloatValue, ScalarDecodeError> {
    let layout = type_info.float_layout();
    let word = || unsigned_value(bytes, target.byte_order).map_err(ScalarDecodeError::Malformed);
    let narrow = |error: &str| ScalarDecodeError::Malformed(error.into());

    match bytes.len() {
        2 if layout == Some(FloatLayout::Binary16) => Ok(FloatValue::Binary16(
            u16::try_from(word()?).map_err(|_| narrow("binary16 value exceeds 16 bits"))?,
        )),
        2 if layout == Some(FloatLayout::BFloat16) => Ok(FloatValue::BFloat16(
            u16::try_from(word()?).map_err(|_| narrow("bfloat16 value exceeds 16 bits"))?,
        )),
        16 if layout == Some(FloatLayout::Binary128) => Ok(FloatValue::Binary128(word()?)),
        4 => Ok(FloatValue::Binary32(
            u32::try_from(word()?).map_err(|_| narrow("binary32 value exceeds 32 bits"))?,
        )),
        8 => Ok(FloatValue::Binary64(
            u64::try_from(word()?).map_err(|_| narrow("binary64 value exceeds 64 bits"))?,
        )),
        16 if is_x87_extended(type_info, target) => {
            let Some((significand, rest)) = bytes.split_first_chunk::<8>() else {
                return Err(narrow("x87 extended significand is truncated"));
            };

            let Some(sign_exponent) = rest.first_chunk::<2>() else {
                return Err(narrow("x87 extended sign and exponent are truncated"));
            };

            Ok(FloatValue::X87Extended {
                significand: u64::from_le_bytes(*significand),
                sign_exponent: u16::from_le_bytes(*sign_exponent),
            })
        }
        _ => Err(ScalarDecodeError::Unavailable(
            crate::UnsupportedVariableFeature::ScalarRepresentation.into(),
        )),
    }
}
