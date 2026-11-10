//! The numbers checked against native Rust arithmetic wherever it is exact:
//! exhaustively over small domains, at boundaries, and by property.

#![allow(
    clippy::cast_lossless,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss,
    reason = "the tests compare against Rust's own casts"
)]

use std::cmp::Ordering;

use proptest::prelude::*;

use super::*;

fn exact(value: i128) -> Exact {
    Exact::from(value)
}

fn int(width: u8, signed: bool) -> IntType {
    IntType::new(width, signed).expect("valid width")
}

/// Every operator on exact integers agrees with `i128` wherever `i128`
/// holds the operands and the result.
#[test]
fn exact_arithmetic_matches_i128_for_every_small_pair() {
    for left in -260..=260_i128 {
        for right in -260..=260_i128 {
            let (a, b) = (exact(left), exact(right));
            assert_eq!(a.add(b), Ok(exact(left + right)));
            assert_eq!(a.sub(b), Ok(exact(left - right)));
            assert_eq!(a.mul(b), Ok(exact(left * right)));
            assert_eq!(a.cmp(&b), left.cmp(&right));
            if right == 0 {
                assert_eq!(a.div(b), Err(NumberError::DivisionByZero));
                assert_eq!(a.rem(b), Err(NumberError::DivisionByZero));
            } else {
                assert_eq!(a.div(b), Ok(exact(left / right)), "{left} / {right}");
                assert_eq!(a.rem(b), Ok(exact(left % right)), "{left} % {right}");
            }

            assert_eq!(a.bitwise(BitOperator::And, b), Ok(exact(left & right)));
            assert_eq!(a.bitwise(BitOperator::Or, b), Ok(exact(left | right)));
            assert_eq!(a.bitwise(BitOperator::Xor, b), Ok(exact(left ^ right)));
        }

        assert_eq!(exact(left).neg(), Ok(exact(-left)));
        assert_eq!(exact(left).not(), Ok(exact(!left)));
        for amount in 0..=130_u32 {
            let shift = exact(i128::from(amount));
            let expected_right = if amount >= 127 {
                left >> 127
            } else {
                left >> amount
            };

            assert_eq!(exact(left).shr(shift), Ok(exact(expected_right)));
            let expected_left = 2_i128
                .checked_pow(amount)
                .and_then(|scale| left.checked_mul(scale));
            if let Some(expected) = expected_left {
                assert_eq!(exact(left).shl(shift), Ok(exact(expected)));
            }
        }

        assert_eq!(
            exact(left).shl(exact(-1)),
            Err(NumberError::ShiftAmount),
            "negative shifts are refused"
        );
    }
}

