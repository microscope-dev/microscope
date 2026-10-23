//! JavaScript property lookups through the prototype chain, without calling
//! getters. Program getters are refused. V8 native accessors
//! (`AccessorInfo`) and builtin getters are evaluated only where this
//! module knows what they compute.
//!
//! A builtin function is identified exactly: it has no script, its shared
//! info holds the builtin's number as a Smi where a program function holds
//! bytecode, and V8 gave it its name, as in `[Symbol.hasInstance]`. No
//! program can create such a function.

use super::objects::{Object, Problem, Reader};
use super::own::{self, Held};
use crate::image::v8::{Constant, Field};

/// Prototype chains are a few objects long; stop after this many.
const MAX_PROTOTYPES: usize = 64;

/// A property key for a lookup.
#[derive(Debug, Clone, Copy)]
pub(super) enum Key<'k> {
    /// A string key.
    Name(&'k str),

    /// A well-known symbol by its description, such as
    /// `Symbol.toStringTag`.
    WellKnown(&'k str),
}

/// What a lookup found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Found {
    /// A data property's tagged value.
    Data(u64),

    /// An `AccessorPair` or `AccessorInfo`, and the object that holds it.
    Accessor {
        accessor: Object,
        holder: Object,
    },
    Absent,
}

/// Whether the named property `property` has the key `key`.
pub(super) fn matches(
    reader: &Reader<'_>,
    property: &own::Named,
    key: Key<'_>,
) -> Result<bool, Problem> {
    match key {
        Key::Name(name) => {
            Ok(!property.symbol && super::properties::is_named(reader, property.key.0, name)?)
        }
        Key::WellKnown(description) => {
            if !property.symbol || !super::values::is_well_known(reader, property.key)? {
                return Ok(false);
            }
            let word = reader.field(property.key, Field::SymbolDescription)?;
            super::properties::is_named(reader, word, description)
        }
    }
}

/// The own property `key` of `holder`, and whether it is enumerable.
pub(super) fn own(
    reader: &Reader<'_>,
    holder: Object,
    key: Key<'_>,
) -> Result<Option<(Held, bool)>, Problem> {
    for property in own::named(reader, holder)? {
        if matches(reader, &property, key)? {
            return Ok(Some((property.held, property.enumerable)));
        }
    }
    Ok(None)
}

/// The prototype of `object`, `None` for `null`. Refuses proxies, whose
/// handler may compute it.
pub(super) fn prototype(reader: &Reader<'_>, object: Object) -> Result<Option<Object>, Problem> {
    if reader.is(object, Constant::JSProxyType)? {
        return Err(
            "a proxy's prototype comes from its handler, which microscope cannot run".into(),
        );
    }
    let map = reader.map(object)?;
    let word = reader.field(map, Field::MapPrototype)?;
    match reader.object(word)? {
        Some(prototype) if !reader.is(prototype, Constant::OddballType)? => Ok(Some(prototype)),
        _ => Ok(None),
    }
}

/// The property `key` on `object` or its prototypes, as `[[Get]]` finds it
/// before calling any getter.
pub(super) fn find(reader: &Reader<'_>, object: Object, key: Key<'_>) -> Result<Found, Problem> {
    let mut holder = object;
    for _ in 0..MAX_PROTOTYPES {
        match own(reader, holder, key)? {
            Some((Held::Data(value), _)) => return Ok(Found::Data(value)),
            Some((Held::Accessor(accessor), _)) => return Ok(Found::Accessor { accessor, holder }),
            Some((Held::Double { .. }, _)) => {
                return Err("a named property stored as a double".into());
            }
            None => {}
        }
        match prototype(reader, holder)? {
            Some(next) => holder = next,
            None => return Ok(Found::Absent),
        }
    }
    Err(format!("{:#x} has too many prototypes", object.0).into())
}

