//! The object model past the built-in values, over the synthetic process the
//! builders in `tests` and `values` write: instances and the attributes they
//! keep in managed dicts, in inline values, behind a dict offset, and in
//! slots; classes and the static types; the sets; the summaries of modules,
//! classes, functions, and code objects; and the attribute lookup
//! `object.__getattribute__` makes.
//!
//! Each class is written with the flags, offsets, and tables CPython gives
//! it, as the layout names them, so the tests read exactly what the builders
//! put in the process.

use super::super::expressions::hash_text;
use super::super::values::Types;
use super::values::{
    Kind, Table, children_of, describe, read_value, value_at, with_runtime, with_values,
};
use super::{FILE, Process, SMALL, layout};
use crate::eval::error::ErrorKind;
use crate::image::python::{Constant, Field, PythonLayout};
use crate::runtime_model::{
    InterpretedChild, InterpretedFailure, InterpretedKey, InterpretedKind, InterpretedSlot,
    InterpretedStep, InterpretedValue,
};
use crate::{InspectionLimits, VirtualAddress};

/// The `tp_flags` bits the classes set, as CPython defines them.
const STATIC_BUILTIN: u64 = 1 << 1;
const INLINE_VALUES: u64 = 1 << 2;
const MANAGED_DICT: u64 = 1 << 4;
const HEAPTYPE: u64 = 1 << 9;

/// The bytes before an object where a managed dict's pointer lies: three
/// words, as CPython keeps them before a managed object.
const MANAGED_PREFIX: u64 = 24;

/// The size of a member descriptor object, which holds its type and its
/// definition.
const DESCRIPTOR_SIZE: u64 = 32;

/// The layout of CPython 3.13, whose instances keep their attributes in
/// inline values after the object, from `tp_basicsize`. The values' header
/// is a capacity, a size, and a valid byte, then the values, then the
/// insertion order.
fn inline_values_layout() -> PythonLayout {
    let mut layout = layout();
    layout.set_field(Field::DictValuesCapacity, 0);
    layout.set_field(Field::DictValuesSize, 1);
    layout.set_field(Field::DictValuesValid, 3);
    layout.set_field(Field::DictValuesValues, 8);
    layout.set_constant(Constant::ManagedDictTagged, 0);
    layout
}

/// The types the object model reads past the built-in values: `object`,
/// which the classes the tests make derive from, and the descriptor types,
/// which say whether their values set and bind. Returns the types and
/// `object`'s address.
fn world(process: &mut Process) -> (Types, u64) {
    let types = process.builtin_types();
    let object = new_type(process, &types, "object");

    // A module and `type` find their attributes in their own dicts, which
    // are the only dicts they have.
    for ty in [types.class, types.module] {
        let mro = process.tuple(types.tuple, &[ty.get()]);
        process.word(ty.get(), Field::TypeMro, mro);
    }

    // A member descriptor sets and binds, a function and a wrapper bind.
    process.word(types.member.get(), Field::TypeDescrSet, 1);
    for ty in [types.member, types.function, types.wrapper] {
        process.word(ty.get(), Field::TypeDescrGet, 1);
    }

    (types, object)
}

/// A type object named `name`, whose type is `type`.
fn new_type(process: &mut Process, types: &Types, name: &str) -> u64 {
    let ty = process.type_object(name).get();
    process.word(ty, Field::ObjectType, types.class.get());
    ty
}

/// What a class the tests build holds, beyond its name and its mro.
#[derive(Default, Clone, Copy)]
struct Spec<'a> {
    /// The class's name, and the module its `__module__` names, if any.
    name: &'a str,
    module: Option<&'a str>,

    /// Flags beyond `Py_TPFLAGS_HEAPTYPE`.
    flags: u64,

    /// The size of its instances, `tp_basicsize`.
    basicsize: u64,

    /// `tp_dictoffset`, which is negative where the dict lies past the
    /// items.
    dictoffset: i64,

    /// `tp_getattro`, or zero for `PyObject_GenericGetAttr`.
    getattro: u64,

    /// The names its instances' attributes are shared under, in order.
    keys: &'a [&'a str],

    /// Its slots, each name with the offset of its member, sorted by name.
    slots: &'a [(&'a str, u64)],

    /// Its own dict's entries, beyond the module and the slots.
    attributes: &'a [(&'a str, u64)],
}

/// A heap class of `spec`, whose method resolution order is the class
/// followed by `mro`, whose first type is its base. Returns its address.
fn class(process: &mut Process, types: &Types, spec: &Spec<'_>, mro: &[u64]) -> u64 {
    let ty = new_type(process, types, spec.name);
    process.word(ty, Field::TypeFlags, HEAPTYPE | spec.flags);
    process.word(ty, Field::TypeBasicsize, spec.basicsize);
    process.word(ty, Field::TypeDictoffset, spec.dictoffset.cast_unsigned());
    process.word(ty, Field::TypeBase, mro[0]);

    let chain: Vec<u64> = std::iter::once(ty).chain(mro.iter().copied()).collect();
    let chain = process.tuple(types.tuple, &chain);
    process.word(ty, Field::TypeMro, chain);

    let getattro = if spec.getattro == 0 {
        types.generic_getattr.get()
    } else {
        spec.getattro
    };
    process.word(ty, Field::TypeGetattro, getattro);

    let qualname = process.str_object(types.str, spec.name);
    process.word(ty, Field::HeapTypeQualname, qualname);
    let keys = shared_keys(process, types, spec.keys);
    process.word(ty, Field::HeapTypeCachedKeys, keys);

    let names: Vec<u64> = spec
        .slots
        .iter()
        .map(|(name, _)| process.str_object(types.str, name))
        .collect();
    let slots = if names.is_empty() {
        0
    } else {
        process.tuple(types.tuple, &names)
    };
    process.word(ty, Field::HeapTypeSlots, slots);

    let mut entries: Vec<(&str, u64)> = spec.attributes.to_vec();
    if let Some(module) = spec.module {
        let module = process.str_object(types.str, module);
        entries.push(("__module__", module));
    }
    let member_kind = u64::from(process.layout.constant(Constant::MemberObjectEx));
    for &(name, offset) in spec.slots {
        let definition = process.alloc(24);
        process.put(definition, Field::MemberDefType, member_kind, 4);
        process.word(definition, Field::MemberDefOffset, offset);
        let descriptor = process.object(types.member, DESCRIPTOR_SIZE);
        process.word(descriptor, Field::MemberDescrMember, definition);
        entries.push((name, descriptor));
    }
    let dict = text_dict(process, types, &entries);
    process.word(ty, Field::TypeDict, dict);

    ty
}

