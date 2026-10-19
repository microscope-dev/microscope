//! Evaluating DWARF location expressions against a stopped thread.

use std::sync::Arc;

use gimli::{EvaluationResult, Location, RunTimeEndian, Value};

use crate::debug_info::dwarf::Reader;
use crate::debug_info::{EntryParameter, VariableRuntime, VariableRuntimeError};
use crate::{
    BaseType, BaseTypeEncoding, ImageAddress, TargetDescription, VariableUnavailableReason,
    VirtualAddress,
};

use super::codec::{
    bytes_to_u64, integer_bytes, register_u64, signed_integer_bytes, wrapping_integer_bytes,
};

use super::location::{Expression, LocationListId, LocationSelectionError, LocationTables, select};
use super::shape::ValueShapeError;
use super::types::DynamicAggregateLayoutKey;
use super::{
    ConstantValue, InspectionBudget, MAX_EVALUATION_ITERATIONS, Metadata, MetadataAbsence,
};

pub(super) enum FrameBaseCache {
    Empty,
    Available(VirtualAddress),
    Unavailable(VariableUnavailableReason),
    Malformed(Arc<str>),
}

/// How an evaluation may obtain the frame base if the executed expression
/// path actually requires one.
pub(super) enum FrameBase<'a> {
    Unsupported,
    Lazy(FrameBaseContext<'a>),
}

pub(super) struct FrameBaseContext<'a> {
    pub(super) location: &'a Metadata<LocationListId>,
    pub(super) tables: LocationTables<'a>,
    pub(super) address: Option<ImageAddress>,
    pub(super) cache: &'a mut FrameBaseCache,
}

/// An evaluation failure that preserves the unavailable-versus-malformed
/// distinction of the frame-base metadata it may consult.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum EvaluateError {
    Unavailable(VariableUnavailableReason),
    Malformed(Arc<str>),
    Fatal(Arc<str>),
}

impl From<VariableUnavailableReason> for EvaluateError {
    fn from(reason: VariableUnavailableReason) -> Self {
        Self::Unavailable(reason)
    }
}

impl From<VariableRuntimeError> for EvaluateError {
    fn from(error: VariableRuntimeError) -> Self {
        match error {
            VariableRuntimeError::Unavailable(reason) => Self::Unavailable(reason),
            VariableRuntimeError::Malformed(description) => Self::Malformed(description),
            VariableRuntimeError::Fatal(error) => Self::Fatal(error),
        }
    }
}

impl From<crate::InspectionExhaustion> for EvaluateError {
    fn from(exhaustion: crate::InspectionExhaustion) -> Self {
        Self::Unavailable(exhaustion.into())
    }
}

impl From<Arc<str>> for EvaluateError {
    fn from(description: Arc<str>) -> Self {
        Self::Malformed(description)
    }
}

impl From<&str> for EvaluateError {
    fn from(description: &str) -> Self {
        Self::Malformed(description.into())
    }
}

impl From<ValueShapeError> for EvaluateError {
    fn from(error: ValueShapeError) -> Self {
        match error {
            ValueShapeError::Malformed(description) => Self::Malformed(description),
            ValueShapeError::Unsupported(_) => {
                Self::Unavailable(crate::UnsupportedVariableFeature::TypeRepresentation.into())
            }
        }
    }
}

impl From<crate::UnsupportedVariableFeature> for EvaluateError {
    fn from(feature: crate::UnsupportedVariableFeature) -> Self {
        Self::Unavailable(feature.into())
    }
}