/// The edges of [−2^127, 2^128 − 1], which `i128` alone cannot check.
#[test]
fn exact_results_cover_both_signednesses_and_nothing_more() {
    let max = Exact::from(u128::MAX);
    let min = exact(i128::MIN);
    let one = exact(1);
    assert_eq!(max.add(one), Err(NumberError::OutOfRange));
    assert_eq!(min.sub(one), Err(NumberError::OutOfRange));
    assert_eq!(max.sub(max), Ok(Exact::ZERO));
    assert_eq!(Exact::ZERO.sub(max), Err(NumberError::OutOfRange));
    assert_eq!(max.add(min), Ok(exact(i128::MAX)));
    assert_eq!(min.neg(), Ok(Exact::from(1_u128 << 127)));
    assert_eq!(min.div(exact(-1)), Ok(Exact::from(1_u128 << 127)));
    assert_eq!(min.mul(exact(-1)), Ok(Exact::from(1_u128 << 127)));
    assert_eq!(min.mul(exact(-2)), Err(NumberError::OutOfRange));
    assert_eq!(min.mul(exact(2)), Err(NumberError::OutOfRange));
    assert_eq!(max.mul(max), Err(NumberError::OutOfRange));
    assert_eq!(max.neg(), Err(NumberError::OutOfRange));
    assert_eq!(max.not(), Err(NumberError::OutOfRange));
    assert_eq!(min.not(), Ok(exact(i128::MAX)));
    assert_eq!(min.shr(exact(1000)), Ok(exact(-1)));
    assert_eq!(max.shr(exact(1000)), Ok(Exact::ZERO));
    assert_eq!(one.shl(exact(127)), Ok(Exact::from(1_u128 << 127)));
    assert_eq!(exact(-1).shl(exact(127)), Ok(min));
    assert_eq!(exact(-1).shl(exact(128)), Err(NumberError::OutOfRange));
    assert_eq!(one.shl(exact(128)), Err(NumberError::OutOfRange));
    assert_eq!(Exact::ZERO.shl(Exact::from(u128::MAX)), Ok(Exact::ZERO));
    assert_eq!(
        max.bitwise(BitOperator::And, exact(-1)),
        Ok(max),
        "-1 is all ones"
    );
    assert_eq!(min.bitwise(BitOperator::Or, max), Ok(exact(-1)));
    assert_eq!(
        min.bitwise(BitOperator::Xor, max),
        Err(NumberError::OutOfRange)
    );
    assert_eq!(min.to_i128(), Some(i128::MIN));
    assert_eq!(max.to_i128(), None);
    assert_eq!(max.to_u128(), Some(u128::MAX));
    assert_eq!(exact(-1).to_u128(), None);
    assert_eq!(exact(0).neg(), Ok(Exact::ZERO));
    assert!(!exact(0).neg().unwrap().negative, "zero is never negative");
}

/// Typed bit operations over every pair of 8-bit patterns agree with
/// native `u8` and `i8` operations after conversion to the common type.
#[test]
fn typed_bit_operations_match_native_8_bit_operations() {
    let types = [int(8, true), int(8, false)];
    for left_type in types {
        for right_type in types {
            let common = left_type.common(right_type);
            assert_eq!(
                common.is_signed(),
                left_type.is_signed() && right_type.is_signed()
            );
            for left in 0..=255_u8 {
                for right in 0..=255_u8 {
                    let a = Integer::Typed(Bits::from_raw(left_type, u128::from(left)));
                    let b = Integer::Typed(Bits::from_raw(right_type, u128::from(right)));
                    for (operator, expected) in [
                        (BitOperator::And, left & right),
                        (BitOperator::Or, left | right),
                        (BitOperator::Xor, left ^ right),
                    ] {
                        let result = a.bitwise(operator, b).expect("typed operands combine");
                        assert_eq!(
                            result,
                            Integer::Typed(Bits::from_raw(common, u128::from(expected)))
                        );
                    }
                }
            }
        }
    }
}

#[test]
fn typed_shifts_match_native_shifts_and_refuse_bad_amounts() {
    for raw in 0..=255_u8 {
        let unsigned = Bits::from_raw(int(8, false), u128::from(raw));
        let signed = Bits::from_raw(int(8, true), u128::from(raw));
        for amount in 0..8_u32 {
            let shift = exact(i128::from(amount));
            assert_eq!(
                unsigned.shl(shift).unwrap().raw(),
                u128::from(raw << amount)
            );
            assert_eq!(
                unsigned.shr(shift).unwrap().raw(),
                u128::from(raw >> amount)
            );
            let signed_raw = raw.cast_signed();
            assert_eq!(
                signed.shl(shift).unwrap().value(),
                exact(i128::from(signed_raw << amount))
            );
            assert_eq!(
                signed.shr(shift).unwrap().value(),
                exact(i128::from(signed_raw >> amount))
            );
        }

        for amount in [-1, 8, 9, 1 << 40] {
            assert_eq!(unsigned.shl(exact(amount)), Err(NumberError::ShiftAmount));
            assert_eq!(signed.shr(exact(amount)), Err(NumberError::ShiftAmount));
        }
    }

    let wide = Bits::from_raw(int(128, true), u128::MAX);
    assert_eq!(wide.shr(exact(127)).unwrap().value(), exact(-1));
    assert_eq!(wide.shl(exact(127)).unwrap().value(), exact(i128::MIN));
    assert_eq!(wide.shl(exact(128)), Err(NumberError::ShiftAmount));
}

