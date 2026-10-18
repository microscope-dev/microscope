//! numpy's arrays, which keep their elements as the machine does, apart
//! from any object.
//!
//! An array's fields after the object's header are numpy's public C ABI,
//! `PyArrayObject_fields`, unchanged from numpy 1 to 2: its data, its
//! number of axes, its shape and strides, its base, and its dtype. numpy's
//! extension carries no DWARF, so their offsets are written here, after
//! the header the layout gives ([`Field::ObjectSize`]). A dtype keeps its
//! kind, byte order, and type number at the same place in numpy 1 and 2,
//! and numpy numbers its built-in types alike in both; the type number
//! alone names the element.
//!
//! An array of truth values, integers, or 32- or 64-bit floats in the
//! machine's byte order shows its elements; any other shows its shape and
//! dtype alone, and says why. Its summary is `array(ELEMENTS,
//! dtype=NAME)`, each element as Python writes the number, a float with
//! the shortest digits its own width needs, as `float32(0.1)` is `0.1`.

use std::sync::Arc;

use super::instances::Class;
use super::objects::{Head, Problem, malformed};
use super::text;
use super::values::{SUMMARY_DEPTH, SUMMARY_ITEMS, SUMMARY_OBJECTS, Summary, Values, note};
use crate::image::python::Field;
use crate::runtime_model::{
    ArrayElement, InterpretedArray, InterpretedChild, InterpretedKind, InterpretedNumber,
    InterpretedSlot, InterpretedValue,
};
use crate::{FloatValue, NumberKind, ScalarValue, VirtualAddress};

/// Where `PyArrayObject_fields` keeps each field read, past the object's
/// header, and the size of those fields, which an array's type's objects
/// hold at least.
const DATA: u64 = 0;
const AXES: u64 = 8;
const SHAPE: u64 = 16;
const STRIDES: u64 = 24;
const DTYPE: u64 = 40;
pub(super) const FIELDS: u64 = 48;

/// Where `PyArray_Descr` keeps its byte order and its type number, past
/// the object's header.
const BYTE_ORDER: u64 = 10;
const TYPE_NUMBER: u64 = 12;

/// The most axes numpy gives an array.
const MAX_AXES: u64 = 64;

/// An array, or a part of one: its first element, its shape and strides,
/// and its elements' type.
#[derive(Debug, Clone)]
pub(super) struct Array {
    pub(super) data: VirtualAddress,
    pub(super) shape: Vec<u64>,
    pub(super) strides: Vec<i64>,
    pub(super) element: Element,
}

