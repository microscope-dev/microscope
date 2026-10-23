//! One-line summaries in one style for every language and client
//! (`docs/views.md`): `"text"`, `len=3 [1, 2, 3]`, `None`. Clients render
//! values with these too, so a summary and a printed value agree.

use std::fmt::Write as _;
use std::sync::Arc;

use rustc_apfloat::Float as _;
use rustc_apfloat::ieee::{BFloat, Half, Quad, X87DoubleExtended};

use crate::{
    BaseType, BaseTypeEncoding, FloatValue, IntegerValue, PresentedCount, PresentedShape,
    ScalarValue, TextCompletion, TextSummary, TypeInfo, TypeKind, ValueChildren, VariableState,
    VariableUnavailableReason, VariableValue, VirtualAddress,
};

/// The most elements a summary previews.
pub const MAX_ELEMENTS: usize = 16;

/// The most characters a summary's elements take before it stops.
pub const MAX_CHARACTERS: usize = 96;

/// Text in double quotes, escaping what is not printable, saying when more
/// text follows or could not be read.
#[must_use]
pub fn quoted(text: &TextSummary) -> String {
    let mut output = String::from("\"");
    for chunk in text.bytes.utf8_chunks() {
        for character in chunk.valid().chars() {
            match character {
                '"' => output.push_str("\\\""),
                '\\' => output.push_str("\\\\"),
                '\n' => output.push_str("\\n"),
                '\t' => output.push_str("\\t"),
                '\r' => output.push_str("\\r"),
                character if character.is_control() => {
                    output.push_str(&character.escape_unicode().to_string());
                }
                character => output.push(character),
            }
        }

        for byte in chunk.invalid() {
            let _ = write!(output, "\\x{byte:02x}");
        }
    }

    output.push('"');

    match text.completion {
        TextCompletion::Complete => {}
        TextCompletion::Truncated { length: None } => output.push_str("..."),
        TextCompletion::Truncated {
            length: Some(length),
        } => {
            let _ = write!(output, "... ({length} bytes)");
        }
        TextCompletion::Unreadable { address } => {
            let _ = write!(output, "... <unreadable at {address}>");
        }
        TextCompletion::Limited { length, exhaustion } => {
            if let Some(length) = length {
                let _ = write!(output, "... ({length} bytes)");
            } else {
                output.push_str("...");
            }

            let _ = write!(
                output,
                " <{}>",
                VariableUnavailableReason::InspectionLimit(exhaustion)
            );
        }
    }

    output
}

/// An integer, in decimal.
#[must_use]
pub fn integer(value: IntegerValue) -> String {
    match value {
        IntegerValue::Signed(value) => value.to_string(),
        IntegerValue::Unsigned(value) => value.to_string(),
    }
}

/// Which characters a scalar's numbers stand for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Characters {
    /// None: the scalar is a number.
    None,

    /// Bytes of an encoding the type does not say, of which only ASCII
    /// is known: C's `char`.
    Bytes,

    /// Unicode code points: UTF-16 and UTF-32 units, `wchar_t`, and
    /// Rust's `char`.
    Unicode,
}

/// A scalar, with the printable character a character type's value stands
/// for: an ASCII one for bytes, and any but a control character for
/// Unicode.
#[must_use]
pub fn scalar(value: &ScalarValue, characters: Characters) -> String {
    let with_character = |number: String, code: Option<u32>| {
        let shown = code
            .and_then(char::from_u32)
            .filter(|character| match characters {
                Characters::None => false,
                Characters::Bytes => character.is_ascii_graphic(),
                Characters::Unicode => !character.is_control(),
            });
        shown.map_or_else(
            || number.clone(),
            |character| format!("{number} '{}'", character.escape_debug()),
        )
    };

    match value {
        ScalarValue::Boolean(value) => value.to_string(),
        ScalarValue::Signed(value) => with_character(value.to_string(), u32::try_from(*value).ok()),
        ScalarValue::Unsigned(value) => {
            with_character(value.to_string(), u32::try_from(*value).ok())
        }
        ScalarValue::Floating(value) => float(*value),
        ScalarValue::Complex { real, imaginary } => {
            let imaginary = float(*imaginary);
            let sign = if imaginary.starts_with('-') { "" } else { "+" };
            format!("({}{sign}{imaginary}i)", float(*real))
        }
    }
}

