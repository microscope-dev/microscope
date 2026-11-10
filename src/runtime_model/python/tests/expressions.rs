//! What expressions read of a frame's names, of a `dict`'s keys, and of an
//! object: the reads `Values` makes for them, over the synthetic process the
//! builders in `values` write. A `str` key is found by its hash only with a
//! hash secret the process holds, so the tests that need one give the process
//! a zeroed secret, as CPython's `PYTHONHASHSEED=0` gives.

use super::super::code::{CodeId, CodeTable};
use proptest::prelude::*;
use proptest::test_runner::TestCaseError;

use super::super::expressions::{hash_float, hash_int, hash_text};
use super::super::objects::Record;
use super::super::values::{Types, Values};
use super::values::{Kind, Table, with_secret, with_values};
use super::{Process, SMALL, layout};
use crate::eval::error::ErrorKind;
use crate::image::python::Field;
use crate::runtime_model::{
    InterpretedChild, InterpretedFailure, InterpretedHandle, InterpretedKey, InterpretedName,
    InterpretedSlot, InterpretedStep,
};
use crate::{InspectionLimit, InspectionLimits, VariableUnavailableReason, VirtualAddress};

/// The kinds of local the tests give a code object: a plain local, a
/// cell, and a hidden one.
const LOCAL: u8 = 0x20;
const CELL: u8 = 0x40;
const HIDDEN: u8 = 0x10;

/// The code points of `text`, as a `str` of it has them.
fn code_points(text: &str) -> Vec<u32> {
    text.chars().map(u32::from).collect()
}

/// A secret of zeroes, as `PYTHONHASHSEED=0` gives the process.
fn zero_secret(process: &mut Process) -> VirtualAddress {
    VirtualAddress::new(process.alloc(16))
}

/// A secret of `k0` and zeroes, whose first word is `k0`.
fn secret_with_k0(process: &mut Process, k0: u64) -> VirtualAddress {
    let secret = zero_secret(process);
    process.memory.write(secret.get(), k0, 8);
    secret
}

/// A `dict`'s entry of the `int` key `value`, which holds `item`.
fn int_entry(process: &mut Process, types: &Types, value: i128, item: u64) -> (u64, u64, i64) {
    (process.int(types.int, value), item, hash_int(value))
}

/// A `dict` of `str` keys with the values `entries`, whose table is `table`,
/// with each key's hash as the zeroed secret gives it.
fn text_dict(process: &mut Process, types: &Types, table: Table, entries: &[(&str, u64)]) -> u64 {
    let mut hashed = Vec::with_capacity(entries.len());
    for (text, value) in entries {
        let key = process.str_object(types.str, text);
        hashed.push((key, *value, hash_text((0, 0), &code_points(text))));
    }

    process.hashed_dict(types.dict, table, &hashed, &[])
}

/// The record of the frame at `frame`, and the row of the code it runs.
fn frame_code<'a>(
    values: &Values<'a, '_>,
    code_type: u64,
    frame: u64,
) -> (Record<'a>, CodeTable, CodeId) {
    let record = values
        .reader
        .record(VirtualAddress::new(frame), Field::FrameSize)
        .expect("the frame is readable");
    let code = record
        .word(Field::FrameExecutable)
        .expect("the frame names its code");
    let mut table = CodeTable::default();
    let id = table
        .code(
            &values.reader,
            VirtualAddress::new(code_type),
            VirtualAddress::new(code),
        )
        .expect("the code object is readable");
    (record, table, id)
}

/// The name `name` means in the frame at `frame`, with the default limits.
fn name_in(
    process: &Process,
    types: Types,
    secret: Option<VirtualAddress>,
    frame: u64,
    name: &str,
) -> Result<Option<InterpretedName>, InterpretedFailure> {
    with_secret(
        process,
        types,
        InspectionLimits::default(),
        secret,
        |values| {
            let (record, table, code) = frame_code(values, process.code_type, frame);
            values.name(&table, code, &record, name)
        },
    )
}

/// The object the slot `slot` of the frame at `frame` holds, with the default
/// limits.
fn variable_in(
    process: &Process,
    types: Types,
    frame: u64,
    slot: u32,
) -> Result<VirtualAddress, InterpretedFailure> {
    with_values(process, types, InspectionLimits::default(), |values| {
        let (record, table, code) = frame_code(values, process.code_type, frame);
        values.variable(&table, code, &record, slot)
    })
}

