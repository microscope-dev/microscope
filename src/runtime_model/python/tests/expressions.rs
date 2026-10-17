//! What expressions read of a frame's names, of a `dict`'s keys, and of an
//! object: the reads `Values` makes for them, over the synthetic process the
//! builders in `values` write. A `str` key is found by its hash only with a
//! hash secret the process holds, so the tests that need one give the process
//! a zeroed secret, as CPython's `PYTHONHASHSEED=0` gives.

use super::super::code::{CodeId, CodeTable};
use super::super::expressions::{hash_int, hash_text};
use super::super::objects::Record;
use super::super::values::{Types, Values};
use super::values::{Kind, Table, with_secret, with_values};
use super::{Process, SMALL, layout};
use crate::eval::error::ErrorKind;
use crate::image::python::Field;
use crate::runtime_model::{
    InterpretedFailure, InterpretedKey, InterpretedKind, InterpretedName, InterpretedOperand,
    InterpretedStep,
};
use crate::{
    InspectionLimit, InspectionLimits, TextCompletion, VariableUnavailableReason, VirtualAddress,
};

/// The kinds of local the tests give a code object: a plain local, a
/// cell, and a hidden one.
const LOCAL: u8 = 0x20;
const CELL: u8 = 0x40;
const HIDDEN: u8 = 0x10;

/// CPython's hash of `None`, which it has been a constant since 3.12.
const NONE_HASH: i64 = 0xfca8_6420;

/// The keys of a table of eight slots holding ints 1 and 9, whose hashes
/// collide in the first slot.
const COLLIDING: [i128; 2] = [1, 9];

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

