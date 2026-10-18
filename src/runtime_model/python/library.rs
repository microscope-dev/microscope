//! The standard library's types that only their own C defines, which no
//! Python code can read: `collections.deque`, `defaultdict`, and
//! `OrderedDict`, `datetime`'s `date`, `time`, `datetime`, `timedelta`,
//! and `timezone`, and `decimal.Decimal`. The layouts copy their structures
//! (`scripts/cpython-layouts.c`).
//!
//! A type is one of them only where C defines it, as no class can be, and
//! its module, its name, and the size of its objects are all the copied
//! structure's: a type of the same name that a program or another
//! extension defines is read as any other object is. A class deriving from
//! one of them holds its value, as one deriving from `int` holds an int's.
//!
//! Each is shown as its type's own `repr` writes it, with the names of the
//! object's type that its C uses, and its items as its children: an
//! `OrderedDict`'s in the order its list of nodes keeps, which `move_to_end`
//! changes and its dict's order does not show. A `Decimal`'s exponent is
//! written `E`, as the default context's `capitals` writes it.
//!
//! numpy's `ndarray` is identified alike, by its module and name and an
//! object size that holds the fields of numpy's public structure, and read
//! by [`super::numpy`].

use std::fmt::Write as _;
use std::sync::Arc;

use super::instances::Class;
use super::numpy::Array;
use super::objects::{Head, Problem, malformed};
use super::values::{Dict, Entry, Object, Summary, Values, note};
use crate::VirtualAddress;
use crate::image::python::{Constant, Field};
use crate::runtime_model::InterpretedKind;

/// The most digits of a `Decimal`'s coefficient read: one with more is
/// shown by its size.
const MAX_DECIMAL_DIGITS: u64 = 1 << 10;

/// libmpdec's number keeps its coefficient in words of this many decimal
/// digits each, below [`DECIMAL_RADIX`].
const DECIMAL_WORD_DIGITS: u64 = 19;
const DECIMAL_RADIX: u64 = 10_000_000_000_000_000_000;

/// The bits of a libmpdec number's `flags` that give its sign and its
/// kind, as `mpdecimal.h` defines them: negative, infinite, a quiet NaN,
/// and a signaling NaN.
const DECIMAL_NEGATIVE: u8 = 1;
const DECIMAL_INFINITE: u8 = 2;
const DECIMAL_NAN: u8 = 4;
const DECIMAL_SIGNALING: u8 = 8;

/// One of the standard library's types that only C defines.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum LibraryType {
    Deque,
    DefaultDict,
    OrderedDict,
    Date,
    Time,
    DateTime,
    Delta,
    TimeZone,
    Decimal,
    NdArray,
}

/// The size a type's objects must have: that of the structure the layout
/// copies, or at least this many bytes past the object's header.
#[derive(Clone, Copy)]
pub(super) enum Size {
    Exact(Field),
    AtLeast(u64),
}

/// Each type, by its module and name, and the size of its objects.
const LIBRARY: [(&str, &str, LibraryType, Size); 10] = [
    (
        "collections",
        "deque",
        LibraryType::Deque,
        Size::Exact(Field::DequeObjectSize),
    ),
    (
        "collections",
        "defaultdict",
        LibraryType::DefaultDict,
        Size::Exact(Field::DefaultDictObjectSize),
    ),
    (
        "collections",
        "OrderedDict",
        LibraryType::OrderedDict,
        Size::Exact(Field::OrderedDictObjectSize),
    ),
    (
        "datetime",
        "date",
        LibraryType::Date,
        Size::Exact(Field::DateObjectSize),
    ),
    (
        "datetime",
        "time",
        LibraryType::Time,
        Size::Exact(Field::TimeObjectSize),
    ),
    (
        "datetime",
        "datetime",
        LibraryType::DateTime,
        Size::Exact(Field::DateTimeObjectSize),
    ),
    (
        "datetime",
        "timedelta",
        LibraryType::Delta,
        Size::Exact(Field::DeltaObjectSize),
    ),
    (
        "datetime",
        "timezone",
        LibraryType::TimeZone,
        Size::Exact(Field::TimeZoneObjectSize),
    ),
    (
        "decimal",
        "Decimal",
        LibraryType::Decimal,
        Size::Exact(Field::DecimalObjectSize),
    ),
    (
        "numpy",
        "ndarray",
        LibraryType::NdArray,
        Size::AtLeast(super::numpy::FIELDS),
    ),
];