/// The keys a class's instances share, of the names `names` in order: a
/// split table, whose entries name them and hold no values.
fn shared_keys(process: &mut Process, types: &Types, names: &[&str]) -> u64 {
    let size = process.offset(Field::DictUnicodeEntrySize);
    let indices = process.offset(Field::DictKeysIndices);
    let keys = process.alloc(indices + 8 + size * names.len() as u64);
    process.put(keys, Field::DictKeysLog2Size, 3, 1);
    process.put(keys, Field::DictKeysLog2IndexBytes, 3, 1);
    let split = u64::from(process.layout.constant(Constant::DictKeysSplit));
    process.put(keys, Field::DictKeysKind, split, 1);
    process.word(keys, Field::DictKeysNentries, names.len() as u64);

    let first = keys + indices + 8;
    for (index, name) in names.iter().enumerate() {
        let entry = first + size * index as u64;
        let key = process.str_object(types.str, name);
        process.word(entry, Field::DictUnicodeEntryKey, key);
    }

    keys
}

/// A dict of `str` keys holding `entries`, whose keys hash as the process's
/// strings do under the zeroed secret.
fn text_dict(process: &mut Process, types: &Types, entries: &[(&str, u64)]) -> u64 {
    let mut hashed = Vec::with_capacity(entries.len());
    for &(text, value) in entries {
        let key = process.str_object(types.str, text);
        let points: Vec<u32> = text.chars().map(u32::from).collect();
        hashed.push((key, value, hash_text((0, 0), &points)));
    }

    process.hashed_dict(types.dict, Table::eight(Kind::Unicode), &hashed, &[])
}

/// An instance of `ty` of `size` bytes, after the words its managed dict
/// pointer lies in, which are zeroed. Returns the instance's address.
fn managed_instance(process: &mut Process, ty: u64, size: u64) -> u64 {
    let base = process.alloc(MANAGED_PREFIX + size);
    let object = base + MANAGED_PREFIX;
    process.word(object, Field::ObjectType, ty);
    object
}

/// Points the managed dict word of the instance at `object` at `word`.
fn set_managed(process: &mut Process, object: u64, word: u64) {
    process.memory.write(object - MANAGED_PREFIX, word, 8);
}

/// The values of a 3.12 instance, as the tagged pointer its managed dict
/// word holds: the address one byte before them, as
/// `_PyDictOrValues_SetValues` writes it. `values` are each key's index and
/// value, in insertion order.
fn tagged_values(process: &mut Process, values: &[(u64, u64)]) -> u64 {
    let base = process.alloc(96);
    let array = base + 32;
    process.memory.write(array - 2, values.len() as u64, 1);
    for (position, &(index, value)) in values.iter().enumerate() {
        process.memory.write(array - 3 - position as u64, index, 1);
        process.memory.write(array + 8 * index, value, 8);
    }

    array - 1
}

/// Writes the inline values at `values`, of 3.13. `slots` holds each key's
/// value, zero where it has none, and `order` the key indices of the values
/// that are live, in insertion order.
fn inline_values(process: &mut Process, values: u64, slots: &[u64], order: &[u64]) {
    let capacity = process.offset(Field::DictValuesCapacity);
    let size = process.offset(Field::DictValuesSize);
    let valid = process.offset(Field::DictValuesValid);
    let array = values + process.offset(Field::DictValuesValues);
    process
        .memory
        .write(values + capacity, slots.len() as u64, 1);
    process.memory.write(values + size, order.len() as u64, 1);
    process.memory.write(values + valid, 1, 1);

    for (index, value) in slots.iter().enumerate() {
        process.memory.write(array + 8 * index as u64, *value, 8);
    }

    let end = array + 8 * slots.len() as u64;
    for (position, index) in order.iter().enumerate() {
        process.memory.write(end + position as u64, *index, 1);
    }
}

/// An instance of `ty` whose `basicsize` bytes are followed by inline values
/// holding `slots` and `order`, as 3.13 lays it out. Returns its address.
fn inline_instance(
    process: &mut Process,
    ty: u64,
    basicsize: u64,
    slots: &[u64],
    order: &[u64],
) -> u64 {
    let header = process.offset(Field::DictValuesValues) + 8 * slots.len() as u64;
    let size = basicsize + header + order.len() as u64;
    let object = managed_instance(process, ty, size);
    inline_values(process, object + basicsize, slots, order);
    object
}

/// A list of `ty`, which is an instance of a class deriving from `list`,
/// whose items are `items`.
fn list_instance(process: &mut Process, ty: u64, items: &[u64]) -> u64 {
    let size = process.offset(Field::ListItems) + 8;
    let object = managed_instance(process, ty, size);
    let array = process.alloc(8 * items.len() as u64);
    process.words(array, items);
    process.word(object, Field::ListSize, items.len() as u64);
    process.word(object, Field::ListItems, array);
    object
}

/// A `list` type whose dict holds the special methods `__getitem__` and
/// `__len__`, which a class deriving from `list` finds in `list`.
fn list_with_items(process: &mut Process, types: &Types) {
    let marker = process.int(types.int, 0);
    let dict = text_dict(
        process,
        types,
        &[("__getitem__", marker), ("__len__", marker)],
    );
    process.word(types.list.get(), Field::TypeDict, dict);
}

