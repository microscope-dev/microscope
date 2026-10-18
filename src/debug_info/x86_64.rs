use iced_x86::{
    Code, Decoder, DecoderOptions, FlowControl, Instruction, InstructionInfoFactory, Mnemonic,
    OpAccess, OpKind, Register,
};

pub(super) mod frames;

const MAX_PROLOGUE_BYTES: usize = 256;
const MAX_PROLOGUE_INSTRUCTIONS: usize = 64;

/// Explains why an instruction prefix cannot be proven to contain only an
/// x86-64 System V prologue.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum PrologueAnalysisError {
    Empty,
    TooLong,
    TooManyInstructions,
    InvalidInstruction,
    MissingFramePointerSave,
    MissingFramePointerSetup,
    UnsupportedInstruction,
}

/// Proves that every byte before a candidate source row is a frame-pointer
/// x86-64 System V prologue: frame setup, callee-save pushes, a constant
/// stack allocation, and unmodified argument copies between registers and
/// frame slots. Anything else is rejected, so an optimized function's real
/// work is never skipped.
pub(super) fn prove_prologue_prefix(
    bytes: &[u8],
    instruction_pointer: u64,
) -> Result<(), PrologueAnalysisError> {
    if bytes.is_empty() {
        return Err(PrologueAnalysisError::Empty);
    }

    if bytes.len() > MAX_PROLOGUE_BYTES {
        return Err(PrologueAnalysisError::TooLong);
    }

    let mut decoder = Decoder::with_ip(64, bytes, instruction_pointer, DecoderOptions::NONE);
    let first = decode(&mut decoder)?;
    let (save, mut count) = if first.mnemonic() == Mnemonic::Endbr64 {
        (decode(&mut decoder)?, 2)
    } else {
        (first, 1)
    };

    if !is_push_register(&save, Register::RBP) {
        return Err(PrologueAnalysisError::MissingFramePointerSave);
    }

    let setup = decode(&mut decoder)?;
    if !is_register_move(&setup, Register::RBP, Register::RSP) {
        return Err(PrologueAnalysisError::MissingFramePointerSetup);
    }

    count += 1;
    let mut tainted = TaintedRegisters::incoming_arguments();

    while decoder.can_decode() {
        count += 1;
        if count > MAX_PROLOGUE_INSTRUCTIONS {
            return Err(PrologueAnalysisError::TooManyInstructions);
        }

        let instruction = decode(&mut decoder)?;

        if is_callee_save_push(&instruction) || is_stack_allocation(&instruction) {
            continue;
        }

        if !is_argument_copy(&instruction, &mut tainted) {
            return Err(PrologueAnalysisError::UnsupportedInstruction);
        }
    }

    Ok(())
}

/// The registers besides rsp that the x86-64 System V ABI has a function
/// return with as its caller left them, in the order of
/// `dwarf::X86_64_CALLEE_SAVED_REGISTERS`, whose bits [`restored_by`]
/// and [`runs_to_return`] name them by.
const CALLEE_SAVED: [Register; 6] = [
    Register::RBX,
    Register::RBP,
    Register::R12,
    Register::R13,
    Register::R14,
    Register::R15,
];

/// The most instructions an epilogue runs to its return once it has
/// restored its caller's registers.
const MAX_RETURN_INSTRUCTIONS: usize = 32;

/// Every register its caller keeps, as bits of [`CALLEE_SAVED`].
pub(super) const ALL_CALLEE_SAVED: u32 = (1 << CALLEE_SAVED.len()) - 1;

/// Reads the code of epilogues, reusing one decoder's buffers for all of
/// an image's: a load reads thousands.
pub(super) struct Epilogues {
    info: InstructionInfoFactory,
}

impl Epilogues {
    pub(super) fn new() -> Self {
        Self {
            info: InstructionInfoFactory::new(),
        }
    }

