//! Composite storage and the location pieces that build it, against a model
//! of each bit the object holds (DWARF 5 section 2.6.1.2).

use proptest::prelude::*;

use crate::model::{CompositeStorage, PieceLocation, StoragePiece};
use crate::test_strategies::u64_edges;

use super::*;

const BASE: u64 = 0x1000;

/// What a register holds at the stop.
#[derive(Debug, Clone, Copy)]
enum Register {
    Value(u64),
    NotSaved,
    Missing,
}

/// A thread at a stop: memory at `BASE` of which some bytes cannot be
/// read, and registers by number.
#[derive(Debug, Clone)]
struct Machine {
    bytes: Vec<u8>,
    readable: Vec<bool>,
    registers: Vec<Register>,
}

impl Machine {
    /// The bit `bit` bits past `BASE`, numbered from each byte's least
    /// significant bit.
    fn memory_bit(&self, bit: u64) -> Bit {
        let index = index(bit / 8);
        match (self.bytes.get(index), self.readable.get(index)) {
            (Some(byte), Some(true)) => Bit::Known(byte >> (bit % 8) & 1 == 1),
            _ => Bit::Unreadable(BASE + bit / 8),
        }
    }

    fn register_reason(&self, number: u16) -> Option<VariableUnavailableReason> {
        match self.registers.get(usize::from(number)) {
            Some(Register::Value(_)) => None,
            Some(Register::NotSaved) => Some(VariableUnavailableReason::RegisterNotSaved(
                format!("r{number}").into(),
            )),
            Some(Register::Missing) | None => Some(VariableUnavailableReason::RegisterUnavailable(
                format!("r{number}").into(),
            )),
        }
    }
}

impl VariableRuntime for Machine {
    fn register(
        &mut self,
        register: DwarfRegister,
    ) -> std::result::Result<crate::debug_info::VariableRegister, VariableRuntimeError> {
        let number = register.number();
        let Some(Register::Value(value)) = self.registers.get(usize::from(number)) else {
            return Err(VariableRuntimeError::Unavailable(
                self.register_reason(number)
                    .expect("a register without a value has a reason"),
            ));
        };

        Ok(crate::debug_info::VariableRegister {
            descriptor: crate::RegisterDescriptor {
                id: crate::RegisterId::new(u32::from(number)),
                name: format!("r{number}").into(),
                bits: 64,
                role: None,
            },
            bytes: Arc::from(value.to_le_bytes()),
        })
    }

    fn call_frame_cfa(&self) -> std::result::Result<VirtualAddress, VariableRuntimeError> {
        Err(VariableRuntimeError::Fatal("no frame".into()))
    }

    fn tls_address(
        &mut self,
        _offset: u64,
    ) -> std::result::Result<VirtualAddress, VariableUnavailableReason> {
        Err(crate::UnsupportedVariableFeature::Tls.into())
    }

    fn relocate(&self, address: ImageAddress) -> std::result::Result<VirtualAddress, Arc<str>> {
        Ok(VirtualAddress::new(address.get()))
    }

    fn image_address(&self, address: VirtualAddress) -> Option<ImageAddress> {
        Some(ImageAddress::new(address.get()))
    }

    /// Reads up to the first byte that cannot be read, as a process would.
    fn read_memory(
        &mut self,
        address: VirtualAddress,
        size: usize,
    ) -> std::result::Result<Arc<[u8]>, VariableRuntimeError> {
        let mut read = Vec::with_capacity(size);
        for completed in 0..size as u64 {
            let at = address.get().checked_add(completed);
            let byte = at
                .and_then(|at| at.checked_sub(BASE))
                .and_then(|offset| usize::try_from(offset).ok())
                .filter(|offset| self.readable.get(*offset) == Some(&true))
                .and_then(|offset| self.bytes.get(offset));
            let Some(byte) = byte else {
                return Err(VariableUnavailableReason::MemoryInaccessible {
                    address,
                    requested: size as u64,
                    completed,
                    next_address: VirtualAddress::new(at.unwrap_or(u64::MAX)),
                }
                .into());
            };

            read.push(*byte);
        }

        Ok(read.into())
    }

