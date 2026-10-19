//! How a view's `format` writes a value: hexadecimal, a character, its
//! bytes, UTF-8 or UTF-16 text, an enumeration's flags or enumerator, a
//! duration, or a time. A value no format suits, or one the program cannot
//! provide, keeps its own rendering.

use std::fmt::Write as _;
use std::sync::Arc;

use crate::eval::target::{Machine, Stop};
use crate::model::ValueStorage;
use crate::{
    InspectedValue, IntegerValue, ScalarValue, TextCompletion, TextSummary, TypeKind,
    ValueChildren, VariableState, VariableValue, VariableValueSource,
};

use super::bind::BoundFormat;
use super::syntax::TimeUnit;

/// The most bytes `bytes` shows, and `utf16` reads.
const MAX_BYTES: u64 = 64;
const MAX_UTF16_BYTES: u64 = 512;

/// A second, in nanoseconds.
const SECOND: u128 = 1_000_000_000;

/// `value` written as `format` says, or `None` when the format does not
/// suit it.
pub fn write<M: Machine>(
    format: BoundFormat,
    value: &InspectedValue,
    machine: &mut M,
) -> Result<Option<String>, Stop> {
    let VariableState::Available {
        value: stored,
        source,
        children,
        ..
    } = &value.state
    else {
        return Ok(None);
    };

    let byte_size = value.type_info.as_ref().and_then(|info| info.byte_size);

    Ok(match format {
        BoundFormat::Hex => integer(stored).map(|integer| hex(integer, byte_size)),
        BoundFormat::Char => integer(stored).and_then(character),
        BoundFormat::Enum(enumeration) => integer(stored).map(|integer| {
            enumerators(machine, enumeration)
                .iter()
                .find(|(_, value)| *value == integer)
                .map_or_else(|| integer.to_string(), |(name, _)| name.to_string())
        }),
        BoundFormat::Flags(enumeration) => {
            integer(stored).map(|integer| flags(integer, &enumerators(machine, enumeration)))
        }
        BoundFormat::Duration(unit) => integer(stored).map(|integer| duration(integer, unit)),
        BoundFormat::Time(unit) => integer(stored).and_then(|integer| time(integer, unit)),
        BoundFormat::Utf8 => {
            // An array is where it is stored, and a slice's elements are
            // where its children are.
            let (address, length) = match (stored, source, children) {
                (VariableValue::Array { .. }, VariableValueSource::Memory(address), _) => {
                    (*address, byte_size.unwrap_or(0))
                }
                (VariableValue::Slice { length, .. }, _, ValueChildren::Available(elements)) => {
                    match elements.storage {
                        ValueStorage::Memory(address) => (address, *length),
                        _ => return Ok(None),
                    }
                }
                _ => return Ok(None),
            };

            utf8_at(address.get(), length, machine)?
        }
        BoundFormat::Bytes => {
            let (VariableValueSource::Memory(address), Some(size)) = (source, byte_size) else {
                return Ok(None);
            };

            let shown = size.min(MAX_BYTES);
            let bytes = machine.read(address.get(), usize::try_from(shown).unwrap_or(0))?;
            let mut text = bytes
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<Vec<_>>()
                .join(" ");
            if shown < size {
                text.push_str(" …");
            }

            Some(text)
        }
        BoundFormat::Utf16 => {
            let (VariableValueSource::Memory(address), Some(size), VariableValue::Array { .. }) =
                (source, byte_size, stored)
            else {
                return Ok(None);
            };

            let read = size.min(MAX_UTF16_BYTES) & !1;
            let bytes = machine.read(address.get(), usize::try_from(read).unwrap_or(0))?;
            let order = machine.byte_order();
            let units = bytes
                .as_chunks::<2>()
                .0
                .iter()
                .map(|unit| match order {
                    crate::ByteOrder::Little => u16::from_le_bytes(*unit),
                    crate::ByteOrder::Big => u16::from_be_bytes(*unit),
                })
                .take_while(|unit| *unit != 0)
                .collect::<Vec<_>>();
            let ended = units.len() * 2 < bytes.len();
            let text = char::decode_utf16(units)
                .map(|unit| unit.unwrap_or(char::REPLACEMENT_CHARACTER))
                .collect::<String>();

            Some(super::summary::quoted(&TextSummary {
                bytes: Arc::from(text.into_bytes()),
                completion: if ended || read == size {
                    TextCompletion::Complete
                } else {
                    TextCompletion::Truncated { length: None }
                },
            }))
        }
    })
}

