//! Expressions over JavaScript values: name lookup in JavaScript's order
//! (frame variables, script top-level bindings, then global properties),
//! members, elements, lengths, and operands. Getters and proxy traps are
//! never called.

use super::lookup::{self, Key};
use super::objects::{Object, Problem, Reader};
use super::own;
use super::present::{Handle, Writer};
use super::variables::{self, Variable};
use crate::image::v8::{Constant, Field};
use crate::runtime_model::{
    InterpretedFrameRef, InterpretedHandle, InterpretedKey, InterpretedKind, InterpretedOperand,
};
use crate::{TextCompletion, TextSummary};

/// The most script contexts searched.
const MAX_SCRIPTS: i64 = 1 << 16;

/// What a name resolves to: a frame variable by index, or a global's value.
pub(super) enum Bound {
    Variable(u32),
    Value(InterpretedHandle),
}

/// What `name` resolves to in the frame at `fp`, or `None` if unbound.
pub(super) fn bind(
    reader: &Reader<'_>,
    frame: InterpretedFrameRef,
    name: &str,
) -> Result<Option<Bound>, Problem> {
    let found = variables::variables(reader, frame)?;
    if let Some(index) = found.iter().position(|variable| variable.name == name) {
        return Ok(Some(Bound::Variable(
            u32::try_from(index).map_err(|_| "too many variables")?,
        )));
    }
    let context = reader.word(
        frame
            .address
            .get()
            .wrapping_add_signed(reader.constant(Constant::OffFpContext)?),
    )?;
    let context = reader
        .object(context)?
        .ok_or("the frame's context is not an object")?;
    let native = native_context(reader, context)?;
    let global = reader
        .object(slot(reader, native, Constant::ContextExtensionIndex)?)?
        .filter(|global| {
            reader
                .is(*global, Constant::JSGlobalObjectType)
                .unwrap_or(false)
        })
        .ok_or("the native context's extension is not a global object")?;
    match lookup::find(reader, global, Key::Name(name))? {
        lookup::Found::Data(word) => Ok(Some(Bound::Value(Handle::Tagged(word).handle()))),
        lookup::Found::Absent => Ok(None),
        lookup::Found::Accessor { .. } => {
            Err(format!("the global {name} is a getter, which microscope does not call").into())
        }
    }
}

/// The handle of the frame's variable at `index`.
pub(super) fn variable(
    reader: &Reader<'_>,
    frame: InterpretedFrameRef,
    index: u32,
) -> Result<InterpretedHandle, Problem> {
    let found: Vec<Variable> = variables::variables(reader, frame)?;
    let variable = found
        .into_iter()
        .nth(usize::try_from(index).unwrap_or(usize::MAX))
        .ok_or("the frame no longer has the variable")?;
    let word = variable.word?;
    if own::is_hole(reader, word)? {
        return Err(format!("{} is not yet initialized", variable.name).into());
    }
    Ok(Handle::Tagged(word).handle())
}

/// The native context of `object` (a context or a map), read from the
/// native context field of `object`'s map.
pub(super) fn native_context(reader: &Reader<'_>, object: Object) -> Result<Object, Problem> {
    let map = reader.map(object)?;
    reader.typed_field(
        map,
        Field::MapConstructorOrBackPointerOrNativeContext,
        Constant::NativeContextType,
    )
}

pub(super) fn slot(reader: &Reader<'_>, context: Object, index: Constant) -> Result<u64, Problem> {
    let index = u64::try_from(reader.constant(index)?).map_err(|_| "a negative context slot")?;
    reader.word(reader.field_address(context, Field::ContextElements)? + 8 * index)
}

/// The script contexts in `native`'s script context table, in the order
/// the scripts ran.
pub(super) fn script_contexts(reader: &Reader<'_>, native: Object) -> Result<Vec<Object>, Problem> {
    let table = reader
        .object(slot(
            reader,
            native,
            Constant::ContextScriptContextTableIndex,
        )?)?
        .filter(|table| {
            reader
                .is(*table, Constant::ScriptContextTableType)
                .unwrap_or(false)
        })
        .ok_or("the native context keeps no script context table")?;
    let length = reader.smi_field(table, Field::ScriptContextTableLength)?;
    if !(0..MAX_SCRIPTS).contains(&length) {
        return Err(format!("the script context table holds {length} contexts").into());
    }
    let first = reader.field_address(table, Field::ScriptContextTableObjects)?;
    (0..length.cast_unsigned())
        .map(|index| {
            reader
                .object(reader.word(first + 8 * index)?)?
                .ok_or_else(|| "a script context is not an object".into())
        })
        .collect()
}

/// The object a handle names, or `None` for a double element.
fn object_of(reader: &Reader<'_>, handle: InterpretedHandle) -> Result<Option<Object>, Problem> {
    match Handle::of(handle) {
        Handle::Double(_) => Ok(None),
        Handle::Tagged(word) => reader.object(word),
    }
}

