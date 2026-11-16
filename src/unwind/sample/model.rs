use std::collections::BTreeMap;
use std::path::PathBuf;

use proptest::prelude::*;

use super::{CHAIN_LIMIT, FrameRules, SampledFrame, SampledState, StackEnd, unwind_sample};
use crate::capture::records::Truncation;
use crate::debug_info::UnwindInfo;
use crate::unwind::x86_64::{SIGCONTEXT_REGISTERS, UCONTEXT_MCONTEXT};
use crate::unwind::{
    DwarfRegister, FRAME_POINTER, MemoryReader, PROGRAM_COUNTER, RegisterFile, STACK_POINTER,
    UnwindModule, UnwindStep,
};
use crate::{
    AddressRange, CodeRole, ImageAddress, LoadedModule, ModuleId, ModuleImage, ModuleImageId,
    SymbolBinding, SymbolExtent, SymbolExtentProvenance, SymbolId, SymbolInfo, SymbolKind,
    UnwindTermination, VirtualAddress,
};

const CODE: u64 = 0x1000;
const FUNCTION: u64 = 0x100;

const SIGNAL_FRAME: u64 = UCONTEXT_MCONTEXT + 8 * 17 + 128;

const STACK: u64 = 0x7000_0000;
const OTHER_STACK: u64 = 0x5000_0000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Rule {
    FramePointer,

    StackPointer { locals: u64, saves_rbp: bool },

    Outermost,

    Signal,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Place {
    Entry,
    Pushed,
    Body,
}

#[derive(Debug, Clone)]
struct Function {
    rule: Rule,
    role: CodeRole,
}

#[derive(Debug, Clone)]
struct Frame {
    function: usize,

    offset: u64,

    locals: u64,

    garbage: u64,
}

#[derive(Debug, Clone)]
struct Stack {
    functions: Vec<Function>,
    frames: Vec<Frame>,
    place: Place,
    copied_words: usize,
    limit: usize,
}

#[derive(Debug)]
struct Truth {
    frames: Vec<(u64, u64, u64)>,
    memory: BTreeMap<u64, u64>,
    stack_pointer: u64,
    stack_end: u64,
}

impl Stack {
    fn base(function: usize) -> u64 {
        CODE + FUNCTION * function as u64
    }

    fn start(&self, function: usize) -> u64 {
        Self::base(function) + u64::from(self.functions[function].rule == Rule::Signal)
    }

    fn address(&self, frame: usize) -> u64 {
        let Frame {
            function, offset, ..
        } = self.frames[frame];
        if self.functions[function].rule == Rule::Signal {
            return self.start(function);
        }
        Self::base(function)
            + if frame == 0 {
                match self.place {
                    Place::Entry => 0,
                    Place::Pushed => 1,
                    Place::Body => offset,
                }
            } else {
                offset
            }
    }

    fn rule(&self, frame: usize) -> Rule {
        self.functions[self.frames[frame].function].rule
    }

    fn role(&self, frame: usize) -> CodeRole {
        self.functions[self.frames[frame].function].role
    }

    #[expect(clippy::too_many_lines, reason = "one pass per part of the layout")]
    fn truth(&self) -> Truth {
        let count = self.frames.len();
        let mut stack_pointers = Vec::with_capacity(count);
        let mut cfas = Vec::with_capacity(count);
        let mut frame_pointers = vec![None; count];
        let mut stack_pointer = STACK;
        let mut stack_end = STACK;

        for (index, frame) in self.frames.iter().enumerate() {
            stack_pointers.push(stack_pointer);
            let place = if index == 0 { self.place } else { Place::Body };
            let cfa = match (self.rule(index), place) {
                _ if self.role(index) == CodeRole::StackSwitch => {
                    frame_pointers[index] = Some(OTHER_STACK);
                    OTHER_STACK + 16
                }
                (Rule::FramePointer, Place::Entry) | (Rule::Outermost, _) => stack_pointer + 8,
                (Rule::FramePointer, Place::Pushed) => stack_pointer + 16,
                (Rule::FramePointer, Place::Body) => {
                    let frame_pointer = stack_pointer + 8 * frame.locals;
                    frame_pointers[index] = Some(frame_pointer);
                    frame_pointer + 16
                }
                (Rule::StackPointer { locals, saves_rbp }, _) => {
                    stack_pointer + 8 * (locals + 1 + u64::from(saves_rbp))
                }
                (Rule::Signal, _) => stack_pointer + SIGNAL_FRAME,
            };
            cfas.push(cfa);
            if !(OTHER_STACK..STACK).contains(&stack_pointer) {
                stack_end = stack_end.max(cfa);
            }
            stack_pointer = cfa;
        }

        let mut caller = 0;
        for index in (0..count).rev() {
            let own = match (self.rule(index), index == 0) {
                _ if frame_pointers[index].is_some() => frame_pointers[index],
                (
                    Rule::StackPointer {
                        saves_rbp: true, ..
                    },
                    _,
                ) => Some(self.frames[index].garbage),
                _ => None,
            };
            frame_pointers[index] = Some(own.unwrap_or(caller));
            caller = frame_pointers[index].unwrap_or_default();
        }
        let frame_pointers = frame_pointers.into_iter().flatten().collect::<Vec<_>>();

        let mut memory = BTreeMap::new();
        for index in 0..count {
            let mut at = stack_pointers[index];
            while at + 8 <= cfas[index] {
                memory.insert(at, 0xdead_0000 + at);
                at += 8;
            }

            let caller_frame_pointer = frame_pointers.get(index + 1).copied().unwrap_or(0);
            let return_address = match self.frames.get(index + 1) {
                Some(_) if self.rule(index + 1) == Rule::Signal => self.address(index + 1),
                Some(_) => self.address(index + 1) + 1,
                None => 0,
            };
            let place = if index == 0 { self.place } else { Place::Body };
            let cfa = cfas[index];
            match (self.rule(index), place) {
                (Rule::Outermost, _) => {}

                (Rule::Signal, _) => {
                    let context = stack_pointers[index] + UCONTEXT_MCONTEXT;
                    for (slot, register) in (0..).zip(SIGCONTEXT_REGISTERS) {
                        let value = match register {
                            FRAME_POINTER => caller_frame_pointer,
                            STACK_POINTER => cfa,
                            PROGRAM_COUNTER => self.address(index + 1),
                            _ => continue,
                        };
                        memory.insert(context + 8 * slot, value);
                    }
                }
                (Rule::FramePointer, Place::Entry)
                | (
                    Rule::StackPointer {
                        saves_rbp: false, ..
                    },
                    _,
                ) => {
                    memory.insert(cfa - 8, return_address);
                }
                _ => {
                    memory.insert(cfa - 16, caller_frame_pointer);
                    memory.insert(cfa - 8, return_address);
                }
            }
        }

        let frames = (0..count)
            .map(|index| (self.address(index), frame_pointers[index], cfas[index]))
            .collect();

        Truth {
            frames,
            memory,
            stack_pointer: STACK,
            stack_end,
        }
    }
}

struct Rules<'a> {
    stack: &'a Stack,
}