/// The `length` bytes at `address`, quoted, when they are text.
fn utf8_at<M: Machine>(address: u64, length: u64, machine: &mut M) -> Result<Option<String>, Stop> {
    let shown = length.min(TextSummary::MAX_BYTES as u64);
    let bytes = match machine.read(address, usize::try_from(shown).unwrap_or(0)) {
        Ok(bytes) => bytes,
        Err(Stop::Missing(_)) => return Ok(None),
        Err(stop) => return Err(stop),
    };

    let completion = if shown == length {
        TextCompletion::Complete
    } else {
        TextCompletion::Truncated {
            length: Some(length),
        }
    };

    Ok(utf8_text(&TextSummary {
        bytes: Arc::from(bytes),
        completion,
    })
    .map(|text| super::summary::quoted(&text)))
}

/// Bytes as text, when they are text: valid UTF-8 with no control
/// characters but tab, newline, and carriage return. Bytes cut short may
/// end inside a character, which is left out. No bytes are not text, nor
/// are bytes that could not all be read.
#[must_use]
pub fn utf8_text(text: &TextSummary) -> Option<TextSummary> {
    let complete = match text.completion {
        TextCompletion::Complete => true,
        TextCompletion::Truncated { .. } => false,
        _ => return None,
    };

    let valid = match std::str::from_utf8(&text.bytes) {
        Ok(valid) => valid,
        Err(error) if !complete && error.error_len().is_none() => {
            std::str::from_utf8(&text.bytes[..error.valid_up_to()]).ok()?
        }
        Err(_) => return None,
    };

    let printable = valid
        .chars()
        .all(|character| !character.is_control() || matches!(character, '\t' | '\n' | '\r'));
    (!valid.is_empty() && printable).then(|| TextSummary {
        bytes: Arc::from(valid.as_bytes()),
        completion: text.completion,
    })
}

/// An integer value, of an integer or enumeration.
fn integer(value: &VariableValue) -> Option<i128> {
    match value {
        VariableValue::Scalar(ScalarValue::Signed(value)) => Some(*value),
        VariableValue::Scalar(ScalarValue::Unsigned(value)) => i128::try_from(*value).ok(),
        VariableValue::Enumeration { value, .. } => signed(*value),
        _ => None,
    }
}

fn signed(value: IntegerValue) -> Option<i128> {
    match value {
        IntegerValue::Signed(value) => Some(value),
        IntegerValue::Unsigned(value) => i128::try_from(value).ok(),
    }
}

/// Hexadecimal, a negative value in its type's two's complement.
fn hex(value: i128, byte_size: Option<u64>) -> String {
    let bits = byte_size
        .filter(|size| (1..=16).contains(size))
        .map_or(128, |size| size * 8);
    let mask = if bits == 128 {
        u128::MAX
    } else {
        (1_u128 << bits) - 1
    };

    format!("{:#x}", value.cast_unsigned() & mask)
}

/// The character a code point is, quoted.
fn character(value: i128) -> Option<String> {
    let character = u32::try_from(value).ok().and_then(char::from_u32)?;
    Some(match character {
        '\'' => "'\\''".to_owned(),
        '\\' => "'\\\\'".to_owned(),
        '\n' => "'\\n'".to_owned(),
        '\t' => "'\\t'".to_owned(),
        '\r' => "'\\r'".to_owned(),
        '\0' => "'\\0'".to_owned(),
        character if character.is_control() => format!("'{}'", character.escape_unicode()),
        character => format!("'{character}'"),
    })
}

/// An enumeration's enumerators, by name and value.
fn enumerators<M: Machine>(
    machine: &M,
    enumeration: crate::TypeReference,
) -> Vec<(Arc<str>, i128)> {
    let Some(info) = machine.type_info(enumeration) else {
        return Vec::new();
    };

    let TypeKind::Enumeration { enumerators, .. } = &info.kind else {
        return Vec::new();
    };

    enumerators
        .iter()
        .filter_map(|enumerator| Some((Arc::clone(&enumerator.name), signed(enumerator.value)?)))
        .collect()
}

