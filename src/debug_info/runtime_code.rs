//! Code a runtime generated into anonymous memory, whose frames keep the
//! frame-pointer chain by the runtime's convention: V8's compiled
//! JavaScript, and its builtins where no symbol names them.
//!
//! No table describes such code, and its functions' bounds are not known,
//! so a frame is found from the instructions at the frame's own address.
//! The runtime calls only from a frame it has entered, whose rbp holds the
//! address of the word that saved the caller's, 16 bytes below the
//! canonical frame address. That holds at each call and through the body.
//! Only a frame stopped at the first two instructions of a prologue
//! (`push rbp`, `mov rbp, rsp`) or at a return has not entered it or has
//! left it, which those instructions prove. A caller's address is the last
//! byte of the call it made, which is never one of those.

use std::path::Path;
use std::sync::Arc;

use iced_x86::{Decoder, DecoderOptions, FlowControl, Mnemonic};

use super::x86_64::frames::{CallerFramePointer, CodeFrame, frame_caller, frame_cfa};
use super::{DebugInfo, UnwindInfo};
use crate::unwind::{MemoryReader, RegisterFile, UnwindStep};
use crate::{AddressRange, ImageAddress, ModuleImageId, UnwindTermination, VirtualAddress};

/// How many bytes are read before a frame's address, which hold the call
/// a caller made: the longest call V8 emits, through memory with a 32-bit
/// displacement, is seven.
const BEFORE: u64 = 8;

/// How many bytes are read from a frame's address, which hold the longest
/// instruction a frame is proved by.
const FROM: u64 = 8;

/// The debug information of the runtime's code `range` holds, at the
/// addresses the process runs it.
pub fn load(
    path: &Path,
    id: ModuleImageId,
    range: AddressRange<ImageAddress>,
) -> Result<DebugInfo, crate::model::SealError> {
    load_named(path, id, range, Vec::new())
}

/// The debug information of the runtime's code `range` holds, as [`load`]
/// gives it, with `symbols` naming its functions.
pub(super) fn load_named(
    path: &Path,
    id: ModuleImageId,
    range: AddressRange<ImageAddress>,
    symbols: Vec<crate::SymbolInfo>,
) -> Result<DebugInfo, crate::model::SealError> {
    let tables = crate::model::seal(
        crate::TargetDescription {
            architecture: crate::Architecture::X86_64,
            byte_order: crate::ByteOrder::Little,
            pointer_width: crate::PointerWidth::Bits64,
        },
        range,
        crate::model::ModuleMetadata {
            symbols,
            ..crate::model::ModuleMetadata::default()
        },
    )?;
    let binding = crate::model::Binding {
        path: Arc::new(path.to_path_buf()),
        debug_path: None,
        id,
    };
    let image = Arc::new(crate::ModuleImage::bind(&binding, Arc::new(tables))?);
    Ok(DebugInfo {
        unwind: Arc::new(RuntimeCodeUnwind),
        variables: super::dwarf::no_variables(&image),
        image,
    })
}

/// Unwinds the runtime's frames by its convention. Image addresses are the
/// process's.
struct RuntimeCodeUnwind;

/// The canonical frame address of the runtime's frame whose address in the
/// process is `address`: its instruction in the innermost frame, the last
/// byte of its call in a caller. The frame is found by the runtime's
/// convention, from the code at `address`, as [`load`]'s are.
pub fn cfa_at(
    address: VirtualAddress,
    registers: &RegisterFile,
    memory: &mut dyn MemoryReader,
) -> Result<VirtualAddress, UnwindTermination> {
    frame_cfa(frame(address, memory), registers)
}

/// The caller of the runtime's frame whose address in the process is
/// `address`, as [`cfa_at`] finds the frame.
pub fn caller_at(
    address: VirtualAddress,
    registers: &RegisterFile,
    memory: &mut dyn MemoryReader,
) -> Result<UnwindStep, UnwindTermination> {
    frame_caller(frame(address, memory), registers, memory)
}

/// The frame at `address` of the process, by the instructions around it.
fn frame(address: VirtualAddress, memory: &mut dyn MemoryReader) -> CodeFrame {
    let address = address.get();
    let based = CodeFrame::Based {
        offset: 16,
        below: None,
    };
    let Some(start) = address.checked_sub(BEFORE) else {
        return based;
    };
    let mut bytes = Vec::with_capacity(usize::try_from(BEFORE + FROM).unwrap_or(16));
    for word in [start, address] {
        match memory.read_u64(VirtualAddress::new(word)) {
            Some(value) => bytes.extend_from_slice(&value.to_le_bytes()),
            // Code that cannot be read proves no instruction.
            None => return based,
        }
    }
    frame_at(&bytes, start, address).unwrap_or(based)
}

