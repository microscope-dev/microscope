use foldhash::HashMap;

use super::{
    DwarfRegister, FRAME_POINTER, FramePointerRule, MemoryReader, Modules, PROGRAM_COUNTER,
    RegisterFile, STACK_POINTER, UnwindModule, interrupted_registers,
};

use crate::capture::records::Truncation;
use crate::{CodeRole, ImageAddress, ModuleImageId, UnwindTermination, VirtualAddress};

#[derive(Debug, Clone, Copy)]
pub struct SampledState<'a> {
    pub registers: &'a [(DwarfRegister, u64)],

    pub stack: &'a [u8],

    pub chain: &'a [u64],
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SampledFrame {
    pub address: VirtualAddress,

    pub module: Option<usize>,
    pub role: CodeRole,

    pub frame_pointer: Option<u64>,

    pub cfa: Option<u64>,
}

pub const CHAIN_LIMIT: u16 = 127;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StackEnd {
    Complete,

    Truncated(Truncation, Option<usize>),
}

#[derive(Debug, Default)]
pub struct FrameRules {
    rules: HashMap<(ModuleImageId, u64), FramePointerRule>,
    roles: HashMap<(ModuleImageId, u64), CodeRole>,

    links: Vec<u64>,
    past_splice: Vec<SampledFrame>,
}

impl FrameRules {
    fn role(&mut self, module: &UnwindModule<'_>, image_address: ImageAddress) -> CodeRole {
        *self
            .roles
            .entry((module.loaded.image, image_address.get()))
            .or_insert_with(|| module.image.code_role(image_address))
    }

    fn rule(&mut self, module: &UnwindModule<'_>, address: VirtualAddress) -> FramePointerRule {
        let Ok(image_address) = module.loaded.image_address(address) else {
            return FramePointerRule::NoUnwindInfo;
        };
        *self
            .rules
            .entry((module.loaded.image, image_address.get()))
            .or_insert_with(|| module.unwind.frame_pointer_rule(image_address))
    }
}

struct StackCopy<'a> {
    start: Option<u64>,
    bytes: &'a [u8],
}

impl MemoryReader for StackCopy<'_> {
    fn read_u64(&mut self, address: VirtualAddress) -> Option<u64> {
        let offset = usize::try_from(address.get().checked_sub(self.start?)?).ok()?;
        let word = self.bytes.get(offset..offset.checked_add(8)?)?;
        Some(u64::from_le_bytes(word.try_into().ok()?))
    }
}

pub fn unwind_sample<M: Modules + ?Sized>(
    modules: &M,
    rules: &mut FrameRules,
    sample: SampledState<'_>,
    limit: usize,
) -> (Vec<SampledFrame>, StackEnd) {
    let mut frames = Vec::new();
    let end = unwind_sample_into(modules, rules, sample, limit, &mut frames);
    (frames, end)
}

pub fn unwind_sample_into<M: Modules + ?Sized>(
    modules: &M,
    rules: &mut FrameRules,
    sample: SampledState<'_>,
    limit: usize,
    frames: &mut Vec<SampledFrame>,
) -> StackEnd {
    frames.clear();
    let end = sampled_frames(modules, rules, sample, limit, frames);
    debug_assert!(
        frames.len() <= limit,
        "a sample unwound to {} frames, past its limit of {limit}",
        frames.len()
    );
    end
}

