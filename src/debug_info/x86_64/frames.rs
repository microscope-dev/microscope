//! The frame of code no compiler describes, proved from its instructions:
//! where, at each instruction, its return address and its caller's frame
//! pointer are.
//!
//! Like call-frame information, the proof takes a frame to be a function
//! of the instruction: every path to an instruction arrives with the same
//! frame, so the frame an instruction has on the path that falls through
//! from the entry is the frame it always has. A call returns with the
//! stack and frame pointers it was made with, as the x86-64 System V ABI
//! requires. Past an instruction that never falls through, as a jump or a
//! return, no path from the entry is known, and nothing is proved.

use iced_x86::{
    Decoder, DecoderOptions, FlowControl, Instruction, InstructionInfoFactory, Mnemonic, OpAccess,
    OpKind, Register,
};

use super::{full_register, is_immediate, is_push_register, is_register_move};
use crate::unwind::{MemoryReader, RBP, RETURN_ADDRESS, RSP, RegisterFile, UnwindStep};
use crate::{AddressRange, ImageAddress, UnwindTermination, VirtualAddress};

/// Where a frame keeps what its caller needs, at one instruction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::debug_info) enum CodeFrame {
    /// Found from the stack pointer: since its entry, the code has pushed
    /// or allocated `pushed` bytes, so its return address is at
    /// rsp + `pushed`.
    Stack {
        pushed: u64,
        caller_frame_pointer: CallerFramePointer,
    },

    /// Found from the frame pointer, which holds the address of the word
    /// that saved the caller's, `offset` bytes below the canonical frame
    /// address. Standard code saves it right below the return address, 16
    /// bytes below. `below` is how far the stack pointer is below rbp,
    /// when the code proves it.
    Based { offset: u64, below: Option<u64> },
}

/// Where the caller's frame pointer is, in a frame found from the stack
/// pointer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::debug_info) enum CallerFramePointer {
    /// rbp still holds it.
    Kept,

    /// The word this many bytes below the canonical frame address holds it.
    Saved(u64),

    /// Nowhere the code proves: it changed rbp without saving it first,
    /// or moved the word that saved it.
    Lost,
}

/// The frame of every instruction of one function's code, from its entry
/// as far as the code proves it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::debug_info) struct CodeFrames {
    /// The offset of each instruction whose frame differs from the one
    /// before, with its frame, in order.
    changes: Vec<(u64, CodeFrame)>,

    /// The offset past the last instruction whose frame is proved.
    proved: u64,
}

/// What an instruction leaves for the instruction after it.
enum Next {
    Frame(CodeFrame),

    /// The instruction never falls through, or leaves a frame its
    /// instructions do not prove.
    Unproved,
}

impl CodeFrames {
    /// Proves the frame at each instruction of `code`, the bytes of a
    /// function from its entry.
    pub(in crate::debug_info) fn analyze(code: &[u8]) -> Self {
        let mut frame = CodeFrame::Stack {
            pushed: 0,
            caller_frame_pointer: CallerFramePointer::Kept,
        };
        let mut changes = vec![(0, frame)];
        let mut proved = 0;
        let mut decoder = Decoder::with_ip(64, code, 0, DecoderOptions::NONE);
        let mut factory = InstructionInfoFactory::new();

        while decoder.can_decode() {
            let instruction = decoder.decode();
            if instruction.is_invalid() {
                break;
            }

            if changes.last().is_some_and(|(_, last)| *last != frame) {
                changes.push((instruction.ip(), frame));
            }

            proved = instruction.next_ip();
            match next(frame, &instruction, &mut factory) {
                Next::Frame(next) => frame = next,
                Next::Unproved => break,
            }
        }

        Self { changes, proved }
    }

    /// The frame of the instruction that holds the byte `offset` bytes
    /// past the entry, if proved.
    pub(in crate::debug_info) fn at(&self, offset: u64) -> Option<CodeFrame> {
        if offset >= self.proved {
            return None;
        }

        let change = self.changes.partition_point(|(at, _)| *at <= offset);
        self.changes
            .get(change.checked_sub(1)?)
            .map(|(_, frame)| *frame)
    }
}