    /// Where the near return ends that `code`, at `instruction_pointer`, runs
    /// straight to, writing neither rsp nor a register its caller keeps; `None`
    /// if it does not. At a return, as the ABI requires, those registers hold
    /// what the caller left in them, so code that runs straight to one without
    /// writing them holds those values already.
    pub(super) fn runs_to_return(&mut self, code: &[u8], instruction_pointer: u64) -> Option<u64> {
        let mut decoder = Decoder::with_ip(64, code, instruction_pointer, DecoderOptions::NONE);
        for _ in 0..MAX_RETURN_INSTRUCTIONS {
            let instruction = decoded(&mut decoder)?;
            let writes = written(&mut self.info, &instruction)
                .any(|register| register == Register::RSP || CALLEE_SAVED.contains(&register));
            if instruction.mnemonic() == Mnemonic::Ret && instruction.op_count() == 0 {
                return Some(instruction.next_ip());
            }

            if writes || instruction.flow_control() != FlowControl::Next {
                return None;
            }
        }

        None
    }

    /// Where the run of `code`, at `instruction_pointer`, that keeps the
    /// registers `kept`, bits of [`CALLEE_SAVED`], ends: before the first
    /// instruction that writes one of them or rsp, or past the first that may
    /// go elsewhere, as a code path a jump reaches proves nothing.
    pub(super) fn keeps(&mut self, code: &[u8], instruction_pointer: u64, kept: u32) -> u64 {
        let mut decoder = Decoder::with_ip(64, code, instruction_pointer, DecoderOptions::NONE);
        let mut end = instruction_pointer;
        for _ in 0..MAX_RETURN_INSTRUCTIONS {
            let Some(instruction) = decoded(&mut decoder) else {
                break;
            };

            let writes = written(&mut self.info, &instruction).any(|register| {
                register == Register::RSP
                    || CALLEE_SAVED
                        .iter()
                        .enumerate()
                        .any(|(index, saved)| kept & (1 << index) != 0 && *saved == register)
            });
            if writes {
                break;
            }

            end = instruction.next_ip();
            if instruction.flow_control() != FlowControl::Next {
                break;
            }
        }

        end
    }
}

/// The register its caller keeps, as a bit of [`CALLEE_SAVED`], that the
/// last instruction of `code`, at `instruction_pointer`, loads from the
/// top of the stack as it frees the frame: a `pop`, or `leave`, which
/// loads rbp. `None` unless `code` decodes into whole instructions.
pub(super) fn restored_by(code: &[u8], instruction_pointer: u64) -> Option<u32> {
    let mut decoder = Decoder::with_ip(64, code, instruction_pointer, DecoderOptions::NONE);
    let mut last = None;
    while decoder.can_decode() {
        last = Some(decoded(&mut decoder)?);
    }

    let last = last?;
    let register = match last.mnemonic() {
        Mnemonic::Leave if last.op_count() == 0 => Register::RBP,
        Mnemonic::Pop if last.op_count() == 1 && last.op0_kind() == OpKind::Register => {
            last.op0_register()
        }
        _ => return None,
    };

    let index = CALLEE_SAVED.iter().position(|saved| *saved == register)?;
    Some(1 << index)
}

/// The next whole instruction, or `None` at the end or before bytes that
/// are none.
fn decoded(decoder: &mut Decoder<'_>) -> Option<Instruction> {
    if !decoder.can_decode() {
        return None;
    }

    let instruction = decoder.decode();
    (instruction.code() != Code::INVALID).then_some(instruction)
}

/// The full registers `instruction` may write.
fn written<'a>(
    info: &'a mut InstructionInfoFactory,
    instruction: &Instruction,
) -> impl Iterator<Item = Register> + 'a {
    info.info(instruction)
        .used_registers()
        .iter()
        .filter(|used| !matches!(used.access(), OpAccess::Read | OpAccess::CondRead))
        .map(|used| full_register(used.register()))
}