fn sampled_frames<M: Modules + ?Sized>(
    modules: &M,
    rules: &mut FrameRules,
    sample: SampledState<'_>,
    limit: usize,
    frames: &mut Vec<SampledFrame>,
) -> StackEnd {
    let registers = RegisterFile::new(sample.registers.iter().copied());
    let Some(instruction) = registers
        .get(PROGRAM_COUNTER)
        .or_else(|| sample.chain.first().copied())
    else {
        return StackEnd::Truncated(Truncation::NoStack, None);
    };

    let mut memory = StackCopy {
        start: registers.get(STACK_POINTER),
        bytes: sample.stack,
    };

    let walked = walk_cfi(
        modules,
        rules,
        instruction,
        registers,
        &mut memory,
        limit,
        frames,
    );
    if walked.certain {
        return walked.end;
    }

    let Some((kept, link)) = splice(modules, rules, frames, sample, &mut memory) else {
        return walked.end;
    };

    let mut past = std::mem::take(&mut rules.past_splice);
    past.clear();
    past.extend(frames.drain(kept + 1..));
    let links = std::mem::take(&mut rules.links);
    let end = match follow_chain(
        modules,
        rules,
        frames,
        (
            sample.chain.get(link..).unwrap_or_default(),
            links.get(link..).unwrap_or_default(),
        ),
        limit,
    ) {
        StackEnd::Truncated(Truncation::ChainEnded, module)
            if sample.chain.len() >= usize::from(CHAIN_LIMIT) =>
        {
            StackEnd::Truncated(Truncation::ChainLimit, module)
        }
        end => end,
    };
    rules.links = links;

    let end = if frames.len() < kept + 1 + past.len() {
        frames.truncate(kept + 1);
        frames.extend_from_slice(&past);
        walked.end
    } else {
        end
    };
    rules.past_splice = past;
    end
}

pub fn unwind_memory<M: Modules + ?Sized>(
    modules: &M,
    registers: &[(DwarfRegister, u64)],
    memory: &mut dyn MemoryReader,
    limit: usize,
) -> (Vec<SampledFrame>, StackEnd) {
    let registers = RegisterFile::new(registers.iter().copied());
    let Some(instruction) = registers.get(PROGRAM_COUNTER) else {
        return (Vec::new(), StackEnd::Truncated(Truncation::NoStack, None));
    };

    let mut rules = FrameRules::default();
    let mut frames = Vec::new();
    let walked = walk_cfi(
        modules,
        &mut rules,
        instruction,
        registers,
        memory,
        limit,
        &mut frames,
    );
    let end = match walked.end {
        StackEnd::Truncated(Truncation::StackCopyExhausted, module) => {
            StackEnd::Truncated(Truncation::Unreadable, module)
        }
        end => end,
    };

    debug_assert!(
        frames.len() <= limit,
        "a stack unwound to {} frames, past its limit of {limit}",
        frames.len()
    );
    (frames, end)
}

struct Walked {
    end: StackEnd,
    certain: bool,
}

impl Walked {
    const fn certain(end: StackEnd) -> Self {
        Self { end, certain: true }
    }
}