/// The object `key` finds in the object at `object`, with the default limits
/// and the process's `secret`.
fn item_in(
    process: &Process,
    types: Types,
    secret: Option<VirtualAddress>,
    object: u64,
    key: &InterpretedKey,
) -> Result<VirtualAddress, InterpretedFailure> {
    with_secret(
        process,
        types,
        InspectionLimits::default(),
        secret,
        |values| values.step(VirtualAddress::new(object), InterpretedStep::Item(key)),
    )
}

/// The length of the object at `object`, with the default limits.
fn length_of(process: &Process, types: Types, object: u64) -> Result<u64, InterpretedFailure> {
    with_values(process, types, InspectionLimits::default(), |values| {
        values.length(VirtualAddress::new(object))
    })
}

/// Makes a frame of a code object whose locals are `names`, of `kinds`, and
/// whose slots are `slots`, and whose globals and built-ins are the `dict`s
/// at `globals` and `builtins`, when they are not zero.
fn frame_with(
    process: &mut Process,
    types: &Types,
    names: &[&str],
    kinds: &[u8],
    slots: &[u64],
    globals: u64,
    builtins: u64,
) -> u64 {
    let code = process.code("f", 1, &[(1, 0)]);
    process.locals(types, code, names, kinds);
    let frame = process.frame(0, code, process.instruction(code, 0));
    let at = frame + process.offset(Field::FrameLocalsplus);
    process.words(at, slots);
    process.word(frame, Field::FrameGlobals, globals);
    process.word(frame, Field::FrameBuiltins, builtins);
    frame
}

#[test]
fn a_name_is_a_variable_of_the_frame_before_any_global_or_built_in() {
    let mut process = Process::new(layout());
    let types = process.builtin_types();
    let local = process.int(types.int, 1);
    let global = process.int(types.int, 2);
    let builtin = process.int(types.int, 3);
    let globals = text_dict(
        &mut process,
        &types,
        Table::eight(Kind::Unicode),
        &[("x", global)],
    );
    let builtins = text_dict(
        &mut process,
        &types,
        Table::eight(Kind::Unicode),
        &[("x", builtin)],
    );
    let frame = frame_with(
        &mut process,
        &types,
        &["x"],
        &[LOCAL],
        &[local],
        globals,
        builtins,
    );
    let secret = Some(zero_secret(&mut process));

    assert_eq!(
        name_in(&process, types, secret, frame, "x"),
        Ok(Some(InterpretedName::Variable(0)))
    );
}

#[test]
fn a_name_is_a_global_before_a_built_in_of_the_same_name() {
    let mut process = Process::new(layout());
    let types = process.builtin_types();
    let global = process.int(types.int, 2);
    let builtin_y = process.int(types.int, 3);
    let builtin_len = process.int(types.int, 4);
    let globals = text_dict(
        &mut process,
        &types,
        Table::eight(Kind::Unicode),
        &[("y", global)],
    );
    let builtins = text_dict(
        &mut process,
        &types,
        Table::eight(Kind::Unicode),
        &[("y", builtin_y), ("len", builtin_len)],
    );
    let frame = frame_with(&mut process, &types, &[], &[], &[], globals, builtins);
    let secret = Some(zero_secret(&mut process));

    assert_eq!(
        name_in(&process, types, secret, frame, "y"),
        Ok(Some(InterpretedName::Object(InterpretedHandle::new(
            global
        ))))
    );
    assert_eq!(
        name_in(&process, types, secret, frame, "len"),
        Ok(Some(InterpretedName::Object(InterpretedHandle::new(
            builtin_len
        ))))
    );
}