/// The enumerators whose bits a value sets, `A | B`, and the bits none
/// names in hexadecimal; the enumerator of 0, or `0`, for none.
fn flags(value: i128, enumerators: &[(Arc<str>, i128)]) -> String {
    if value == 0 {
        return enumerators
            .iter()
            .find(|(_, flag)| *flag == 0)
            .map_or_else(|| "0".to_owned(), |(name, _)| name.to_string());
    }

    let mut names = Vec::new();
    let mut covered = 0_i128;
    for (name, flag) in enumerators {
        if *flag != 0 && value & flag == *flag && covered & flag != *flag {
            names.push(name.to_string());
            covered |= flag;
        }
    }

    let rest = value & !covered;
    if rest != 0 {
        names.push(format!("{:#x}", rest.cast_unsigned()));
    }

    names.join(" | ")
}

/// A count of `unit`s as Go writes a duration: `1h2m3.5s`, `1.5s`,
/// `250ms`, `3µs`, `7ns`, or `0s`.
fn duration(value: i128, unit: TimeUnit) -> String {
    let Some(total) = value.checked_mul(i128::try_from(unit.nanoseconds()).unwrap_or(1)) else {
        return format!("{value}{}", unit_suffix(unit));
    };

    let negative = total < 0;
    let total = total.unsigned_abs();
    let mut output = String::new();

    if negative {
        output.push('-');
    }

    if total == 0 {
        output.push_str("0s");
    } else if total < 1_000 {
        let _ = write!(output, "{total}ns");
    } else if total < 1_000_000 {
        output.push_str(&decimal(total, 1_000));
        output.push_str("µs");
    } else if total < SECOND {
        output.push_str(&decimal(total, 1_000_000));
        output.push_str("ms");
    } else {
        let hours = total / (3600 * SECOND);
        let minutes = total / (60 * SECOND) % 60;
        let seconds = total % (60 * SECOND);

        if hours > 0 {
            let _ = write!(output, "{hours}h");
        }

        if hours > 0 || minutes > 0 {
            let _ = write!(output, "{minutes}m");
        }

        output.push_str(&decimal(seconds, SECOND));
        output.push('s');
    }

    output
}

/// A count of `unit`s since the Unix epoch as the UTC date and time it is,
/// as Go writes one: `2009-11-10 23:00:00.5 +0000 UTC`, in the proleptic
/// Gregorian calendar, a year before 1 as 0 or less. `None` for a count
/// beyond any calendar.
fn time(value: i128, unit: TimeUnit) -> Option<String> {
    let total = value.checked_mul(i128::try_from(unit.nanoseconds()).ok()?)?;
    let second = i128::try_from(SECOND).ok()?;
    let (seconds, nanoseconds) = (total.div_euclid(second), total.rem_euclid(second));
    let (days, of_day) = (seconds.div_euclid(86_400), seconds.rem_euclid(86_400));

    // Howard Hinnant's `civil_from_days`, in 400-year eras from 0000-03-01.
    let shifted = days + 719_468;
    let era = shifted.div_euclid(146_097);
    let of_era = shifted.rem_euclid(146_097);
    let year_of_era = (of_era - of_era / 1_460 + of_era / 36_524 - of_era / 146_096) / 365;
    let day_of_year = of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let shifted_month = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * shifted_month + 2) / 5 + 1;
    let month = if shifted_month < 10 {
        shifted_month + 3
    } else {
        shifted_month - 9
    };

    let year = year_of_era + era * 400 + i128::from(month <= 2);
    let mut output = String::new();

    if year < 0 {
        output.push('-');
    }

    let _ = write!(
        output,
        "{:04}-{month:02}-{day:02} {:02}:{:02}:{:02}",
        year.unsigned_abs(),
        of_day / 3600,
        of_day / 60 % 60,
        of_day % 60,
    );

    if nanoseconds != 0 {
        let fraction = format!("{nanoseconds:09}");
        output.push('.');
        output.push_str(fraction.trim_end_matches('0'));
    }

    output.push_str(" +0000 UTC");
    Some(output)
}

const fn unit_suffix(unit: TimeUnit) -> &'static str {
    match unit {
        TimeUnit::Nanoseconds => "ns",
        TimeUnit::Microseconds => "µs",
        TimeUnit::Milliseconds => "ms",
        TimeUnit::Seconds => "s",
    }
}

