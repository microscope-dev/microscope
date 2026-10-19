//! Platform-neutral disassembly.
//!
//! Variable-length machine code has no self-describing instruction
//! boundaries, so decoding from an arbitrary address can produce a
//! convincing but wrong instruction stream. The engine decodes only forward
//! from proven instruction starts: a stopped thread's program counter, the
//! start of a function's debug-information range, a code symbol, or an
//! executable section. Every known start it passes must coincide with a
//! decoded boundary; when one does not, the conflict is reported and
//! decoding resumes at the known start. Instructions before an address are
//! shown only when decoding forward from a known start lands exactly on it.
//!
//! An indirect jump, call, or return is resolved against the stopped state:
//! the memory it loads its target from is read at the stop, and registers
//! are used only for the instruction the selected thread is about to
//! execute, the one place the stop determines them.

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

/// The most instructions a window may request before its address.
pub const MAX_WINDOW_BEFORE: u32 = 1024;

/// The most instructions a window may request from its address onward.
pub const MAX_WINDOW_AFTER: u32 = 4096;

/// The most instructions one function disassembly decodes across its ranges.
pub const MAX_FUNCTION_INSTRUCTIONS: usize = 32768;

/// How far before an address the engine looks for a known instruction start.
pub const MAX_BACKWARD_DISTANCE: u64 = 64 * 1024;

const READ_CHUNK: u64 = 4096;

/// The assembly language used to render instructions.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub enum AssemblySyntax {
    /// Intel syntax: destination first, `qword ptr [rbp-0x8]`.
    #[default]
    Intel,

    /// AT&T syntax as GNU tools print it: source first, `-0x8(%rbp)`.
    Att,
}

/// What one piece of rendered instruction text is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum InstructionTokenKind {
    /// The operation name.
    Mnemonic,

    /// An instruction prefix such as `lock` or `rep`.
    Prefix,

    /// A size or other keyword such as `qword ptr`.
    Keyword,

    /// A register name.
    Register,

    /// An immediate value or displacement.
    Number,

    /// An absolute code or data address, such as a branch target.
    Address,

    /// Separators, brackets, and operators.
    Punctuation,

    /// Whitespace and other text.
    Text,
}

/// One piece of rendered instruction text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstructionToken {
    /// What the text is.
    pub kind: InstructionTokenKind,

    /// The rendered text.
    pub text: Arc<str>,
}

/// How an instruction transfers control.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ControlFlow {
    /// Execution continues with the next instruction.
    Sequential,

    /// An unconditional jump to a target encoded in the instruction.
    Jump,

    /// A conditional jump to a target encoded in the instruction.
    ConditionalJump,

    /// A jump to a target computed at run time.
    IndirectJump,

    /// A call to a target encoded in the instruction.
    Call,

    /// A call to a target computed at run time.
    IndirectCall,

    /// A return to the caller.
    Return,

    /// A software interrupt or system call.
    Interrupt,

    /// The start, abort, or end of a hardware transaction.
    Transaction,

    /// An instruction that always raises an exception, such as `ud2`.
    Exception,
}

/// Why an instruction refers to an address.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum InstructionReferenceKind {
    /// The target of a direct jump or call.
    BranchTarget,

    /// The address of a memory operand that the instruction alone
    /// determines, such as a program-counter-relative operand.
    MemoryOperand,
}

/// An address an instruction encodes, resolved against the loaded modules.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstructionReference {
    /// Why the instruction refers to the address.
    pub kind: InstructionReferenceKind,

    /// The referenced process address.
    pub address: VirtualAddress,

    /// The module, section, and symbol containing the address.
    pub description: AddressDescription,
}

