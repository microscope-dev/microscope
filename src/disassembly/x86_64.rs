//! The x86-64 instruction decoder and renderer, built on iced-x86.

use std::collections::HashSet;
use std::sync::Arc;

use iced_x86::{
    Code, DecoderError, DecoderOptions, FlowControl, Formatter, FormatterOutput, FormatterTextKind,
    GasFormatter, Instruction, IntelFormatter, OpKind, Register,
};

use super::{
    AssemblySyntax, ControlFlow, InstructionReferenceKind, InstructionToken, InstructionTokenKind,
    RawDecode, RawIndirect,
};

use crate::ByteOrder;
use crate::unwind::{DwarfRegister, FS_BASE, GS_BASE, RegisterFile};

/// The longest x86-64 instruction encoding the architecture permits.
const MAX_INSTRUCTION_LENGTH: usize = 15;

pub struct Decoder {
    formatter: Box<dyn Formatter>,

    /// The mnemonics, registers, and punctuation rendered so far, which
    /// nearly every instruction repeats, so that its tokens share them.
    texts: HashSet<Arc<str>>,
}

impl Decoder {
    pub fn new(syntax: AssemblySyntax) -> Self {
        let mut formatter: Box<dyn Formatter> = match syntax {
            AssemblySyntax::Intel => Box::new(IntelFormatter::new()),
            AssemblySyntax::Att => Box::new(GasFormatter::new()),
        };

        let options = formatter.options_mut();
        options.set_hex_prefix("0x");
        options.set_hex_suffix("");
        options.set_uppercase_hex(false);
        options.set_space_after_operand_separator(true);

        // Program-counter-relative operands keep their encoded form; the
        // absolute address they name is reported as a reference instead.
        options.set_rip_relative_addresses(true);
        options.set_branch_leading_zeros(false);
        options.set_show_branch_size(false);

        Self {
            formatter,
            texts: HashSet::new(),
        }
    }
}

impl super::InstructionDecoder for Decoder {
    fn max_instruction_length(&self) -> usize {
        MAX_INSTRUCTION_LENGTH
    }

    fn byte_order(&self) -> ByteOrder {
        ByteOrder::Little
    }

    fn decode(
        &mut self,
        address: u64,
        bytes: &[u8],
        registers: Option<&RegisterFile>,
    ) -> RawDecode {
        let mut decoder = iced_x86::Decoder::with_ip(64, bytes, address, DecoderOptions::NONE);
        let instruction = decoder.decode();
        if instruction.is_invalid() {
            return match decoder.last_error() {
                DecoderError::NoMoreBytes => RawDecode::Incomplete,
                _ => RawDecode::Invalid,
            };
        }

        let mut tokens = Tokens {
            tokens: Vec::with_capacity(16),
            texts: &mut self.texts,
        };

        self.formatter.format(&instruction, &mut tokens);
        RawDecode::Instruction {
            length: instruction.len(),
            tokens: tokens.tokens,
            flow: flow(&instruction),
            references: references(&instruction),
            indirect: indirect(&instruction, bytes, registers),
        }
    }
}

struct Tokens<'a> {
    tokens: Vec<InstructionToken>,
    texts: &'a mut HashSet<Arc<str>>,
}

impl FormatterOutput for Tokens<'_> {
    fn write(&mut self, text: &str, kind: FormatterTextKind) {
        let kind = match kind {
            FormatterTextKind::Mnemonic => InstructionTokenKind::Mnemonic,
            FormatterTextKind::Prefix => InstructionTokenKind::Prefix,
            FormatterTextKind::Keyword
            | FormatterTextKind::Directive
            | FormatterTextKind::Decorator => InstructionTokenKind::Keyword,
            FormatterTextKind::Register => InstructionTokenKind::Register,
            FormatterTextKind::Number | FormatterTextKind::SelectorValue => {
                InstructionTokenKind::Number
            }
            FormatterTextKind::LabelAddress | FormatterTextKind::FunctionAddress => {
                InstructionTokenKind::Address
            }
            FormatterTextKind::Punctuation | FormatterTextKind::Operator => {
                InstructionTokenKind::Punctuation
            }
            _ => InstructionTokenKind::Text,
        };

        // Numbers and addresses rarely repeat, so sharing them saves nothing.
        let text = if matches!(
            kind,
            InstructionTokenKind::Number | InstructionTokenKind::Address
        ) {
            text.into()
        } else if let Some(shared) = self.texts.get(text) {
            Arc::clone(shared)
        } else {
            let shared: Arc<str> = text.into();
            self.texts.insert(Arc::clone(&shared));
            shared
        };

        self.tokens.push(InstructionToken { kind, text });
    }
}

fn flow(instruction: &Instruction) -> ControlFlow {
    match instruction.flow_control() {
        FlowControl::Next => ControlFlow::Sequential,
        FlowControl::UnconditionalBranch => ControlFlow::Jump,
        FlowControl::ConditionalBranch => ControlFlow::ConditionalJump,
        FlowControl::IndirectBranch => ControlFlow::IndirectJump,
        FlowControl::Call => ControlFlow::Call,
        FlowControl::IndirectCall => ControlFlow::IndirectCall,
        FlowControl::Return => ControlFlow::Return,
        FlowControl::Interrupt => ControlFlow::Interrupt,
        FlowControl::XbeginXabortXend => ControlFlow::Transaction,
        FlowControl::Exception => ControlFlow::Exception,
    }
}