fn walk_cfi<M: Modules + ?Sized>(
    modules: &M,
    rules: &mut FrameRules,
    instruction: u64,
    mut registers: RegisterFile,
    memory: &mut dyn MemoryReader,
    limit: usize,
    frames: &mut Vec<SampledFrame>,
) -> Walked {
    debug_assert!(frames.is_empty(), "a walk begins with no frames");
    let mut address = VirtualAddress::new(instruction);
    let mut previous_cfa = None;
    let end = loop {
        let found = modules.module_at(address);
        let index = found.map(|(index, _)| index);
        let role = found.map_or(CodeRole::Ordinary, |(index, image_address)| {
            rules.role(&modules.module(index), image_address)
        });
        frames.push(SampledFrame {
            address,
            module: index,
            role,
            frame_pointer: registers.get(FRAME_POINTER),
            cfa: None,
        });

        if frames.len() >= limit {
            return Walked::certain(StackEnd::Truncated(Truncation::DepthLimit, None));
        }

        if role == CodeRole::Outermost {
            return Walked::certain(StackEnd::Complete);
        }

        if role == CodeRole::StackSwitch {
            break StackEnd::Truncated(Truncation::StackSwitch, index);
        }

        if role == CodeRole::SignalTrampoline {
            let Some((interrupted, instruction)) = registers
                .get(STACK_POINTER)
                .and_then(|context| interrupted_registers(context, memory).ok())
                .and_then(|interrupted| {
                    let instruction = interrupted.get(PROGRAM_COUNTER)?;
                    Some((interrupted, instruction))
                })
            else {
                break StackEnd::Truncated(Truncation::StackCopyExhausted, None);
            };

            address = VirtualAddress::new(instruction);
            registers = interrupted;
            continue;
        }

        let Some((module, image_address)) = found.map(|(index, at)| (modules.module(index), at))
        else {
            break StackEnd::Truncated(Truncation::UnmappedAddress, None);
        };

        let step = match module.unwind.unwind(image_address, &registers, memory) {
            Ok(step) => step,
            Err(UnwindTermination::Complete) => {
                return Walked::certain(StackEnd::Complete);
            }
            Err(UnwindTermination::NoUnwindInfo { .. }) => {
                break StackEnd::Truncated(Truncation::NoUnwindInfo, index);
            }
            Err(UnwindTermination::MemoryReadFailed { .. }) => {
                break StackEnd::Truncated(Truncation::StackCopyExhausted, None);
            }
            Err(_) => break StackEnd::Truncated(Truncation::InvalidCaller, index),
        };

        if let Some(last) = frames.last_mut() {
            last.cfa = Some(step.cfa.get());
        }

        let caller = match step.registers.get(PROGRAM_COUNTER) {
            None => return Walked::certain(StackEnd::Complete),
            Some(0) => return zero_return_address(&registers, step.cfa, index),
            Some(caller) => caller,
        };

        if previous_cfa == Some(step.cfa) {
            break StackEnd::Truncated(Truncation::InvalidCaller, index);
        }

        previous_cfa = Some(step.cfa);

        if let Some(trampoline) = signal_trampoline(modules, rules, caller, &step.registers) {
            frames.push(trampoline);
            if frames.len() >= limit {
                return Walked::certain(StackEnd::Truncated(Truncation::DepthLimit, None));
            }

            let interrupted = step
                .registers
                .get(STACK_POINTER)
                .and_then(|context| interrupted_registers(context, memory).ok());
            let Some((interrupted, instruction)) = interrupted.and_then(|interrupted| {
                let instruction = interrupted.get(PROGRAM_COUNTER)?;
                Some((interrupted, instruction))
            }) else {
                break StackEnd::Truncated(Truncation::StackCopyExhausted, None);
            };

            address = VirtualAddress::new(instruction);
            registers = interrupted;
            continue;
        }

        address = if step.signal_frame {
            VirtualAddress::new(caller)
        } else {
            VirtualAddress::new(caller.wrapping_sub(1))
        };

        registers = step.registers;
    };

    Walked {
        end,
        certain: false,
    }
}

fn zero_return_address(
    registers: &RegisterFile,
    cfa: VirtualAddress,
    index: Option<usize>,
) -> Walked {
    if registers.get(STACK_POINTER).is_none_or(|sp| cfa.get() > sp) {
        return Walked::certain(StackEnd::Complete);
    }

    Walked {
        end: StackEnd::Truncated(Truncation::InvalidCaller, index),
        certain: false,
    }
}

fn signal_trampoline<M: Modules + ?Sized>(
    modules: &M,
    rules: &mut FrameRules,
    caller: u64,
    registers: &RegisterFile,
) -> Option<SampledFrame> {
    let (index, image_address) = modules.module_at(VirtualAddress::new(caller))?;
    (rules.role(&modules.module(index), image_address) == CodeRole::SignalTrampoline).then(|| {
        SampledFrame {
            address: VirtualAddress::new(caller),
            module: Some(index),
            role: CodeRole::SignalTrampoline,
            frame_pointer: registers.get(FRAME_POINTER),
            cfa: None,
        }
    })
}

