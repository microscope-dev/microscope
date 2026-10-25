//! JavaScript values for the debugger: summaries as `util.inspect` prints
//! them, with children read on demand.
//!
//! A handle ([`InterpretedHandle`]) is a tagged word (a Smi or heap object
//! pointer), or for a double array element, its address with low bits
//! `0b10`, which no tagged word has.

use std::sync::Arc;

use super::inspect::{self, Kind as Shape, Layout, Value};
use super::objects::{Object, Problem, Reader};
use super::own::{self, Held};
use super::values::Builder;
use crate::image::v8::NodeRelease;
use crate::runtime_model::{
    ArrayElement, InterpretedArray, InterpretedChild, InterpretedHandle, InterpretedKind,
    InterpretedNumber, InterpretedSlot, InterpretedValue,
};
use crate::{FloatValue, NumberKind, ScalarValue, VirtualAddress};

/// The Node release whose `util.inspect` output to match. If the module
/// doesn't name it, this is its V8 line's first release and `assumed` says so.
#[derive(Debug, Clone)]
pub(super) struct Writer {
    pub release: NodeRelease,
    pub assumed: Option<Arc<str>>,
}

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

/// The address of a heap object or double element, or `None` for a Smi.
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

/// The value `handle` names, formatted for `writer`.
pub(super) fn value(
    reader: &Reader<'_>,
    handle: InterpretedHandle,
    writer: &Writer,
) -> InterpretedSlot {
    match presented(reader, handle, writer) {
        Ok(Some(value)) => InterpretedSlot::Value(value),
        Ok(None) => InterpretedSlot::Unbound,
        Err(problem) => InterpretedSlot::Unreadable(problem),
    }
}

/// The value `handle` names, or `None` for the hole (an uninitialized
/// variable).
fn presented(
    reader: &Reader<'_>,
    handle: InterpretedHandle,
    writer: &Writer,
) -> Result<Option<InterpretedValue>, Problem> {
    let word = match Handle::of(handle) {
        Handle::Double(address) => {
            let number = f64::from_bits(reader.word(address)?);
            return Ok(Some(number_value(handle, Some(address), number, writer)));
        }
        Handle::Tagged(word) => word,
    };
    if own::is_hole(reader, word)? {
        return Ok(None);
    }
    let mut builder = Builder::new(*reader);
    let root = builder.value(word, 0);
    let (summary, detail, problem) = texts(&builder.graph, &root, writer);
    let at = address(reader, handle);

    let (type_name, kind, attributes) = match &root {
        Value::Undefined => ("undefined".into(), InterpretedKind::Undefined, 0),
        Value::Null => ("null".into(), InterpretedKind::Nothing, 0),
        Value::Boolean(value) => ("boolean".into(), InterpretedKind::Bool(*value), 0),
        // Every Number is a double, a small integer included.
        Value::Number(number) => ("number".into(), InterpretedKind::Float(*number), 0),
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
        detail,
        problem,
        part: Arc::from([]),
        array,
    }))
}

/// The summary of `root`, its full text if longer, and any reason the
/// text may not match Node's.
fn texts(
    graph: &inspect::ValueGraph,
    root: &Value,
    writer: &Writer,
) -> (Arc<str>, Option<Arc<str>>, Option<Arc<str>>) {
    let text = inspect::inspect(graph, root, writer.release, Layout::Default);
    let line = inspect::inspect(graph, root, writer.release, Layout::Line).text;
    let summary: Arc<str> = summary_line(graph, root, &line, &text.text).into();
    let detail = (*summary != text.text).then(|| text.text.into());
    let problem = match (&writer.assumed, text.approximate) {
        (Some(assumed), _) => Some(Arc::clone(assumed)),
        (None, true) => Some(Arc::from("spacing may differ from Node's output")),
        (None, false) => None,
    };
    (summary, detail, problem)
}

/// The summary line. For an error, the first line of its stack in `text`
/// (name and message); otherwise the single-line `line`, cut with `…`
/// before any error stack it holds.
fn summary_line(graph: &inspect::ValueGraph, root: &Value, line: &str, text: &str) -> String {
    let Some((first, _)) = line.split_once('\n') else {
        return line.to_owned();
    };
    let error = match root {
        Value::Object(id) => graph
            .row(*id)
            .is_some_and(|object| matches!(object.kind, Shape::Error(_))),
        _ => false,
    };
    if error {
        text.lines().next().unwrap_or_default().to_owned()
    } else {
        format!("{first} …")
    }
}

/// The value of a number stored in a double array element.
fn number_value(
    handle: InterpretedHandle,
    address: Option<u64>,
    number: f64,
    writer: &Writer,
) -> InterpretedValue {
    let graph = inspect::ValueGraph::new();
    let text = inspect::inspect(
        &graph,
        &Value::Number(number),
        writer.release,
        Layout::Default,
    )
    .text;
    InterpretedValue {
        object: handle,
        address: address.map(VirtualAddress::new),
        class: InterpretedHandle::new(0),
        module: None,
        type_name: "number".into(),
        kind: InterpretedKind::Float(number),
        attributes: 0,
        summary: text.into(),
        detail: None,
        problem: None,
        part: Arc::from([]),
        array: None,
    }
}