/// A module named `name`, whose dict holds its `__name__`.
fn module_object(process: &mut Process, types: &Types, name: &str) -> u64 {
    let value = process.str_object(types.str, name);
    let dict = text_dict(process, types, &[("__name__", value)]);
    let module = process.object(types.module, 32);
    process.word(module, Field::ModuleDict, dict);
    module
}

/// A function whose qualified name is `qualname`.
fn function_object(process: &mut Process, types: &Types, qualname: &str) -> u64 {
    let name = process.str_object(types.str, qualname);
    let function = process.object(types.function, 32);
    process.word(function, Field::FunctionQualname, name);
    function
}

/// A built-in function or method named `name`, bound to `bound`, which is
/// zero for none.
fn builtin_object(process: &mut Process, types: &Types, name: &str, bound: u64) -> u64 {
    let text = process.c_string(name);
    let method = process.alloc(16);
    process.word(method, Field::MethodDefName, text);
    let object = process.object(types.builtin, 40);
    process.word(object, Field::BuiltinMethod, method);
    process.word(object, Field::BuiltinSelf, bound);
    object
}

/// A code object of `name`, in [`FILE`], whose first line is `first_line`,
/// with the named and typed fields `repr` reads.
fn code_object(process: &mut Process, types: &Types, name: &str, first_line: u32) -> u64 {
    let code = process.code(name, first_line, &[(1, 0)]);
    process.word(code, Field::ObjectType, types.code.get());
    let qualname = process.str_object(types.str, name);
    process.word(code, Field::CodeName, qualname);
    let file = process.str_object(types.str, FILE);
    process.word(code, Field::CodeFilename, file);
    code
}

/// A slot of a set's table.
pub(super) enum Slot {
    /// Holds no key.
    Empty,

    /// Holds the item `key`.
    Item(u64),

    /// Held an item that was removed, which is a dummy with the hash -1.
    Deleted,
}

/// A set or frozenset of type `ty`, whose table holds `slots` in order.
pub(super) fn set_object(process: &mut Process, ty: VirtualAddress, slots: &[Slot]) -> u64 {
    let object = process.object(ty, 48);
    let stride = process.offset(Field::SetEntrySize);
    let table = process.alloc(stride * slots.len() as u64);
    let dummy = process.alloc(16);
    let mut used = 0;
    for (index, slot) in slots.iter().enumerate() {
        let entry = table + stride * index as u64;
        match *slot {
            Slot::Empty => {}
            Slot::Item(key) => {
                used += 1;
                process.word(entry, Field::SetEntryKey, key);
                process.word(entry, Field::SetEntryHash, 1);
            }
            Slot::Deleted => {
                process.word(entry, Field::SetEntryKey, dummy);
                process.word(entry, Field::SetEntryHash, u64::MAX);
            }
        }
    }

    process.word(object, Field::SetUsed, used);
    process.word(object, Field::SetMask, slots.len() as u64 - 1);
    process.word(object, Field::SetTable, table);
    object
}

/// The attribute `name` of the object at `object`, read with the default
/// limits.
fn attribute_in(
    process: &Process,
    types: Types,
    object: u64,
    name: &str,
) -> Result<VirtualAddress, InterpretedFailure> {
    with_values(process, types, InspectionLimits::default(), |values| {
        values.step(
            VirtualAddress::new(object),
            InterpretedStep::Attribute(name),
        )
    })
}

/// The item of the object at `object` that the integer `index` finds, read
/// with the default limits.
fn item_at(
    process: &Process,
    types: Types,
    object: u64,
    index: i128,
) -> Result<VirtualAddress, InterpretedFailure> {
    let key = InterpretedKey::Int(index);
    with_values(process, types, InspectionLimits::default(), |values| {
        values.step(VirtualAddress::new(object), InterpretedStep::Item(&key))
    })
}

/// The name and summary of each of `children`, which must all be attributes.
fn attributes_of(children: &[InterpretedChild]) -> Vec<(String, String)> {
    children
        .iter()
        .map(|child| match child {
            InterpretedChild::Attribute { name, value } => (name.to_string(), describe(value)),
            InterpretedChild::Element(_) | InterpretedChild::Entry { .. } => {
                panic!("the children are attributes")
            }
        })
        .collect()
}

/// Each of `children` as one line: an element's summary, an entry's key and
/// value, or an attribute's name and value.
fn show_children(children: &[InterpretedChild]) -> Vec<String> {
    children
        .iter()
        .map(|child| match child {
            InterpretedChild::Element(value) => describe(value),
            InterpretedChild::Entry { key, value } => {
                format!("{}: {}", describe(key), describe(value))
            }
            InterpretedChild::Attribute { name, value } => format!("{name}={}", describe(value)),
        })
        .collect()
}

/// The value at `object`, read with the default limits, which must be a
/// value.
fn presented_at(process: &Process, types: Types, object: u64) -> InterpretedValue {
    read_value(process, types, object)
}

#[test]
fn an_instance_with_tagged_values_shows_its_attributes_by_name_in_insertion_order() {
    let mut process = Process::new(layout());
    let (types, object) = world(&mut process);
    let point = class(
        &mut process,
        &types,
        &Spec {
            name: "Point",
            module: Some("geom"),
            flags: MANAGED_DICT,
            basicsize: 16,
            keys: &["x", "y", "label"],
            ..Spec::default()
        },
        &[object],
    );
    let x = process.int(types.int, 3);
    let y = process.int(types.int, 5);

    // Key 1 is set first, so `y` is the first attribute.
    let values = tagged_values(&mut process, &[(1, y), (0, x)]);
    let instance = managed_instance(&mut process, point, 16);
    set_managed(&mut process, instance, values);

    let shown = presented_at(&process, types, instance);
    assert_eq!(shown.module.as_deref(), Some("geom"));
    assert_eq!(shown.type_name.as_ref(), "Point");
    assert_eq!(shown.kind, InterpretedKind::Instance);
    assert_eq!(shown.attributes, 2);
    assert_eq!(
        shown.summary.as_ref(),
        format!("<geom.Point object at {instance:#x}>")
    );
    assert_eq!(shown.problem, None);

    let children = children_of(&process, types, instance, 0..2).expect("the attributes are read");
    assert_eq!(
        attributes_of(&children),
        vec![
            ("y".to_owned(), "5".to_owned()),
            ("x".to_owned(), "3".to_owned())
        ]
    );
}