fn next(frame: CodeFrame, instruction: &Instruction, factory: &mut InstructionInfoFactory) -> Next {
    match instruction.flow_control() {
        FlowControl::Next | FlowControl::ConditionalBranch => {}
        FlowControl::Call | FlowControl::IndirectCall => return Next::Frame(frame),
        _ => return Next::Unproved,
    }

    match frame {
        CodeFrame::Stack {
            pushed,
            caller_frame_pointer,
        } => next_on_stack(pushed, caller_frame_pointer, instruction, factory),
        CodeFrame::Based { offset, below } => next_based(offset, below, instruction, factory),
    }
}

/// The frame after `instruction` of a frame found from the stack pointer.
fn next_on_stack(
    pushed: u64,
    caller_frame_pointer: CallerFramePointer,
    instruction: &Instruction,
    factory: &mut InstructionInfoFactory,
) -> Next {
    use CallerFramePointer::{Kept, Lost, Saved};

    let stack = |pushed: Option<u64>, caller_frame_pointer| {
        pushed.map_or(Next::Unproved, |pushed| {
            Next::Frame(CodeFrame::Stack {
                pushed,
                caller_frame_pointer,
            })
        })
    };

    // The word at rsp is `pushed` + 8 bytes below the frame.
    let top = pushed.saturating_add(8);
    if is_push_register(instruction, Register::RBP) {
        let saved = match caller_frame_pointer {
            Kept => Saved(top.saturating_add(8)),
            other => other,
        };
        return stack(pushed.checked_add(8), saved);
    }

    if is_pop(instruction) {
        // The word popped may be the caller's saved frame pointer: into
        // rbp, it is kept again; anywhere else, it is gone.
        let popped = caller_frame_pointer == Saved(top);
        let caller_frame_pointer = match (is_pop_register(instruction, Register::RBP), popped) {
            (true, true) => Kept,
            (true, false) => match caller_frame_pointer {
                Kept => Lost,
                other => other,
            },
            (false, true) => Lost,
            (false, false) => caller_frame_pointer,
        };

        return stack(pushed.checked_sub(8), caller_frame_pointer);
    }

    if is_push(instruction) {
        return stack(pushed.checked_add(8), caller_frame_pointer);
    }

    if let Some(change) = stack_adjustment(instruction) {
        return stack(pushed.checked_add_signed(change), caller_frame_pointer);
    }

    if is_register_move(instruction, Register::RBP, Register::RSP)
        && caller_frame_pointer == Saved(top)
    {
        return Next::Frame(CodeFrame::Based {
            offset: top,
            below: Some(0),
        });
    }

    let written = written(instruction, factory);
    if written.contains(&Register::RSP) {
        return Next::Unproved;
    }

    let caller_frame_pointer = if written.contains(&Register::RBP) && caller_frame_pointer == Kept {
        Lost
    } else {
        caller_frame_pointer
    };

    stack(Some(pushed), caller_frame_pointer)
}

/// The frame after `instruction` of a frame found from the frame pointer.
fn next_based(
    offset: u64,
    below: Option<u64>,
    instruction: &Instruction,
    factory: &mut InstructionInfoFactory,
) -> Next {
    // Leaving the frame puts the stack pointer back where rbp points, at
    // the word that saved the caller's frame pointer.
    let restored = |pushed: Option<u64>| {
        pushed.map_or(Next::Unproved, |pushed| {
            Next::Frame(CodeFrame::Stack {
                pushed,
                caller_frame_pointer: CallerFramePointer::Saved(offset),
            })
        })
    };
    let based = |below: Option<u64>| Next::Frame(CodeFrame::Based { offset, below });

    if instruction.mnemonic() == Mnemonic::Leave
        || (is_pop_register(instruction, Register::RBP) && below == Some(0))
    {
        return offset.checked_sub(16).map_or(Next::Unproved, |pushed| {
            Next::Frame(CodeFrame::Stack {
                pushed,
                caller_frame_pointer: CallerFramePointer::Kept,
            })
        });
    }

    if is_register_move(instruction, Register::RSP, Register::RBP) {
        return restored(offset.checked_sub(8));
    }

    if let Some(displacement) = frame_relative_stack(instruction) {
        return restored(
            offset
                .checked_add_signed(-displacement)
                .and_then(|below| below.checked_sub(8)),
        );
    }

    let written = written(instruction, factory);
    if written.contains(&Register::RBP) {
        return Next::Unproved;
    }

    if is_push(instruction) {
        return based(below.and_then(|below| below.checked_add(8)));
    }

    if is_pop(instruction) {
        return based(below.and_then(|below| below.checked_sub(8)));
    }

    if let Some(change) = stack_adjustment(instruction) {
        return based(below.and_then(|below| below.checked_add_signed(change)));
    }

    if written.contains(&Register::RSP) {
        return based(None);
    }

    based(below)
}