impl LibraryType {
    /// The type named `name` in `module`, and the size its objects must
    /// have, if one is.
    pub(super) fn named(module: Option<&str>, name: &str) -> Option<(Self, Size)> {
        LIBRARY
            .iter()
            .find(|(owner, named, ..)| Some(*owner) == module && *named == name)
            .map(|&(_, _, ty, size)| (ty, size))
    }
}

/// An object of one of the types, as far as presenting it needs it read.
pub(super) enum Library {
    /// A deque of `length` items, the first at `index` of the block at
    /// `block`, and the most it holds, if it is bounded.
    Deque {
        block: VirtualAddress,
        index: u64,
        length: u64,
        maxlen: Option<u64>,
    },

    /// A dict, and what makes the value of a key it holds none for.
    DefaultDict {
        dict: Dict,
        factory: Option<VirtualAddress>,
    },

    /// A dict, and the first node of the list that orders its keys.
    OrderedDict {
        dict: Dict,
        first: VirtualAddress,
    },
    Date(Date),
    Time {
        time: Time,
        tzinfo: Option<VirtualAddress>,
    },
    DateTime {
        date: Date,
        time: Time,
        tzinfo: Option<VirtualAddress>,
    },
    Delta {
        days: i32,
        seconds: i32,
        microseconds: i32,
    },

    /// A fixed offset from UTC, a `timedelta`, and its name, if it was
    /// given one.
    TimeZone {
        offset: VirtualAddress,
        name: Option<VirtualAddress>,
    },
    Decimal(Decimal),
    Array(Array),
}

#[derive(Clone, Copy)]
pub(super) struct Date {
    year: u16,
    month: u8,
    day: u8,
}

/// A time of day, and its fold: 1 for the second of two times a clock
/// shows twice.
#[derive(Clone, Copy)]
pub(super) struct Time {
    hour: u8,
    minute: u8,
    second: u8,
    microsecond: u32,
    fold: u8,
}

/// A libmpdec number: its `flags`, its exponent, and its coefficient's
/// digits, in words of [`DECIMAL_WORD_DIGITS`], least significant first;
/// or the count of digits of one of more than are read.
pub(super) enum Decimal {
    Read {
        flags: u8,
        exponent: i64,
        words: Vec<u64>,
    },
    Large {
        digits: u64,
    },
}

impl Library {
    /// What kind of value it is.
    pub(super) fn kind(&self) -> InterpretedKind {
        match self {
            Self::Deque { length, .. } => InterpretedKind::Sequence { length: *length },
            Self::DefaultDict { dict, .. } | Self::OrderedDict { dict, .. } => {
                InterpretedKind::Map {
                    length: dict.length,
                }
            }
            Self::Date(_)
            | Self::Time { .. }
            | Self::DateTime { .. }
            | Self::Delta { .. }
            | Self::TimeZone { .. }
            | Self::Decimal(_) => InterpretedKind::Other,
            Self::Array(array) => array.kind(),
        }
    }

    /// Whether it holds other objects, as a container does, which a
    /// summary nests.
    pub(super) const fn contains(&self) -> bool {
        matches!(
            self,
            Self::Deque { .. } | Self::DefaultDict { .. } | Self::OrderedDict { .. }
        )
    }

    /// The `dict` it is, if it is one.
    pub(super) const fn dict(&self) -> Option<Dict> {
        match self {
            Self::DefaultDict { dict, .. } | Self::OrderedDict { dict, .. } => Some(*dict),
            _ => None,
        }
    }
}