fn resolve_frame_base(
    context: &mut FrameBaseContext<'_>,
    endian: RunTimeEndian,
    runtime: &mut dyn VariableRuntime,
    budget: &mut InspectionBudget,
) -> std::result::Result<VirtualAddress, EvaluateError> {
    if matches!(context.cache, FrameBaseCache::Empty) {
        *context.cache = match context.location {
            Metadata::Value(frame_base) => {
                match select(context.tables.list(*frame_base), context.address) {
                    Ok(Some(expression)) => {
                        match evaluate_frame_base(
                            expression,
                            endian,
                            context.address,
                            runtime,
                            budget,
                        ) {
                            Ok(value) => FrameBaseCache::Available(value),
                            // Request-specific exhaustion must not poison the
                            // frame-base cache shared by later inspections.
                            Err(EvaluateError::Unavailable(
                                reason @ (VariableUnavailableReason::EvaluationLimit
                                | VariableUnavailableReason::InspectionLimit(_)),
                            )) => return Err(reason.into()),
                            Err(EvaluateError::Unavailable(reason)) => {
                                FrameBaseCache::Unavailable(reason)
                            }
                            Err(EvaluateError::Malformed(description)) => {
                                FrameBaseCache::Malformed(description)
                            }
                            Err(error @ EvaluateError::Fatal(_)) => return Err(error),
                        }
                    }
                    Err(LocationSelectionError::Unavailable(reason)) => {
                        FrameBaseCache::Unavailable(reason)
                    }
                    Err(LocationSelectionError::Malformed(description)) => {
                        FrameBaseCache::Malformed(description)
                    }
                    Ok(None) => FrameBaseCache::Unavailable(
                        VariableUnavailableReason::UnavailableAtInstruction,
                    ),
                }
            }

            Metadata::Absent(
                MetadataAbsence::NoFrameBase
                | MetadataAbsence::NotApplicable
                | MetadataAbsence::NoLocation,
            ) => FrameBaseCache::Unavailable(VariableUnavailableReason::CallFrameUnavailable(
                crate::CallFrameUnavailableReason::NoInstructionContext,
            )),
            Metadata::Malformed(description) => FrameBaseCache::Malformed(Arc::clone(description)),
        };
    }

    match context.cache {
        FrameBaseCache::Available(value) => Ok(*value),
        FrameBaseCache::Unavailable(reason) => Err(EvaluateError::Unavailable(reason.clone())),
        FrameBaseCache::Malformed(description) => {
            Err(EvaluateError::Malformed(Arc::clone(description)))
        }
        FrameBaseCache::Empty => Err(EvaluateError::Fatal(
            "the frame base cache was not populated".into(),
        )),
    }
}

fn evaluate_frame_base(
    expression: Expression<'_>,
    endian: RunTimeEndian,
    address: Option<ImageAddress>,
    runtime: &mut dyn VariableRuntime,
    budget: &mut InspectionBudget,
) -> std::result::Result<VirtualAddress, EvaluateError> {
    // A frame-base expression may not itself require a frame base.
    let pieces = evaluate(
        expression,
        endian,
        address,
        &mut FrameBase::Unsupported,
        runtime,
        budget,
    )?;
    let [piece] = pieces.as_slice() else {
        return Err(EvaluateError::Malformed(
            "frame base is not one complete piece".into(),
        ));
    };

    if piece.size_in_bits.is_some() || piece.bit_offset.is_some() {
        return Err(EvaluateError::Malformed(
            "frame base is a partial piece".into(),
        ));
    }

    match piece.location {
        Location::Address { address } => Ok(VirtualAddress::new(address)),
        Location::Register { register } => {
            register_u64(runtime, register.into(), endian).map(VirtualAddress::new)
        }
        _ => Err(EvaluateError::Malformed(
            "frame base did not evaluate to an address or register".into(),
        )),
    }
}

/// Evaluates a location expression in the frame `runtime` reads, executing
/// at `address` in this image when known.
pub(super) fn evaluate<'expression>(
    expression: Expression<'expression>,
    endian: RunTimeEndian,
    address: Option<ImageAddress>,
    frame_base: &mut FrameBase<'_>,
    runtime: &mut dyn VariableRuntime,
    budget: &mut InspectionBudget,
) -> std::result::Result<Vec<gimli::Piece<Reader<'expression>>>, EvaluateError> {
    evaluate_with_object(
        expression, endian, address, frame_base, runtime, budget, None,
    )
}

