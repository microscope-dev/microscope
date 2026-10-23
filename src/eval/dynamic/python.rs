//! Python's rules, as CPython 3.12 and later apply them. A `bool` counts as
//! the `int` 0 or 1, as Python's `bool` derives from `int`, and stays a
//! `bool` where Python's result is one. An `int` beyond the 128 bits
//! microscope computes with is refused.

use std::cmp::Ordering;
use std::num::FpCategory;

use super::{arithmetic, holds, nearest, refused, texts};
use crate::TextCompletion;
use crate::eval::error::ErrorKind;
use crate::eval::number::{BitOperator, Exact, Float, NumberError};
use crate::eval::syntax::ast::{BinaryOp, UnaryOp};
use crate::eval::syntax::binary_text;
use crate::eval::target::{Key, Loaded, LoadedValue, Stop};

/// A number as Python computes with it.
#[derive(Debug, Clone, Copy)]
enum Number {
    Int(Exact),
    Float(f64),
}

/// A value as a number, a `bool` counting as 0 or 1.
fn number(value: &LoadedValue) -> Option<Number> {
    match value {
        LoadedValue::Bool(value) => Some(Number::Int(Exact::from(u128::from(*value)))),
        LoadedValue::Int(value) => Some(Number::Int(*value)),
        LoadedValue::Float(value) => Some(Number::Float(value.to_f64())),
        _ => None,
    }
}

fn zero_division(message: &str) -> Stop {
    refused(
        ErrorKind::Arithmetic,
        format!("ZeroDivisionError: {message}"),
    )
}

fn is_zero(value: f64) -> bool {
    value.classify() == FpCategory::Zero
}

pub(super) fn truth(value: &Loaded) -> Result<bool, Stop> {
    match &value.value {
        LoadedValue::Nothing | LoadedValue::Undefined => Ok(false),
        LoadedValue::Bool(value) => Ok(*value),
        LoadedValue::Int(value) => Ok(!value.is_zero()),
        LoadedValue::Float(value) => Ok(!value.is_zero()),
        LoadedValue::Text(text) | LoadedValue::Bytes(text) => super::text_truth(text),
        LoadedValue::Collection { length } => Ok(*length != 0),
        LoadedValue::Object => Err(refused(
            ErrorKind::Unsupported,
            format!(
                "the truth of a `{}` is what its type's code says, which microscope does not run",
                value.type_name
            ),
        )),
    }
}

pub(super) fn compare(op: BinaryOp, left: &Loaded, right: &Loaded) -> Result<bool, Stop> {
    let equality = matches!(op, BinaryOp::Eq | BinaryOp::Ne);
    let ordering = match (number(&left.value), number(&right.value)) {
        (Some(left), Some(right)) => numbers(left, right),
        _ => match (&left.value, &right.value) {
            // UTF-8's byte order is its characters' order, which is
            // Python's.
            (LoadedValue::Text(left), LoadedValue::Text(right))
            | (LoadedValue::Bytes(left), LoadedValue::Bytes(right)) => {
                Some(texts(left, right, equality, |left, right| {
                    Ok(left.cmp(right))
                })?)
            }
            (LoadedValue::Nothing, LoadedValue::Nothing) if equality => Some(Ordering::Equal),
            _ => {
                for value in [left, right] {
                    if matches!(
                        value.value,
                        LoadedValue::Collection { .. } | LoadedValue::Object
                    ) {
                        return Err(refused(
                            ErrorKind::Unsupported,
                            format!(
                                "a `{}` is not compared: microscope compares numbers, truth \
                                 values, text, bytes, and the value for nothing",
                                value.type_name
                            ),
                        ));
                    }
                }

                if !equality {
                    return Err(refused(
                        ErrorKind::Type,
                        format!(
                            "`{}` cannot order `{}` and `{}`",
                            binary_text(op),
                            left.type_name,
                            right.type_name
                        ),
                    ));
                }

                // Values of different kinds are unequal.
                None
            }
        },
    };

    Ok(holds(op, ordering))
}

/// The exact order of two numbers; `None` when either is NaN.
fn numbers(left: Number, right: Number) -> Option<Ordering> {
    match (left, right) {
        (Number::Int(left), Number::Int(right)) => Some(left.cmp(&right)),
        (Number::Float(left), Number::Float(right)) => left.partial_cmp(&right),
        (Number::Float(left), Number::Int(right)) => Float::from_f64(left).compare_exact(right),
        (Number::Int(left), Number::Float(right)) => Float::from_f64(right)
            .compare_exact(left)
            .map(Ordering::reverse),
    }
}