#[test]
fn a_hidden_local_is_not_a_name_and_a_name_in_no_namespace_is_none() {
    let mut process = Process::new(layout());
    let types = process.builtin_types();
    let hidden = process.int(types.int, 1);
    let global = process.int(types.int, 2);
    let globals = text_dict(
        &mut process,
        &types,
        Table::eight(Kind::Unicode),
        &[("h", global)],
    );
    let builtins = text_dict(&mut process, &types, Table::eight(Kind::Unicode), &[]);
    let frame = frame_with(
        &mut process,
        &types,
        &["h"],
        &[HIDDEN],
        &[hidden],
        globals,
        builtins,
    );
    let secret = Some(zero_secret(&mut process));

    // The hidden local does not shadow the global of its name, and a name no
    // namespace holds is no name.
    assert_eq!(
        name_in(&process, types, secret, frame, "h"),
        Ok(Some(InterpretedName::Object(InterpretedHandle::new(
            global
        ))))
    );
    assert_eq!(name_in(&process, types, secret, frame, "zz"), Ok(None));
}

#[test]
fn a_null_slot_is_unbound_and_a_cell_holds_its_variable() {
    let mut process = Process::new(layout());
    let types = process.builtin_types();
    let three = process.int(types.int, 3);
    let cell = process.object(types.cell, SMALL);
    process.word(cell, Field::CellRef, three);
    let empty = process.object(types.cell, SMALL);
    let frame = frame_with(
        &mut process,
        &types,
        &["a", "b", "c", "d"],
        &[LOCAL, CELL, CELL, CELL],
        &[0, cell, empty, three],
        0,
        0,
    );

    assert_eq!(
        variable_in(&process, types, frame, 0),
        Err(InterpretedFailure::Unavailable(
            VariableUnavailableReason::Unbound
        ))
    );
    assert_eq!(
        variable_in(&process, types, frame, 1),
        Ok(VirtualAddress::new(three))
    );
    assert_eq!(
        variable_in(&process, types, frame, 2),
        Err(InterpretedFailure::Unavailable(
            VariableUnavailableReason::Unbound
        ))
    );

    // A parameter the frame has not yet put in its cell is its object.
    assert_eq!(
        variable_in(&process, types, frame, 3),
        Ok(VirtualAddress::new(three))
    );
}

#[test]
fn a_key_other_than_an_int_or_bool_is_refused_as_a_type() {
    let mut process = Process::new(layout());
    let types = process.builtin_types();
    let zero = process.int(types.int, 10);
    let one = process.int(types.int, 11);
    let list = process.list(types.list, &[zero, one]);

    for key in [
        InterpretedKey::Text(b"0".to_vec()),
        InterpretedKey::Float(1.0),
        InterpretedKey::Nothing,
    ] {
        assert!(
            matches!(
                item_in(&process, types, None, list, &key),
                Err(InterpretedFailure::Refused(ErrorKind::Type, _))
            ),
            "the key {key:?} indexes a list"
        );
    }
}

#[test]
fn the_least_float_an_int_key_can_equal_finds_it() {
    let mut process = Process::new(layout());
    let types = process.builtin_types();
    let item = process.str_object(types.str, "one");
    let least = process.str_object(types.str, "least");
    let entries = [
        int_entry(&mut process, &types, 1, item),
        int_entry(&mut process, &types, i128::MIN, least),
    ];
    let dict = process.hashed_dict(types.dict, Table::eight(Kind::General), &entries, &[]);

    // -2.0**127 is exactly the least i128.
    let key = InterpretedKey::Float(-(2.0_f64.powi(127)));
    assert_eq!(
        item_in(&process, types, None, dict, &key),
        Ok(VirtualAddress::new(least))
    );
}

#[test]
fn a_str_key_is_found_by_its_probe_when_the_secret_gives_the_hashes_the_dict_keeps() {
    let mut process = Process::new(layout());
    let types = process.builtin_types();
    let secret = zero_secret(&mut process);
    let items: Vec<u64> = (0..3).map(|value| process.int(types.int, value)).collect();
    let dict = text_dict(
        &mut process,
        &types,
        Table::eight(Kind::Unicode),
        &[("a", items[0]), ("bb", items[1]), ("ccc", items[2])],
    );

    let (found, hashing) = with_secret(
        &process,
        types,
        InspectionLimits::default(),
        Some(secret),
        |values| {
            let found = values.step(
                VirtualAddress::new(dict),
                InterpretedStep::Item(&InterpretedKey::Text(b"bb".to_vec())),
            );
            (found, values.hashing.get())
        },
    );
    assert_eq!(found, Ok(VirtualAddress::new(items[1])));
    assert_eq!(hashing, Some(true));
}