fn splice<M: Modules + ?Sized>(
    modules: &M,
    rules: &mut FrameRules,
    unwound: &[SampledFrame],
    sample: SampledState<'_>,
    memory: &mut StackCopy<'_>,
) -> Option<(usize, usize)> {
    let mut links = std::mem::take(&mut rules.links);
    links.clear();
    let mut link = sample
        .registers
        .iter()
        .find_map(|(register, value)| (*register == FRAME_POINTER).then_some(*value));

    while let Some(current) = link {
        links.push(current);
        if links.len() >= sample.chain.len() {
            break;
        }

        match memory.read_u64(VirtualAddress::new(current.wrapping_add(8))) {
            Some(return_address) if sample.chain.get(links.len()) != Some(&return_address) => break,
            _ => {}
        }

        link = memory.read_u64(VirtualAddress::new(current));
    }

    let found = unwound.iter().enumerate().rev().find_map(|(index, frame)| {
        let position = links
            .iter()
            .position(|link| Some(*link) == frame.frame_pointer)?;
        let caller = position + 1;
        let chained = *sample.chain.get(caller)?;

        let module = modules.module(frame.module?);
        if rules.rule(&module, frame.address) != FramePointerRule::Kept {
            return None;
        }

        if let Some(next) = unwound.get(index + 1)
            && next.address.get().wrapping_add(1) != chained
        {
            return None;
        }

        Some((index, caller))
    });
    rules.links = links;
    found
}

fn follow_chain<M: Modules + ?Sized>(
    modules: &M,
    rules: &mut FrameRules,
    frames: &mut Vec<SampledFrame>,
    (chain, links): (&[u64], &[u64]),
    limit: usize,
) -> StackEnd {
    for (position, &return_address) in chain.iter().enumerate() {
        let entry = VirtualAddress::new(return_address);
        let address = match modules.module_at(entry) {
            Some((index, image_address))
                if rules.role(&modules.module(index), image_address)
                    == CodeRole::SignalTrampoline =>
            {
                entry
            }
            _ => VirtualAddress::new(return_address.wrapping_sub(1)),
        };
        let Some((index, image_address)) = modules.module_at(address) else {
            return StackEnd::Truncated(Truncation::UnmappedAddress, None);
        };

        let module = modules.module(index);
        let index = Some(index);
        let role = rules.role(&module, image_address);
        let kept = rules.rule(&module, address);
        let frame_pointer = links.get(position).copied();
        frames.push(SampledFrame {
            address,
            module: index,
            role,
            frame_pointer,
            cfa: frame_pointer
                .filter(|_| kept == FramePointerRule::Kept)
                .map(|link| link.wrapping_add(16)),
        });
        if role == CodeRole::Outermost {
            return StackEnd::Complete;
        }

        if frames.len() >= limit {
            return StackEnd::Truncated(Truncation::DepthLimit, None);
        }

        if role == CodeRole::StackSwitch
            && !(kept == FramePointerRule::Kept && position + 1 < chain.len())
        {
            return StackEnd::Truncated(Truncation::StackSwitch, index);
        }

        match kept {
            FramePointerRule::Kept if position + 1 < chain.len() => {}
            FramePointerRule::Kept => return StackEnd::Truncated(Truncation::ChainEnded, index),
            FramePointerRule::Outermost => return StackEnd::Complete,
            FramePointerRule::NotKept => {
                return StackEnd::Truncated(Truncation::FramePointerNotKept, index);
            }
            FramePointerRule::NoUnwindInfo => {
                return StackEnd::Truncated(Truncation::NoUnwindInfo, index);
            }
        }
    }

    StackEnd::Truncated(Truncation::ChainEnded, None)
}

#[cfg(test)]
mod model;

#[cfg(test)]
mod tests {
    use std::path::Path;

    use iced_x86::{Decoder, DecoderOptions, Mnemonic, Register};
    use object::{Object as _, ObjectSection as _, ObjectSymbol as _};

