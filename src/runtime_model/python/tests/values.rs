//! The built-in values the model presents, read from the process memory
//! the builders below synthesize with the shared layout. The builders
//! write each object whole, so the values read exactly what the tests put
//! there.

use std::sync::Arc;

use super::super::code::CodeTable;
use super::super::objects::Reader;
use super::super::values::{Budget, Types, Values};
use super::{Process, SMALL, UNMAPPED, layout};
use crate::image::python::{Constant, Field};
use crate::inspection::InspectionBudget;
use crate::runtime_model::{
    InterpretedChild, InterpretedKind, InterpretedLocal, InterpretedSlot, InterpretedValue,
};
use crate::{
    InspectionExhaustion, InspectionLimit, InspectionLimits, VariableUnavailableReason,
    VirtualAddress,
};

/// The depth at which a summary shows `...`, as the model's summary gives
/// it. The model keeps the constant private, so the test mirrors it.
const SUMMARY_DEPTH: usize = 6;

/// How many items of a container a summary shows, mirrored as
/// [`SUMMARY_DEPTH`] is.
const SUMMARY_ITEMS: u64 = 64;

/// How many characters of a `str` a summary shows, mirrored as
/// [`SUMMARY_DEPTH`] is.
const SUMMARY_TEXT: usize = 1024;

/// The keys table of a combined `dict`: its kind of entries, and its index
/// and table sizes as log2 values.
#[derive(Clone, Copy)]
pub(super) struct Table {
    pub(super) kind: Kind,
    pub(super) log2_size: u8,
    pub(super) log2_index_bytes: u8,
}

impl Table {
    /// A table of eight slots, whose indices are one byte each.
    pub(super) const fn eight(kind: Kind) -> Self {
        Self {
            kind,
            log2_size: 3,
            log2_index_bytes: 3,
        }
    }
}

/// The two kinds of a combined `dict`'s entries: a general entry holds a
/// hash, and a unicode entry does not.
#[derive(Clone, Copy)]
pub(super) enum Kind {
    General,
    Unicode,
}

impl Process {
    /// A C string holding `text`, in memory whole-chunk reads stay within.
    fn c_string(&mut self, text: &str) -> u64 {
        let size = (text.len() as u64 + 1).next_multiple_of(16);
        let address = self.alloc(size);
        self.memory.fill(address, text.as_bytes());
        address
    }

    /// A type object named `name`, as CPython names a type.
    pub(super) fn type_object(&mut self, name: &str) -> VirtualAddress {
        let object = self.alloc(self.offset(Field::TypeName) + 8);
        let name = self.c_string(name);
        self.word(object, Field::TypeName, name);
        VirtualAddress::new(object)
    }

    /// The type objects of the nine built-in types the model presents.
    pub(super) fn builtin_types(&mut self) -> Types {
        Types {
            none: self.type_object("NoneType"),
            bool: self.type_object("bool"),
            int: self.type_object("int"),
            float: self.type_object("float"),
            str: self.type_object("str"),
            bytes: self.type_object("bytes"),
            list: self.type_object("list"),
            tuple: self.type_object("tuple"),
            dict: self.type_object("dict"),
        }
    }

    /// An object of type `ty` that is `size` zeroed bytes, with its type
    /// written.
    pub(super) fn object(&mut self, ty: VirtualAddress, size: u64) -> u64 {
        let object = self.alloc(size);
        self.word(object, Field::ObjectType, ty.get());
        object
    }

    /// Writes `values` as consecutive words from `at`.
    pub(super) fn words(&mut self, at: u64, values: &[u64]) {
        for (index, value) in (0_u64..).zip(values) {
            self.memory.write(at + 8 * index, *value, 8);
        }
    }

    /// The tag of an `int` with the sign bits `sign` and `digits` digits.
    pub(super) fn long_tag(&self, sign: u64, digits: usize) -> u64 {
        let shift = u32::from(self.layout.constant(Constant::LongNonSizeBits));
        sign | ((digits as u64) << shift)
    }

    /// An `int`-shaped object of type `ty`, with the tag `tag` and the
    /// 30-bit `digits`, least significant first.
    pub(super) fn long(&mut self, ty: VirtualAddress, tag: u64, digits: &[u32]) -> u64 {
        let size = self.offset(Field::LongDigits) + 4 * (digits.len() as u64);
        let object = self.object(ty, size);
        self.word(object, Field::LongTag, tag);
        let at = object + self.offset(Field::LongDigits);
        for (index, digit) in (0_u64..).zip(digits) {
            self.memory.write(at + 4 * index, u64::from(*digit), 4);
        }
        object
    }

    /// An `int` of type `ty` with the value `value`, in digits of the
    /// layout's shift.
    pub(super) fn int(&mut self, ty: VirtualAddress, value: i128) -> u64 {
        let shift = u32::from(self.layout.constant(Constant::LongShift));
        let mut magnitude = value.unsigned_abs();
        let mut digits = Vec::new();
        while magnitude != 0 {
            let digit = magnitude & ((1 << shift) - 1);
            digits.push(u32::try_from(digit).expect("a digit fits in 32 bits"));
            magnitude >>= shift;
        }

        let sign = if digits.is_empty() {
            1
        } else if value < 0 {
            2
        } else {
            0
        };
        let tag = self.long_tag(sign, digits.len());
        self.long(ty, tag, &digits)
    }