    fn entry_value(
        &mut self,
        _parameter: EntryParameter,
        _budget: &mut InspectionBudget,
    ) -> std::result::Result<u64, VariableRuntimeError> {
        Err(VariableRuntimeError::Fatal("no caller".into()))
    }
}

fn machine() -> impl Strategy<Value = Machine> {
    (
        proptest::collection::vec(any::<u8>(), 64),
        prop_oneof![1 => Just(None), 1 => (0_usize..64, 1_usize..8).prop_map(Some)],
        u64_edges(),
        any::<u64>(),
    )
        .prop_map(|(bytes, unreadable, first, second)| {
            let readable = (0..bytes.len())
                .map(|index| {
                    unreadable
                        .is_none_or(|(start, length)| !(start..start + length).contains(&index))
                })
                .collect();

            Machine {
                bytes,
                readable,
                registers: vec![
                    Register::Value(first),
                    Register::Value(second),
                    Register::NotSaved,
                    Register::Missing,
                ],
            }
        })
}

/// What an object holds in one bit.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Bit {
    Known(bool),
    Undefined,
    Unavailable(VariableUnavailableReason),

    /// Memory at this address cannot be read.
    Unreadable(u64),
}

/// A test's bit or byte count as an index.
fn index(count: u64) -> usize {
    usize::try_from(count).expect("test sizes fit usize")
}

/// The error a read that meets `address` first among unreadable memory
/// gives, keeping only that address: where a read starts depends on how
/// pieces split it.
fn inaccessible(address: u64) -> EvaluateError {
    VariableUnavailableReason::MemoryInaccessible {
        address: VirtualAddress::new(address),
        requested: 0,
        completed: 0,
        next_address: VirtualAddress::new(address),
    }
    .into()
}

/// A read's result with only what the model decides: the first address
/// that could not be read, and not why something is malformed.
fn outcome<T>(
    result: std::result::Result<T, EvaluateError>,
) -> std::result::Result<T, EvaluateError> {
    result.map_err(|error| match error {
        EvaluateError::Unavailable(VariableUnavailableReason::MemoryInaccessible {
            next_address,
            ..
        }) => inaccessible(next_address.get()),
        EvaluateError::Malformed(_) => EvaluateError::Malformed("".into()),
        error => error,
    })
}

/// What reading `bits` must produce: their bytes, least significant first,
/// or else the undefined ranges among them, then the first unavailable
/// place, then the first unreadable address.
fn expected_read(bits: &[Bit]) -> std::result::Result<Vec<u8>, EvaluateError> {
    let mut ranges: Vec<crate::ValueBitRange> = Vec::new();

    for (index, bit) in bits.iter().enumerate() {
        if *bit != Bit::Undefined {
            continue;
        }

        let index = index as u64;

        match ranges.last_mut() {
            Some(last) if last.offset + last.size == index => last.size += 1,
            _ => ranges.push(crate::ValueBitRange {
                offset: index,
                size: 1,
            }),
        }
    }

    match ranges.as_slice() {
        [] => {}
        [only] if only.size == bits.len() as u64 => {
            return Err(VariableUnavailableReason::OptimizedOut(
                crate::OptimizedOutReason::EmptyLocation,
            )
            .into());
        }
        _ => {
            return Err(VariableUnavailableReason::OptimizedOut(
                crate::OptimizedOutReason::UndefinedPieces {
                    ranges: ranges.into(),
                },
            )
            .into());
        }
    }

    if let Some(reason) = bits.iter().find_map(|bit| match bit {
        Bit::Unavailable(reason) => Some(reason.clone()),
        _ => None,
    }) {
        return Err(reason.into());
    }

    if let Some(address) = bits.iter().find_map(|bit| match bit {
        Bit::Unreadable(address) => Some(*address),
        _ => None,
    }) {
        return Err(inaccessible(address));
    }

    let mut bytes = vec![0_u8; bits.len().div_ceil(8)];

    for (index, bit) in bits.iter().enumerate() {
        if *bit == Bit::Known(true) {
            bytes[index / 8] |= 1 << (index % 8);
        }
    }

    Ok(bytes)
}