fn is_string(reader: &Reader<'_>, object: Object) -> Result<bool, Problem> {
    Ok(reader.instance_type(object)? < reader.constant(Constant::FirstNonstringType)?)
}

/// The member `name` of the value `handle` names.
pub(super) fn member(
    reader: &Reader<'_>,
    handle: InterpretedHandle,
    name: &str,
) -> Result<Option<InterpretedHandle>, Problem> {
    let Some(object) = object_of(reader, handle)? else {
        return Err("microscope does not read members of numbers".into());
    };
    let first = reader.constant(Constant::FirstJSReceiverType)?;
    let ty = reader.instance_type(object)?;
    if ty < first {
        if name == "length" && is_string(reader, object)? {
            return number(
                reader,
                i64::try_from(length(reader, handle)?).unwrap_or(i64::MAX),
            )
            .map(Some);
        }
        return Err("microscope does not read members of primitives".into());
    }
    if reader.is(object, Constant::JSProxyType)? {
        return Err("the object is a proxy, and microscope does not call proxy traps".into());
    }
    if let Ok(index) = name.parse::<u32>()
        && index.to_string() == name
    {
        return element(reader, object, index);
    }
    if let Some(found) = built_in_getter(reader, handle, object, name)? {
        return Ok(Some(found));
    }
    match lookup::get(reader, object, Key::Name(name))? {
        Some(lookup::Value::Word(word)) => Ok(Some(Handle::Tagged(word).handle())),
        Some(lookup::Value::Text(_)) => {
            Err("V8 computes this member on demand, so no object holds it".into())
        }
        None => Ok(None),
    }
}

/// The element `index` of `object` or of its prototype chain.
fn element(
    reader: &Reader<'_>,
    object: Object,
    index: u32,
) -> Result<Option<InterpretedHandle>, Problem> {
    if reader.is(object, Constant::JSTypedArrayType)? {
        return typed_element(reader, object, index);
    }
    let below = index
        .checked_add(1)
        .ok_or("the index is past the end of the array")?;
    let mut holder = object;
    for _ in 0..64 {
        let found = own::elements(reader, holder, None, below)?;
        if let Some(element) = found.iter().find(|element| element.index == index) {
            return match element.held {
                own::Held::Data(word) => Ok(Some(Handle::Tagged(word).handle())),
                own::Held::Double { address, .. } => Ok(Some(Handle::Double(address).handle())),
                own::Held::Accessor(_) => Err("microscope does not call accessor elements".into()),
            };
        }
        match lookup::prototype(reader, holder)? {
            Some(next) => holder = next,
            None => return Ok(None),
        }
    }
    Err("the prototype chain is too long".into())
}

/// `.size` of a Map or Set and `.length` of a typed array, where the
/// getter that would run is V8's own, read from the fields it reads.
fn built_in_getter(
    reader: &Reader<'_>,
    handle: InterpretedHandle,
    object: Object,
    name: &str,
) -> Result<Option<InterpretedHandle>, Problem> {
    let (getter_name, kinds): (&str, &[Constant]) = match name {
        "size" => ("get size", &[Constant::JSMapType, Constant::JSSetType]),
        "length" => ("get length", &[Constant::JSTypedArrayType]),
        _ => return Ok(None),
    };
    let lookup::Found::Accessor { accessor, .. } = lookup::find(reader, object, Key::Name(name))?
    else {
        return Ok(None);
    };
    if !reader.is(accessor, Constant::AccessorPairType)? {
        return Ok(None);
    }
    let Some(getter) = reader.object(reader.field(accessor, Field::AccessorPairGetter)?)? else {
        return Ok(None);
    };
    if !lookup::is_builtin(reader, getter, getter_name)? {
        return Ok(None);
    }
    for kind in kinds {
        if reader.is(object, *kind)? {
            return number(
                reader,
                i64::try_from(length(reader, handle)?).unwrap_or(i64::MAX),
            )
            .map(Some);
        }
    }
    Err(format!("V8's `{getter_name}` would throw for this object").into())
}

/// A number as a Smi handle.
fn number(reader: &Reader<'_>, value: i64) -> Result<InterpretedHandle, Problem> {
    reader
        .smi_word(value)?
        .map(|word| Handle::Tagged(word).handle())
        .ok_or_else(|| {
            format!("{value} is too large for a Smi, and microscope does not make heap numbers")
                .into()
        })
}