impl Rules<'_> {
    fn read(memory: &mut dyn MemoryReader, address: u64) -> Option<u64> {
        memory.read_u64(VirtualAddress::new(address))
    }
}

impl UnwindInfo for Rules<'_> {
    fn cfa(
        &self,
        address: ImageAddress,
        registers: &RegisterFile,
        memory: &mut dyn MemoryReader,
    ) -> Result<VirtualAddress, UnwindTermination> {
        self.unwind(address, registers, memory).map(|step| step.cfa)
    }

    fn unwind(
        &self,
        address: ImageAddress,
        registers: &RegisterFile,
        memory: &mut dyn MemoryReader,
    ) -> Result<UnwindStep, UnwindTermination> {
        let offset = address.get().wrapping_sub(CODE);
        let function = usize::try_from(offset / FUNCTION)
            .ok()
            .filter(|function| *function < self.stack.functions.len())
            .ok_or_else(|| UnwindTermination::NoUnwindInfo {
                address: VirtualAddress::new(address.get()),
            })?;
        let within = offset % FUNCTION;
        let register = |register: DwarfRegister| {
            registers
                .get(register)
                .ok_or_else(|| UnwindTermination::RegisterUnavailable {
                    register: register.to_string().into(),
                })
        };
        let unreadable = |address: u64| UnwindTermination::MemoryReadFailed {
            address: VirtualAddress::new(address),
        };

        let (cfa, saved_frame_pointer) = match self.stack.functions[function].rule {
            Rule::Outermost => {
                let cfa = register(STACK_POINTER)? + 8;
                return Ok(UnwindStep {
                    registers: RegisterFile::new([(STACK_POINTER, cfa)]),
                    cfa: VirtualAddress::new(cfa),
                    signal_frame: false,
                });
            }
            Rule::Signal => {
                return Err(UnwindTermination::NoUnwindInfo {
                    address: VirtualAddress::new(address.get()),
                });
            }
            Rule::FramePointer if within == 0 => (register(STACK_POINTER)? + 8, None),
            Rule::FramePointer if within == 1 => {
                let cfa = register(STACK_POINTER)? + 16;
                (cfa, Some(cfa - 16))
            }
            Rule::FramePointer => {
                let cfa = register(FRAME_POINTER)?.wrapping_add(16);
                (cfa, Some(cfa.wrapping_sub(16)))
            }
            Rule::StackPointer { locals, saves_rbp } => {
                let cfa = register(STACK_POINTER)? + 8 * (locals + 1 + u64::from(saves_rbp));
                (cfa, saves_rbp.then(|| cfa - 16))
            }
        };

        let return_address = cfa.wrapping_sub(8);
        let caller =
            Self::read(memory, return_address).ok_or_else(|| unreadable(return_address))?;
        let mut caller_registers =
            RegisterFile::new([(STACK_POINTER, cfa), (PROGRAM_COUNTER, caller)]);

        let frame_pointer = saved_frame_pointer.map_or_else(
            || registers.get(FRAME_POINTER),
            |saved| Self::read(memory, saved),
        );
        if let Some(frame_pointer) = frame_pointer {
            caller_registers.set(FRAME_POINTER, frame_pointer);
        }

        Ok(UnwindStep {
            registers: caller_registers,
            cfa: VirtualAddress::new(cfa),
            signal_frame: false,
        })
    }
}

