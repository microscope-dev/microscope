use gimli::Value;
use proptest::prelude::*;

use crate::test_strategies::{i128_edges, u64_edges, u128_edges};

use super::super::ConstantValue;
use super::super::evaluate::{dwarf_value_bytes, materialize_constant};
use super::*;

/// What a LEB128 encoding means, from its bits alone: the payload bits of
/// every byte up to the first without a continuation bit, and where that
/// byte ends. `None` if no byte ends the number.
fn leb128_bits(bytes: &[u8]) -> Option<(Vec<bool>, usize)> {
    let end = bytes.iter().position(|byte| byte & 0x80 == 0)? + 1;
    let bits = bytes[..end]
        .iter()
        .flat_map(|byte| (0..7).map(move |bit| byte >> bit & 1 == 1))
        .collect();

    Some((bits, end))
}

/// The longest encoding the readers take: a 128-bit number's 19 bytes and
/// one byte of padding.
const LONGEST_LEB128: usize = 20;

fn reference_uleb128(bytes: &[u8]) -> Option<(u128, usize)> {
    let (bits, end) = leb128_bits(bytes)?;
    if end > LONGEST_LEB128 || bits.iter().skip(128).any(|bit| *bit) {
        return None;
    }

    let value = bits
        .iter()
        .take(128)
        .enumerate()
        .fold(0_u128, |value, (index, bit)| {
            value | u128::from(*bit) << index
        });

    Some((value, end))
}

fn reference_sleb128(bytes: &[u8]) -> Option<(i128, usize)> {
    let (bits, end) = leb128_bits(bytes)?;
    let sign = *bits.last()?;

    // The number fits when every bit from bit 127 up repeats its sign.
    if end > LONGEST_LEB128 || bits.iter().skip(127).any(|bit| *bit != sign) {
        return None;
    }

    let value = (0..128)
        .map(|index| bits.get(index).copied().unwrap_or(sign))
        .enumerate()
        .fold(0_u128, |value, (index, bit)| {
            value | u128::from(bit) << index
        });

    Some((value.cast_signed(), end))
}

fn encode_uleb128(mut value: u128) -> Vec<u8> {
    let mut bytes = Vec::new();
    loop {
        let byte = (value & 0x7f) as u8;
        value >>= 7;
        if value == 0 {
            bytes.push(byte);
            return bytes;
        }

        bytes.push(byte | 0x80);
    }
}

fn encode_sleb128(mut value: i128) -> Vec<u8> {
    let mut bytes = Vec::new();
    loop {
        let byte = (value & 0x7f) as u8;
        value >>= 7;
        if (value == 0 && byte & 0x40 == 0) || (value == -1 && byte & 0x40 != 0) {
            bytes.push(byte);
            return bytes;
        }

        bytes.push(byte | 0x80);
    }
}

/// Turns a minimal encoding into a damaged or padded one: `padding` more
/// bytes that repeat the sign, then `flips` xored into the payload of the
/// last two bytes, then one byte that follows the number.
fn reshape(mut bytes: Vec<u8>, padding: usize, flips: [u8; 2], fill: u8, after: u8) -> Vec<u8> {
    for _ in 0..padding {
        let last = bytes.len() - 1;
        bytes[last] |= 0x80;
        bytes.push(fill);
    }

    let length = bytes.len();
    for (byte, flip) in bytes[length.saturating_sub(2)..].iter_mut().zip(flips) {
        *byte ^= flip & 0x7f;
    }

    bytes.push(after);
    bytes
}

/// Values near the edges of 128 bits, where a reader can lose a bit.
fn i128_leb128_values() -> impl Strategy<Value = i128> {
    prop_oneof![
        1 => prop::sample::select(vec![
            i128::MIN,
            i128::MAX,
            1 << 126,
            -(1 << 126),
            (1 << 126) - 1,
            -(1 << 126) - 1,
        ]),
        3 => i128_edges(),
    ]
}

fn flips() -> impl Strategy<Value = [u8; 2]> {
    prop_oneof![2 => Just([0, 0]), 1 => any::<[u8; 2]>()]
}