#[test]
fn an_instance_whose_managed_word_is_a_dict_shows_the_dict_entries() {
    let mut process = Process::new(layout());
    let (types, object) = world(&mut process);
    let point = class(
        &mut process,
        &types,
        &Spec {
            name: "Point",
            module: Some("geom"),
            flags: MANAGED_DICT,
            basicsize: 16,
            ..Spec::default()
        },
        &[object],
    );
    let x = process.int(types.int, 3);
    let y = process.int(types.int, 5);
    let dict = text_dict(&mut process, &types, &[("x", x), ("y", y)]);
    let instance = managed_instance(&mut process, point, 16);
    set_managed(&mut process, instance, dict);

    let shown = presented_at(&process, types, instance);
    assert_eq!(shown.attributes, 2);
    assert_eq!(shown.problem, None);
    let children = children_of(&process, types, instance, 0..2).expect("the attributes are read");
    assert_eq!(
        attributes_of(&children),
        vec![
            ("x".to_owned(), "3".to_owned()),
            ("y".to_owned(), "5".to_owned())
        ]
    );
}

#[test]
fn an_instance_with_inline_values_shows_the_attributes_still_held_in_insertion_order() {
    let mut process = Process::new(inline_values_layout());
    let (types, object) = world(&mut process);
    let packed = class(
        &mut process,
        &types,
        &Spec {
            name: "Packed",
            module: Some("geom"),
            flags: MANAGED_DICT | INLINE_VALUES,
            basicsize: 32,
            keys: &["a", "b", "c"],
            ..Spec::default()
        },
        &[object],
    );
    let one = process.int(types.int, 1);
    let three = process.int(types.int, 3);

    // `b` has no value: its slot is NULL, and it is not in the insertion
    // order, as a deleted attribute is not.
    let instance = inline_instance(&mut process, packed, 32, &[one, 0, three], &[0, 2]);

    let shown = presented_at(&process, types, instance);
    assert_eq!(shown.attributes, 2);
    assert_eq!(shown.problem, None);
    let children = children_of(&process, types, instance, 0..2).expect("the attributes are read");
    assert_eq!(
        attributes_of(&children),
        vec![
            ("a".to_owned(), "1".to_owned()),
            ("c".to_owned(), "3".to_owned())
        ]
    );
}

#[test]
fn an_instance_whose_managed_dict_is_set_shows_the_dict_instead_of_its_inline_values() {
    let mut process = Process::new(inline_values_layout());
    let (types, object) = world(&mut process);
    let packed = class(
        &mut process,
        &types,
        &Spec {
            name: "Packed",
            module: Some("geom"),
            flags: MANAGED_DICT | INLINE_VALUES,
            basicsize: 32,
            keys: &["a", "b", "c"],
            ..Spec::default()
        },
        &[object],
    );
    let one = process.int(types.int, 1);
    let three = process.int(types.int, 3);
    let instance = inline_instance(&mut process, packed, 32, &[one, 0, three], &[0, 2]);

    let nine = process.int(types.int, 9);
    let dict = text_dict(&mut process, &types, &[("z", nine)]);
    set_managed(&mut process, instance, dict);

    let shown = presented_at(&process, types, instance);
    assert_eq!(shown.attributes, 1);
    let children = children_of(&process, types, instance, 0..1).expect("the attributes are read");
    assert_eq!(
        attributes_of(&children),
        vec![("z".to_owned(), "9".to_owned())]
    );
}

#[test]
fn an_instance_with_a_positive_dictoffset_shows_the_dict_at_that_offset() {
    let mut process = Process::new(layout());
    let (types, object) = world(&mut process);
    let holder = class(
        &mut process,
        &types,
        &Spec {
            name: "Holder",
            module: Some("geom"),
            basicsize: 24,
            dictoffset: 16,
            ..Spec::default()
        },
        &[object],
    );
    let seven = process.int(types.int, 7);
    let dict = text_dict(&mut process, &types, &[("k", seven)]);
    let instance = process.object(VirtualAddress::new(holder), 24);
    process.memory.write(instance + 16, dict, 8);

    let shown = presented_at(&process, types, instance);
    assert_eq!(shown.attributes, 1);
    assert_eq!(shown.problem, None);
    let children = children_of(&process, types, instance, 0..1).expect("the attributes are read");
    assert_eq!(
        attributes_of(&children),
        vec![("k".to_owned(), "7".to_owned())]
    );
}

#[test]
fn an_instance_whose_dict_lies_past_its_items_is_unreadable_by_its_class() {
    let mut process = Process::new(layout());
    let (types, object) = world(&mut process);
    let gate = class(
        &mut process,
        &types,
        &Spec {
            name: "Gate",
            module: Some("app"),
            basicsize: 24,
            dictoffset: -8,
            ..Spec::default()
        },
        &[object],
    );
    let instance = process.object(VirtualAddress::new(gate), 24);

    match value_at(&process, types, instance) {
        InterpretedSlot::Unreadable(problem) => assert!(
            problem.contains("`app.Gate` keeps its dict past its items"),
            "the problem: {problem}"
        ),
        other => panic!(
            "expected the instance to be unreadable, got {}",
            describe(&other)
        ),
    }
}

