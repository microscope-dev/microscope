//! V8's `JSReceiver::GetConstructorName`, which inspect uses as
//! `internalGetConstructorName` and call sites use for their type name.
//! Ports `GetConstructorHelper` and `JSReceiver::class_name` from
//! `objects/js-objects.cc` for each supported version.
//!
//! The name comes from the first of: the constructor function or template
//! in the map, for an object built with its constructor as `new.target`;
//! an own `Symbol.toStringTag` string on the object or a prototype; a
//! prototype's own `constructor` function; the object's kind. Empty names
//! and `Object` are skipped. Native accessors V8 would call and access
//! checks are refused.

use super::lookup::{self, Key};
use super::objects::{MAX_STRING, Object, Problem, Reader};
use super::own::Held;
use crate::image::v8::{Constant, Field};

/// Prototype chains are a few objects long; stop after this many.
const MAX_PROTOTYPES: usize = 64;

/// The most back pointers followed from a map to its root map.
const MAX_BACK_POINTERS: usize = 1 << 12;

/// V8's `JSReceiver::GetConstructorName` of `receiver`, for this version.
pub(super) fn constructor_name(reader: &Reader<'_>, receiver: Object) -> Result<Vec<u16>, Problem> {
    if let Some(name) = from_map(reader, receiver)? {
        return Ok(name);
    }

    // `PrototypeIterator` with `AdvanceIgnoringProxies`: follow map
    // prototypes, through proxies too, until null.
    let mut current = receiver;
    for _ in 0..MAX_PROTOTYPES {
        if let Some(word) = own_data(reader, current, Key::WellKnown("Symbol.toStringTag"))?
            && let Some(tag) = reader.object(word)?
            && is_string(reader, tag)?
        {
            return units(reader, tag);
        }
        // From 14.6 V8 names a hidden prototype (the global object behind a
        // global proxy) by its template. `own_data` refuses the proxy, so
        // this walk never reaches one.
        if current != receiver
            && let Some(word) = own_data(reader, current, Key::Name("constructor"))?
            && let Some(function) = reader.object(word)?
            && is_function(reader, function)?
        {
            let shared = shared(reader, function)?;
            let name = debug_name(reader, shared)?;
            if is_useful(&name) {
                return Ok(name);
            }
        }
        let prototype = reader.field(reader.map(current)?, Field::MapPrototype)?;
        match reader.object(prototype)? {
            Some(next) if !reader.is(next, Constant::OddballType)? => current = next,
            _ => return class_name(reader, receiver).map(|name| name.encode_utf16().collect()),
        }
    }
    Err(format!("{:#x} has too many prototypes", receiver.0).into())
}

/// The name of the constructor in `receiver`'s map, if the map is not a
/// prototype map and `new.target` was the constructor itself: a function's
/// debug name, or from 13.6 an API template's class name.
fn from_map(reader: &Reader<'_>, receiver: Object) -> Result<Option<Vec<u16>>, Problem> {
    if reader.is(receiver, Constant::JSProxyType)? {
        return Ok(None);
    }
    let map = reader.map(receiver)?;
    let second = i64::from(reader.byte_field(map, Field::MapBitField2)?);
    let third = i64::from(
        reader
            .int32_field(map, Field::MapBitField3)?
            .cast_unsigned(),
    );
    let flag = |bits: i64, mask, shift| {
        super::properties::part(reader, bits, mask, Some(shift)).map(|bit| bit != 0)
    };
    if !flag(
        second,
        Constant::MapNewTargetIsBaseMask,
        Constant::MapNewTargetIsBaseShift,
    )? || flag(
        third,
        Constant::MapIsPrototypeMapMask,
        Constant::MapIsPrototypeMapShift,
    )? {
        return Ok(None);
    }
    let Some(constructor) = map_constructor(reader, map)? else {
        return Ok(None);
    };
    if is_function(reader, constructor)? {
        // 14.6 uses `JSFunction::GetDebugName`; earlier versions use
        // `SharedFunctionInfo::DebugName`.
        let shared = shared(reader, constructor)?;
        let name = if reader.version.major >= 14 {
            refuse_unnamed_kinds(reader, shared)?;
            let name: Vec<u16> = super::names::debug_name(reader, constructor, shared)?
                .encode_utf16()
                .collect();
            whole(name)?
        } else {
            debug_name(reader, shared)?
        };
        return Ok(is_useful(&name).then_some(name));
    }
    if reader.version.major >= 13 && reader.is(constructor, Constant::FunctionTemplateInfoType)? {
        let word = reader.field(constructor, Field::FunctionTemplateInfoClassName)?;
        // The class name is a string or undefined.
        if let Some(name) = reader.object(word)?
            && is_string(reader, name)?
        {
            return units(reader, name).map(Some);
        }
    }
    Ok(None)
}