/// Where one piece of a built composite is.
#[derive(Debug, Clone)]
enum PieceKind {
    Memory { byte: u64, bit_offset: u64 },
    Bytes { raw: Vec<u8>, bit_offset: u64 },
    Undefined,
    Unavailable(u16),
    ImplicitPointer,
}

fn piece_kind() -> impl Strategy<Value = PieceKind> {
    prop_oneof![
        3 => (0_u64..72, 0_u64..8).prop_map(|(byte, bit_offset)| PieceKind::Memory { byte, bit_offset }),
        3 => (proptest::collection::vec(any::<u8>(), 8), 0_u64..8)
            .prop_map(|(raw, bit_offset)| PieceKind::Bytes { raw, bit_offset }),
        1 => Just(PieceKind::Undefined),
        1 => (2_u16..4).prop_map(PieceKind::Unavailable),
        1 => Just(PieceKind::ImplicitPointer),
    ]
}

/// The composite `kinds` describe, and each of its bits.
fn composite_and_bits(
    kinds: &[(u64, PieceKind)],
    machine: &Machine,
) -> (Vec<StoragePiece>, Vec<Bit>) {
    let mut pieces = Vec::new();
    let mut bits = Vec::new();
    let mut offset = 0;

    for (size, kind) in kinds {
        let (location, piece_bits): (PieceLocation, Vec<Bit>) = match kind {
            PieceKind::Memory { byte, bit_offset } => (
                PieceLocation::Memory {
                    address: VirtualAddress::new(BASE + byte),
                    bit_offset: *bit_offset,
                },
                (0..*size)
                    .map(|index| machine.memory_bit(byte * 8 + bit_offset + index))
                    .collect(),
            ),
            PieceKind::Bytes { raw, bit_offset } => (
                PieceLocation::Bytes {
                    source: VariableValueSource::Computed,
                    raw: Arc::from(raw.as_slice()),
                    bit_offset: *bit_offset,
                },
                (0..*size)
                    .map(|index| {
                        let bit = bit_offset + index;
                        Bit::Known(raw[index_of(bit)] >> (bit % 8) & 1 == 1)
                    })
                    .collect(),
            ),
            PieceKind::Undefined => (PieceLocation::Undefined, vec![Bit::Undefined; index(*size)]),
            PieceKind::Unavailable(register) => {
                let reason = machine
                    .register_reason(*register)
                    .expect("an unavailable register");
                (
                    PieceLocation::Unavailable(reason.clone()),
                    vec![Bit::Unavailable(reason); index(*size)],
                )
            }
            PieceKind::ImplicitPointer => (
                PieceLocation::ImplicitPointer {
                    debug_info_offset: 0x40,
                    byte_offset: 0,
                },
                vec![Bit::Undefined; index(*size)],
            ),
        };

        pieces.push(StoragePiece {
            offset,
            size: *size,
            location,
        });
        bits.extend(piece_bits);
        offset += size;
    }

    // Whole bytes, as an object has.
    let tail = offset.next_multiple_of(8) - offset;

    if tail != 0 {
        pieces.push(StoragePiece {
            offset,
            size: tail,
            location: PieceLocation::Undefined,
        });
        bits.extend(std::iter::repeat_n(Bit::Undefined, index(tail)));
    }

    (pieces, bits)
}

fn index_of(bit: u64) -> usize {
    index(bit / 8)
}

/// What selecting `size` bytes `from` bits into a composite of `bits`, and
/// reading them, gives.
fn expected_selection(
    bits: &[Bit],
    from: i64,
    size: u64,
) -> std::result::Result<Vec<u8>, EvaluateError> {
    let total = bits.len() as u64;
    let from = u64::try_from(from)
        .ok()
        .filter(|from| *from <= total)
        .ok_or_else(|| EvaluateError::Malformed("".into()))?;
    let end = from + size * 8;
    if end > total {
        return Err(EvaluateError::Malformed("".into()));
    }

    expected_read(&bits[index(from)..index(end)])
}

fn readable_machine() -> Machine {
    Machine {
        bytes: (0..64).collect(),
        readable: vec![true; 64],
        registers: Vec::new(),
    }
}

