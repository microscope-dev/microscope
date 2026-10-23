//! JavaScript's rules, as ECMAScript defines them. A Number loads as a
//! `Float`, a `BigInt` as an `Int`, `null` as `Nothing`, and `undefined` as
//! `Undefined`. An operation that would run the program's code, such as an
//! object's `valueOf`, or read a number from a string, is refused.

use std::cmp::Ordering;

use super::{arithmetic, holds, refused, texts};
use crate::eval::error::ErrorKind;
use crate::eval::number::{BitOperator, Exact, Float, IntType};
use crate::eval::syntax::ast::{BinaryOp, UnaryOp};
use crate::eval::target::{Key, Loaded, LoadedValue, Stop};
use crate::{TextCompletion, TextSummary};

/// A value as JavaScript's operators see it.
#[derive(Debug, Clone, Copy)]
enum Primitive<'a> {
    Undefined,
    Null,
    Bool(bool),
    Number(f64),
    BigInt(Exact),
    String(&'a TextSummary),

    /// An object, or any other value microscope does not tell apart.
    Object,
}

fn primitive(value: &LoadedValue) -> Primitive<'_> {
    match value {
        LoadedValue::Undefined => Primitive::Undefined,
        LoadedValue::Nothing => Primitive::Null,
        LoadedValue::Bool(value) => Primitive::Bool(*value),
        LoadedValue::Float(value) => Primitive::Number(value.to_f64()),
        LoadedValue::Int(value) => Primitive::BigInt(*value),
        LoadedValue::Text(text) => Primitive::String(text),
        LoadedValue::Bytes(_) | LoadedValue::Collection { .. } | LoadedValue::Object => {
            Primitive::Object
        }
    }
}

/// A number JavaScript computes with.
#[derive(Debug, Clone, Copy)]
enum Numeric {
    Number(f64),
    BigInt(Exact),
}

fn object_code(value: &Loaded) -> Stop {
    refused(
        ErrorKind::Unsupported,
        format!(
            "JavaScript turns a `{}` into a primitive value by calling its own code, such as \
             `valueOf`, which microscope does not run",
            value.type_name
        ),
    )
}

fn string_number() -> Stop {
    refused(
        ErrorKind::Unsupported,
        "JavaScript reads a number from a string by its own grammar, which microscope does not \
         follow",
    )
}

/// `ToNumeric` of a primitive that is no object or string.
const fn numeric(value: Primitive<'_>) -> Option<Numeric> {
    Some(match value {
        Primitive::Undefined => Numeric::Number(f64::NAN),
        Primitive::Null => Numeric::Number(0.0),
        Primitive::Bool(value) => Numeric::Number(if value { 1.0 } else { 0.0 }),
        Primitive::Number(value) => Numeric::Number(value),
        Primitive::BigInt(value) => Numeric::BigInt(value),
        Primitive::String(_) | Primitive::Object => return None,
    })
}

/// `ToNumeric` of an operand of arithmetic. `+` would join a string.
fn operand(value: &Loaded, joins: bool) -> Result<Numeric, Stop> {
    match primitive(&value.value) {
        Primitive::Object => Err(object_code(value)),
        Primitive::String(_) if joins => Err(refused(
            ErrorKind::Unsupported,
            "`+` with a string joins text into a new string, which microscope does not make",
        )),
        Primitive::String(_) => Err(string_number()),
        other => numeric(other).ok_or_else(string_number),
    }
}

fn mixed() -> Stop {
    refused(
        ErrorKind::Type,
        "TypeError: Cannot mix BigInt and other types, use explicit conversions",
    )
}

/// `ToInt32`: the integer part, wrapped to 32 bits.
fn to_int32(value: f64) -> i32 {
    if !value.is_finite() {
        return 0;
    }

    // Both steps are exact for an integer-valued double.
    let wrapped = value.trunc().rem_euclid(4_294_967_296.0);

    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "the value is an integer in [0, 2^32)"
    )]
    let bits = wrapped as u32;
    bits.cast_signed()
}

pub(super) fn truth(value: &Loaded) -> Result<bool, Stop> {
    Ok(match &value.value {
        LoadedValue::Nothing | LoadedValue::Undefined => false,
        LoadedValue::Bool(value) => *value,
        LoadedValue::Int(value) => !value.is_zero(),
        LoadedValue::Float(value) => !(value.is_zero() || value.is_nan()),
        LoadedValue::Text(text) => super::text_truth(text)?,
        // Every object is true, an empty array or Map too.
        LoadedValue::Bytes(_) | LoadedValue::Collection { .. } | LoadedValue::Object => true,
    })
}