    /// A `float` of type `ty` with the value `value`.
    pub(super) fn float(&mut self, ty: VirtualAddress, value: f64) -> u64 {
        let object = self.object(ty, self.offset(Field::FloatValue) + 8);
        self.word(object, Field::FloatValue, value.to_bits());
        object
    }

    /// A `str` of type `ty` holding the ASCII `text`.
    pub(super) fn str_object(&mut self, ty: VirtualAddress, text: &str) -> u64 {
        let object = self.string(text);
        self.word(object, Field::ObjectType, ty.get());
        object
    }

    /// A `str` of type `ty` holding the two-byte code units `units`, which
    /// is compact and not ASCII.
    pub(super) fn wide_str(&mut self, ty: VirtualAddress, units: &[u16]) -> u64 {
        let compact = self.offset(Field::UnicodeCompactSize);
        let object = self.alloc(compact + 2 * (units.len() as u64));
        self.word(object, Field::ObjectType, ty.get());
        self.word(object, Field::UnicodeLength, units.len() as u64);
        let kind = 2 << self.layout.constant(Constant::UnicodeKindShift);
        let compact_bit = 1 << self.layout.constant(Constant::UnicodeCompactShift);
        self.put(object, Field::UnicodeState, kind | compact_bit, 4);
        let bytes: Vec<u8> = units.iter().flat_map(|unit| unit.to_le_bytes()).collect();
        self.memory.fill(object + compact, &bytes);
        object
    }

    /// A `tuple` of type `ty`, whose items lie inline.
    pub(super) fn tuple(&mut self, ty: VirtualAddress, items: &[u64]) -> u64 {
        let size = self.offset(Field::TupleItems) + 8 * (items.len() as u64);
        let object = self.object(ty, size);
        self.word(object, Field::TupleSize, items.len() as u64);
        let at = object + self.offset(Field::TupleItems);
        self.words(at, items);
        object
    }

    /// A `list` of type `ty` whose items are `items`.
    pub(super) fn list(&mut self, ty: VirtualAddress, items: &[u64]) -> u64 {
        let object = self.object(ty, self.offset(Field::ListItems) + 8);
        self.set_items(object, items);
        object
    }

    /// Points the `list` at `list` at an array holding `items`.
    fn set_items(&mut self, list: u64, items: &[u64]) {
        let array = if items.is_empty() {
            0
        } else {
            let array = self.alloc(8 * (items.len() as u64));
            self.words(array, items);
            array
        };
        self.word(list, Field::ListSize, items.len() as u64);
        self.word(list, Field::ListItems, array);
    }

    /// A combined `dict` of type `ty` whose header counts `used` items. Its
    /// table holds `entries` as (key, value) pairs in insertion order, and a
    /// value of 0 marks a deleted entry.
    pub(super) fn dict(
        &mut self,
        ty: VirtualAddress,
        table: Table,
        used: u64,
        entries: &[(u64, u64)],
    ) -> u64 {
        let (size_field, key_field, value_field, kind_constant) = entry_fields(table.kind);

        let size = self.offset(size_field);
        let indices = self.offset(Field::DictKeysIndices);
        let slots = 1_u64 << table.log2_index_bytes;
        let keys = self.alloc(indices + slots + size * (entries.len() as u64));
        let kind = u64::from(self.layout.constant(kind_constant));
        self.put(keys, Field::DictKeysLog2Size, u64::from(table.log2_size), 1);
        self.put(
            keys,
            Field::DictKeysLog2IndexBytes,
            u64::from(table.log2_index_bytes),
            1,
        );
        self.put(keys, Field::DictKeysKind, kind, 1);
        self.word(keys, Field::DictKeysNentries, entries.len() as u64);
        let first = keys + indices + slots;
        for (index, (key, value)) in (0_u64..).zip(entries) {
            let entry = first + size * index;
            self.put(entry, key_field, *key, 8);
            self.put(entry, value_field, *value, 8);
        }

        let object = self.object(ty, self.offset(Field::DictKeys) + 8);
        self.word(object, Field::DictUsed, used);
        self.word(object, Field::DictValues, 0);
        self.word(object, Field::DictKeys, keys);
        object
    }