/// The value of `object[key]` when no program getter computes it: a data
/// property, or a native accessor this module can evaluate. `None` if the
/// property is absent. Program getters are refused.
pub(super) fn get(
    reader: &Reader<'_>,
    object: Object,
    key: Key<'_>,
) -> Result<Option<Value>, Problem> {
    match find(reader, object, key)? {
        Found::Data(value) => Ok(Some(Value::Word(value))),
        Found::Absent => Ok(None),
        Found::Accessor { accessor, .. } => native(reader, object, accessor).map(Some),
    }
}

/// A lookup result: a tagged word, or a string V8 computes instead of
/// storing, such as a bound function's name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Value {
    Word(u64),
    /// UTF-16 units, such as `"bound " + name`.
    Text(Vec<u16>),
}

/// What the native accessor `accessor` returns for `receiver`.
fn native(reader: &Reader<'_>, receiver: Object, accessor: Object) -> Result<Value, Problem> {
    if !reader.is(accessor, Constant::AccessorInfoType)? {
        return Err("the property is a getter, and microscope does not call getters".into());
    }
    let name = reader.field(accessor, Field::AccessorInfoName)?;
    let functions = reader.constant(Constant::FirstJSFunctionType)?
        ..=reader.constant(Constant::LastJSFunctionType)?;
    let ty = reader.instance_type(receiver)?;
    if super::properties::is_named(reader, name, "name")? {
        if functions.contains(&ty) || ty == reader.constant(Constant::JSBoundFunctionType)? {
            return function_name(reader, receiver).map(Value::Text);
        }
    } else if super::properties::is_named(reader, name, "prototype")? && functions.contains(&ty) {
        return function_prototype(reader, receiver)?
            .map(|prototype| Value::Word(prototype.0))
            .ok_or_else(|| "the function has no prototype".into());
    }
    Err(format!(
        "the property is V8's native accessor {:?}, which microscope cannot evaluate",
        reader
            .object(name)?
            .map_or_else(String::new, |name| reader.string(name).unwrap_or_default())
    )
    .into())
}

/// `function.name` as V8's native accessor computes it:
/// `JSFunction::GetName` (the shared name), or `JSBoundFunction::GetName`
/// (`bound ` once per binding, then the target's name).
pub(super) fn function_name(reader: &Reader<'_>, function: Object) -> Result<Vec<u16>, Problem> {
    let mut prefix = Vec::new();
    let mut target = function;
    for _ in 0..MAX_PROTOTYPES {
        if !reader.is(target, Constant::JSBoundFunctionType)? {
            break;
        }
        prefix.extend("bound ".encode_utf16());
        let word = reader.field(target, Field::JSBoundFunctionBoundTargetFunction)?;
        target = reader
            .object(word)?
            .ok_or("a bound function's target is not an object")?;
    }
    let functions = reader.constant(Constant::FirstJSFunctionType)?
        ..=reader.constant(Constant::LastJSFunctionType)?;
    if !functions.contains(&reader.instance_type(target)?) {
        // V8 leaves out the name of a bound proxy or wrapped function.
        return Ok(prefix);
    }
    let shared = reader.typed_field(
        target,
        Field::JSFunctionSharedFunctionInfo,
        Constant::SharedFunctionInfoType,
    )?;
    let flags = i64::from(reader.int32_field(shared, Field::SharedFunctionInfoFlags)?);
    if flags & reader.constant(Constant::SfiNameShouldPrintAsAnonymousMask)? != 0 {
        prefix.extend("anonymous".encode_utf16());
        return Ok(prefix);
    }
    if let (Some(name), _) = super::names::shared_name(reader, shared)? {
        reader.string_into(name, 0, u64::from(u32::MAX), 0, &mut prefix)?;
    }
    Ok(prefix)
}

/// `function.prototype` as V8's native accessor computes it: the object in
/// its prototype-or-initial-map slot, or the initial map's prototype.
/// `None` for a function without one.
fn function_prototype(reader: &Reader<'_>, function: Object) -> Result<Option<Object>, Problem> {
    let word = reader.field(function, Field::JSFunctionPrototypeOrInitialMap)?;
    let Some(held) = reader.object(word)? else {
        return Ok(None);
    };
    if own::is_hole(reader, word)? {
        return Ok(None);
    }
    let map = reader.map(held)?;
    // A map's map is the meta map, whose own map is itself.
    if reader.map(map)? == map {
        let prototype = reader.field(held, Field::MapPrototype)?;
        return reader.object(prototype);
    }
    Ok(Some(held))
}