/// `Map::GetConstructor`: follows back pointers (maps with the same meta
/// map) to the root of `map`'s transition tree and returns what it holds.
/// `None` for a Smi.
fn map_constructor(reader: &Reader<'_>, map: Object) -> Result<Option<Object>, Problem> {
    let meta = reader.map(map)?;
    let mut word = reader.field(map, Field::MapConstructorOrBackPointerOrNativeContext)?;
    for _ in 0..MAX_BACK_POINTERS {
        let Some(held) = reader.object(word)? else {
            return Ok(None);
        };
        if reader.map(held)? != meta {
            if reader.is(held, Constant::Tuple2Type)? {
                return Err(
                    "the map stores its constructor with a non-object prototype, \
                     and microscope does not read that layout"
                        .into(),
                );
            }
            return Ok(Some(held));
        }
        word = reader.field(held, Field::MapConstructorOrBackPointerOrNativeContext)?;
    }
    Err(format!("the map {:#x} has too many back pointers", map.0).into())
}

/// The tagged value of `holder`'s own data property `key`, as
/// `JSReceiver::GetDataProperty` returns it for an own lookup that skips
/// interceptors. `None` if absent, for a proxy, or for an accessor whose
/// getter V8 does not call.
fn own_data(reader: &Reader<'_>, holder: Object, key: Key<'_>) -> Result<Option<u64>, Problem> {
    if reader.is(holder, Constant::JSProxyType)? {
        return Ok(None);
    }
    if reader.is(holder, Constant::JSGlobalProxyType)? {
        return Err(
            "a global proxy's own properties belong to its global object, \
             and microscope does not look them up there"
                .into(),
        );
    }
    let bits = i64::from(reader.byte_field(reader.map(holder)?, Field::MapBitField)?);
    if super::properties::part(
        reader,
        bits,
        Constant::MapIsAccessCheckNeededMask,
        Some(Constant::MapIsAccessCheckNeededShift),
    )? != 0
    {
        return Err("the object's properties are behind an access check".into());
    }
    match lookup::own(reader, holder, key)? {
        Some((Held::Data(word), _)) => Ok(Some(word)),
        // A double is neither a string nor a function.
        Some((Held::Double { .. }, _)) | None => Ok(None),
        Some((Held::Accessor(accessor), _)) => {
            // From 14.6 V8 calls a side-effect-free native accessor.
            if reader.version.major >= 14 && reader.is(accessor, Constant::AccessorInfoType)? {
                return Err(
                    "the property is a V8 native accessor, which V8 would call and microscope \
                     cannot"
                        .into(),
                );
            }
            Ok(None)
        }
    }
}

/// `SharedFunctionInfo::DebugName`: its own name, else the one V8 inferred.
fn debug_name(reader: &Reader<'_>, shared: Object) -> Result<Vec<u16>, Problem> {
    refuse_unnamed_kinds(reader, shared)?;
    if let (Some(name), _) = super::names::shared_name(reader, shared)? {
        let name = units(reader, name)?;
        if !name.is_empty() {
            return Ok(name);
        }
    }
    whole(
        super::names::function_name(reader, shared)?
            .encode_utf16()
            .collect(),
    )
}