    /// A combined `dict` of type `ty` whose table holds `entries` as (key,
    /// value, hash) triples in insertion order, with its index table filled
    /// as CPython's insertion fills it: each entry takes the first empty slot
    /// of its probe sequence. The entries listed in `deleted` keep their
    /// slots, which become deleted, and hold no key or value. The hash of a
    /// general table's entry is written to the entry, and that of a unicode
    /// table's key to the key's `str`, so the table's hashes agree with
    /// the entries' keys.
    pub(super) fn hashed_dict(
        &mut self,
        ty: VirtualAddress,
        table: Table,
        entries: &[(u64, u64, i64)],
        deleted: &[usize],
    ) -> u64 {
        let (size_field, key_field, value_field, kind_constant) = entry_fields(table.kind);
        let size = self.offset(size_field);
        let indices = self.offset(Field::DictKeysIndices);
        let slots = 1_u64 << table.log2_index_bytes;
        let width = 1_u64 << (table.log2_index_bytes - table.log2_size);
        let keys = self.alloc(indices + slots + size * (entries.len() as u64));
        let kind = u64::from(self.layout.constant(kind_constant));
        self.put(keys, Field::DictKeysLog2Size, u64::from(table.log2_size), 1);
        self.put(
            keys,
            Field::DictKeysLog2IndexBytes,
            u64::from(table.log2_index_bytes),
            1,
        );
        self.put(keys, Field::DictKeysKind, kind, 1);
        self.word(keys, Field::DictKeysNentries, entries.len() as u64);

        let index_slots = 1_u64 << table.log2_size;
        let mask = index_slots - 1;
        let width_bytes = usize::try_from(width).expect("an index is a few bytes wide");
        let at = |slot: u64| usize::try_from(slot).expect("a slot is within the table");
        let empty = (-1_i64).cast_unsigned();
        for slot in 0..index_slots {
            self.memory
                .write(keys + indices + slot * width, empty, width_bytes);
        }

        let mut taken = vec![false; at(index_slots)];
        let mut placed = Vec::with_capacity(entries.len());
        for (index, (_, _, hash)) in (0_u64..).zip(entries) {
            let mut slot = hash.cast_unsigned() & mask;
            let mut perturb = hash.cast_unsigned();
            while taken[at(slot)] {
                perturb >>= 5;
                slot = slot.wrapping_mul(5).wrapping_add(perturb).wrapping_add(1) & mask;
            }

            taken[at(slot)] = true;
            placed.push(slot);
            self.memory
                .write(keys + indices + slot * width, index, width_bytes);
        }

        for &index in deleted {
            let slot = placed[index];
            self.memory.write(
                keys + indices + slot * width,
                (-2_i64).cast_unsigned(),
                width_bytes,
            );
        }

        let first = keys + indices + slots;
        for (index, (key, value, hash)) in (0_u64..).zip(entries) {
            let entry = first + size * index;
            let live = !deleted.contains(&usize::try_from(index).expect("an entry index fits"));
            let (key, value) = if live { (*key, *value) } else { (0, 0) };
            self.put(entry, key_field, key, 8);
            self.put(entry, value_field, value, 8);
            match table.kind {
                Kind::General => self.put(entry, Field::DictEntryHash, hash.cast_unsigned(), 8),
                Kind::Unicode if live => {
                    self.put(key, Field::UnicodeHash, hash.cast_unsigned(), 8);
                }
                Kind::Unicode => {}
            }
        }

        let object = self.object(ty, self.offset(Field::DictKeys) + 8);
        self.word(
            object,
            Field::DictUsed,
            (entries.len() - deleted.len()) as u64,
        );
        self.word(object, Field::DictValues, 0);
        self.word(object, Field::DictKeys, keys);
        object
    }

    /// A `dict` of type `ty` that keeps its values apart from its keys, as
    /// an instance's do, and whose header counts `used` items.
    pub(super) fn split_dict(&mut self, ty: VirtualAddress, used: u64) -> u64 {
        let object = self.object(ty, self.offset(Field::DictKeys) + 8);
        let separate = self.alloc(16);
        self.word(object, Field::DictUsed, used);
        self.word(object, Field::DictValues, separate);
        object
    }

    /// Gives the code object `code` the local variables `names`, whose
    /// kinds are `kinds`.
    pub(super) fn locals(&mut self, types: &Types, code: u64, names: &[&str], kinds: &[u8]) {
        let name_objects: Vec<u64> = names.iter().map(|name| self.string(name)).collect();
        let tuple = self.tuple(types.tuple, &name_objects);
        let kinds = self.bytes(kinds);
        self.word(code, Field::CodeLocalsplusnames, tuple);
        self.word(code, Field::CodeLocalspluskinds, kinds);
    }
}

/// The fields of a combined `dict`'s entries of `kind`: the size of an
/// entry, where its key and value are, and the constant of its keys' kind.
fn entry_fields(kind: Kind) -> (Field, Field, Field, Constant) {
    match kind {
        Kind::General => (
            Field::DictEntrySize,
            Field::DictEntryKey,
            Field::DictEntryValue,
            Constant::DictKeysGeneral,
        ),
        Kind::Unicode => (
            Field::DictUnicodeEntrySize,
            Field::DictUnicodeEntryKey,
            Field::DictUnicodeEntryValue,
            Constant::DictKeysUnicode,
        ),
    }
}

/// Runs `read` on the values of `process`, which pays for its reads from a
/// budget of `limits`.
pub(super) fn with_values<R>(
    process: &Process,
    types: Types,
    limits: InspectionLimits,
    read: impl FnOnce(&Values<'_, '_>) -> R,
) -> R {
    with_secret(process, types, limits, None, read)
}