pub(super) fn binary(op: BinaryOp, left: &Loaded, right: &Loaded) -> Result<LoadedValue, Stop> {
    // `bool`'s own `&`, `|`, and `^` give a `bool`.
    if let (LoadedValue::Bool(left), LoadedValue::Bool(right)) = (&left.value, &right.value) {
        match op {
            BinaryOp::BitAnd => return Ok(LoadedValue::Bool(left & right)),
            BinaryOp::BitOr => return Ok(LoadedValue::Bool(left | right)),
            BinaryOp::BitXor => return Ok(LoadedValue::Bool(left ^ right)),
            _ => {}
        }
    }

    let (Some(left_number), Some(right_number)) = (number(&left.value), number(&right.value))
    else {
        return Err(refused(
            if computes(op, left, right) {
                ErrorKind::Unsupported
            } else {
                ErrorKind::Type
            },
            format!(
                "`{}` cannot be applied to `{}` and `{}`",
                binary_text(op),
                left.type_name,
                right.type_name
            ),
        ));
    };

    match (left_number, right_number) {
        (Number::Int(left), Number::Int(right)) => integers(op, left, right),
        (left, right) => floats(op, float(left), float(right))
            .map(|value| LoadedValue::Float(Float::from_f64(value))),
    }
}

/// A number as a `float`, as Python converts an `int` for arithmetic with
/// one: the nearest, ties to even.
fn float(number: Number) -> f64 {
    match number {
        Number::Int(value) => nearest(value),
        Number::Float(value) => value,
    }
}

fn integers(op: BinaryOp, left: Exact, right: Exact) -> Result<LoadedValue, Stop> {
    let result = match op {
        BinaryOp::Add => left.add(right),
        BinaryOp::Sub => left.sub(right),
        BinaryOp::Mul => left.mul(right),
        BinaryOp::Div if right.is_zero() => return Err(zero_division("division by zero")),
        BinaryOp::Div => {
            return Ok(LoadedValue::Float(Float::from_f64(true_divide(
                left, right,
            ))));
        }
        BinaryOp::Rem if right.is_zero() => return Err(zero_division("integer modulo by zero")),
        BinaryOp::Rem => floored_rem(left, right),
        BinaryOp::BitAnd => left.bitwise(BitOperator::And, right),
        BinaryOp::BitOr => left.bitwise(BitOperator::Or, right),
        BinaryOp::BitXor => left.bitwise(BitOperator::Xor, right),
        BinaryOp::Shl | BinaryOp::Shr if right.is_negative() => {
            return Err(refused(
                ErrorKind::Arithmetic,
                "ValueError: negative shift count",
            ));
        }
        BinaryOp::Shl => left.shl(right),
        _ => left.shr(right),
    };

    result.map(LoadedValue::Int).map_err(arithmetic)
}

/// The remainder of division rounded down, which has the divisor's sign.
fn floored_rem(left: Exact, right: Exact) -> Result<Exact, NumberError> {
    let rem = left.rem(right)?;
    if !rem.is_zero() && rem.is_negative() != right.is_negative() {
        rem.add(right)
    } else {
        Ok(rem)
    }
}

/// How many of a quotient's leading bits are kept before it is rounded:
/// a double's 53, a guard bit, and a last bit that also records whether
/// any bit below it is set.
const KEPT: u32 = 55;

/// `left / right` rounded once to the nearest double, ties to even, as
/// CPython's `long_true_divide` gives it. Dividing the operands' nearest
/// doubles would round twice.
fn true_divide(left: Exact, right: Exact) -> f64 {
    let magnitude = quotient(left.magnitude(), right.magnitude());
    if left.is_negative() == right.is_negative() {
        magnitude
    } else {
        -magnitude
    }
}

/// `dividend / divisor` for a nonzero divisor, by long division to `KEPT`
/// bits.
fn quotient(dividend: u128, divisor: u128) -> f64 {
    if dividend == 0 || divisor == 0 {
        return 0.0;
    }

    // The weight of the dividend's bit that the next step brings down,
    // 2^position; below the point its bits are zero.
    let mut position = 127 - dividend.leading_zeros().cast_signed();
    let mut remainder: u128 = 0;
    let mut bits: u64 = 0;
    let mut kept = 0;
    while kept < KEPT {
        let next = if position >= 0 {
            (dividend >> position) & 1
        } else {
            0
        };

        // The remainder is below the divisor, so doubling it overflows by
        // at most the bit shifted out.
        let carried = remainder >> 127 == 1;
        remainder = (remainder << 1) | next;
        let bit = carried || remainder >= divisor;
        if bit {
            remainder = remainder.wrapping_sub(divisor);
        }

        if bits != 0 || bit {
            bits = (bits << 1) | u64::from(bit);
            kept += 1;
        }

        if kept < KEPT {
            position -= 1;
        }
    }

    let below = position > 0 && dividend & ((1 << position) - 1) != 0;
    let bits = bits | u64::from(remainder != 0 || below);

    #[allow(clippy::cast_precision_loss, reason = "the one rounding, to nearest")]
    let rounded = bits as f64;

    // Scaling by a power of two is exact: the quotient lies within
    // [2^-128, 2^128].
    rounded * f64::from_bits(u64::from((position + 1023).unsigned_abs()) << 52)
}