#[test]
fn a_str_key_is_found_by_scanning_when_the_secret_gives_other_hashes() {
    let mut process = Process::new(layout());
    let types = process.builtin_types();
    let secret = secret_with_k0(&mut process, 1);
    let item = process.int(types.int, 42);
    let dict = text_dict(
        &mut process,
        &types,
        Table::eight(Kind::Unicode),
        &[("a", item)],
    );

    let (found, hashing) = with_secret(
        &process,
        types,
        InspectionLimits::default(),
        Some(secret),
        |values| {
            let found = values.step(
                VirtualAddress::new(dict),
                InterpretedStep::Item(&InterpretedKey::Text(b"a".to_vec())),
            );
            (found, values.hashing.get())
        },
    );
    assert_eq!(found, Ok(VirtualAddress::new(item)));
    assert_eq!(hashing, Some(false));
}

#[test]
fn a_str_key_is_found_by_scanning_when_no_secret_is_known() {
    let mut process = Process::new(layout());
    let types = process.builtin_types();
    let item = process.int(types.int, 42);
    let dict = text_dict(
        &mut process,
        &types,
        Table::eight(Kind::Unicode),
        &[("a", item)],
    );

    assert_eq!(
        item_in(
            &process,
            types,
            None,
            dict,
            &InterpretedKey::Text(b"a".to_vec())
        ),
        Ok(VirtualAddress::new(item))
    );
}

#[test]
fn a_nan_key_finds_nothing_by_scanning() {
    let mut process = Process::new(layout());
    let types = process.builtin_types();
    let item = process.int(types.int, 42);
    let entries = [int_entry(&mut process, &types, 1, item)];
    let dict = process.hashed_dict(types.dict, Table::eight(Kind::General), &entries, &[]);

    assert!(
        matches!(
            item_in(
                &process,
                types,
                None,
                dict,
                &InterpretedKey::Float(f64::NAN)
            ),
            Err(InterpretedFailure::Refused(ErrorKind::MissingKey, _))
        ),
        "a NaN finds no key"
    );
}

#[test]
fn a_table_whose_probe_never_meets_an_empty_slot_is_unavailable() {
    let mut process = Process::new(layout());
    let types = process.builtin_types();
    let key = process.int(types.int, 1);
    let item = process.int(types.int, 42);
    // Every slot holds the index of the one entry, whose hash is zero, so
    // no probe for 2 ever meets an empty slot.
    let dict = process.dict(types.dict, Table::eight(Kind::General), 1, &[(key, item)]);

    assert!(
        matches!(
            item_in(&process, types, None, dict, &InterpretedKey::Int(2)),
            Err(InterpretedFailure::Unavailable(
                VariableUnavailableReason::Interpreted(_)
            ))
        ),
        "the malformed table is unavailable"
    );
}

#[test]
fn a_str_key_in_a_large_table_is_found_by_probing_within_a_small_budget_that_a_scan_exhausts() {
    let mut process = Process::new(layout());
    let types = process.builtin_types();
    let secret = zero_secret(&mut process);
    let names: Vec<String> = (0..300).map(|index| format!("key{index}")).collect();
    let items: Vec<u64> = (0..300)
        .map(|index| process.int(types.int, index))
        .collect();
    let entries: Vec<(&str, u64)> = names
        .iter()
        .map(String::as_str)
        .zip(items.iter().copied())
        .collect();
    let table = Table {
        kind: Kind::Unicode,
        log2_size: 9,
        log2_index_bytes: 10,
    };
    let dict = text_dict(&mut process, &types, table, &entries);
    let wanted = InterpretedKey::Text(b"key299".to_vec());
    let limits = InspectionLimits {
        memory_reads: 64,
        ..InspectionLimits::default()
    };

    let found = with_secret(&process, types, limits, Some(secret), |values| {
        values.step(VirtualAddress::new(dict), InterpretedStep::Item(&wanted))
    });
    assert_eq!(found, Ok(VirtualAddress::new(items[299])));

    let scanned = with_secret(&process, types, limits, None, |values| {
        values.step(VirtualAddress::new(dict), InterpretedStep::Item(&wanted))
    });
    assert!(
        matches!(
            scanned,
            Err(InterpretedFailure::Unavailable(
                VariableUnavailableReason::InspectionLimit(exhaustion)
            )) if exhaustion.resource == InspectionLimit::MemoryReads
        ),
        "the scan: {scanned:?}"
    );
}