/// Refuses functions `DebugName` names some other way: class field
/// initializers (`<instance_members_initializer>` or
/// `<static_initializer>`, never reachable as constructors) and
/// WebAssembly exports, named by their signature.
fn refuse_unnamed_kinds(reader: &Reader<'_>, shared: Object) -> Result<(), Problem> {
    if let Some(data) = super::names::function_data(reader, shared)?
        && reader.is(data, Constant::WasmExportedFunctionDataType)?
    {
        return Err("microscope does not name WebAssembly functions".into());
    }
    let flags = i64::from(reader.int32_field(shared, Field::SharedFunctionInfoFlags)?);
    let kind = super::properties::part(
        reader,
        flags,
        Constant::SfiFunctionKindMask,
        Some(Constant::SfiFunctionKindShift),
    )?;
    // The last initializer kind is the static one preceded by a member from
    // 14.6, and the plain static one before.
    let first = reader.constant(Constant::FunctionKindClassMembersInitializerFunction)?;
    let last = reader
        .constant(Constant::FunctionKindClassStaticInitializerFunctionPrecededByMember)
        .or_else(|_| reader.constant(Constant::FunctionKindClassStaticInitializerFunction))?;
    if (first..=last).contains(&kind) {
        return Err("microscope does not name class field initializers".into());
    }
    Ok(())
}