/// Where an indirect jump, call, or return transfers control if it executes
/// in the stopped state.
///
/// The target is what the stop holds: the register containing it, or the
/// memory the instruction loads it from, such as a global offset table slot
/// or a return address on the stack. Registers are known only for the
/// instruction the selected thread is about to execute; elsewhere a target
/// is known only when the instruction alone determines where it is loaded
/// from. A slot the dynamic loader has not filled is reported as it is.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum IndirectTarget {
    /// A register holds the target.
    Register {
        /// The target and the module, section, and symbol containing it.
        target: AddressDescription,
    },

    /// The target is loaded from memory.
    Memory {
        /// The memory holding the target and what contains it.
        slot: AddressDescription,

        /// The target and the module, section, and symbol containing it.
        target: AddressDescription,
    },

    /// The memory holding the target is unreadable.
    Unreadable {
        /// The memory holding the target and what contains it.
        slot: AddressDescription,

        /// The first unreadable address.
        address: VirtualAddress,

        /// Why the memory is unreadable.
        reason: MemoryReadUnavailableReason,
    },

    /// The target depends on a register whose value the stop does not
    /// determine for this instruction: it is not the instruction the selected
    /// thread is about to execute, or that thread is inside a system call the
    /// kernel will restart.
    NeedsRegisters,

    /// The debugger does not compute targets of this form: a far transfer, a
    /// return from an interrupt or system call, or a branch whose operand
    /// size differs between processor vendors.
    Unsupported,
}

/// A successfully decoded instruction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecodedInstruction {
    /// The rendered instruction, in order.
    pub tokens: Arc<[InstructionToken]>,

    /// How the instruction transfers control.
    pub flow: ControlFlow,

    /// The addresses the instruction encodes.
    pub references: Arc<[InstructionReference]>,

    /// For an indirect jump, call, or return, where it transfers control in
    /// the stopped state.
    pub indirect_target: Option<Arc<IndirectTarget>>,
}

impl DecodedInstruction {
    /// Returns the rendered instruction text.
    #[must_use]
    pub fn text(&self) -> String {
        self.tokens
            .iter()
            .map(|token| token.text.as_ref())
            .collect()
    }

    /// Returns the operation name.
    #[must_use]
    pub fn mnemonic(&self) -> Option<&str> {
        self.tokens
            .iter()
            .find(|token| token.kind == InstructionTokenKind::Mnemonic)
            .map(|token| token.text.as_ref())
    }
}

/// What the bytes at one decoded position are.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InstructionContent {
    /// A valid instruction.
    Decoded(DecodedInstruction),

    /// One byte that begins no valid instruction. Decoding continues at the
    /// next byte.
    Invalid,

    /// The readable prefix of an instruction whose remaining bytes are
    /// unreadable.
    Truncated,
}

/// One position of a disassembly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DisassembledInstruction {
    /// The address of the first byte.
    pub address: VirtualAddress,

    /// The bytes as the program sees them; debugger breakpoint traps are
    /// never shown.
    pub bytes: Arc<[u8]>,

    /// What the bytes are.
    pub content: InstructionContent,

    /// The module, section, and symbol containing the instruction.
    pub location: AddressDescription,

    /// The source line containing the instruction, when known. The source
    /// file belongs to the image of the instruction's module.
    pub source: Option<SourceLocation>,
}

impl DisassembledInstruction {
    /// Returns the address after the last byte.
    #[must_use]
    pub fn end(&self) -> VirtualAddress {
        VirtualAddress::new(self.address.get() + self.bytes.len() as u64)
    }
}

/// Evidence that an address begins an instruction.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum BoundaryEvidence {
    /// A stopped thread is about to execute the instruction.
    ProgramCounter,

    /// Debug information begins a function's address range there.
    FunctionRange,

    /// A code symbol begins there.
    CodeSymbol,

    /// An executable section begins there.
    SectionStart,

    /// The disassembled range ends there.
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

/// A known instruction start that a decoded instruction overlapped.
///
/// The decoded instruction is reported in full and decoding resumes at the
/// known start, so the two overlap. Either the bytes before the boundary are
/// not the code decoding assumed, such as data embedded in code, or the
/// evidence is wrong; the disassembly cannot tell which.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BoundaryConflict {
    /// The known instruction start.
    pub boundary: VirtualAddress,

    /// Why the boundary is known.
    pub evidence: BoundaryEvidence,

    /// The decoded instruction that overlaps the boundary.
    pub instruction: VirtualAddress,
}