/// Runs `read` on the values of `process` with the process's hash secret at
/// `secret`, which pays for its reads from a budget of `limits`.
pub(super) fn with_secret<R>(
    process: &Process,
    types: Types,
    limits: InspectionLimits,
    secret: Option<VirtualAddress>,
    read: impl FnOnce(&Values<'_, '_>) -> R,
) -> R {
    let mut inspection = InspectionBudget::new(limits);
    let budget = Budget::new(&mut inspection);
    let values = Values {
        reader: Reader {
            stop: &process.memory,
            layout: &process.layout,
            charge: Some(&budget),
        },
        types,
        budget: &budget,
        secret,
        hashing: std::cell::Cell::new(None),
    };
    read(&values)
}

/// The value at `address`, read with the default limits.
fn value_at(process: &Process, types: Types, address: u64) -> InterpretedSlot {
    with_values(process, types, InspectionLimits::default(), |values| {
        values.value(VirtualAddress::new(address))
    })
}

/// The value at `address`, which must be one.
fn read_value(process: &Process, types: Types, address: u64) -> InterpretedValue {
    value_of(value_at(process, types, address))
}

/// The value a slot holds, which must be one.
fn value_of(slot: InterpretedSlot) -> InterpretedValue {
    match slot {
        InterpretedSlot::Value(value) => value,
        other => panic!("expected a value, got {other:?}"),
    }
}

/// Asserts that `value` is an object of the built-in type `name`.
fn assert_builtin(value: &InterpretedValue, name: &str) {
    assert_eq!(
        value.module.as_deref(),
        Some("builtins"),
        "the module of {name}"
    );
    assert_eq!(value.type_name.as_ref(), name);
}

/// How a slot reads, in words: a value's summary, or what it lacks.
fn describe(slot: &InterpretedSlot) -> String {
    match slot {
        InterpretedSlot::Value(value) => value.summary.to_string(),
        InterpretedSlot::Unbound => "unbound".to_owned(),
        InterpretedSlot::Cell => "cell".to_owned(),
        InterpretedSlot::Limit(_) => "limit".to_owned(),
        InterpretedSlot::Unreadable(problem) => format!("unreadable: {problem}"),
    }
}

/// The children `range` of the container at `address`, read with the
/// default limits.
fn children_of(
    process: &Process,
    types: Types,
    address: u64,
    range: std::ops::Range<u64>,
) -> Result<Vec<InterpretedChild>, Arc<str>> {
    with_values(process, types, InspectionLimits::default(), |values| {
        values.children(VirtualAddress::new(address), range)
    })
}

/// The summaries of each entry's key and value, of children that are all
/// entries.
fn entry_summaries(children: &[InterpretedChild]) -> Vec<(String, String)> {
    children
        .iter()
        .map(|child| match child {
            InterpretedChild::Entry { key, value } => (describe(key), describe(value)),
            InterpretedChild::Element(_) => panic!("a dict's children are entries"),
        })
        .collect()
}

/// The locals of the frame at `frame`, read with the default limits.
fn locals_of(process: &Process, types: Types, frame: u64) -> Vec<InterpretedLocal> {
    with_values(process, types, InspectionLimits::default(), |values| {
        let record = values
            .reader
            .record(VirtualAddress::new(frame), Field::FrameSize)
            .expect("the frame is readable");
        let code = record
            .word(Field::FrameExecutable)
            .expect("the frame names its code");
        let mut table = CodeTable::default();
        let code = table
            .code(
                &values.reader,
                VirtualAddress::new(process.code_type),
                VirtualAddress::new(code),
            )
            .expect("the code object is readable");
        values
            .locals(&table, code, &record)
            .expect("the locals are readable")
    })
}

/// Makes a frame of a code object with the locals a, b, c, d, and e, whose
/// first is a parameter, and whose slots are `slots`. The local c is
/// hidden, and d is a cell.
fn local_frame(process: &mut Process, types: &Types, slots: [u64; 5]) -> u64 {
    let code = process.code("f", 1, &[(1, 0)]);
    process.put(code, Field::CodeArgcount, 1, 4);
    process.locals(
        types,
        code,
        &["a", "b", "c", "d", "e"],
        &[0x20, 0x20, 0x30, 0x40, 0x20],
    );
    let frame = process.frame(0, code, process.instruction(code, 0));
    let at = frame + process.offset(Field::FrameLocalsplus);
    process.words(at, &slots);
    frame
}

#[test]
fn none_and_the_booleans_are_shown_by_their_names() {
    let mut process = Process::new(layout());
    let types = process.builtin_types();
    let none = process.object(types.none, SMALL);
    let yes = process.int(types.bool, 1);
    let no = process.int(types.bool, 0);

    let none = read_value(&process, types, none);
    assert_eq!(none.summary.as_ref(), "None");
    assert_eq!(none.kind, InterpretedKind::Nothing);
    assert_builtin(&none, "NoneType");
    assert_eq!(none.problem, None);

    let yes = read_value(&process, types, yes);
    assert_eq!(yes.summary.as_ref(), "True");
    assert_eq!(yes.kind, InterpretedKind::Bool(true));
    assert_builtin(&yes, "bool");
    assert_eq!(yes.problem, None);

    let no = read_value(&process, types, no);
    assert_eq!(no.summary.as_ref(), "False");
    assert_eq!(no.kind, InterpretedKind::Bool(false));
    assert_builtin(&no, "bool");
    assert_eq!(no.problem, None);
}