/// `value / scale` in decimal, its fraction without trailing zeros.
fn decimal(value: u128, scale: u128) -> String {
    let whole = value / scale;
    let fraction = value % scale;
    if fraction == 0 {
        return whole.to_string();
    }

    let digits = scale.ilog10() as usize;
    let fraction = format!("{fraction:0digits$}");
    format!("{whole}.{}", fraction.trim_end_matches('0'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations_read_as_go_writes_them() {
        let cases = [
            (0, TimeUnit::Seconds, "0s"),
            (7, TimeUnit::Nanoseconds, "7ns"),
            (3, TimeUnit::Microseconds, "3µs"),
            (1_500, TimeUnit::Nanoseconds, "1.5µs"),
            (250, TimeUnit::Milliseconds, "250ms"),
            (1_500, TimeUnit::Milliseconds, "1.5s"),
            (90, TimeUnit::Seconds, "1m30s"),
            (3_723_500, TimeUnit::Milliseconds, "1h2m3.5s"),
            (-2, TimeUnit::Seconds, "-2s"),
        ];

        for (value, unit, expected) in cases {
            assert_eq!(duration(value, unit), expected, "{value} {unit:?}");
        }
    }

    #[test]
    fn times_read_as_go_writes_them_in_utc() {
        let cases = [
            (0, TimeUnit::Seconds, "1970-01-01 00:00:00 +0000 UTC"),
            (
                1_257_894_000_000_000_500,
                TimeUnit::Nanoseconds,
                "2009-11-10 23:00:00.0000005 +0000 UTC",
            ),
            (
                -1,
                TimeUnit::Milliseconds,
                "1969-12-31 23:59:59.999 +0000 UTC",
            ),
            (
                951_782_400,
                TimeUnit::Seconds,
                "2000-02-29 00:00:00 +0000 UTC",
            ),
            (
                -62_135_596_800,
                TimeUnit::Seconds,
                "0001-01-01 00:00:00 +0000 UTC",
            ),
            (
                -62_167_219_201,
                TimeUnit::Seconds,
                "-0001-12-31 23:59:59 +0000 UTC",
            ),
            (
                253_402_300_800,
                TimeUnit::Seconds,
                "10000-01-01 00:00:00 +0000 UTC",
            ),
        ];

        for (value, unit, expected) in cases {
            assert_eq!(
                time(value, unit).as_deref(),
                Some(expected),
                "{value} {unit:?}"
            );
        }

        assert_eq!(time(i128::MAX, TimeUnit::Seconds), None);
    }

    #[test]
    fn only_printable_utf8_is_text() {
        let text = |bytes: &[u8], completion| {
            utf8_text(&TextSummary {
                bytes: Arc::from(bytes),
                completion,
            })
            .map(|text| text.bytes.to_vec())
        };

        let cut = TextCompletion::Truncated { length: None };
        assert_eq!(
            text("tab\tµ\n".as_bytes(), TextCompletion::Complete),
            Some("tab\tµ\n".as_bytes().to_vec())
        );
        assert_eq!(text(b"", TextCompletion::Complete), None);
        assert_eq!(text(&[0xff, 0, 1], TextCompletion::Complete), None);
        assert_eq!(text(b"nul\0", TextCompletion::Complete), None);

        // A character cut short by the read is left out, unless the bytes
        // were whole.
        assert_eq!(text(&[b'a', 0xc2], cut), Some(b"a".to_vec()));
        assert_eq!(text(&[b'a', 0xc2], TextCompletion::Complete), None);
        assert_eq!(
            text(
                b"ab",
                TextCompletion::Unreadable {
                    address: crate::VirtualAddress::new(2)
                }
            ),
            None
        );
    }

    #[test]
    fn flags_name_each_bit_once_and_show_the_rest() {
        let enumerators = [
            (Arc::from("NONE"), 0),
            (Arc::from("READ"), 1),
            (Arc::from("WRITE"), 2),
            (Arc::from("READ_WRITE"), 3),
        ];
        assert_eq!(flags(0, &enumerators), "NONE");
        assert_eq!(flags(3, &enumerators), "READ | WRITE");
        assert_eq!(flags(0x41, &enumerators), "READ | 0x40");
        assert_eq!(hex(-1, Some(2)), "0xffff");
    }
}