pub(super) fn evaluate_dynamic_aggregate_address(
    layouts: crate::image::type_facts::TypeFactsView<'_>,
    tables: LocationTables<'_>,
    key: DynamicAggregateLayoutKey,
    endian: RunTimeEndian,
    runtime: &mut dyn VariableRuntime,
    budget: &mut InspectionBudget,
    object_address: VirtualAddress,
) -> std::result::Result<VirtualAddress, EvaluateError> {
    let expression = super::layout_child(key.child)
        .and_then(|child| layouts.dynamic_layout(key.aggregate, child))
        .ok_or_else(|| {
            EvaluateError::Unavailable(
                crate::UnsupportedVariableFeature::RuntimeAggregateLocation.into(),
            )
        })?;
    let expression = tables.expression(expression);
    let pieces = evaluate_with_object(
        expression,
        endian,
        None,
        &mut FrameBase::Unsupported,
        runtime,
        budget,
        Some(object_address),
    )?;
    let [piece] = pieces.as_slice() else {
        return Err(EvaluateError::Malformed(
            "runtime aggregate child location produced multiple pieces".into(),
        ));
    };

    if piece.size_in_bits.is_some() || piece.bit_offset.is_some() {
        return Err(crate::UnsupportedVariableFeature::CompositeLocation.into());
    }

    let Location::Address { address } = piece.location else {
        return Err(EvaluateError::Malformed(
            "runtime aggregate child location did not produce an address".into(),
        ));
    };

    Ok(VirtualAddress::new(address))
}

/// The location an expression with no operations describes: an object the
/// compiler did not keep (DWARF 5 section 2.6.1.1.1).
fn empty_location<'expression>() -> Vec<gimli::Piece<Reader<'expression>>> {
    vec![gimli::Piece {
        size_in_bits: None,
        bit_offset: None,
        location: Location::Empty,
    }]
}

pub(super) fn evaluate_with_object<'expression>(
    expression: Expression<'expression>,
    endian: RunTimeEndian,
    address: Option<ImageAddress>,
    frame_base: &mut FrameBase<'_>,
    runtime: &mut dyn VariableRuntime,
    budget: &mut InspectionBudget,
    object_address: Option<VirtualAddress>,
) -> std::result::Result<Vec<gimli::Piece<Reader<'expression>>>, EvaluateError> {
    budget.consume_expression_work(u64::from(MAX_EVALUATION_ITERATIONS))?;

    // gimli rejects an expression with no operations as a stack underflow.
    if expression.bytes().is_empty() {
        return Ok(empty_location());
    }

    let reader = gimli::EndianSlice::new(expression.bytes(), endian);
    let mut evaluation = gimli::Expression(reader).evaluation(expression.encoding());
    if let Some(address) = object_address {
        evaluation.set_initial_value(address.get());
        evaluation.set_object_address(address.get());
    }

    // Bound evaluation so a malformed expression with a backward branch cannot
    // hang the controller thread.
    evaluation.set_max_iterations(MAX_EVALUATION_ITERATIONS);
    let mut result = evaluation_step(evaluation.evaluate())?;

    loop {
        result = match result {
            EvaluationResult::Complete => return Ok(evaluation.result()),
            EvaluationResult::RequiresRegister {
                register,
                base_type,
            } => {
                let register = runtime.register(register.into())?;
                let value = evaluation_value(
                    &register.bytes,
                    evaluation_value_type(expression, base_type.0)?,
                    endian,
                )?;
                evaluation_step(evaluation.resume_with_register(value))?
            }
            EvaluationResult::RequiresFrameBase => {
                let value = match frame_base {
                    FrameBase::Unsupported => {
                        return Err("frame base is unavailable".into());
                    }
                    FrameBase::Lazy(context) => {
                        resolve_frame_base(context, endian, runtime, budget)?
                    }
                };

                evaluation_step(evaluation.resume_with_frame_base(value.get()))?
            }
            EvaluationResult::RequiresCallFrameCfa => evaluation_step(
                evaluation.resume_with_call_frame_cfa(runtime.call_frame_cfa()?.get()),
            )?,
            EvaluationResult::RequiresRelocatedAddress(address) => {
                evaluation_step(evaluation.resume_with_relocated_address(
                    runtime.relocate(ImageAddress::new(address))?.get(),
                ))?
            }
            EvaluationResult::RequiresIndexedAddress { index, relocate } => {
                evaluation_step(evaluation.resume_with_indexed_address(indexed_address(
                    expression, index, relocate, runtime,
                )?))?
            }
            EvaluationResult::RequiresBaseType(offset) => evaluation_step(
                evaluation.resume_with_base_type(evaluation_value_type(expression, offset.0)?),
            )?,
            EvaluationResult::RequiresMemory {
                address,
                size,
                space: None,
                base_type,
            } => {
                budget.consume_memory(usize::from(size))?;
                let bytes = runtime.read_memory(VirtualAddress::new(address), usize::from(size))?;
                let value = evaluation_value(
                    &bytes,
                    evaluation_value_type(expression, base_type.0)?,
                    endian,
                )?;
                evaluation_step(evaluation.resume_with_memory(value))?
            }
            EvaluationResult::RequiresMemory { space: Some(_), .. } => {
                return Err(crate::UnsupportedVariableFeature::AddressSpace.into());
            }
            EvaluationResult::RequiresEntryValue(operand) => {
                evaluation_step(evaluation.resume_with_entry_value(entry_value(
                    operand, expression, endian, runtime, budget,
                )?))?
            }
            EvaluationResult::RequiresParameterRef(offset) => {
                let offset = debug_info_offset(expression, offset)
                    .ok_or_else(|| Arc::<str>::from("parameter reference outside a unit"))?;
                let word = runtime.entry_value(EntryParameter::Parameter(offset), budget)?;
                evaluation_step(evaluation.resume_with_parameter_ref(word))?
            }
            EvaluationResult::RequiresAtLocation(reference) => {
                let bytes = called_procedure(expression, reference, address)?;
                evaluation_step(
                    evaluation.resume_with_at_location(gimli::EndianSlice::new(bytes, endian)),
                )?
            }
            EvaluationResult::RequiresTls(offset) => {
                evaluation_step(evaluation.resume_with_tls(runtime.tls_address(offset)?.get()))?
            }

            EvaluationResult::RequiresWasmLocal { .. }
            | EvaluationResult::RequiresWasmGlobal { .. }
            | EvaluationResult::RequiresWasmStack { .. } => {
                return Err(crate::UnsupportedVariableFeature::WasmLocation.into());
            }
        };
    }
}