#[test]
fn a_signed_leb128_past_128_bits_is_refused() {
    // +2^127 and a negative number below -2^127, both one bit too wide.
    let mut too_large = vec![0x80; 18];
    too_large.extend([0x82, 0x00]);
    assert!(read_sleb128_i128(&too_large, &mut 0).is_err());

    let mut too_small = vec![0x80; 18];
    too_small.extend([0x81, 0x7f]);
    assert!(read_sleb128_i128(&too_small, &mut 0).is_err());

    // -2^128, which the property found read as 0.
    let mut far_too_small = vec![0x80; 18];
    far_too_small.extend([0xfc, 0x7f]);
    assert!(read_sleb128_i128(&far_too_small, &mut 0).is_err());
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(1024))]

    /// A signed LEB128 number is read exactly when its value fits 128 bits
    /// and its encoding is no longer than 20 bytes, and the reader stops
    /// after its last byte.
    #[test]
    fn signed_leb128_reads_exactly_the_numbers_that_fit(
        value in i128_leb128_values(),
        padding in 0_usize..4,
        flips in flips(),
        after in any::<u8>(),
    ) {
        let fill = if value < 0 { 0x7f } else { 0x00 };
        let bytes = reshape(encode_sleb128(value), padding, flips, fill, after);
        let mut cursor = 0;
        let read = read_sleb128_i128(&bytes, &mut cursor).ok().map(|value| (value, cursor));
        prop_assert_eq!(read, reference_sleb128(&bytes), "bytes {:02x?}", bytes);
    }

    #[test]
    fn unsigned_leb128_reads_exactly_the_numbers_that_fit(
        value in u128_edges(),
        padding in 0_usize..4,
        flips in flips(),
        after in any::<u8>(),
    ) {
        let bytes = reshape(encode_uleb128(value), padding, flips, 0x00, after);
        let mut cursor = 0;
        let read = read_uleb128_u128(&bytes, &mut cursor).ok().map(|value| (value, cursor));
        prop_assert_eq!(read, reference_uleb128(&bytes), "bytes {:02x?}", bytes);
    }

    /// A number cut before its last byte is never read.
    #[test]
    fn a_truncated_leb128_is_refused(value in i128_edges(), cut in any::<prop::sample::Index>()) {
        let bytes = encode_sleb128(value);
        let prefix = &bytes[..cut.index(bytes.len())];
        prop_assert!(read_sleb128_i128(prefix, &mut 0).is_err());
        prop_assert!(read_uleb128_u128(prefix, &mut 0).is_err());
    }
}

fn target(byte_order: ByteOrder) -> TargetDescription {
    TargetDescription {
        architecture: Architecture::X86_64,
        byte_order,
        pointer_width: crate::PointerWidth::Bits64,
    }
}

fn integer_type(encoding: BaseTypeEncoding, byte_size: u64, bit_size: Option<u64>) -> BaseType {
    BaseType {
        name: "test".into(),
        base_name: "test".into(),
        encoding,
        byte_size,
        bit_size,
    }
}

/// The low `bits` bits of `value`.
fn pattern_of(value: u128, bits: u32) -> u128 {
    if bits == 128 {
        value
    } else {
        value & ((1 << bits) - 1)
    }
}

/// `pattern`, `bits` wide, read as two's complement.
fn signed_pattern(pattern: u128, bits: u32) -> i128 {
    if bits < 128 && pattern >> (bits - 1) == 1 {
        (pattern | !((1 << bits) - 1)).cast_signed()
    } else {
        pattern.cast_signed()
    }
}

/// `size` bytes of `pattern` in `byte_order`, one at a time.
fn laid_out(pattern: u128, size: usize, byte_order: ByteOrder) -> Vec<u8> {
    let mut bytes = (0..size)
        .map(|index| (pattern >> (8 * index)) as u8)
        .collect::<Vec<_>>();
    if byte_order == ByteOrder::Big {
        bytes.reverse();
    }

    bytes
}

/// Whether `value` lies in the range of a `bits`-wide integer, signed or
/// not.
fn in_range(value: IntegerValue, signed: bool, bits: u32) -> bool {
    match (value, signed) {
        (IntegerValue::Signed(value), true) => {
            bits == 128 || (-(1_i128 << (bits - 1))..1_i128 << (bits - 1)).contains(&value)
        }
        (IntegerValue::Unsigned(value), true) => value < 1_u128 << (bits - 1),
        (IntegerValue::Signed(value), false) => {
            value >= 0 && (bits == 128 || value.cast_unsigned() < 1_u128 << bits)
        }
        (IntegerValue::Unsigned(value), false) => bits == 128 || value < 1_u128 << bits,
    }
}