#[test]
fn an_exact_operand_must_fit_the_typed_width_either_way() {
    for typed in [int(8, true), int(8, false)] {
        let ones = Integer::Typed(Bits::from_raw(typed, 0xff));
        for value in -300..=300_i128 {
            let result = ones.bitwise(BitOperator::And, Integer::Exact(exact(value)));
            if (-128..=255).contains(&value) {
                let expected = Bits::from_raw(typed, value.cast_unsigned() & 0xff);
                assert_eq!(result, Ok(Integer::Typed(expected)));
            } else {
                assert_eq!(result, Err(NumberError::DoesNotFit(typed)));
            }

            // The exact operand on the left, or-ed with all ones, is all
            // ones of the typed width when it fits.
            let flipped = Integer::Exact(exact(value)).bitwise(BitOperator::Or, ones);
            if (-128..=255).contains(&value) {
                assert_eq!(flipped, Ok(Integer::Typed(Bits::from_raw(typed, 0xff))));
            } else {
                assert_eq!(flipped, Err(NumberError::DoesNotFit(typed)));
            }
        }
    }

    assert_eq!(
        Integer::Exact(exact(-1)).not(),
        Ok(Integer::Exact(Exact::ZERO))
    );
}

/// Every 16-bit-range value truncated to every width, against `rem_euclid`,
/// and against Rust's own casts at the native widths.
#[test]
fn truncation_to_every_width_matches_modular_arithmetic() {
    for value in -32_768..=65_535_i128 {
        for width in 1..=MAX_WIDTH {
            for signed in [false, true] {
                let ty = int(width, signed);
                let bits = Bits::truncate(ty, exact(value));
                let expected = if width >= 127 {
                    if signed || value >= 0 {
                        exact(value)
                    } else if width == 128 {
                        Exact::from(value.cast_unsigned())
                    } else {
                        Exact::from(value.cast_unsigned() & (u128::MAX >> 1))
                    }
                } else {
                    let modulus = 1_i128 << width;
                    let low = value.rem_euclid(modulus);
                    if signed && low >= modulus / 2 {
                        exact(low - modulus)
                    } else {
                        exact(low)
                    }
                };

                assert_eq!(bits.value(), expected, "{value} as {ty:?}");
                assert_eq!(
                    Bits::exactly(ty, exact(value)).is_ok(),
                    ty.contains(exact(value))
                );
            }
        }

        let natives = [
            (int(8, true), i128::from(value as i8)),
            (int(8, false), i128::from(value as u8)),
            (int(16, true), i128::from(value as i16)),
            (int(32, false), i128::from(value as u32)),
            (int(64, false), i128::from(value as u64)),
        ];
        for (ty, expected) in natives {
            assert_eq!(Bits::truncate(ty, exact(value)).value(), exact(expected));
        }
    }
}

#[test]
fn type_bounds_hold_at_every_width() {
    assert_eq!(int(1, true).min(), exact(-1));
    assert_eq!(int(1, true).max(), exact(0));
    assert_eq!(int(1, false).max(), exact(1));
    assert_eq!(int(128, true).min(), exact(i128::MIN));
    assert_eq!(int(128, false).max(), Exact::from(u128::MAX));
    assert!(IntType::new(0, false).is_none());
    assert!(IntType::new(129, true).is_none());
    for width in 1..=MAX_WIDTH {
        let signed = int(width, true);
        let unsigned = int(width, false);
        assert_eq!(Bits::truncate(signed, signed.max()).value(), signed.max());
        assert_eq!(Bits::truncate(signed, signed.min()).value(), signed.min());
        assert_eq!(
            Bits::truncate(unsigned, unsigned.max()).value(),
            unsigned.max()
        );
        assert_eq!(
            Bits::truncate(signed, signed.max().add(exact(1)).unwrap()).value(),
            signed.min(),
            "one past the maximum wraps to the minimum"
        );
    }
}