pub(super) fn compare(op: BinaryOp, left: &Loaded, right: &Loaded) -> Result<bool, Stop> {
    let (left_value, right_value) = (primitive(&left.value), primitive(&right.value));
    if matches!(op, BinaryOp::Eq | BinaryOp::Ne) {
        let equal = loosely_equal(left_value, right_value, left, right)?;
        return Ok(equal == (op == BinaryOp::Eq));
    }

    for (value, loaded) in [(left_value, left), (right_value, right)] {
        if matches!(value, Primitive::Object) {
            return Err(object_code(loaded));
        }
    }

    let order = match (left_value, right_value) {
        (Primitive::String(left), Primitive::String(right)) => {
            Some(texts(left, right, false, utf16_order)?)
        }
        (left, right) => match (numeric(left), numeric(right)) {
            (Some(left), Some(right)) => order(left, right),
            _ => return Err(string_number()),
        },
    };

    Ok(holds(op, order))
}

/// The order of two strings' UTF-16 code units, which is not their
/// characters' order: U+FFFF follows U+1F600's first unit.
fn utf16_order(left: &[u8], right: &[u8]) -> Result<Ordering, Stop> {
    let (Ok(left), Ok(right)) = (std::str::from_utf8(left), std::str::from_utf8(right)) else {
        return Err(refused(
            ErrorKind::Unsupported,
            "text that is not UTF-8 has no order among JavaScript's strings",
        ));
    };

    Ok(left.encode_utf16().cmp(right.encode_utf16()))
}

/// The exact order of two numbers; `None` when either is NaN.
fn order(left: Numeric, right: Numeric) -> Option<Ordering> {
    match (left, right) {
        (Numeric::Number(left), Numeric::Number(right)) => left.partial_cmp(&right),
        (Numeric::BigInt(left), Numeric::BigInt(right)) => Some(left.cmp(&right)),
        (Numeric::Number(left), Numeric::BigInt(right)) => {
            Float::from_f64(left).compare_exact(right)
        }
        (Numeric::BigInt(left), Numeric::Number(right)) => Float::from_f64(right)
            .compare_exact(left)
            .map(Ordering::reverse),
    }
}

/// `IsLooselyEqual`, `==`, where it runs none of the program's code.
fn loosely_equal(
    left: Primitive<'_>,
    right: Primitive<'_>,
    left_loaded: &Loaded,
    right_loaded: &Loaded,
) -> Result<bool, Stop> {
    use Primitive as P;
    let number = |value: bool| P::Number(if value { 1.0 } else { 0.0 });
    Ok(match (left, right) {
        (P::Object, P::Object) => {
            return Err(refused(
                ErrorKind::Unsupported,
                "JavaScript compares objects by identity, which microscope does not compare",
            ));
        }
        (P::Undefined | P::Null, P::Undefined | P::Null) => true,
        (P::Undefined | P::Null, _) | (_, P::Undefined | P::Null) => false,
        (P::Bool(left), P::Bool(right)) => left == right,
        (P::Bool(left), right) => {
            return loosely_equal(number(left), right, left_loaded, right_loaded);
        }
        (left, P::Bool(right)) => {
            return loosely_equal(left, number(right), left_loaded, right_loaded);
        }
        (P::String(left), P::String(right)) => {
            texts(left, right, true, |left, right| Ok(left.cmp(right)))? == Ordering::Equal
        }
        (P::Object, _) => return Err(object_code(left_loaded)),
        (_, P::Object) => return Err(object_code(right_loaded)),
        (P::String(_), _) | (_, P::String(_)) => return Err(string_number()),
        (left, right) => match (numeric(left), numeric(right)) {
            (Some(left), Some(right)) => order(left, right) == Some(Ordering::Equal),
            _ => return Err(string_number()),
        },
    })
}

pub(super) fn binary(op: BinaryOp, left: &Loaded, right: &Loaded) -> Result<LoadedValue, Stop> {
    let joins = op == BinaryOp::Add;
    let left = operand(left, joins)?;
    let right = operand(right, joins)?;
    match (left, right) {
        (Numeric::Number(left), Numeric::Number(right)) => Ok(LoadedValue::Float(Float::from_f64(
            numbers(op, left, right),
        ))),
        (Numeric::BigInt(left), Numeric::BigInt(right)) => {
            bigints(op, left, right).map(LoadedValue::Int)
        }
        _ => Err(mixed()),
    }
}

