//! Turning the pieces a location expression describes into storage, resolved
//! once at the stop: registers, computed values, and implicit values are
//! captured as bytes, memory stays an address read when needed, and implicit
//! pointers and undefined bits stay what they are.

use std::sync::Arc;

use gimli::{Location, Piece, Reader as _, RunTimeEndian, Value};

use crate::debug_info::dwarf::Reader;
use crate::debug_info::{VariableRuntime, VariableRuntimeError};
use crate::model::{CompositeStorage, PieceLocation, StoragePiece, ValueStorage};
use crate::{
    BaseType, OptimizedOutReason, TargetDescription, VariableUnavailableReason,
    VariableValueSource, VirtualAddress,
};

use super::codec::dwarf_address_bytes;
use super::evaluate::{EvaluateError, dwarf_value_bytes, evaluation_error};
use super::storage;
use super::{MAX_EVALUATION_MEMORY_BYTES, MAX_LOCATION_PIECES};

/// The storage of a `byte_size`-byte object that `pieces` describe, whose
/// scalar type is `scalar` when it has one.
pub(super) fn storage_from_pieces(
    pieces: &[Piece<Reader<'_>>],
    byte_size: u64,
    scalar: Option<&BaseType>,
    endian: RunTimeEndian,
    target: TargetDescription,
    runtime: &mut dyn VariableRuntime,
) -> Result<ValueStorage, EvaluateError> {
    if pieces.len() > MAX_LOCATION_PIECES {
        return Err(VariableUnavailableReason::EvaluationLimit.into());
    }

    let bits = byte_size
        .checked_mul(8)
        .ok_or(VariableUnavailableReason::EvaluationLimit)?;

    match pieces {
        [] => Err(EvaluateError::Malformed(
            "DWARF location expression produced no pieces".into(),
        )),
        [piece]
            if piece.size_in_bits.is_none_or(|size| size == bits)
                && piece.bit_offset.is_none_or(|offset| offset == 0) =>
        {
            whole(piece, byte_size, scalar, endian, target, runtime)
        }
        _ if endian == RunTimeEndian::Big => {
            Err(crate::UnsupportedVariableFeature::CompositeLocation.into())
        }
        _ => composite(pieces, byte_size, bits, runtime),
    }
}

/// The storage of an object one location describes entirely.
fn whole(
    piece: &Piece<Reader<'_>>,
    byte_size: u64,
    scalar: Option<&BaseType>,
    endian: RunTimeEndian,
    target: TargetDescription,
    runtime: &mut dyn VariableRuntime,
) -> Result<ValueStorage, EvaluateError> {
    let size =
        usize::try_from(byte_size).map_err(|_| VariableUnavailableReason::EvaluationLimit)?;
    let bytes = |source, raw: Arc<[u8]>| {
        let end = raw.len();
        ValueStorage::Bytes {
            source,
            raw,
            start: 0,
            end,
            address: None,
        }
    };

    match &piece.location {
        Location::Empty => {
            Err(VariableUnavailableReason::OptimizedOut(OptimizedOutReason::EmptyLocation).into())
        }
        Location::Address { address } => Ok(ValueStorage::Memory(VirtualAddress::new(*address))),
        Location::Register { register } => {
            let register = runtime.register((*register).into())?;
            if register.bytes.len() < size {
                return Err(EvaluateError::Malformed(
                    "register value is shorter than the scalar type".into(),
                ));
            }

            let raw = match endian {
                RunTimeEndian::Little => Arc::from(&register.bytes[..size]),
                RunTimeEndian::Big => Arc::from(&register.bytes[register.bytes.len() - size..]),
            };

            Ok(bytes(
                VariableValueSource::Register(register.descriptor),
                raw,
            ))
        }
        Location::Value { value } => Ok(bytes(
            VariableValueSource::Computed,
            match scalar {
                Some(scalar) => dwarf_value_bytes(*value, scalar, target)?,
                // An object wider than the value extends it as a piece would.
                None if size > 8 => {
                    let mut raw = extended_value(*value, byte_size.saturating_mul(8))?.to_vec();
                    raw.truncate(size);
                    raw.into()
                }
                None => dwarf_address_bytes(*value, size, target)?,
            },
        )),
        Location::Bytes { value } => {
            // Like a piece of one, an implicit value must cover the object.
            let raw = implicit_bytes(value)?;
            if raw.len() < size {
                return Err(EvaluateError::Malformed(
                    "implicit value is shorter than its object".into(),
                ));
            }

            Ok(bytes(
                VariableValueSource::Constant,
                Arc::from(&raw[..size]),
            ))
        }
        Location::ImplicitPointer { value, byte_offset } => Ok(ValueStorage::ImplicitPointer {
            debug_info_offset: u64::try_from(value.0)
                .map_err(|_| VariableUnavailableReason::EvaluationLimit)?,
            byte_offset: *byte_offset,
        }),
    }
}

/// The storage of an object several pieces describe, or one that describes
/// only part of it.
fn composite(
    pieces: &[Piece<Reader<'_>>],
    byte_size: u64,
    bits: u64,
    runtime: &mut dyn VariableRuntime,
) -> Result<ValueStorage, EvaluateError> {
    let mut resolved: Vec<StoragePiece> = Vec::with_capacity(pieces.len() + 1);
    let mut offset = 0_u64;

    for piece in pieces {
        let size = match piece.size_in_bits {
            Some(size) => size,
            None if pieces.len() == 1 => bits,
            None => {
                return Err(EvaluateError::Malformed(
                    "one of multiple DWARF location pieces has no size".into(),
                ));
            }
        };

        let end = offset
            .checked_add(size)
            .filter(|end| *end <= bits)
            .ok_or_else(|| {
                EvaluateError::Malformed(
                    "DWARF location pieces exceed the declared value size".into(),
                )
            })?;
        let location = piece_location(
            &piece.location,
            piece.bit_offset.unwrap_or(0),
            size,
            runtime,
        )?;
        push(
            &mut resolved,
            StoragePiece {
                offset,
                size,
                location,
            },
        );
        offset = end;
    }

    // Bits no piece describes are undefined (DWARF 5 section 2.6.1.2).
    if offset < bits {
        push(
            &mut resolved,
            StoragePiece {
                offset,
                size: bits - offset,
                location: PieceLocation::Undefined,
            },
        );
    }

    if resolved
        .iter()
        .all(|piece| piece.location == PieceLocation::Undefined)
    {
        return Err(
            VariableUnavailableReason::OptimizedOut(OptimizedOutReason::EmptyLocation).into(),
        );
    }

    let composite = ValueStorage::Composite(CompositeStorage {
        pieces: resolved.into(),
        start: 0,
    });
    Ok(storage::narrow(composite, byte_size))
}

/// Appends a piece, joining it to the previous one when together they are
/// one run of memory or of undefined bits.
fn push(pieces: &mut Vec<StoragePiece>, piece: StoragePiece) {
    if let Some(last) = pieces.last_mut() {
        let joined = match (&last.location, &piece.location) {
            (PieceLocation::Undefined, PieceLocation::Undefined) => true,
            (
                PieceLocation::Memory {
                    address: first,
                    bit_offset: first_offset,
                },
                PieceLocation::Memory {
                    address: second,
                    bit_offset: second_offset,
                },
            ) => {
                first
                    .get()
                    .checked_mul(8)
                    .and_then(|bit| bit.checked_add(first_offset + last.size))
                    == second
                        .get()
                        .checked_mul(8)
                        .and_then(|bit| bit.checked_add(*second_offset))
            }
            _ => false,
        };

        if joined {
            last.size += piece.size;
            return;
        }
    }

    pieces.push(piece);
}

/// Where a piece of `size` bits from `bit_offset` within its location is,
/// under the rules of DWARF 5 section 2.6.1.2: register and value pieces
/// take their low-order bits.
fn piece_location(
    location: &Location<Reader<'_>>,
    bit_offset: u64,
    size: u64,
    runtime: &mut dyn VariableRuntime,
) -> Result<PieceLocation, EvaluateError> {
    let needed = bit_offset
        .checked_add(size)
        .ok_or_else(|| EvaluateError::Malformed("DWARF location piece range overflows".into()))?;
    let covers = |raw: &[u8]| {
        u64::try_from(raw.len())
            .ok()
            .and_then(|length| length.checked_mul(8))
            .is_some_and(|available| needed <= available)
    };

    Ok(match location {
        Location::Empty => PieceLocation::Undefined,
        Location::Address { address } => PieceLocation::Memory {
            address: VirtualAddress::new(*address),
            bit_offset,
        },
        Location::Register { register } => match runtime.register((*register).into()) {
            Ok(register) => {
                if !covers(&register.bytes) {
                    return Err(EvaluateError::Malformed(
                        "a DWARF location piece extends past its register".into(),
                    ));
                }

                PieceLocation::Bytes {
                    source: VariableValueSource::Register(register.descriptor),
                    raw: register.bytes,
                    bit_offset,
                }
            }

            // The other pieces may still be read.
            Err(VariableRuntimeError::Unavailable(reason)) => PieceLocation::Unavailable(reason),
            Err(error) => return Err(error.into()),
        },
        Location::Value { value } => PieceLocation::Bytes {
            source: VariableValueSource::Computed,
            raw: extended_value(*value, needed)?,
            bit_offset,
        },
        Location::Bytes { value } => {
            let raw = implicit_bytes(value)?;
            if !covers(&raw) {
                return Err(EvaluateError::Malformed(
                    "a DWARF location piece extends past its implicit value".into(),
                ));
            }

            PieceLocation::Bytes {
                source: VariableValueSource::Constant,
                raw,
                bit_offset,
            }
        }
        Location::ImplicitPointer { value, byte_offset } => {
            if bit_offset != 0 {
                return Err(EvaluateError::Malformed(
                    "an implicit pointer piece has a bit offset".into(),
                ));
            }

            PieceLocation::ImplicitPointer {
                debug_info_offset: u64::try_from(value.0)
                    .map_err(|_| VariableUnavailableReason::EvaluationLimit)?,
                byte_offset: *byte_offset,
            }
        }
    })
}

/// A computed value's bytes, least significant first, covering `needed`
/// bits: a typed integer extends by its signedness, and a generic value by
/// zeros only when that does not change what it means.
fn extended_value(value: Value, needed: u64) -> Result<Arc<[u8]>, EvaluateError> {
    let (bytes, signed): (Vec<u8>, Option<bool>) = match value {
        Value::Generic(value) => (value.to_le_bytes().to_vec(), None),
        Value::U8(value) => (vec![value], Some(false)),
        Value::U16(value) => (value.to_le_bytes().to_vec(), Some(false)),
        Value::U32(value) => (value.to_le_bytes().to_vec(), Some(false)),
        Value::U64(value) => (value.to_le_bytes().to_vec(), Some(false)),
        Value::I8(value) => (value.to_le_bytes().to_vec(), Some(true)),
        Value::I16(value) => (value.to_le_bytes().to_vec(), Some(true)),
        Value::I32(value) => (value.to_le_bytes().to_vec(), Some(true)),
        Value::I64(value) => (value.to_le_bytes().to_vec(), Some(true)),
        Value::F32(value) => (value.to_bits().to_le_bytes().to_vec(), None),
        Value::F64(value) => (value.to_bits().to_le_bytes().to_vec(), None),
    };

    let length = usize::try_from(needed.div_ceil(8))
        .ok()
        .filter(|length| *length <= MAX_EVALUATION_MEMORY_BYTES)
        .ok_or(VariableUnavailableReason::EvaluationLimit)?;
    if length <= bytes.len() {
        return Ok(bytes.into());
    }

    let negative = bytes.last().is_some_and(|byte| byte & 0x80 != 0);
    let fill = match (value, signed, negative) {
        // A float, or a generic value that zeros would make positive.
        (Value::F32(_) | Value::F64(_), ..) | (_, None, true) => None,
        (_, Some(true), true) => Some(0xff),
        (_, Some(_), _) | (_, None, false) => Some(0),
    };

    let Some(fill) = fill else {
        return Err(EvaluateError::Malformed(
            "a DWARF location piece is wider than the value it holds".into(),
        ));
    };

    let mut extended = bytes;
    extended.resize(length, fill);
    Ok(extended.into())
}

fn implicit_bytes(value: &Reader<'_>) -> Result<Arc<[u8]>, EvaluateError> {
    Ok(value
        .to_slice()
        .map_err(|error| EvaluateError::Malformed(evaluation_error(error)))?
        .into_owned()
        .into())
}