/// The frame proved at `address` by the bytes from `start`, which hold
/// the instructions around it, if they prove one other than the
/// convention's.
fn frame_at(bytes: &[u8], start: u64, address: u64) -> Option<CodeFrame> {
    let at = usize::try_from(address - start).ok()?;
    let ends_call = (2..=7).any(|length: usize| {
        let Some(from) = (at + 1).checked_sub(length) else {
            return false;
        };
        let mut decoder = Decoder::with_ip(
            64,
            &bytes[from..=at],
            start + from as u64,
            DecoderOptions::NONE,
        );
        let instruction = decoder.decode();
        !instruction.is_invalid()
            && matches!(
                instruction.flow_control(),
                FlowControl::Call | FlowControl::IndirectCall
            )
            && instruction.next_ip() == address + 1
    });
    if ends_call {
        return None;
    }

    let decode = |offset: usize| {
        let mut decoder = Decoder::with_ip(
            64,
            bytes.get(offset..)?,
            start + offset as u64,
            DecoderOptions::NONE,
        );
        Some(decoder.decode())
    };
    let here = decode(at)?;
    let pushes_frame_pointer = |instruction: &iced_x86::Instruction| {
        super::x86_64::is_push_register(instruction, iced_x86::Register::RBP)
    };
    let sets_frame_pointer = |instruction: &iced_x86::Instruction| {
        super::x86_64::is_register_move(
            instruction,
            iced_x86::Register::RBP,
            iced_x86::Register::RSP,
        )
    };
    let entry = CodeFrame::Stack {
        pushed: 0,
        caller_frame_pointer: CallerFramePointer::Kept,
    };

    if here.mnemonic() == Mnemonic::Ret {
        return Some(entry);
    }
    if pushes_frame_pointer(&here) && decode(at + 1).is_some_and(|next| sets_frame_pointer(&next)) {
        return Some(entry);
    }
    let previous = at.checked_sub(1).and_then(decode);
    if sets_frame_pointer(&here) && previous.is_some_and(|previous| pushes_frame_pointer(&previous))
    {
        return Some(CodeFrame::Stack {
            pushed: 8,
            caller_frame_pointer: CallerFramePointer::Saved(16),
        });
    }
    None
}

impl UnwindInfo for RuntimeCodeUnwind {
    fn cfa(
        &self,
        address: ImageAddress,
        registers: &RegisterFile,
        memory: &mut dyn MemoryReader,
    ) -> Result<VirtualAddress, UnwindTermination> {
        cfa_at(VirtualAddress::new(address.get()), registers, memory)
    }

    fn unwind(
        &self,
        address: ImageAddress,
        registers: &RegisterFile,
        memory: &mut dyn MemoryReader,
    ) -> Result<UnwindStep, UnwindTermination> {
        caller_at(VirtualAddress::new(address.get()), registers, memory)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const START: u64 = 0x1000;

    /// The frame at the byte `at` of `code`, which begins at `START`.
    fn frame(code: &[u8], at: u64) -> Option<CodeFrame> {
        frame_at(code, START, START + at)
    }

    #[test]
    fn a_frame_entering_or_leaving_is_proved_by_its_instructions() {
        // push rbp; mov rbp, rsp; push rsi; call [r13+0x40]; mov rsp, rbp;
        // pop rbp; ret
        let code = [
            0x55, 0x48, 0x89, 0xe5, 0x56, 0x41, 0xff, 0x55, 0x40, 0x48, 0x89, 0xec, 0x5d, 0xc3,
        ];
        let entry = Some(CodeFrame::Stack {
            pushed: 0,
            caller_frame_pointer: CallerFramePointer::Kept,
        });
        assert_eq!(frame(&code, 0), entry);
        assert_eq!(
            frame(&code, 1),
            Some(CodeFrame::Stack {
                pushed: 8,
                caller_frame_pointer: CallerFramePointer::Saved(16),
            })
        );
        // The body, the end of the call a caller made, and the epilogue
        // until rbp is the caller's hold the convention.
        for at in [4, 5, 8, 9, 12] {
            assert_eq!(frame(&code, at), None, "{at}");
        }
        assert_eq!(frame(&code, 13), entry);
    }

    #[test]
    fn the_end_of_a_call_is_a_callers_frame_whatever_its_bytes_say() {
        // call rel32 whose last byte is 0xc3, a ret on its own, then
        // push rbp; mov rbp, rsp, which is a prologue only from its start.
        let code = [0xe8, 0x00, 0x00, 0x00, 0xc3, 0x55, 0x48, 0x89, 0xe5];
        assert_eq!(frame(&code, 4), None);
        // A call's last byte that is a push of rbp is the call's.
        let code = [0xe8, 0x00, 0x00, 0x00, 0x55, 0x48, 0x89, 0xe5];
        assert_eq!(frame(&code, 4), None);
    }
}