/// Number arithmetic in doubles. Bit operations and shifts take `ToInt32` of
/// their operands, and a shift count's low five bits.
fn numbers(op: BinaryOp, left: f64, right: f64) -> f64 {
    match op {
        BinaryOp::Add => left + right,
        BinaryOp::Sub => left - right,
        BinaryOp::Mul => left * right,
        BinaryOp::Div => left / right,
        // C's `fmod`, with the dividend's sign.
        BinaryOp::Rem => left % right,
        BinaryOp::BitAnd => f64::from(to_int32(left) & to_int32(right)),
        BinaryOp::BitOr => f64::from(to_int32(left) | to_int32(right)),
        BinaryOp::BitXor => f64::from(to_int32(left) ^ to_int32(right)),
        BinaryOp::Shl => f64::from(to_int32(left).wrapping_shl(to_int32(right).cast_unsigned())),
        _ => f64::from(to_int32(left).wrapping_shr(to_int32(right).cast_unsigned())),
    }
}

/// `BigInt` arithmetic, exact, with division truncated toward zero.
fn bigints(op: BinaryOp, left: Exact, right: Exact) -> Result<Exact, Stop> {
    match op {
        BinaryOp::Add => left.add(right),
        BinaryOp::Sub => left.sub(right),
        BinaryOp::Mul => left.mul(right),
        BinaryOp::Div | BinaryOp::Rem if right.is_zero() => {
            return Err(refused(
                ErrorKind::Arithmetic,
                "RangeError: Division by zero",
            ));
        }
        BinaryOp::Div => left.div(right),
        BinaryOp::Rem => left.rem(right),
        BinaryOp::BitAnd => left.bitwise(BitOperator::And, right),
        BinaryOp::BitOr => left.bitwise(BitOperator::Or, right),
        BinaryOp::BitXor => left.bitwise(BitOperator::Xor, right),
        // A negative count shifts the other way.
        BinaryOp::Shl if right.is_negative() => right.neg().and_then(|count| left.shr(count)),
        BinaryOp::Shl => left.shl(right),
        _ if right.is_negative() => right.neg().and_then(|count| left.shl(count)),
        _ => left.shr(right),
    }
    .map_err(arithmetic)
}

pub(super) fn unary(op: UnaryOp, operand: &Loaded) -> Result<LoadedValue, Stop> {
    Ok(match (op, self::operand(operand, false)?) {
        (UnaryOp::Neg, Numeric::Number(value)) => LoadedValue::Float(Float::from_f64(-value)),
        (UnaryOp::Neg, Numeric::BigInt(value)) => {
            LoadedValue::Int(value.neg().map_err(arithmetic)?)
        }
        (_, Numeric::Number(value)) => {
            LoadedValue::Float(Float::from_f64(f64::from(!to_int32(value))))
        }
        (_, Numeric::BigInt(value)) => LoadedValue::Int(value.not().map_err(arithmetic)?),
    })
}

/// A value as a property key: a string, or a `BigInt` or an integer Number
/// below 2^53, whose name JavaScript writes as microscope does.
pub(super) fn key(value: &Loaded) -> Result<Key, Stop> {
    let refuse = || {
        refused(
            ErrorKind::Unsupported,
            format!(
                "microscope names a property by a string, a BigInt, or an integer Number below \
                 2^53, not by a `{}` such as this",
                value.type_name
            ),
        )
    };

    match &value.value {
        LoadedValue::Int(value) => Ok(Key::Integer(*value)),
        LoadedValue::Float(number) => {
            let int = IntType::new(64, true).ok_or_else(refuse)?;
            let integer = number.to_int(int).map_err(|_| refuse())?.value();
            let limit = Exact::from(1_u128 << 53);
            let index = number.compare_exact(integer) == Some(Ordering::Equal)
                && integer < limit
                && limit.neg().is_ok_and(|limit| limit < integer);
            if index {
                Ok(Key::Integer(integer))
            } else {
                Err(refuse())
            }
        }
        LoadedValue::Text(text) => match text.completion {
            TextCompletion::Complete => Ok(Key::Text(text.bytes.to_vec())),
            completion => Err(super::incomplete(completion)),
        },
        _ => Err(refuse()),
    }
}