/// Raw 128-bit values near the edges of an integer `bits` wide, signed and
/// not, and Clang's 64-bit sign extensions of narrow patterns.
fn values_near(bits: u32) -> impl Strategy<Value = u128> {
    let edge =
        |offset: i128| move |base: u128| base.cast_signed().wrapping_add(offset).cast_unsigned();
    let unsigned_top = if bits == 128 { 0 } else { 1_u128 << bits };
    let signed_top = 1_u128 << (bits - 1);
    let extended = move |pattern: u128| {
        let pattern = pattern_of(pattern, bits) | signed_top;
        if bits < 64 {
            pattern | (u128::from(u64::MAX) & !((1 << bits) - 1))
        } else {
            pattern
        }
    };

    prop_oneof![
        Just(unsigned_top).prop_map(edge(0)),
        Just(unsigned_top).prop_map(edge(-1)),
        Just(signed_top).prop_map(edge(0)),
        Just(signed_top).prop_map(edge(-1)),
        Just(signed_top.wrapping_neg()).prop_map(edge(0)),
        Just(signed_top.wrapping_neg()).prop_map(edge(-1)),
        any::<u128>().prop_map(extended),
        any::<u128>().prop_map(move |value| pattern_of(value, bits)),
        u128_edges(),
    ]
}

fn byte_order() -> impl Strategy<Value = ByteOrder> {
    prop_oneof![Just(ByteOrder::Little), Just(ByteOrder::Big)]
}

