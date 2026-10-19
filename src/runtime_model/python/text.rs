//! The text the debugger presents a built-in Python value as: Python's
//! `repr` for ints at any size, floats, bytes, and strings. A string
//! escapes a backslash, its quote, and each character every supported
//! interpreter's Unicode database says does not print (`printable.gen.rs`):
//! the controls, separators but the space, format characters, private
//! use, lone surrogates, and code points no version assigns. A character
//! that a later version than the interpreter's assigns is kept as it is,
//! where that interpreter's `repr` escapes it.
//! `tests/fixtures/python/truth.py` defines the same text.

use std::fmt::Write as _;

#[path = "printable.gen.rs"]
mod printable;

/// Whether every supported interpreter's `repr` keeps the code point
/// `point` as it is.
fn printable(point: u32) -> bool {
    let after = printable::ESCAPED.partition_point(|(first, _)| *first <= point);
    after
        .checked_sub(1)
        .and_then(|index| printable::ESCAPED.get(index))
        .is_none_or(|(_, past)| point >= *past)
}

/// A string's characters as CPython stores them: code points, which may be
/// lone surrogates, as no Rust `char` may be.
pub(super) fn string(code_points: &[u32]) -> String {
    let single = code_points.contains(&u32::from(b'\''));
    let double = code_points.contains(&u32::from(b'"'));
    let quote = if single && !double { '"' } else { '\'' };
    let mut text = String::with_capacity(code_points.len() + 2);
    text.push(quote);

    for &point in code_points {
        match char::from_u32(point) {
            Some('\\') => text.push_str("\\\\"),
            Some(character) if character == quote => {
                text.push('\\');
                text.push(character);
            }
            Some('\n') => text.push_str("\\n"),
            Some('\r') => text.push_str("\\r"),
            Some('\t') => text.push_str("\\t"),
            // A lone surrogate, which is all from_u32 refuses below
            // 0x110000, does not print; CPython stores none higher.
            Some(character) if printable(point) => text.push(character),
            _ if point < 0x100 => {
                let _ = write!(text, "\\x{point:02x}");
            }
            _ if point < 0x1_0000 => {
                let _ = write!(text, "\\u{point:04x}");
            }
            _ => {
                let _ = write!(text, "\\U{point:08x}");
            }
        }
    }

    text.push(quote);
    text
}

/// Code points as UTF-8, with each lone surrogate encoded as a scalar
/// value would be, as CPython's `surrogatepass` handler encodes it: the
/// bytes sort as the code points do.
pub(super) fn utf8(code_points: &[u32]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(code_points.len());

    for &point in code_points {
        if let Some(character) = char::from_u32(point) {
            let mut buffer = [0; 4];
            bytes.extend_from_slice(character.encode_utf8(&mut buffer).as_bytes());
        } else {
            // A surrogate, which a str may hold alone: three bytes, as
            // every code point from U+0800 to U+FFFF takes.
            let [_, _, high, low] = point.to_be_bytes();
            let point = u16::from_be_bytes([high, low]);
            bytes.extend_from_slice(&[
                0xe0 | u8::try_from(point >> 12).unwrap_or(0),
                0x80 | u8::try_from((point >> 6) & 0x3f).unwrap_or(0),
                0x80 | u8::try_from(point & 0x3f).unwrap_or(0),
            ]);
        }
    }

    bytes
}

/// A `bytes` object's contents, as `repr` gives them.
pub(super) fn bytes(contents: &[u8]) -> String {
    let quote = if contents.contains(&b'\'') && !contents.contains(&b'"') {
        b'"'
    } else {
        b'\''
    };

    let mut text = String::with_capacity(contents.len() + 3);
    text.push('b');
    text.push(char::from(quote));

    for &byte in contents {
        match byte {
            b'\\' => text.push_str("\\\\"),
            b'\n' => text.push_str("\\n"),
            b'\r' => text.push_str("\\r"),
            b'\t' => text.push_str("\\t"),
            _ if byte == quote => {
                text.push('\\');
                text.push(char::from(byte));
            }
            0x20..0x7f => text.push(char::from(byte)),
            _ => {
                let _ = write!(text, "\\x{byte:02x}");
            }
        }
    }

    text.push(char::from(quote));
    text
}

/// A float as `repr` gives it: the shortest digits that read back as the
/// same float, in positional notation unless the decimal point would lie
/// more than 16 digits past the first or more than 4 before it.
pub(super) fn float(value: f64) -> String {
    if value.is_nan() {
        return "nan".to_owned();
    }

    let sign = if value.is_sign_negative() { "-" } else { "" };
    if value.is_infinite() {
        return format!("{sign}inf");
    }

    // Rust's exponent form gives the same shortest digits, as `d.ddde±x`.
    shortest(sign, &format!("{:e}", value.abs()))
}