#[test]
fn an_int_has_no_length() {
    let mut process = Process::new(layout());
    let types = process.builtin_types();
    let int = process.int(types.int, 5);
    assert!(matches!(
        length_of(&process, types, int),
        Err(InterpretedFailure::Refused(ErrorKind::Type, _))
    ));
}

/// CPython's hash of the finite `value`, worked from what the value is:
/// m·2^e for its integral significand m, reduced modulo 2^61 − 1, where
/// 2^60 is the inverse of 2; then signed as the value, with −1, which
/// CPython keeps for errors, made −2.
fn reference_float_hash(value: f64) -> i64 {
    const MODULUS: u128 = (1 << 61) - 1;
    let bits = value.to_bits();
    let biased = i32::try_from((bits >> 52) & 0x7ff).unwrap();
    let fraction = bits & ((1 << 52) - 1);
    let (significand, exponent) = if biased == 0 {
        (fraction, -1074)
    } else {
        (fraction | 1 << 52, biased - 1075)
    };

    let (factor, times) = if exponent >= 0 {
        (2, exponent)
    } else {
        (MODULUS.div_ceil(2), -exponent)
    };
    let mut hash = u128::from(significand) % MODULUS;
    for _ in 0..times {
        hash = hash * factor % MODULUS;
    }

    let hash = i64::try_from(hash).unwrap();
    match if value < 0.0 { -hash } else { hash } {
        -1 => -2,
        hash => hash,
    }
}

/// Any finite float, its exponent field below that of infinity: zeroes,
/// subnormals, and normals of each sign.
fn finite_float() -> impl Strategy<Value = f64> {
    (any::<bool>(), 0_u64..0x7ff, any::<u64>()).prop_map(|(negative, exponent, fraction)| {
        f64::from_bits(u64::from(negative) << 63 | exponent << 52 | fraction & ((1 << 52) - 1))
    })
}

/// A float that is an integer below 2^127 in magnitude.
fn integral_float() -> impl Strategy<Value = f64> {
    (-(1_i64 << 53)..=1 << 53, 0..=73_i32).prop_map(|(significand, shift)| {
        #[expect(
            clippy::cast_precision_loss,
            reason = "the significand fits in 53 bits"
        )]
        let significand = significand as f64;
        significand * 2_f64.powi(shift)
    })
}

proptest! {
    #[test]
    fn a_floats_hash_is_its_value_modulo_the_hash_prime(value in finite_float()) {
        prop_assert_eq!(hash_float(value), Some(reference_float_hash(value)), "{:e}", value);
        prop_assert_ne!(hash_float(value), Some(-1));
    }

    #[test]
    fn an_integral_float_hashes_as_its_int(value in integral_float()) {
        #[expect(clippy::cast_possible_truncation, reason = "the value is an integer below 2^127")]
        let int = value as i128;
        prop_assert_eq!(hash_float(value), Some(hash_int(int)), "{:e}", value);
        prop_assert_eq!(hash_float(value), Some(reference_float_hash(value)), "{:e}", value);
    }
}

/// A combined `dict` of int keys as a model: its table's size and index
/// width, its keys in insertion order, and which of them were deleted.
#[derive(Debug, Clone)]
struct IntDict {
    log2_size: u8,
    log2_width: u8,
    keys: Vec<i128>,
    deleted: Vec<bool>,

    /// Keys it never held, which collide with those it holds.
    absent: Vec<i128>,
}