/// What an array's elements are: those it reads, by their name, or the
/// name of a type it does not read, and why.
#[derive(Debug, Clone)]
pub(super) enum Element {
    Read(ArrayElement, &'static str),
    Unread(String, &'static str),
}

/// One element's value.
#[derive(Debug, Clone, Copy)]
enum Number {
    Bool(bool),
    Signed(i64),
    Unsigned(u64),
    F32(f32),
    F64(f64),
}

impl Number {
    /// The element as the debugger's scalar, its float's bits exact.
    fn scalar(self) -> ScalarValue {
        match self {
            Self::Bool(value) => ScalarValue::Boolean(value),
            Self::Signed(value) => ScalarValue::Signed(i128::from(value)),
            Self::Unsigned(value) => ScalarValue::Unsigned(u128::from(value)),
            Self::F32(value) => ScalarValue::Floating(FloatValue::Binary32(value.to_bits())),
            Self::F64(value) => ScalarValue::Floating(FloatValue::Binary64(value.to_bits())),
        }
    }

    /// The element as Python writes the number; a float with the shortest
    /// digits that read back as the same number of its own width.
    fn text(self) -> String {
        match self {
            Self::Bool(true) => "True".to_owned(),
            Self::Bool(false) => "False".to_owned(),
            Self::Signed(value) => value.to_string(),
            Self::Unsigned(value) => value.to_string(),
            Self::F32(value) => text::float32(value),
            Self::F64(value) => text::float(value),
        }
    }
}

impl Array {
    /// What kind of value it is: a sequence of its elements on its first
    /// axis, where it has one and shows them.
    pub(super) fn kind(&self) -> InterpretedKind {
        match (self.shape.first(), &self.element) {
            (Some(&length), Element::Read(..)) => InterpretedKind::Sequence { length },
            _ => InterpretedKind::Other,
        }
    }

    /// The array as the debugger describes it, where it shows elements.
    pub(super) fn interpreted(&self) -> Option<Arc<InterpretedArray>> {
        let Element::Read(element, name) = self.element else {
            return None;
        };

        (!self.shape.is_empty()).then(|| {
            Arc::new(InterpretedArray {
                data: self.data,
                shape: self.shape.clone().into(),
                strides: self.strides.clone().into(),
                element,
                element_name: name.into(),
            })
        })
    }

    /// The part at `index` of its first axis: an array of the rest.
    fn row(&self, index: u64) -> Self {
        Self {
            data: offset(self.data, index, self.strides.first().copied().unwrap_or(0)),
            shape: self.shape.get(1..).unwrap_or_default().to_vec(),
            strides: self.strides.get(1..).unwrap_or_default().to_vec(),
            element: self.element.clone(),
        }
    }

    /// Its part at `part`, an index on each of its first axes.
    fn part(&self, part: &[u64]) -> Option<Self> {
        let mut array = self.clone();
        for &index in part {
            if array.shape.first().is_none_or(|length| index >= *length) {
                return None;
            }

            array = array.row(index);
        }

        Some(array)
    }
}

/// The address `index` strides of `stride` bytes from `data`.
fn offset(data: VirtualAddress, index: u64, stride: i64) -> VirtualAddress {
    let bytes = i64::try_from(index)
        .unwrap_or(i64::MAX)
        .wrapping_mul(stride);
    VirtualAddress::new(data.get().wrapping_add_signed(bytes))
}

/// The element of numpy's type number `number`, in byte order `order`.
fn element(number: i32, order: u8) -> Element {
    let read = |element, name| {
        if order == b'>' {
            Element::Unread(
                format!("{name} (big-endian)"),
                "microscope reads the elements of arrays in the machine's byte order",
            )
        } else {
            Element::Read(element, name)
        }
    };
    let number_of = |kind, name| read(ArrayElement::Number(kind), name);
    match number {
        0 => read(ArrayElement::Bool, "bool"),
        1 => number_of(NumberKind::I8, "int8"),
        2 => number_of(NumberKind::U8, "uint8"),
        3 => number_of(NumberKind::I16, "int16"),
        4 => number_of(NumberKind::U16, "uint16"),
        5 => number_of(NumberKind::I32, "int32"),
        6 => number_of(NumberKind::U32, "uint32"),
        7 | 9 => number_of(NumberKind::I64, "int64"),
        8 | 10 => number_of(NumberKind::U64, "uint64"),
        11 => number_of(NumberKind::F32, "float32"),
        12 => number_of(NumberKind::F64, "float64"),
        number => {
            let name = match number {
                13 => "longdouble",
                14 => "complex64",
                15 => "complex128",
                16 => "clongdouble",
                17 => "object",
                18 => "bytes",
                19 => "str",
                20 => "void",
                21 => "datetime64",
                22 => "timedelta64",
                23 => "float16",
                _ => "",
            };
            Element::Unread(
                if name.is_empty() {
                    format!("dtype {number}")
                } else {
                    name.to_owned()
                },
                "microscope reads the elements of arrays of truth values, integers, and 32- \
                 and 64-bit floats",
            )
        }
    }
}

impl Values<'_, '_> {
    /// The array the head is of, an object of a type laid out as numpy's
    /// arrays are.
    pub(super) fn array(&self, head: &Head<'_, '_>) -> Result<Array, Problem> {
        let address = head.address();
        let reader = &self.reader;
        let header = reader.offset(Field::ObjectSize)?;
        let bad = || malformed(address, "numpy array");
        let word = |at: u64| head.bytes(address.wrapping_add(header + at), 8);
        let le = |bytes: Vec<u8>| -> Result<u64, Problem> {
            Ok(u64::from_le_bytes(bytes.try_into().map_err(|_| bad())?))
        };

        let data = VirtualAddress::new(le(word(DATA)?)?);
        let axes = head.bytes(address.wrapping_add(header + AXES), 4)?;
        let axes = u32::from_le_bytes(axes.try_into().map_err(|_| bad())?);
        let axes = u64::from(axes);
        if axes > MAX_AXES {
            return Err(bad());
        }

        let numbers = |pointer: u64| -> Result<Vec<i64>, Problem> {
            if axes == 0 {
                return Ok(Vec::new());
            }

            Ok(reader
                .bytes(VirtualAddress::new(pointer), axes * 8)?
                .as_chunks::<8>()
                .0
                .iter()
                .map(|number| i64::from_le_bytes(*number))
                .collect())
        };
        let shape = numbers(le(word(SHAPE)?)?)?
            .into_iter()
            .map(|length| u64::try_from(length).map_err(|_| bad()))
            .collect::<Result<Vec<u64>, Problem>>()?;
        let strides = numbers(le(word(STRIDES)?)?)?;
        if shape
            .iter()
            .try_fold(1_u64, |count, length| count.checked_mul(*length))
            .is_none()
        {
            return Err(bad());
        }

        // The dtype's byte order, then its type number, a C int.
        let dtype = VirtualAddress::new(le(word(DTYPE)?)?);
        let number_at = usize::try_from(TYPE_NUMBER - BYTE_ORDER).map_err(|_| bad())?;
        let described = reader.bytes(
            dtype.wrapping_add(header + BYTE_ORDER),
            TYPE_NUMBER - BYTE_ORDER + 4,
        )?;
        let order = described.first().copied().ok_or_else(bad)?;
        let number = described
            .get(number_at..)
            .and_then(|bytes| <[u8; 4]>::try_from(bytes).ok())
            .map(i32::from_le_bytes)
            .ok_or_else(bad)?;

        Ok(Array {
            data,
            shape,
            strides,
            element: element(number, order),
        })
    }

    /// The number of type `element` at `at`.
    fn number(&self, at: VirtualAddress, element: ArrayElement) -> Result<Number, Problem> {
        let size = match element {
            ArrayElement::Bool => 1,
            ArrayElement::Number(kind) => kind.size(),
        };
        let bytes = self.reader.bytes(at, size)?;
        let mut word = [0; 8];
        word.get_mut(..bytes.len())
            .ok_or_else(|| malformed(at, "number"))?
            .copy_from_slice(&bytes);
        let bits = u64::from_le_bytes(word);

        // An integer narrower than 64 bits, sign-extended from its top bit.
        let signed = |width: u64| {
            let shift = 64 - width * 8;
            Number::Signed((bits << shift).cast_signed() >> shift)
        };
        Ok(match element {
            ArrayElement::Bool => Number::Bool(bits != 0),
            ArrayElement::Number(
                kind @ (NumberKind::I8 | NumberKind::I16 | NumberKind::I32 | NumberKind::I64),
            ) => signed(kind.size()),
            ArrayElement::Number(
                NumberKind::U8 | NumberKind::U16 | NumberKind::U32 | NumberKind::U64,
            ) => Number::Unsigned(bits),
            ArrayElement::Number(NumberKind::F32) => Number::F32(f32::from_bits(
                u32::try_from(bits).map_err(|_| malformed(at, "number"))?,
            )),
            ArrayElement::Number(NumberKind::F64) => Number::F64(f64::from_bits(bits)),
        })
    }

    /// The text of the array, or its part, `array`, as its summary shows
    /// it.
    pub(super) fn array_text(&self, array: &Array, summary: &mut Summary) -> String {
        match array.element {
            Element::Read(element, name) => {
                let elements = self.nested(array, element, 0, summary);
                format!("array({elements}, dtype={name})")
            }
            Element::Unread(ref name, why) => {
                note(summary, why.to_owned());
                let shape: Vec<String> = array.shape.iter().map(u64::to_string).collect();
                let shape = match shape.as_slice() {
                    [one] => format!("({one},)"),
                    shape => format!("({})", shape.join(", ")),
                };
                format!("<numpy.ndarray of shape {shape}, dtype {name}>")
            }
        }
    }

    /// The elements of `array`, `depth` axes into an array's summary, as
    /// nested lists.
    fn nested(
        &self,
        array: &Array,
        element: ArrayElement,
        depth: usize,
        summary: &mut Summary,
    ) -> String {
        let Some(&length) = array.shape.first() else {
            return self.element_text(array.data, element, summary);
        };

        if depth >= SUMMARY_DEPTH {
            note(
                summary,
                format!("a summary nests {SUMMARY_DEPTH} containers deep"),
            );
            return "...".to_owned();
        }

        // A row is an object of the summary as an element is, so that an
        // array whose rows hold no elements is bounded too.
        summary.objects += 1;
        let shown = length.min(SUMMARY_ITEMS);
        let mut texts = Vec::new();
        for index in 0..shown {
            if summary.objects >= SUMMARY_OBJECTS || summary.problem.is_some() {
                note(
                    summary,
                    format!("a summary shows {SUMMARY_OBJECTS} objects"),
                );
                texts.push("...".to_owned());
                return format!("[{}]", texts.join(", "));
            }

            texts.push(self.nested(&array.row(index), element, depth + 1, summary));
        }

        if shown < length {
            note(
                summary,
                format!("a summary shows {SUMMARY_ITEMS} items of each container"),
            );
            texts.push("...".to_owned());
        }

        format!("[{}]", texts.join(", "))
    }

    /// The text of the element of type `element` at `at`.
    fn element_text(
        &self,
        at: VirtualAddress,
        element: ArrayElement,
        summary: &mut Summary,
    ) -> String {
        summary.objects += 1;
        match self.number(at, element) {
            Ok(number) => number.text(),
            Err(problem) => {
                note(summary, problem.to_string());
                "...".to_owned()
            }
        }
    }

    /// The children `range` of the part at `part` of the array `array`, of
    /// the object at `object`, whose class is `class`: its numbers, where
    /// it has one axis, or else its arrays of the rest.
    pub(super) fn array_children(
        &self,
        (object, class): (VirtualAddress, &Class),
        array: &Array,
        part: &[u64],
        range: std::ops::Range<u64>,
    ) -> Result<Vec<InterpretedChild>, Problem> {
        let array = array
            .part(part)
            .ok_or_else(|| malformed(object, "numpy array"))?;
        let (Some(&length), Element::Read(element, name)) =
            (array.shape.first(), array.element.clone())
        else {
            return Err(format!("the object at {object:#x} holds no items").into());
        };

        let end = range.end.min(length);
        let start = range.start.min(end);
        let mut children = Vec::new();
        for index in start..end {
            let row = array.row(index);
            let slot = if row.shape.is_empty() {
                match self.number(row.data, element) {
                    Ok(number) => InterpretedSlot::Number(InterpretedNumber {
                        address: row.data,
                        summary: number.text().into(),
                        value: number.scalar(),
                        element,
                        module: Some("numpy".into()),
                        type_name: name.into(),
                    }),
                    Err(problem) => self.failed(problem),
                }
            } else {
                let mut summary = Summary {
                    objects: 1,
                    within: vec![object],
                    problem: None,
                };
                let text = self.array_text(&row, &mut summary);
                InterpretedSlot::Value(InterpretedValue {
                    object,
                    class: class.address,
                    module: class.module.clone(),
                    type_name: Arc::clone(&class.name),
                    kind: row.kind(),
                    attributes: 0,
                    summary: text.into(),
                    problem: summary.problem,
                    part: part.iter().copied().chain([index]).collect(),
                    array: row.interpreted(),
                })
            };
            children.push(InterpretedChild::Element(slot));
        }

        Ok(children)
    }
}