#[test]
fn ints_are_shown_in_decimal_with_their_value_where_it_fits_in_128_bits() {
    let mut process = Process::new(layout());
    let types = process.builtin_types();
    let cases: [(i128, &str); 7] = [
        (0, "0"),
        (7, "7"),
        (-12345, "-12345"),
        (1 << 30, "1073741824"),
        ((1 << 100) + 12345, "1267650600228229401496703217721"),
        (i128::MAX, "170141183460469231731687303715884105727"),
        (i128::MIN, "-170141183460469231731687303715884105728"),
    ];
    for (value, summary) in cases {
        let address = process.int(types.int, value);
        let shown = read_value(&process, types, address);
        assert_eq!(shown.summary.as_ref(), summary, "the int {value}");
        assert_eq!(shown.kind, InterpretedKind::Int(Some(value)));
        assert_builtin(&shown, "int");
        assert_eq!(shown.problem, None);
    }

    // Five digits reach past 128 bits, so the kind has no value.
    let digits = [1, 0, 0, 0, 1024];
    let tag = process.long_tag(0, digits.len());
    let big = process.long(types.int, tag, &digits);
    let shown = read_value(&process, types, big);
    assert_eq!(
        shown.summary.as_ref(),
        "1361129467683753853853498429727072845825"
    );
    assert_eq!(shown.kind, InterpretedKind::Int(None));
    assert_builtin(&shown, "int");
}

#[test]
fn an_object_whose_head_cannot_be_read_whole_is_read_by_its_fields() {
    let mut process = Process::new(layout());
    let types = process.builtin_types();
    let address = process.int(types.int, 7);
    let end = address + process.offset(Field::LongDigits) + 4;
    process.unmap(end, 64);

    let shown = read_value(&process, types, address);
    assert_eq!(shown.summary.as_ref(), "7");
    assert_eq!(shown.problem, None);
}

#[test]
fn floats_are_shown_as_their_shortest_repr() {
    let mut process = Process::new(layout());
    let types = process.builtin_types();
    for (value, summary) in [(0.5, "0.5"), (1e16, "1e+16")] {
        let address = process.float(types.float, value);
        let shown = read_value(&process, types, address);
        assert_eq!(shown.summary.as_ref(), summary);
        assert_eq!(shown.kind, InterpretedKind::Float(value));
        assert_builtin(&shown, "float");
        assert_eq!(shown.problem, None);
    }

    let address = process.float(types.float, f64::NAN);
    let shown = read_value(&process, types, address);
    assert_eq!(shown.summary.as_ref(), "nan");
    assert!(matches!(shown.kind, InterpretedKind::Float(value) if value.is_nan()));
}

#[test]
fn an_int_whose_tag_or_digits_disagree_is_unreadable_as_malformed() {
    let mut process = Process::new(layout());
    let types = process.builtin_types();
    let sign_bits_of_three = process.long_tag(3, 1);
    let digit_past_thirty_bits = process.long_tag(0, 1);
    let zero_with_a_digit = process.long_tag(1, 1);
    let cases = [
        process.long(types.int, sign_bits_of_three, &[1]),
        process.long(types.int, digit_past_thirty_bits, &[1 << 30]),
        process.long(types.int, zero_with_a_digit, &[1]),
    ];
    for address in cases {
        let slot = value_at(&process, types, address);
        let expected = format!("the int at {address:#x} is malformed");
        assert_eq!(slot, InterpretedSlot::Unreadable(expected.into()));
    }
}

#[test]
fn an_int_of_more_digits_than_it_reads_is_shown_by_its_size() {
    let mut process = Process::new(layout());
    let types = process.builtin_types();
    let tag = process.long_tag(0, 1025);
    let big = process.long(types.int, tag, &[]);

    let shown = read_value(&process, types, big);
    assert_eq!(shown.summary.as_ref(), "<int of 1025 digits>");
    assert_eq!(shown.kind, InterpretedKind::Int(None));
    assert_eq!(
        shown.problem.as_deref(),
        Some("an int of more than 1024 digits is shown by its size")
    );
    assert_builtin(&shown, "int");
}

#[test]
fn a_str_longer_than_the_summary_shows_its_first_characters_and_a_lone_surrogate_escaped() {
    let mut process = Process::new(layout());
    let types = process.builtin_types();
    let long = process.str_object(types.str, &"a".repeat(1030));
    let surrogate = process.wide_str(types.str, &[0xdc80]);

    let shown = read_value(&process, types, long);
    let first = "a".repeat(SUMMARY_TEXT);
    assert_eq!(shown.summary.as_ref(), format!("'{first}'..."));
    assert_eq!(shown.kind, InterpretedKind::Text);
    assert_eq!(
        shown.problem.as_deref(),
        Some("a str shows 1024 of its 1030 characters")
    );
    assert_builtin(&shown, "str");

    let shown = read_value(&process, types, surrogate);
    assert_eq!(shown.summary.as_ref(), "'\\udc80'");
    assert_builtin(&shown, "str");
    assert_eq!(shown.problem, None);
}