impl Values<'_, '_> {
    /// The library type the type at `ty`, of `flags`, named `name` in
    /// `module`, is: where C defines it and its objects are the size of
    /// the structure the layout copies.
    pub(super) fn library_type(
        &self,
        ty: VirtualAddress,
        defined_in_c: bool,
        (module, name): (Option<&str>, &str),
    ) -> Result<Option<LibraryType>, Problem> {
        let Some((library, size)) = LibraryType::named(module, name).filter(|_| defined_in_c)
        else {
            return Ok(None);
        };

        let reader = &self.reader;
        let basicsize = reader.field_word(ty, Field::TypeBasicsize)?;
        let sized = match size {
            Size::Exact(size) => reader.layout.field(size) == Some(basicsize),
            Size::AtLeast(fields) => {
                basicsize >= reader.offset(Field::ObjectSize)?.saturating_add(fields)
            }
        };
        Ok(sized.then_some(library))
    }

    /// The object the head is of, of the library type `class`, which an
    /// object of the type at `ty` holds: `ty` is `class` or derives from
    /// it.
    pub(super) fn library(
        &self,
        head: &Head<'_, '_>,
        class: &Arc<Class>,
        ty: VirtualAddress,
    ) -> Result<Option<Object>, Problem> {
        let Some(library) = class.library else {
            return Ok(None);
        };

        let reader = &self.reader;
        let address = head.address();
        let pointer = |field| -> Result<Option<VirtualAddress>, Problem> {
            Ok(Some(head.field_word(field)?)
                .filter(|word| *word != 0)
                .map(VirtualAddress::new))
        };
        let int = |field| -> Result<i32, Problem> {
            let bytes = head.field_bytes(field, 4)?;
            Ok(i32::from_le_bytes(
                bytes.try_into().map_err(|_| malformed(address, "object"))?,
            ))
        };
        // A time or datetime has a tzinfo only where its `hastzinfo`, in
        // the head every `datetime` type shares, says so: one made without
        // is allocated short of the field.
        let tzinfo = |field| -> Result<Option<VirtualAddress>, Problem> {
            if head.field_byte(Field::DateHasTzinfo)? == 0 {
                return Ok(None);
            }

            pointer(field)?
                .map(Some)
                .ok_or_else(|| malformed(address, "time"))
        };

        let value = match library {
            LibraryType::Deque => {
                let length = signed(head.field_word(Field::VarObjectSize)?, address)?;
                let index = signed(head.field_word(Field::DequeLeftIndex)?, address)?;
                let maxlen = head.field_word(Field::DequeMaxlen)?.cast_signed();
                let block =
                    pointer(Field::DequeLeftBlock)?.ok_or_else(|| malformed(address, "deque"))?;
                if index >= u64::from(reader.layout.constant(Constant::DequeBlockLength))
                    || maxlen < -1
                    || u64::try_from(maxlen).is_ok_and(|maxlen| length > maxlen)
                {
                    return Err(malformed(address, "deque"));
                }

                Library::Deque {
                    block,
                    index,
                    length,
                    maxlen: u64::try_from(maxlen).ok(),
                }
            }
            LibraryType::DefaultDict => Library::DefaultDict {
                dict: self.dict_of(head)?,
                factory: pointer(Field::DefaultDictFactory)?,
            },
            LibraryType::OrderedDict => {
                let dict = self.dict_of(head)?;
                let first = pointer(Field::OrderedDictFirst)?.unwrap_or(VirtualAddress::new(0));
                Library::OrderedDict { dict, first }
            }
            LibraryType::Date => {
                Library::Date(date(&head.field_bytes(Field::DateData, 4)?, address)?)
            }
            LibraryType::Time => Library::Time {
                time: time(
                    &head.field_bytes(Field::TimeData, 6)?,
                    head.field_byte(Field::TimeFold)?,
                    address,
                )?,
                tzinfo: tzinfo(Field::TimeTzinfo)?,
            },
            LibraryType::DateTime => {
                let data = head.field_bytes(Field::DateTimeData, 10)?;
                let (day, clock) = data.split_at(4);
                Library::DateTime {
                    date: date(day, address)?,
                    time: time(clock, head.field_byte(Field::DateTimeFold)?, address)?,
                    tzinfo: tzinfo(Field::DateTimeTzinfo)?,
                }
            }
            LibraryType::Delta => Library::Delta {
                days: int(Field::DeltaDays)?,
                seconds: int(Field::DeltaSeconds)?,
                microseconds: int(Field::DeltaMicroseconds)?,
            },
            LibraryType::TimeZone => Library::TimeZone {
                offset: pointer(Field::TimeZoneOffset)?
                    .ok_or_else(|| malformed(address, "timezone"))?,
                name: pointer(Field::TimeZoneName)?,
            },
            LibraryType::Decimal => Library::Decimal(self.decimal(head)?),
            LibraryType::NdArray => Library::Array(self.array(head)?),
        };

        let name = reader.c_string(VirtualAddress::new(reader.field_word(ty, Field::TypeName)?))?;
        Ok(Some(Object::Library {
            class: Arc::clone(class),
            name,
            value,
        }))
    }

