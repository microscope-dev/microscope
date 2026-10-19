//! Selecting, reading, and describing a value's storage. These are the only
//! operations that look inside a composite: selecting part of one that lies
//! within a single piece yields that piece's own storage, so everything else
//! treats it as any value in that place.

use std::sync::Arc;

use crate::debug_info::VariableRuntime;
use crate::model::{CompositeStorage, PieceLocation, StoragePiece, ValueStorage};
use crate::{
    ByteOrder, OptimizedOutReason, ValueBitRange, VariableUnavailableReason, VariableValueSource,
    VirtualAddress,
};

use super::InspectionBudget;
use super::codec::extract_bit_field;
use super::evaluate::EvaluateError;

/// The storage `offset` bytes into `storage`, which may select less of it.
pub(super) fn offset(storage: ValueStorage, offset: i64) -> Result<ValueStorage, EvaluateError> {
    let outside =
        || EvaluateError::Malformed("member offset is outside its containing value".into());

    match storage {
        ValueStorage::Memory(address) => address
            .get()
            .checked_add_signed(offset)
            .map(|address| ValueStorage::Memory(VirtualAddress::new(address)))
            .ok_or({
                EvaluateError::Unavailable(VariableUnavailableReason::ValueAccess(
                    crate::ValueAccessUnavailableReason::AddressOverflow,
                ))
            }),
        ValueStorage::Bytes {
            source,
            raw,
            start,
            end,
            address,
        } => {
            let delta =
                isize::try_from(offset).map_err(|_| VariableUnavailableReason::EvaluationLimit)?;
            let start = start
                .checked_add_signed(delta)
                .filter(|start| *start <= end)
                .ok_or_else(outside)?;
            let address = address.and_then(|address| {
                address
                    .get()
                    .checked_add_signed(offset)
                    .map(VirtualAddress::new)
            });

            Ok(ValueStorage::Bytes {
                source,
                raw,
                start,
                end,
                address,
            })
        }
        ValueStorage::ImplicitPointer { .. } => Err(EvaluateError::Malformed(
            "an unresolved implicit pointer cannot be offset".into(),
        )),
        ValueStorage::Composite(CompositeStorage { pieces, start }) => {
            let start = offset
                .checked_mul(8)
                .and_then(|bits| start.checked_add_signed(bits))
                .filter(|start| *start <= composite_bits(&pieces))
                .ok_or_else(outside)?;
            Ok(ValueStorage::Composite(CompositeStorage { pieces, start }))
        }
    }
}

/// The storage of the first `size` bytes of `storage`: a composite's own
/// piece's storage when they lie within one.
pub(super) fn narrow(storage: ValueStorage, size: u64) -> ValueStorage {
    let ValueStorage::Composite(composite) = &storage else {
        return storage;
    };

    let Some(end) = size
        .checked_mul(8)
        .and_then(|bits| composite.start.checked_add(bits))
    else {
        return storage;
    };

    let Some(piece) = composite
        .pieces
        .iter()
        .find(|piece| piece.offset <= composite.start && end <= piece.offset + piece.size)
    else {
        return storage;
    };

    let within = composite.start - piece.offset;
    let Ok(byte_size) = usize::try_from(size) else {
        return storage;
    };

    match &piece.location {
        PieceLocation::Memory {
            address,
            bit_offset,
        } if (bit_offset + within) % 8 == 0 => address
            .get()
            .checked_add((bit_offset + within) / 8)
            .map_or(storage, |address| {
                ValueStorage::Memory(VirtualAddress::new(address))
            }),
        PieceLocation::Bytes {
            source,
            raw,
            bit_offset,
        } if (bit_offset + within) % 8 == 0 => {
            // Pieces are validated to lie within their bytes.
            let Some((start, end)) = usize::try_from((bit_offset + within) / 8)
                .ok()
                .and_then(|start| Some((start, start.checked_add(byte_size)?)))
            else {
                return storage;
            };

            ValueStorage::Bytes {
                source: source.clone(),
                raw: Arc::clone(raw),
                start,
                end,
                address: None,
            }
        }

        PieceLocation::ImplicitPointer {
            debug_info_offset,
            byte_offset,
        } if within == 0 && end == piece.offset + piece.size => ValueStorage::ImplicitPointer {
            debug_info_offset: *debug_info_offset,
            byte_offset: *byte_offset,
        },
        _ => storage,
    }
}