/// A float, exactly as its format holds it: its shortest digits that read
/// back as it, with an exponent when it is very large or very small.
#[must_use]
pub fn float(value: FloatValue) -> String {
    match value {
        FloatValue::Binary16(bits) => soft_shortest(Half::from_bits(u128::from(bits))),
        FloatValue::BFloat16(bits) => soft_shortest(BFloat::from_bits(u128::from(bits))),
        FloatValue::Binary128(bits) => soft_shortest(Quad::from_bits(bits)),
        FloatValue::Binary32(bits) => shortest(f32::from_bits(bits)),
        FloatValue::Binary64(bits) => shortest(f64::from_bits(bits)),
        FloatValue::X87Extended {
            significand,
            sign_exponent,
        } => soft_shortest(X87DoubleExtended::from_bits(
            u128::from(significand) | (u128::from(sign_exponent) << 64),
        )),
    }
}

/// [`shortest`] for a float format Rust has no type for: the fewest
/// significant digits that read back as the value, written as Rust writes
/// an `f64`.
fn soft_shortest<F: rustc_apfloat::Float + std::fmt::Display>(value: F) -> String {
    if value.is_nan() {
        return "NaN".to_owned();
    }

    if value.is_infinite() {
        return if value.is_negative() { "-inf" } else { "inf" }.to_owned();
    }

    if value.is_zero() {
        return if value.is_negative() { "-0" } else { "0" }.to_owned();
    }

    let natural = value.to_string();
    let digits = (1..64)
        .map(|precision| format!("{value:.precision$}"))
        .find(|text| {
            F::from_str_r(text, rustc_apfloat::Round::NearestTiesToEven)
                .is_ok_and(|parsed| parsed.value.to_bits() == value.to_bits())
        })
        .unwrap_or(natural);
    decimal(&digits).unwrap_or(digits)
}

/// A decimal number apfloat wrote, as `-1.25E+30` or `0.001`, written as
/// [`shortest`] writes one: plain digits from 1e-6 to 1e21, and otherwise
/// a significand and exponent, as `-1.25e30`.
fn decimal(text: &str) -> Option<String> {
    let (negative, text) = text
        .strip_prefix('-')
        .map_or((false, text), |rest| (true, rest));
    let (significand, exponent) = match text.split_once(['E', 'e']) {
        Some((significand, exponent)) => (significand, exponent.parse::<i32>().ok()?),
        None => (text, 0),
    };

    let (whole, fraction) = significand.split_once('.').unwrap_or((significand, ""));
    if !whole
        .chars()
        .chain(fraction.chars())
        .all(|c| c.is_ascii_digit())
    {
        return None;
    }

    // The digits, and where the decimal point falls among them.
    let all = format!("{whole}{fraction}");
    let leading = all.len() - all.trim_start_matches('0').len();
    let digits = all.trim_matches('0');
    if digits.is_empty() {
        return Some(if negative { "-0" } else { "0" }.to_owned());
    }

    let point = i32::try_from(whole.len()).ok()? + exponent - i32::try_from(leading).ok()?;

    // The exponent of the first significant digit.
    let scientific = point - 1;
    let sign = if negative { "-" } else { "" };
    let length = i32::try_from(digits.len()).ok()?;

    Some(if (-6..21).contains(&scientific) {
        if point <= 0 {
            format!(
                "{sign}0.{}{digits}",
                "0".repeat(usize::try_from(-point).ok()?)
            )
        } else if point >= length {
            format!(
                "{sign}{digits}{}",
                "0".repeat(usize::try_from(point - length).ok()?)
            )
        } else {
            let (integer, rest) = digits.split_at(usize::try_from(point).ok()?);
            format!("{sign}{integer}.{rest}")
        }
    } else {
        let (first, rest) = digits.split_at(1);
        let rest = if rest.is_empty() {
            String::new()
        } else {
            format!(".{rest}")
        };

        format!("{sign}{first}{rest}e{scientific}")
    })
}

/// A float's shortest round-trip digits, which take an exponent past 1e21
/// or below 1e-6, as JavaScript writes numbers, rather than hundreds of
/// zeros.
fn shortest<F: std::fmt::Display + std::fmt::LowerExp>(value: F) -> String {
    let scientific = format!("{value:e}");
    let exponent = scientific
        .rsplit_once('e')
        .and_then(|(_, exponent)| exponent.parse::<i32>().ok());
    match exponent {
        Some(exponent) if !(-6..21).contains(&exponent) => scientific,
        _ => value.to_string(),
    }
}