/// The values every float-to-integer conversion must get right.
fn special_f64s() -> Vec<f64> {
    let mut values = vec![
        0.0,
        -0.0,
        0.5,
        -0.5,
        1.0,
        -1.0,
        1.5,
        -1.5,
        2.9,
        -2.9,
        f64::MIN_POSITIVE,
        -f64::MIN_POSITIVE,
        f64::from_bits(1),
        f64::MAX,
        f64::MIN,
        f64::INFINITY,
        f64::NEG_INFINITY,
        f64::EPSILON,
    ];
    for exponent in [7, 8, 15, 16, 31, 32, 53, 63, 64, 127, 128] {
        let power = 2_f64.powi(exponent);
        for value in [power, -power] {
            values.extend([value, value.next_up(), value.next_down()]);
        }
    }

    values
}

/// Converting to each native integer type agrees with Rust's saturating
/// `as`, for every special value in both binary formats.
#[test]
fn floats_convert_to_integers_like_saturating_native_casts() {
    macro_rules! check_native {
        ($float:expr, $value:expr, $($native:ty),*) => {$(
            let ty = int(<$native>::BITS as u8, <$native>::MIN != 0);
            let native = $value as $native;
            let expected = if <$native>::MIN == 0 { Exact::from(native as u128) } else { Exact::from(native as i128) };
            assert_eq!($float.to_int(ty).map(Bits::value), Ok(expected), "{} as {}", $value, stringify!($native));
        )*};
    }

    for value in special_f64s() {
        let double = Float::from_f64(value);
        check_native!(
            double, value, i8, u8, i16, u16, i32, u32, i64, u64, i128, u128
        );
        let single_value = value as f32;
        let single = Float::from_f32(single_value);
        check_native!(
            single,
            single_value,
            i8,
            u8,
            i16,
            u16,
            i32,
            u32,
            i64,
            u64,
            i128,
            u128
        );
        let extended = double.convert(FloatFormat::X87Extended);
        check_native!(extended, value, i8, u8, i32, u64, i128, u128);
    }

    for format in [
        FloatFormat::Binary32,
        FloatFormat::Binary64,
        FloatFormat::X87Extended,
    ] {
        let nan = Float::from_f64(f64::NAN).convert(format);
        assert_eq!(nan.to_int(int(32, true)), Err(NumberError::NotANumber));
    }

    let i1 = int(1, true);
    assert_eq!(
        Float::from_f64(-7.0).to_int(i1).map(Bits::value),
        Ok(exact(-1))
    );
    assert_eq!(
        Float::from_f64(7.0).to_int(i1).map(Bits::value),
        Ok(exact(0))
    );
}

#[test]
fn floats_compare_with_integers_exactly() {
    let samples: Vec<i128> = (-70..=70)
        .chain([
            1 << 53,
            (1 << 53) + 1,
            -(1 << 53) - 1,
            i128::from(i64::MAX),
            i128::MIN,
            i128::MAX,
        ])
        .collect();
    for value in special_f64s().into_iter().chain([0.25, 70.5, -70.5]) {
        for &integer in &samples {
            // The comparison a reference computes without rounding: the
            // integer converted only when f64 holds it exactly.
            let converted = integer as f64;
            #[allow(
                clippy::cast_possible_truncation,
                reason = "the round trip checks exactness"
            )]
            let holds = converted.abs() < 2_f64.powi(127) && converted as i128 == integer;
            if !holds {
                continue;
            }

            assert_eq!(
                Float::from_f64(value).compare_exact(exact(integer)),
                value.partial_cmp(&converted),
                "{value} vs {integer}"
            );
        }
    }

    let above = Float::from_f64(9_007_199_254_740_992.0);
    assert_eq!(
        above.compare_exact(exact(9_007_199_254_740_993)),
        Some(Ordering::Less)
    );
    let huge = Float::from_f64(2_f64.powi(128));
    assert_eq!(
        huge.compare_exact(Exact::from(u128::MAX)),
        Some(Ordering::Greater)
    );
    let tiny = Float::from_f64(-(2_f64.powi(127)));
    assert_eq!(tiny.compare_exact(exact(i128::MIN)), Some(Ordering::Equal));
    assert_eq!(Float::from_f64(f64::NAN).compare_exact(Exact::ZERO), None);
    assert_eq!(
        Float::from_f64(-0.0).compare_exact(Exact::ZERO),
        Some(Ordering::Equal)
    );
}