fn image(stack: &Stack) -> ModuleImage {
    let symbols = stack
        .functions
        .iter()
        .enumerate()
        .map(|(index, function)| {
            let start = ImageAddress::new(stack.start(index));
            SymbolInfo {
                id: SymbolId::new(u32::try_from(index).unwrap()),
                name: format!("f{index}").into(),
                address: start,
                kind: SymbolKind::Function,
                binding: SymbolBinding::Global,
                exported: false,
                extent: Some(SymbolExtent {
                    range: AddressRange {
                        start,
                        end: ImageAddress::new(Stack::base(index) + FUNCTION),
                    },
                    provenance: SymbolExtentProvenance::Declared,
                }),
                storage: None,
                role: function.role,
            }
        })
        .collect();

    ModuleImage::new(
        PathBuf::from("/model"),
        crate::TargetDescription {
            architecture: crate::Architecture::X86_64,
            byte_order: crate::ByteOrder::Little,
            pointer_width: crate::PointerWidth::Bits64,
        },
        AddressRange {
            start: ImageAddress::new(CODE),
            end: ImageAddress::new(Stack::base(stack.functions.len())),
        },
        crate::model::ModuleMetadata {
            symbols,
            ..crate::model::ModuleMetadata::default()
        },
    )
}

fn kernel_chain(truth: &Truth) -> Vec<u64> {
    let mut chain = vec![truth.frames[0].0];
    let mut link = truth.frames[0].1;
    while chain.len() < usize::from(CHAIN_LIMIT) {
        let (Some(next), Some(return_address)) = (
            truth.memory.get(&link),
            link.checked_add(8)
                .and_then(|above| truth.memory.get(&above)),
        ) else {
            break;
        };
        chain.push(*return_address);
        link = *next;
    }
    chain
}

fn copy(truth: &Truth, words: usize) -> Vec<u8> {
    (0..words as u64)
        .flat_map(|word| {
            let at = truth.stack_pointer + 8 * word;
            truth
                .memory
                .get(&at)
                .copied()
                .unwrap_or(0xaaaa_aaaa_aaaa_aaaa)
                .to_le_bytes()
        })
        .collect()
}

fn function() -> impl Strategy<Value = Function> {
    prop_oneof![
        Just(Rule::FramePointer),
        (0_u64..3, any::<bool>())
            .prop_map(|(locals, saves_rbp)| Rule::StackPointer { locals, saves_rbp }),
    ]
    .prop_map(|rule| Function {
        rule,
        role: CodeRole::Ordinary,
    })
}

