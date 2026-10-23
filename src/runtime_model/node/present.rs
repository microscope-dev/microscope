//! JavaScript values as the debugger presents them: each summarized as
//! `util.inspect` writes it, with its children read on demand.
//!
//! A value is named by its handle ([`InterpretedHandle`]): its tagged word,
//! a Smi or a pointer to a heap object, or, for an element of a double
//! array, which no object holds, the element's address with the low bits
//! `0b10`, which no tagged word has.

use std::sync::Arc;

use super::inspect::{self, Kind as Shape, Value};
use super::objects::{Object, Problem, Reader};
use super::own::{self, Held};
use super::values::Builder;
use crate::runtime_model::{
    ArrayElement, InterpretedArray, InterpretedChild, InterpretedHandle, InterpretedKind,
    InterpretedNumber, InterpretedSlot, InterpretedValue,
};
use crate::{FloatValue, NumberKind, ScalarValue, VirtualAddress};

/// The low bits of a handle that names a double array's element.
const DOUBLE_TAG: u64 = 0b10;
const TAG_MASK: u64 = 0b11;

/// What a handle names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Handle {
    /// A tagged word.
    Tagged(u64),
    /// A double array's element, by its address.
    Double(u64),
}

impl Handle {
    pub(super) const fn of(handle: InterpretedHandle) -> Self {
        let bits = handle.get();
        if bits & TAG_MASK == DOUBLE_TAG {
            Self::Double(bits & !TAG_MASK)
        } else {
            Self::Tagged(bits)
        }
    }

    pub(super) const fn handle(self) -> InterpretedHandle {
        InterpretedHandle::new(match self {
            Self::Tagged(word) => word,
            Self::Double(address) => address | DOUBLE_TAG,
        })
    }
}

/// The memory the value `handle` names lives at: a heap object's, or a
/// double element's; none for a Smi.
pub(super) fn address(reader: &Reader<'_>, handle: InterpretedHandle) -> Option<VirtualAddress> {
    match Handle::of(handle) {
        Handle::Double(address) => Some(VirtualAddress::new(address)),
        Handle::Tagged(word) => {
            let object = reader.object(word).ok()??;
            let tag = reader
                .constant(crate::image::v8::Constant::HeapObjectTag)
                .ok()?;
            Some(VirtualAddress::new(object.0 - tag.cast_unsigned()))
        }
    }
}

/// The value `handle` names, presented, as Node `major` writes it.
pub(super) fn value(reader: &Reader<'_>, handle: InterpretedHandle, major: u32) -> InterpretedSlot {
    match presented(reader, handle, major) {
        Ok(Some(value)) => InterpretedSlot::Value(value),
        Ok(None) => InterpretedSlot::Unbound,
        Err(problem) => InterpretedSlot::Unreadable(problem),
    }
}