/// Whether a block holds every instruction of its range.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockCompletion {
    /// Decoding reached the end of the range.
    Complete,

    /// Memory became unreadable before the range ended.
    Unreadable {
        /// The first unreadable address.
        address: VirtualAddress,

        /// Why the memory is unreadable.
        reason: MemoryReadUnavailableReason,
    },

    /// The instruction limit was reached; decoding would continue here.
    Limited {
        /// The first instruction start not decoded.
        next: VirtualAddress,
    },
}

/// Contiguously decoded instructions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DisassemblyBlock {
    /// The decoded range: for a function, the debug-information range or
    /// symbol extent; for a window, the decoded instructions.
    pub range: AddressRange<VirtualAddress>,

    /// The decoded instructions in address order. An instruction overlaps
    /// the next only where a conflict reports it.
    pub instructions: Arc<[DisassembledInstruction]>,

    /// Known instruction starts that decoded instructions overlapped.
    pub conflicts: Arc<[BoundaryConflict]>,

    /// Whether decoding covered the whole range.
    pub completion: BlockCompletion,
}

/// Why fewer instructions than requested precede a window's address.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContextShortfall {
    /// No known instruction start precedes the earliest decoded instruction
    /// within the search distance.
    NoKnownBoundary,

    /// Decoding forward from the preceding known start overlapped this known
    /// boundary, so the instructions before it are unproven.
    Desynchronized {
        /// The boundary that decoding did not land on.
        boundary: VirtualAddress,
    },

    /// The memory between the preceding known start and the earliest
    /// decoded instruction is unreadable.
    Unreadable {
        /// The first unreadable address.
        address: VirtualAddress,
    },
}

/// What proves that a window's address begins an instruction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TargetBoundary {
    /// The address is itself a known instruction start.
    Known(BoundaryEvidence),

    /// Decoding forward from a known instruction start landed exactly on it.
    Reached {
        /// The known start decoding began at.
        from: VirtualAddress,

        /// Why that start is known.
        evidence: BoundaryEvidence,
    },

    /// Decoding forward from the nearest known instruction start overlapped
    /// the address, which therefore probably lies inside an instruction.
    Crossed {
        /// The known start decoding began at.
        from: VirtualAddress,

        /// The decoded instruction containing the address.
        instruction: VirtualAddress,
    },

    /// The address could not be checked.
    Unverified(ContextShortfall),
}

/// How a disassembled function was identified.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FunctionOrigin {
    /// Debug information describes an out-of-line function instance.
    DebugInfo {
        /// The function instance.
        instance: CodeInstanceId,
    },

    /// Only a code symbol describes the function.
    Symbol {
        /// The symbol.
        symbol: SymbolId,

        /// How the end of the symbol's extent was determined.
        provenance: SymbolExtentProvenance,
    },
}

/// A function whose code was disassembled.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DisassembledFunction {
    /// The loaded module containing the function.
    pub module: ModuleId,

    /// The source-level name for debug information, or the linker name.
    pub name: Arc<str>,

    /// How the function was identified.
    pub origin: FunctionOrigin,
}

impl DisassembledFunction {
    /// Returns the source-level spelling of a Rust, C++, or D mangled name, or
    /// `None` when the name is not mangled in a recognized scheme.
    #[must_use]
    pub fn demangled_name(&self) -> Option<String> {
        crate::demangle::demangle(&self.name)
    }
}

/// The instructions a disassembly decoded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DisassemblyView {
    /// Every address range of one function, in address order.
    Function {
        /// The function.
        function: DisassembledFunction,

        /// One block per range.
        blocks: Arc<[DisassemblyBlock]>,
    },

    /// Instructions before and from one address.
    Window {
        /// The address the window is centered on.
        address: VirtualAddress,

        /// What proves that the address begins an instruction.
        boundary: TargetBoundary,

        /// Why fewer instructions than requested precede the address.
        leading: Option<ContextShortfall>,

        /// The instructions before and from the address.
        block: DisassemblyBlock,
    },
}

/// Instructions decoded from one stopped snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Disassembly {
    /// The debugger revision at which the bytes were read.
    pub revision: u64,

    /// The stopped snapshot that authorized the read.
    pub stop_id: StopId,

    /// The target the instructions were decoded for.
    pub target: TargetDescription,

    /// The syntax the instructions were rendered in.
    pub syntax: AssemblySyntax,

    /// The decoded instructions.
    pub view: DisassemblyView,
}

