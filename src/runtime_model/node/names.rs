//! What a `SharedFunctionInfo` says of its function: its name, as V8's call
//! sites give it, and the bytecode it runs.

use super::objects::{Object, Problem, Reader};
use crate::image::v8::{Constant, Field, ScopeInfoSlice, scope_info_slice};

/// The function's name as V8's `SharedFunctionInfo::DebugName` gives it:
/// its own, kept in the function's scope once it is compiled, or else the
/// one V8 inferred from where it was defined; empty for an anonymous
/// function.
pub(super) fn function_name(reader: &Reader<'_>, shared: Object) -> Result<String, Problem> {
    let word = reader.field(shared, Field::SharedFunctionInfoNameOrScopeInfo)?;
    // V8 keeps the Smi 0 for a function with no name and no scope.
    let Some(held) = reader.object(word)? else {
        return inferred_name(reader, shared, None);
    };
    if !reader.is(held, Constant::ScopeInfoType)? {
        let name = reader.string(held)?;
        return if name.is_empty() {
            inferred_name(reader, shared, None)
        } else {
            Ok(name)
        };
    }

    let flags = scope_flags(reader, held)?;
    let function_variable = (flags & reader.constant(Constant::ScopeFunctionVariableMask)?)
        >> reader.constant(Constant::ScopeFunctionVariableShift)?;
    // VariableAllocationInfo::NONE is 0: the scope keeps no name.
    if function_variable != 0 {
        let name = scope_word(reader, held, ScopeInfoSlice::FunctionVariableInfo)?;
        if let Some(name) = name.map(|word| reader.object(word)).transpose()?.flatten() {
            let name = reader.string(name)?;
            if !name.is_empty() {
                return Ok(name);
            }
        }
    }
    inferred_name(reader, shared, Some((held, flags)))
}

/// The flags of the scope `scope`: a Smi before V8 13, a 32-bit integer
/// since.
fn scope_flags(reader: &Reader<'_>, scope: Object) -> Result<i64, Problem> {
    let word = reader.field(scope, Field::ScopeInfoFlags)?;
    if reader.version.major < 13 {
        return reader
            .smi(word)?
            .ok_or_else(|| "a scope's flags are no Smi".into());
    }
    // The flags are the low half of the word; padding fills the high.
    Ok(i64::from(
        word.to_le_bytes()[..4]
            .iter()
            .rev()
            .fold(0_u32, |flags, byte| (flags << 8) | u32::from(*byte)),
    ))
}

/// The first word of the slice `slice` of the scope `scope`, or `None`
/// where the scope has it empty.
fn scope_word(
    reader: &Reader<'_>,
    scope: Object,
    slice: ScopeInfoSlice,
) -> Result<Option<u64>, Problem> {
    let start = scope.0 - reader.constant(Constant::HeapObjectTag)?.cast_unsigned();
    let found = scope_info_slice(reader.version, slice, &mut |offset| {
        reader.word(start + offset).ok()
    })?;
    match found {
        Some(found) if found.length > 0 => reader.word(start + found.offset).map(Some),
        _ => Ok(None),
    }
}

/// The name V8 inferred for an anonymous function: kept by its scope,
/// `scope` with its flags, once it is compiled, or else by its uncompiled
/// data.
fn inferred_name(
    reader: &Reader<'_>,
    shared: Object,
    scope: Option<(Object, i64)>,
) -> Result<String, Problem> {
    if let Some((scope, flags)) = scope {
        if flags & reader.constant(Constant::ScopeHasInferredFunctionNameMask)? == 0 {
            return Ok(String::new());
        }
        let name = scope_word(reader, scope, ScopeInfoSlice::InferredFunctionName)?;
        return match name.map(|word| reader.object(word)).transpose()?.flatten() {
            Some(name)
                if reader.instance_type(name)?
                    < reader.constant(Constant::FirstNonstringType)? =>
            {
                reader.string(name)
            }
            _ => Ok(String::new()),
        };
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

/// Whether `shared` is a native function, one V8 exposes to the program,
/// as `Array.prototype.sort`, which its stack traces show though it has no
/// script.
pub(super) fn is_native(reader: &Reader<'_>, shared: Object) -> Result<bool, Problem> {
    let flags = i64::from(reader.int32_field(shared, Field::SharedFunctionInfoFlags)?);
    Ok(flags & reader.constant(Constant::SfiIsNativeMask)? != 0)
}