fn stack() -> impl Strategy<Value = Stack> {
    (
        proptest::collection::vec(function(), 1..5),
        1_usize..9,
        proptest::option::of(any::<prop::sample::Index>()),
        proptest::option::of(any::<prop::sample::Index>()),
        any::<bool>(),
    )
        .prop_flat_map(|(mut functions, depth, switch, signal, returns_to_zero)| {
            let ordinary = functions.len();
            functions.push(Function {
                rule: Rule::Outermost,
                role: CodeRole::Outermost,
            });
            functions.push(Function {
                rule: Rule::FramePointer,
                role: CodeRole::StackSwitch,
            });
            functions.push(Function {
                rule: Rule::Signal,
                role: CodeRole::SignalTrampoline,
            });
            let garbage = prop_oneof![any::<u64>(), (0_u64..32).prop_map(|word| STACK + 8 * word)];
            let frame = (0..ordinary, 0x10_u64..0xf0, 0_u64..3, garbage).prop_map(
                |(function, offset, locals, garbage)| Frame {
                    function,
                    offset,
                    locals,
                    garbage,
                },
            );
            let frames = proptest::collection::vec(frame, depth).prop_map(move |mut frames| {
                if let Some(switch) = switch {
                    let at = switch.index(frames.len());
                    frames[at].function = ordinary + 1;
                }

                if let Some(signal) = signal.filter(|_| frames.len() > 1) {
                    let at = signal.index(frames.len());
                    frames.insert(
                        at,
                        Frame {
                            function: ordinary + 2,
                            offset: 0,
                            locals: 0,
                            garbage: 0,
                        },
                    );
                }
                if !returns_to_zero {
                    frames.push(Frame {
                        function: ordinary,
                        offset: 0x10,
                        locals: 0,
                        garbage: 0,
                    });
                }
                frames
            });
            (
                Just(functions),
                frames,
                prop::sample::select(vec![Place::Body, Place::Body, Place::Entry, Place::Pushed]),
                0_usize..48,
                1_usize..12,
            )
        })
        .prop_map(|(functions, frames, place, copied_words, limit)| Stack {
            functions,
            frames,
            place,
            copied_words,
            limit,
        })
        .prop_map(|mut stack| {
            if stack.rule(0) != Rule::FramePointer || stack.role(0) == CodeRole::StackSwitch {
                stack.place = Place::Body;
            }
            stack
        })
}

fn check(stack: &Stack, frames: &[SampledFrame], end: StackEnd) -> Result<(), TestCaseError> {
    let truth = stack.truth();
    prop_assert!(
        frames.len() <= stack.limit,
        "{} frames past the limit",
        frames.len()
    );
    prop_assert!(
        frames.len() <= truth.frames.len(),
        "{} frames of a stack of {}",
        frames.len(),
        truth.frames.len()
    );

    for (index, (frame, (address, frame_pointer, cfa))) in
        frames.iter().zip(&truth.frames).enumerate()
    {
        prop_assert_eq!(frame.address.get(), *address, "frame {}", index);
        prop_assert_eq!(frame.module, Some(0), "frame {}", index);
        prop_assert_eq!(frame.role, stack.role(index), "frame {}", index);
        if let Some(found) = frame.frame_pointer {
            prop_assert_eq!(found, *frame_pointer, "frame {}'s rbp", index);
        }
        if let Some(found) = frame.cfa {
            prop_assert_eq!(found, *cfa, "frame {}'s CFA", index);
        }
    }

    if end == StackEnd::Complete {
        prop_assert_eq!(
            frames.len(),
            truth.frames.len(),
            "a complete stack is all of it"
        );
    }
    Ok(())
}

fn unwinds_to_a_prefix(stack: &Stack) -> Result<(), TestCaseError> {
    let truth = stack.truth();
    let (frames, end) = sample(stack, &truth);
    check(stack, &frames, end)
}

fn sample(stack: &Stack, truth: &Truth) -> (Vec<SampledFrame>, StackEnd) {
    let words = stack
        .copied_words
        .min(usize::try_from((truth.stack_end - truth.stack_pointer) / 8).unwrap());
    let bytes = copy(truth, words);
    let chain = kernel_chain(truth);
    let registers = [
        (PROGRAM_COUNTER, truth.frames[0].0),
        (STACK_POINTER, truth.stack_pointer),
        (FRAME_POINTER, truth.frames[0].1),
    ];
    unwind(
        stack,
        SampledState {
            registers: &registers,
            stack: &bytes,
            chain: &chain,
        },
    )
}