/// A 32-bit float as [`float`] writes a float, with the shortest digits
/// that read back as the same 32-bit float.
pub(super) fn float32(value: f32) -> String {
    if value.is_nan() {
        return "nan".to_owned();
    }

    let sign = if value.is_sign_negative() { "-" } else { "" };
    if value.is_infinite() {
        return format!("{sign}inf");
    }

    shortest(sign, &format!("{:e}", value.abs()))
}

/// A float's text from its sign and its shortest digits in exponent form,
/// in positional notation where Python's `repr` writes it so.
fn shortest(sign: &str, shortest: &str) -> String {
    let (mantissa, exponent) = shortest.split_once('e').unwrap_or((shortest, "0"));
    let digits: String = mantissa.chars().filter(char::is_ascii_digit).collect();
    let exponent: i64 = exponent.parse().unwrap_or(0);

    // Where the decimal point lies, counted in digits from the first.
    let point = exponent + 1;
    let count = i64::try_from(digits.len()).unwrap_or(i64::MAX);
    let zeros = |count: i64| "0".repeat(usize::try_from(count).unwrap_or(0));

    if point <= -4 || point > 16 {
        let (first, rest) = digits.split_at(1);
        let fraction = if rest.is_empty() {
            String::new()
        } else {
            format!(".{rest}")
        };
        let exponent_sign = if exponent < 0 { '-' } else { '+' };
        format!(
            "{sign}{first}{fraction}e{exponent_sign}{:02}",
            exponent.unsigned_abs()
        )
    } else if point <= 0 {
        format!("{sign}0.{}{digits}", zeros(-point))
    } else if point < count {
        let (whole, fraction) = digits.split_at(usize::try_from(point).unwrap_or(0));
        format!("{sign}{whole}.{fraction}")
    } else {
        format!("{sign}{digits}{}.0", zeros(point - count))
    }
}

/// An int from its magnitude's digits of `shift` bits each, least
/// significant first, in decimal.
pub(super) fn int(negative: bool, digits: &[u32], shift: u32) -> String {
    // Base 10^9 limbs, least significant first, which each digit is
    // shifted into from the most significant down.
    const LIMB: u64 = 1_000_000_000;
    let mut limbs: Vec<u64> = Vec::new();

    for &digit in digits.iter().rev() {
        let mut carry = u64::from(digit);
        for limb in &mut limbs {
            let shifted = (*limb << shift) + carry;
            *limb = shifted % LIMB;
            carry = shifted / LIMB;
        }

        while carry != 0 {
            limbs.push(carry % LIMB);
            carry /= LIMB;
        }
    }

    let mut text = String::new();

    match limbs.split_last() {
        None => text.push('0'),
        Some((most, rest)) => {
            if negative {
                text.push('-');
            }

            let _ = write!(text, "{most}");
            for limb in rest.iter().rev() {
                let _ = write!(text, "{limb:09}");
            }
        }
    }

    text
}

#[cfg(test)]
mod tests {
    use super::*;

    fn points(text: &str) -> Vec<u32> {
        text.chars().map(u32::from).collect()
    }

    #[test]
    fn a_string_escapes_its_quote_controls_and_lone_surrogates() {
        for (value, expected) in [
            ("hello", "'hello'"),
            ("", "''"),
            ("it's", "\"it's\""),
            ("both ' and \"", "'both \\' and \"'"),
            ("tab\tnew\nreturn\r", "'tab\\tnew\\nreturn\\r'"),
            ("back\\slash", "'back\\\\slash'"),
            (
                "\u{0}\u{1f}\u{7f}\u{85}\u{9f}",
                "'\\x00\\x1f\\x7f\\x85\\x9f'",
            ),
            (
                "caf\u{e9} \u{65e5} \u{1f40d}",
                "'caf\u{e9} \u{65e5} \u{1f40d}'",
            ),
            (
                "\u{a0}\u{ad}\u{200b}\u{2028}\u{202e}\u{feff}",
                "'\\xa0\\xad\\u200b\\u2028\\u202e\\ufeff'",
            ),
            (
                "\u{e000}\u{e0001}\u{10ffff}",
                "'\\ue000\\U000e0001\\U0010ffff'",
            ),
        ] {
            assert_eq!(string(&points(value)), expected, "{value:?}");
        }

        assert_eq!(string(&[0x61, 0xd800, 0xdfff]), "'a\\ud800\\udfff'");
    }

    /// Each interpreter's `repr` of every code point alone, one to a line,
    /// with a newline's and a carriage return's as Python writes them.
    const REPRS: &str = "import sys\n\
        sys.stdout.write('\\n'.join(repr(chr(point)) for point in range(0x110000)))";

