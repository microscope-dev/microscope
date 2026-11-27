//! Unwinder for dynamically generated `x86_64` runtime code without DWARF CFI.
//!
//! Assumes frame-pointer chaining (`RBP`) by default, and inspects a small window
//! around the current PC to handle the prologue (`push rbp; mov rbp, rsp`) and
//! `ret` instructions where `RBP` does not yet (or no longer) point to the frame.

use std::path::Path;
use std::sync::Arc;

use iced_x86::{Decoder, DecoderOptions, FlowControl, Mnemonic};

use super::x86_64::frames::{Below, CallerFramePointer, CodeFrame, frame_caller, frame_cfa};
use super::{DebugInfo, UnwindInfo};
use crate::unwind::{MemoryReader, RegisterFile, UnwindStep};
use crate::{AddressRange, ImageAddress, ModuleImageId, UnwindTermination, VirtualAddress};

const BEFORE: u64 = 8;

const FROM: u64 = 8;

pub fn load(
    path: &Path,
    id: ModuleImageId,
    range: AddressRange<ImageAddress>,
) -> Result<DebugInfo, crate::model::SealError> {
    load_named(path, id, range, Vec::new())
}

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

struct RuntimeCodeUnwind;

pub fn cfa_at(
    address: VirtualAddress,
    registers: &RegisterFile,
    memory: &mut dyn MemoryReader,
) -> Result<VirtualAddress, UnwindTermination> {
    frame_cfa(frame(address, memory), registers)
}

pub fn caller_at(
    address: VirtualAddress,
    registers: &RegisterFile,
    memory: &mut dyn MemoryReader,
) -> Result<UnwindStep, UnwindTermination> {
    frame_caller(frame(address, memory), registers, memory)
}

fn frame(address: VirtualAddress, memory: &mut dyn MemoryReader) -> CodeFrame {
    let address = address.get();
    let based = CodeFrame::Based {
        offset: 16,
        below: Below::Unknown,
    };
    let Some(start) = address.checked_sub(BEFORE) else {
        return based;
    };
    let mut bytes = Vec::with_capacity(usize::try_from(BEFORE + FROM).unwrap_or(16));
    for word in [start, address] {
        match memory.read_u64(VirtualAddress::new(word)) {
            Some(value) => bytes.extend_from_slice(&value.to_le_bytes()),
            None => return based,
        }
    }
    frame_at(&bytes, start, address).unwrap_or(based)
}

fn frame_at(bytes: &[u8], start: u64, address: u64) -> Option<CodeFrame> {
    let at = usize::try_from(address - start).ok()?;
    // Unwinding a caller passes `return_address - 1` (the last byte of the `call`).
    // If bytes ending at `address` decode as a `call`, we are mid-frame and must not
    // misread the call's displacement byte as a `ret` or `push rbp`.
    let ends_call = (2..=7).any(|length: usize| {
        let Some(from) = (at + 1).checked_sub(length) else {
            return false;
        };
        let Some(window) = bytes.get(from..=at) else {
            return false;
        };
        let mut decoder = Decoder::with_ip(64, window, start + from as u64, DecoderOptions::NONE);
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

    fn frame(code: &[u8], at: u64) -> Option<CodeFrame> {
        frame_at(code, START, START + at)
    }

    #[test]
    fn a_frame_entering_or_leaving_is_proved_by_its_instructions() {
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
        for at in [4, 5, 8, 9, 12] {
            assert_eq!(frame(&code, at), None, "{at}");
        }
        assert_eq!(frame(&code, 13), entry);
    }

    #[test]
    fn the_end_of_a_call_is_a_callers_frame_whatever_its_bytes_say() {
        let code = [0xe8, 0x00, 0x00, 0x00, 0xc3, 0x55, 0x48, 0x89, 0xe5];
        assert_eq!(frame(&code, 4), None);
        let code = [0xe8, 0x00, 0x00, 0x00, 0x55, 0x48, 0x89, 0xe5];
        assert_eq!(frame(&code, 4), None);
    }
}