#[test]
fn reading_no_bytes_of_a_composite_reads_nothing() {
    let mut machine = readable_machine();

    // 17 bits of memory, then 7 undefined bits.
    let memory = PieceKind::Memory {
        byte: 0,
        bit_offset: 0,
    };
    let (pieces, _) = composite_and_bits(&[(17, memory)], &machine);
    let pieces: Arc<[StoragePiece]> = pieces.into();

    for start in [0, 3, 17, 20, 24] {
        let storage = ValueStorage::Composite(CompositeStorage {
            pieces: Arc::clone(&pieces),
            start,
        });
        let read = storage::read(&storage, 0, &mut machine, &mut InspectionBudget::default());
        assert_eq!(
            read.map(|(_, raw)| raw.to_vec()),
            Ok(Vec::new()),
            "from bit {start}"
        );
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(512))]

    /// Composite reads assemble exactly the bits they cover, from wherever
    /// a selection starts, and selecting part of a composite reads what the
    /// composite holds there. A read that meets memory it cannot read
    /// fails with the first such address.
    #[test]
    fn composite_reads_match_their_pieces_bit_for_bit(
        kinds in proptest::collection::vec((1_u64..=20, piece_kind()), 1..8),
        mut machine in machine(),
        start in any::<prop::sample::Index>(),
        delta in -3_i64..=3,
        size in 0_u64..=8,
        field in (any::<prop::sample::Index>(), 1_u64..=64),
        byte_order in prop::sample::select(vec![ByteOrder::Little, ByteOrder::Big]),
    ) {
        let (pieces, bits) = composite_and_bits(&kinds, &machine);
        let total = bits.len() as u64;
        let start = start.index(index(total) + 1) as u64;
        let storage = ValueStorage::Composite(CompositeStorage {
            pieces: pieces.into(),
            start,
        });

        // Selection moves by whole bytes from a start that need not be one.
        let from = i64::try_from(start).expect("a small composite") + delta * 8;
        let expected = expected_selection(&bits, from, size);
        let selected = storage::offset(storage.clone(), delta);
        let read = selected.clone().and_then(|selected| {
            storage::read(&selected, index(size), &mut machine, &mut InspectionBudget::default())
                .map(|(_, raw)| raw.to_vec())
        });
        prop_assert_eq!(outcome(read.clone()), outcome(expected));

        if let (Ok(selected), Ok(bytes)) = (selected, &read) {
            let narrowed = storage::narrow(selected, size);
            let again = storage::read(&narrowed, index(size), &mut machine, &mut InspectionBudget::default())
                .map(|(_, raw)| raw.to_vec());
            prop_assert_eq!(again.as_ref(), Ok(bytes), "narrowed to {:?}", narrowed);
        }

        // A composite numbers its bits from each byte's least significant
        // one, whatever the byte order; only little-endian targets have
        // composites.
        let (offset, width) = field;
        let offset = offset.index(index(total - start) + 1) as u64;
        let read_bits = storage::read_bits(
            &storage,
            offset,
            width,
            byte_order,
            &mut machine,
            &mut InspectionBudget::default(),
        );
        let end = start + offset + width;
        let expected = if end > total {
            Err(EvaluateError::Malformed("".into()))
        } else {
            expected_read(&bits[index(start + offset)..index(end)]).map(|bytes| {
                let mut wide = [0; 16];
                wide[..bytes.len()].copy_from_slice(&bytes);
                u128::from_le_bytes(wide)
            })
        };
        prop_assert_eq!(outcome(read_bits), outcome(expected));
    }
}

/// Where a generated piece's bits come from.
#[derive(Debug, Clone)]
enum Source {
    Empty,
    Memory(u64),
    Register(u16),
    Value(Value),
    Bytes(Vec<u8>),
    ImplicitPointer,
}

#[derive(Debug, Clone)]
struct ModelPiece {
    source: Source,
    size: Option<u64>,
    bit_offset: Option<u64>,
}