#[test]
fn slots_are_shown_after_the_dict_attributes_in_sorted_order_and_unset_slots_are_left_out() {
    let mut process = Process::new(layout());
    let (types, object) = world(&mut process);
    let slotted = class(
        &mut process,
        &types,
        &Spec {
            name: "Slotted",
            module: Some("geom"),
            flags: MANAGED_DICT,
            basicsize: 40,
            keys: &["extra"],
            slots: &[("a", 16), ("b", 24), ("c", 32)],
            ..Spec::default()
        },
        &[object],
    );
    let nine = process.int(types.int, 9);
    let one = process.int(types.int, 1);
    let three = process.int(types.int, 3);
    let values = tagged_values(&mut process, &[(0, nine)]);
    let instance = managed_instance(&mut process, slotted, 40);
    set_managed(&mut process, instance, values);
    process.memory.write(instance + 16, one, 8);
    // `b` is unset: its slot is NULL.
    process.memory.write(instance + 32, three, 8);

    let shown = presented_at(&process, types, instance);
    assert_eq!(shown.attributes, 3);
    let children = children_of(&process, types, instance, 0..3).expect("the attributes are read");
    assert_eq!(
        attributes_of(&children),
        vec![
            ("extra".to_owned(), "9".to_owned()),
            ("a".to_owned(), "1".to_owned()),
            ("c".to_owned(), "3".to_owned())
        ]
    );
}

#[test]
fn a_derived_class_slot_hides_a_base_slot_of_the_same_name_even_when_it_is_unset() {
    let mut process = Process::new(layout());
    let (types, object) = world(&mut process);
    let base = class(
        &mut process,
        &types,
        &Spec {
            name: "Base",
            basicsize: 24,
            slots: &[("a", 16)],
            ..Spec::default()
        },
        &[object],
    );
    let derived = class(
        &mut process,
        &types,
        &Spec {
            name: "Derived",
            basicsize: 32,
            slots: &[("a", 24)],
            ..Spec::default()
        },
        &[base, object],
    );
    let instance = process.object(VirtualAddress::new(derived), 32);
    // The base's slot holds a value, which the derived slot hides.
    process.memory.write(instance + 16, 5, 8);

    let shown = presented_at(&process, types, instance);
    assert_eq!(shown.attributes, 0);
    assert_eq!(shown.problem, None);
}

#[test]
fn an_attribute_is_read_from_the_instance_or_else_from_the_class_it_derives_from() {
    let mut process = Process::new(layout());
    let (types, object) = world(&mut process);
    let seven = process.int(types.int, 7);
    let base = class(
        &mut process,
        &types,
        &Spec {
            name: "Base",
            module: Some("app"),
            attributes: &[("kind", seven)],
            ..Spec::default()
        },
        &[object],
    );
    let plain = class(
        &mut process,
        &types,
        &Spec {
            name: "Plain",
            module: Some("app"),
            flags: MANAGED_DICT,
            basicsize: 16,
            keys: &["label", "kind"],
            ..Spec::default()
        },
        &[base, object],
    );
    let text = process.str_object(types.str, "hi");
    let eight = process.int(types.int, 8);

    // The first instance has no `kind`, so its class's is found.
    let without = managed_instance(&mut process, plain, 16);
    let values = tagged_values(&mut process, &[(0, text)]);
    set_managed(&mut process, without, values);
    // The second's own `kind` shadows the class's, which is not a descriptor.
    let with = managed_instance(&mut process, plain, 16);
    let values = tagged_values(&mut process, &[(0, text), (1, eight)]);
    set_managed(&mut process, with, values);

    assert_eq!(
        attribute_in(&process, types, without, "label"),
        Ok(VirtualAddress::new(text))
    );
    assert_eq!(
        attribute_in(&process, types, without, "kind"),
        Ok(VirtualAddress::new(seven))
    );
    assert_eq!(
        attribute_in(&process, types, with, "kind"),
        Ok(VirtualAddress::new(eight))
    );
}

#[test]
fn a_missing_attribute_is_a_missing_key() {
    let mut process = Process::new(layout());
    let (types, object) = world(&mut process);
    let plain = class(
        &mut process,
        &types,
        &Spec {
            name: "Plain",
            module: Some("app"),
            flags: MANAGED_DICT,
            basicsize: 16,
            keys: &["label"],
            ..Spec::default()
        },
        &[object],
    );
    let text = process.str_object(types.str, "hi");
    let instance = managed_instance(&mut process, plain, 16);
    let values = tagged_values(&mut process, &[(0, text)]);
    set_managed(&mut process, instance, values);

    match attribute_in(&process, types, instance, "nope") {
        Err(InterpretedFailure::Refused(ErrorKind::MissingKey, message)) => {
            assert_eq!(message.as_ref(), "the `app.Plain` has no attribute `nope`");
        }
        other => panic!("expected a missing key, got {other:?}"),
    }
}

#[test]
fn a_slot_is_read_through_its_member_descriptor_and_an_unset_slot_is_missing() {
    let mut process = Process::new(layout());
    let (types, object) = world(&mut process);
    let slotted = class(
        &mut process,
        &types,
        &Spec {
            name: "Slotted",
            basicsize: 32,
            slots: &[("a", 16), ("b", 24)],
            ..Spec::default()
        },
        &[object],
    );
    let one = process.int(types.int, 1);
    let instance = process.object(VirtualAddress::new(slotted), 32);
    process.memory.write(instance + 16, one, 8);

    assert_eq!(
        attribute_in(&process, types, instance, "a"),
        Ok(VirtualAddress::new(one))
    );
    match attribute_in(&process, types, instance, "b") {
        Err(InterpretedFailure::Refused(ErrorKind::MissingKey, message)) => {
            assert!(message.contains("holds no value"), "the message: {message}");
        }
        other => panic!("expected a missing key, got {other:?}"),
    }
}