/// The value `handle` names, or `None` for the hole, a variable not yet
/// initialized.
fn presented(
    reader: &Reader<'_>,
    handle: InterpretedHandle,
    major: u32,
) -> Result<Option<InterpretedValue>, Problem> {
    let word = match Handle::of(handle) {
        Handle::Double(address) => {
            let number = f64::from_bits(reader.word(address)?);
            return Ok(Some(number_value(handle, Some(address), number, major)));
        }
        Handle::Tagged(word) => word,
    };
    if own::is_hole(reader, word)? {
        return Ok(None);
    }
    let mut builder = Builder::new(*reader);
    let root = builder.value(word, 0);
    let text = inspect::inspect(&builder.graph, &root, major);
    let summary: Arc<str> = text.text.into();
    let problem = text
        .approximate
        .then(|| Arc::from("the layout of this text may differ from Node's in its spacing"));
    let at = address(reader, handle);

    let (type_name, kind, attributes) = match &root {
        Value::Undefined | Value::Null => (
            if root == Value::Null {
                "null"
            } else {
                "undefined"
            }
            .into(),
            InterpretedKind::Nothing,
            0,
        ),
        Value::Boolean(value) => ("boolean".into(), InterpretedKind::Bool(*value), 0),
        Value::Number(number) => {
            let kind = reader
                .smi(word)?
                .map_or(InterpretedKind::Float(*number), |integer| {
                    InterpretedKind::Int(Some(i128::from(integer)))
                });
            ("number".into(), kind, 0)
        }
        Value::BigInt(_) => (
            "bigint".into(),
            InterpretedKind::Int(bigint_value(&summary)),
            0,
        ),
        Value::String(_) => ("string".into(), InterpretedKind::Text, 0),
        Value::Symbol(_) => ("symbol".into(), InterpretedKind::Other, 0),
        Value::Unreadable(problem) => return Err(problem.as_str().into()),
        Value::Object(id) => {
            let object = builder
                .graph
                .row(*id)
                .ok_or("the value's object was not read")?;
            let named = object.properties.len() as u64;
            let (kind, attributes) = match &object.kind {
                Shape::Array(array) => (
                    InterpretedKind::Sequence {
                        length: u64::from(array.length),
                    },
                    named,
                ),
                Shape::Set { size, .. } => (
                    InterpretedKind::Sequence {
                        length: u64::from(*size),
                    },
                    named,
                ),
                Shape::Map { size, .. } => (
                    InterpretedKind::Map {
                        length: u64::from(*size),
                    },
                    named,
                ),
                Shape::TypedArray { length, .. } => {
                    (InterpretedKind::Sequence { length: *length }, named)
                }
                Shape::Ordinary | Shape::Error(_) => (InterpretedKind::Instance, named),
                _ => (InterpretedKind::Other, named),
            };
            (
                constructor_text(&object.constructor).into(),
                kind,
                attributes,
            )
        }
    };
    let array = typed_array(reader, word)?;
    Ok(Some(InterpretedValue {
        object: handle,
        address: at,
        class: InterpretedHandle::new(class(reader, word).unwrap_or(0)),
        module: None,
        type_name,
        kind,
        attributes,
        summary,
        problem,
        part: Arc::from([]),
        array,
    }))
}

/// A number's value as a double element holds it.
fn number_value(
    handle: InterpretedHandle,
    address: Option<u64>,
    number: f64,
    major: u32,
) -> InterpretedValue {
    let graph = inspect::ValueGraph::new();
    let text = inspect::inspect(&graph, &Value::Number(number), major).text;
    InterpretedValue {
        object: handle,
        address: address.map(VirtualAddress::new),
        class: InterpretedHandle::new(0),
        module: None,
        type_name: "number".into(),
        kind: InterpretedKind::Float(number),
        attributes: 0,
        summary: text.into(),
        problem: None,
        part: Arc::from([]),
        array: None,
    }
}

/// A `BigInt`'s value where it fits in 128 bits, from its decimal text.
fn bigint_value(summary: &str) -> Option<i128> {
    summary.strip_suffix('n')?.parse().ok()
}

/// The name a constructor gives its type.
fn constructor_text(constructor: &inspect::Constructor) -> String {
    match constructor {
        inspect::Constructor::Named(name) => String::from_utf16_lossy(name.units()),
        inspect::Constructor::NullPrototype { .. } => "[Object: null prototype]".to_owned(),
        inspect::Constructor::Unnamed { .. } => "Object".to_owned(),
    }
}

/// The class of the object `word` points to: its prototype, whose chain
/// chooses the view that presents it. `None` for a primitive.
fn class(reader: &Reader<'_>, word: u64) -> Option<u64> {
    let object = reader.object(word).ok()??;
    super::lookup::prototype(reader, object)
        .ok()?
        .map(|prototype| prototype.0)
}

/// The numbers a typed array keeps, which its elements are.
fn typed_array(reader: &Reader<'_>, word: u64) -> Result<Option<Arc<InterpretedArray>>, Problem> {
    let Some(object) = reader.object(word)? else {
        return Ok(None);
    };
    if !reader.is(object, crate::image::v8::Constant::JSTypedArrayType)? {
        return Ok(None);
    }
    let (element, length, data) = super::typed::extent(reader, object)?;
    let Some(kind) = number_kind(element) else {
        return Ok(None);
    };
    let name = super::typed::constructor_name(element);
    Ok(Some(Arc::new(InterpretedArray {
        data: VirtualAddress::new(data),
        shape: Arc::from([length]),
        strides: Arc::from([kind.size().cast_signed()]),
        element: ArrayElement::Number(kind),
        element_name: name.trim_end_matches("Array").to_ascii_lowercase().into(),
    })))
}