/// The full registers `instruction` writes, implicitly or not.
fn written(instruction: &Instruction, factory: &mut InstructionInfoFactory) -> Vec<Register> {
    factory
        .info(instruction)
        .used_registers()
        .iter()
        .filter(|used| {
            matches!(
                used.access(),
                OpAccess::Write
                    | OpAccess::CondWrite
                    | OpAccess::ReadWrite
                    | OpAccess::ReadCondWrite
            )
        })
        .map(|used| full_register(used.register()))
        .collect()
}

/// A push of eight bytes.
fn is_push(instruction: &Instruction) -> bool {
    instruction.mnemonic() == Mnemonic::Push && instruction.stack_pointer_increment() == -8
}

/// A pop of eight bytes.
fn is_pop(instruction: &Instruction) -> bool {
    instruction.mnemonic() == Mnemonic::Pop && instruction.stack_pointer_increment() == 8
}

fn is_pop_register(instruction: &Instruction, register: Register) -> bool {
    is_pop(instruction)
        && instruction.op0_kind() == OpKind::Register
        && instruction.op0_register() == register
}

/// How many bytes `sub rsp, imm`, `add rsp, imm`, or `lea rsp, [rsp+disp]`
/// allocates, negative when it frees them.
fn stack_adjustment(instruction: &Instruction) -> Option<i64> {
    if instruction.op_count() != 2
        || instruction.op0_kind() != OpKind::Register
        || instruction.op0_register() != Register::RSP
    {
        return None;
    }

    let immediate =
        || is_immediate(instruction.op1_kind()).then(|| instruction.immediate(1).cast_signed());
    match instruction.mnemonic() {
        Mnemonic::Sub => immediate(),
        Mnemonic::Add => immediate().and_then(i64::checked_neg),
        Mnemonic::Lea if is_plain_memory(instruction, Register::RSP) => instruction
            .memory_displacement64()
            .cast_signed()
            .checked_neg(),
        _ => None,
    }
}

/// The displacement of `lea rsp, [rbp+disp]`.
fn frame_relative_stack(instruction: &Instruction) -> Option<i64> {
    (instruction.mnemonic() == Mnemonic::Lea
        && instruction.op0_kind() == OpKind::Register
        && instruction.op0_register() == Register::RSP
        && is_plain_memory(instruction, Register::RBP))
    .then(|| instruction.memory_displacement64().cast_signed())
}

fn is_plain_memory(instruction: &Instruction, base: Register) -> bool {
    instruction.op1_kind() == OpKind::Memory
        && instruction.memory_base() == base
        && instruction.memory_index() == Register::None
        && instruction.segment_prefix() == Register::None
}

/// The most bytes an encoded frame names as pushed or as an offset.
const MOST_ENCODED_BYTES: u64 = (1 << 15) - 1;