    /// The dict the head's object is, as a `defaultdict` or an
    /// `OrderedDict` is first.
    fn dict_of(&self, head: &Head<'_, '_>) -> Result<Dict, Problem> {
        match self.dict(head)? {
            Object::Dict(dict) => Ok(dict),
            _ => Err(malformed(head.address(), "dict")),
        }
    }

    /// The libmpdec number of the `Decimal` the head is of.
    fn decimal(&self, head: &Head<'_, '_>) -> Result<Decimal, Problem> {
        let address = head.address();
        let bad = || malformed(address, "Decimal");
        let flags = head.field_byte(Field::DecimalFlags)?;
        let exponent = head.field_word(Field::DecimalExponent)?.cast_signed();
        let digits = signed(head.field_word(Field::DecimalDigits)?, address)?;
        let length = signed(head.field_word(Field::DecimalLength)?, address)?;
        let special = flags & (DECIMAL_INFINITE | DECIMAL_NAN | DECIMAL_SIGNALING) != 0;
        if digits > MAX_DECIMAL_DIGITS {
            return Ok(Decimal::Large { digits });
        }

        // A number's words hold exactly its digits; an infinity has none,
        // and a NaN only the digits of its payload, if it has one.
        if length != digits.div_ceil(DECIMAL_WORD_DIGITS)
            || (!special && length == 0)
            || (flags & DECIMAL_INFINITE != 0 && length != 0)
        {
            return Err(bad());
        }

        let data = VirtualAddress::new(head.field_word(Field::DecimalData)?);
        let words: Vec<u64> = if length == 0 {
            Vec::new()
        } else {
            self.reader
                .bytes(data, length * 8)?
                .as_chunks::<8>()
                .0
                .iter()
                .map(|word| u64::from_le_bytes(*word))
                .collect()
        };

        let counted = words.last().map_or(0, |last| {
            word_digits(*last) + (length - 1) * DECIMAL_WORD_DIGITS
        });
        if counted != digits || words.iter().any(|word| *word >= DECIMAL_RADIX) {
            return Err(bad());
        }

        Ok(Decimal::Read {
            flags,
            exponent,
            words,
        })
    }