/// `JSReceiver::class_name`: the name of the kind of object `receiver` is.
fn class_name(reader: &Reader<'_>, receiver: Object) -> Result<&'static str, Problem> {
    use Constant as C;
    let ty = reader.instance_type(receiver)?;
    let is = |constant| reader.constant(constant).ok() == Some(ty);
    let within = |first, last| -> Result<bool, Problem> {
        Ok((reader.constant(first)?..=reader.constant(last)?).contains(&ty))
    };
    if within(
        C::FirstJSFunctionOrBoundFunctionOrWrappedFunctionType,
        C::LastJSFunctionOrBoundFunctionOrWrappedFunctionType,
    )? {
        return Ok("Function");
    }
    let simple = [
        (C::JSArgumentsObjectType, "Arguments"),
        (C::JSArrayType, "Array"),
        (C::JSArrayIteratorType, "ArrayIterator"),
        (C::JSDateType, "Date"),
        (C::JSErrorType, "Error"),
        (C::JSMapType, "Map"),
        (C::JSRegExpType, "RegExp"),
        (C::JSSetType, "Set"),
        (C::JSWeakMapType, "WeakMap"),
        (C::JSWeakSetType, "WeakSet"),
        (C::JSSharedStructType, "SharedStruct"),
        (C::JSSharedArrayType, "SharedArray"),
        (C::JSAtomicsMutexType, "AtomicsMutex"),
        (C::JSAtomicsConditionType, "AtomicsCondition"),
    ];
    if let Some((_, name)) = simple.iter().find(|(constant, _)| is(*constant)) {
        return Ok(*name);
    }
    if is(C::JSArrayBufferType) {
        let flags = i64::from(
            reader
                .int32_field(receiver, Field::JSArrayBufferBitField)?
                .cast_unsigned(),
        );
        let shared = super::properties::part(
            reader,
            flags,
            C::JSArrayBufferIsSharedMask,
            Some(C::JSArrayBufferIsSharedShift),
        )? != 0;
        return Ok(if shared {
            "SharedArrayBuffer"
        } else {
            "ArrayBuffer"
        });
    }
    if within(C::FirstJSGeneratorObjectType, C::LastJSGeneratorObjectType)? {
        return Ok("Generator");
    }
    if within(C::FirstJSMapIteratorType, C::LastJSMapIteratorType)? {
        return Ok("MapIterator");
    }
    if within(C::FirstJSSetIteratorType, C::LastJSSetIteratorType)? {
        return Ok("SetIterator");
    }
    if is(C::JSProxyType) {
        return Ok(if lookup::is_callable(reader, receiver)? {
            "Function"
        } else {
            "Object"
        });
    }
    if is(C::JSTypedArrayType) {
        // `TYPED_ARRAYS` leaves out the kinds for views of resizable
        // buffers, so such an array is named `Object`.
        let kind = super::own::elements_kind(reader, reader.map(receiver)?)?;
        let kinds = [
            (C::ElementsKindUint8, "Uint8Array"),
            (C::ElementsKindInt8, "Int8Array"),
            (C::ElementsKindUint16, "Uint16Array"),
            (C::ElementsKindInt16, "Int16Array"),
            (C::ElementsKindUint32, "Uint32Array"),
            (C::ElementsKindInt32, "Int32Array"),
            (C::ElementsKindBigUint64, "BigUint64Array"),
            (C::ElementsKindBigInt64, "BigInt64Array"),
            (C::ElementsKindUint8Clamped, "Uint8ClampedArray"),
            (C::ElementsKindFloat32, "Float32Array"),
            (C::ElementsKindFloat64, "Float64Array"),
            (C::ElementsKindFloat16, "Float16Array"),
        ];
        if let Some((_, name)) = kinds
            .iter()
            .find(|(constant, _)| reader.constant(*constant).ok() == Some(kind))
        {
            return Ok(*name);
        }
        return Ok("Object");
    }
    if is(C::JSPrimitiveWrapperType) {
        return wrapped(reader, receiver);
    }
    // 14.6 dropped the global proxy's name.
    if reader.version.major < 14 && is(C::JSGlobalProxyType) {
        return Ok("global");
    }
    Ok("Object")
}

/// The name of the primitive type that `wrapper` holds.
fn wrapped(reader: &Reader<'_>, wrapper: Object) -> Result<&'static str, Problem> {
    use Constant as C;
    let value = reader.field(wrapper, Field::JSPrimitiveWrapperValue)?;
    let Some(value) = reader.object(value)? else {
        return Ok("Number");
    };
    if is_string(reader, value)? {
        return Ok("String");
    }
    if reader.is(value, C::OddballType)? {
        let kind = reader.smi_field(value, Field::OddballKind)?;
        if [C::OddballTrue, C::OddballFalse]
            .iter()
            .any(|constant| reader.constant(*constant).ok() == Some(kind))
        {
            return Ok("Boolean");
        }
    }
    let kinds = [
        (C::HeapNumberType, "Number"),
        (C::BigIntBaseType, "BigInt"),
        (C::SymbolType, "Symbol"),
        (C::ScriptType, "Script"),
    ];
    for (constant, name) in kinds {
        if reader.is(value, constant)? {
            return Ok(name);
        }
    }
    Err(format!("a primitive wrapper holds a non-primitive {:#x}", value.0).into())
}

/// Whether V8 accepts `name` as a constructor name: neither empty nor
/// `Object`.
fn is_useful(name: &[u16]) -> bool {
    !name.is_empty() && !name.iter().copied().eq("Object".encode_utf16())
}

/// Whether `object` is a `JSFunction`. Bound functions are not.
fn is_function(reader: &Reader<'_>, object: Object) -> Result<bool, Problem> {
    let ty = reader.instance_type(object)?;
    Ok((reader.constant(Constant::FirstJSFunctionType)?
        ..=reader.constant(Constant::LastJSFunctionType)?)
        .contains(&ty))
}

fn is_string(reader: &Reader<'_>, object: Object) -> Result<bool, Problem> {
    Ok(reader.instance_type(object)? < reader.constant(Constant::FirstNonstringType)?)
}

fn shared(reader: &Reader<'_>, function: Object) -> Result<Object, Problem> {
    reader.typed_field(
        function,
        Field::JSFunctionSharedFunctionInfo,
        Constant::SharedFunctionInfoType,
    )
}

/// Every unit of the string `string`.
fn units(reader: &Reader<'_>, string: Object) -> Result<Vec<u16>, Problem> {
    let mut text = Vec::new();
    reader.string_into(string, 0, u64::from(u32::MAX), 0, &mut text)?;
    Ok(text)
}

/// `name`, refused if it may have been cut off: [`Reader::string`] reads
/// at most [`MAX_STRING`] characters.
fn whole(name: Vec<u16>) -> Result<Vec<u16>, Problem> {
    if u64::try_from(name.len()).is_ok_and(|length| length < MAX_STRING) {
        return Ok(name);
    }
    Err("a function name too long to read in full".into())
}