#[test]
fn lists_and_tuples_are_shown_with_their_items_in_order() {
    let mut process = Process::new(layout());
    let types = process.builtin_types();
    let one = process.int(types.int, 1);
    let two = process.str_object(types.str, "two");
    let none = process.object(types.none, SMALL);
    let list = process.list(types.list, &[one, two, none]);
    let single = process.tuple(types.tuple, &[one]);
    let empty = process.tuple(types.tuple, &[]);

    let shown = read_value(&process, types, list);
    assert_eq!(shown.summary.as_ref(), "[1, 'two', None]");
    assert_eq!(shown.kind, InterpretedKind::Sequence { length: 3 });
    assert_builtin(&shown, "list");
    assert_eq!(shown.problem, None);

    let shown = read_value(&process, types, single);
    assert_eq!(shown.summary.as_ref(), "(1,)");
    assert_eq!(shown.kind, InterpretedKind::Sequence { length: 1 });
    assert_builtin(&shown, "tuple");

    let shown = read_value(&process, types, empty);
    assert_eq!(shown.summary.as_ref(), "()");
    assert_eq!(shown.kind, InterpretedKind::Sequence { length: 0 });
    assert_builtin(&shown, "tuple");
}

#[test]
fn a_list_within_itself_is_shown_as_python_repr_shows_it() {
    let mut process = Process::new(layout());
    let types = process.builtin_types();
    let list = process.list(types.list, &[]);
    process.set_items(list, &[list]);

    // The top-level list is within the summary, so its item, the same
    // list, is the within case and shows `[...]`. The whole is `[[...]]`,
    // with no problem, since the repeat is the list's own text.
    let shown = read_value(&process, types, list);
    assert_eq!(shown.summary.as_ref(), "[[...]]");
    assert_eq!(shown.kind, InterpretedKind::Sequence { length: 1 });
    assert_eq!(shown.problem, None);
}

#[test]
fn a_list_nested_past_the_summary_depth_shows_an_ellipsis_at_the_limit() {
    let mut process = Process::new(layout());
    let types = process.builtin_types();
    // The innermost list makes one more level than the summary's depth.
    let mut nested = process.list(types.list, &[]);
    for _ in 0..SUMMARY_DEPTH {
        nested = process.list(types.list, &[nested]);
    }

    let shown = read_value(&process, types, nested);
    let open = "[".repeat(SUMMARY_DEPTH);
    let close = "]".repeat(SUMMARY_DEPTH);
    assert_eq!(shown.summary.as_ref(), format!("{open}...{close}"));
    assert_eq!(
        shown.problem.as_deref(),
        Some("a summary nests 6 containers deep")
    );
}

#[test]
fn a_list_longer_than_the_summary_shows_its_first_items_then_an_ellipsis() {
    let mut process = Process::new(layout());
    let types = process.builtin_types();
    let items: Vec<u64> = (0..=SUMMARY_ITEMS)
        .map(|value| process.int(types.int, i128::from(value)))
        .collect();
    let list = process.list(types.list, &items);

    // Each small object costs one read, so the default budget affords the
    // summary.
    let shown = read_value(&process, types, list);
    let first: Vec<String> = (0..SUMMARY_ITEMS).map(|value| value.to_string()).collect();
    assert_eq!(
        shown.summary.as_ref(),
        format!("[{}, ...]", first.join(", "))
    );
    assert_eq!(
        shown.kind,
        InterpretedKind::Sequence {
            length: SUMMARY_ITEMS + 1
        }
    );
    assert_eq!(
        shown.problem.as_deref(),
        Some("a summary shows 64 items of each container")
    );
}

#[test]
fn a_dict_with_str_keys_is_shown_with_its_entries_in_order() {
    let mut process = Process::new(layout());
    let types = process.builtin_types();
    let a = process.str_object(types.str, "a");
    let b = process.str_object(types.str, "b");
    let one = process.int(types.int, 1);
    let two = process.int(types.int, 2);
    let dict = process.dict(
        types.dict,
        Table::eight(Kind::Unicode),
        2,
        &[(a, one), (b, two)],
    );

    let shown = read_value(&process, types, dict);
    assert_eq!(shown.summary.as_ref(), "{'a': 1, 'b': 2}");
    assert_eq!(shown.kind, InterpretedKind::Map { length: 2 });
    assert_builtin(&shown, "dict");
    assert_eq!(shown.problem, None);
}

#[test]
fn a_dict_with_other_keys_is_shown_with_its_general_entries() {
    let mut process = Process::new(layout());
    let types = process.builtin_types();
    let key = process.int(types.int, 1);
    let value = process.str_object(types.str, "one");
    let dict = process.dict(types.dict, Table::eight(Kind::General), 1, &[(key, value)]);

    let shown = read_value(&process, types, dict);
    assert_eq!(shown.summary.as_ref(), "{1: 'one'}");
    assert_eq!(shown.kind, InterpretedKind::Map { length: 1 });
    assert_builtin(&shown, "dict");
    assert_eq!(shown.problem, None);
}