fn evaluation_step<T>(
    result: std::result::Result<T, gimli::Error>,
) -> std::result::Result<T, EvaluateError> {
    result.map_err(|error| match error {
        gimli::Error::TooManyIterations => VariableUnavailableReason::EvaluationLimit.into(),
        // DW_OP_GNU_variable_value and DW_OP_GNU_uninit.
        gimli::Error::UnsupportedEvaluation => {
            crate::UnsupportedVariableFeature::ExpressionOperation.into()
        }
        error => evaluation_error(error).into(),
    })
}

/// The address at `index` in the unit's address table, relocated when the
/// expression asks.
fn indexed_address(
    expression: Expression<'_>,
    index: gimli::DebugAddrIndex<usize>,
    relocate: bool,
    runtime: &dyn VariableRuntime,
) -> std::result::Result<u64, EvaluateError> {
    let address = u64::try_from(index.0)
        .ok()
        .and_then(|index| expression.indexed_address(index))
        .ok_or_else(|| Arc::<str>::from("DWARF address index is unavailable"))?;
    if relocate {
        Ok(runtime.relocate(ImageAddress::new(address))?.get())
    } else {
        Ok(address)
    }
}

/// The value a `DW_OP_entry_value` operand had on entry, as the caller
/// recovers it.
fn entry_value(
    operand: gimli::Expression<Reader<'_>>,
    expression: Expression<'_>,
    endian: RunTimeEndian,
    runtime: &mut dyn VariableRuntime,
    budget: &mut InspectionBudget,
) -> std::result::Result<gimli::Value, EvaluateError> {
    let (parameter, value_type) = entry_parameter(operand, expression)?;
    let word = runtime.entry_value(parameter, budget)?;
    let bytes = match endian {
        RunTimeEndian::Little => word.to_le_bytes(),
        RunTimeEndian::Big => word.to_be_bytes(),
    };

    evaluation_value(&bytes, value_type, endian)
}