fn decode(decoder: &mut Decoder<'_>) -> Result<Instruction, PrologueAnalysisError> {
    if !decoder.can_decode() {
        return Err(PrologueAnalysisError::InvalidInstruction);
    }

    let instruction = decoder.decode();
    if instruction.code() == Code::INVALID {
        return Err(PrologueAnalysisError::InvalidInstruction);
    }

    Ok(instruction)
}

// Structural prologue checks compare exact registers: operand-size overrides
// (`push bp`, `mov ebp, esp`) are not the canonical eight-byte frame
// operations this proof requires.
fn is_push_register(instruction: &Instruction, register: Register) -> bool {
    instruction.mnemonic() == Mnemonic::Push
        && instruction.op_count() == 1
        && instruction.op0_kind() == OpKind::Register
        && instruction.op0_register() == register
}

fn is_register_move(instruction: &Instruction, destination: Register, source: Register) -> bool {
    instruction.mnemonic() == Mnemonic::Mov
        && instruction.op_count() == 2
        && instruction.op0_kind() == OpKind::Register
        && instruction.op1_kind() == OpKind::Register
        && instruction.op0_register() == destination
        && instruction.op1_register() == source
}

fn is_callee_save_push(instruction: &Instruction) -> bool {
    [
        Register::RBX,
        Register::R12,
        Register::R13,
        Register::R14,
        Register::R15,
    ]
    .iter()
    .any(|register| is_push_register(instruction, *register))
}

fn is_stack_allocation(instruction: &Instruction) -> bool {
    matches!(instruction.mnemonic(), Mnemonic::Sub | Mnemonic::And)
        && instruction.op_count() == 2
        && instruction.op0_kind() == OpKind::Register
        && instruction.op0_register() == Register::RSP
        && is_immediate(instruction.op1_kind())
}

fn is_argument_copy(instruction: &Instruction, tainted: &mut TaintedRegisters) -> bool {
    if !is_copy_mnemonic(instruction.mnemonic()) || instruction.op_count() != 2 {
        return false;
    }

    match (instruction.op0_kind(), instruction.op1_kind()) {
        (OpKind::Register, OpKind::Register) => {
            let source = full_register(instruction.op1_register());
            let destination = full_register(instruction.op0_register());

            // Redefining the stack or frame pointer is never argument homing;
            // accepting it would prove away real work that destroys the frame
            // this analysis just validated.
            if matches!(destination, Register::RSP | Register::RBP) {
                return false;
            }

            if !tainted.contains(source) {
                return false;
            }

            tainted.insert(destination);
            true
        }
        (OpKind::Memory, OpKind::Register) => {
            tainted.contains(full_register(instruction.op1_register()))
                && is_frame_store(instruction)
        }
        (OpKind::Register, OpKind::Memory) => {
            let destination = full_register(instruction.op0_register());
            if matches!(destination, Register::RSP | Register::RBP)
                || !is_stack_argument_load(instruction)
            {
                return false;
            }

            tainted.insert(destination);
            true
        }
        _ => false,
    }
}

const fn is_copy_mnemonic(mnemonic: Mnemonic) -> bool {
    matches!(
        mnemonic,
        Mnemonic::Mov
            | Mnemonic::Movapd
            | Mnemonic::Movaps
            | Mnemonic::Movd
            | Mnemonic::Movdqa
            | Mnemonic::Movdqu
            | Mnemonic::Movq
            | Mnemonic::Movsd
            | Mnemonic::Movss
            | Mnemonic::Movsx
            | Mnemonic::Movsxd
            | Mnemonic::Movupd
            | Mnemonic::Movups
            | Mnemonic::Movzx
            | Mnemonic::Vmovapd
            | Mnemonic::Vmovaps
            | Mnemonic::Vmovd
            | Mnemonic::Vmovdqa
            | Mnemonic::Vmovdqu
            | Mnemonic::Vmovq
            | Mnemonic::Vmovsd
            | Mnemonic::Vmovss
            | Mnemonic::Vmovupd
            | Mnemonic::Vmovups
    )
}

