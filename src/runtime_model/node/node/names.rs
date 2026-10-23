//! What a `SharedFunctionInfo` says of its function: its name, as V8's call
//! sites give it, and the bytecode it runs.

use super::objects::{Object, Problem, Reader};
use crate::image::v8::{Constant, Field};

/// The function's name: its own, or else the one V8 inferred from where it
/// was defined; empty for an anonymous function.
pub(super) fn function_name(reader: &Reader<'_>, shared: Object) -> Result<String, Problem> {
    let word = reader.field(shared, Field::SharedFunctionInfoNameOrScopeInfo)?;
    let Some(held) = reader.object(word)? else {
        // V8 keeps the Smi 0 for a function with no name.
        return inferred_name(reader, shared, None);
    };
    if reader.is(held, Constant::ScopeInfoType)? {
        return Err("the name a scope keeps is not read yet".into());
    }
    let name = reader.string(held)?;
    if name.is_empty() {
        return inferred_name(reader, shared, None);
    }
    Ok(name)
}

/// The name V8 inferred for an anonymous function: kept by the function's
/// uncompiled data, or its scope, `scope`.
fn inferred_name(
    reader: &Reader<'_>,
    shared: Object,
    scope: Option<Object>,
) -> Result<String, Problem> {
    if scope.is_some() {
        return Err("the name a scope keeps is not read yet".into());
    }
    let Some(data) = function_data(reader, shared)? else {
        return Ok(String::new());
    };
    let uncompiled = [
        Constant::UncompiledDataWithPreparseDataType,
        Constant::UncompiledDataWithoutPreparseDataType,
    ];
    let ty = reader.instance_type(data)?;
    if !uncompiled
        .iter()
        .any(|kind| reader.constant(*kind).ok() == Some(ty))
    {
        return Ok(String::new());
    }
    let name = reader.field(data, Field::UncompiledDataInferredName)?;
    reader
        .object(name)?
        .map_or_else(|| Ok(String::new()), |name| reader.string(name))
}

/// The object a `SharedFunctionInfo` keeps its code in: its bytecode, the
/// interpreter data that holds it, or its uncompiled data.
fn function_data(reader: &Reader<'_>, shared: Object) -> Result<Option<Object>, Problem> {
    let field = if reader
        .layout
        .has(Field::SharedFunctionInfoTrustedFunctionData)
    {
        Field::SharedFunctionInfoTrustedFunctionData
    } else {
        Field::SharedFunctionInfoFunctionData
    };
    let data = reader.object(reader.field(shared, field)?)?;
    // From 13.0 the uncompiled data is untrusted and kept apart.
    if data.is_none()
        && reader
            .layout
            .has(Field::SharedFunctionInfoUntrustedFunctionData)
    {
        return reader
            .object(reader.field(shared, Field::SharedFunctionInfoUntrustedFunctionData)?);
    }
    Ok(data)
}

/// The bytecode `shared` runs, where it keeps it where a reader can prove
/// it: `None` for a function with none or one kept another way, as in its
/// baseline code.
pub(super) fn bytecode(reader: &Reader<'_>, shared: Object) -> Result<Option<Object>, Problem> {
    let Some(data) = function_data(reader, shared)? else {
        return Ok(None);
    };
    if reader.is(data, Constant::BytecodeArrayType)? {
        return Ok(Some(data));
    }
    if reader.is(data, Constant::InterpreterDataType)? {
        return reader
            .typed_field(
                data,
                Field::InterpreterDataBytecodeArray,
                Constant::BytecodeArrayType,
            )
            .map(Some);
    }
    Ok(None)
}
