//! The rules of operations on values of a dynamic type, which the types
//! the operands have when they run choose.
//!
//! Numbers follow microscope's exact rules, with a truth value counting as
//! the integer 0 or 1, as Python's do. An integer `/`, and a `%` of a
//! negative operand, are refused: a dynamic language may divide integers
//! into a fraction and take a floored remainder, as Python does, where
//! microscope truncates. A value that is no number, truth value, text,
//! bytes, or nothing is neither compared nor computed with, since its own
//! type's code decides what those mean.

use std::cmp::Ordering;

use super::error::ErrorKind;
use super::number::{BitOperator, Exact, Float, FloatOperator, NumberError};
use super::syntax::ast::{BinaryOp, UnaryOp};
use super::syntax::binary_text;
use super::target::{Key, Loaded, LoadedValue, Refusal, Stop};
use crate::{TextCompletion, TextSummary, VariableState};

/// A number an operation on values of a dynamic type computes.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Number {
    Int(Exact),
    Float(Float),
}

fn refused(kind: ErrorKind, message: impl Into<String>) -> Stop {
    Stop::Refused(Refusal::new(kind, message))
}

fn arithmetic(error: NumberError) -> Stop {
    refused(ErrorKind::Arithmetic, error.to_string())
}

/// Why text read in part has no value an operation can use.
fn incomplete(completion: TextCompletion) -> Stop {
    Stop::missing(VariableState::Unavailable(super::interp::incomplete(
        completion,
    )))
}

/// A value as a number, a truth value counting as 0 or 1.
fn number(value: &LoadedValue) -> Option<Number> {
    match value {
        LoadedValue::Bool(value) => Some(Number::Int(Exact::from(u128::from(*value)))),
        LoadedValue::Int(value) => Some(Number::Int(*value)),
        LoadedValue::Float(value) => Some(Number::Float(*value)),
        _ => None,
    }
}

/// Whether a value is true, as its language says.
pub fn truth(value: &Loaded) -> Result<bool, Stop> {
    match &value.value {
        LoadedValue::Nothing => Ok(false),
        LoadedValue::Bool(value) => Ok(*value),
        LoadedValue::Int(value) => Ok(!value.is_zero()),
        LoadedValue::Float(value) => Ok(!value.is_zero()),
        LoadedValue::Text(text) | LoadedValue::Bytes(text) => {
            if !text.bytes.is_empty() {
                return Ok(true);
            }

            match text.completion {
                TextCompletion::Complete => Ok(false),
                completion => Err(incomplete(completion)),
            }
        }
        LoadedValue::Collection { length } => Ok(*length != 0),
        LoadedValue::Object => Err(refused(
            ErrorKind::Type,
            format!(
                "the truth of a `{}` is what its type's code says, which microscope does not run",
                value.type_name
            ),
        )),
    }
}