// Frame accesses require the exact 64-bit RBP base with no segment override:
// an FS/GS prefix targets thread-local storage rather than the stack, and an
// address-size override truncates the effective address to EBP.
fn is_plain_frame_access(instruction: &Instruction) -> bool {
    instruction.memory_index() == Register::None
        && instruction.segment_prefix() == Register::None
        && instruction.memory_base() == Register::RBP
}

fn is_frame_store(instruction: &Instruction) -> bool {
    is_plain_frame_access(instruction) && signed_displacement(instruction) < 0
}

fn is_stack_argument_load(instruction: &Instruction) -> bool {
    is_plain_frame_access(instruction) && signed_displacement(instruction) >= 16
}

const fn signed_displacement(instruction: &Instruction) -> i64 {
    instruction.memory_displacement64().cast_signed()
}

const fn is_immediate(kind: OpKind) -> bool {
    matches!(
        kind,
        OpKind::Immediate8
            | OpKind::Immediate8_2nd
            | OpKind::Immediate16
            | OpKind::Immediate32
            | OpKind::Immediate64
            | OpKind::Immediate8to16
            | OpKind::Immediate8to32
            | OpKind::Immediate8to64
            | OpKind::Immediate32to64
    )
}

fn full_register(register: Register) -> Register {
    register.info().full_register()
}

struct TaintedRegisters {
    registers: [Register; MAX_PROLOGUE_INSTRUCTIONS + 16],
    len: usize,
}

impl TaintedRegisters {
    fn incoming_arguments() -> Self {
        let mut registers = Self {
            registers: [Register::None; MAX_PROLOGUE_INSTRUCTIONS + 16],
            len: 0,
        };

        for register in [
            Register::RDI,
            Register::RSI,
            Register::RDX,
            Register::RCX,
            Register::R8,
            Register::R9,
            Register::ZMM0,
            Register::ZMM1,
            Register::ZMM2,
            Register::ZMM3,
            Register::ZMM4,
            Register::ZMM5,
            Register::ZMM6,
            Register::ZMM7,
        ] {
            registers.insert(register);
        }

        registers
    }

    fn contains(&self, register: Register) -> bool {
        self.registers[..self.len].contains(&register)
    }