#[test]
fn a_deleted_dict_entry_is_passed_over_by_the_children() {
    let mut process = Process::new(layout());
    let types = process.builtin_types();
    let a = process.str_object(types.str, "a");
    let x = process.str_object(types.str, "x");
    let b = process.str_object(types.str, "b");
    let one = process.int(types.int, 1);
    let two = process.int(types.int, 2);
    let dict = process.dict(
        types.dict,
        Table::eight(Kind::Unicode),
        2,
        &[(a, one), (x, 0), (b, two)],
    );

    let children = children_of(&process, types, dict, 0..2).expect("the entries are readable");
    assert_eq!(
        entry_summaries(&children),
        [
            ("'a'".to_owned(), "1".to_owned()),
            ("'b'".to_owned(), "2".to_owned()),
        ]
    );
}

#[test]
fn a_dict_whose_count_disagrees_with_its_entries_is_malformed() {
    let mut process = Process::new(layout());
    let types = process.builtin_types();
    let a = process.str_object(types.str, "a");
    let b = process.str_object(types.str, "b");
    let one = process.int(types.int, 1);
    let two = process.int(types.int, 2);
    // The header counts one item, but the table holds two live entries.
    let dict = process.dict(
        types.dict,
        Table::eight(Kind::Unicode),
        1,
        &[(a, one), (b, two)],
    );
    let problem = format!("the dict at {dict:#x} is malformed");

    let shown = read_value(&process, types, dict);
    assert_eq!(shown.summary.as_ref(), "{'a': 1, 'b': 2, ...}");
    assert_eq!(shown.kind, InterpretedKind::Map { length: 1 });
    assert_eq!(shown.problem.as_deref(), Some(problem.as_str()));

    let children = children_of(&process, types, dict, 0..1);
    assert_eq!(children.err().as_deref(), Some(problem.as_str()));
}

#[test]
fn a_dict_whose_index_is_smaller_than_its_table_is_unreadable() {
    let mut process = Process::new(layout());
    let types = process.builtin_types();
    let a = process.str_object(types.str, "a");
    let one = process.int(types.int, 1);
    let table = Table {
        kind: Kind::Unicode,
        log2_size: 3,
        log2_index_bytes: 2,
    };
    let dict = process.dict(types.dict, table, 1, &[(a, one)]);

    assert_eq!(
        value_at(&process, types, dict),
        InterpretedSlot::Unreadable(format!("the dict at {dict:#x} is malformed").into())
    );
}

#[test]
fn a_split_dict_is_shown_by_its_size_and_has_no_children() {
    let mut process = Process::new(layout());
    let types = process.builtin_types();
    let split = process.split_dict(types.dict, 2);

    let shown = read_value(&process, types, split);
    assert_eq!(shown.summary.as_ref(), "<dict of 2 items>");
    assert_eq!(shown.kind, InterpretedKind::Other);
    assert!(
        shown
            .problem
            .as_deref()
            .is_some_and(|problem| problem.contains("keeps its values apart")),
        "the problem: {:?}",
        shown.problem
    );
    assert!(children_of(&process, types, split, 0..2).is_err());
}

#[test]
fn an_object_of_another_type_is_shown_by_its_module_its_name_and_its_address() {
    let mut process = Process::new(layout());
    let types = process.builtin_types();
    let lock_type = process.type_object("_thread.lock");
    let lock = process.object(lock_type, SMALL);
    let gate_type = process.type_object("Gate");
    let gate = process.object(gate_type, SMALL);

    let shown = read_value(&process, types, lock);
    assert_eq!(shown.module.as_deref(), Some("_thread"));
    assert_eq!(shown.type_name.as_ref(), "lock");
    assert_eq!(shown.kind, InterpretedKind::Other);
    assert_eq!(
        shown.summary.as_ref(),
        format!("<_thread.lock object at {lock:#x}>")
    );
    assert_eq!(shown.problem, None);

    let shown = read_value(&process, types, gate);
    assert_eq!(shown.module, None);
    assert_eq!(shown.type_name.as_ref(), "Gate");
    assert_eq!(
        shown.summary.as_ref(),
        format!("<Gate object at {gate:#x}>")
    );
}

#[test]
fn a_list_item_at_unmapped_memory_is_unreadable_in_the_summary_and_the_children() {
    let mut process = Process::new(layout());
    let types = process.builtin_types();
    let list = process.list(types.list, &[UNMAPPED]);
    let problem = format!("the memory at {UNMAPPED:#x} is unreadable");

    let shown = read_value(&process, types, list);
    assert_eq!(
        shown.summary.as_ref(),
        format!("[<unreadable object at {UNMAPPED:#x}>]")
    );
    assert_eq!(shown.problem.as_deref(), Some(problem.as_str()));

    let children = children_of(&process, types, list, 0..1).expect("the items are readable");
    match children.as_slice() {
        [InterpretedChild::Element(slot)] => {
            assert_eq!(*slot, InterpretedSlot::Unreadable(problem.into()));
        }
        other => panic!("expected one element, got {other:?}"),
    }
}

