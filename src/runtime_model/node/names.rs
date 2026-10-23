//! What a `SharedFunctionInfo` says of its function: its name, as V8's call
//! sites give it, and the bytecode it runs.

use super::objects::{Object, Problem, Reader};
use crate::image::v8::{Constant, Field, ScopeInfoSlice, scope_info_slice};

/// The name of `function`, whose shared information is `shared`, as V8's
/// `JSFunction::GetDebugName` gives it. A function whose map still keeps
/// the `name` accessor every function starts with is named by `shared`;
/// any other, as a method keyed by a symbol whose name V8 defined as the
/// data property `[description]`, by its `name` property where that is a
/// string.
pub(super) fn debug_name(
    reader: &Reader<'_>,
    function: Object,
    shared: Object,
) -> Result<String, Problem> {
    if !keeps_name_accessor(reader, function)? {
        let name = super::properties::data_property(reader, function, "name")?;
        if let Some(name) = name.map(|word| reader.object(word)).transpose()?.flatten()
            && reader.instance_type(name)? < reader.constant(Constant::FirstNonstringType)?
        {
            return reader.string(name);
        }
    }
    function_name(reader, shared)
}

/// Whether `function`'s map keeps its `name` as V8 first defines it, an
/// `AccessorInfo` at `kNameDescriptorIndex` (`UseFastFunctionNameLookup`).
fn keeps_name_accessor(reader: &Reader<'_>, function: Object) -> Result<bool, Problem> {
    let map = reader.map(function)?;
    let bits = i64::from(
        reader
            .int32_field(map, Field::MapBitField3)?
            .cast_unsigned(),
    );
    let own = (bits & reader.constant(Constant::BitField3NumberOfOwnDescriptorsMask)?)
        >> reader.constant(Constant::BitField3NumberOfOwnDescriptorsShift)?;
    if own < reader.constant(Constant::JSFunctionMinDescriptorsForFastBindAndWrap)? {
        return Ok(false);
    }
    let index = reader.constant(Constant::JSFunctionNameDescriptorIndex)?;
    let key = super::properties::descriptor(reader, map, index, Constant::PropDescKey)?;
    if !super::properties::is_named(reader, key, "name")? {
        return Ok(false);
    }
    // A weak reference is no object to the reader, so only a strong
    // pointer is followed, as `GetHeapObjectIfStrong` follows it.
    let value = super::properties::descriptor(reader, map, index, Constant::PropDescValue)?;
    reader.object(value)?.map_or(Ok(false), |value| {
        reader.is(value, Constant::AccessorInfoType)
    })
}

/// The function's name as V8's `SharedFunctionInfo::DebugName` gives it:
/// its own, kept in the function's scope once it is compiled, or else the
/// one V8 inferred from where it was defined; empty for an anonymous
/// function.
pub(super) fn function_name(reader: &Reader<'_>, shared: Object) -> Result<String, Problem> {
    let (name, scope) = shared_name(reader, shared)?;
    if let Some(name) = name {
        let name = reader.string(name)?;
        if !name.is_empty() {
            return Ok(name);
        }
    }
    inferred_name(reader, shared, scope)
}

/// A function's own name, if any, and its scope with its flags, if any.
type SharedName = (Option<Object>, Option<(Object, i64)>);

/// The string `SharedFunctionInfo::Name` gives, its own name with none
/// inferred, or `None` for none; and the function's scope with its flags,
/// where it keeps one.
pub(super) fn shared_name(reader: &Reader<'_>, shared: Object) -> Result<SharedName, Problem> {
    let word = reader.field(shared, Field::SharedFunctionInfoNameOrScopeInfo)?;
    // V8 keeps the Smi 0 for a function with no name and no scope.
    let Some(held) = reader.object(word)? else {
        return Ok((None, None));
    };
    if !reader.is(held, Constant::ScopeInfoType)? {
        if reader.instance_type(held)? >= reader.constant(Constant::FirstNonstringType)? {
            return Err(format!("the name of {:#x} is no string", shared.0).into());
        }
        return Ok((Some(held), None));
    }

    let flags = scope_flags(reader, held)?;
    let function_variable = (flags & reader.constant(Constant::ScopeFunctionVariableMask)?)
        >> reader.constant(Constant::ScopeFunctionVariableShift)?;
    // VariableAllocationInfo::NONE is 0: the scope keeps no name.
    let name = if function_variable == 0 {
        None
    } else {
        scope_word(reader, held, ScopeInfoSlice::FunctionVariableInfo)?
            .map(|word| reader.object(word))
            .transpose()?
            .flatten()
            .filter(|name| {
                reader.instance_type(*name).is_ok_and(|ty| {
                    reader
                        .constant(Constant::FirstNonstringType)
                        .is_ok_and(|end| ty < end)
                })
            })
    };
    Ok((name, Some((held, flags))))
}

/// The flags of the scope `scope`: a Smi before V8 13, a 32-bit integer
/// since.
pub(super) fn scope_flags(reader: &Reader<'_>, scope: Object) -> Result<i64, Problem> {
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
pub(super) fn inferred_name(
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
pub(super) fn function_data(
    reader: &Reader<'_>,
    shared: Object,
) -> Result<Option<Object>, Problem> {
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

/// The bytecode `shared` runs, as V8's `GetActiveBytecodeArray` finds it:
/// kept by itself, by its interpreter data, or by its baseline code;
/// `None` for a function with none.
pub(super) fn bytecode(reader: &Reader<'_>, shared: Object) -> Result<Option<Object>, Problem> {
    let Some(mut data) = function_data(reader, shared)? else {
        return Ok(None);
    };
    if reader.is(data, Constant::CodeType)? {
        data = reader
            .object(reader.field(data, Field::CodeDeoptimizationDataOrInterpreterData)?)?
            .ok_or("a function's baseline code keeps no bytecode")?;
    }
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
