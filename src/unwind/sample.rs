//! Unwinds a stack the kernel sampled, from what a sample carries: the
//! thread's registers, a copy of the top of its stack, and the return
//! addresses the kernel found walking the frame-pointer chain.
//!
//! The copy is unwound by call-frame information first, which is exact
//! wherever it reaches, including leaf functions that keep no frame
//! pointer, which the kernel's walk skips or misreads. Where the copy ends,
//! the stack continues with the kernel's chain from the deepest frame
//! that provably keeps its frame pointer, and every frame after it must
//! provably keep its own for its caller to be believed. A stack that
//! cannot be followed further ends with the reason, never with a guess.

use foldhash::HashMap;

use super::{
    DwarfRegister, FramePointerRule, MemoryReader, RBP, RETURN_ADDRESS, RSP, RegisterFile,
    UnwindModule, interrupted_registers, unwind_module_for,
};

use crate::capture::records::Truncation;
use crate::{CodeRole, ModuleImageId, UnwindTermination, VirtualAddress};

/// One sampled thread's state.
#[derive(Debug, Clone, Copy)]
pub struct SampledState<'a> {
    /// The user registers by DWARF number.
    pub registers: &'a [(DwarfRegister, u64)],

    /// The stack's bytes from the stack pointer up.
    pub stack: &'a [u8],

    /// The kernel's frame-pointer walk: the instruction pointer, then
    /// return addresses, innermost first.
    pub chain: &'a [u64],
}

/// One frame of an unwound sample.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SampledFrame {
    /// The address whose code the frame runs: the instruction pointer of
    /// the innermost frame and of a frame a signal interrupted, and
    /// otherwise the byte before its return address, inside the call.
    pub address: VirtualAddress,

    /// The module covering the address, by index in the modules.
    pub module: Option<usize>,
    pub role: CodeRole,
}

/// How an unwound stack ends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StackEnd {
    /// At its outermost frame.
    Complete,

    /// Short of it, for this reason, with the module at fault if any.
    Truncated(Truncation, Option<usize>),
}

/// Remembers each instruction's [`FramePointerRule`], which every sample
/// through it asks again.
#[derive(Debug, Default)]
pub struct FrameRules {
    rules: HashMap<(ModuleImageId, u64), FramePointerRule>,
}