impl CodeFrame {
    /// The frame as an image table holds it: a tag in the low two bits
    /// and up to two byte counts of 15 bits each above it. A frame whose
    /// counts do not fit has no encoding, and `below`, which no unwind
    /// reads, is not kept.
    pub(in crate::debug_info) fn encode(self) -> Option<u32> {
        let (tag, first, second) = match self {
            Self::Stack {
                pushed,
                caller_frame_pointer: CallerFramePointer::Kept,
            } => (0, pushed, 0),
            Self::Stack {
                pushed,
                caller_frame_pointer: CallerFramePointer::Saved(offset),
            } => (1, pushed, offset),
            Self::Stack {
                pushed,
                caller_frame_pointer: CallerFramePointer::Lost,
            } => (2, pushed, 0),
            Self::Based { offset, .. } => (3, offset, 0),
        };
        if first > MOST_ENCODED_BYTES || second > MOST_ENCODED_BYTES {
            return None;
        }
        u32::try_from(tag | first << 2 | second << 17).ok()
    }

    /// The frame `encode` gave `value`, if it names one.
    pub(in crate::debug_info) fn decode(value: u32) -> Option<Self> {
        let value = u64::from(value);
        let (first, second) = ((value >> 2) & MOST_ENCODED_BYTES, value >> 17);
        let stack = |caller_frame_pointer| Self::Stack {
            pushed: first,
            caller_frame_pointer,
        };
        match (value & 3, second) {
            (0, 0) => Some(stack(CallerFramePointer::Kept)),
            (1, offset) => Some(stack(CallerFramePointer::Saved(offset))),
            (2, 0) => Some(stack(CallerFramePointer::Lost)),
            (3, 0) => Some(Self::Based {
                offset: first,
                below: None,
            }),
            _ => None,
        }
    }
}

/// Adds the frames of one function of a runtime's code that no call-frame
/// information describes, whose code is `code`, at `address`, by encoded
/// frame. Where its instructions prove the frame, that frame is kept.
/// Past that, the runtime's convention holds at each call the code makes:
/// it calls only from a frame it has entered, whose rbp holds the address
/// of the word that saved the caller's, 16 bytes below the canonical frame
/// address. Elsewhere nothing is known.
pub(in crate::debug_info) fn runtime_frames(
    code: &[u8],
    address: u64,
    frames: &mut Vec<(AddressRange<ImageAddress>, u32)>,
) {
    let range = |start: u64, end: u64| AddressRange {
        start: ImageAddress::new(address.saturating_add(start)),
        end: ImageAddress::new(address.saturating_add(end)),
    };
    let proved = CodeFrames::analyze(code);
    let ends = proved
        .changes
        .iter()
        .skip(1)
        .map(|(offset, _)| *offset)
        .chain(std::iter::once(proved.proved));
    for ((start, frame), end) in proved.changes.iter().zip(ends) {
        if let Some(value) = frame.encode().filter(|_| start < &end) {
            frames.push((range(*start, end), value));
        }
    }

    let convention = CodeFrame::Based {
        offset: 16,
        below: None,
    }
    .encode();
    let Some(convention) = convention else {
        return;
    };
    let mut decoder = Decoder::with_ip(64, code, 0, DecoderOptions::NONE);
    decoder
        .set_position(
            usize::try_from(proved.proved)
                .unwrap_or(code.len())
                .min(code.len()),
        )
        .ok();
    decoder.set_ip(proved.proved);
    while decoder.can_decode() {
        let instruction = decoder.decode();
        if instruction.is_invalid() {
            continue;
        }
        if matches!(
            instruction.flow_control(),
            FlowControl::Call | FlowControl::IndirectCall
        ) {
            frames.push((range(instruction.ip(), instruction.next_ip()), convention));
        }
    }
}

/// The canonical frame address of `frame`, with `registers` its own.
pub(in crate::debug_info) fn frame_cfa(
    frame: CodeFrame,
    registers: &RegisterFile,
) -> Result<VirtualAddress, UnwindTermination> {
    let cfa = match frame {
        CodeFrame::Stack { pushed, .. } => register(registers, RSP, "rsp")?
            .checked_add(pushed)
            .and_then(|top| top.checked_add(8)),
        CodeFrame::Based { offset, .. } => register(registers, RBP, "rbp")?.checked_add(offset),
    };

    cfa.map(VirtualAddress::new)
        .ok_or_else(|| UnwindTermination::InvalidCaller {
            description: "a frame ends past the address space".into(),
        })
}

