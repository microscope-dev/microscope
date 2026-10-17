//! numpy's arrays, read from synthetic memory: their fields after the
//! object's header, as numpy's public structure lays them out, their
//! dtype's byte order and type number, and their elements by shape and
//! strides.

use super::super::values::Types;
use super::values::{describe, read_value, with_values};
use super::{Process, layout};
use crate::image::python::Field;
use crate::runtime_model::{ArrayElement, InterpretedChild, InterpretedKind, InterpretedSlot};
use crate::{InspectionLimits, NumberKind, ScalarValue, VirtualAddress};

/// numpy's type numbers of the types the tests write.
const INT8: i32 = 1;
const INT32: i32 = 5;
const INT64: i32 = 7;
const FLOAT32: i32 = 11;
const FLOAT64: i32 = 12;
const COMPLEX128: i32 = 15;

/// The size of numpy's array objects past the header, which holds every
/// field numpy 2 gives an array.
const ARRAY_FIELDS: u64 = 80;

/// One array's elements, their layout, and their dtype.
struct Spec<'a> {
    data: &'a [u8],

    /// Where its first element is, in bytes into `data`.
    first: u64,
    shape: &'a [i64],
    strides: &'a [i64],
    number: i32,
    order: u8,
}

impl<'a> Spec<'a> {
    /// A native-order array of `data`, its first element first.
    const fn new(data: &'a [u8], shape: &'a [i64], strides: &'a [i64], number: i32) -> Self {
        Self {
            data,
            first: 0,
            shape,
            strides,
            number,
            order: b'=',
        }
    }
}

/// The world, and numpy's array type, whose objects are `fields` bytes past
/// the header.
fn numpy_world(fields: u64) -> (Process, Types, VirtualAddress) {
    let mut process = Process::new(layout());
    let types = process.builtin_types();
    let ty = process.type_object("numpy.ndarray");
    let size = process.offset(Field::ObjectSize) + fields;
    process.word(ty.get(), Field::TypeBasicsize, size);
    (process, types, ty)
}

/// An array object of `ty`, as `spec` describes it.
fn array(process: &mut Process, ty: VirtualAddress, spec: &Spec<'_>) -> u64 {
    let header = process.offset(Field::ObjectSize);
    let data = process.alloc((spec.data.len() as u64).max(8));
    process.memory.fill(data, spec.data);
    let words = |process: &mut Process, values: &[i64]| {
        let at = process.alloc((8 * values.len() as u64).max(8));
        let values: Vec<u64> = values.iter().map(|value| value.cast_unsigned()).collect();
        process.words(at, &values);
        at
    };
    let shape = words(process, spec.shape);
    let strides = words(process, spec.strides);

    let dtype = process.alloc(header + 16);
    process
        .memory
        .write(dtype + header + 10, u64::from(spec.order), 1);
    process.memory.write(
        dtype + header + 12,
        u64::from(spec.number.cast_unsigned()),
        4,
    );

    let object = process.object(ty, header + ARRAY_FIELDS);
    process.memory.write(object + header, data + spec.first, 8);
    process
        .memory
        .write(object + header + 8, spec.shape.len() as u64, 4);
    process.memory.write(object + header + 16, shape, 8);
    process.memory.write(object + header + 24, strides, 8);
    process.memory.write(object + header + 40, dtype, 8);
    object
}

/// The children `range` of the part at `part` of the object at `object`.
fn part_children(
    process: &Process,
    types: Types,
    (object, part): (u64, &[u64]),
    range: std::ops::Range<u64>,
) -> Vec<InterpretedChild> {
    with_values(process, types, InspectionLimits::default(), |values| {
        values.children(VirtualAddress::new(object), part, range)
    })
    .expect("the array's children")
}

/// Each child's summary, of children that are all elements.
fn summaries(children: &[InterpretedChild]) -> Vec<String> {
    children
        .iter()
        .map(|child| match child {
            InterpretedChild::Element(slot) => describe(slot),
            other => panic!("an array's children are elements: {other:?}"),
        })
        .collect()
}

fn bytes_of<const N: usize>(values: impl IntoIterator<Item = [u8; N]>) -> Vec<u8> {
    values.into_iter().flatten().collect()
}

#[test]
fn an_array_of_one_axis_shows_each_number_by_its_type() {
    let (mut process, types, ty) = numpy_world(ARRAY_FIELDS);
    let data = bytes_of([(-128_i8).to_le_bytes(), 127_i8.to_le_bytes()]);
    let object = array(&mut process, ty, &Spec::new(&data, &[2], &[1], INT8));
    let value = read_value(&process, types, object);
    assert_eq!(&*value.summary, "array([-128, 127], dtype=int8)");
    assert_eq!(value.kind, InterpretedKind::Sequence { length: 2 });
    let array = value.array.expect("an array the debugger describes");
    assert_eq!(array.element, ArrayElement::Number(NumberKind::I8));
    assert_eq!(&*array.element_name, "int8");
    assert!(array.contiguous().is_some(), "next to each other");

    let children = part_children(&process, types, (object, &[]), 0..2);
    let InterpretedChild::Element(InterpretedSlot::Number(first)) = &children[0] else {
        panic!("a number: {children:?}");
    };
    assert_eq!(first.value, ScalarValue::Signed(-128), "sign-extended");
    assert_eq!(summaries(&children), ["-128", "127"]);
}