/// A `bytes` of type `types.bytes` holding `data`.
fn bytes_object(process: &mut Process, types: &Types, data: &[u8]) -> u64 {
    let object = process.bytes(data);
    process.word(object, Field::ObjectType, types.bytes.get());
    object
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

/// The operand of the object at `object`, which must have one.
fn operand_of(process: &Process, types: Types, object: u64) -> InterpretedOperand {
    with_values(process, types, InspectionLimits::default(), |values| {
        values
            .operand(VirtualAddress::new(object))
            .expect("the object is readable")
    })
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
        Ok(Some(InterpretedName::Object(VirtualAddress::new(global))))
    );
    assert_eq!(
        name_in(&process, types, secret, frame, "len"),
        Ok(Some(InterpretedName::Object(VirtualAddress::new(
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
        Ok(Some(InterpretedName::Object(VirtualAddress::new(global))))
    );
    assert_eq!(name_in(&process, types, secret, frame, "zz"), Ok(None));
}

#[test]
fn a_variable_is_the_object_its_slot_holds() {
    let mut process = Process::new(layout());
    let types = process.builtin_types();
    let five = process.int(types.int, 5);
    let seven = process.int(types.int, 7);
    let frame = frame_with(
        &mut process,
        &types,
        &["a", "b", "c", "d"],
        &[LOCAL, LOCAL, LOCAL, LOCAL],
        &[five, 0, 0, seven],
        0,
        0,
    );

    assert_eq!(
        variable_in(&process, types, frame, 0),
        Ok(VirtualAddress::new(five))
    );
    assert_eq!(
        variable_in(&process, types, frame, 3),
        Ok(VirtualAddress::new(seven))
    );
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
fn a_list_is_indexed_from_either_end_and_an_index_past_it_is_out_of_bounds() {
    let mut process = Process::new(layout());
    let types = process.builtin_types();
    let items: Vec<u64> = (1..=3).map(|value| process.int(types.int, value)).collect();
    let list = process.list(types.list, &items);

    assert_eq!(
        item_in(&process, types, None, list, &InterpretedKey::Int(0)),
        Ok(VirtualAddress::new(items[0]))
    );
    assert_eq!(
        item_in(&process, types, None, list, &InterpretedKey::Int(-1)),
        Ok(VirtualAddress::new(items[2]))
    );
    for index in [3, -4] {
        assert!(
            matches!(
                item_in(&process, types, None, list, &InterpretedKey::Int(index)),
                Err(InterpretedFailure::Refused(ErrorKind::Bounds, _))
            ),
            "index {index} is out of bounds"
        );
    }
}

#[test]
fn a_tuple_is_indexed_as_a_list_is() {
    let mut process = Process::new(layout());
    let types = process.builtin_types();
    let one = process.int(types.int, 1);
    let two = process.int(types.int, 2);
    let tuple = process.tuple(types.tuple, &[one, two]);

    assert_eq!(
        item_in(&process, types, None, tuple, &InterpretedKey::Int(1)),
        Ok(VirtualAddress::new(two))
    );
    assert_eq!(
        item_in(&process, types, None, tuple, &InterpretedKey::Int(-2)),
        Ok(VirtualAddress::new(one))
    );
}

#[test]
fn a_bool_indexes_a_sequence_as_zero_or_one_and_other_keys_are_refused_as_types() {
    let mut process = Process::new(layout());
    let types = process.builtin_types();
    let zero = process.int(types.int, 10);
    let one = process.int(types.int, 11);
    let list = process.list(types.list, &[zero, one]);

    assert_eq!(
        item_in(&process, types, None, list, &InterpretedKey::Bool(true)),
        Ok(VirtualAddress::new(one))
    );
    assert_eq!(
        item_in(&process, types, None, list, &InterpretedKey::Bool(false)),
        Ok(VirtualAddress::new(zero))
    );
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
fn a_general_table_key_is_found_by_its_hash_past_a_colliding_key() {
    let mut process = Process::new(layout());
    let types = process.builtin_types();
    let first = process.int(types.int, 100);
    let second = process.int(types.int, 200);
    let entries = [
        int_entry(&mut process, &types, COLLIDING[0], first),
        int_entry(&mut process, &types, COLLIDING[1], second),
    ];
    let dict = process.hashed_dict(types.dict, Table::eight(Kind::General), &entries, &[]);

    assert_eq!(
        item_in(&process, types, None, dict, &InterpretedKey::Int(1)),
        Ok(VirtualAddress::new(first))
    );
    assert_eq!(
        item_in(&process, types, None, dict, &InterpretedKey::Int(9)),
        Ok(VirtualAddress::new(second))
    );
}

#[test]
fn a_key_is_found_past_a_deleted_entry_in_its_probe_chain() {
    let mut process = Process::new(layout());
    let types = process.builtin_types();
    let gone = process.int(types.int, 100);
    let kept = process.int(types.int, 200);
    let entries = [
        int_entry(&mut process, &types, COLLIDING[0], gone),
        int_entry(&mut process, &types, COLLIDING[1], kept),
    ];
    let dict = process.hashed_dict(types.dict, Table::eight(Kind::General), &entries, &[0]);

    assert_eq!(
        item_in(&process, types, None, dict, &InterpretedKey::Int(1)),
        Err(InterpretedFailure::Refused(
            ErrorKind::MissingKey,
            "the dict holds no key 1".into()
        ))
    );
    assert_eq!(
        item_in(&process, types, None, dict, &InterpretedKey::Int(9)),
        Ok(VirtualAddress::new(kept))
    );
}

#[test]
fn a_missing_key_ends_its_probe_at_an_empty_slot_and_is_missing() {
    let mut process = Process::new(layout());
    let types = process.builtin_types();
    let first = process.int(types.int, 100);
    let second = process.int(types.int, 200);
    let entries = [
        int_entry(&mut process, &types, COLLIDING[0], first),
        int_entry(&mut process, &types, COLLIDING[1], second),
    ];
    let dict = process.hashed_dict(types.dict, Table::eight(Kind::General), &entries, &[]);

    // 17 probes the same slots as 1 and 9, and reaches an empty slot.
    assert_eq!(
        item_in(&process, types, None, dict, &InterpretedKey::Int(17)),
        Err(InterpretedFailure::Refused(
            ErrorKind::MissingKey,
            "the dict holds no key 17".into()
        ))
    );
}

#[test]
fn a_float_or_a_bool_finds_the_int_key_it_equals() {
    let mut process = Process::new(layout());
    let types = process.builtin_types();
    let item = process.str_object(types.str, "one");
    let entries = [int_entry(&mut process, &types, 1, item)];
    let dict = process.hashed_dict(types.dict, Table::eight(Kind::General), &entries, &[]);

    for key in [InterpretedKey::Float(1.0), InterpretedKey::Bool(true)] {
        assert_eq!(
            item_in(&process, types, None, dict, &key),
            Ok(VirtualAddress::new(item)),
            "the key {key:?}"
        );
    }
}

#[test]
fn none_finds_a_none_key_by_the_hash_cpython_gives_it() {
    let mut process = Process::new(layout());
    let types = process.builtin_types();
    let none = process.object(types.none, SMALL);
    let item = process.int(types.int, 42);
    let entries = [(none, item, NONE_HASH)];
    let dict = process.hashed_dict(types.dict, Table::eight(Kind::General), &entries, &[]);

    assert_eq!(
        item_in(&process, types, None, dict, &InterpretedKey::Nothing),
        Ok(VirtualAddress::new(item))
    );
}

#[test]
fn a_key_of_another_type_whose_hash_is_equal_refuses_the_lookup() {
    let mut process = Process::new(layout());
    let types = process.builtin_types();
    let key_type = process.type_object("Key");
    let key = process.object(key_type, SMALL);
    let item = process.int(types.int, 42);
    let entries = [(key, item, hash_int(9))];
    let dict = process.hashed_dict(types.dict, Table::eight(Kind::General), &entries, &[]);

    match item_in(&process, types, None, dict, &InterpretedKey::Int(9)) {
        Err(InterpretedFailure::Refused(ErrorKind::Unsupported, message)) => {
            assert!(
                message.contains("only its type's `__eq__`") && message.contains("`Key`"),
                "the message: {message}"
            );
        }
        other => panic!("expected the lookup to refuse, got {other:?}"),
    }
}

#[test]
fn a_key_of_another_type_whose_hash_differs_is_passed_over() {
    let mut process = Process::new(layout());
    let types = process.builtin_types();
    let key_type = process.type_object("Key");
    let key = process.object(key_type, SMALL);
    let item = process.int(types.int, 42);
    let wanted = process.int(types.int, 9);
    let wanted_item = process.int(types.int, 43);
    let entries = [(key, item, hash_int(1)), (wanted, wanted_item, hash_int(9))];
    let dict = process.hashed_dict(types.dict, Table::eight(Kind::General), &entries, &[]);

    assert_eq!(
        item_in(&process, types, None, dict, &InterpretedKey::Int(9)),
        Ok(VirtualAddress::new(wanted_item))
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
fn an_operand_has_the_kind_and_type_name_of_its_object() {
    let mut process = Process::new(layout());
    let types = process.builtin_types();
    let int = process.int(types.int, 7);
    let none = process.object(types.none, SMALL);
    let yes = process.int(types.bool, 1);
    let float = process.float(types.float, 1.5);
    let digits = [1, 0, 0, 0, 1024];
    let tag = process.long_tag(0, digits.len());
    let large = process.long(types.int, tag, &digits);
    let other_type = process.type_object("Gate");
    let other = process.object(other_type, SMALL);

    let cases = [
        (int, "int", InterpretedKind::Int(Some(7))),
        (none, "NoneType", InterpretedKind::Nothing),
        (yes, "bool", InterpretedKind::Bool(true)),
        (float, "float", InterpretedKind::Float(1.5)),
        (large, "int", InterpretedKind::Int(None)),
        (other, "Gate", InterpretedKind::Other),
    ];
    for (object, type_name, kind) in cases {
        let operand = operand_of(&process, types, object);
        assert_eq!(operand.type_name.as_ref(), type_name);
        assert_eq!(operand.kind, kind, "the operand of {type_name}");
        assert_eq!(operand.text, None);
    }
}

#[test]
fn a_str_and_bytes_operand_carry_their_whole_text() {
    let mut process = Process::new(layout());
    let types = process.builtin_types();
    let text = process.str_object(types.str, "abc");
    let wide_units: Vec<u16> = "日本語".encode_utf16().collect();
    let wide = process.wide_str(types.str, &wide_units);
    let data = bytes_object(&mut process, &types, &[b'a', 0, 0xff]);

    let operand = operand_of(&process, types, text);
    assert_eq!(operand.kind, InterpretedKind::Text);
    let summary = operand.text.expect("a str has its text");
    assert_eq!(summary.bytes.as_ref(), b"abc");
    assert_eq!(summary.completion, TextCompletion::Complete);

    let operand = operand_of(&process, types, wide);
    let summary = operand.text.expect("a str has its text");
    assert_eq!(summary.bytes.as_ref(), "日本語".as_bytes());
    assert_eq!(summary.completion, TextCompletion::Complete);

    let operand = operand_of(&process, types, data);
    assert_eq!(operand.kind, InterpretedKind::Bytes);
    let summary = operand.text.expect("bytes have their contents");
    assert_eq!(summary.bytes.as_ref(), [b'a', 0, 0xff]);
    assert_eq!(summary.completion, TextCompletion::Complete);
}

#[test]
fn a_sequence_or_map_operand_carries_its_length() {
    let mut process = Process::new(layout());
    let types = process.builtin_types();
    let one = process.int(types.int, 1);
    let two = process.int(types.int, 2);
    let list = process.list(types.list, &[one, two]);
    let tuple = process.tuple(types.tuple, &[one]);
    let entries = [
        int_entry(&mut process, &types, 1, one),
        int_entry(&mut process, &types, 2, two),
    ];
    let dict = process.hashed_dict(types.dict, Table::eight(Kind::General), &entries, &[]);

    assert_eq!(
        operand_of(&process, types, list).kind,
        InterpretedKind::Sequence { length: 2 }
    );
    assert_eq!(
        operand_of(&process, types, tuple).kind,
        InterpretedKind::Sequence { length: 1 }
    );
    assert_eq!(
        operand_of(&process, types, dict).kind,
        InterpretedKind::Map { length: 2 }
    );
}

#[test]
fn the_length_of_a_text_bytes_sequence_or_map_is_its_count_and_an_int_has_none() {
    let mut process = Process::new(layout());
    let types = process.builtin_types();
    let text = process.str_object(types.str, "abc");
    let data = bytes_object(&mut process, &types, &[1, 2]);
    let one = process.int(types.int, 1);
    let list = process.list(types.list, &[one, one, one]);
    let tuple = process.tuple(types.tuple, &[one]);
    let dict = text_dict(
        &mut process,
        &types,
        Table::eight(Kind::Unicode),
        &[("a", one), ("b", one)],
    );
    let int = process.int(types.int, 5);

    assert_eq!(length_of(&process, types, text), Ok(3));
    assert_eq!(length_of(&process, types, data), Ok(2));
    assert_eq!(length_of(&process, types, list), Ok(3));
    assert_eq!(length_of(&process, types, tuple), Ok(1));
    assert_eq!(length_of(&process, types, dict), Ok(2));
    assert!(
        matches!(
            length_of(&process, types, int),
            Err(InterpretedFailure::Refused(ErrorKind::Type, _))
        ),
        "an int has no length"
    );
}