/// Binary32 and binary64 arithmetic is IEEE arithmetic, as the host's is.
#[test]
fn float_arithmetic_matches_host_ieee_arithmetic() {
    let values: Vec<f64> = special_f64s()
        .into_iter()
        .chain([f64::NAN, 0.1, 0.2, 3.0, -7.25])
        .collect();
    let same = |left: f64, right: f64| {
        (left.is_nan() && right.is_nan()) || left.to_bits() == right.to_bits()
    };

    for &left in &values {
        for &right in &values {
            for (operator, expected) in [
                (FloatOperator::Add, left + right),
                (FloatOperator::Sub, left - right),
                (FloatOperator::Mul, left * right),
                (FloatOperator::Div, left / right),
                (FloatOperator::Rem, left % right),
            ] {
                let FloatValue::Binary64(bits) =
                    Float::binary(operator, Float::from_f64(left), Float::from_f64(right))
                        .to_value()
                else {
                    panic!("binary64 operands compute in binary64");
                };

                assert!(
                    same(f64::from_bits(bits), expected),
                    "{left} {operator:?} {right}"
                );
            }

            assert_eq!(
                Float::from_f64(left).compare(Float::from_f64(right)),
                left.partial_cmp(&right)
            );
            let (narrow_left, narrow_right) = (left as f32, right as f32);
            let FloatValue::Binary32(bits) = Float::binary(
                FloatOperator::Mul,
                Float::from_f32(narrow_left),
                Float::from_f32(narrow_right),
            )
            .to_value() else {
                panic!("binary32 operands compute in binary32");
            };

            let product = narrow_left * narrow_right;
            assert!(
                (product.is_nan() && f32::from_bits(bits).is_nan()) || bits == product.to_bits()
            );
        }
    }
}

#[test]
fn mixed_float_formats_compute_in_the_widest() {
    let single = Float::from_f32(1.5);
    let double = Float::from_f64(10.0);
    let sum = Float::binary(FloatOperator::Add, single, double);
    assert_eq!(sum.to_value(), FloatValue::Binary64(11.5_f64.to_bits()));
    let extended = Float::from_f64(1.25).convert(FloatFormat::X87Extended);
    let product = Float::binary(
        FloatOperator::Mul,
        extended,
        Float::from_exact(exact(2), FloatFormat::Binary64),
    );
    assert_eq!(product.format(), FloatFormat::X87Extended);
    assert_eq!(product.compare_exact(Exact::ZERO), Some(Ordering::Greater));
    assert_eq!(
        Float::from_value(product.to_value()),
        product,
        "x87 bits round-trip"
    );
    assert_eq!(product.to_string(), "2.5");

    // 2^64 + 1 needs 65 significant bits: x87 rounds it, f64 more so.
    let big = Exact::from((1_u128 << 64) + 1);
    let rounded = Float::from_exact(big, FloatFormat::X87Extended);
    assert_eq!(rounded.compare_exact(big), Some(Ordering::Less));
    let nearest = Float::from_exact(Exact::from((1_u128 << 63) + 1), FloatFormat::X87Extended);
    assert_eq!(
        nearest.compare_exact(Exact::from((1_u128 << 63) + 1)),
        Some(Ordering::Equal)
    );
}