/// A `BigInt`'s value where it fits in 128 bits, from its decimal text.
fn bigint_value(summary: &str) -> Option<i128> {
    summary.strip_suffix('n')?.parse().ok()
}

/// The type name for `constructor`.
fn constructor_text(constructor: &inspect::Constructor) -> String {
    match constructor {
        inspect::Constructor::Named(name) => String::from_utf16_lossy(name.units()),
        inspect::Constructor::NullPrototype { .. } => "[Object: null prototype]".to_owned(),
        inspect::Constructor::Unnamed { .. } => "Object".to_owned(),
    }
}

/// The prototype of the object `word` points to, used to pick its view.
/// `None` for a primitive.
fn class(reader: &Reader<'_>, word: u64) -> Option<u64> {
    let object = reader.object(word).ok()??;
    super::lookup::prototype(reader, object)
        .ok()?
        .map(|prototype| prototype.0)
}

/// The numeric elements of a typed array.
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

/// The number kind of a typed array's element, or `None` for 16-bit floats,
/// which `NumberKind` lacks.
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

/// The children in `range`: array or typed array elements, Set members, or
/// Map entries, then own enumerable named properties.
pub(super) fn children(
    reader: &Reader<'_>,
    handle: InterpretedHandle,
    range: std::ops::Range<u64>,
    writer: &Writer,
) -> Result<Vec<InterpretedChild>, Problem> {
    let Handle::Tagged(word) = Handle::of(handle) else {
        return Ok(Vec::new());
    };
    let object = reader.object(word)?.ok_or("a number has no children")?;
    let InterpretedSlot::Value(value) = self::value(reader, handle, writer) else {
        return Err("the value cannot be read".into());
    };
    let elements = value.elements();
    let slot = |held: Held| -> InterpretedSlot { held_slot(reader, held, writer) };
    let mut found = Vec::new();

    if range.start < elements {
        let end = range.end.min(elements);
        found.extend(element_children(
            reader,
            object,
            &value,
            range.start..end,
            writer,
        )?);
    }
    if range.end > elements {
        let start = range.start.max(elements) - elements;
        let end = range.end - elements;
        let mut index = 0;
        let indexed = reader.is(object, crate::image::v8::Constant::JSArrayType)?
            || reader.is(object, crate::image::v8::Constant::JSTypedArrayType)?;
        if !indexed {
            for element in own::elements(reader, object, 0..u32::MAX)? {
                if !element.enumerable {
                    continue;
                }
                if (start..end).contains(&index) {
                    found.push(InterpretedChild::Attribute {
                        name: element.index.to_string().into(),
                        value: slot(element.held),
                    });
                }
                index += 1;
            }
        }
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

/// The elements or entries of `object` in `range`.
fn element_children(
    reader: &Reader<'_>,
    object: Object,
    value: &InterpretedValue,
    range: std::ops::Range<u64>,
    writer: &Writer,
) -> Result<Vec<InterpretedChild>, Problem> {
    use crate::image::v8::Constant;
    if let Some(array) = &value.array {
        let (_, _, kind) = array
            .contiguous()
            .ok_or("the typed array is not a contiguous row of numbers")?;
        return range
            .map(|index| {
                let at = array.data.get() + index * kind.size();
                Ok(InterpretedChild::Element(typed_number(
                    reader,
                    at,
                    kind,
                    &array.element_name,
                    writer,
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
                let key = word_slot(reader, key, writer);
                match entry {
                    Some(entry) => InterpretedChild::Entry {
                        key,
                        value: word_slot(reader, entry, writer),
                    },
                    None => InterpretedChild::Element(key),
                }
            })
            .collect());
    }
    if value.array.is_none() && reader.is(object, Constant::JSTypedArrayType)? {
        return Err("microscope can't show 16-bit float typed array elements".into());
    }
    let start = u32::try_from(range.start).map_err(|_| "the index is past the end of the array")?;
    let end = u32::try_from(range.end).map_err(|_| "the index is past the end of the array")?;
    let present = own::elements(reader, object, start..end)?;
    Ok((start..end)
        .map(|index| {
            InterpretedChild::Element(
                present
                    .iter()
                    .find(|element| element.index == index)
                    .map_or(InterpretedSlot::Unbound, |element| {
                        held_slot(reader, element.held, writer)
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
    writer: &Writer,
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
                number_text(f64::from(float), writer),
            )
        }
        NumberKind::F64 => (
            ScalarValue::Floating(FloatValue::Binary64(bits)),
            number_text(f64::from_bits(bits), writer),
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
fn number_text(number: f64, writer: &Writer) -> String {
    inspect::inspect(
        &inspect::ValueGraph::new(),
        &Value::Number(number),
        writer.release,
        Layout::Default,
    )
    .text
}

/// The slot shown for a property or element holding `held`.
fn held_slot(reader: &Reader<'_>, held: Held, writer: &Writer) -> InterpretedSlot {
    match held {
        Held::Data(word) => word_slot(reader, word, writer),
        Held::Double { address, .. } => value(reader, Handle::Double(address).handle(), writer),
        Held::Accessor(_) => {
            InterpretedSlot::Unreadable("microscope does not call accessor properties".into())
        }
    }
}

fn word_slot(reader: &Reader<'_>, word: u64, writer: &Writer) -> InterpretedSlot {
    value(reader, Handle::Tagged(word).handle(), writer)
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