#[test]
fn a_data_descriptor_that_is_not_a_member_is_refused() {
    let mut process = Process::new(layout());
    let (types, object) = world(&mut process);
    let property = new_type(&mut process, &types, "property");
    process.word(property, Field::TypeDescrSet, 1);
    let descriptor = process.object(VirtualAddress::new(property), 32);
    let gated = class(
        &mut process,
        &types,
        &Spec {
            name: "Gated",
            basicsize: 16,
            attributes: &[("prop", descriptor)],
            ..Spec::default()
        },
        &[object],
    );
    let instance = process.object(VirtualAddress::new(gated), 16);

    match attribute_in(&process, types, instance, "prop") {
        Err(InterpretedFailure::Refused(ErrorKind::Unsupported, message)) => {
            assert!(
                message.contains("`prop` of a `Gated` is a `property`"),
                "the message: {message}"
            );
        }
        other => panic!("expected a refusal, got {other:?}"),
    }
}

#[test]
fn a_method_that_binds_to_its_instance_is_refused() {
    let mut process = Process::new(layout());
    let (types, object) = world(&mut process);
    let function = process.object(types.function, 32);
    let bound = class(
        &mut process,
        &types,
        &Spec {
            name: "Bound",
            basicsize: 16,
            attributes: &[("method", function)],
            ..Spec::default()
        },
        &[object],
    );
    let instance = process.object(VirtualAddress::new(bound), 16);

    match attribute_in(&process, types, instance, "method") {
        Err(InterpretedFailure::Refused(ErrorKind::Unsupported, message)) => {
            assert!(message.contains("binds"), "the message: {message}");
        }
        other => panic!("expected a refusal, got {other:?}"),
    }
}

#[test]
fn a_class_with_a_getattr_hook_reads_its_dict_but_refuses_a_miss() {
    let mut process = Process::new(layout());
    let (types, object) = world(&mut process);

    // A wrapper of `PyObject_GenericGetAttr` as `__getattribute__`, which
    // makes the hook a `__getattr__` over the generic lookup.
    let wrapper = process.object(types.wrapper, 32);
    process.word(
        wrapper,
        Field::WrapperDescrWrapped,
        types.generic_getattr.get(),
    );
    let marker = process.int(types.int, 0);
    let hook = VirtualAddress::new(process.alloc(16));
    let hooked = class(
        &mut process,
        &types,
        &Spec {
            name: "Hooked",
            flags: MANAGED_DICT,
            basicsize: 16,
            getattro: hook.get(),
            keys: &["present"],
            attributes: &[("__getattribute__", wrapper), ("__getattr__", marker)],
            ..Spec::default()
        },
        &[object],
    );
    let four = process.int(types.int, 4);
    let instance = managed_instance(&mut process, hooked, 16);
    let values = tagged_values(&mut process, &[(0, four)]);
    set_managed(&mut process, instance, values);

    assert_eq!(
        attribute_in(&process, types, instance, "present"),
        Ok(VirtualAddress::new(four))
    );
    match attribute_in(&process, types, instance, "nope") {
        Err(InterpretedFailure::Refused(ErrorKind::Unsupported, message)) => {
            assert!(message.contains("`__getattr__`"), "the message: {message}");
        }
        other => panic!("expected a refusal, got {other:?}"),
    }
}

#[test]
fn a_class_whose_getattro_is_neither_generic_nor_a_hook_is_refused() {
    let mut process = Process::new(layout());
    let (types, object) = world(&mut process);
    let other = VirtualAddress::new(process.alloc(16));
    let odd = class(
        &mut process,
        &types,
        &Spec {
            name: "Odd",
            basicsize: 16,
            getattro: other.get(),
            ..Spec::default()
        },
        &[object],
    );
    let instance = process.object(VirtualAddress::new(odd), 16);

    match attribute_in(&process, types, instance, "anything") {
        Err(InterpretedFailure::Refused(ErrorKind::Unsupported, message)) => {
            assert!(
                message.contains("its own `__getattribute__`"),
                "the message: {message}"
            );
        }
        other => panic!("expected a refusal, got {other:?}"),
    }
}

#[test]
fn a_static_builtin_types_class_attribute_is_found_in_the_dict_its_interpreter_keeps() {
    for extension in [false, true] {
        a_managed_static_types_class_attribute_is_found(extension);
    }
}

/// A static type's class attribute, which its interpreter keeps in the
/// state at its index among the built-in types', or for a type of an
/// extension the interpreter manages, as `_datetime`'s, among theirs, where
/// the built-in types' state at that index is another type's.
fn a_managed_static_types_class_attribute_is_found(extension: bool) {
    let mut process = Process::new(layout());
    let (types, _) = world(&mut process);
    let answer = process.int(types.int, 42);
    let dict = text_dict(&mut process, &types, &[("answer", answer)]);
    let flagged = new_type(&mut process, &types, "Flagged");
    process.word(flagged, Field::TypeFlags, STATIC_BUILTIN);
    process.word(flagged, Field::TypeSubclasses, 1);
    let mro = process.tuple(types.tuple, &[flagged]);
    process.word(flagged, Field::TypeMro, mro);

    // The interpreter keeps the state of the first static type after its
    // header.
    let interpreter = process.alloc(128);
    let builtin = interpreter + process.offset(Field::InterpreterStaticTypes);
    let state = if extension {
        process.memory.write(builtin, types.int.get(), 8);
        interpreter + process.offset(Field::InterpreterStaticExtensionTypes)
    } else {
        builtin
    };
    process.memory.write(state, flagged, 8);
    process
        .memory
        .write(state + process.offset(Field::StaticTypeStateDict), dict, 8);
    let runtime = process.alloc(SMALL);
    process.word(runtime, Field::RuntimeInterpretersHead, interpreter);

    let found = with_runtime(
        &process,
        types,
        InspectionLimits::default(),
        None,
        VirtualAddress::new(runtime),
        |values| {
            values.step(
                VirtualAddress::new(flagged),
                InterpretedStep::Attribute("answer"),
            )
        },
    );
    assert_eq!(
        found,
        Ok(VirtualAddress::new(answer)),
        "extension: {extension}"
    );
}