    /// The items `range` of the deque whose first of `length` items is at
    /// `index` of the block at `block`.
    pub(super) fn deque_items(
        &self,
        (block, index, length): (VirtualAddress, u64, u64),
        range: std::ops::Range<u64>,
    ) -> Result<Vec<VirtualAddress>, Problem> {
        let reader = &self.reader;
        let per_block = u64::from(reader.layout.constant(Constant::DequeBlockLength));
        let (data, link) = (
            reader.offset(Field::DequeBlockData)?,
            reader.offset(Field::DequeBlockRightLink)?,
        );
        let end = range.end.min(length);
        let start = range.start.min(end);
        let next = |block: VirtualAddress| -> Result<VirtualAddress, Problem> {
            match reader.word(block.wrapping_add(link))? {
                0 => Err(malformed(block, "deque block")),
                next => Ok(VirtualAddress::new(next)),
            }
        };

        let mut block = block;
        let mut at = index + start;
        for _ in 0..at / per_block {
            block = next(block)?;
        }

        at %= per_block;
        let mut items = Vec::new();
        let mut taken = start;
        while taken < end {
            if at == per_block {
                block = next(block)?;
                at = 0;
            }

            let run = (per_block - at).min(end - taken);
            let bytes = reader.bytes(block.wrapping_add(data + at * 8), run * 8)?;
            for item in bytes.as_chunks::<8>().0 {
                match u64::from_le_bytes(*item) {
                    0 => return Err(malformed(block, "deque block")),
                    item => items.push(VirtualAddress::new(item)),
                }
            }

            at += run;
            taken += run;
        }

        Ok(items)
    }

    /// Calls `each` with each of the `OrderedDict`'s items in `range`, in
    /// the order its nodes keep, from the node at `first`, until it fails.
    /// Each node holds a key of the dict, the very object, which a probe
    /// for its hash finds.
    pub(super) fn linked_entries(
        &self,
        (dict, first): (Dict, VirtualAddress),
        range: std::ops::Range<u64>,
        mut each: impl FnMut(Entry) -> Result<(), Problem>,
    ) -> Result<(), Problem> {
        let reader = &self.reader;
        let bad = || malformed(dict.object, "OrderedDict");
        let mut node = first;
        let mut item = 0;
        while item < range.end.min(dict.length) {
            if node == VirtualAddress::new(0) {
                return Err(bad());
            }

            if item >= range.start {
                let key = VirtualAddress::new(reader.field_word(node, Field::OrderedDictNodeKey)?);
                let hash = reader
                    .field_word(node, Field::OrderedDictNodeHash)?
                    .cast_signed();
                let value = self.identical(dict, key, hash)?.ok_or_else(bad)?;
                each(Entry {
                    key,
                    value,
                    hash: Some(hash),
                })?;
            }

            node = VirtualAddress::new(reader.field_word(node, Field::OrderedDictNodeNext)?);
            item += 1;
        }

        Ok(())
    }