/// The number kind of a typed array's element, `None` for a 16-bit float,
/// which the debugger has no kind for.
const fn number_kind(element: inspect::ElementType) -> Option<NumberKind> {
    use inspect::ElementType as E;
    Some(match element {
        E::Int8 => NumberKind::I8,
        E::Uint8 | E::Uint8Clamped => NumberKind::U8,
        E::Int16 => NumberKind::I16,
        E::Uint16 => NumberKind::U16,
        E::Int32 => NumberKind::I32,
        E::Uint32 => NumberKind::U32,
        E::BigInt64 => NumberKind::I64,
        E::BigUint64 => NumberKind::U64,
        E::Float32 => NumberKind::F32,
        E::Float64 => NumberKind::F64,
        E::Float16 => return None,
    })
}

/// The children `range` of the value `handle` names: an array's elements
/// by index, a typed array's numbers, a Set's members, or a Map's entries;
/// then its own enumerable named properties.
pub(super) fn children(
    reader: &Reader<'_>,
    handle: InterpretedHandle,
    range: std::ops::Range<u64>,
    major: u32,
) -> Result<Vec<InterpretedChild>, Problem> {
    let Handle::Tagged(word) = Handle::of(handle) else {
        return Ok(Vec::new());
    };
    let object = reader.object(word)?.ok_or("a number has no children")?;
    let InterpretedSlot::Value(value) = self::value(reader, handle, major) else {
        return Err("the value cannot be read".into());
    };
    let elements = value.elements();
    let slot = |held: Held| -> InterpretedSlot { held_slot(reader, held, major) };
    let mut found = Vec::new();

    if range.start < elements {
        let end = range.end.min(elements);
        found.extend(element_children(
            reader,
            object,
            &value,
            range.start..end,
            major,
        )?);
    }
    if range.end > elements {
        let start = range.start.max(elements) - elements;
        let end = range.end - elements;
        let mut index = 0;
        for property in own::named(reader, object)? {
            if !property.enumerable {
                continue;
            }
            if (start..end).contains(&index) {
                found.push(InterpretedChild::Attribute {
                    name: key_text(reader, property.key, property.symbol)?.into(),
                    value: slot(property.held),
                });
            }
            index += 1;
        }
    }
    Ok(found)
}

/// The elements or entries `range` of `object`, presented as `value`.
fn element_children(
    reader: &Reader<'_>,
    object: Object,
    value: &InterpretedValue,
    range: std::ops::Range<u64>,
    major: u32,
) -> Result<Vec<InterpretedChild>, Problem> {
    use crate::image::v8::Constant;
    if let Some(array) = &value.array {
        let (_, _, kind) = array
            .contiguous()
            .ok_or("a typed array is no row of numbers")?;
        return range
            .map(|index| {
                let at = array.data.get() + index * kind.size();
                Ok(InterpretedChild::Element(typed_number(
                    reader,
                    at,
                    kind,
                    &array.element_name,
                    major,
                )))
            })
            .collect();
    }
    let ty = reader.instance_type(object)?;
    if ty == reader.constant(Constant::JSMapType)? || ty == reader.constant(Constant::JSSetType)? {
        let map = ty == reader.constant(Constant::JSMapType)?;
        let (entries, _) = super::collections::all_entries(reader, object, map, range.end)?;
        return Ok(entries
            .into_iter()
            .skip(usize::try_from(range.start).unwrap_or(usize::MAX))
            .map(|(key, entry)| {
                let key = word_slot(reader, key, major);
                match entry {
                    Some(entry) => InterpretedChild::Entry {
                        key,
                        value: word_slot(reader, entry, major),
                    },
                    None => InterpretedChild::Element(key),
                }
            })
            .collect());
    }
    if value.array.is_none() && reader.is(object, Constant::JSTypedArrayType)? {
        return Err(
            "the typed array's elements are 16-bit floats, which microscope has no kind for".into(),
        );
    }
    let start = u32::try_from(range.start).map_err(|_| "an index past an array's")?;
    let end = u32::try_from(range.end).map_err(|_| "an index past an array's")?;
    let present = own::elements(reader, object, None, end)?;
    Ok((start..end)
        .map(|index| {
            InterpretedChild::Element(
                present
                    .iter()
                    .find(|element| element.index == index)
                    .map_or(InterpretedSlot::Unbound, |element| {
                        held_slot(reader, element.held, major)
                    }),
            )
        })
        .collect())
}