/// The `.debug_info` offset of an entry in the expression's own unit.
fn debug_info_offset(expression: Expression<'_>, offset: gimli::UnitOffset<usize>) -> Option<u64> {
    expression
        .unit_offset()?
        .checked_add(u64::try_from(offset.0).ok()?)
}

/// The expression a `DW_OP_call*` runs, which the procedure it names holds
/// at `address`.
fn called_procedure(
    expression: Expression<'_>,
    reference: gimli::DieReference<usize>,
    address: Option<ImageAddress>,
) -> std::result::Result<&[u8], EvaluateError> {
    let offset = match reference {
        gimli::DieReference::UnitRef(offset) => debug_info_offset(expression, offset),
        gimli::DieReference::DebugInfoRef(offset) => u64::try_from(offset.0).ok(),
    };

    let procedure = offset
        .and_then(|offset| expression.procedure(offset))
        .ok_or(crate::UnsupportedVariableFeature::CrossDieEvaluation)?;

    // A procedure without a location has no effect (DWARF 5 section 2.5.1.5).
    let Some(location) = procedure.locations() else {
        return Ok(&[]);
    };

    match select(location, address) {
        Ok(Some(called)) => Ok(called.bytes()),
        Ok(None) => Err(VariableUnavailableReason::UnavailableAtInstruction.into()),
        Err(LocationSelectionError::Unavailable(reason)) => Err(reason.into()),
        Err(LocationSelectionError::Malformed(description)) => {
            Err(EvaluateError::Malformed(description))
        }
    }
}

/// What a `DW_OP_entry_value` operand asks of the caller, and the type of
/// the value it yields: a register's value, or the value at the address it
/// held (DWARF 5 section 2.5.1.7).
fn entry_parameter(
    operand: gimli::Expression<Reader<'_>>,
    expression: Expression<'_>,
) -> std::result::Result<(EntryParameter, gimli::ValueType), EvaluateError> {
    let mut operations = operand.operations(expression.encoding());
    let mut next = || {
        operations
            .next()
            .map_err(|error| EvaluateError::Malformed(evaluation_error(error)))
    };

    let parameter = match (next()?, next()?, next()?) {
        (Some(gimli::Operation::Register { register }), None, None) => (
            EntryParameter::Register(register.into()),
            gimli::ValueType::Generic,
        ),
        // DW_OP_regval_type, or DW_OP_bregN 0 used as one.
        (
            Some(gimli::Operation::RegisterOffset {
                register,
                offset: 0,
                base_type,
            }),
            None,
            None,
        ) => (
            EntryParameter::Register(register.into()),
            evaluation_value_type(expression, base_type.0)?,
        ),
        (
            Some(gimli::Operation::RegisterOffset {
                register,
                offset: 0,
                base_type: gimli::UnitOffset(0),
            }),
            Some(gimli::Operation::Deref {
                base_type: gimli::UnitOffset(0),
                size,
                space: false,
            }),
            None,
        ) if u64::from(size) == u64::from(expression.encoding().address_size) => (
            EntryParameter::Referent(register.into()),
            gimli::ValueType::Generic,
        ),
        _ => return Err(crate::UnsupportedVariableFeature::EntryValue.into()),
    };

    Ok(parameter)
}

fn evaluation_value_type(
    expression: Expression<'_>,
    offset: usize,
) -> std::result::Result<gimli::ValueType, VariableUnavailableReason> {
    if offset == 0 {
        return Ok(gimli::ValueType::Generic);
    }

    u64::try_from(offset)
        .ok()
        .and_then(|offset| expression.base_type(offset))
        .ok_or_else(|| crate::UnsupportedVariableFeature::TypedValue.into())
}