    /// The text of an object of a library type, `value`, whose type's C
    /// names it `name`, where it is `depth` containers deep in a summary.
    pub(super) fn library_text(
        &self,
        name: &str,
        value: &Library,
        depth: usize,
        summary: &mut Summary,
    ) -> String {
        // A collection's own C writes its type's name without its module.
        let short = name.rsplit_once('.').map_or(name, |(_, name)| name);
        match value {
            Library::Deque {
                block,
                index,
                length,
                maxlen,
            } => {
                let items = self.deque_items(
                    (*block, *index, *length),
                    0..(*length).min(super::values::SUMMARY_ITEMS),
                );
                let list = self.listed(items, *length, ("[", "]"), depth, summary);
                maxlen.map_or_else(
                    || format!("{short}({list})"),
                    |maxlen| format!("{short}({list}, maxlen={maxlen})"),
                )
            }
            Library::DefaultDict { dict, factory } => {
                let factory = self.factory_text(*factory, depth, summary);
                let dict = self.dict_text(*dict, depth, summary);
                format!("{short}({factory}, {dict})")
            }
            Library::OrderedDict { dict, first } => {
                if dict.length == 0 {
                    return format!("{short}()");
                }

                let mut pairs = Vec::new();
                let read = self.linked_entries(
                    (*dict, *first),
                    0..super::values::SUMMARY_ITEMS,
                    |entry| {
                        pairs.push((entry.key, entry.value));
                        Ok(())
                    },
                );
                let pairs = self.pairs_text((&pairs, read), dict.length, depth, summary);
                format!("{short}({pairs})")
            }
            Library::Date(day) => format!("{name}({}, {}, {})", day.year, day.month, day.day),
            Library::Time { time, tzinfo } => {
                let mut text = format!("{name}({})", clock(*time));
                self.tzinfo_text(&mut text, *tzinfo, depth, summary);
                if time.fold != 0 {
                    keyword(&mut text, &format!("fold={}", time.fold));
                }

                text
            }
            Library::DateTime { date, time, tzinfo } => {
                let mut text = format!(
                    "{name}({}, {}, {}, {})",
                    date.year,
                    date.month,
                    date.day,
                    clock(*time)
                );
                if time.fold != 0 {
                    keyword(&mut text, &format!("fold={}", time.fold));
                }

                self.tzinfo_text(&mut text, *tzinfo, depth, summary);
                text
            }
            Library::Delta {
                days,
                seconds,
                microseconds,
            } => format!("{name}({})", delta(*days, *seconds, *microseconds)),
            Library::TimeZone { offset, name: None } if self.zero_delta(*offset) => {
                format!("{name}.utc")
            }
            Library::TimeZone { offset, name: zone } => {
                let offset = self.item_text(*offset, depth + 1, summary);
                zone.map_or_else(
                    || format!("{name}({offset})"),
                    |zone| {
                        let zone = self.item_text(zone, depth + 1, summary);
                        format!("{name}({offset}, {zone})")
                    },
                )
            }
            Library::Decimal(Decimal::Read {
                flags,
                exponent,
                words,
            }) => format!("Decimal('{}')", scientific(*flags, *exponent, words)),
            Library::Array(array) => self.array_text(array, summary),
            Library::Decimal(Decimal::Large { digits }) => {
                note(
                    summary,
                    format!(
                        "a Decimal of more than {MAX_DECIMAL_DIGITS} digits is shown by its size"
                    ),
                );
                format!("<Decimal of {digits} digits>")
            }
        }
    }

    /// The text of a `defaultdict` within itself, as its `repr` writes it:
    /// its dict is the part `repr` has already entered.
    pub(super) fn recurring_library_text(
        &self,
        name: &str,
        value: &Library,
        depth: usize,
        summary: &mut Summary,
    ) -> String {
        let short = name.rsplit_once('.').map_or(name, |(_, name)| name);
        match value {
            Library::Deque { .. } => "[...]".to_owned(),
            Library::DefaultDict { factory, .. } => {
                let factory = self.factory_text(*factory, depth, summary);
                format!("{short}({factory}, {{...}})")
            }
            _ => "...".to_owned(),
        }
    }

    fn factory_text(
        &self,
        factory: Option<VirtualAddress>,
        depth: usize,
        summary: &mut Summary,
    ) -> String {
        factory.map_or_else(
            || "None".to_owned(),
            |factory| self.item_text(factory, depth + 1, summary),
        )
    }

    /// Appends a time's tzinfo to its text, unless it is `None`.
    fn tzinfo_text(
        &self,
        text: &mut String,
        tzinfo: Option<VirtualAddress>,
        depth: usize,
        summary: &mut Summary,
    ) {
        if let Some(tzinfo) = tzinfo.filter(|tzinfo| *tzinfo != self.types.none) {
            let tzinfo = self.item_text(tzinfo, depth + 1, summary);
            keyword(text, &format!("tzinfo={tzinfo}"));
        }
    }

    /// Whether the object at `offset` is a `timedelta` of no time, as the
    /// offset of the one `timezone` CPython makes of no offset and no name
    /// is: UTC.
    fn zero_delta(&self, offset: VirtualAddress) -> bool {
        matches!(
            self.object(offset),
            Ok(Object::Library {
                value: Library::Delta {
                    days: 0,
                    seconds: 0,
                    microseconds: 0
                },
                ..
            })
        )
    }
}

/// A count CPython keeps as a signed size.
fn signed(size: u64, address: VirtualAddress) -> Result<u64, Problem> {
    u64::try_from(size.cast_signed()).map_err(|_| malformed(address, "object"))
}