#[test]
fn an_element_the_budget_cannot_afford_is_a_limit_and_the_ones_before_it_are_read() {
    let mut process = Process::new(layout());
    let types = process.builtin_types();
    let items: Vec<u64> = (1..=3).map(|value| process.int(types.int, value)).collect();
    let list = process.list(types.list, &items);
    // Reading the list takes one read, its items one more, and each int
    // one, so the first int is the only one that fits.
    let limits = InspectionLimits {
        memory_reads: 3,
        ..InspectionLimits::default()
    };

    let children = with_values(&process, types, limits, |values| {
        values.children(VirtualAddress::new(list), 0..3)
    })
    .expect("the list's items are read");
    let slots: Vec<&InterpretedSlot> = children
        .iter()
        .map(|child| match child {
            InterpretedChild::Element(slot) => slot,
            InterpretedChild::Entry { .. } => panic!("a list has no entries"),
        })
        .collect();

    assert_eq!(describe(slots[0]), "1");
    assert!(
        matches!(
            slots[1],
            InterpretedSlot::Limit(exhaustion)
                if exhaustion.resource == InspectionLimit::MemoryReads && exhaustion.limit == 3
        ),
        "the second element: {:?}",
        slots[1]
    );
    assert_eq!(describe(slots[2]), "limit");
}

#[test]
fn a_summary_the_budget_runs_out_in_ends_where_it_ran_out() {
    let mut process = Process::new(layout());
    let types = process.builtin_types();
    let inner: Vec<u64> = (3..=4).map(|value| process.int(types.int, value)).collect();
    let inner = process.list(types.list, &inner);
    let items = [
        process.int(types.int, 1),
        process.int(types.int, 2),
        inner,
        process.int(types.int, 5),
    ];
    let list = process.list(types.list, &items);
    let mapping = process.dict(
        types.dict,
        Table::eight(Kind::General),
        2,
        &[(items[0], items[1]), (items[3], inner)],
    );

    // The outer list, its items, two ints, the inner list, its items, and
    // its first int: the inner list is cut short, so it is left out.
    let limits = InspectionLimits {
        memory_reads: 7,
        ..InspectionLimits::default()
    };
    let shown = value_of(with_values(&process, types, limits, |values| {
        values.value(VirtualAddress::new(list))
    }));
    assert_eq!(shown.summary.as_ref(), "[1, 2, ...]");
    let spent = VariableUnavailableReason::InspectionLimit(InspectionExhaustion {
        resource: InspectionLimit::MemoryReads,
        limit: 7,
        used: 7,
        requested: 1,
    })
    .to_string();
    assert_eq!(shown.problem.as_deref(), Some(spent.as_str()));

    // The dict, its keys, its entries, and the first entry's key and value.
    let limits = InspectionLimits {
        memory_reads: 5,
        ..InspectionLimits::default()
    };
    let shown = value_of(with_values(&process, types, limits, |values| {
        values.value(VirtualAddress::new(mapping))
    }));
    assert_eq!(shown.summary.as_ref(), "{1: 2, ...}");
}

#[test]
fn a_frame_shows_its_parameters_values_cells_and_unbound_locals_but_not_hidden_ones() {
    let mut process = Process::new(layout());
    let types = process.builtin_types();
    let five = process.int(types.int, 5);
    let seven = process.int(types.int, 7);
    let frame = local_frame(&mut process, &types, [five, 0, 0, 0, seven]);

    let shown: Vec<(String, bool, String)> = locals_of(&process, types, frame)
        .iter()
        .map(|local| {
            (
                local.name.to_string(),
                local.parameter,
                describe(&local.slot),
            )
        })
        .collect();
    assert_eq!(
        shown,
        [
            ("a".to_owned(), true, "5".to_owned()),
            ("b".to_owned(), false, "unbound".to_owned()),
            ("d".to_owned(), false, "cell".to_owned()),
            ("e".to_owned(), false, "7".to_owned()),
        ]
    );
}

#[test]
fn a_stack_reference_reads_its_object_and_a_tagged_integer_is_unreadable() {
    let mut process = Process::new(layout());
    process.layout.set_constant(Constant::StackRefs, 1);
    let types = process.builtin_types();
    let five = process.int(types.int, 5);
    // A pointer with its counting bit, an unbound slot, and a tagged int.
    let frame = local_frame(&mut process, &types, [five | 1, 1, 0, 0, (5 << 2) | 3]);

    let shown: Vec<(String, String)> = locals_of(&process, types, frame)
        .iter()
        .map(|local| (local.name.to_string(), describe(&local.slot)))
        .collect();
    assert_eq!(
        shown,
        [
            ("a".to_owned(), "5".to_owned()),
            ("b".to_owned(), "unbound".to_owned()),
            ("d".to_owned(), "cell".to_owned()),
            (
                "e".to_owned(),
                "unreadable: the variable holds a tagged integer, which only the interpreter uses"
                    .to_owned(),
            ),
        ]
    );
}