/// `op`, a comparison, of two values.
pub fn compare(op: BinaryOp, left: &Loaded, right: &Loaded) -> Result<bool, Stop> {
    let equality = matches!(op, BinaryOp::Eq | BinaryOp::Ne);
    let ordering = match (number(&left.value), number(&right.value)) {
        (Some(left), Some(right)) => numbers(left, right),
        _ => match (&left.value, &right.value) {
            (LoadedValue::Text(left), LoadedValue::Text(right))
            | (LoadedValue::Bytes(left), LoadedValue::Bytes(right)) => {
                Some(texts(left, right, equality)?)
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

    Ok(match (op, ordering) {
        (BinaryOp::Ne, None) => true,
        (_, None) => false,
        (BinaryOp::Eq, Some(order)) => order == Ordering::Equal,
        (BinaryOp::Ne, Some(order)) => order != Ordering::Equal,
        (BinaryOp::Lt, Some(order)) => order == Ordering::Less,
        (BinaryOp::Le, Some(order)) => order != Ordering::Greater,
        (BinaryOp::Gt, Some(order)) => order == Ordering::Greater,
        (_, Some(order)) => order != Ordering::Less,
    })
}

/// The exact order of two numbers; `None` when either is NaN.
fn numbers(left: Number, right: Number) -> Option<Ordering> {
    match (left, right) {
        (Number::Int(left), Number::Int(right)) => Some(left.cmp(&right)),
        (Number::Float(left), Number::Float(right)) => left.compare(right),
        (Number::Float(left), Number::Int(right)) => left.compare_exact(right),
        (Number::Int(left), Number::Float(right)) => {
            right.compare_exact(left).map(Ordering::reverse)
        }
    }
}

/// The order of two texts' bytes, which for UTF-8 is their characters'
/// order. Text read in part is unequal to text that differs within what
/// was read, and has no other order.
fn texts(left: &TextSummary, right: &TextSummary, equality: bool) -> Result<Ordering, Stop> {
    let (left_bytes, right_bytes) = (left.bytes.as_ref(), right.bytes.as_ref());
    let whole = |text: &TextSummary| text.completion == TextCompletion::Complete;
    if whole(left) && whole(right) {
        return Ok(left_bytes.cmp(right_bytes));
    }

    let shared = left_bytes.len().min(right_bytes.len());
    if equality && left_bytes[..shared] != right_bytes[..shared] {
        return Ok(Ordering::Less);
    }

    let partial = if whole(left) { right } else { left };
    Err(incomplete(partial.completion))
}

/// `op`, arithmetic or a bit operation, of two values.
pub fn binary(op: BinaryOp, left: &Loaded, right: &Loaded) -> Result<Number, Stop> {
    let (Some(left_number), Some(right_number)) = (number(&left.value), number(&right.value))
    else {
        return Err(refused(
            ErrorKind::Type,
            format!(
                "`{}` cannot be applied to `{}` and `{}`",
                binary_text(op),
                left.type_name,
                right.type_name
            ),
        ));
    };

    match (left_number, right_number) {
        (Number::Int(left), Number::Int(right)) => integers(op, left, right).map(Number::Int),
        (left, right) => floats(op, float(left), float(right)).map(Number::Float),
    }
}

fn float(number: Number) -> Float {
    match number {
        Number::Int(value) => Float::from_exact(value, super::number::FloatFormat::Binary64),
        Number::Float(value) => value,
    }
}

fn integers(op: BinaryOp, left: Exact, right: Exact) -> Result<Exact, Stop> {
    let negative = |value: Exact| value < Exact::ZERO;
    match op {
        BinaryOp::Add => left.add(right),
        BinaryOp::Sub => left.sub(right),
        BinaryOp::Mul => left.mul(right),
        BinaryOp::Div => {
            return Err(refused(
                ErrorKind::Arithmetic,
                "microscope does not divide integers of a dynamic type, whose language may \
                 divide them into a fraction, as Python's `/` does; multiply one by 1.0 to \
                 divide them as floats",
            ));
        }
        BinaryOp::Rem if negative(left) || negative(right) => {
            return Err(refused(
                ErrorKind::Arithmetic,
                "microscope takes `%` of integers of a dynamic type only when neither is \
                 negative, since their language may floor the quotient, as Python does, \
                 where microscope truncates it",
            ));
        }
        BinaryOp::Rem => left.rem(right),
        BinaryOp::BitAnd => left.bitwise(BitOperator::And, right),
        BinaryOp::BitOr => left.bitwise(BitOperator::Or, right),
        BinaryOp::BitXor => left.bitwise(BitOperator::Xor, right),
        BinaryOp::Shl => left.shl(right),
        _ => left.shr(right),
    }
    .map_err(arithmetic)
}

fn floats(op: BinaryOp, left: Float, right: Float) -> Result<Float, Stop> {
    let operator = match op {
        BinaryOp::Add => FloatOperator::Add,
        BinaryOp::Sub => FloatOperator::Sub,
        BinaryOp::Mul => FloatOperator::Mul,
        // A language may raise where IEEE arithmetic gives an infinity, as
        // Python does.
        BinaryOp::Div if right.is_zero() => return Err(arithmetic(NumberError::DivisionByZero)),
        BinaryOp::Div => FloatOperator::Div,
        BinaryOp::Rem => {
            return Err(refused(
                ErrorKind::Arithmetic,
                "microscope does not take `%` of a float of a dynamic type, whose language \
                 may floor the quotient, as Python does, where microscope truncates it",
            ));
        }
        _ => {
            return Err(refused(
                ErrorKind::Type,
                format!("`{}` applies to integers, not floats", binary_text(op)),
            ));
        }
    };

    Ok(Float::binary(operator, left, right))
}

/// `-` or `~` of a value.
pub fn unary(op: UnaryOp, operand: &Loaded) -> Result<Number, Stop> {
    let symbol = if op == UnaryOp::Neg { "-" } else { "~" };
    match (op, number(&operand.value)) {
        (UnaryOp::Neg, Some(Number::Int(value))) => {
            value.neg().map(Number::Int).map_err(arithmetic)
        }
        (UnaryOp::Neg, Some(Number::Float(value))) => Ok(Number::Float(value.neg())),
        (UnaryOp::BitNot, Some(Number::Int(value))) => {
            value.not().map(Number::Int).map_err(arithmetic)
        }
        (UnaryOp::BitNot, Some(Number::Float(_))) => Err(refused(
            ErrorKind::Type,
            "`~` applies to integers, not floats",
        )),
        _ => Err(refused(
            ErrorKind::Type,
            format!("`{symbol}` cannot be applied to `{}`", operand.type_name),
        )),
    }
}

/// A value as the key of an index: a number, truth value, text, or the
/// value for nothing.
pub fn key(value: &Loaded) -> Result<Key, Stop> {
    match &value.value {
        LoadedValue::Int(value) => Ok(Key::Integer(*value)),
        LoadedValue::Bool(value) => Ok(Key::Bool(*value)),
        LoadedValue::Float(value) => Ok(Key::Float(*value)),
        LoadedValue::Nothing => Ok(Key::Nothing),
        LoadedValue::Text(text) => match text.completion {
            TextCompletion::Complete => Ok(Key::Text(text.bytes.to_vec())),
            completion => Err(incomplete(completion)),
        },
        _ => Err(refused(
            ErrorKind::Type,
            format!(
                "a `{}` is no key: microscope finds items by numbers, truth values, text, and the \
                 value for nothing",
                value.type_name
            ),
        )),
    }
}