fn unwind(stack: &Stack, sample: SampledState<'_>) -> (Vec<SampledFrame>, StackEnd) {
    let rules = Rules { stack };
    let image = image(stack);
    let modules = [UnwindModule {
        loaded: LoadedModule {
            id: ModuleId::new(1),
            image: ModuleImageId::new(1),
            load_bias: crate::LoadBias::default(),
            window: None,
        },
        image: &image,
        unwind: &rules,
    }];
    unwind_sample(
        &modules[..],
        &mut FrameRules::default(),
        sample,
        stack.limit,
    )
}

proptest! {
    #[test]
    fn a_sampled_stack_is_a_prefix_of_the_true_stack(stack in stack()) {
        unwinds_to_a_prefix(&stack)?;
    }
}

#[test]
fn the_chain_names_a_signal_trampoline_at_its_entry() {
    let function = |rule, role| Function { rule, role };
    let frame = |function, offset| Frame {
        function,
        offset,
        locals: 0,
        garbage: 0,
    };
    let stack = Stack {
        functions: vec![
            function(Rule::FramePointer, CodeRole::Ordinary),
            function(Rule::Outermost, CodeRole::Outermost),
            function(Rule::FramePointer, CodeRole::StackSwitch),
            function(Rule::Signal, CodeRole::SignalTrampoline),
        ],
        frames: vec![frame(2, 0x10), frame(3, 0), frame(0, 0x10), frame(1, 0x10)],
        place: Place::Body,
        copied_words: 0,
        limit: 2,
    };
    unwinds_to_a_prefix(&stack).unwrap();
}

#[test]
fn a_sample_in_a_signal_trampoline_continues_to_what_it_interrupted() {
    let function = |rule, role| Function { rule, role };
    let frame = |function, offset| Frame {
        function,
        offset,
        locals: 1,
        garbage: 0,
    };
    let stack = Stack {
        functions: vec![
            function(Rule::FramePointer, CodeRole::Ordinary),
            function(Rule::Outermost, CodeRole::Outermost),
            function(Rule::FramePointer, CodeRole::StackSwitch),
            function(Rule::Signal, CodeRole::SignalTrampoline),
        ],
        frames: vec![frame(3, 0), frame(0, 0x10), frame(0, 0x20), frame(1, 0x10)],
        place: Place::Body,
        copied_words: 48,
        limit: 12,
    };
    unwinds_to_a_prefix(&stack).unwrap();

    let truth = stack.truth();
    let (frames, end) = sample(&stack, &truth);
    assert_eq!(end, StackEnd::Complete);
    assert_eq!(
        frames
            .iter()
            .map(|frame| frame.address.get())
            .collect::<Vec<_>>(),
        truth.frames.iter().map(|frame| frame.0).collect::<Vec<_>>()
    );
}

fn zero_return_address(frame_pointer: u64) -> (Vec<u64>, StackEnd) {
    let function = |rule| Function {
        rule,
        role: CodeRole::Ordinary,
    };
    let stack = Stack {
        functions: vec![
            function(Rule::StackPointer {
                locals: 2,
                saves_rbp: false,
            }),
            function(Rule::FramePointer),
        ],
        frames: Vec::new(),
        place: Place::Body,
        copied_words: 5,
        limit: 8,
    };

    let rip = Stack::base(0) + 0x10;
    let caller = Stack::base(1) + 0x20;
    let bytes = [0, 0, caller + 1, 0, 0]
        .iter()
        .flat_map(|word: &u64| word.to_le_bytes())
        .collect::<Vec<_>>();
    let (frames, end) = unwind(
        &stack,
        SampledState {
            registers: &[
                (PROGRAM_COUNTER, rip),
                (STACK_POINTER, STACK),
                (FRAME_POINTER, frame_pointer),
            ],
            stack: &bytes,
            chain: &[rip],
        },
    );

    let addresses = frames.iter().map(|frame| frame.address.get()).collect();
    (addresses, end)
}

#[test]
fn a_zero_return_address_above_the_frame_ends_the_stack() {
    let (frames, end) = zero_return_address(STACK + 24);
    assert_eq!(frames, [Stack::base(0) + 0x10, Stack::base(1) + 0x20]);
    assert_eq!(end, StackEnd::Complete);
}

#[test]
fn a_zero_return_address_below_the_frame_is_no_stack_end() {
    let (frames, end) = zero_return_address(STACK - 8);
    assert_eq!(frames, [Stack::base(0) + 0x10, Stack::base(1) + 0x20]);
    assert_eq!(end, StackEnd::Truncated(Truncation::InvalidCaller, Some(0)));
}