/// Tables of 8 to 256 slots, with one- or two-byte indices, filled to
/// CPython's two thirds at most, and their keys forced to collide: each
/// is one of a few residues modulo the table's size, so most share their
/// first slot and probe past one another.
fn int_dict() -> impl Strategy<Value = IntDict> {
    (3..=8_u8, 0..=1_u8)
        .prop_flat_map(|(log2_size, log2_width)| {
            let slots = 1_i128 << log2_size;
            let usable = (2 << log2_size) / 3;
            let most = if log2_width == 0 {
                usable.min(127)
            } else {
                usable
            };
            let key = (any::<bool>(), 0..3_i128, 0..1024_i128).prop_map(
                move |(negative, residue, multiple)| {
                    let key = residue + slots * multiple;
                    if negative { -key } else { key }
                },
            );
            (
                Just((log2_size, log2_width)),
                proptest::collection::btree_set(key.clone(), 0..=most),
                proptest::collection::vec(any::<prop::sample::Index>(), 0..8),
                proptest::collection::vec(key, 0..8),
            )
        })
        .prop_flat_map(|((log2_size, log2_width), keys, deleted, absent)| {
            // A set's keys are in order; a dict's are in any.
            Just(keys.into_iter().collect::<Vec<_>>())
                .prop_shuffle()
                .prop_map(move |keys| {
                    let mut gone = vec![false; keys.len()];
                    if !keys.is_empty() {
                        for index in &deleted {
                            gone[index.index(keys.len())] = true;
                        }
                    }
                    let absent = absent
                        .iter()
                        .copied()
                        .filter(|key| !keys.contains(key))
                        .collect();
                    IntDict {
                        log2_size,
                        log2_width,
                        keys,
                        deleted: gone,
                        absent,
                    }
                })
        })
}

proptest! {
    /// A dict of colliding int keys finds each key it holds by CPython's
    /// probe, finds none it deleted or never held, and lists its items in
    /// insertion order, across the chunks its entries are read in. It
    /// takes about 0.6 s: a third of its tables hold more than 64 keys,
    /// each looked up through a fresh reader.
    #[test]
    fn a_dict_of_colliding_ints_agrees_with_a_map(dict in int_dict(), start in 0..80_u64, length in 0..80_u64) {
        let mut process = Process::new(layout());
        let types = process.builtin_types();
        let entries: Vec<_> = (0_i128..)
            .zip(&dict.keys)
            .map(|(index, &key)| {
                let value = process.int(types.int, 1000 + index);
                int_entry(&mut process, &types, key, value)
            })
            .collect();
        let deleted: Vec<usize> = (0..dict.keys.len()).filter(|&index| dict.deleted[index]).collect();
        let table = Table {
            kind: Kind::General,
            log2_size: dict.log2_size,
            log2_index_bytes: dict.log2_size + dict.log2_width,
        };
        let object = process.hashed_dict(types.dict, table, &entries, &deleted);

        let missing = |key: i128| {
            Err(InterpretedFailure::Refused(
                ErrorKind::MissingKey,
                format!("the dict holds no key {key}").into(),
            ))
        };
        for (index, (&key, &(_, value, _))) in dict.keys.iter().zip(&entries).enumerate() {
            let expected = if dict.deleted[index] {
                missing(key)
            } else {
                Ok(VirtualAddress::new(value))
            };
            prop_assert_eq!(item_in(&process, types, None, object, &InterpretedKey::Int(key)), expected);
        }
        for &key in &dict.absent {
            prop_assert_eq!(item_in(&process, types, None, object, &InterpretedKey::Int(key)), missing(key));
        }

        let live: Vec<(String, String)> = (0_i128..)
            .zip(&dict.keys)
            .zip(&dict.deleted)
            .filter(|(_, deleted)| !**deleted)
            .map(|((index, key), _)| (key.to_string(), (1000 + index).to_string()))
            .collect();
        let range = start..start + length;
        let children = super::values::children_of(&process, types, object, range.clone())
            .map_err(|problem| TestCaseError::fail(problem.to_string()))?;
        let summary = |slot: &InterpretedSlot| match slot {
            InterpretedSlot::Value(value) => value.summary.to_string(),
            _ => format!("{slot:?}"),
        };
        let read: Vec<(String, String)> = children
            .iter()
            .map(|child| match child {
                InterpretedChild::Entry { key, value } => (summary(key), summary(value)),
                _ => ("not an entry".to_owned(), format!("{child:?}")),
            })
            .collect();
        let expected: Vec<_> = live
            .iter()
            .skip(usize::try_from(range.start).unwrap())
            .take(usize::try_from(length).unwrap())
            .cloned()
            .collect();
        prop_assert_eq!(read, expected);
    }
}