/// Exact integers weighted toward the cases the laws and range checks
/// turn on: small values, the ends of the range, and powers of two and
/// their neighbors, of either sign.
fn any_exact() -> impl Strategy<Value = Exact> {
    let power = (0_u32..128, -1_i128..=1, any::<bool>()).prop_map(|(shift, offset, negative)| {
        let magnitude = (1_u128 << shift).saturating_add_signed(offset);
        if negative && magnitude <= 1 << 127 {
            Exact::from(magnitude.cast_signed().wrapping_neg())
        } else {
            Exact::from(magnitude)
        }
    });

    prop_oneof![
        3 => (-300..300_i128).prop_map(Exact::from),
        2 => prop::sample::select(vec![
            Exact::ZERO,
            exact(1),
            exact(-1),
            exact(i128::MAX),
            exact(i128::MIN),
            exact(i128::MIN + 1),
            Exact::from(u128::MAX),
            Exact::from(u128::MAX - 1),
            Exact::from(1_u128 << 127),
        ]),
        2 => power,
        1 => any::<i128>().prop_map(Exact::from),
        1 => any::<u128>().prop_map(Exact::from),
    ]
}

/// A 256-bit two's complement integer, high limb first, wide enough to hold
/// any sum, difference, or bitwise result of two exact integers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Wide(i128, u128);

impl Wide {
    fn of(value: Exact) -> Self {
        let positive = Self(0, value.magnitude());
        if value.is_negative() {
            positive.negated()
        } else {
            positive
        }
    }

    fn negated(self) -> Self {
        let Self(high, low) = self;
        let (low, borrow) = 0_u128.overflowing_sub(low);
        Self((!high).wrapping_add(i128::from(!borrow)), low)
    }

    fn plus(self, other: Self) -> Self {
        let (low, carry) = self.1.overflowing_add(other.1);
        Self(self.0 + other.0 + i128::from(carry), low)
    }

    /// The exact integer this is, or `OutOfRange` beyond `-2^127..2^128`.
    fn exact(self) -> Result<Exact, NumberError> {
        match self {
            Self(0, low) => Ok(Exact::from(low)),
            Self(-1, low) if low >= 1 << 127 => Ok(Exact::from(low.cast_signed())),
            _ => Err(NumberError::OutOfRange),
        }
    }
}

/// `magnitude · factor` as a sign and a 256-bit magnitude, high limb first,
/// converted back when it is in range.
fn signed_wide(negative: bool, high: u128, low: u128) -> Result<Exact, NumberError> {
    match (negative, high) {
        (_, 0) if low == 0 => Ok(Exact::ZERO),
        (false, 0) => Ok(Exact::from(low)),
        (true, 0) if low <= 1 << 127 => Ok(Exact::from(low.cast_signed().wrapping_neg())),
        _ => Err(NumberError::OutOfRange),
    }
}

/// The full 256-bit product of two magnitudes, high limb first.
fn wide_product(left: u128, right: u128) -> (u128, u128) {
    let half = |value: u128| (value >> 64, value & u128::from(u64::MAX));
    let ((left_high, left_low), (right_high, right_low)) = (half(left), half(right));
    let low = left_low * right_low;
    let middle = (left_high * right_low, left_low * right_high);
    let (middle_sum, middle_carry) = middle.0.overflowing_add(middle.1);
    let (low, low_carry) = low.overflowing_add(middle_sum << 64);
    let high = left_high * right_high
        + (middle_sum >> 64)
        + (u128::from(middle_carry) << 64)
        + u128::from(low_carry);
    (high, low)
}

