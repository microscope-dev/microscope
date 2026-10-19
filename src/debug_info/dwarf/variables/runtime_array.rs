//! Arrays bounded at run time: what one value of such an array finds its
//! bounds, strides, and elements to be.

use std::sync::Arc;

use gimli::Location;

use crate::debug_info::VariableRuntime;
use crate::image::type_facts::{BoundPart, LayoutChild};
use crate::inspection::InspectionBudget;
use crate::model::{ArrayOrdering, ArrayPlacement, ValueStorage};
use crate::{
    ArrayBound, ArrayDimension, ArrayExtent, ImageAddress, RuntimeDimension, ScalarValue,
    TextSummary, TypeId, VariableUnavailableReason, VariableValue, VirtualAddress,
};

use super::codec::{decode_scalar, unsigned_value};
use super::evaluate::evaluate_with_object;
use super::evaluate::{EvaluateError, FrameBase, FrameBaseCache, FrameBaseContext};
use super::pieces::storage_from_pieces;
use super::shape::ValueShape;
use super::storage;
use super::types::TypeResolution;
use super::{DwarfVariableInfo, Metadata, MetadataAbsence};

/// A dimension's lower bound and its count, unknown for a C flexible array
/// member.
pub(super) type Bound = (i128, Option<u64>);

/// What one value of an array bounded at run time is.
pub(super) enum Resolved {
    /// Elements, at `data`, each dimension's lower bound and count, the
    /// count unknown for a C flexible array member, and each dimension's
    /// stride in bytes.
    Elements {
        data: ValueStorage,
        dimensions: Vec<Bound>,
        strides: Vec<i64>,
    },
    NotAllocated,
    NotAssociated,
}

impl Resolved {
    /// Where the elements of an array whose every count is known are.
    pub(super) fn placement(dimensions: &[Bound], strides: &[i64]) -> Option<ArrayPlacement> {
        Some(ArrayPlacement {
            dimensions: dimensions
                .iter()
                .map(|&(lower_bound, count)| {
                    Some(ArrayDimension {
                        lower_bound,
                        count: count?,
                    })
                })
                .collect::<Option<_>>()?,
            strides: strides.into(),
        })
    }
}

/// The byte offset of the element at source `indices`, checked against
/// every count that is known. An index outside them is unavailable, as a
/// slice's is.
pub(super) fn element_offset(
    dimensions: &[Bound],
    strides: &[i64],
    indices: &[i128],
) -> Result<i64, EvaluateError> {
    let mut offset = 0_i64;

    for ((&(lower_bound, count), &stride), &index) in dimensions.iter().zip(strides).zip(indices) {
        let relative = index
            .checked_sub(lower_bound)
            .filter(|relative| {
                *relative >= 0
                    && count.is_none_or(|count| relative.cast_unsigned() < u128::from(count))
            })
            .ok_or_else(|| VariableUnavailableReason::IndexOutOfBounds {
                index,
                lower_bound,
                count: count.unwrap_or(0),
            })?;
        offset = i64::try_from(relative)
            .ok()
            .and_then(|relative| relative.checked_mul(stride))
            .and_then(|step| offset.checked_add(step))
            .ok_or(VariableUnavailableReason::EvaluationLimit)?;
    }

    Ok(offset)
}

impl DwarfVariableInfo {
    /// The text of the array bounded at run time `shape` in `storage`, when
    /// it is a string.
    pub(super) fn runtime_array_text(
        &self,
        type_id: TypeId,
        shape: &ValueShape,
        storage: &ValueStorage,
        address: Option<ImageAddress>,
        runtime: &mut dyn VariableRuntime,
        budget: &mut InspectionBudget,
    ) -> Option<TextSummary> {
        let Ok(Resolved::Elements {
            data,
            dimensions,
            strides,
        }) = self.resolve_runtime_array(shape, storage, address, runtime, budget)
        else {
            return None;
        };

        let placement = Resolved::placement(&dimensions, &strides)?;
        self.resolved_text(type_id, shape, &data, &placement, runtime, budget)
    }