/// ECMA-262 `Number::toString(x)` in radix 10, built from the shortest
/// round-trip digits that Rust's exponent formatting produces.
#[must_use]
pub fn number_to_string(x: f64) -> String {
    if x.is_nan() {
        return "NaN".to_owned();
    }
    if x == 0.0 {
        return "0".to_owned();
    }
    if x < 0.0 {
        return format!("-{}", number_to_string(-x));
    }
    if x.is_infinite() {
        return "Infinity".to_owned();
    }
    let scientific = format!("{x:e}");
    let (mantissa, exponent) = scientific.split_once('e').unwrap_or((&scientific, "0"));
    let digits: String = mantissa.chars().filter(char::is_ascii_digit).collect();
    let Ok(exponent) = exponent.parse::<i64>() else {
        return scientific;
    };
    let k = i64::try_from(digits.len()).unwrap_or(i64::MAX);
    let n = exponent + 1;
    if k <= n && n <= 21 {
        let zeros = usize::try_from(n - k).unwrap_or(0);
        return format!("{digits}{}", "0".repeat(zeros));
    }
    if 0 < n && n <= 21 {
        let point = usize::try_from(n).unwrap_or(0);
        return format!("{}.{}", &digits[..point], &digits[point..]);
    }
    if -6 < n && n <= 0 {
        let zeros = usize::try_from(-n).unwrap_or(0);
        return format!("0.{}{digits}", "0".repeat(zeros));
    }
    let sign = if n - 1 < 0 { '-' } else { '+' };
    let magnitude = (n - 1).unsigned_abs();
    let (first, rest) = digits.split_at(1);
    if rest.is_empty() {
        format!("{first}e{sign}{magnitude}")
    } else {
        format!("{first}.{rest}e{sign}{magnitude}")
    }
}

/// util.inspect's `formatNumber`: `Number::toString`, but `-0` for
/// negative zero.
#[must_use]
pub fn javascript_number(x: f64) -> String {
    if x == 0.0 && x.is_sign_negative() {
        return "-0".to_owned();
    }

    number_to_string(x)
}

/// Python's `repr` of a float: its shortest round-trip digits, in
/// exponent notation when the exponent is below -4 or at least 16.
#[must_use]
pub fn python_float(x: f64) -> String {
    if x.is_nan() {
        return "nan".to_owned();
    }

    let sign = if x.is_sign_negative() { "-" } else { "" };
    if x.is_infinite() {
        return format!("{sign}inf");
    }

    if x == 0.0 {
        return format!("{sign}0.0");
    }

    let scientific = format!("{:e}", x.abs());
    let (mantissa, exponent) = scientific.split_once('e').unwrap_or((&scientific, "0"));
    let digits: String = mantissa.chars().filter(char::is_ascii_digit).collect();
    let Ok(exponent) = exponent.parse::<i64>() else {
        return format!("{sign}{scientific}");
    };

    let length = i64::try_from(digits.len()).unwrap_or(i64::MAX);
    let zeros = |count: i64| "0".repeat(usize::try_from(count).unwrap_or(0));
    if (-4..16).contains(&exponent) {
        // How many digits come before the point.
        let point = exponent + 1;
        return if point <= 0 {
            format!("{sign}0.{}{digits}", zeros(-point))
        } else if point >= length {
            format!("{sign}{digits}{}.0", zeros(point - length))
        } else {
            let (whole, fraction) = digits.split_at(usize::try_from(point).unwrap_or(0));
            format!("{sign}{whole}.{fraction}")
        };
    }

    let (first, rest) = digits.split_at(1);
    let point = if rest.is_empty() { "" } else { "." };
    let exponent_sign = if exponent < 0 { '-' } else { '+' };
    format!(
        "{sign}{first}{point}{rest}e{exponent_sign}{:02}",
        exponent.unsigned_abs()
    )
}

/// Which characters a type's integers stand for: a one-byte character
/// type's are bytes, and a wider one's Unicode code points.
#[must_use]
pub const fn characters(type_info: &TypeInfo) -> Characters {
    match &type_info.kind {
        TypeKind::Base(BaseType {
            encoding: BaseTypeEncoding::SignedCharacter | BaseTypeEncoding::UnsignedCharacter,
            byte_size,
            ..
        }) => {
            if *byte_size == 1 {
                Characters::Bytes
            } else {
                Characters::Unicode
            }
        }
        _ => Characters::None,
    }
}

