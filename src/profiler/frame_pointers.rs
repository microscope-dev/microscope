//! Whether the kernel's frame-pointer walk carries a stack through an
//! image: the question auto mode asks of each image to choose how much
//! stack a sample copies.

use iced_x86::{Decoder, DecoderOptions, FlowControl, Mnemonic};
use object::{Object, ObjectSection, SectionFlags};

use crate::debug_info::UnwindInfo;
use crate::unwind::FramePointerRule;
use crate::{CodeRole, ImageAddress, ModuleImage};

/// How an image's call sites keep the frame-pointer chain.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CallSites {
    /// Calls whose frame keeps the chain while the callee runs.
    pub kept: u32,

    /// Calls whose frame does not, by their rules.
    pub not_kept: u32,

    /// The first call that does not keep it, by image address.
    pub first_not_kept: Option<u64>,
}

impl CallSites {
    /// Whether a walk of the chain passes the image's frames: a frame can
    /// stand between two others only while it calls, and all but one call
    /// in a thousand keep the chain. A few hand-written routines, as Go's
    /// `runtime.clone`, call without a frame of their own; a stack through
    /// one still ends there, saying why.
    pub const fn keep_the_chain(self) -> bool {
        self.not_kept as u64 * 1000 <= self.kept as u64 + self.not_kept as u64
    }
}

/// Classifies every call instruction of the executable sections of the
/// ELF image `data` by the frame-pointer rule `unwind` gives it. Left out
/// are calls without rules, which no unwinder passes; calls in outermost
/// frames, where every stack ends; calls in stack switches, which only a
/// runtime model crosses; and calls into the runtime's machinery, which a
/// function may make before its frame exists, as Go's prologue calls
/// `morestack`.
pub fn call_sites(data: &[u8], image: &ModuleImage, unwind: &dyn UnwindInfo) -> CallSites {
    const SHF_EXECINSTR: u64 = 0x4;
    let mut sites = CallSites::default();
    let Ok(file) = object::File::parse(data) else {
        return sites;
    };

    for section in file.sections() {
        let executable = matches!(
            section.flags(),
            SectionFlags::Elf { sh_flags } if sh_flags & SHF_EXECINSTR != 0
        );
        let Ok(code) = section.data() else { continue };
        if !executable || code.is_empty() {
            continue;
        }

        let mut decoder = Decoder::with_ip(64, code, section.address(), DecoderOptions::NONE);

        for instruction in &mut decoder {
            // A system call returns to the instruction after it, with
            // no frame of its own.
            if instruction.mnemonic() != Mnemonic::Call {
                continue;
            }

            let address = ImageAddress::new(instruction.ip());
            if matches!(
                image.code_role(address),
                CodeRole::Outermost | CodeRole::StackSwitch
            ) {
                continue;
            }

            if instruction.flow_control() == FlowControl::Call
                && image.code_role(ImageAddress::new(instruction.near_branch_target()))
                    != CodeRole::Ordinary
            {
                continue;
            }

            match unwind.frame_pointer_rule(address) {
                FramePointerRule::Kept | FramePointerRule::Outermost => sites.kept += 1,
                FramePointerRule::NotKept => {
                    sites.not_kept += 1;
                    sites.first_not_kept.get_or_insert_with(|| instruction.ip());
                }
                FramePointerRule::NoUnwindInfo => {}
            }
        }
    }

    sites
}