/// A date's packed bytes: its year, two bytes, high first, then its month
/// and its day.
fn date(data: &[u8], address: VirtualAddress) -> Result<Date, Problem> {
    let &[high, low, month, day] = data else {
        return Err(malformed(address, "date"));
    };
    Ok(Date {
        year: u16::from_be_bytes([high, low]),
        month,
        day,
    })
}

/// A time's packed bytes: its hour, minute, and second, then its
/// microsecond, three bytes, high first.
fn time(data: &[u8], fold: u8, address: VirtualAddress) -> Result<Time, Problem> {
    let &[hour, minute, second, high, middle, low] = data else {
        return Err(malformed(address, "time"));
    };
    Ok(Time {
        hour,
        minute,
        second,
        microsecond: u32::from_be_bytes([0, high, middle, low]),
        fold,
    })
}

/// A time's hour and minute, then its second and its microsecond where
/// they are not 0, as its `repr` writes them.
fn clock(time: Time) -> String {
    let Time {
        hour,
        minute,
        second,
        microsecond,
        ..
    } = time;
    if microsecond != 0 {
        format!("{hour}, {minute}, {second}, {microsecond}")
    } else if second != 0 {
        format!("{hour}, {minute}, {second}")
    } else {
        format!("{hour}, {minute}")
    }
}

/// A `timedelta`'s arguments, as its `repr` writes them: each part that is
/// not 0, by its keyword, or `0`.
fn delta(days: i32, seconds: i32, microseconds: i32) -> String {
    let parts: Vec<String> = [
        ("days", days),
        ("seconds", seconds),
        ("microseconds", microseconds),
    ]
    .into_iter()
    .filter(|(_, part)| *part != 0)
    .map(|(name, part)| format!("{name}={part}"))
    .collect();
    if parts.is_empty() {
        "0".to_owned()
    } else {
        parts.join(", ")
    }
}

/// Appends a keyword argument to a `repr`'s text, within its parentheses.
fn keyword(text: &mut String, argument: &str) {
    if text.ends_with(')') {
        text.pop();
    }

    text.push_str(", ");
    text.push_str(argument);
    text.push(')');
}

/// How many decimal digits `word` has; 0 has one.
fn word_digits(word: u64) -> u64 {
    word.checked_ilog10().map_or(1, |log| u64::from(log) + 1)
}