    /// The text of an array bounded at run time whose elements, at `data`,
    /// `placement` found, when they are adjacent characters.
    pub(super) fn resolved_text(
        &self,
        type_id: TypeId,
        shape: &ValueShape,
        data: &ValueStorage,
        placement: &ArrayPlacement,
        runtime: &mut dyn VariableRuntime,
        budget: &mut InspectionBudget,
    ) -> Option<TextSummary> {
        let ValueShape::RuntimeArray {
            element,
            element_size,
            ordering,
            ..
        } = shape
        else {
            return None;
        };

        let [dimension] = placement.dimensions.as_ref() else {
            return None;
        };

        if i64::try_from(*element_size).ok() != placement.strides.first().copied() {
            return None;
        }

        // Its characters are adjacent, as a static array's.
        let contiguous = ValueShape::Array {
            element: *element,
            dimensions: Arc::clone(&placement.dimensions),
            ordering: *ordering,
            byte_size: dimension.count.saturating_mul(*element_size),
        };

        let value = VariableValue::Array {
            dimensions: Arc::clone(&placement.dimensions),
        };

        self.text_summary(type_id, &contiguous, &value, data, None, runtime, budget)
    }

    /// Finds what the value of the array bounded at run time `shape` in
    /// `storage` is, in the frame executing `address`.
    pub(super) fn resolve_runtime_array(
        &self,
        shape: &ValueShape,
        storage: &ValueStorage,
        address: Option<ImageAddress>,
        runtime: &mut dyn VariableRuntime,
        budget: &mut InspectionBudget,
    ) -> Result<Resolved, EvaluateError> {
        let ValueShape::RuntimeArray {
            array,
            element_size,
            dimensions,
            ordering,
            ..
        } = shape
        else {
            return Err(EvaluateError::Fatal(
                "only arrays bounded at run time resolve here".into(),
            ));
        };

        // Expressions find the array's parts from where it is, and from
        // the frame, whose base is its function's.
        let object = match storage {
            ValueStorage::Memory(address) => Some(*address),
            _ => None,
        };

        let frame_base_location = address
            .and_then(|address| self.catalog().function_at(address))
            .and_then(|function| function.objects().next())
            .map_or(Metadata::Absent(MetadataAbsence::NoFrameBase), |object| {
                object.frame_base()
            });
        let mut cache = FrameBaseCache::Empty;
        let mut frame_base = FrameBase::Lazy(FrameBaseContext {
            location: &frame_base_location,
            tables: self.locations(),
            address,
            cache: &mut cache,
        });
        let mut parts = Parts {
            info: self,
            array: *array,
            object,
            address,
            frame_base: &mut frame_base,
        };

        for (child, absent) in [
            (LayoutChild::Allocated, Resolved::NotAllocated),
            (LayoutChild::Associated, Resolved::NotAssociated),
        ] {
            if parts.value(child, runtime, budget)? == Some(0) {
                return Ok(absent);
            }
        }

        let data = parts
            .value(LayoutChild::DataLocation, runtime, budget)?
            .map_or_else(
                || storage.clone(),
                |data| ValueStorage::Memory(VirtualAddress::new(data)),
            );
        let mut bounds = Vec::with_capacity(dimensions.len());
        let mut strides = Vec::with_capacity(dimensions.len());

        for (index, dimension) in dimensions.iter().enumerate() {
            let (bound, stride) = parts.dimension(index, *dimension, runtime, budget)?;
            bounds.push(bound);
            strides.push(stride);
        }

        let strides = adjacent_strides(&bounds, &strides, *ordering, *element_size)?;
        Ok(Resolved::Elements {
            data,
            dimensions: bounds,
            strides,
        })
    }
}

/// The expressions of one array bounded at run time, evaluated for one
/// value of it.
struct Parts<'info, 'frame, 'base> {
    info: &'info DwarfVariableInfo,
    array: TypeId,
    object: Option<VirtualAddress>,
    address: Option<ImageAddress>,
    frame_base: &'frame mut FrameBase<'base>,
}