/// Reads the first `size` bytes of `storage`, with where they came from.
pub(super) fn read(
    storage: &ValueStorage,
    size: usize,
    runtime: &mut dyn VariableRuntime,
    budget: &mut InspectionBudget,
) -> Result<(VariableValueSource, Arc<[u8]>), EvaluateError> {
    match storage {
        ValueStorage::Memory(address) => {
            budget.consume_memory(size)?;
            let raw = runtime.read_memory(*address, size)?;
            Ok((VariableValueSource::Memory(*address), raw))
        }

        ValueStorage::Bytes {
            raw, start, end, ..
        } => {
            let selected_end = start
                .checked_add(size)
                .filter(|selected_end| *selected_end <= *end)
                .ok_or_else(|| {
                    EvaluateError::Malformed(
                        "selected value extends beyond its containing storage".into(),
                    )
                })?;
            Ok((source(storage), Arc::from(&raw[*start..selected_end])))
        }
        ValueStorage::ImplicitPointer { .. } => Err(EvaluateError::Malformed(
            "an unresolved implicit pointer cannot be read".into(),
        )),
        ValueStorage::Composite(composite) => {
            let bits = u64::try_from(size)
                .ok()
                .and_then(|size| size.checked_mul(8))
                .ok_or(VariableUnavailableReason::EvaluationLimit)?;
            let raw = read_composite(composite, composite.start, bits, runtime, budget)?;
            Ok((VariableValueSource::Composite, raw.into()))
        }
    }
}

/// Reads `bit_size` bits from `bit_offset` bits into `storage`, as an
/// unsigned integer, reading only those bits of a composite.
pub(super) fn read_bits(
    storage: &ValueStorage,
    bit_offset: u64,
    bit_size: u64,
    byte_order: ByteOrder,
    runtime: &mut dyn VariableRuntime,
    budget: &mut InspectionBudget,
) -> Result<u128, EvaluateError> {
    if bit_size == 0 || bit_size > 128 {
        return Err(EvaluateError::Malformed(
            "bit-field width exceeds its declared scalar storage".into(),
        ));
    }

    if let ValueStorage::Composite(composite) = storage {
        let start = composite
            .start
            .checked_add(bit_offset)
            .ok_or(VariableUnavailableReason::EvaluationLimit)?;
        let raw = read_composite(composite, start, bit_size, runtime, budget)?;
        let mut wide = [0; 16];
        wide[..raw.len()].copy_from_slice(&raw);
        return Ok(u128::from_le_bytes(wide));
    }

    let first_byte = bit_offset / 8;
    let span = (bit_offset % 8 + bit_size).div_ceil(8);
    let selected = offset(
        storage.clone(),
        i64::try_from(first_byte).map_err(|_| VariableUnavailableReason::EvaluationLimit)?,
    )?;
    let span = usize::try_from(span).map_err(|_| VariableUnavailableReason::EvaluationLimit)?;
    let (_, bytes) = read(&selected, span, runtime, budget)?;

    extract_bit_field(&bytes, bit_offset % 8, bit_size, byte_order)
        .map_err(EvaluateError::Malformed)
}

/// Where a value in `storage` comes from. Bytes read once for a whole page
/// of elements still report each element's own address, and part of a
/// register above its least significant byte is not the register's value.
pub(super) fn source(storage: &ValueStorage) -> VariableValueSource {
    match storage {
        ValueStorage::Memory(address)
        | ValueStorage::Bytes {
            address: Some(address),
            ..
        } => VariableValueSource::Memory(*address),
        ValueStorage::Bytes {
            source: VariableValueSource::Register(_),
            start: 1..,
            ..
        }
        | ValueStorage::Composite(_) => VariableValueSource::Composite,
        ValueStorage::Bytes { source, .. } => source.clone(),
        ValueStorage::ImplicitPointer { .. } => VariableValueSource::ImplicitPointer,
    }
}

/// The address of the object `storage` holds, when it is in memory.
pub(super) const fn concrete_address(storage: &ValueStorage) -> Option<VirtualAddress> {
    match storage {
        ValueStorage::Memory(address) => Some(*address),
        ValueStorage::Bytes { address, .. } => *address,
        ValueStorage::ImplicitPointer { .. } | ValueStorage::Composite(_) => None,
    }
}

/// How many bits a composite's pieces cover.
pub(super) fn composite_bits(pieces: &[StoragePiece]) -> u64 {
    pieces.last().map_or(0, |piece| piece.offset + piece.size)
}