/// A libmpdec number in scientific notation, as `mpd_to_sci` writes it
/// with capitals: libmpdec's `_mpd_to_string`, without its other formats.
fn scientific(flags: u8, exponent: i64, words: &[u64]) -> String {
    let mut text = String::new();
    if flags & DECIMAL_NEGATIVE != 0 {
        text.push('-');
    }

    let coefficient =
        words
            .iter()
            .rev()
            .enumerate()
            .fold(String::new(), |mut digits, (index, word)| {
                if index == 0 {
                    digits.push_str(&word.to_string());
                } else {
                    let _ = write!(digits, "{word:019}");
                }
                digits
            });

    if flags & (DECIMAL_NAN | DECIMAL_SIGNALING) != 0 {
        text.push_str(if flags & DECIMAL_NAN != 0 {
            "NaN"
        } else {
            "sNaN"
        });
        text.push_str(&coefficient);
        return text;
    }

    if flags & DECIMAL_INFINITE != 0 {
        text.push_str("Infinity");
        return text;
    }

    // The number of digits left of the point, were the exponent 0, and
    // where the point is written, relative to the coefficient's first
    // digit: exponent notation, past the first digit, unless the number
    // is an integer or a fraction of fewer than six leading zeros.
    let digits = i128::try_from(coefficient.len()).unwrap_or(i128::MAX);
    let exponent = i128::from(exponent);
    let left = digits + exponent;
    let point = if exponent <= 0 && left > -6 { left } else { 1 };
    if point <= 0 {
        text.push_str("0.");
        text.extend(std::iter::repeat_n(
            '0',
            usize::try_from(-point).unwrap_or(0),
        ));
        text.push_str(&coefficient);
    } else if point >= digits {
        text.push_str(&coefficient);
        text.extend(std::iter::repeat_n(
            '0',
            usize::try_from(point - digits).unwrap_or(0),
        ));
    } else {
        let (whole, fraction) = coefficient.split_at(usize::try_from(point).unwrap_or(0));
        text.push_str(whole);
        text.push('.');
        text.push_str(fraction);
    }

    if left != point {
        let shown = left - point;
        text.push('E');
        text.push(if shown < 0 { '-' } else { '+' });
        text.push_str(&shown.unsigned_abs().to_string());
    }

    text
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The words of a coefficient, least significant first, from its
    /// digits.
    fn words(digits: &str) -> Vec<u64> {
        let mut words = Vec::new();
        let mut end = digits.len();
        while end > 0 {
            let start = end.saturating_sub(19);
            words.push(digits[start..end].parse().unwrap());
            end = start;
        }
        words
    }

    /// Each case's text is Python's `str(Decimal(text))`, which writes the
    /// number back as it reads it.
    #[test]
    fn a_decimal_is_written_as_mpd_to_sci_writes_it() {
        let cases: &[(&str, u8, i64, &str)] = &[
            ("0", 0, 0, "0"),
            ("-0", DECIMAL_NEGATIVE, 0, "0"),
            ("1.5", 0, -1, "15"),
            ("-12.345", DECIMAL_NEGATIVE, -3, "12345"),
            ("0.000001", 0, -6, "1"),
            ("1E-7", 0, -7, "1"),
            ("1.0E-7", 0, -8, "10"),
            ("0.00", 0, -2, "0"),
            ("0E-7", 0, -7, "0"),
            ("1E+3", 0, 3, "1"),
            ("1.23E+5", 0, 3, "123"),
            ("0E+2", 0, 2, "0"),
            ("100", 0, 0, "100"),
            (
                "12345678901234567890123.4567",
                0,
                -4,
                "123456789012345678901234567",
            ),
            ("1.0000000000000000000", 0, -19, "10000000000000000000"),
            ("Infinity", DECIMAL_INFINITE, 0, ""),
            ("-Infinity", DECIMAL_NEGATIVE | DECIMAL_INFINITE, 0, ""),
            ("NaN", DECIMAL_NAN, 0, ""),
            ("-sNaN", DECIMAL_NEGATIVE | DECIMAL_SIGNALING, 0, ""),
            ("NaN123", DECIMAL_NAN, 0, "123"),
        ];
        for &(expected, flags, exponent, digits) in cases {
            let coefficient = if digits.is_empty() {
                Vec::new()
            } else {
                words(digits)
            };
            assert_eq!(
                scientific(flags, exponent, &coefficient),
                expected,
                "{digits}E{exponent}"
            );
        }
    }

    #[test]
    fn a_word_has_at_least_one_digit() {
        assert_eq!(word_digits(0), 1);
        assert_eq!(word_digits(9), 1);
        assert_eq!(word_digits(10), 2);
        assert_eq!(word_digits(DECIMAL_RADIX - 1), 19);
    }

    #[test]
    fn a_timedelta_writes_the_parts_it_has() {
        assert_eq!(delta(0, 0, 0), "0");
        assert_eq!(delta(-1, 5, 0), "days=-1, seconds=5");
        assert_eq!(delta(0, 0, 7), "microseconds=7");
    }

    #[test]
    fn a_time_writes_its_seconds_and_microseconds_where_they_are_not_zero() {
        let time = |second, microsecond| Time {
            hour: 1,
            minute: 2,
            second,
            microsecond,
            fold: 0,
        };
        assert_eq!(clock(time(0, 0)), "1, 2");
        assert_eq!(clock(time(3, 0)), "1, 2, 3");
        assert_eq!(clock(time(0, 4)), "1, 2, 0, 4");
    }
}