impl Parts<'_, '_, '_> {
    /// One dimension's lower bound and count, and its stride when it has
    /// its own.
    fn dimension(
        &mut self,
        index: usize,
        dimension: RuntimeDimension,
        runtime: &mut dyn VariableRuntime,
        budget: &mut InspectionBudget,
    ) -> Result<(Bound, Option<i64>), EvaluateError> {
        let RuntimeDimension {
            lower_bound,
            extent,
            byte_stride,
        } = dimension;
        let Ok(dimension_index) = u32::try_from(index) else {
            return Err(EvaluateError::Malformed(
                "the array has too many dimensions".into(),
            ));
        };

        let bound = |part| LayoutChild::Bound {
            dimension: dimension_index,
            part,
        };

        let lower = self.bound(lower_bound, bound(BoundPart::Lower), runtime, budget)?;

        let count =
            match extent {
                ArrayExtent::Upper(upper) => {
                    let upper = self.bound(upper, bound(BoundPart::Extent), runtime, budget)?;

                    // A dimension ending before it begins is empty, as
                    // Fortran's may be.
                    Some(
                        upper
                            .checked_sub(lower)
                            .and_then(|span| span.checked_add(1))
                            .map_or(0, |count| u64::try_from(count.max(0)).unwrap_or(u64::MAX)),
                    )
                }
                ArrayExtent::Count(count) => {
                    let count = self.bound(count, bound(BoundPart::Extent), runtime, budget)?;
                    Some(u64::try_from(count).map_err(|_| {
                        EvaluateError::Malformed("an array's count is negative".into())
                    })?)
                }
                ArrayExtent::Unknown => None,
            };

        let stride = match byte_stride {
            Some(stride) => {
                let stride = self.bound(stride, bound(BoundPart::Stride), runtime, budget)?;
                Some(
                    i64::try_from(stride)
                        .map_err(|_| VariableUnavailableReason::EvaluationLimit)?,
                )
            }
            None => None,
        };

        Ok(((lower, count), stride))
    }

    /// The value the expression for `child` computes, or `None` when the
    /// array has no such expression.
    fn value(
        &mut self,
        child: LayoutChild,
        runtime: &mut dyn VariableRuntime,
        budget: &mut InspectionBudget,
    ) -> Result<Option<u64>, EvaluateError> {
        let Some(expression) = self.info.type_facts().dynamic_layout(self.array, child) else {
            return Ok(None);
        };

        let tables = self.info.locations();
        let pieces = evaluate_with_object(
            tables.expression(expression),
            self.info.endian,
            self.address,
            self.frame_base,
            runtime,
            budget,
            self.object,
        )?;
        let [piece] = pieces.as_slice() else {
            return Err(EvaluateError::Malformed(
                "an array's expression computed several pieces".into(),
            ));
        };

        let value = match piece.location {
            Location::Address { address } => address,
            Location::Value { value } => value.to_u64(u64::MAX).map_err(|_| {
                EvaluateError::Malformed("an array's expression is not an integer".into())
            })?,
            _ => {
                return Err(EvaluateError::Malformed(
                    "an array's expression computed no value".into(),
                ));
            }
        };

        Ok(Some(value))
    }

    /// The value of one bound, count, or stride.
    fn bound(
        &mut self,
        bound: ArrayBound,
        child: LayoutChild,
        runtime: &mut dyn VariableRuntime,
        budget: &mut InspectionBudget,
    ) -> Result<i128, EvaluateError> {
        let missing = || EvaluateError::Malformed("an array's bound has no expression".into());

        match bound {
            ArrayBound::Constant(value) => Ok(value),
            ArrayBound::Computed { byte_size, signed } => {
                let value = self.value(child, runtime, budget)?.ok_or_else(missing)?;
                Ok(integer(
                    &value.to_le_bytes(),
                    byte_size.min(8),
                    signed,
                    crate::ByteOrder::Little,
                ))
            }
            ArrayBound::Stored { byte_size, signed } => {
                let Some(expression) = self.info.type_facts().dynamic_layout(self.array, child)
                else {
                    return Err(missing());
                };

                let tables = self.info.locations();
                let pieces = evaluate_with_object(
                    tables.expression(expression),
                    self.info.endian,
                    self.address,
                    self.frame_base,
                    runtime,
                    budget,
                    self.object,
                )?;

                let storage = storage_from_pieces(
                    &pieces,
                    u64::from(byte_size),
                    None,
                    self.info.endian,
                    self.info.target,
                    runtime,
                )?;
                let (_, raw) = storage::read(&storage, usize::from(byte_size), runtime, budget)?;

                Ok(integer(
                    &raw,
                    byte_size,
                    signed,
                    self.info.target.byte_order,
                ))
            }
            ArrayBound::Variable { debug_info_offset } => {
                let variable = self
                    .info
                    .catalog()
                    .object_at_offset(debug_info_offset)
                    .ok_or(crate::UnsupportedVariableFeature::CrossDieEvaluation)?;
                let TypeResolution::Resolved(ty) = variable.type_info() else {
                    return Err(EvaluateError::Malformed(
                        "an array's bound is a variable of no type".into(),
                    ));
                };

                let base = match self.info.value_shape(ty)? {
                    ValueShape::Scalar(base) => base,
                    ValueShape::Enumeration { representation, .. } => representation,
                    _ => {
                        return Err(EvaluateError::Malformed(
                            "an array's bound is a variable that is not an integer".into(),
                        ));
                    }
                };

                let mut cache = FrameBaseCache::Empty;
                let storage = self.info.located_data_object(
                    variable,
                    self.address,
                    runtime,
                    &mut cache,
                    budget,
                )?;
                let size = usize::try_from(base.byte_size)
                    .map_err(|_| VariableUnavailableReason::EvaluationLimit)?;
                let (_, raw) = storage::read(&storage, size, runtime, budget)?;

                match decode_scalar(&base, &raw, self.info.target) {
                    Ok(ScalarValue::Signed(value)) => Ok(value),
                    Ok(ScalarValue::Unsigned(value)) => i128::try_from(value)
                        .map_err(|_| VariableUnavailableReason::EvaluationLimit.into()),
                    _ => Err(EvaluateError::Malformed(
                        "an array's bound is a variable that is not an integer".into(),
                    )),
                }
            }
        }
    }
}