impl FrameRules {
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

/// The stack bytes a sample copied, at the stack pointer's address. A
/// sample without its stack pointer has a copy at no known address, from
/// which nothing can be read.
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

/// A frame call-frame information reached, with its own rbp.
struct Unwound {
    frame: SampledFrame,
    frame_pointer: Option<u64>,
}

/// Unwinds one sample through `modules`, innermost frame first, collecting
/// at most `limit` frames.
pub fn unwind_sample(
    modules: &[UnwindModule<'_>],
    rules: &mut FrameRules,
    sample: SampledState<'_>,
    limit: usize,
) -> (Vec<SampledFrame>, StackEnd) {
    let (frames, end) = sampled_frames(modules, rules, sample, limit);
    debug_assert!(
        frames.len() <= limit,
        "a sample unwound to {} frames, past its limit of {limit}",
        frames.len()
    );
    (frames, end)
}

fn sampled_frames(
    modules: &[UnwindModule<'_>],
    rules: &mut FrameRules,
    sample: SampledState<'_>,
    limit: usize,
) -> (Vec<SampledFrame>, StackEnd) {
    let registers = RegisterFile::new(sample.registers.iter().copied());
    let Some(instruction) = registers
        .get(RETURN_ADDRESS)
        .or_else(|| sample.chain.first().copied())
    else {
        return (Vec::new(), StackEnd::Truncated(Truncation::NoStack, None));
    };

    let mut memory = StackCopy {
        start: registers.get(RSP),
        bytes: sample.stack,
    };

    let walked = walk_cfi(modules, instruction, registers, &mut memory, limit);
    if walked.certain {
        return (frames(walked.unwound), walked.end);
    }

    let (mut unwound, cfi_end) = (walked.unwound, walked.end);

    // The kernel's chain from the deepest frame that keeps the chain.
    let Some((kept, link)) = splice(modules, rules, &unwound, sample, &mut memory) else {
        return (frames(unwound), cfi_end);
    };

    unwound.truncate(kept + 1);
    let mut frames = frames(unwound);
    let end = follow_chain(modules, rules, &mut frames, &sample.chain[link..], limit);
    (frames, end)
}

/// Unwinds a stack by call-frame information alone from `registers`,
/// reading it from `memory` as it goes: a parked task's, whose registers
/// its runtime saved, read while the program runs. A word `memory` cannot
/// read ends the stack as unreadable.
pub fn unwind_memory(
    modules: &[UnwindModule<'_>],
    registers: &[(DwarfRegister, u64)],
    memory: &mut dyn MemoryReader,
    limit: usize,
) -> (Vec<SampledFrame>, StackEnd) {
    let registers = RegisterFile::new(registers.iter().copied());
    let Some(instruction) = registers.get(RETURN_ADDRESS) else {
        return (Vec::new(), StackEnd::Truncated(Truncation::NoStack, None));
    };

    let walked = walk_cfi(modules, instruction, registers, memory, limit);
    let end = match walked.end {
        StackEnd::Truncated(Truncation::StackCopyExhausted, module) => {
            StackEnd::Truncated(Truncation::Unreadable, module)
        }
        end => end,
    };

    debug_assert!(
        walked.unwound.len() <= limit,
        "a stack unwound to {} frames, past its limit of {limit}",
        walked.unwound.len()
    );
    (frames(walked.unwound), end)
}

/// What call-frame information found of a stack: the frames it reached,
/// and how it ended, which is `certain` unless the kernel's chain may
/// still reach further.
struct Walked {
    unwound: Vec<Unwound>,
    end: StackEnd,
    certain: bool,
}

impl Walked {
    const fn certain(unwound: Vec<Unwound>, end: StackEnd) -> Self {
        Self {
            unwound,
            end,
            certain: true,
        }
    }
}

/// Unwinds by call-frame information from `instruction` with `registers`,
/// reading the stack from `memory`, collecting at most `limit` frames.
fn walk_cfi(
    modules: &[UnwindModule<'_>],
    instruction: u64,
    mut registers: RegisterFile,
    memory: &mut dyn MemoryReader,
    limit: usize,
) -> Walked {
    let mut unwound: Vec<Unwound> = Vec::new();
    let mut address = VirtualAddress::new(instruction);
    let mut previous_cfa = None;
    let end = loop {
        let found = unwind_module_for(modules, address);
        let module = found.map(|(module, _)| module);
        let index = module.and_then(|found| {
            modules
                .iter()
                .position(|module| module.loaded.id == found.loaded.id)
        });
        let role = found.map_or(CodeRole::Ordinary, |(module, image_address)| {
            module.image.code_role(image_address)
        });
        unwound.push(Unwound {
            frame: SampledFrame {
                address,
                module: index,
                role,
            },
            frame_pointer: registers.get(RBP),
        });

        if unwound.len() >= limit {
            return Walked::certain(unwound, StackEnd::Truncated(Truncation::DepthLimit, None));
        }

        if role == CodeRole::Outermost {
            return Walked::certain(unwound, StackEnd::Complete);
        }

        // A switch's frame lies on the stack it switched from, which its
        // stack pointer no longer names: only the kernel's chain, through
        // the frame pointer the switch kept, reaches its caller.
        if role == CodeRole::StackSwitch {
            break StackEnd::Truncated(Truncation::StackSwitch, index);
        }

        let Some((module, image_address)) = found else {
            break StackEnd::Truncated(Truncation::UnmappedAddress, None);
        };

        let step = match module.unwind.unwind(image_address, &registers, memory) {
            Ok(step) => step,
            Err(UnwindTermination::Complete) => {
                return Walked::certain(unwound, StackEnd::Complete);
            }
            Err(UnwindTermination::NoUnwindInfo { .. }) => {
                break StackEnd::Truncated(Truncation::NoUnwindInfo, index);
            }
            Err(UnwindTermination::MemoryReadFailed { .. }) => {
                break StackEnd::Truncated(Truncation::StackCopyExhausted, None);
            }
            Err(_) => break StackEnd::Truncated(Truncation::InvalidCaller, index),
        };

        let caller = match step.registers.get(RETURN_ADDRESS) {
            None | Some(0) => return Walked::certain(unwound, StackEnd::Complete),
            Some(caller) => caller,
        };

        // A caller in the same frame as its callee would repeat forever.
        if previous_cfa == Some(step.cfa) {
            break StackEnd::Truncated(Truncation::InvalidCaller, index);
        }

        previous_cfa = Some(step.cfa);

        // A signal handler returns to the trampoline that restores what it
        // interrupted: its frame, at its first instruction, and then the
        // interrupted instruction, with the registers the kernel saved.
        if let Some(trampoline) = signal_trampoline(modules, caller, &step.registers) {
            unwound.push(trampoline);
            if unwound.len() >= limit {
                return Walked::certain(unwound, StackEnd::Truncated(Truncation::DepthLimit, None));
            }

            let interrupted = step
                .registers
                .get(RSP)
                .and_then(|context| interrupted_registers(context, memory));
            let Some((interrupted, instruction)) = interrupted.and_then(|interrupted| {
                let instruction = interrupted.get(RETURN_ADDRESS)?;
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
        unwound,
        end,
        certain: false,
    }
}

/// The frame of the signal trampoline at `caller`, if a trampoline is
/// there, with the registers of the handler it returns from.
fn signal_trampoline(
    modules: &[UnwindModule<'_>],
    caller: u64,
    registers: &RegisterFile,
) -> Option<Unwound> {
    let (module, image_address) = unwind_module_for(modules, VirtualAddress::new(caller))?;
    (module.image.code_role(image_address) == CodeRole::SignalTrampoline).then(|| Unwound {
        frame: SampledFrame {
            address: VirtualAddress::new(caller),
            module: modules
                .iter()
                .position(|candidate| candidate.loaded.id == module.loaded.id),
            role: CodeRole::SignalTrampoline,
        },
        frame_pointer: registers.get(RBP),
    })
}

fn frames(unwound: Vec<Unwound>) -> Vec<SampledFrame> {
    unwound.into_iter().map(|unwound| unwound.frame).collect()
}

/// The deepest frame call-frame information reached whose rbp is a link
/// of the kernel's walk, with the index of the chain's entry for its
/// caller, if the frame provably keeps the chain there.
///
/// The walk over the copy repeats the kernel's: from rbp, each link holds
/// the next link, and the word above it a return address, which must be
/// the chain's.
fn splice(
    modules: &[UnwindModule<'_>],
    rules: &mut FrameRules,
    unwound: &[Unwound],
    sample: SampledState<'_>,
    memory: &mut StackCopy<'_>,
) -> Option<(usize, usize)> {
    let mut links = Vec::new();
    let mut link = sample
        .registers
        .iter()
        .find_map(|(register, value)| (*register == RBP).then_some(*value));

    while let Some(current) = link {
        links.push(current);
        if links.len() >= sample.chain.len() {
            break;
        }

        match memory.read_u64(VirtualAddress::new(current.wrapping_add(8))) {
            // A link whose return address is in the copy must agree with
            // the kernel's, or the two walks part.
            Some(return_address) if return_address != sample.chain[links.len()] => break,
            _ => {}
        }

        link = memory.read_u64(VirtualAddress::new(current));
    }

    for (index, frame) in unwound.iter().enumerate().rev() {
        let Some(frame_pointer) = frame.frame_pointer else {
            continue;
        };

        let Some(position) = links.iter().position(|link| *link == frame_pointer) else {
            continue;
        };

        let caller = position + 1;
        if caller >= sample.chain.len() {
            continue;
        }

        let Some(module) = frame.frame.module.map(|module| &modules[module]) else {
            continue;
        };

        if rules.rule(module, frame.frame.address) != FramePointerRule::Kept {
            continue;
        }

        // Where call-frame information found the caller too, both must
        // name the same one.
        if let Some(next) = unwound.get(index + 1)
            && next.frame.address.get().wrapping_add(1) != sample.chain[caller]
        {
            continue;
        }

        return Some((index, caller));
    }

    None
}

/// Appends the callers `chain` lists, beginning with the caller of the
/// last of `frames`, while each frame provably keeps the chain.
fn follow_chain(
    modules: &[UnwindModule<'_>],
    rules: &mut FrameRules,
    frames: &mut Vec<SampledFrame>,
    chain: &[u64],
    limit: usize,
) -> StackEnd {
    for (position, &return_address) in chain.iter().enumerate() {
        let address = VirtualAddress::new(return_address.wrapping_sub(1));
        let Some((module, image_address)) = unwind_module_for(modules, address) else {
            return StackEnd::Truncated(Truncation::UnmappedAddress, None);
        };

        let index = modules
            .iter()
            .position(|candidate| candidate.loaded.id == module.loaded.id);
        let role = module.image.code_role(image_address);
        frames.push(SampledFrame {
            address,
            module: index,
            role,
        });
        if role == CodeRole::Outermost {
            return StackEnd::Complete;
        }

        if frames.len() >= limit {
            return StackEnd::Truncated(Truncation::DepthLimit, None);
        }

        let kept = rules.rule(&module, address);

        // A switch the chain does not carry on through ends the stack at
        // the switch, whatever its frame keeps.
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
mod tests {
    use std::path::Path;

    use iced_x86::{Decoder, DecoderOptions, Mnemonic, Register};
    use object::{Object as _, ObjectSection as _, ObjectSymbol as _};

    use super::*;
    use crate::{LoadedModule, ModuleId, ModuleImageId};

    /// `work` in `stores-clang-O2`, which keeps its frame pointer.
    struct Work {
        info: crate::debug_info::DebugInfo,

        /// Just past its prologue, where its frame is found by rbp.
        body: u64,

        /// In its epilogue, after it pops rbx and before r14, where its
        /// rules still find rbx below the stack pointer.
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

        /// Unwinds a sample at `rip` with `registers` and the stack
        /// `words`, whose kernel chain is this frame and its caller.
        fn unwind(
            &self,
            rip: u64,
            registers: &[(DwarfRegister, u64)],
            words: &[u64],
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
                image: &self.info.image,
                unwind: self.info.unwind.as_ref(),
            }];

            let (frames, end) = unwind_sample(
                &modules,
                &mut FrameRules::default(),
                SampledState {
                    registers,
                    stack: &stack,
                    chain: &[rip, self.returns_to],
                },
                16,
            );

            let addresses = frames.iter().map(|frame| frame.address.get()).collect();
            (addresses, end)
        }
    }

    /// A sample in an epilogue, after a register was popped whose rules
    /// still name its slot, now below the stack pointer and outside the
    /// copy: the caller is found all the same, and the stack is whole.
    #[test]
    fn a_register_saved_below_the_copy_still_leaves_its_caller_found() {
        let work = Work::new();

        // The copy holds r14, r15, the caller's rbp, and the return address.
        let sp = 0x7000_0000_u64;
        let rip = work.epilogue;
        assert_eq!(
            work.unwind(
                rip,
                &[(RETURN_ADDRESS, rip), (RSP, sp), (RBP, sp + 16)],
                &[7, 8, 0, work.returns_to],
            ),
            (vec![rip, work.returns_to - 1], StackEnd::Complete)
        );
    }

    /// A sample without its stack pointer has a copy at no known address:
    /// read as if it began at zero, its words would name a false caller.
    /// The kernel's chain names the true one.
    #[test]
    fn a_copy_without_its_stack_pointer_names_no_caller() {
        let work = Work::new();
        let rip = work.body;
        let (addresses, _) = work.unwind(
            rip,
            &[(RETURN_ADDRESS, rip), (RBP, 0x10)],
            &[0, 0, 0, work.returns_to + 0x20],
        );
        assert_eq!(addresses, [rip, work.returns_to - 1]);
    }
}