fn floats(op: BinaryOp, left: f64, right: f64) -> Result<f64, Stop> {
    Ok(match op {
        BinaryOp::Add => left + right,
        BinaryOp::Sub => left - right,
        BinaryOp::Mul => left * right,
        BinaryOp::Div if is_zero(right) => return Err(zero_division("float division by zero")),
        BinaryOp::Div => left / right,
        BinaryOp::Rem if is_zero(right) => return Err(zero_division("float modulo")),
        BinaryOp::Rem => float_rem(left, right),
        _ => {
            return Err(refused(
                ErrorKind::Type,
                format!("`{}` applies to integers, not floats", binary_text(op)),
            ));
        }
    })
}

/// CPython's `float_rem`: C's `fmod`, moved to the divisor's sign, and a
/// zero with the divisor's sign.
fn float_rem(left: f64, right: f64) -> f64 {
    let rem = left % right;
    if is_zero(rem) {
        0.0_f64.copysign(right)
    } else if (right < 0.0) != (rem < 0.0) {
        rem + right
    } else {
        rem
    }
}

/// Whether `op` could compute a value from operands that are not both
/// numbers: an object's type defines its own operators; text, bytes, and
/// collections are joined, and combined as sets, with each other; and
/// with a number they are repeated by `*` and formatted by `%`. Anything
/// else, such as nothing with a number, is Python's type error.
fn computes(op: BinaryOp, left: &Loaded, right: &Loaded) -> bool {
    let joined = |value: &Loaded| {
        matches!(
            value.value,
            LoadedValue::Text(_) | LoadedValue::Bytes(_) | LoadedValue::Collection { .. }
        )
    };
    let object = |value: &Loaded| value.value == LoadedValue::Object;
    object(left)
        || object(right)
        || (joined(left) && joined(right))
        || ((joined(left) || joined(right)) && matches!(op, BinaryOp::Mul | BinaryOp::Rem))
}

pub(super) fn unary(op: UnaryOp, operand: &Loaded) -> Result<LoadedValue, Stop> {
    let symbol = if op == UnaryOp::Neg { "-" } else { "~" };
    match (op, number(&operand.value)) {
        (UnaryOp::Neg, Some(Number::Int(value))) => {
            value.neg().map(LoadedValue::Int).map_err(arithmetic)
        }
        (UnaryOp::Neg, Some(Number::Float(value))) => {
            Ok(LoadedValue::Float(Float::from_f64(-value)))
        }
        (UnaryOp::BitNot, Some(Number::Int(value))) => {
            value.not().map(LoadedValue::Int).map_err(arithmetic)
        }
        (UnaryOp::BitNot, Some(Number::Float(_))) => Err(refused(
            ErrorKind::Type,
            "`~` applies to integers, not floats",
        )),
        _ => Err(refused(
            // Only an object's own type may define the operator.
            if operand.value == LoadedValue::Object {
                ErrorKind::Unsupported
            } else {
                ErrorKind::Type
            },
            format!("`{symbol}` cannot be applied to `{}`", operand.type_name),
        )),
    }
}

/// A value as the key of an index: a number, truth value, text, or the
/// value for nothing.
pub(super) fn key(value: &Loaded) -> Result<Key, Stop> {
    match &value.value {
        LoadedValue::Int(value) => Ok(Key::Integer(*value)),
        LoadedValue::Bool(value) => Ok(Key::Bool(*value)),
        LoadedValue::Float(value) => Ok(Key::Float(*value)),
        LoadedValue::Nothing => Ok(Key::Nothing),
        LoadedValue::Text(text) => match text.completion {
            TextCompletion::Complete => Ok(Key::Text(text.bytes.to_vec())),
            completion => Err(super::incomplete(completion)),
        },
        // Bytes, a tuple, or an object may be a key, by its type's hash.
        _ => Err(refused(
            ErrorKind::Unsupported,
            format!(
                "a `{}` is no key: microscope finds items by numbers, truth values, text, and the \
                 value for nothing",
                value.type_name
            ),
        )),
    }
}
