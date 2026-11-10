//! numpy's arrays, read from synthetic memory: their fields after the
//! object's header, as numpy's public structure lays them out, their
//! dtype's byte order and type number, and their elements by shape and
//! strides.

use super::super::values::Types;
use super::values::{read_value, with_values};
use super::{Process, layout};
use crate::image::python::Field;
use crate::{InspectionLimits, VirtualAddress};

/// numpy's type numbers of the types the tests write.
const INT8: i32 = 1;
const INT32: i32 = 5;
const FLOAT64: i32 = 12;

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

fn bytes_of<const N: usize>(values: impl IntoIterator<Item = [u8; N]>) -> Vec<u8> {
    values.into_iter().flatten().collect()
}

#[test]
fn a_part_past_an_arrays_shape_is_no_part_of_it() {
    let (mut process, types, ty) = numpy_world(ARRAY_FIELDS);
    let data = bytes_of((0_i32..6).map(i32::to_le_bytes));
    let object = array(
        &mut process,
        ty,
        &Spec::new(&data, &[2, 3], &[12, 4], INT32),
    );
    let past = with_values(&process, types, InspectionLimits::default(), |values| {
        values.children(VirtualAddress::new(object), &[2], 0..1)
    });
    assert!(past.is_err(), "row 2 of 2: {past:?}");
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

#[test]
fn each_row_of_an_array_wider_than_a_summary_shows_is_shown_cut_short() {
    let (mut process, types, ty) = numpy_world(ARRAY_FIELDS);
    let data: Vec<u8> = (0..130_u8).map(|value| value % 100).collect();
    let object = array(
        &mut process,
        ty,
        &Spec::new(&data, &[2, 65], &[65, 1], INT8),
    );

    let row = |first: u8| {
        let numbers: Vec<String> = (first..first + 64)
            .map(|value| (value % 100).to_string())
            .collect();
        format!("[{}, ...]", numbers.join(", "))
    };
    let value = read_value(&process, types, object);
    assert_eq!(
        &*value.summary,
        format!("array([{}, {}], dtype=int8)", row(0), row(65))
    );
    assert_eq!(
        value.problem.as_deref(),
        Some("a summary shows 64 items of each container")
    );
}