    use super::*;
    use crate::{LoadedModule, ModuleId, ModuleImageId};

    struct Work {
        info: crate::debug_info::DebugInfo,

        body: u64,

        epilogue: u64,
        returns_to: u64,
    }

    impl Work {
        fn new() -> Self {
            let path =
                Path::new(env!("CARGO_MANIFEST_DIR")).join("build/golden/stores/stores-clang-O2");
            let data = std::fs::read(&path).expect("build the golden programs");
            let info =
                crate::debug_info::load_bytes(&path, &data).expect("load its debug information");
            let file = object::File::parse(&*data).expect("parse it");
            let text = file.section_by_name(".text").expect("a text section");
            let instructions = Decoder::with_ip(
                64,
                text.data().expect("text bytes"),
                text.address(),
                DecoderOptions::NONE,
            )
            .iter()
            .collect::<Vec<_>>();

            let symbol = |name: &str| {
                file.symbols()
                    .find(|symbol| symbol.name() == Ok(name))
                    .expect("the symbol")
                    .address()
            };

            let after = |start: u64, found: &dyn Fn(&iced_x86::Instruction) -> bool| {
                instructions
                    .iter()
                    .skip_while(|instruction| instruction.ip() < start)
                    .find(|instruction| found(instruction))
                    .expect("the instruction")
                    .ip()
            };

            let pop = |register| {
                move |instruction: &iced_x86::Instruction| {
                    instruction.mnemonic() == Mnemonic::Pop
                        && instruction.op0_register() == register
                }
            };

            let body = after(symbol("work"), &|instruction| {
                instruction.mnemonic() == Mnemonic::Mov
                    && instruction.op0_register() == Register::RBP
                    && instruction.op1_register() == Register::RSP
            });

            let body = instructions
                .iter()
                .find(|instruction| instruction.ip() == body)
                .expect("the frame's setup")
                .next_ip();
            let epilogue = after(
                after(symbol("work"), &pop(Register::RBX)),
                &pop(Register::R14),
            );
            let call = after(symbol("rt_thread_start"), &|instruction| {
                instruction.mnemonic() == Mnemonic::Call
            });
            let returns_to = instructions
                .iter()
                .find(|instruction| instruction.ip() == call)
                .expect("the call")
                .next_ip();

            Self {
                info,
                body,
                epilogue,
                returns_to,
            }
        }

        fn unwind(
            &self,
            rip: u64,
            registers: &[(DwarfRegister, u64)],
            words: &[u64],
        ) -> (Vec<u64>, StackEnd) {
            unwind_in(&self.info, registers, words, &[rip, self.returns_to])
        }
    }

    fn unwind_in(
        info: &crate::debug_info::DebugInfo,
        registers: &[(DwarfRegister, u64)],
        words: &[u64],
        chain: &[u64],
    ) -> (Vec<u64>, StackEnd) {
        let stack = words
            .iter()
            .flat_map(|word| word.to_le_bytes())
            .collect::<Vec<_>>();

        let modules = [UnwindModule {
            loaded: LoadedModule {
                id: ModuleId::new(1),
                image: ModuleImageId::new(1),
                load_bias: crate::LoadBias::default(),
                window: None,
            },
            image: &info.image,
            unwind: info.unwind.as_ref(),
        }];

        let (frames, end) = unwind_sample(
            &modules[..],
            &mut FrameRules::default(),
            SampledState {
                registers,
                stack: &stack,
                chain,
            },
            16,
        );

        let addresses = frames.iter().map(|frame| frame.address.get()).collect();
        (addresses, end)
    }

    struct Clone {
        info: crate::debug_info::DebugInfo,

        tail: u64,

        returns_to: u64,
    }