/// Whether `function` is V8's builtin function named `name`.
pub(super) fn is_builtin(
    reader: &Reader<'_>,
    function: Object,
    name: &str,
) -> Result<bool, Problem> {
    let functions = reader.constant(Constant::FirstJSFunctionType)?
        ..=reader.constant(Constant::LastJSFunctionType)?;
    if !functions.contains(&reader.instance_type(function)?) {
        return Ok(false);
    }
    let shared = reader.typed_field(
        function,
        Field::JSFunctionSharedFunctionInfo,
        Constant::SharedFunctionInfoType,
    )?;
    let script = reader.field(shared, Field::SharedFunctionInfoScript)?;
    if let Some(script) = reader.object(script)?
        && reader.is(script, Constant::ScriptType)?
    {
        return Ok(false);
    }
    // From 13.0 a builtin's number is in the untrusted function data.
    let data = if reader
        .layout
        .has(Field::SharedFunctionInfoUntrustedFunctionData)
    {
        Field::SharedFunctionInfoUntrustedFunctionData
    } else {
        Field::SharedFunctionInfoFunctionData
    };
    if reader.smi(reader.field(shared, data)?)?.is_none() {
        return Ok(false);
    }
    match super::names::shared_name(reader, shared)? {
        (Some(own), _) => super::properties::is_named(reader, own.0, name),
        (None, _) => Ok(name.is_empty()),
    }
}

/// Whether `typeof object` is `"function"`: its map is callable.
pub(super) fn is_callable(reader: &Reader<'_>, object: Object) -> Result<bool, Problem> {
    let map = reader.map(object)?;
    let bits = i64::from(reader.byte_field(map, Field::MapBitField)?);
    Ok(super::properties::part(
        reader,
        bits,
        Constant::MapIsCallableMask,
        Some(Constant::MapIsCallableShift),
    )? != 0)
}

/// Whether `object instanceof constructor`, by `OrdinaryHasInstance`. Only
/// V8's own `Symbol.hasInstance` is accepted; any other is refused.
pub(super) fn instance_of(
    reader: &Reader<'_>,
    object: Object,
    constructor: Object,
) -> Result<bool, Problem> {
    match find(reader, constructor, Key::WellKnown("Symbol.hasInstance"))? {
        Found::Data(method) => {
            let method = reader
                .object(method)?
                .ok_or("Symbol.hasInstance is not a function")?;
            if !is_builtin(reader, method, "[Symbol.hasInstance]")? {
                return Err("the constructor defines its own Symbol.hasInstance".into());
            }
        }
        Found::Accessor { .. } => {
            return Err("the constructor's Symbol.hasInstance is a getter".into());
        }
        Found::Absent => return Ok(false),
    }
    let mut target = constructor;
    for _ in 0..MAX_PROTOTYPES {
        if !reader.is(target, Constant::JSBoundFunctionType)? {
            break;
        }
        let word = reader.field(target, Field::JSBoundFunctionBoundTargetFunction)?;
        target = reader
            .object(word)?
            .ok_or("a bound function's target is not an object")?;
    }
    let Some(Value::Word(prototype)) = get(reader, target, Key::Name("prototype"))? else {
        return Ok(false);
    };
    let Some(prototype) = reader.object(prototype)? else {
        return Ok(false);
    };
    let mut holder = object;
    for _ in 0..MAX_PROTOTYPES {
        match self::prototype(reader, holder)? {
            Some(next) if next == prototype => return Ok(true),
            Some(next) => holder = next,
            None => return Ok(false),
        }
    }
    Err(format!("{:#x} has too many prototypes", object.0).into())
}