#[test]
fn a_float32_has_the_shortest_digits_of_its_own_width() {
    let (mut process, types, ty) = numpy_world(ARRAY_FIELDS);
    let data = bytes_of([0.1_f32.to_le_bytes(), 16_777_216_f32.to_le_bytes()]);
    let object = array(&mut process, ty, &Spec::new(&data, &[2], &[4], FLOAT32));
    assert_eq!(
        &*read_value(&process, types, object).summary,
        "array([0.1, 16777216.0], dtype=float32)"
    );
}

#[test]
fn an_array_with_negative_strides_runs_backwards_and_lies_apart() {
    let (mut process, types, ty) = numpy_world(ARRAY_FIELDS);
    let data = bytes_of([1.0_f64, 2.0, 3.0].map(f64::to_le_bytes));
    let spec = Spec {
        first: 16,
        ..Spec::new(&data, &[3], &[-8], FLOAT64)
    };
    let object = array(&mut process, ty, &spec);
    let value = read_value(&process, types, object);
    assert_eq!(&*value.summary, "array([3.0, 2.0, 1.0], dtype=float64)");
    assert!(
        value.array.expect("an array").contiguous().is_none(),
        "elements that run backwards are not read at once"
    );
}

#[test]
fn an_array_of_two_axes_has_rows_which_are_its_parts() {
    let (mut process, types, ty) = numpy_world(ARRAY_FIELDS);
    let data = bytes_of((0_i32..6).map(i32::to_le_bytes));
    let object = array(
        &mut process,
        ty,
        &Spec::new(&data, &[2, 3], &[12, 4], INT32),
    );
    let value = read_value(&process, types, object);
    assert_eq!(
        &*value.summary,
        "array([[0, 1, 2], [3, 4, 5]], dtype=int32)"
    );
    assert!(
        value.array.expect("an array").contiguous().is_none(),
        "rows are read one at a time"
    );

    let rows = part_children(&process, types, (object, &[]), 0..2);
    let InterpretedChild::Element(InterpretedSlot::Value(row)) = &rows[1] else {
        panic!("a row: {rows:?}");
    };
    assert_eq!(&*row.summary, "array([3, 4, 5], dtype=int32)");
    assert_eq!(&*row.part, [1]);
    assert_eq!(
        row.object,
        VirtualAddress::new(object),
        "a part of the object"
    );
    assert!(
        row.array
            .as_ref()
            .is_some_and(|row| row.contiguous().is_some())
    );
    assert_eq!(
        summaries(&part_children(&process, types, (object, &[1]), 1..3)),
        ["4", "5"]
    );

    // A part past the array's shape is no part of it.
    let past = with_values(&process, types, InspectionLimits::default(), |values| {
        values.children(VirtualAddress::new(object), &[2], 0..1)
    });
    assert!(past.is_err(), "row 2 of 2: {past:?}");
}

#[test]
fn an_array_of_no_axes_is_its_one_element() {
    let (mut process, types, ty) = numpy_world(ARRAY_FIELDS);
    let data = 5_i64.to_le_bytes();
    let object = array(&mut process, ty, &Spec::new(&data, &[], &[], INT64));
    let value = read_value(&process, types, object);
    assert_eq!(&*value.summary, "array(5, dtype=int64)");
    assert_eq!(value.kind, InterpretedKind::Other);
    assert!(value.array.is_none(), "no elements to page");
}

#[test]
fn an_array_of_elements_microscope_does_not_read_shows_its_shape_and_says_why() {
    let (mut process, types, ty) = numpy_world(ARRAY_FIELDS);
    let data = [0; 32];
    let object = array(&mut process, ty, &Spec::new(&data, &[2], &[16], COMPLEX128));
    let value = read_value(&process, types, object);
    assert_eq!(
        &*value.summary,
        "<numpy.ndarray of shape (2,), dtype complex128>"
    );
    assert_eq!(value.kind, InterpretedKind::Other);
    assert!(value.problem.is_some(), "says why");

    let big = Spec {
        order: b'>',
        ..Spec::new(&data, &[2, 1], &[4, 4], INT32)
    };
    let object = array(&mut process, ty, &big);
    assert_eq!(
        &*read_value(&process, types, object).summary,
        "<numpy.ndarray of shape (2, 1), dtype int32 (big-endian)>"
    );
}

#[test]
fn a_type_of_numpys_name_too_small_for_its_fields_is_no_array() {
    let (mut process, types, ty) = numpy_world(16);
    let data = [0; 8];
    let object = array(&mut process, ty, &Spec::new(&data, &[1], &[8], FLOAT64));
    assert_eq!(
        &*read_value(&process, types, object).summary,
        format!("<numpy.ndarray object at {object:#x}>")
    );
}
