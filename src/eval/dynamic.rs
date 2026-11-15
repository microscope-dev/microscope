use std::borrow::Cow;
use std::cmp::Ordering;

use super::error::ErrorKind;
use super::number::{Exact, Float, FloatFormat, NumberError};
use super::syntax::ast::{BinaryOp, UnaryOp};
use super::target::{Key, Loaded, LoadedValue, Refusal, Rules, Stop};
use crate::{TextCompletion, TextSummary, VariableState};

mod javascript;
mod python;

#[cfg(test)]
mod tests;

fn refused(kind: ErrorKind, message: impl Into<String>) -> Stop {
    Stop::Refused(Refusal::new(kind, message))
}

fn arithmetic(error: NumberError) -> Stop {
    refused(ErrorKind::Arithmetic, error.to_string())
}

fn incomplete(completion: TextCompletion) -> Stop {
    Stop::missing(VariableState::Unavailable(super::interp::incomplete(
        completion,
    )))
}

fn own(value: &Loaded) -> Result<Rules, Stop> {
    value.language.ok_or_else(|| {
        Stop::Failed(crate::Error::internal(
            "an operation on a dynamic value has one of the program's",
        ))
    })
}

fn rules(left: &Loaded, right: &Loaded) -> Result<Rules, Stop> {
    match (left.language, right.language) {
        (Some(left_rules), Some(right_rules)) if left_rules != right_rules => Err(refused(
            ErrorKind::Unsupported,
            format!(
                "the `{}` is a {} value and the `{}` a {} value; microscope does not combine \
                 values of two languages",
                left.type_name,
                left_rules.name(),
                right.type_name,
                right_rules.name()
            ),
        )),
        (Some(rules), _) | (None, Some(rules)) => Ok(rules),
        (None, None) => Err(Stop::Failed(crate::Error::internal(
            "an operation on dynamic values has one of the program's",
        ))),
    }
}

fn adopt(value: &Loaded, rules: Rules) -> Cow<'_, Loaded> {
    if value.language.is_some() {
        return Cow::Borrowed(value);
    }

    let adopted = match (&value.value, rules) {
        (LoadedValue::Float(float), _) => LoadedValue::Float(float.convert(FloatFormat::Binary64)),
        (LoadedValue::Int(integer), Rules::JavaScript) => {
            LoadedValue::Float(Float::from_exact(*integer, FloatFormat::Binary64))
        }
        _ => return Cow::Borrowed(value),
    };

    Cow::Owned(Loaded {
        type_name: value.type_name.clone(),
        language: Some(rules),
        value: adopted,
    })
}

fn computed(rules: Rules, value: LoadedValue) -> Loaded {
    let type_name = match (rules, &value) {
        (Rules::Python, LoadedValue::Bool(_)) => "bool",
        (Rules::Python, LoadedValue::Int(_)) => "int",
        (Rules::Python, _) => "float",
        (Rules::JavaScript, LoadedValue::Bool(_)) => "boolean",
        (Rules::JavaScript, LoadedValue::Int(_)) => "bigint",
        (Rules::JavaScript, _) => "number",
    };

    Loaded {
        type_name: type_name.into(),
        language: Some(rules),
        value,
    }
}

pub fn shown(value: &Loaded) -> Option<String> {
    use crate::view::summary::{javascript_number, python_float};
    Some(match (value.language?, &value.value) {
        (Rules::Python, LoadedValue::Bool(true)) => "True".to_owned(),
        (Rules::Python, LoadedValue::Bool(false)) => "False".to_owned(),
        (Rules::Python, LoadedValue::Int(value)) => value.to_string(),
        (Rules::Python, LoadedValue::Float(value)) => python_float(value.to_f64()),
        (Rules::JavaScript, LoadedValue::Bool(value)) => value.to_string(),
        (Rules::JavaScript, LoadedValue::Int(value)) => format!("{value}n"),
        (Rules::JavaScript, LoadedValue::Float(value)) => javascript_number(value.to_f64()),
        _ => return None,
    })
}

fn nearest(value: Exact) -> f64 {
    Float::from_exact(value, FloatFormat::Binary64).to_f64()
}

fn text_truth(text: &TextSummary) -> Result<bool, Stop> {
    if !text.bytes.is_empty() {
        return Ok(true);
    }

    match text.completion {
        TextCompletion::Complete => Ok(false),
        completion => Err(incomplete(completion)),
    }
}

fn holds(op: BinaryOp, order: Option<Ordering>) -> bool {
    match (op, order) {
        (BinaryOp::Ne, None) => true,
        (_, None) => false,
        (BinaryOp::Eq, Some(order)) => order == Ordering::Equal,
        (BinaryOp::Ne, Some(order)) => order != Ordering::Equal,
        (BinaryOp::Lt, Some(order)) => order == Ordering::Less,
        (BinaryOp::Le, Some(order)) => order != Ordering::Greater,
        (BinaryOp::Gt, Some(order)) => order == Ordering::Greater,
        (_, Some(order)) => order != Ordering::Less,
    }
}

fn texts(
    left: &TextSummary,
    right: &TextSummary,
    equality: bool,
    whole: impl FnOnce(&[u8], &[u8]) -> Result<Ordering, Stop>,
) -> Result<Ordering, Stop> {
    let (left_bytes, right_bytes) = (left.bytes.as_ref(), right.bytes.as_ref());
    let complete = |text: &TextSummary| text.completion == TextCompletion::Complete;
    if complete(left) && complete(right) {
        return whole(left_bytes, right_bytes);
    }

    if equality
        && left_bytes
            .iter()
            .zip(right_bytes)
            .any(|(left, right)| left != right)
    {
        return Ok(Ordering::Less);
    }

    let partial = if complete(left) { right } else { left };
    Err(incomplete(partial.completion))
}

pub fn truth(value: &Loaded) -> Result<bool, Stop> {
    match own(value)? {
        Rules::Python => python::truth(value),
        Rules::JavaScript => javascript::truth(value),
    }
}

pub fn compare(op: BinaryOp, left: &Loaded, right: &Loaded) -> Result<bool, Stop> {
    let rules = rules(left, right)?;
    let (left, right) = (adopt(left, rules), adopt(right, rules));
    match rules {
        Rules::Python => python::compare(op, &left, &right),
        Rules::JavaScript => javascript::compare(op, &left, &right),
    }
}

pub fn binary(op: BinaryOp, left: &Loaded, right: &Loaded) -> Result<Loaded, Stop> {
    let rules = rules(left, right)?;
    let (left, right) = (adopt(left, rules), adopt(right, rules));
    let value = match rules {
        Rules::Python => python::binary(op, &left, &right),
        Rules::JavaScript => javascript::binary(op, &left, &right),
    }?;
    Ok(computed(rules, value))
}

pub fn unary(op: UnaryOp, operand: &Loaded) -> Result<Loaded, Stop> {
    let rules = own(operand)?;
    let value = match rules {
        Rules::Python => python::unary(op, operand),
        Rules::JavaScript => javascript::unary(op, operand),
    }?;
    Ok(computed(rules, value))
}

pub fn key(value: &Loaded) -> Result<Key, Stop> {
    match own(value)? {
        Rules::Python => python::key(value),
        Rules::JavaScript => javascript::key(value),
    }
}