/// One number of a typed array, at `at`.
fn typed_number(
    reader: &Reader<'_>,
    at: u64,
    kind: NumberKind,
    name: &str,
    major: u32,
) -> InterpretedSlot {
    let size = usize::try_from(kind.size()).unwrap_or(8);
    let mut bytes = [0; 8];
    if let Err(problem) = reader.bytes_into(at, &mut bytes[..size]) {
        return InterpretedSlot::Unreadable(problem);
    }
    let bits = u64::from_le_bytes(bytes);
    let signed = |width: u32| {
        let shift = 64 - width;
        i128::from((bits << shift).cast_signed() >> shift)
    };
    let (value, text) = match kind {
        NumberKind::I8 => (ScalarValue::Signed(signed(8)), signed(8).to_string()),
        NumberKind::I16 => (ScalarValue::Signed(signed(16)), signed(16).to_string()),
        NumberKind::I32 => (ScalarValue::Signed(signed(32)), signed(32).to_string()),
        NumberKind::I64 => (ScalarValue::Signed(signed(64)), format!("{}n", signed(64))),
        NumberKind::U8 | NumberKind::U16 | NumberKind::U32 => {
            (ScalarValue::Unsigned(u128::from(bits)), bits.to_string())
        }
        NumberKind::U64 => (ScalarValue::Unsigned(u128::from(bits)), format!("{bits}n")),
        NumberKind::F32 => {
            #[allow(
                clippy::cast_possible_truncation,
                reason = "the low 32 bits are the float"
            )]
            let float = f32::from_bits(bits as u32);
            (
                ScalarValue::Floating(FloatValue::Binary32(float.to_bits())),
                number_text(f64::from(float), major),
            )
        }
        NumberKind::F64 => (
            ScalarValue::Floating(FloatValue::Binary64(bits)),
            number_text(f64::from_bits(bits), major),
        ),
    };
    InterpretedSlot::Number(InterpretedNumber {
        address: VirtualAddress::new(at),
        value,
        element: ArrayElement::Number(kind),
        module: None,
        type_name: name.into(),
        summary: text.into(),
    })
}

/// A number as JavaScript writes it.
fn number_text(number: f64, major: u32) -> String {
    inspect::inspect(&inspect::ValueGraph::new(), &Value::Number(number), major).text
}

/// What a property or element holding `held` presents.
fn held_slot(reader: &Reader<'_>, held: Held, major: u32) -> InterpretedSlot {
    match held {
        Held::Data(word) => word_slot(reader, word, major),
        Held::Double { address, .. } => value(reader, Handle::Double(address).handle(), major),
        Held::Accessor(_) => InterpretedSlot::Unreadable(
            "the property is an accessor, which microscope does not call".into(),
        ),
    }
}

fn word_slot(reader: &Reader<'_>, word: u64, major: u32) -> InterpretedSlot {
    value(reader, Handle::Tagged(word).handle(), major)
}

/// A property's key as text: a string, or a symbol as `[Symbol(tag)]`.
fn key_text(reader: &Reader<'_>, key: Object, symbol: bool) -> Result<String, Problem> {
    if !symbol {
        return reader.string(key);
    }
    let word = reader.field(key, crate::image::v8::Field::SymbolDescription)?;
    let description = match reader.object(word)? {
        Some(text)
            if reader.instance_type(text)?
                < reader.constant(crate::image::v8::Constant::FirstNonstringType)? =>
        {
            reader.string(text)?
        }
        _ => String::new(),
    };
    Ok(format!("[Symbol({description})]"))
}