/// The caller of `frame`: its stack pointer, return address, and frame
/// pointer, as far as the frame proves them. The code's calling
/// convention is not known, so no other register is.
pub(in crate::debug_info) fn frame_caller(
    frame: CodeFrame,
    registers: &RegisterFile,
    memory: &mut dyn MemoryReader,
) -> Result<UnwindStep, UnwindTermination> {
    let cfa = frame_cfa(frame, registers)?;
    let return_address = read(memory, cfa, 8)?;

    let frame_pointer = match frame {
        CodeFrame::Based { offset, .. }
        | CodeFrame::Stack {
            caller_frame_pointer: CallerFramePointer::Saved(offset),
            ..
        } => Some(read(memory, cfa, offset)?),
        CodeFrame::Stack {
            caller_frame_pointer: CallerFramePointer::Kept,
            ..
        } => registers.get(RBP),
        CodeFrame::Stack {
            caller_frame_pointer: CallerFramePointer::Lost,
            ..
        } => None,
    };

    let mut caller = RegisterFile::new([(RETURN_ADDRESS, return_address), (RSP, cfa.get())]);
    if let Some(frame_pointer) = frame_pointer {
        caller.set(RBP, frame_pointer);
    }

    Ok(UnwindStep {
        registers: caller,
        cfa,
        signal_frame: false,
    })
}

fn register(
    registers: &RegisterFile,
    number: crate::unwind::DwarfRegister,
    name: &str,
) -> Result<u64, UnwindTermination> {
    registers
        .get(number)
        .ok_or_else(|| UnwindTermination::RegisterUnavailable {
            register: name.into(),
        })
}

fn read(
    memory: &mut dyn MemoryReader,
    cfa: VirtualAddress,
    below: u64,
) -> Result<u64, UnwindTermination> {
    let address = VirtualAddress::new(cfa.get().wrapping_sub(below));
    memory
        .read_u64(address)
        .ok_or(UnwindTermination::MemoryReadFailed { address })
}

#[cfg(test)]
mod tests {
    use super::*;

    use CallerFramePointer::{Kept, Lost, Saved};
    use CodeFrame::{Based, Stack};