/// Assembles `bits` bits of a composite from bit `start`, least significant
/// first, or reports the undefined bits or unavailable place they include.
fn read_composite(
    composite: &CompositeStorage,
    start: u64,
    bits: u64,
    runtime: &mut dyn VariableRuntime,
    budget: &mut InspectionBudget,
) -> Result<Vec<u8>, EvaluateError> {
    let end = start
        .checked_add(bits)
        .filter(|end| *end <= composite_bits(&composite.pieces))
        .ok_or_else(|| {
            EvaluateError::Malformed("selected value extends beyond its containing storage".into())
        })?;
    let overlapping = || {
        composite
            .pieces
            .iter()
            .filter(move |piece| piece.offset < end && start < piece.offset + piece.size)
            .map(move |piece| {
                let from = piece.offset.max(start);
                let to = (piece.offset + piece.size).min(end);
                (piece, from - piece.offset, from - start, to - from)
            })
    };

    // Nothing is read unless every bit can be.
    let mut undefined: Vec<ValueBitRange> = Vec::new();
    let mut unavailable = None;

    for (piece, _, destination, size) in overlapping() {
        match &piece.location {
            // The bits of an implicit pointer are the pointer, which has no
            // value, only a referent.
            PieceLocation::Undefined | PieceLocation::ImplicitPointer { .. } => {
                match undefined.last_mut() {
                    Some(last) if last.offset + last.size == destination => last.size += size,
                    _ => undefined.push(ValueBitRange {
                        offset: destination,
                        size,
                    }),
                }
            }
            PieceLocation::Unavailable(reason) => {
                unavailable.get_or_insert_with(|| reason.clone());
            }
            PieceLocation::Memory { .. } | PieceLocation::Bytes { .. } => {}
        }
    }

    match undefined.as_slice() {
        [] => {}
        [only] if only.size == bits => {
            return Err(
                VariableUnavailableReason::OptimizedOut(OptimizedOutReason::EmptyLocation).into(),
            );
        }
        _ => {
            return Err(VariableUnavailableReason::OptimizedOut(
                OptimizedOutReason::UndefinedPieces {
                    ranges: undefined.into(),
                },
            )
            .into());
        }
    }

    if let Some(reason) = unavailable {
        return Err(reason.into());
    }

    let length = usize::try_from(bits.div_ceil(8))
        .map_err(|_| VariableUnavailableReason::EvaluationLimit)?;
    let mut value = vec![0; length];

    for (piece, within, destination, size) in overlapping() {
        match &piece.location {
            PieceLocation::Memory {
                address,
                bit_offset,
            } => {
                let first = bit_offset + within;
                let length = usize::try_from((first % 8 + size).div_ceil(8))
                    .map_err(|_| VariableUnavailableReason::EvaluationLimit)?;
                let address = address
                    .get()
                    .checked_add(first / 8)
                    .map(VirtualAddress::new)
                    .ok_or(VariableUnavailableReason::ValueAccess(
                        crate::ValueAccessUnavailableReason::AddressOverflow,
                    ))?;
                budget.consume_memory(length)?;
                let raw = runtime.read_memory(address, length)?;
                copy_bits(&raw, first % 8, &mut value, destination, size)?;
            }

            PieceLocation::Bytes {
                raw, bit_offset, ..
            } => copy_bits(raw, bit_offset + within, &mut value, destination, size)?,
            PieceLocation::Undefined
            | PieceLocation::ImplicitPointer { .. }
            | PieceLocation::Unavailable(_) => {
                return Err(EvaluateError::Fatal(
                    "pieces without bits were not refused".into(),
                ));
            }
        }
    }

    Ok(value)
}

/// Copies `size` bits, numbered from each byte's least significant bit.
fn copy_bits(
    source: &[u8],
    from: u64,
    destination: &mut [u8],
    to: u64,
    size: u64,
) -> Result<(), EvaluateError> {
    let index =
        |bit: u64| usize::try_from(bit / 8).map_err(|_| VariableUnavailableReason::EvaluationLimit);

    if from.is_multiple_of(8) && to.is_multiple_of(8) && size.is_multiple_of(8) {
        let (from, to, length) = (index(from)?, index(to)?, index(size)?);
        destination[to..to + length].copy_from_slice(&source[from..from + length]);
        return Ok(());
    }

    for bit in 0..size {
        let (source_bit, destination_bit) = (from + bit, to + bit);
        let value = (source[index(source_bit)?] >> (source_bit % 8)) & 1;
        destination[index(destination_bit)?] |= value << (destination_bit % 8);
    }

    Ok(())
}