/// A constant of a type `size` bytes wide: an integer near the type's
/// edges, in any form, or bytes of about its size.
fn constant(size: usize) -> impl Strategy<Value = ConstantValue> {
    let bits = (size * 8) as u32;
    prop_oneof![
        3 => values_near(bits).prop_map(ConstantValue::Unsigned),
        2 => values_near(bits).prop_map(ConstantValue::Fixed),
        3 => values_near(bits).prop_map(|value| ConstantValue::Signed(value.cast_signed())),
        1 => prop::collection::vec(any::<u8>(), size - 1..size + 2)
            .prop_map(|bytes| ConstantValue::Bytes(bytes.into())),
    ]
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(1024))]

    /// A constant is materialized exactly when it fits its type, as the
    /// type's bit pattern, and decoding that pattern gives the value back.
    /// An unsigned constant fits when it is below 2^bits, or, for a type
    /// narrower than 64 bits, when it is the 64-bit sign extension of a
    /// pattern whose top bit is set, as Clang writes it.
    #[test]
    fn constants_materialize_exactly_when_they_fit(
        (size, constant) in (1_usize..=16).prop_flat_map(|size| (Just(size), constant(size))),
        signed in any::<bool>(),
        byte_order in byte_order(),
    ) {
        let bits = (size * 8) as u32;
        let expected = match &constant {
            ConstantValue::Unsigned(value) | ConstantValue::Fixed(value) => {
                let pattern = pattern_of(*value, bits);
                let fits = in_range(IntegerValue::Unsigned(*value), false, bits);
                let extension = bits < 64
                    && pattern >> (bits - 1) == 1
                    && *value == signed_pattern(pattern, bits).cast_unsigned() & u128::from(u64::MAX);
                (fits || extension).then(|| laid_out(pattern, size, byte_order))
            }
            ConstantValue::Signed(value) => in_range(IntegerValue::Signed(*value), true, bits)
                .then(|| laid_out(pattern_of(value.cast_unsigned(), bits), size, byte_order)),
            ConstantValue::Bytes(bytes) => (bytes.len() == size).then(|| bytes.to_vec()),
        };

        let materialized = materialize_constant(&constant, size, target(byte_order))
            .ok()
            .map(|bytes| bytes.to_vec());
        prop_assert_eq!(&materialized, &expected);

        // Whatever was materialized reads back as its pattern, and a
        // signed constant read as signed is the constant itself.
        if let Some(bytes) = expected {
            let encoding = if signed { BaseTypeEncoding::Signed } else { BaseTypeEncoding::Unsigned };
            let decoded =
                decode_integer_value(&integer_type(encoding, size as u64, None), &bytes, byte_order);
            let pattern = laid_out_pattern(&bytes, byte_order);
            let value = if signed {
                IntegerValue::Signed(signed_pattern(pattern, bits))
            } else {
                IntegerValue::Unsigned(pattern)
            };
            prop_assert_eq!(decoded.ok(), Some(value));

            if let (ConstantValue::Signed(constant), true) = (&constant, signed) {
                prop_assert_eq!(value, IntegerValue::Signed(*constant));
            }
        }
    }

    /// Decoding keeps a bit-field's low `bit_size` bits, sign-extended for
    /// a signed type, and checking takes exactly the values in the type's
    /// range, in the type's signedness.
    #[test]
    fn integers_decode_and_check_against_their_range(
        (size, bits, raw) in (1_u64..=16).prop_flat_map(|size| {
            (Just(size), 1..=size as u32 * 8, prop::collection::vec(any::<u8>(), size as usize))
        }),
        bit_size_given in any::<bool>(),
        encoding in prop::sample::select(vec![
            BaseTypeEncoding::Signed,
            BaseTypeEncoding::SignedCharacter,
            BaseTypeEncoding::Unsigned,
            BaseTypeEncoding::UnsignedCharacter,
            BaseTypeEncoding::Boolean,
        ]),
        byte_order in byte_order(),
        candidate in (any::<bool>(), u128_edges()),
    ) {
        let bits = if bit_size_given { bits } else { size as u32 * 8 };
        let base = integer_type(encoding, size, bit_size_given.then_some(u64::from(bits)));
        let signed = matches!(encoding, BaseTypeEncoding::Signed | BaseTypeEncoding::SignedCharacter);
        let boolean = encoding == BaseTypeEncoding::Boolean;

        let within = |value: IntegerValue| {
            in_range(value, signed, bits)
                && !(boolean && value != IntegerValue::Unsigned(0) && value != IntegerValue::Unsigned(1)
                    && value != IntegerValue::Signed(0) && value != IntegerValue::Signed(1))
        };
        let normalized = |value: IntegerValue| match value {
            IntegerValue::Signed(value) if !signed => IntegerValue::Unsigned(value.cast_unsigned()),
            IntegerValue::Unsigned(value) if signed => IntegerValue::Signed(value.cast_signed()),
            value => value,
        };

        let pattern = pattern_of(laid_out_pattern(&raw, byte_order), bits);
        let value = if signed {
            IntegerValue::Signed(signed_pattern(pattern, bits))
        } else {
            IntegerValue::Unsigned(pattern)
        };
        prop_assert_eq!(
            decode_integer_value(&base, &raw, byte_order).ok(),
            within(value).then_some(value)
        );

        let (negative, magnitude) = candidate;
        let candidate = if negative {
            IntegerValue::Signed(magnitude.cast_signed())
        } else {
            IntegerValue::Unsigned(magnitude)
        };
        prop_assert_eq!(
            checked_integer_value(candidate, &base).ok(),
            within(candidate).then(|| normalized(candidate))
        );
    }

    /// A value an expression computed is stored as its low bytes, after
    /// sign extension of a signed value, and a boolean keeps only its low
    /// bit.
    #[test]
    fn computed_values_wrap_to_their_type(
        value in prop_oneof![
            any::<u8>().prop_map(Value::U8),
            any::<u16>().prop_map(Value::U16),
            any::<u32>().prop_map(Value::U32),
            any::<u64>().prop_map(Value::U64),
            u64_edges().prop_map(Value::Generic),
            any::<i8>().prop_map(Value::I8),
            any::<i16>().prop_map(Value::I16),
            any::<i32>().prop_map(Value::I32),
            u64_edges().prop_map(|value| Value::I64(value.cast_signed())),
        ],
        size in 1_usize..=16,
        boolean in any::<bool>(),
        byte_order in byte_order(),
    ) {
        let wide = match value {
            Value::Generic(value) | Value::U64(value) => u128::from(value),
            Value::U8(value) => u128::from(value),
            Value::U16(value) => u128::from(value),
            Value::U32(value) => u128::from(value),
            Value::I8(value) => i128::from(value).cast_unsigned(),
            Value::I16(value) => i128::from(value).cast_unsigned(),
            Value::I32(value) => i128::from(value).cast_unsigned(),
            Value::I64(value) => i128::from(value).cast_unsigned(),
            Value::F32(_) | Value::F64(_) => unreachable!("only integers are generated"),
        };

        let wide = if boolean { wide & 1 } else { wide };
        let encoding = if boolean { BaseTypeEncoding::Boolean } else { BaseTypeEncoding::Unsigned };
        let stored = dwarf_value_bytes(value, &integer_type(encoding, size as u64, None), target(byte_order))
            .map(|bytes| bytes.to_vec());
        prop_assert_eq!(stored.ok(), Some(laid_out(wide, size, byte_order)));
    }
}

/// The integer `bytes` hold in `byte_order`, read one byte at a time.
fn laid_out_pattern(bytes: &[u8], byte_order: ByteOrder) -> u128 {
    let mut bytes = bytes.to_vec();
    if byte_order == ByteOrder::Big {
        bytes.reverse();
    }

    bytes.iter().enumerate().fold(0, |value, (index, byte)| {
        value | u128::from(*byte) << (8 * index)
    })
}