    fn insert(&mut self, register: Register) {
        if !self.contains(register) {
            self.registers[self.len] = register;
            self.len += 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use PrologueAnalysisError::{
        InvalidInstruction, MissingFramePointerSave, MissingFramePointerSetup,
        UnsupportedInstruction,
    };

    type Case = (&'static [u8], u64, Result<(), PrologueAnalysisError>);

    const PROLOGUES: [Case; 16] = [
        // gcc -O0 parameter homing
        (
            &[
                0x55, 0x48, 0x89, 0xe5, 0x89, 0xc8, 0x45, 0x89, 0xc2, 0x45, 0x89, 0xc8, 0xf3, 0x0f,
                0x11, 0x45, 0xd4, 0xf2, 0x0f, 0x11, 0x4d, 0xc8, 0x89, 0xf9, 0x88, 0x4d, 0xec, 0x89,
                0xf1, 0x88, 0x4d, 0xe8, 0x88, 0x55, 0xe4, 0x88, 0x45, 0xe0, 0x44, 0x89, 0xd0, 0x66,
                0x89, 0x45, 0xdc, 0x44, 0x89, 0xc0, 0x66, 0x89, 0x45, 0xd8,
            ],
            0x1129,
            Ok(()),
        ),
        // A stack argument copy and callee-save bookkeeping
        (
            &[
                0x55, 0x48, 0x89, 0xe5, 0x53, 0x48, 0x83, 0xec, 0x20, 0x48, 0x8b, 0x45, 0x10, 0x48,
                0x89, 0x45, 0xf8,
            ],
            0x1000,
            Ok(()),
        ),
        // endbr64 before the prologue
        (
            &[
                0xf3, 0x0f, 0x1e, 0xfa, 0x55, 0x48, 0x89, 0xe5, 0x89, 0x7d, 0xfc,
            ],
            0x1000,
            Ok(()),
        ),
        // gcc -O2 real work at entry
        (
            &[
                0x48, 0xc7, 0x05, 0x7d, 0x2e, 0x00, 0x00, 0x63, 0x00, 0x00, 0x00,
            ],
            0x11e0,
            Err(MissingFramePointerSave),
        ),
        // Real work after frame setup
        (
            &[
                0x55, 0x48, 0x89, 0xe5, 0xc7, 0x45, 0xfc, 0x63, 0x00, 0x00, 0x00,
            ],
            0x1000,
            Err(UnsupportedInstruction),
        ),
        // Control flow after frame setup
        (
            &[0x55, 0x48, 0x89, 0xe5, 0xeb, 0x00],
            0x1000,
            Err(UnsupportedInstruction),
        ),
        // A stack probe
        (
            &[
                0x55, 0x48, 0x89, 0xe5, 0x48, 0x81, 0xec, 0x00, 0x10, 0x00, 0x00, 0x48, 0x83, 0x0c,
                0x24, 0x00,
            ],
            0x1000,
            Err(UnsupportedInstruction),
        ),
        // An rsp-relative argument store
        (
            &[
                0x55, 0x48, 0x89, 0xe5, 0x48, 0x83, 0xec, 0x20, 0x48, 0x89, 0x3c, 0x24,
            ],
            0x1000,
            Err(UnsupportedInstruction),
        ),
        // mov rsp, rdi; mov rbp, rdi; mov rbp, [rbp+0x10]
        (
            &[0x55, 0x48, 0x89, 0xe5, 0x48, 0x89, 0xfc],
            0x1000,
            Err(UnsupportedInstruction),
        ),
        (
            &[0x55, 0x48, 0x89, 0xe5, 0x48, 0x89, 0xfd],
            0x1000,
            Err(UnsupportedInstruction),
        ),
        (
            &[0x55, 0x48, 0x89, 0xe5, 0x48, 0x8b, 0x6d, 0x10],
            0x1000,
            Err(UnsupportedInstruction),
        ),
        // push bp; mov ebp, esp
        (
            &[0x66, 0x55, 0x48, 0x89, 0xe5],
            0x1000,
            Err(MissingFramePointerSave),
        ),
        (&[0x55, 0x89, 0xe5], 0x1000, Err(MissingFramePointerSetup)),
        // mov fs:[rbp-8], rdi; mov [ebp-8], rdi
        (
            &[0x55, 0x48, 0x89, 0xe5, 0x64, 0x48, 0x89, 0x7d, 0xf8],
            0x1000,
            Err(UnsupportedInstruction),
        ),
        (
            &[0x55, 0x48, 0x89, 0xe5, 0x67, 0x48, 0x89, 0x7d, 0xf8],
            0x1000,
            Err(UnsupportedInstruction),
        ),
        // A candidate inside an instruction
        (&[0x55, 0x48, 0x89], 0x1000, Err(InvalidInstruction)),
    ];

    /// Code runs straight to a return keeping the registers its caller
    /// keeps only when nothing on the way writes one, the stack pointer, or
    /// goes elsewhere: CPython's `_PyObject_Malloc` after its `leave`,
    /// clearing the registers it may.
    #[test]
    fn only_code_that_keeps_the_callers_registers_runs_to_its_return() {
        // xor edx, edx; xor ecx, ecx; xor esi, esi; xor edi, edi;
        // xor r8d, r8d; ret
        let tail = [
            0x31, 0xd2, 0x31, 0xc9, 0x31, 0xf6, 0x31, 0xff, 0x45, 0x31, 0xc0, 0xc3,
        ];
        assert_eq!(
            Epilogues::new().runs_to_return(&tail, 0x14_5618),
            Some(0x14_5624)
        );
        assert_eq!(
            Epilogues::new().runs_to_return(&tail[8..], 0x14_5620),
            Some(0x14_5624)
        );
        assert_eq!(
            Epilogues::new().runs_to_return(&[0xc3], 0x1000),
            Some(0x1001)
        );

        let cases: [&[u8]; 9] = [
            // pop rbx; ret
            &[0x5b, 0xc3],
            // mov rbx, rax; ret
            &[0x48, 0x89, 0xc3, 0xc3],
            // xor r12d, r12d; ret
            &[0x45, 0x31, 0xe4, 0xc3],
            // add rsp, 8; ret
            &[0x48, 0x83, 0xc4, 0x08, 0xc3],
            // leave; ret
            &[0xc9, 0xc3],
            // jmp to the next instruction; ret
            &[0xeb, 0x00, 0xc3],
            // ret 8, which frees its caller's arguments
            &[0xc2, 0x08, 0x00],
            // no return
            &[0x31, 0xd2],
            // an instruction cut short
            &[0x48, 0x89],
        ];
        for (index, code) in cases.into_iter().enumerate() {
            assert_eq!(
                Epilogues::new().runs_to_return(code, 0x1000),
                None,
                "case {index}"
            );
        }
    }

    /// A frame freed by a `pop` or `leave` restores the register it loads,
    /// which code keeps until it writes it or may go elsewhere: CPython
    /// 3.15's handlers, which pop rbp and jump to the next.
    #[test]
    fn a_pop_restores_the_register_it_loads_until_it_is_written() {
        const RBX: u32 = 1;
        const RBP: u32 = 2;
        // add rsp, 0x30; pop rbp
        assert_eq!(
            restored_by(&[0x48, 0x83, 0xc4, 0x30, 0x5d], 0x1000),
            Some(RBP)
        );
        assert_eq!(restored_by(&[0xc9], 0x1000), Some(RBP));
        assert_eq!(restored_by(&[0x5b], 0x1000), Some(RBX));
        // pop rax; pop bp; add rsp, 8; a pop cut short
        for code in [
            &[0x58][..],
            &[0x66, 0x5d],
            &[0x48, 0x83, 0xc4, 0x08],
            &[0x41],
        ] {
            assert_eq!(restored_by(code, 0x1000), None, "{code:x?}");
        }

        // cmp qword [r8+rax*8], 0x1a01d40; je; jmp [r8+rax*8]
        let dispatch = [
            0x49, 0x81, 0x3c, 0xc0, 0x40, 0x1d, 0xa0, 0x01, 0x0f, 0x84, 0x5c, 0xd1, 0xff, 0xff,
            0x41, 0xff, 0x24, 0xc0,
        ];
        assert_eq!(
            Epilogues::new().keeps(&dispatch, 0x1a0_4bd6, RBP),
            0x1a0_4be4
        );
        // mov rbp, rdi; ret, which moves rsp
        assert_eq!(
            Epilogues::new().keeps(&[0x48, 0x89, 0xfd, 0xc3], 0x1000, RBP),
            0x1000
        );
        assert_eq!(
            Epilogues::new().keeps(&[0x48, 0x89, 0xfd, 0xc3], 0x1000, RBX),
            0x1003
        );
        // push rax
        assert_eq!(Epilogues::new().keeps(&[0x50], 0x1000, RBX), 0x1000);
    }

    #[test]
    fn only_frame_setup_and_argument_homing_prove_a_prologue() {
        for (index, (bytes, address, expected)) in PROLOGUES.into_iter().enumerate() {
            assert_eq!(
                prove_prologue_prefix(bytes, address),
                expected,
                "case {index}"
            );
        }
    }
}
