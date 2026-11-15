use iced_x86::{Decoder, DecoderOptions, FlowControl, Mnemonic};
use object::{Object, ObjectSection, SectionFlags};

use crate::debug_info::UnwindInfo;
use crate::unwind::FramePointerRule;
use crate::{CodeRole, ImageAddress, ModuleImage};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CallSites {
    pub kept: u32,

    pub not_kept: u32,

    pub first_not_kept: Option<u64>,
}

impl CallSites {
    pub const fn keep_the_chain(self) -> bool {
        self.not_kept as u64 * 1000 <= self.kept as u64 + self.not_kept as u64
    }
}

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

#[cfg(test)]
mod tests {
    use iced_x86::{Code, Decoder, DecoderOptions, Register};

    #[inline(never)]
    fn leaf(value: u64) -> u64 {
        std::hint::black_box(value).wrapping_add(1)
    }

    #[test]
    fn microscope_keeps_the_frame_pointer_chain() {
        use std::io::{Read, Seek, SeekFrom};

        assert_eq!(leaf(1), 2);
        let address = leaf as fn(u64) -> u64 as usize as u64;
        let mut code = [0; 16];
        let mut memory = std::fs::File::open("/proc/self/mem").expect("open /proc/self/mem");
        memory
            .seek(SeekFrom::Start(address))
            .expect("seek to the code");
        memory.read_exact(&mut code).expect("read the code");

        let mut decoder = Decoder::with_ip(64, &code, address, DecoderOptions::NONE);
        let push = decoder.decode();
        let link = decoder.decode();
        assert!(
            push.code() == Code::Push_r64 && push.op0_register() == Register::RBP,
            "{push}"
        );
        assert!(
            link.code() == Code::Mov_rm64_r64
                && link.op0_register() == Register::RBP
                && link.op1_register() == Register::RSP,
            "{link}"
        );
    }
}