    /// Every code point is presented as each interpreter in the matrix
    /// writes it, but one that interpreter's Unicode version does not
    /// assign and another's does, which is kept as it is.
    #[test]
    fn each_character_is_as_every_interpreter_in_the_matrix_writes_it() {
        let root = std::env::var_os("MICROSCOPE_CPYTHON")
            .expect("MICROSCOPE_CPYTHON names the interpreters; run tests in the dev shell");
        let mut interpreters: Vec<_> = std::fs::read_dir(&root)
            .expect("list the interpreters")
            .map(|entry| entry.expect("an interpreter").path())
            .collect();
        interpreters.sort();
        let reprs: Vec<(String, Vec<String>)> = interpreters
            .iter()
            .map(|interpreter| {
                let output = std::process::Command::new(interpreter.join("python"))
                    .args(["-I", "-c", REPRS])
                    .output()
                    .expect("run the interpreter");
                assert!(output.status.success(), "{}", interpreter.display());
                let text = String::from_utf8(output.stdout).expect("UTF-8 reprs");
                let lines: Vec<String> = text.split('\n').map(str::to_owned).collect();
                assert_eq!(lines.len(), 0x11_0000, "{}", interpreter.display());
                (interpreter.display().to_string(), lines)
            })
            .collect();

        for point in 0..0x11_0000_u32 {
            let ours = string(&[point]);
            let index = usize::try_from(point).expect("a small index");

            for (interpreter, lines) in &reprs {
                let theirs = &lines[index];
                if ours == *theirs {
                    continue;
                }

                let kept = char::from_u32(point).map(|character| format!("'{character}'"));
                assert!(
                    kept.as_ref() == Some(&ours)
                        && reprs.iter().any(|(_, other)| other[index] == ours),
                    "U+{point:04X}: {interpreter} writes {theirs}, microscope {ours}"
                );
            }
        }
    }

    #[test]
    fn bytes_are_their_repr() {
        for (value, expected) in [
            (&b""[..], "b''"),
            (b"plain", "b'plain'"),
            (b"\x00\x01binary\xff'", "b\"\\x00\\x01binary\\xff'\""),
            (b"' and \"", "b'\\' and \"'"),
            (b"\t\n\r\\\x7f", "b'\\t\\n\\r\\\\\\x7f'"),
        ] {
            assert_eq!(bytes(value), expected, "{value:?}");
        }
    }

    #[test]
    fn a_float_is_its_shortest_repr() {
        for (value, expected) in [
            (0.0, "0.0"),
            (-0.0, "-0.0"),
            (0.5, "0.5"),
            (0.1, "0.1"),
            (1.0, "1.0"),
            (-12.25, "-12.25"),
            (1e16, "1e+16"),
            (1e15, "1000000000000000.0"),
            (1_234_567_890_123_456.0, "1234567890123456.0"),
            (1.5e300, "1.5e+300"),
            (5e-324, "5e-324"),
            (0.0001, "0.0001"),
            (0.00001, "1e-05"),
            (0.000_123, "0.000123"),
            (123_456.789, "123456.789"),
            (f64::INFINITY, "inf"),
            (f64::NEG_INFINITY, "-inf"),
            (f64::NAN, "nan"),
            (f64::MAX, "1.7976931348623157e+308"),
        ] {
            assert_eq!(float(value), expected, "{value:e}");
        }
    }

    /// A 32-bit float has the shortest digits of its own width, in the
    /// notation Python's `repr` writes a float in.
    #[test]
    fn a_float32_is_its_shortest_digits() {
        for (value, expected) in [
            (0.1_f32, "0.1"),
            (1.0, "1.0"),
            (-0.0, "-0.0"),
            (16_777_216.0, "16777216.0"),
            (123_456.79, "123456.79"),
            (1e16, "1e+16"),
            (1e-5, "1e-05"),
            (f32::MAX, "3.4028235e+38"),
            (f32::MIN_POSITIVE, "1.1754944e-38"),
            (f32::NAN, "nan"),
            (f32::NEG_INFINITY, "-inf"),
        ] {
            assert_eq!(float32(value), expected, "{value:e}");
        }
    }

    #[test]
    fn an_int_is_its_digits_in_decimal_at_any_size() {
        let mask = (1 << 30) - 1;
        assert_eq!(int(false, &[], 30), "0");
        assert_eq!(int(false, &[7], 30), "7");
        assert_eq!(int(true, &[12_345], 30), "-12345");
        assert_eq!(int(false, &[mask], 30), "1073741823");
        assert_eq!(int(false, &[0, 1], 30), "1073741824");

        // 2**100 + 12345, and -(10**38) - 1, as CPython stores them.
        let big: u128 = (1 << 100) + 12_345;
        let digits = |value: u128| -> Vec<u32> {
            (0..5)
                .map(|at| u32::try_from((value >> (30 * at)) & u128::from(mask)).expect("a digit"))
                .collect()
        };

        assert_eq!(int(false, &digits(big), 30), big.to_string());

        let huge: u128 = 10_u128.pow(38) + 1;
        assert_eq!(int(true, &digits(huge), 30), format!("-{huge}"));

        // A 15-bit build's digits.
        assert_eq!(int(false, &[0, 0, 1], 15), (1_u64 << 30).to_string());
    }
}