/// A value on one line: its presentation's summary when a view presents
/// it, its text, a scalar, or a placeholder for an aggregate's parts.
#[must_use]
pub fn value(type_info: Option<&TypeInfo>, state: &VariableState) -> String {
    let (value, text, presentation, children) = match state {
        VariableState::Available {
            value,
            text,
            presentation,
            children,
            ..
        } => (value, text, presentation, children),
        VariableState::Unavailable(_) => return "<unavailable>".to_owned(),
        VariableState::Malformed(_) => return "<malformed>".to_owned(),
        VariableState::Invalid { .. } => return "<invalid>".to_owned(),
    };

    if let Some(presentation) = presentation
        && presentation.shape != PresentedShape::Raw
    {
        return presentation.summary.to_string();
    }

    if let (Some(text), false) = (text, matches!(value, VariableValue::Address(_))) {
        return quoted(text);
    }

    let characters = type_info.map_or(Characters::None, characters);
    let partless = matches!(children, ValueChildren::Available(parts) if parts.total() == 0);
    let rendered = match value {
        VariableValue::Scalar(value) => scalar(value, characters),
        VariableValue::Enumeration { value, matches } => {
            symbol(*value, matches).unwrap_or_else(|| integer(*value))
        }
        VariableValue::Address(address) => {
            let text = format!("{:#x}", address.address.get());
            address
                .function
                .as_ref()
                .map_or_else(|| text.clone(), |function| format!("{text} <{function}>"))
        }
        VariableValue::ImplicitPointer => "<implicit pointer>".to_owned(),
        VariableValue::NotAllocated => "<not allocated>".to_owned(),
        VariableValue::NotAssociated => "<not associated>".to_owned(),
        VariableValue::Function { code, function } => self::function(*code, function.as_deref()),
        VariableValue::Array { .. } | VariableValue::Slice { .. } => "[…]".to_owned(),
        // A record with no parts has nothing to elide; Rust's is `()`.
        VariableValue::Record if partless => {
            if type_info.is_some_and(|info| info.name.as_ref() == "()") {
                "()".to_owned()
            } else {
                "{}".to_owned()
            }
        }
        VariableValue::Record | VariableValue::Union | VariableValue::Variant { .. } => {
            "{…}".to_owned()
        }
    };

    match (text, value) {
        (Some(text), VariableValue::Address(_)) => format!("{rendered} {}", quoted(text)),
        _ => rendered,
    }
}

/// The name an enumeration-like value has: its first exact constant's, or
/// the flag constants it combines, joined by `|`.
#[must_use]
pub fn symbol(value: IntegerValue, matches: &[crate::Enumerator]) -> Option<String> {
    if matches.iter().any(|enumerator| enumerator.value == value) {
        return matches
            .first()
            .map(|enumerator| enumerator.name.to_string());
    }

    (!matches.is_empty()).then(|| {
        matches
            .iter()
            .map(|enumerator| enumerator.name.as_ref())
            .collect::<Vec<_>>()
            .join("|")
    })
}

/// A function value: the function it calls, `nil`, or the address of
/// code no debug information names.
#[must_use]
pub fn function(code: Option<VirtualAddress>, function: Option<&str>) -> String {
    match (code, function) {
        (None, _) => "nil".to_owned(),
        (Some(_), Some(function)) => function.to_owned(),
        (Some(code), None) => format!("{:#x}", code.get()),
    }
}

/// A count as a summary begins: `len=3`, or `len>=3` when there are at
/// least that many.
fn length(count: PresentedCount) -> String {
    match count {
        PresentedCount::Exact(count) => format!("len={count}"),
        PresentedCount::AtLeast(count) => format!("len>={count}"),
    }
}

/// A sequence's summary: its length and the elements previewed.
#[must_use]
pub fn sequence(count: PresentedCount, elements: &[String], complete: bool) -> String {
    bracketed(count, elements, complete, ('[', ']'))
}

/// A map's summary: its length and the entries previewed, each `key:
/// value`.
#[must_use]
pub fn map(count: PresentedCount, entries: &[String], complete: bool) -> String {
    bracketed(count, entries, complete, ('{', '}'))
}

/// A record's summary: `{name: value, …}`, or `(value, …)` when its
/// members are positions, as a tuple's are. `complete` says whether every
/// member is in `members`.
#[must_use]
pub fn record(members: &[(Arc<str>, String)], complete: bool) -> String {
    let positional = members
        .iter()
        .enumerate()
        .all(|(index, (name, _))| name.as_ref() == index.to_string());
    let (open, close) = if positional { ('(', ')') } else { ('{', '}') };
    let mut output = String::from(open);

    for (index, (name, value)) in members.iter().enumerate() {
        if index > 0 {
            output.push_str(", ");
        }

        if output.chars().count() > MAX_CHARACTERS {
            output.push('…');
            output.push(close);
            return output;
        }

        if !positional {
            output.push_str(name);
            output.push_str(": ");
        }

        output.push_str(value);
    }

    if !complete {
        output.push_str(if members.is_empty() { "…" } else { ", …" });
    }

    output.push(close);
    output
}

fn bracketed(
    count: PresentedCount,
    items: &[String],
    complete: bool,
    (open, close): (char, char),
) -> String {
    let mut output = format!("{} {open}", length(count));

    for (index, item) in items.iter().enumerate() {
        if index > 0 {
            output.push_str(", ");
        }

        output.push_str(item);
    }

    if !complete {
        output.push_str(if items.is_empty() { "…" } else { ", …" });
    }

    output.push(close);
    output
}
