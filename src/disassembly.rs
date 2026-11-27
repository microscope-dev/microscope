#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
mod x86_64;

use std::collections::BTreeMap;
use std::fmt;
use std::sync::Arc;

use crate::unwind::RegisterFile;
use crate::{
    AddressDescription, AddressRange, Architecture, ByteOrder, CodeInstanceId, Error,
    MemoryReadUnavailableReason, ModuleId, Result, SourceLocation, StopId, SymbolExtentProvenance,
    SymbolId, TargetDescription, VirtualAddress,
};

pub const MAX_WINDOW_BEFORE: u32 = 1024;

pub const MAX_WINDOW_AFTER: u32 = 4096;

pub const MAX_FUNCTION_INSTRUCTIONS: usize = 32768;

pub const MAX_BACKWARD_DISTANCE: u64 = 64 * 1024;

const READ_CHUNK: u64 = 4096;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub enum AssemblySyntax {
    #[default]
    Intel,

    Att,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum InstructionTokenKind {
    Mnemonic,

    Prefix,

    Keyword,

    Register,

    Number,

    Address,

    Punctuation,

    Text,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstructionToken {
    pub kind: InstructionTokenKind,

    pub text: Arc<str>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ControlFlow {
    Sequential,

    Jump,

    ConditionalJump,

    IndirectJump,

    Call,

    IndirectCall,

    Return,

    Interrupt,

    Transaction,

    Exception,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum InstructionReferenceKind {
    BranchTarget,

    MemoryOperand,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstructionReference {
    pub kind: InstructionReferenceKind,

    pub address: VirtualAddress,

    pub description: AddressDescription,
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum IndirectTarget {
    Register {
        target: AddressDescription,
    },

    Memory {
        slot: AddressDescription,

        target: AddressDescription,
    },

    Unreadable {
        slot: AddressDescription,

        address: VirtualAddress,

        reason: MemoryReadUnavailableReason,
    },

    NeedsRegisters,

    Unsupported,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecodedInstruction {
    pub tokens: Arc<[InstructionToken]>,

    pub flow: ControlFlow,

    pub references: Arc<[InstructionReference]>,

    pub indirect_target: Option<Arc<IndirectTarget>>,
}

impl DecodedInstruction {
    #[must_use]
    pub fn text(&self) -> String {
        self.tokens
            .iter()
            .map(|token| token.text.as_ref())
            .collect()
    }

    #[must_use]
    pub fn mnemonic(&self) -> Option<&str> {
        self.tokens
            .iter()
            .find(|token| token.kind == InstructionTokenKind::Mnemonic)
            .map(|token| token.text.as_ref())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InstructionContent {
    Decoded(DecodedInstruction),

    Invalid,

    Truncated,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DisassembledInstruction {
    pub address: VirtualAddress,

    pub bytes: Arc<[u8]>,

    pub content: InstructionContent,

    pub location: AddressDescription,

    pub source: Option<SourceLocation>,
}

impl DisassembledInstruction {
    #[must_use]
    pub fn end(&self) -> VirtualAddress {
        VirtualAddress::new(self.address.get() + self.bytes.len() as u64)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum BoundaryEvidence {
    ProgramCounter,

    FunctionRange,

    CodeSymbol,

    SectionStart,

    RangeEnd,
}

impl fmt::Display for BoundaryEvidence {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::ProgramCounter => "the program counter",
            Self::FunctionRange => "a function's debug-information range",
            Self::CodeSymbol => "a code symbol",
            Self::SectionStart => "the start of an executable section",
            Self::RangeEnd => "the end of the range",
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BoundaryConflict {
    pub boundary: VirtualAddress,

    pub evidence: BoundaryEvidence,

    pub instruction: VirtualAddress,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockCompletion {
    Complete,

    Unreadable {
        address: VirtualAddress,

        reason: MemoryReadUnavailableReason,
    },

    Limited {
        next: VirtualAddress,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DisassemblyBlock {
    pub range: AddressRange<VirtualAddress>,

    pub instructions: Arc<[DisassembledInstruction]>,

    pub conflicts: Arc<[BoundaryConflict]>,

    pub completion: BlockCompletion,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContextShortfall {
    NoKnownBoundary,

    Desynchronized {
        /// The boundary that decoding did not land on.
        boundary: VirtualAddress,
    },

    Unreadable {
        address: VirtualAddress,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TargetBoundary {
    Known(BoundaryEvidence),

    Reached {
        from: VirtualAddress,

        evidence: BoundaryEvidence,
    },

    Crossed {
        from: VirtualAddress,

        instruction: VirtualAddress,
    },

    Unverified(ContextShortfall),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FunctionOrigin {
    DebugInfo {
        instance: CodeInstanceId,
    },

    Symbol {
        symbol: SymbolId,

        provenance: SymbolExtentProvenance,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DisassembledFunction {
    pub module: ModuleId,

    pub name: Arc<str>,

    pub origin: FunctionOrigin,
}

impl DisassembledFunction {
    #[must_use]
    pub fn demangled_name(&self) -> Option<String> {
        crate::demangle::demangle(&self.name)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DisassemblyView {
    Function {
        function: DisassembledFunction,

        blocks: Arc<[DisassemblyBlock]>,
    },

    Window {
        address: VirtualAddress,

        boundary: TargetBoundary,

        leading: Option<ContextShortfall>,

        block: DisassemblyBlock,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Disassembly {
    pub revision: u64,

    pub stop_id: StopId,

    pub target: TargetDescription,

    pub syntax: AssemblySyntax,

    pub view: DisassemblyView,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DisassemblyRange {
    Function(VirtualAddress),

    Window {
        address: VirtualAddress,

        before: u32,

        after: u32,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DisassemblyQuery {
    pub range: DisassemblyRange,

    pub syntax: AssemblySyntax,
}

pub struct CodeRead {
    pub bytes: Vec<u8>,
    pub unreadable: Option<MemoryReadUnavailableReason>,
}

pub trait CodeSource {
    fn read(&mut self, address: VirtualAddress, size: usize) -> Result<CodeRead>;

    fn instruction_starts(
        &self,
        range: AddressRange<VirtualAddress>,
    ) -> BTreeMap<VirtualAddress, BoundaryEvidence>;

    fn describe(&self, address: VirtualAddress) -> AddressDescription;

    fn source_location(&self, address: VirtualAddress) -> Option<SourceLocation>;

    fn registers(&self, address: VirtualAddress) -> Option<&RegisterFile>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RawIndirect {
    Load { address: u64, size: usize },

    Value(u64),

    NeedsRegisters,

    Unsupported,
}

pub enum RawDecode {
    Instruction {
        length: usize,
        tokens: Vec<InstructionToken>,
        flow: ControlFlow,
        references: Vec<(InstructionReferenceKind, u64)>,

        indirect: Option<RawIndirect>,
    },

    Invalid,

    Incomplete,
}

pub trait InstructionDecoder {
    fn max_instruction_length(&self) -> usize;

    fn byte_order(&self) -> ByteOrder;

    fn decode(&mut self, address: u64, bytes: &[u8], registers: Option<&RegisterFile>)
    -> RawDecode;
}

pub fn decoder_for(
    target: TargetDescription,
    syntax: AssemblySyntax,
) -> Result<Box<dyn InstructionDecoder>> {
    match target.architecture {
        #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
        Architecture::X86_64 => Ok(Box::new(x86_64::Decoder::new(syntax))),
        architecture => Err(Error::DisassemblyUnsupported(architecture)),
    }
}

#[cfg(feature = "fuzzing")]
pub fn fuzz(data: &[u8]) {
    fuzz::run(data);
}

pub struct Engine<'a> {
    source: &'a mut dyn CodeSource,
    decoder: &'a mut dyn InstructionDecoder,
    chunks: BTreeMap<u64, Chunk>,
}

struct Chunk {
    bytes: Vec<u8>,
    unreadable: Option<MemoryReadUnavailableReason>,
}

#[derive(Clone, Copy)]
enum Until {
    Boundary { end: u64, limit: usize },

    Count(usize),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SweepEnd {
    /// The last instruction ended exactly at the boundary.
    Reached,

    /// The last instruction, at this address, crossed the boundary.
    Crossed { instruction: u64 },

    /// The requested number of instructions was decoded.
    Counted,

    /// The limit was reached before the boundary.
    Limited { next: u64 },

    Unreadable {
        address: u64,
        reason: MemoryReadUnavailableReason,
    },
}

pub struct Window {
    pub boundary: TargetBoundary,
    pub shortfall: Option<ContextShortfall>,
    pub block: DisassemblyBlock,
}

struct Leading {
    boundary: TargetBoundary,
    shortfall: Option<ContextShortfall>,
    instructions: Vec<DisassembledInstruction>,
}

struct Sweep {
    instructions: Vec<DisassembledInstruction>,
    conflicts: Vec<BoundaryConflict>,
    end: SweepEnd,
}

struct View {
    bytes: Vec<u8>,
    unreadable: Option<MemoryReadUnavailableReason>,
}

impl<'a> Engine<'a> {
    pub fn new(source: &'a mut dyn CodeSource, decoder: &'a mut dyn InstructionDecoder) -> Self {
        Self {
            source,
            decoder,
            chunks: BTreeMap::new(),
        }
    }

    pub fn function(
        &mut self,
        ranges: &[AddressRange<VirtualAddress>],
    ) -> Result<Vec<DisassemblyBlock>> {
        let mut remaining = MAX_FUNCTION_INSTRUCTIONS;
        let mut blocks = Vec::with_capacity(ranges.len());

        for range in ranges {
            let (start, end) = (range.start.get(), range.end.get());
            let starts = self.source.instruction_starts(*range);
            let sweep = self.sweep(
                start,
                Until::Boundary {
                    end,
                    limit: remaining,
                },
                &starts,
            )?;
            remaining -= sweep.instructions.len();
            let mut conflicts = sweep.conflicts;

            let completion = match sweep.end {
                SweepEnd::Reached => BlockCompletion::Complete,
                SweepEnd::Crossed { instruction } => {
                    conflicts.push(BoundaryConflict {
                        boundary: range.end,
                        evidence: BoundaryEvidence::RangeEnd,
                        instruction: VirtualAddress::new(instruction),
                    });
                    BlockCompletion::Complete
                }
                SweepEnd::Limited { next } => BlockCompletion::Limited {
                    next: VirtualAddress::new(next),
                },
                SweepEnd::Unreadable { address, reason } => BlockCompletion::Unreadable {
                    address: VirtualAddress::new(address),
                    reason,
                },
                SweepEnd::Counted => {
                    return Err(Error::internal(
                        "a bounded sweep ended without its boundary",
                    ));
                }
            };

            blocks.push(DisassemblyBlock {
                range: *range,
                instructions: sweep.instructions.into(),
                conflicts: conflicts.into(),
                completion,
            });
        }

        Ok(blocks)
    }

    pub fn window(&mut self, address: VirtualAddress, before: u32, after: u32) -> Result<Window> {
        let leading = self.leading(address, before as usize)?;
        let mut instructions = leading.instructions;
        let mut conflicts = Vec::new();
        let mut completion = BlockCompletion::Complete;

        if after != 0 {
            let count = after as usize;
            let span = u64::try_from(count.saturating_mul(self.decoder.max_instruction_length()))
                .unwrap_or(u64::MAX);
            let starts = self.source.instruction_starts(AddressRange {
                start: address,
                end: VirtualAddress::new(address.get().saturating_add(span)),
            });
            let sweep = self.sweep(address.get(), Until::Count(count), &starts)?;
            instructions.extend(sweep.instructions);
            conflicts = sweep.conflicts;

            if let SweepEnd::Unreadable { address, reason } = sweep.end {
                completion = BlockCompletion::Unreadable {
                    address: VirtualAddress::new(address),
                    reason,
                };
            }
        }

        let range = AddressRange {
            start: instructions.first().map_or(address, |first| first.address),
            end: instructions
                .last()
                .map_or(address, DisassembledInstruction::end),
        };

        Ok(Window {
            boundary: leading.boundary,
            shortfall: leading.shortfall,
            block: DisassemblyBlock {
                range,
                instructions: instructions.into(),
                conflicts: conflicts.into(),
                completion,
            },
        })
    }

    /// Collects up to `before` instructions preceding `address` without
    /// guessing instruction boundaries in variable-length ISAs.
    ///
    /// Sweeps forward from progressively earlier known boundaries (function
    /// starts, symbols, section starts) and stops if a sweep crosses `end`
    /// instead of landing on it.
    fn leading(&mut self, address: VirtualAddress, before: usize) -> Result<Leading> {
        let target = address.get();
        let known = self.source.instruction_starts(AddressRange {
            start: VirtualAddress::new(target.saturating_sub(MAX_BACKWARD_DISTANCE)),
            end: VirtualAddress::new(target.saturating_add(1)),
        });

        let mut segments = Vec::new();
        let mut collected = 0;
        let mut end = target;
        let mut boundary = None;
        let mut shortfall = Some(ContextShortfall::NoKnownBoundary);

        for (&start, &evidence) in known.range(..address).rev() {
            let sweep = self.sweep(
                start.get(),
                Until::Boundary {
                    end,
                    limit: usize::MAX,
                },
                &BTreeMap::new(),
            )?;

            let at_target = end == target;
            match sweep.end {
                SweepEnd::Reached => {
                    if at_target {
                        boundary = Some(TargetBoundary::Reached {
                            from: start,
                            evidence,
                        });
                    }

                    collected += sweep.instructions.len();
                    segments.push(sweep.instructions);
                    end = start.get();

                    if collected >= before {
                        shortfall = None;
                        break;
                    }
                }
                SweepEnd::Crossed { instruction } => {
                    if at_target {
                        boundary = Some(TargetBoundary::Crossed {
                            from: start,
                            instruction: VirtualAddress::new(instruction),
                        });
                    }

                    shortfall = Some(ContextShortfall::Desynchronized {
                        boundary: VirtualAddress::new(end),
                    });
                    break;
                }
                SweepEnd::Unreadable { address, .. } => {
                    shortfall = Some(ContextShortfall::Unreadable {
                        address: VirtualAddress::new(address),
                    });
                    break;
                }
                SweepEnd::Counted | SweepEnd::Limited { .. } => {
                    return Err(Error::internal("an unlimited bounded sweep ended early"));
                }
            }
        }

        let boundary = match known.get(&address) {
            Some(&evidence) => TargetBoundary::Known(evidence),
            None => boundary.unwrap_or_else(|| {
                TargetBoundary::Unverified(shortfall.unwrap_or(ContextShortfall::NoKnownBoundary))
            }),
        };

        let mut instructions = segments.into_iter().rev().flatten().collect::<Vec<_>>();
        instructions.drain(..instructions.len().saturating_sub(before));

        Ok(Leading {
            boundary,
            shortfall: if instructions.len() >= before {
                None
            } else {
                shortfall
            },
            instructions,
        })
    }

    fn sweep(
        &mut self,
        start: u64,
        until: Until,
        starts: &BTreeMap<VirtualAddress, BoundaryEvidence>,
    ) -> Result<Sweep> {
        let mut instructions = Vec::new();
        let mut conflicts = Vec::new();
        let mut position = start;
        let end = loop {
            match until {
                Until::Boundary { end, .. } if position == end => break SweepEnd::Reached,
                Until::Boundary { limit, .. } if instructions.len() == limit => {
                    break SweepEnd::Limited { next: position };
                }
                Until::Count(count) if instructions.len() == count => break SweepEnd::Counted,
                _ => {}
            }

            let length = self.decoder.max_instruction_length();
            let view = self.view(position, length)?;
            let mut truncated = None;
            let address = VirtualAddress::new(position);
            let registers = self.source.registers(address);

            let (content, length) = match self.decoder.decode(position, &view.bytes, registers) {
                RawDecode::Instruction {
                    length,
                    tokens,
                    flow,
                    references,
                    indirect,
                } => (
                    InstructionContent::Decoded(DecodedInstruction {
                        tokens: tokens.into(),
                        flow,
                        references: references
                            .into_iter()
                            .map(|(kind, target)| {
                                let target = VirtualAddress::new(target);
                                InstructionReference {
                                    kind,
                                    address: target,
                                    description: self.source.describe(target),
                                }
                            })
                            .collect(),
                        indirect_target: indirect
                            .map(|indirect| self.indirect_target(indirect).map(Arc::new))
                            .transpose()?,
                    }),
                    length,
                ),
                RawDecode::Invalid => (InstructionContent::Invalid, 1),
                RawDecode::Incomplete => {
                    let Some(reason) = view.unreadable else {
                        return Err(Error::internal(
                            "only unreadable memory leaves an instruction incomplete",
                        ));
                    };

                    if view.bytes.is_empty() {
                        break SweepEnd::Unreadable {
                            address: position,
                            reason,
                        };
                    }

                    truncated = Some(reason);
                    (InstructionContent::Truncated, view.bytes.len())
                }
            };

            let next = position + length as u64;
            instructions.push(self.instruction(address, &view.bytes, length, content)?);

            let limit = match until {
                Until::Boundary { end, .. } => next.min(end),
                Until::Count(_) => next,
            };

            let overlapped = starts
                .range(VirtualAddress::new(position + 1)..VirtualAddress::new(limit))
                .next();

            match (overlapped, until) {
                (Some((&boundary, &evidence)), _) => {
                    conflicts.push(BoundaryConflict {
                        boundary,
                        evidence,
                        instruction: address,
                    });
                    position = boundary.get();
                }
                (None, _) if let Some(reason) = truncated => {
                    break SweepEnd::Unreadable {
                        address: next,
                        reason,
                    };
                }
                (None, Until::Boundary { end, .. }) if next > end => {
                    break SweepEnd::Crossed {
                        instruction: position,
                    };
                }
                (None, _) => position = next,
            }
        };

        Ok(Sweep {
            instructions,
            conflicts,
            end,
        })
    }

    fn indirect_target(&mut self, indirect: RawIndirect) -> Result<IndirectTarget> {
        Ok(match indirect {
            RawIndirect::Load { address, size } => {
                let slot = self.source.describe(VirtualAddress::new(address));
                let view = self.view(address, size)?;

                if view.bytes.len() < size {
                    IndirectTarget::Unreadable {
                        slot,
                        address: VirtualAddress::new(address + view.bytes.len() as u64),
                        reason: view.unreadable.ok_or_else(|| {
                            Error::internal("only unreadable memory shortens a view")
                        })?,
                    }
                } else {
                    let target =
                        target_value(&view.bytes, self.decoder.byte_order()).ok_or_else(|| {
                            Error::internal("an indirect branch loads more than eight bytes")
                        })?;
                    IndirectTarget::Memory {
                        slot,
                        target: self.source.describe(VirtualAddress::new(target)),
                    }
                }
            }
            RawIndirect::Value(target) => IndirectTarget::Register {
                target: self.source.describe(VirtualAddress::new(target)),
            },
            RawIndirect::NeedsRegisters => IndirectTarget::NeedsRegisters,
            RawIndirect::Unsupported => IndirectTarget::Unsupported,
        })
    }

    fn instruction(
        &self,
        address: VirtualAddress,
        bytes: &[u8],
        length: usize,
        content: InstructionContent,
    ) -> Result<DisassembledInstruction> {
        let bytes = bytes
            .get(..length)
            .ok_or_else(|| Error::internal("a decoded instruction is longer than its bytes"))?;

        Ok(DisassembledInstruction {
            address,
            bytes: bytes.into(),
            content,
            location: self.source.describe(address),
            source: self.source.source_location(address),
        })
    }

    fn view(&mut self, address: u64, length: usize) -> Result<View> {
        let mut bytes = Vec::with_capacity(length);
        let mut current = address;

        while bytes.len() < length {
            let chunk_start = current - current % READ_CHUNK;
            if !self.chunks.contains_key(&chunk_start) {
                let chunk = self.read_chunk(chunk_start)?;
                self.chunks.insert(chunk_start, chunk);
            }

            let chunk = self
                .chunks
                .get(&chunk_start)
                .ok_or_else(|| Error::internal("a chunk read just now is missing"))?;
            let offset = usize::try_from(current - chunk_start)
                .map_err(|_| Error::internal("a chunk offset is larger than READ_CHUNK"))?;

            if offset >= chunk.bytes.len() {
                let Some(reason) = chunk.unreadable else {
                    return Err(Error::internal(
                        "only an unreadable chunk is shorter than a chunk",
                    ));
                };

                if offset == chunk.bytes.len() {
                    return Ok(View {
                        bytes,
                        unreadable: Some(reason),
                    });
                }

                let wanted = length - bytes.len();
                let read = self.source.read(VirtualAddress::new(current), wanted)?;
                let unreadable = (read.bytes.len() < wanted).then(|| {
                    read.unreadable
                        .unwrap_or(MemoryReadUnavailableReason::Inaccessible)
                });
                bytes.extend_from_slice(&read.bytes);
                return Ok(View { bytes, unreadable });
            }

            let take = (chunk.bytes.len() - offset).min(length - bytes.len());
            let taken = chunk
                .bytes
                .get(offset..offset + take)
                .ok_or_else(|| Error::internal("a chunk is shorter than its own length"))?;
            bytes.extend_from_slice(taken);
            current += take as u64;
        }

        Ok(View {
            bytes,
            unreadable: None,
        })
    }

    fn read_chunk(&mut self, start: u64) -> Result<Chunk> {
        let size = usize::try_from(READ_CHUNK.min(u64::MAX - start))
            .map_err(|_| Error::internal("a read chunk is larger than READ_CHUNK"))?;
        let read = self.source.read(VirtualAddress::new(start), size)?;
        let unreadable = if (read.bytes.len() as u64) < READ_CHUNK {
            Some(
                read.unreadable
                    .unwrap_or(MemoryReadUnavailableReason::Inaccessible),
            )
        } else {
            None
        };

        Ok(Chunk {
            bytes: read.bytes,
            unreadable,
        })
    }
}

fn target_value(bytes: &[u8], byte_order: ByteOrder) -> Option<u64> {
    let mut word = [0; 8];
    match byte_order {
        ByteOrder::Little => {
            word.get_mut(..bytes.len())?.copy_from_slice(bytes);
            Some(u64::from_le_bytes(word))
        }
        ByteOrder::Big => {
            word.get_mut(8_usize.checked_sub(bytes.len())?..)?
                .copy_from_slice(bytes);
            Some(u64::from_be_bytes(word))
        }
    }
}

#[cfg(any(test, feature = "fuzzing"))]
mod fake;
#[cfg(any(test, feature = "fuzzing"))]
mod fuzz;
#[cfg(test)]
mod tests;