fn gimli_pieces(pieces: &[ModelPiece]) -> Vec<gimli::Piece<Reader<'_>>> {
    pieces
        .iter()
        .map(|piece| gimli::Piece {
            size_in_bits: piece.size,
            bit_offset: piece.bit_offset,
            location: match &piece.source {
                Source::Empty => Location::Empty,
                Source::Memory(byte) => Location::Address {
                    address: BASE + byte,
                },
                Source::Register(number) => Location::Register {
                    register: gimli::Register(*number),
                },
                Source::Value(value) => Location::Value { value: *value },
                Source::Bytes(bytes) => Location::Bytes {
                    value: gimli::EndianSlice::new(bytes, RunTimeEndian::Little),
                },
                Source::ImplicitPointer => Location::ImplicitPointer {
                    value: gimli::DebugInfoOffset(0x40),
                    byte_offset: 0,
                },
            },
        })
        .collect()
}

/// A computed value's bits, least significant first, and how it extends
/// past them: by its sign, by zeros, or not at all.
fn value_bits(value: Value) -> (u64, u64, Option<bool>) {
    let signed = |value: i64, width| {
        let negative = value < 0;
        (value.cast_unsigned(), width, Some(negative))
    };

    match value {
        Value::Generic(value) => (value, 64, (value >> 63 == 0).then_some(false)),
        Value::U8(value) => (u64::from(value), 8, Some(false)),
        Value::U16(value) => (u64::from(value), 16, Some(false)),
        Value::U32(value) => (u64::from(value), 32, Some(false)),
        Value::U64(value) => (value, 64, Some(false)),
        Value::I8(value) => signed(i64::from(value), 8),
        Value::I16(value) => signed(i64::from(value), 16),
        Value::I32(value) => signed(i64::from(value), 32),
        Value::I64(value) => signed(value, 64),
        Value::F32(value) => (u64::from(value.to_bits()), 32, None),
        Value::F64(value) => (value.to_bits(), 64, None),
    }
}

/// The bits `size` bits from `bit_offset` within a piece's source hold, or
/// `None` if the piece reaches past its source or is otherwise malformed.
fn source_bits(source: &Source, bit_offset: u64, size: u64, machine: &Machine) -> Option<Vec<Bit>> {
    let positions = bit_offset..bit_offset + size;
    match source {
        Source::Empty => Some(vec![Bit::Undefined; index(size)]),
        Source::Memory(byte) => Some(
            positions
                .map(|bit| machine.memory_bit(byte * 8 + bit))
                .collect(),
        ),
        Source::Register(number) => match machine.registers.get(usize::from(*number)) {
            Some(Register::Value(value)) => (bit_offset + size <= 64).then(|| {
                positions
                    .map(|bit| Bit::Known(value >> bit & 1 == 1))
                    .collect()
            }),
            _ => Some(vec![
                Bit::Unavailable(
                    machine.register_reason(*number).expect("no value")
                );
                index(size)
            ]),
        },
        Source::Value(value) => {
            let (bits, width, extension) = value_bits(*value);
            if bit_offset + size > width && extension.is_none() {
                return None;
            }

            Some(
                positions
                    .map(|bit| {
                        Bit::Known(if bit < width {
                            bits >> bit & 1 == 1
                        } else {
                            extension == Some(true)
                        })
                    })
                    .collect(),
            )
        }
        Source::Bytes(bytes) => (bit_offset + size <= bytes.len() as u64 * 8).then(|| {
            positions
                .map(|bit| Bit::Known(bytes[index_of(bit)] >> (bit % 8) & 1 == 1))
                .collect()
        }),
        // A pointer has no bits to read, only a referent.
        Source::ImplicitPointer => (bit_offset == 0).then(|| vec![Bit::Undefined; index(size)]),
    }
}

/// What the object of `byte_size` bytes that `pieces` describe holds, bit
/// by bit, under DWARF 5 section 2.6.1.2: each piece in turn, register and
/// value pieces taking their low bits, and bits no piece covers undefined.
/// `None` if the pieces are malformed.
fn model_bits(pieces: &[ModelPiece], byte_size: u64, machine: &Machine) -> Option<Vec<Bit>> {
    let total = byte_size * 8;
    let mut bits = Vec::new();

    for piece in pieces {
        let size = match piece.size {
            Some(size) => size,
            None if pieces.len() == 1 => total,
            None => return None,
        };

        if bits.len() as u64 + size > total {
            return None;
        }

        bits.extend(source_bits(
            &piece.source,
            piece.bit_offset.unwrap_or(0),
            size,
            machine,
        )?);
    }

    bits.resize(index(total), Bit::Undefined);
    Some(bits)
}

