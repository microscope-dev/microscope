//! Text conversions inspect needs: string escaping, JavaScript's
//! `Number::toString` and `BigInt::toString`, `Date`'s `toISOString`, and
//! entry display width.

use std::fmt::Write as _;

use super::BigInt;
// `Number::toString` lives with the summaries, which computed values use too.
pub use crate::view::summary::number_to_string;

/// `meta` in inspect.js: the escape of each code unit below 0xA0 that has one.
fn meta(unit: u16) -> Option<&'static str> {
    const LOW: [&str; 32] = [
        "\\x00", "\\x01", "\\x02", "\\x03", "\\x04", "\\x05", "\\x06", "\\x07", "\\b", "\\t",
        "\\n", "\\x0B", "\\f", "\\r", "\\x0E", "\\x0F", "\\x10", "\\x11", "\\x12", "\\x13",
        "\\x14", "\\x15", "\\x16", "\\x17", "\\x18", "\\x19", "\\x1A", "\\x1B", "\\x1C", "\\x1D",
        "\\x1E", "\\x1F",
    ];
    const HIGH: [&str; 33] = [
        "\\x7F", "\\x80", "\\x81", "\\x82", "\\x83", "\\x84", "\\x85", "\\x86", "\\x87", "\\x88",
        "\\x89", "\\x8A", "\\x8B", "\\x8C", "\\x8D", "\\x8E", "\\x8F", "\\x90", "\\x91", "\\x92",
        "\\x93", "\\x94", "\\x95", "\\x96", "\\x97", "\\x98", "\\x99", "\\x9A", "\\x9B", "\\x9C",
        "\\x9D", "\\x9E", "\\x9F",
    ];
    match unit {
        0x00..=0x1f => LOW.get(usize::from(unit)).copied(),
        0x27 => Some("\\'"),
        0x5c => Some("\\\\"),
        0x7f..=0x9f => HIGH.get(usize::from(unit - 0x7f)).copied(),
        _ => None,
    }
}

/// Escapes control characters, backslashes, lone surrogates, and (if
/// `single`) single quotes: `strEscapeSequencesReplacer`, its `Single`
/// variant, and `escapeFn`.
pub fn escape(units: &[u16], single: bool) -> String {
    let mut kept: Vec<u16> = Vec::with_capacity(units.len());
    let mut out = String::with_capacity(units.len());
    let mut i = 0;
    while let Some(&unit) = units.get(i) {
        let escaped = match unit {
            0x27 if !single => None,
            0xd800..=0xdbff => {
                if units
                    .get(i + 1)
                    .is_some_and(|next| (0xdc00..=0xdfff).contains(next))
                {
                    kept.push(unit);
                    kept.push(units[i + 1]);
                    i += 2;
                    continue;
                }
                Some(format!("\\u{unit:x}"))
            }
            0xdc00..=0xdfff => Some(format!("\\u{unit:x}")),
            _ => meta(unit).map(str::to_owned),
        };
        match escaped {
            Some(text) => {
                out.push_str(&String::from_utf16_lossy(&kept));
                kept.clear();
                out.push_str(&text);
            }
            None => kept.push(unit),
        }
        i += 1;
    }
    out.push_str(&String::from_utf16_lossy(&kept));
    out
}

/// `strEscape`: quotes and escapes the units, picking a quote that needs no
/// escaping if one exists.
pub fn str_escape(units: &[u16]) -> String {
    const SINGLE: u16 = 0x27;
    const DOUBLE: u16 = 0x22;
    const BACKTICK: u16 = 0x60;
    let has = |unit| units.contains(&unit);
    let has_template = units.windows(2).any(|pair| pair == [0x24, 0x7b]);
    let quote = if !has(SINGLE) {
        '\''
    } else if !has(DOUBLE) {
        '"'
    } else if !has(BACKTICK) && !has_template {
        '`'
    } else {
        '\''
    };
    format!("{quote}{}{quote}", escape(units, quote == '\''))
}