/// Returns the addresses an instruction alone determines: direct branch
/// targets and memory operands addressed relative to the instruction or by
/// an absolute displacement. An FS- or GS-relative operand addresses
/// thread-local storage rather than the flat address space, so it names no
/// address.
fn references(instruction: &Instruction) -> Vec<(InstructionReferenceKind, u64)> {
    let mut references = Vec::new();
    for operand in 0..instruction.op_count() {
        match instruction.op_kind(operand) {
            OpKind::NearBranch16 | OpKind::NearBranch32 | OpKind::NearBranch64 => {
                references.push((
                    InstructionReferenceKind::BranchTarget,
                    instruction.near_branch_target(),
                ));
            }
            OpKind::Memory => {
                if matches!(instruction.segment_prefix(), Register::FS | Register::GS) {
                    continue;
                }

                if instruction.is_ip_rel_memory_operand() {
                    references.push((
                        InstructionReferenceKind::MemoryOperand,
                        instruction.ip_rel_memory_address(),
                    ));
                } else if instruction.memory_base() == Register::None
                    && instruction.memory_index() == Register::None
                {
                    references.push((
                        InstructionReferenceKind::MemoryOperand,
                        instruction.memory_displacement64(),
                    ));
                }
            }
            _ => {}
        }
    }

    references
}

/// Returns a register's value, read by its x86-64 psABI DWARF number. A
/// segment register stands for its base address, which is zero in 64-bit
/// mode except for FS and GS.
fn register_value(registers: &RegisterFile, register: Register) -> Option<u64> {
    let dwarf = match register {
        Register::ES | Register::CS | Register::SS | Register::DS => return Some(0),
        Register::FS => return registers.get(FS_BASE),
        Register::GS => return registers.get(GS_BASE),
        _ => match register.full_register() {
            Register::RAX => 0,
            Register::RDX => 1,
            Register::RCX => 2,
            Register::RBX => 3,
            Register::RSI => 4,
            Register::RDI => 5,
            Register::RBP => 6,
            Register::RSP => 7,
            Register::R8 => 8,
            Register::R9 => 9,
            Register::R10 => 10,
            Register::R11 => 11,
            Register::R12 => 12,
            Register::R13 => 13,
            Register::R14 => 14,
            Register::R15 => 15,
            _ => return None,
        },
    };

    let value = registers.get(DwarfRegister::new(dwarf))?;
    Some(match register.size() {
        8 => value,
        bytes => value & ((1 << (bytes * 8)) - 1),
    })
}

/// Returns where an indirect jump, call, or return finds its target: memory
/// whose address the instruction alone determines, or, given the registers
/// it will execute with, any register or memory operand.
fn indirect(
    instruction: &Instruction,
    bytes: &[u8],
    registers: Option<&RegisterFile>,
) -> Option<RawIndirect> {
    if !matches!(
        instruction.flow_control(),
        FlowControl::IndirectBranch | FlowControl::IndirectCall | FlowControl::Return
    ) {
        return None;
    }

    // In 64-bit mode Intel processors ignore an operand-size prefix on a near
    // branch, while AMD processors narrow its target to 16 bits.
    let amd = iced_x86::Decoder::with_ip(
        64,
        &bytes[..instruction.len()],
        instruction.ip(),
        DecoderOptions::AMD,
    )
    .decode();
    if amd.code() != instruction.code() {
        return Some(RawIndirect::Unsupported);
    }

    Some(match (instruction.code(), instruction.op0_kind()) {
        (Code::Jmp_rm64 | Code::Call_rm64, OpKind::Register) => registers
            .and_then(|registers| register_value(registers, instruction.op0_register()))
            .map_or(RawIndirect::NeedsRegisters, RawIndirect::Value),
        (Code::Jmp_rm64 | Code::Call_rm64, OpKind::Memory) => {
            let size = instruction.memory_size().size();

            // An address without registers holds at every instruction.
            let static_address = instruction.virtual_address(0, 0, |register, _, _| {
                matches!(
                    register,
                    Register::ES | Register::CS | Register::SS | Register::DS
                )
                .then_some(0)
            });
            static_address
                .or_else(|| {
                    let registers = registers?;
                    instruction
                        .virtual_address(0, 0, |register, _, _| register_value(registers, register))
                })
                .map_or(RawIndirect::NeedsRegisters, |address| RawIndirect::Load {
                    address,
                    size,
                })
        }
        (Code::Retnq | Code::Retnq_imm16, _) => registers
            .and_then(|registers| register_value(registers, Register::RSP))
            .map_or(RawIndirect::NeedsRegisters, |address| RawIndirect::Load {
                address,
                size: 8,
            }),
        _ => RawIndirect::Unsupported,
    })
}