/// Whether one piece describes all of the object, which then is that
/// piece's storage.
fn describes_whole(pieces: &[ModelPiece], byte_size: u64) -> bool {
    matches!(pieces, [piece] if piece.size.is_none_or(|size| size == byte_size * 8)
        && piece.bit_offset.is_none_or(|offset| offset == 0))
}

/// Builds the storage `pieces` describe and reads all of it.
fn read_pieces(
    pieces: &[ModelPiece],
    byte_size: u64,
    endian: RunTimeEndian,
    machine: &mut Machine,
) -> (
    std::result::Result<ValueStorage, EvaluateError>,
    std::result::Result<Vec<u8>, EvaluateError>,
) {
    let byte_order = match endian {
        RunTimeEndian::Little => ByteOrder::Little,
        RunTimeEndian::Big => ByteOrder::Big,
    };

    let storage = storage_from_pieces(
        &gimli_pieces(pieces),
        byte_size,
        None,
        endian,
        target(byte_order),
        machine,
    );
    let read = storage.clone().and_then(|storage| {
        storage::read(
            &storage,
            index(byte_size),
            machine,
            &mut InspectionBudget::default(),
        )
        .map(|(_, raw)| raw.to_vec())
    });

    (storage, outcome(read))
}

fn computed_value() -> impl Strategy<Value = Value> {
    prop_oneof![
        3 => u64_edges().prop_map(Value::Generic),
        1 => any::<u8>().prop_map(Value::U8),
        1 => any::<u16>().prop_map(Value::U16),
        1 => any::<u32>().prop_map(Value::U32),
        1 => any::<u64>().prop_map(Value::U64),
        1 => any::<i8>().prop_map(Value::I8),
        1 => any::<i16>().prop_map(Value::I16),
        1 => any::<i32>().prop_map(Value::I32),
        1 => any::<i64>().prop_map(Value::I64),
        1 => any::<u32>().prop_map(|bits| Value::F32(f32::from_bits(bits))),
        1 => any::<u64>().prop_map(|bits| Value::F64(f64::from_bits(bits))),
    ]
}

/// A source that holds bits: memory, a register, a value, or bytes.
fn holding_source() -> impl Strategy<Value = Source> {
    prop_oneof![
        3 => (0_u64..64).prop_map(Source::Memory),
        3 => (0_u16..4).prop_map(Source::Register),
        2 => computed_value().prop_map(Source::Value),
        2 => proptest::collection::vec(any::<u8>(), 0..=16).prop_map(Source::Bytes),
    ]
}

fn source() -> impl Strategy<Value = Source> {
    prop_oneof![
        10 => holding_source(),
        1 => Just(Source::Empty),
        1 => Just(Source::ImplicitPointer),
    ]
}

/// Piece sizes that split `bits` bits, most often all of them, sometimes
/// leaving a tail or describing too many.
fn sizes(bits: u64) -> impl Strategy<Value = Vec<u64>> {
    (
        proptest::collection::vec(0..=bits, 0..4),
        prop_oneof![4 => Just(bits), 1 => 1..=bits, 1 => bits + 1..bits + 9],
    )
        .prop_map(|(mut cuts, end)| {
            cuts.retain(|cut| *cut < end);
            cuts.push(0);
            cuts.push(end);
            cuts.sort_unstable();
            cuts.dedup();
            cuts.windows(2).map(|pair| pair[1] - pair[0]).collect()
        })
}

fn bit_offset() -> impl Strategy<Value = Option<u64>> {
    prop_oneof![3 => Just(None), 2 => Just(Some(0)), 2 => (1_u64..16).prop_map(Some), 1 => (16_u64..80).prop_map(Some)]
}