    /// A frame found from the stack pointer, as `CodeFrames::at` gives it.
    #[expect(
        clippy::unnecessary_wraps,
        reason = "it is compared with what the analysis may not prove"
    )]
    const fn stack(pushed: u64, caller_frame_pointer: CallerFramePointer) -> Option<CodeFrame> {
        Some(Stack {
            pushed,
            caller_frame_pointer,
        })
    }

    /// The frame at each offset of `code`.
    fn frames(code: &[u8]) -> Vec<Option<CodeFrame>> {
        let frames = CodeFrames::analyze(code);
        (0..=code.len() as u64)
            .map(|offset| frames.at(offset))
            .collect()
    }

    #[test]
    fn an_encoded_frame_decodes_to_itself_and_bounds_are_refused() {
        for frame in [
            Stack {
                pushed: 0,
                caller_frame_pointer: Kept,
            },
            Stack {
                pushed: 0x7ff8,
                caller_frame_pointer: Saved(0x7fff),
            },
            Stack {
                pushed: 24,
                caller_frame_pointer: Lost,
            },
            Based {
                offset: 16,
                below: None,
            },
        ] {
            let encoded = frame.encode().expect("it fits");
            assert_eq!(CodeFrame::decode(encoded), Some(frame), "{frame:?}");
        }
        // `below` is not kept, and a count past 15 bits has no encoding.
        let based = Based {
            offset: 16,
            below: Some(8),
        };
        assert_eq!(
            based.encode().and_then(CodeFrame::decode),
            Some(Based {
                offset: 16,
                below: None
            })
        );
        assert_eq!(stack(1 << 15, Kept).and_then(CodeFrame::encode), None);
        // A Kept or Lost frame with a second count is no encoding.
        assert_eq!(CodeFrame::decode(1 << 17), None);
        assert_eq!(CodeFrame::decode(2 | 1 << 17), None);
        assert_eq!(CodeFrame::decode(3 | 1 << 17), None);
    }

    #[test]
    fn a_runtime_function_is_proved_to_its_first_unproved_instruction_then_at_calls() {
        // V8's CallApiCallbackGeneric opens with `pop rax`, its return
        // address, which nothing proves; later, `call [r13+0x40]` keeps
        // the convention's frame.
        // pop rax; push rbp; mov rbp, rsp; call [r13+0x40]; leave; ret
        let code = [
            0x58, 0x55, 0x48, 0x89, 0xe5, 0x41, 0xff, 0x55, 0x40, 0xc9, 0xc3,
        ];
        let mut frames = Vec::new();
        runtime_frames(&code, 0x1000, &mut frames);
        let entry = stack(0, Kept).and_then(CodeFrame::encode).expect("entry");
        let convention = Based {
            offset: 16,
            below: None,
        }
        .encode()
        .expect("convention");
        let range = |start: u64, end: u64| AddressRange {
            start: ImageAddress::new(start),
            end: ImageAddress::new(end),
        };
        assert_eq!(
            frames,
            [
                (range(0x1000, 0x1001), entry),
                (range(0x1005, 0x1009), convention)
            ]
        );
    }

    #[test]
    fn a_trampoline_without_a_frame_pointer_is_found_from_the_stack() {
        // CPython 3.12's: sub rsp, 8; call rcx; add rsp, 8; ret
        let code = [
            0x48, 0x83, 0xec, 0x08, 0xff, 0xd1, 0x48, 0x83, 0xc4, 0x08, 0xc3,
        ];
        let entry = stack(0, Kept);
        let called = stack(8, Kept);
        assert_eq!(
            frames(&code),
            [
                entry, entry, entry, entry, called, called, called, called, called, called, entry,
                None
            ]
        );
    }

    #[test]
    fn a_trampoline_with_a_frame_pointer_is_found_from_it() {
        // CPython 3.13's: push rbp; mov rbp, rsp; call rcx; pop rbp; ret
        let code = [0x55, 0x48, 0x89, 0xe5, 0xff, 0xd1, 0x5d, 0xc3];
        let pushed = stack(8, Saved(16));
        let based = Some(Based {
            offset: 16,
            below: Some(0),
        });
        assert_eq!(
            frames(&code),
            [
                stack(0, Kept),
                pushed,
                pushed,
                pushed,
                based,
                based,
                based,
                stack(0, Kept),
                None
            ]
        );
    }

    #[test]
    fn a_frame_left_by_leave_or_through_rbp_is_found_from_the_stack_again() {
        // endbr64; push rbp; mov rbp, rsp; push rbx; sub rsp, 0x18; call rax;
        // lea rsp, [rbp-8]; pop rbx; pop rbp; ret
        let code = [
            0xf3, 0x0f, 0x1e, 0xfa, 0x55, 0x48, 0x89, 0xe5, 0x53, 0x48, 0x83, 0xec, 0x18, 0xff,
            0xd0, 0x48, 0x8d, 0x65, 0xf8, 0x5b, 0x5d, 0xc3,
        ];
        let frames = CodeFrames::analyze(&code);
        let based = |below| Some(Based { offset: 16, below });
        assert_eq!(frames.at(8), based(Some(0)), "at push rbx");
        assert_eq!(frames.at(13), based(Some(32)), "at the call");
        assert_eq!(frames.at(19), stack(16, Saved(16)), "at pop rbx");
        assert_eq!(frames.at(20), stack(8, Saved(16)), "at pop rbp");
        assert_eq!(frames.at(21), stack(0, Kept), "at ret");

        // push rbp; mov rbp, rsp; sub rsp, 0x20; leave; ret
        let based = |below| Some(Based { offset: 16, below });
        let code = [0x55, 0x48, 0x89, 0xe5, 0x48, 0x83, 0xec, 0x20, 0xc9, 0xc3];
        let frames = CodeFrames::analyze(&code);
        assert_eq!(frames.at(8), based(Some(32)), "at leave");
        assert_eq!(frames.at(9), stack(0, Kept), "at ret");
    }

    #[test]
    fn rbp_saved_after_other_registers_is_found_where_it_was_pushed() {
        // push rbx; push rbp; mov rbp, rsp; call rax
        let code = [0x53, 0x55, 0x48, 0x89, 0xe5, 0xff, 0xd0];
        let frames = CodeFrames::analyze(&code);
        assert_eq!(frames.at(2), stack(16, Saved(24)));
        assert_eq!(
            frames.at(5),
            Some(Based {
                offset: 24,
                below: Some(0)
            })
        );
    }

    #[test]
    fn rbp_changed_unsaved_loses_the_callers_and_saved_keeps_it() {
        // mov rbp, rdi; call rax
        let frames = CodeFrames::analyze(&[0x48, 0x89, 0xfd, 0xff, 0xd0]);
        assert_eq!(frames.at(3), stack(0, Lost));

        // push rbp; mov rbp, rdi; call rax
        let frames = CodeFrames::analyze(&[0x55, 0x48, 0x89, 0xfd, 0xff, 0xd0]);
        assert_eq!(frames.at(4), stack(8, Saved(16)));

        // push rbp; mov rbp, rdi; pop rax; call rax: the saved word is
        // popped elsewhere.
        let frames = CodeFrames::analyze(&[0x55, 0x48, 0x89, 0xfd, 0x58, 0xff, 0xd0]);
        assert_eq!(frames.at(5), stack(0, Lost));
    }

    #[test]
    fn conditional_branches_fall_through_and_other_transfers_end_the_proof() {
        // push rbp; mov rbp, rsp; je +0; call rax
        let based = Some(Based {
            offset: 16,
            below: Some(0),
        });
        let frames = CodeFrames::analyze(&[0x55, 0x48, 0x89, 0xe5, 0x74, 0x00, 0xff, 0xd0]);
        assert_eq!(frames.at(6), based);

        // push rbp; mov rbp, rsp; jmp +0; call rax
        let frames = CodeFrames::analyze(&[0x55, 0x48, 0x89, 0xe5, 0xeb, 0x00, 0xff, 0xd0]);
        assert_eq!(frames.at(4), based, "the jump itself");
        assert_eq!(frames.at(6), None);

        // ud2; call rax
        assert_eq!(CodeFrames::analyze(&[0x0f, 0x0b, 0xff, 0xd0]).at(2), None);
    }

    #[test]
    fn a_stack_pointer_the_code_cannot_follow_ends_the_proof() {
        // and rsp, -16; call rax
        let frames = CodeFrames::analyze(&[0x48, 0x83, 0xe4, 0xf0, 0xff, 0xd0]);
        assert_eq!(frames.at(0), stack(0, Kept));
        assert_eq!(frames.at(4), None);

        // pop rax at entry pops the return address.
        assert_eq!(CodeFrames::analyze(&[0x58, 0xff, 0xd0]).at(1), None);

        // push rbp; mov rbp, rsp; mov rbp, rdi; call rax
        let frames = CodeFrames::analyze(&[0x55, 0x48, 0x89, 0xe5, 0x48, 0x89, 0xfd, 0xff, 0xd0]);
        assert_eq!(frames.at(7), None);

        // push rbp; mov rbp, rsp; and rsp, -16; pop rbp: where rsp is, rbp
        // does not say.
        let frames = CodeFrames::analyze(&[0x55, 0x48, 0x89, 0xe5, 0x48, 0x83, 0xe4, 0xf0, 0x5d]);
        assert_eq!(
            frames.at(8),
            Some(Based {
                offset: 16,
                below: None
            })
        );
        assert_eq!(frames.at(9), None);
    }

    #[test]
    fn code_cut_short_is_proved_only_to_its_last_whole_instruction() {
        // push rbp; mov rbp, (cut)
        let frames = CodeFrames::analyze(&[0x55, 0x48, 0x89]);
        assert_eq!(frames.at(0), stack(0, Kept));
        assert_eq!(frames.at(1), None);
        assert_eq!(CodeFrames::analyze(&[]).at(0), None);
    }
}