/// What each operator gives, from the wide model.
fn modeled(operator: &str, a: Exact, b: Exact) -> Result<Exact, NumberError> {
    let (left, right) = (Wide::of(a), Wide::of(b));
    let negative = a.is_negative() != b.is_negative();
    let amount = || {
        if b.is_negative() {
            Err(NumberError::ShiftAmount)
        } else {
            Ok(b.magnitude())
        }
    };

    match operator {
        "add" => left.plus(right).exact(),
        "sub" => left.plus(right.negated()).exact(),
        "mul" => {
            let (high, low) = wide_product(a.magnitude(), b.magnitude());
            signed_wide(negative, high, low)
        }
        "div" | "rem" if b.is_zero() => Err(NumberError::DivisionByZero),
        "div" => signed_wide(negative, 0, a.magnitude() / b.magnitude()),
        "rem" => signed_wide(a.is_negative(), 0, a.magnitude() % b.magnitude()),
        "and" => Wide(left.0 & right.0, left.1 & right.1).exact(),
        "or" => Wide(left.0 | right.0, left.1 | right.1).exact(),
        "xor" => Wide(left.0 ^ right.0, left.1 ^ right.1).exact(),
        "shl" => {
            let amount = amount()?;
            if a.is_zero() {
                return Ok(Exact::ZERO);
            }

            match u32::try_from(amount) {
                Ok(amount @ 0..128) => {
                    let (high, low) = wide_product(a.magnitude(), 1 << amount);
                    signed_wide(a.is_negative(), high, low)
                }
                _ => Err(NumberError::OutOfRange),
            }
        }
        // floor(a / 2^amount): a negative value rounds away from zero.
        "shr" => {
            let amount = amount()?;
            let magnitude = a.magnitude();
            let (quotient, inexact) = match u32::try_from(amount) {
                Ok(amount @ 0..128) => (magnitude >> amount, magnitude & ((1 << amount) - 1) != 0),
                _ => (0, magnitude != 0),
            };
            let rounded = quotient + u128::from(a.is_negative() && inexact);
            signed_wide(a.is_negative(), 0, rounded)
        }
        _ => unreachable!("{operator}"),
    }
}

type Operator = fn(Exact, Exact) -> Result<Exact, NumberError>;

fn shift_amount_strategy() -> impl Strategy<Value = Exact> {
    prop_oneof![
        3 => (0..140_i128).prop_map(Exact::from),
        1 => any_exact(),
    ]
}

proptest! {

    /// Every operator gives the exact result whenever it lies in
    /// `-2^127..2^128`, and `OutOfRange` exactly when it does not.
    #[test]
    fn exact_operators_match_a_wider_model(
        a in any_exact(),
        b in any_exact(),
        amount in shift_amount_strategy(),
    ) {
        let operators: [(&str, Operator); 8] = [
            ("add", Exact::add),
            ("sub", Exact::sub),
            ("mul", Exact::mul),
            ("div", Exact::div),
            ("rem", Exact::rem),
            ("and", |a, b| a.bitwise(BitOperator::And, b)),
            ("or", |a, b| a.bitwise(BitOperator::Or, b)),
            ("xor", |a, b| a.bitwise(BitOperator::Xor, b)),
        ];
        for (name, operator) in operators {
            prop_assert_eq!(operator(a, b), modeled(name, a, b), "{:?} {} {:?}", a, name, b);
        }

        prop_assert_eq!(a.shl(amount), modeled("shl", a, amount), "{:?} << {:?}", a, amount);
        prop_assert_eq!(a.shr(amount), modeled("shr", a, amount), "{:?} >> {:?}", a, amount);
        prop_assert_eq!(a.neg(), modeled("sub", Exact::ZERO, a), "-{:?}", a);
        prop_assert_eq!(a.not(), modeled("sub", exact(-1), a), "!{:?}", a);
    }

    #[test]
    fn patterns_round_trip_through_their_values(raw in any::<u128>(), width in 1..=MAX_WIDTH, signed in any::<bool>()) {
        patterns_round_trip(raw, width, signed)?;
    }
}

/// A bit pattern of any width and signedness reads back through its value,
/// its complement, and casts to the other signedness and to 128 bits.
fn patterns_round_trip(raw: u128, width: u8, signed: bool) -> Result<(), TestCaseError> {
    let ty = int(width, signed);
    let bits = Bits::from_raw(ty, raw);
    prop_assert!(ty.contains(bits.value()));
    prop_assert_eq!(Bits::truncate(ty, bits.value()), bits);
    prop_assert_eq!(bits.not().not(), bits);
    let flipped = int(width, !signed);
    prop_assert_eq!(bits.cast(flipped).cast(ty), bits);
    let wider = int(128, signed);
    prop_assert_eq!(bits.cast(wider).cast(ty), bits);
    Ok(())
}

/// Found by the property: a signed one-bit integer.
#[test]
fn a_signed_one_bit_zero_round_trips() {
    patterns_round_trip(0, 1, true).unwrap();
}