fn model_pieces() -> impl Strategy<Value = (u64, Vec<ModelPiece>)> {
    (1_u64..=12).prop_flat_map(|byte_size| {
        let pieces = sizes(byte_size * 8).prop_flat_map(|sizes| {
            sizes
                .into_iter()
                .map(|size| {
                    (source(), bit_offset(), prop::bool::weighted(0.1)).prop_map(
                        move |(source, bit_offset, unsized_)| ModelPiece {
                            source,
                            size: (!unsized_).then_some(size),
                            bit_offset,
                        },
                    )
                })
                .collect::<Vec<_>>()
        });
        (Just(byte_size), pieces)
    })
}

#[test]
fn a_float_value_describes_a_whole_object_as_its_parts_do() {
    let mut machine = readable_machine();
    let piece = |size, bit_offset| ModelPiece {
        source: Source::Value(Value::F32(1.5)),
        size,
        bit_offset,
    };

    let low = 1.5_f32.to_bits().to_le_bytes()[..3].to_vec();
    for pieces in [
        vec![piece(None, None)],
        vec![piece(Some(24), None)],
        vec![piece(Some(8), Some(0)), piece(Some(16), Some(8))],
    ] {
        let (_, read) = read_pieces(&pieces, 3, RunTimeEndian::Little, &mut machine);
        assert_eq!(read, Ok(low.clone()), "{pieces:?}");
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(1024))]

    /// The storage location pieces describe reads exactly the bits the
    /// model gives them, and malformed pieces are refused. A big-endian
    /// target's composites are unsupported, never misread.
    #[test]
    fn pieces_read_as_the_bits_they_describe(
        (byte_size, pieces) in model_pieces(),
        mut machine in machine(),
    ) {
        let (storage, read) = read_pieces(&pieces, byte_size, RunTimeEndian::Little, &mut machine);
        let whole = describes_whole(&pieces, byte_size);

        if whole && matches!(pieces[0].source, Source::ImplicitPointer) {
            let is_pointer = matches!(storage, Ok(ValueStorage::ImplicitPointer { .. }));
            prop_assert!(is_pointer, "{:?}", storage);
        } else {
            let expected = model_bits(&pieces, byte_size, &machine).map_or_else(
                || Err(EvaluateError::Malformed("".into())),
                |bits| outcome(expected_read(&bits)),
            );
            prop_assert_eq!(read, expected);
        }

        if !whole {
            let (storage, _) = read_pieces(&pieces, byte_size, RunTimeEndian::Big, &mut machine);
            prop_assert_eq!(
                storage,
                Err(crate::UnsupportedVariableFeature::CompositeLocation.into())
            );
        }
    }

    /// One piece that describes all of an object reads like the same
    /// source split into adjacent pieces, whether a memory piece moves on
    /// by its address or by its bit offset.
    #[test]
    fn a_whole_piece_reads_like_its_parts(
        (byte_size, cuts) in (1_u64..=10).prop_flat_map(|byte_size| {
            (Just(byte_size), proptest::collection::vec(1..byte_size * 8, 1..4))
        }),
        source in holding_source(),
        by_address in any::<[bool; 4]>(),
        sized in any::<bool>(),
        mut machine in machine(),
    ) {
        let bits = byte_size * 8;
        let mut cuts = cuts;
        cuts.extend([0, bits]);
        cuts.sort_unstable();
        cuts.dedup();

        let whole = [ModelPiece {
            source: source.clone(),
            size: sized.then_some(bits),
            bit_offset: None,
        }];
        let parts = cuts
            .windows(2)
            .zip(by_address.iter().cycle())
            .map(|(pair, by_address)| {
                let (from, size) = (pair[0], pair[1] - pair[0]);
                match &source {
                    Source::Memory(byte) if *by_address => ModelPiece {
                        source: Source::Memory(byte + from / 8),
                        size: Some(size),
                        bit_offset: Some(from % 8),
                    },
                    source => ModelPiece {
                        source: source.clone(),
                        size: Some(size),
                        bit_offset: Some(from),
                    },
                }
            })
            .collect::<Vec<_>>();

        let (_, whole) = read_pieces(&whole, byte_size, RunTimeEndian::Little, &mut machine);
        let (_, split) = read_pieces(&parts, byte_size, RunTimeEndian::Little, &mut machine);
        prop_assert_eq!(whole, split, "parts {:?}", parts);
    }
}