/// Each dimension's stride: its own, or else the one that makes the
/// elements adjacent in the array's order. Only the outermost dimension
/// may go on without end.
fn adjacent_strides(
    bounds: &[Bound],
    strides: &[Option<i64>],
    ordering: ArrayOrdering,
    element_size: u64,
) -> Result<Vec<i64>, EvaluateError> {
    let mut span =
        i64::try_from(element_size).map_err(|_| VariableUnavailableReason::EvaluationLimit)?;
    let mut order = (0..bounds.len()).collect::<Vec<_>>();
    if ordering == ArrayOrdering::RowMajor {
        order.reverse();
    }

    let mut adjacent = vec![0; bounds.len()];

    for (position, &index) in order.iter().enumerate() {
        adjacent[index] = strides[index].unwrap_or(span);
        span = match bounds[index].1 {
            Some(count) => i64::try_from(count)
                .ok()
                .and_then(|count| adjacent[index].checked_mul(count))
                .ok_or(VariableUnavailableReason::EvaluationLimit)?,
            None if position + 1 == order.len() => 0,
            None => return Err(crate::UnsupportedVariableFeature::TypeRepresentation.into()),
        };
    }

    Ok(adjacent)
}

/// The integer of `byte_size` bytes at the start of `bytes`.
fn integer(bytes: &[u8], byte_size: u8, signed: bool, byte_order: crate::ByteOrder) -> i128 {
    let size = usize::from(byte_size).min(bytes.len()).min(16);
    let bytes = match byte_order {
        crate::ByteOrder::Little => &bytes[..size],
        crate::ByteOrder::Big => &bytes[bytes.len() - size..],
    };

    let raw = unsigned_value(bytes, byte_order).unwrap_or(0);
    let bits = size * 8;

    if signed && bits < 128 && bits > 0 {
        let shift = 128 - bits;
        (raw << shift).cast_signed() >> shift
    } else {
        raw.cast_signed()
    }
}