/// The code a disassembly decodes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DisassemblyRange {
    /// Every address range of the function containing an address.
    Function(VirtualAddress),

    /// Instructions around an address the caller asserts begins one: up to
    /// `before` instructions proven to precede it, then `after` instructions
    /// beginning with it.
    Window {
        /// The address.
        address: VirtualAddress,

        /// The instructions to show before the address.
        before: u32,

        /// The instructions to show from the address onward.
        after: u32,
    },
}

/// Selects the code to disassemble and how to render it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DisassemblyQuery {
    /// The code to decode.
    pub range: DisassemblyRange,

    /// The syntax to render instructions in.
    pub syntax: AssemblySyntax,
}

/// Bytes read from the target: a readable prefix and, when it is short,
/// why the next byte is unreadable.
pub struct CodeRead {
    pub bytes: Vec<u8>,
    pub unreadable: Option<MemoryReadUnavailableReason>,
}

/// The target state one disassembly reads.
pub trait CodeSource {
    /// Reads the readable prefix of `size` bytes at `address`, with
    /// debugger breakpoint traps hidden.
    fn read(&mut self, address: VirtualAddress, size: usize) -> Result<CodeRead>;

    /// Returns the known instruction starts within `range` in address order,
    /// one per address.
    fn instruction_starts(
        &self,
        range: AddressRange<VirtualAddress>,
    ) -> BTreeMap<VirtualAddress, BoundaryEvidence>;

    /// Describes the module, section, and symbol containing an address.
    fn describe(&self, address: VirtualAddress) -> AddressDescription;

    /// Returns the source line containing an instruction address.
    fn source_location(&self, address: VirtualAddress) -> Option<SourceLocation>;

    /// Returns the registers the selected thread will execute the instruction
    /// at `address` with, when it is about to execute that instruction. A
    /// register whose value the stop does not determine is absent.
    fn registers(&self, address: VirtualAddress) -> Option<&RegisterFile>;
}

/// Where an indirect branch's target comes from, as one decoding determines
/// it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RawIndirect {
    /// `size` bytes at `address`, in the target's byte order, hold the target.
    Load { address: u64, size: usize },

    /// A register holds the target.
    Value(u64),

    /// The target depends on registers that were not supplied.
    NeedsRegisters,

    /// The decoder does not compute this form's target.
    Unsupported,
}

/// One decoding step of an architecture's instruction decoder.
pub enum RawDecode {
    Instruction {
        length: usize,
        tokens: Vec<InstructionToken>,
        flow: ControlFlow,
        references: Vec<(InstructionReferenceKind, u64)>,

        /// Present exactly for an indirect jump, call, or return.
        indirect: Option<RawIndirect>,
    },

    /// The bytes begin no valid instruction.
    Invalid,

    /// The bytes are a valid prefix of a longer instruction.
    Incomplete,
}

/// Decodes and renders one architecture's instructions.
pub trait InstructionDecoder {
    /// The longest encoding of one instruction.
    fn max_instruction_length(&self) -> usize;

    /// The byte order of values in memory.
    fn byte_order(&self) -> ByteOrder;

    /// Decodes the instruction at `address` from its leading bytes. An
    /// indirect branch's register operands are evaluated only against
    /// `registers`, the values the instruction will execute with.
    fn decode(&mut self, address: u64, bytes: &[u8], registers: Option<&RegisterFile>)
    -> RawDecode;
}

/// Returns the decoder for a target, or an error for an unsupported one.
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

/// Decodes target code from proven instruction starts.
pub struct Engine<'a> {
    source: &'a mut dyn CodeSource,
    decoder: &'a mut dyn InstructionDecoder,
    chunks: BTreeMap<u64, Chunk>,
}

/// One aligned chunk of target memory: its readable prefix and, when that is
/// short, why the rest is unreadable.
struct Chunk {
    bytes: Vec<u8>,
    unreadable: Option<MemoryReadUnavailableReason>,
}