/// A static type whose state, in either place its interpreter keeps one,
/// is another type's has no class attributes to be read from it.
#[test]
fn a_static_type_whose_state_is_another_types_is_malformed() {
    let mut process = Process::new(layout());
    let (types, _) = world(&mut process);
    let answer = process.int(types.int, 42);
    let dict = text_dict(&mut process, &types, &[("answer", answer)]);
    let flagged = new_type(&mut process, &types, "Flagged");
    process.word(flagged, Field::TypeFlags, STATIC_BUILTIN);
    process.word(flagged, Field::TypeSubclasses, 1);
    let mro = process.tuple(types.tuple, &[flagged]);
    process.word(flagged, Field::TypeMro, mro);

    let interpreter = process.alloc(128);
    for field in [
        Field::InterpreterStaticTypes,
        Field::InterpreterStaticExtensionTypes,
    ] {
        let state = interpreter + process.offset(field);
        process.memory.write(state, types.int.get(), 8);
        process
            .memory
            .write(state + process.offset(Field::StaticTypeStateDict), dict, 8);
    }
    let runtime = process.alloc(SMALL);
    process.word(runtime, Field::RuntimeInterpretersHead, interpreter);

    let found = with_runtime(
        &process,
        types,
        InspectionLimits::default(),
        None,
        VirtualAddress::new(runtime),
        |values| {
            values.step(
                VirtualAddress::new(flagged),
                InterpretedStep::Attribute("answer"),
            )
        },
    );
    assert!(
        matches!(&found, Err(InterpretedFailure::Unavailable(reason)) if format!("{reason:?}").contains("malformed")),
        "{found:?}"
    );
}

#[test]
fn a_set_is_shown_with_its_items_in_table_order_past_its_deleted_entries() {
    let mut process = Process::new(layout());
    let (types, _) = world(&mut process);
    let two = process.int(types.int, 2);
    let one = process.int(types.int, 1);
    let set = set_object(
        &mut process,
        types.set,
        &[
            Slot::Item(two),
            Slot::Deleted,
            Slot::Item(one),
            Slot::Empty,
            Slot::Empty,
            Slot::Empty,
            Slot::Empty,
            Slot::Empty,
        ],
    );

    let shown = presented_at(&process, types, set);
    assert_eq!(shown.summary.as_ref(), "{2, 1}");
    assert_eq!(shown.kind, InterpretedKind::Sequence { length: 2 });
    assert_eq!(shown.type_name.as_ref(), "set");
    assert_eq!(shown.module.as_deref(), Some("builtins"));
    assert_eq!(shown.problem, None);

    let children = children_of(&process, types, set, 0..2).expect("the items are read");
    assert_eq!(show_children(&children), vec!["2", "1"]);
    let length = with_values(&process, types, InspectionLimits::default(), |values| {
        values.length(VirtualAddress::new(set))
    });
    assert_eq!(length, Ok(2));
}

#[test]
fn an_empty_set_and_an_empty_frozenset_are_shown_by_their_constructors() {
    let mut process = Process::new(layout());
    let (types, _) = world(&mut process);
    let empty = set_object(&mut process, types.set, &[Slot::Deleted, Slot::Empty]);
    let frozen = set_object(&mut process, types.frozenset, &[Slot::Empty, Slot::Empty]);

    assert_eq!(
        presented_at(&process, types, empty).summary.as_ref(),
        "set()"
    );
    assert_eq!(
        presented_at(&process, types, frozen).summary.as_ref(),
        "frozenset()"
    );
}

#[test]
fn a_frozenset_of_one_item_is_shown_in_braces_with_that_item_as_its_child() {
    let mut process = Process::new(layout());
    let (types, _) = world(&mut process);
    let four = process.int(types.int, 4);
    let frozen = set_object(
        &mut process,
        types.frozenset,
        &[Slot::Empty, Slot::Item(four)],
    );

    let shown = presented_at(&process, types, frozen);
    assert_eq!(shown.summary.as_ref(), "frozenset({4})");
    assert_eq!(shown.type_name.as_ref(), "frozenset");
    let children = children_of(&process, types, frozen, 0..1).expect("the item is read");
    assert_eq!(show_children(&children), vec!["4"]);
}

#[test]
fn a_list_subclass_is_shown_as_its_list_with_its_attributes_after_its_elements() {
    let mut process = Process::new(layout());
    let (types, object) = world(&mut process);
    list_with_items(&mut process, &types);
    let stack = class(
        &mut process,
        &types,
        &Spec {
            name: "Stack",
            module: Some("app"),
            flags: MANAGED_DICT,
            basicsize: 32,
            keys: &["label"],
            ..Spec::default()
        },
        &[types.list.get(), object],
    );
    let ten = process.int(types.int, 10);
    let twenty = process.int(types.int, 20);
    let label = process.str_object(types.str, "x");
    let instance = list_instance(&mut process, stack, &[ten, twenty]);
    let values = tagged_values(&mut process, &[(0, label)]);
    set_managed(&mut process, instance, values);

    let shown = presented_at(&process, types, instance);
    assert_eq!(shown.summary.as_ref(), "[10, 20]");
    // The subclass is presented by its list's kind.
    assert_eq!(shown.kind, InterpretedKind::Sequence { length: 2 });
    assert_eq!(shown.module.as_deref(), Some("app"));
    assert_eq!(shown.type_name.as_ref(), "Stack");
    assert_eq!(shown.attributes, 1);
    assert_eq!(shown.problem, None);

    let children = children_of(&process, types, instance, 0..3).expect("the children are read");
    assert_eq!(show_children(&children), vec!["10", "20", "label='x'"]);
}