/// `keyStrRegExp`: whether a key prints bare.
pub fn is_identifier(key: &str) -> bool {
    let mut chars = key.chars();
    chars
        .next()
        .is_some_and(|first| first.is_ascii_alphabetic() || first == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// A string's length in UTF-16 units, JavaScript's `length`.
pub fn js_len(text: &str) -> usize {
    text.chars().map(char::len_utf16).sum()
}

/// `text` padded on the left with spaces to `width` UTF-16 units.
pub fn pad_start(text: &str, width: usize) -> String {
    let fill = width.saturating_sub(js_len(text));
    format!("{}{text}", " ".repeat(fill))
}

/// `text` padded on the right with spaces to `width` UTF-16 units.
pub fn pad_end(text: &str, width: usize) -> String {
    let fill = width.saturating_sub(js_len(text));
    format!("{text}{}", " ".repeat(fill))
}

/// `getStringWidth(text, false)`: an entry's width in columns. Node uses ICU
/// once a string has a unit at or past 0x7F. Below U+0300 ICU's widths are
/// reproduced (1 per printable character, 0 per control character); past
/// that the second value is false and the width is Node's no-ICU estimate.
pub fn string_width(text: &str) -> (usize, bool) {
    let mut width = 0;
    let mut exact = true;
    for c in text.chars() {
        let code = u32::from(c);
        width += match code {
            0x00..=0x1f | 0x7f..=0x9f => 0,
            0x20..=0x7e | 0xa0..=0x2ff => 1,
            _ => {
                exact = false;
                if is_full_width(code) {
                    2
                } else {
                    usize::from(!is_zero_width(code))
                }
            }
        };
    }
    (width, exact)
}

/// `isFullWidthCodePoint` in Node's build without ICU.
const fn is_full_width(code: u32) -> bool {
    code >= 0x1100
        && (code <= 0x115f
            || code == 0x2329
            || code == 0x232a
            || (code >= 0x2e80 && code <= 0x3247 && code != 0x303f)
            || (code >= 0x3250 && code <= 0x4dbf)
            || (code >= 0x4e00 && code <= 0xa4c6)
            || (code >= 0xa960 && code <= 0xa97c)
            || (code >= 0xac00 && code <= 0xd7a3)
            || (code >= 0xf900 && code <= 0xfaff)
            || (code >= 0xfe10 && code <= 0xfe19)
            || (code >= 0xfe30 && code <= 0xfe6b)
            || (code >= 0xff01 && code <= 0xff60)
            || (code >= 0xffe0 && code <= 0xffe6)
            || (code >= 0x1b000 && code <= 0x1b001)
            || (code >= 0x1f200 && code <= 0x1f251)
            || (code >= 0x1f300 && code <= 0x1f64f)
            || (code >= 0x20000 && code <= 0x3fffd))
}

/// `isZeroWidthCodePoint`.
const fn is_zero_width(code: u32) -> bool {
    code <= 0x1f
        || (code >= 0x7f && code <= 0x9f)
        || (code >= 0x300 && code <= 0x36f)
        || (code >= 0x200b && code <= 0x200f)
        || (code >= 0x20d0 && code <= 0x20ff)
        || (code >= 0xfe00 && code <= 0xfe0f)
        || (code >= 0xfe20 && code <= 0xfe2f)
        || (code >= 0xe0100 && code <= 0xe01ef)
}

/// A `BigInt`'s decimal digits with its sign.
pub fn bigint_to_string(value: &BigInt) -> String {
    const CHUNK: u128 = 10_000_000_000_000_000_000;
    let mut digits: Vec<u64> = value.magnitude.clone();
    while digits.last() == Some(&0) {
        digits.pop();
    }
    if digits.is_empty() {
        return "0".to_owned();
    }
    let mut chunks = Vec::new();
    while !digits.is_empty() {
        let mut remainder: u128 = 0;
        for digit in digits.iter_mut().rev() {
            let current = (remainder << 64) | u128::from(*digit);
            #[allow(
                clippy::cast_possible_truncation,
                reason = "the quotient of a value below CHUNK << 64 by CHUNK fits"
            )]
            {
                *digit = (current / CHUNK) as u64;
            }
            remainder = current % CHUNK;
        }
        chunks.push(remainder);
        while digits.last() == Some(&0) {
            digits.pop();
        }
    }
    let mut text = String::new();
    if value.negative {
        text.push('-');
    }
    let mut chunks = chunks.iter().rev();
    if let Some(first) = chunks.next() {
        let _ = write!(text, "{first}");
    }
    for chunk in chunks {
        let _ = write!(text, "{chunk:019}");
    }
    text
}

/// `Date.prototype.toISOString` of a valid time value, or `None` outside a
/// Date's range.
pub fn iso_date(time: f64) -> Option<String> {
    const DAY: i64 = 86_400_000;
    if !time.is_finite() || time.abs() > 8.64e15 || time.fract() != 0.0 {
        return None;
    }
    #[allow(
        clippy::cast_possible_truncation,
        reason = "an integral value within 8.64e15 converts exactly"
    )]
    let time = time as i64;
    let days = time.div_euclid(DAY);
    let ms = time.rem_euclid(DAY);
    let (year, month, day) = civil_from_days(days);
    let year = if (0..=9999).contains(&year) {
        format!("{year:04}")
    } else if year < 0 {
        format!("-{:06}", year.unsigned_abs())
    } else {
        format!("+{year:06}")
    };
    Some(format!(
        "{year}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{:03}Z",
        ms / 3_600_000,
        ms / 60_000 % 60,
        ms / 1000 % 60,
        ms % 1000,
    ))
}

/// The proleptic Gregorian date of a day count from 1970-01-01 (Howard
/// Hinnant's `civil_from_days`).
const fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + if month <= 2 { 1 } else { 0 };
    (year, month, day)
}

/// An IEEE 754 half-precision number's value.
pub fn f16_to_f64(bits: u16) -> f64 {
    let sign = if bits & 0x8000 == 0 { 1.0 } else { -1.0 };
    let exponent = i32::from((bits >> 10) & 0x1f);
    let fraction = f64::from(bits & 0x3ff);
    match exponent {
        0 => sign * fraction * 2f64.powi(-24),
        0x1f if fraction == 0.0 => sign * f64::INFINITY,
        0x1f => f64::NAN,
        _ => sign * (1.0 + fraction / 1024.0) * 2f64.powi(exponent - 15),
    }
}