/// Where a sweep must stop.
#[derive(Clone, Copy)]
enum Until {
    /// At a known boundary, which the last instruction must not cross, or
    /// after `limit` instructions.
    Boundary { end: u64, limit: usize },

    /// After `count` instructions.
    Count(usize),
}

/// How a sweep ended.
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

    /// Memory became unreadable at this address.
    Unreadable {
        address: u64,
        reason: MemoryReadUnavailableReason,
    },
}

/// A decoded window and what proves its address.
pub struct Window {
    pub boundary: TargetBoundary,
    pub shortfall: Option<ContextShortfall>,
    pub block: DisassemblyBlock,
}

/// The instructions proven to precede a window's address.
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

/// The leading bytes at one address: up to the requested length, fewer when
/// memory becomes unreadable.
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

    /// Decodes each range of a function, sharing one instruction limit.
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

    /// Decodes up to `before` instructions proven to precede `address` and
    /// `after` instructions beginning at it.
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

    /// Decodes the instructions proven to precede `address`, walking back
    /// one known start at a time. Each segment runs between consecutive
    /// known starts, so only its end can be crossed; a crossed or unreadable
    /// segment ends the walk, since nothing before it is proven.
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

    /// Decodes forward from `start`, resuming at every known start that an
    /// instruction overlaps.
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
            instructions.push(self.instruction(address, &view.bytes[..length], content));

            // A known start inside the range takes precedence over the range
            // end: decoding resumes there rather than ending.
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

    /// Resolves where an indirect branch transfers control by reading the
    /// memory holding its target.
    fn indirect_target(&mut self, indirect: RawIndirect) -> Result<IndirectTarget> {
        Ok(match indirect {
            RawIndirect::Load { address, size } => {
                let slot = self.source.describe(VirtualAddress::new(address));
                let view = self.view(address, size)?;

                if view.bytes.len() < size {
                    // A view never extends past the final address.
                    IndirectTarget::Unreadable {
                        slot,
                        address: VirtualAddress::new(address + view.bytes.len() as u64),
                        reason: view.unreadable.ok_or_else(|| {
                            Error::internal("only unreadable memory shortens a view")
                        })?,
                    }
                } else {
                    let target = target_value(&view.bytes, self.decoder.byte_order());
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
        content: InstructionContent,
    ) -> DisassembledInstruction {
        DisassembledInstruction {
            address,
            bytes: bytes.into(),
            content,
            location: self.source.describe(address),
            source: self.source.source_location(address),
        }
    }

    /// Returns up to `length` bytes at `address`, reading aligned chunks so
    /// that neighboring instructions share reads.
    fn view(&mut self, address: u64, length: usize) -> Result<View> {
        let mut bytes = Vec::with_capacity(length);
        let mut current = address;

        while bytes.len() < length {
            let chunk_start = current - current % READ_CHUNK;
            if !self.chunks.contains_key(&chunk_start) {
                let chunk = self.read_chunk(chunk_start)?;
                self.chunks.insert(chunk_start, chunk);
            }

            let chunk = &self.chunks[&chunk_start];
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

                // Readable memory may begin after the chunk's unreadable
                // prefix ends, so read this address directly.
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
            bytes.extend_from_slice(&chunk.bytes[offset..offset + take]);
            current += take as u64;
        }

        Ok(View {
            bytes,
            unreadable: None,
        })
    }

    fn read_chunk(&mut self, start: u64) -> Result<Chunk> {
        // The final chunk of the address space is shortened rather than
        // wrapping; its remainder is unreadable.
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

/// Reads an address of up to eight bytes stored in `byte_order`.
fn target_value(bytes: &[u8], byte_order: ByteOrder) -> u64 {
    let mut word = [0; 8];
    match byte_order {
        ByteOrder::Little => {
            word[..bytes.len()].copy_from_slice(bytes);
            u64::from_le_bytes(word)
        }
        ByteOrder::Big => {
            word[8 - bytes.len()..].copy_from_slice(bytes);
            u64::from_be_bytes(word)
        }
    }
}

#[cfg(any(test, feature = "fuzzing"))]
mod fake;
#[cfg(feature = "fuzzing")]
mod fuzz;
#[cfg(test)]
mod tests;