#[test]
fn a_list_subclass_indexes_by_the_lists_getitem_and_refuses_its_own() {
    let mut process = Process::new(layout());
    let (types, object) = world(&mut process);
    list_with_items(&mut process, &types);
    let marker = process.int(types.int, 0);
    let spec = Spec {
        module: Some("app"),
        basicsize: 32,
        ..Spec::default()
    };
    let stack = class(
        &mut process,
        &types,
        &Spec {
            name: "Stack",
            ..spec
        },
        &[types.list.get(), object],
    );
    let dicey = class(
        &mut process,
        &types,
        &Spec {
            name: "Dicey",
            attributes: &[("__getitem__", marker)],
            ..spec
        },
        &[types.list.get(), object],
    );
    let ten = process.int(types.int, 10);
    let twenty = process.int(types.int, 20);
    let stack_list = list_instance(&mut process, stack, &[ten, twenty]);
    let dicey_list = list_instance(&mut process, dicey, &[ten, twenty]);

    assert_eq!(
        item_at(&process, types, stack_list, 1),
        Ok(VirtualAddress::new(twenty))
    );
    assert_eq!(
        item_at(&process, types, stack_list, -2),
        Ok(VirtualAddress::new(ten))
    );
    let length = with_values(&process, types, InspectionLimits::default(), |values| {
        values.length(VirtualAddress::new(stack_list))
    });
    assert_eq!(length, Ok(2));

    match item_at(&process, types, dicey_list, 1) {
        Err(InterpretedFailure::Refused(ErrorKind::Unsupported, message)) => {
            assert!(
                message.contains("`app.Dicey`") && message.contains("its own `__getitem__`"),
                "the message: {message}"
            );
        }
        other => panic!("expected a refusal, got {other:?}"),
    }
}

#[test]
fn a_module_and_a_class_are_shown_by_their_names() {
    let mut process = Process::new(layout());
    let (types, object) = world(&mut process);
    let module = module_object(&mut process, &types, "pkg.mod");
    let point = class(
        &mut process,
        &types,
        &Spec {
            name: "Point",
            module: Some("geom"),
            ..Spec::default()
        },
        &[object],
    );

    let shown = presented_at(&process, types, module);
    assert_eq!(shown.summary.as_ref(), "<module 'pkg.mod'>");
    assert_eq!(shown.kind, InterpretedKind::Other);
    assert_eq!(shown.type_name.as_ref(), "module");
    assert_eq!(shown.module.as_deref(), Some("builtins"));

    let shown = presented_at(&process, types, point);
    assert_eq!(shown.summary.as_ref(), "<class 'geom.Point'>");
    assert_eq!(shown.kind, InterpretedKind::Other);
    assert_eq!(shown.type_name.as_ref(), "type");
    assert_eq!(shown.module.as_deref(), Some("builtins"));

    // A module whose dict cannot be read is shown by its address, saying
    // why it has no name.
    let unread = process.object(types.module, 32);
    process.word(unread, Field::ModuleDict, 0x10);
    let shown = presented_at(&process, types, unread);
    assert_eq!(
        shown.summary.as_ref(),
        format!("<module object at {unread:#x}>")
    );
    assert!(shown.problem.is_some(), "{shown:?}");
}

#[test]
fn functions_and_built_in_functions_and_methods_are_shown_by_their_names() {
    let mut process = Process::new(layout());
    let (types, _) = world(&mut process);
    let module = module_object(&mut process, &types, "builtins");
    let one = process.int(types.int, 1);
    let list = process.list(types.list, &[one]);
    let function = function_object(&mut process, &types, "Point.move");
    let len = builtin_object(&mut process, &types, "len", 0);
    let print = builtin_object(&mut process, &types, "print", module);
    let append = builtin_object(&mut process, &types, "append", list);

    let shown = presented_at(&process, types, function);
    assert_eq!(
        shown.summary.as_ref(),
        format!("<function Point.move at {function:#x}>")
    );
    assert_eq!(shown.type_name.as_ref(), "function");

    assert_eq!(
        presented_at(&process, types, len).summary.as_ref(),
        "<built-in function len>"
    );
    assert_eq!(
        presented_at(&process, types, print).summary.as_ref(),
        "<built-in function print>"
    );

    let shown = presented_at(&process, types, append);
    assert_eq!(
        shown.summary.as_ref(),
        format!("<built-in method append of list object at {list:#x}>")
    );
    assert_eq!(shown.type_name.as_ref(), "builtin_function_or_method");
}

#[test]
fn a_code_object_is_shown_with_its_file_and_its_first_line_or_minus_one_for_none() {
    let mut process = Process::new(layout());
    let (types, _) = world(&mut process);
    let numbered = code_object(&mut process, &types, "f", 3);
    let unnumbered = code_object(&mut process, &types, "g", 0);

    let shown = presented_at(&process, types, numbered);
    assert_eq!(
        shown.summary.as_ref(),
        format!("<code object f at {numbered:#x}, file \"app.py\", line 3>")
    );
    assert_eq!(shown.type_name.as_ref(), "code");
    assert_eq!(
        presented_at(&process, types, unnumbered).summary.as_ref(),
        format!("<code object g at {unnumbered:#x}, file \"app.py\", line -1>")
    );
}

#[test]
fn a_missing_attribute_of_an_instance_with_no_values_is_a_missing_key() {
    let mut process = Process::new(layout());
    let (types, object) = world(&mut process);
    let plain = class(
        &mut process,
        &types,
        &Spec {
            name: "Plain",
            module: Some("app"),
            flags: MANAGED_DICT,
            basicsize: 16,
            keys: &["label"],
            ..Spec::default()
        },
        &[object],
    );
    let instance = managed_instance(&mut process, plain, 16);
    let values = tagged_values(&mut process, &[]);
    set_managed(&mut process, instance, values);

    match attribute_in(&process, types, instance, "nope") {
        Err(InterpretedFailure::Refused(ErrorKind::MissingKey, _)) => {}
        other => panic!("expected a missing key, got {other:?}"),
    }
}
