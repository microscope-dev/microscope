//! Errors as inspect reads them: `stack` from its builtin getter, and
//! `name` and `message` through the prototype chain.
//!
//! V8 stores an error's stack under the private `error_stack_symbol`. Once
//! something has read `stack` it is the text; before that it is the raw
//! call sites, as a fixed array or inside an `ErrorStackData`.

use super::access;
use super::inspect::{ErrorDetails, JsString, Key, Property, Value};
use super::lookup;
use super::objects::{Object, Problem, Reader};
use super::own::{self, Held};
use super::properties;
use super::values;
use super::values::Builder;
use crate::image::v8::{Constant, Field};

/// The own non-enumerable properties inspect shows on an error.
const HIDDEN: [&str; 4] = ["cause", "errors", "error", "suppressed"];

/// What inspect reads from the error `object`, with values at `level`.
pub(super) fn details(
    builder: &mut Builder<'_>,
    object: Object,
    level: u32,
) -> Result<ErrorDetails, Problem> {
    let reader = builder.reader;
    let stack =
        stack(builder, object).unwrap_or_else(|problem| Value::Unreadable(problem.to_string()));
    let get = |builder: &mut Builder<'_>, name: &str| -> Value {
        match lookup::get(&reader, object, lookup::Key::Name(name)) {
            Ok(Some(lookup::Value::Word(word))) => builder.value(word, level),
            Ok(Some(lookup::Value::Text(units))) => Value::String(JsString::complete(units)),
            Ok(None) => Value::Undefined,
            Err(problem) => Value::Unreadable(problem.to_string()),
        }
    };
    let name = get(builder, "name");
    let message = get(builder, "message");

    let mut hidden = Vec::new();
    for property in own::named(&reader, object)? {
        if property.enumerable || property.symbol {
            continue;
        }
        for key in HIDDEN {
            if super::properties::is_named(&reader, property.key.0, key)? {
                hidden.push(Property {
                    key: Key::String(JsString::from(key)),
                    slot: builder.slot(property.held, level),
                });
            }
        }
    }
    Ok(ErrorDetails {
        stack,
        name,
        message,
        hidden,
    })
}

/// The value of `error.stack` when V8's own getter computes it.
fn stack(builder: &mut Builder<'_>, error: Object) -> Result<Value, Problem> {
    let reader = builder.reader;
    match lookup::find(&reader, error, lookup::Key::Name("stack"))? {
        lookup::Found::Data(word) => return Ok(builder.value(word, 0)),
        lookup::Found::Absent => return Ok(Value::Undefined),
        lookup::Found::Accessor { holder, .. } if holder == error => {}
        lookup::Found::Accessor { .. } => {
            return Err(
                "the error's stack is an inherited getter, and microscope does not call getters"
                    .into(),
            );
        }
    }
    let symbol = stack_symbol(&reader, error)?;
    let kept = own::all_named(&reader, error)?
        .into_iter()
        .find(|property| property.key == symbol)
        .map(|property| property.held);
    let Some(Held::Data(word)) = kept else {
        // V8's getter returns undefined if no stack was captured.
        return Ok(Value::Undefined);
    };
    let held = if let Some(data) = reader.object(word)?
        && reader.is(data, Constant::ErrorStackDataType)?
    {
        let field = if reader
            .layout
            .has(Field::ErrorStackDataCallSiteInfosOrFormattedStack)
        {
            Field::ErrorStackDataCallSiteInfosOrFormattedStack
        } else {
            Field::ErrorStackDataRawDataForCallSiteInfosOrFormattedStack
        };
        reader.field(data, field)?
    } else {
        word
    };
    if let Some(sites) = reader.object(held)?
        && reader.is(sites, Constant::FixedArrayType)?
    {
        return super::call_sites::format(builder, error, sites);
    }
    Ok(builder.value(held, 0))
}

/// V8's private `error_stack_symbol`, the key an error's stack is stored
/// under. In every supported version `InstallError` (`bootstrapper.cc`)
/// puts it in the first descriptor of `%Error%`'s initial map, at
/// `kJSErrorErrorStackSymbolIndex`. Private root symbols have no
/// description to search for, so it is found by position.
fn stack_symbol(reader: &Reader<'_>, error: Object) -> Result<Object, Problem> {
    let native = access::native_context(reader, reader.map(error)?)?;
    let function = reader
        .object(access::slot(
            reader,
            native,
            Constant::ContextErrorFunctionIndex,
        )?)?
        .filter(|function| {
            reader
                .is(*function, Constant::JSFunctionType)
                .unwrap_or(false)
        })
        .ok_or("the realm's %Error% is not a function")?;
    let initial = reader
        .object(reader.field(function, Field::JSFunctionPrototypeOrInitialMap)?)?
        .filter(|map| {
            reader
                .map(*map)
                .is_ok_and(|meta| reader.map(meta) == Ok(meta))
        })
        .ok_or("%Error% has no initial map")?;
    let key = properties::descriptor(reader, initial, 0, Constant::PropDescKey)?;
    reader
        .object(key)?
        .filter(|symbol| {
            reader.is(*symbol, Constant::SymbolType).unwrap_or(false)
                && values::is_private(reader, *symbol).unwrap_or(false)
        })
        .ok_or_else(|| "the first property of %Error%'s instances is not a private symbol".into())
}