fn evaluation_value(
    bytes: &[u8],
    value_type: gimli::ValueType,
    endian: RunTimeEndian,
) -> std::result::Result<Value, EvaluateError> {
    if value_type == gimli::ValueType::Generic {
        return bytes_to_u64(bytes, endian).map(Value::Generic);
    }

    let Ok(size) = usize::try_from(value_type.bit_size(u64::MAX) / 8) else {
        return Err(EvaluateError::Malformed(
            "register or memory value is too large to read".into(),
        ));
    };

    if bytes.len() < size {
        return Err(EvaluateError::Malformed(
            "register or memory value is shorter than its DWARF type".into(),
        ));
    }

    let bytes = match endian {
        RunTimeEndian::Little => &bytes[..size],
        RunTimeEndian::Big => &bytes[bytes.len() - size..],
    };

    Value::parse(value_type, gimli::EndianSlice::new(bytes, endian))
        .map_err(|error| EvaluateError::Malformed(evaluation_error(error)))
}

pub(super) fn dwarf_value_bytes(
    value: Value,
    type_info: &BaseType,
    target: TargetDescription,
) -> std::result::Result<Arc<[u8]>, EvaluateError> {
    let Ok(size) = usize::try_from(type_info.byte_size) else {
        return Err(EvaluateError::Malformed(
            "scalar type is too large to read".into(),
        ));
    };

    let mut integer = match value {
        Value::Generic(value) | Value::U64(value) => u128::from(value),
        Value::U8(value) => u128::from(value),
        Value::U16(value) => u128::from(value),
        Value::U32(value) => u128::from(value),
        Value::I8(value) => i128::from(value).cast_unsigned(),
        Value::I16(value) => i128::from(value).cast_unsigned(),
        Value::I32(value) => i128::from(value).cast_unsigned(),
        Value::I64(value) => i128::from(value).cast_unsigned(),
        Value::F32(value) if size == 4 => {
            return integer_bytes(u128::from(value.to_bits()), size, target);
        }
        Value::F64(value) if size == 8 => {
            return integer_bytes(u128::from(value.to_bits()), size, target);
        }
        Value::F32(_) | Value::F64(_) => {
            return Err(EvaluateError::Malformed(
                "computed floating-point size mismatch".into(),
            ));
        }
    };

    // GCC and Clang represent optimized source booleans with word-sized
    // bitwise expressions (notably DW_OP_not). The source truth value is the
    // low bit after conversion to the declared one-byte boolean type.
    if type_info.encoding == BaseTypeEncoding::Boolean {
        integer &= 1;
    }

    wrapping_integer_bytes(integer, size, target)
}

pub(super) fn materialize_constant(
    value: &ConstantValue,
    size: usize,
    target: TargetDescription,
) -> std::result::Result<Arc<[u8]>, EvaluateError> {
    match value {
        // Producers use DW_FORM_sdata when the implicit high bits are signed.
        // A fixed data form supplies zero high bits; the target type then
        // interprets the materialized byte pattern.
        ConstantValue::Unsigned(value) | ConstantValue::Fixed(value) => {
            integer_bytes(narrowed_constant(*value, size), size, target)
        }
        ConstantValue::Signed(value) => signed_integer_bytes(*value, size, target),
        ConstantValue::Bytes(bytes) if bytes.len() == size => Ok(Arc::clone(bytes)),
        ConstantValue::Bytes(_) => Err(EvaluateError::Malformed(
            "constant value size does not match its scalar type".into(),
        )),
    }
}

/// The pattern a wider constant stands for: Clang writes a narrow type's
/// constant with its sign bit set as the 64-bit sign extension of its
/// pattern, in an unsigned form. Any other value is kept, to be refused if it
/// does not fit.
fn narrowed_constant(value: u128, size: usize) -> u128 {
    let bits = size * 8;
    if size == 0 || bits >= 64 {
        return value;
    }

    let pattern = value & ((1_u128 << bits) - 1);
    let negative = pattern >> (bits - 1) == 1;
    let extended = pattern | (u128::from(u64::MAX) & !((1_u128 << bits) - 1));

    if negative && value == extended {
        pattern
    } else {
        value
    }
}

pub(super) fn evaluation_error(error: gimli::Error) -> Arc<str> {
    format!("DWARF expression evaluation failed: {error}").into()
}