    impl Clone {
        fn new(program: &str, function: &str) -> Self {
            let path = Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("build/test-programs")
                .join(program);
            let data = std::fs::read(&path).expect("build the test programs");
            let info =
                crate::debug_info::load_bytes(&path, &data).expect("load its debug information");
            let file = object::File::parse(&*data).expect("parse it");
            let text = file.section_by_name(".text").expect("a text section");
            let instructions = Decoder::with_ip(
                64,
                text.data().expect("text bytes"),
                text.address(),
                DecoderOptions::NONE,
            )
            .iter()
            .collect::<Vec<_>>();
            let start = file
                .symbols()
                .find(|symbol| symbol.name() == Ok(function))
                .expect("the clone function")
                .address();

            let tail = instructions
                .iter()
                .find(|instruction| {
                    instruction.ip() >= start && instruction.mnemonic() == Mnemonic::Syscall
                })
                .expect("its system call")
                .next_ip();
            let returns_to = instructions
                .iter()
                .find(|instruction| {
                    instruction.mnemonic() == Mnemonic::Call
                        && instruction.near_branch_target() == start
                })
                .expect("a call to it")
                .next_ip();

            Self {
                info,
                tail,
                returns_to,
            }
        }

        fn unwind(&self, rax: Option<u64>, top: u64) -> (Vec<u64>, StackEnd) {
            let mut registers = vec![
                (PROGRAM_COUNTER, self.tail),
                (STACK_POINTER, 0x7000_0000),
                (FRAME_POINTER, 0x7100_0000),
            ];
            registers.extend(rax.map(|rax| (crate::unwind::x86_64::RAX, rax)));
            unwind_in(&self.info, &registers, &[top], &[self.tail])
        }
    }

    fn both_threads_of_a_sampled_clone_unwind_as_the_debuggers(clone: &Clone) {
        let arguments = 0x7000_0040;
        assert_eq!(
            clone.unwind(Some(0), arguments),
            (vec![clone.tail], StackEnd::Complete)
        );

        for rax in [1234, (-38_i64).cast_unsigned()] {
            let (frames, end) = clone.unwind(Some(rax), clone.returns_to);
            assert_eq!(frames, [clone.tail, clone.returns_to - 1], "rax {rax:#x}");
            assert_eq!(
                end,
                StackEnd::Truncated(Truncation::StackCopyExhausted, None)
            );
        }

        assert_eq!(
            clone.unwind(None, clone.returns_to),
            (
                vec![clone.tail],
                StackEnd::Truncated(Truncation::InvalidCaller, Some(0))
            )
        );
    }

    #[test]
    fn both_threads_of_a_sampled_glibc_clone_unwind_as_the_debuggers() {
        both_threads_of_a_sampled_clone_unwind_as_the_debuggers(&Clone::new(
            "tls-modules-gcc-static",
            "__clone3",
        ));
    }

    #[test]
    fn both_threads_of_a_sampled_musl_clone_unwind_as_the_debuggers() {
        both_threads_of_a_sampled_clone_unwind_as_the_debuggers(&Clone::new(
            "threads-musl-static",
            "__clone",
        ));
    }

    #[test]
    fn a_register_saved_below_the_copy_still_leaves_its_caller_found() {
        let work = Work::new();

        let sp = 0x7000_0000_u64;
        let rip = work.epilogue;
        assert_eq!(
            work.unwind(
                rip,
                &[
                    (PROGRAM_COUNTER, rip),
                    (STACK_POINTER, sp),
                    (FRAME_POINTER, sp + 16)
                ],
                &[7, 8, 0, work.returns_to],
            ),
            (vec![rip, work.returns_to - 1], StackEnd::Complete)
        );
    }

    #[test]
    fn a_copy_without_its_stack_pointer_names_no_caller() {
        let work = Work::new();
        let rip = work.body;
        let (addresses, _) = work.unwind(
            rip,
            &[(PROGRAM_COUNTER, rip), (FRAME_POINTER, 0x10)],
            &[0, 0, 0, work.returns_to + 0x20],
        );
        assert_eq!(addresses, [rip, work.returns_to - 1]);
    }
}