/// The element `index` of a typed array, or `None` past its length, as an
/// integer-indexed object answers without looking at its prototypes.
fn typed_element(
    reader: &Reader<'_>,
    object: Object,
    index: u32,
) -> Result<Option<InterpretedHandle>, Problem> {
    use super::inspect::ElementType as E;
    let (element, length, data) = super::typed::extent(reader, object)?;
    if u64::from(index) >= length {
        return Ok(None);
    }
    let size = super::typed::size(element);
    let address = data + u64::from(index) * size;
    if element == E::Float64 {
        return Ok(Some(Handle::Double(address).handle()));
    }
    let mut bytes = [0u8; 8];
    reader.bytes_into(address, &mut bytes[..usize::try_from(size).unwrap_or(8)])?;
    let [b0, b1, b2, b3, ..] = bytes;
    let integer = match element {
        E::Int8 => i64::from(i8::from_le_bytes([b0])),
        E::Uint8 | E::Uint8Clamped => i64::from(b0),
        E::Int16 => i64::from(i16::from_le_bytes([b0, b1])),
        E::Uint16 => i64::from(u16::from_le_bytes([b0, b1])),
        E::Int32 => i64::from(i32::from_le_bytes([b0, b1, b2, b3])),
        E::Uint32 => i64::from(u32::from_le_bytes([b0, b1, b2, b3])),
        _ => {
            return Err(format!(
                "microscope can't use an element of a {} as a value yet",
                element.name()
            )
            .into());
        }
    };
    number(reader, integer).map(Some)
}

/// The item `key` of the value `handle` names, by name or index.
pub(super) fn item(
    reader: &Reader<'_>,
    handle: InterpretedHandle,
    key: &InterpretedKey,
) -> Result<Option<InterpretedHandle>, Problem> {
    match key {
        InterpretedKey::Int(index) => member(reader, handle, &index.to_string()),
        InterpretedKey::Text(text) => {
            let name = std::str::from_utf8(text).map_err(|_| "the key is not UTF-8")?;
            member(reader, handle, name)
        }
        _ => Err("JavaScript keys must be strings or indices".into()),
    }
}

/// The length of a string, array, or typed array, or the size of a Map or
/// Set.
pub(super) fn length(reader: &Reader<'_>, handle: InterpretedHandle) -> Result<u64, Problem> {
    let object = object_of(reader, handle)?.ok_or("a number has no length")?;
    if is_string(reader, object)? {
        return u64::try_from(reader.int32_field(object, Field::StringLength)?)
            .map_err(|_| "a negative length".into());
    }
    let ty = reader.instance_type(object)?;
    let is = |constant| reader.constant(constant).ok() == Some(ty);
    if is(Constant::JSArrayType) {
        let word = reader.field(object, Field::JSArrayLength)?;
        if let Some(length) = reader.smi(word)? {
            return u64::try_from(length).map_err(|_| "a negative length".into());
        }
        let number = reader
            .object(word)?
            .ok_or("an array's length is not a number")?;
        let value =
            f64::from_bits(reader.word(reader.field_address(number, Field::HeapNumberValue)?)?);
        #[allow(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "an array's length is below 2^32"
        )]
        return Ok(value as u64);
    }
    if is(Constant::JSTypedArrayType) {
        return super::typed::extent(reader, object).map(|(_, length, _)| length);
    }
    if is(Constant::JSMapType) || is(Constant::JSSetType) {
        let (_, size) =
            super::collections::all_entries(reader, object, is(Constant::JSMapType), 0)?;
        return u64::try_from(size).map_err(|_| "a negative size".into());
    }
    Err("microscope can't read a length for this value".into())
}

/// The value `handle` names, as an operator uses it.
pub(super) fn operand(
    reader: &Reader<'_>,
    handle: InterpretedHandle,
    writer: &Writer,
) -> Result<InterpretedOperand, Problem> {
    let crate::runtime_model::InterpretedSlot::Value(value) =
        super::present::value(reader, handle, writer)
    else {
        return Err("the value cannot be read".into());
    };
    let text = match (value.kind, Handle::of(handle)) {
        (InterpretedKind::Text, Handle::Tagged(word)) => {
            let object = reader.object(word)?.ok_or("a string is not an object")?;
            let length = u64::try_from(reader.int32_field(object, Field::StringLength)?)
                .map_err(|_| "a negative length")?;
            let mut units = Vec::new();
            let limit = u64::try_from(TextSummary::MAX_BYTES).unwrap_or(256);
            reader.string_into(object, 0, limit, 0, &mut units)?;
            let raw: Vec<u8> = units.iter().flat_map(|unit| unit.to_le_bytes()).collect();
            let completion = if length <= limit {
                TextCompletion::Complete
            } else {
                TextCompletion::Truncated { length: None }
            };
            // UTF-8 cannot hold a lone surrogate, so comparing it would be
            // wrong. A cut may split a pair at the end.
            let whole = match completion {
                TextCompletion::Complete => units.as_slice(),
                _ => &units[..units.len().saturating_sub(1)],
            };
            if char::decode_utf16(whole.iter().copied()).any(|unit| unit.is_err()) {
                return Err(
                    "microscope can't compute with a string that holds a lone surrogate".into(),
                );
            }
            Some(TextSummary::from_units(
                &raw,
                2,
                crate::ByteOrder::Little,
                completion,
            ))
        }
        _ => None,
    };
    Ok(InterpretedOperand {
        type_name: value.type_name,
        kind: value.kind,
        text,
    })
}
