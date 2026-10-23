//! Expressions over JavaScript values: names bound as JavaScript binds them
//! (the frame's variables, then the script contexts' top-level bindings,
//! then the global object's properties), and members, elements, lengths,
//! and operands, with no getter or proxy trap called.

use super::lookup::{self, Key};
use super::objects::{Object, Problem, Reader};
use super::own;
use super::present::Handle;
use super::variables::{self, Variable};
use crate::image::v8::{Constant, Field};
use crate::runtime_model::{
    InterpretedHandle, InterpretedKey, InterpretedKind, InterpretedOperand,
};
use crate::{TextCompletion, TextSummary};

/// The most script contexts searched.
const MAX_SCRIPTS: i64 = 1 << 16;

/// What a name means in the frame at `fp`: one of its variables, by its
/// index among them, or a top-level binding or a global's value.
pub(super) enum Bound {
    Variable(u32),
    Value(InterpretedHandle),
}

/// What `name` means in the frame at `fp`, or `None` where it means
/// nothing there.
pub(super) fn bind(reader: &Reader<'_>, fp: u64, name: &str) -> Result<Option<Bound>, Problem> {
    let found = variables::variables(reader, fp)?;
    if let Some(index) = found.iter().position(|variable| variable.name == name) {
        return Ok(Some(Bound::Variable(
            u32::try_from(index).map_err(|_| "too many variables")?,
        )));
    }
    let context = reader.word(fp.wrapping_add_signed(reader.constant(Constant::OffFpContext)?))?;
    let context = reader
        .object(context)?
        .ok_or("the frame's context is no object")?;
    let native = native_context(reader, context)?;
    if let Some(word) = script_binding(reader, native, name)? {
        return Ok(Some(Bound::Value(Handle::Tagged(word).handle())));
    }
    let global = reader
        .object(slot(reader, native, Constant::ContextExtensionIndex)?)?
        .filter(|global| {
            reader
                .is(*global, Constant::JSGlobalObjectType)
                .unwrap_or(false)
        })
        .ok_or("the native context's extension is no global object")?;
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
    fp: u64,
    index: u32,
) -> Result<InterpretedHandle, Problem> {
    let found: Vec<Variable> = variables::variables(reader, fp)?;
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

/// The native context of `object`, a context or an object's map, which its
/// own map, a context's map or its realm's meta map, keeps.
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

/// The top-level `let`, `const`, or `class` binding `name` of any script,
/// from the native context's script context table.
fn script_binding(reader: &Reader<'_>, native: Object, name: &str) -> Result<Option<u64>, Problem> {
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
    for index in 0..length.cast_unsigned() {
        let context = reader
            .object(reader.word(first + 8 * index)?)?
            .ok_or("a script context is no object")?;
        for (own, word) in variables::locals(reader, context)? {
            if own == name {
                let word = word?;
                if own::is_hole(reader, word)? {
                    return Err(format!("{name} is not yet initialized").into());
                }
                return Ok(Some(word));
            }
        }
    }
    Ok(None)
}

/// The object a handle names, refusing a number, which has no members.
fn object_of(reader: &Reader<'_>, handle: InterpretedHandle) -> Result<Option<Object>, Problem> {
    match Handle::of(handle) {
        Handle::Double(_) => Ok(None),
        Handle::Tagged(word) => reader.object(word),
    }
}

/// Whether `object` is a string.
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
        return Err(
            "a number's members are its prototype's, which microscope does not read".into(),
        );
    };
    let first = reader.constant(Constant::FirstJSReceiverType)?;
    let ty = reader.instance_type(object)?;
    if ty < first {
        return Err(
            "a primitive's members are its prototype's, which microscope does not read".into(),
        );
    }
    if reader.is(object, Constant::JSProxyType)? {
        return Err("the object is a proxy, whose traps microscope does not call".into());
    }
    if let Ok(index) = name.parse::<u32>()
        && index.to_string() == name
    {
        return element(reader, object, index);
    }
    match lookup::get(reader, object, Key::Name(name))? {
        Some(lookup::Value::Word(word)) => Ok(Some(Handle::Tagged(word).handle())),
        Some(lookup::Value::Text(_)) => {
            Err("the member is a string V8 computes, which no object holds".into())
        }
        None => Ok(None),
    }
}

/// The element `index` of `object`, an own one or a prototype's.
fn element(
    reader: &Reader<'_>,
    object: Object,
    index: u32,
) -> Result<Option<InterpretedHandle>, Problem> {
    let below = index.checked_add(1).ok_or("an index past an array's")?;
    let mut holder = object;
    for _ in 0..64 {
        let found = own::elements(reader, holder, None, below)?;
        if let Some(element) = found.iter().find(|element| element.index == index) {
            return match element.held {
                own::Held::Data(word) => Ok(Some(Handle::Tagged(word).handle())),
                own::Held::Double { address, .. } => Ok(Some(Handle::Double(address).handle())),
                own::Held::Accessor(_) => {
                    Err("the element is an accessor, which microscope does not call".into())
                }
            };
        }
        match lookup::prototype(reader, holder)? {
            Some(next) => holder = next,
            None => return Ok(None),
        }
    }
    Err("the prototypes are too many".into())
}

/// The item `key` of the value `handle` names: a member by its name or
/// an element by its index.
pub(super) fn item(
    reader: &Reader<'_>,
    handle: InterpretedHandle,
    key: &InterpretedKey,
) -> Result<Option<InterpretedHandle>, Problem> {
    match key {
        InterpretedKey::Int(index) => member(reader, handle, &index.to_string()),
        InterpretedKey::Text(text) => {
            let name = std::str::from_utf8(text).map_err(|_| "a key that is no UTF-8")?;
            member(reader, handle, name)
        }
        _ => Err("JavaScript keys are strings and indices".into()),
    }
}

/// The length of the value `handle` names: a string's, an array's, a typed
/// array's, or a Map's or Set's size.
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
            .ok_or("an array's length is no number")?;
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
    Err("the value has no length microscope reads".into())
}

/// The value `handle` names, as an operation computes with it.
pub(super) fn operand(
    reader: &Reader<'_>,
    handle: InterpretedHandle,
    major: u32,
) -> Result<InterpretedOperand, Problem> {
    let crate::runtime_model::InterpretedSlot::Value(value) =
        super::present::value(reader, handle, major)
    else {
        return Err("the value cannot be read".into());
    };
    let text = match (value.kind, Handle::of(handle)) {
        (InterpretedKind::Text, Handle::Tagged(word)) => {
            let object = reader.object(word)?.ok_or("a string is no object")?;
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
