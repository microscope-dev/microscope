//! What each implemented instruction does.
//!
//! Every instruction reads all it needs before it writes, and writes memory
//! before it returns, so one that faults leaves the caller's registers as
//! they were: [`super::execute`] works on a copy and keeps it only on success.

use std::cell::Cell;

use iced_x86::{Code, Instruction, Mnemonic, OpKind, Register};

use super::flags::{self, mask, sign_extend, with_status};
use super::{
    Access, Accesses, CARRY, DIRECTION, FPE_INTDIV, Fault, Outcome, R11, RAX, RBP, RCX, RDI, RDX,
    RSI, RSP, Registers, SIGFPE, Stop,
};

use crate::sim::memory::AddressSpace;

type Step = Result<Outcome, Stop>;

/// Every instruction form the model implements, in iced's order. The
/// lockstep test (`conformance::cpu`) executes each on the real CPU, but
/// for [`PROBED_THROUGH_THE_KERNEL`], and requires this list to be
/// exactly the forms it executes, so a form is added with a golden
/// program that executes it. Any other form is a gap, though it shares a
/// mnemonic with one here.
pub const IMPLEMENTED: [Code; 128] = [
    Code::Add_rm32_r32,
    Code::Add_rm64_r64,
    Code::Add_r32_rm32,
    Code::Add_r64_rm64,
    Code::Add_RAX_imm32,
    Code::Or_rm8_r8,
    Code::Or_rm64_r64,
    Code::And_r64_rm64,
    Code::And_AL_imm8,
    Code::Sub_rm32_r32,
    Code::Sub_rm64_r64,
    Code::Xor_rm32_r32,
    Code::Xor_rm64_r64,
    Code::Xor_r64_rm64,
    Code::Cmp_rm64_r64,
    Code::Cmp_r64_rm64,
    Code::Cmp_AL_imm8,
    Code::Push_r64,
    Code::Pop_r64,
    Code::Movsxd_r64_rm32,
    Code::Imul_r16_rm16_imm16,
    Code::Imul_r64_rm64_imm32,
    Code::Imul_r32_rm32_imm8,
    Code::Imul_r64_rm64_imm8,
    Code::Jb_rel8_64,
    Code::Jae_rel8_64,
    Code::Je_rel8_64,
    Code::Jne_rel8_64,
    Code::Jbe_rel8_64,
    Code::Ja_rel8_64,
    Code::Js_rel8_64,
    Code::Jns_rel8_64,
    Code::Jl_rel8_64,
    Code::Jge_rel8_64,
    Code::Jle_rel8_64,
    Code::Jg_rel8_64,
    Code::Add_rm8_imm8,
    Code::Or_rm8_imm8,
    Code::And_rm8_imm8,
    Code::Cmp_rm8_imm8,
    Code::Add_rm64_imm32,
    Code::Add_rm32_imm8,
    Code::Add_rm64_imm8,
    Code::Adc_rm64_imm8,
    Code::Sbb_rm64_imm8,
    Code::And_rm32_imm8,
    Code::And_rm64_imm8,
    Code::Sub_rm32_imm8,
    Code::Sub_rm64_imm8,
    Code::Cmp_rm32_imm8,
    Code::Cmp_rm64_imm8,
    Code::Test_rm8_r8,
    Code::Test_rm32_r32,
    Code::Test_rm64_r64,
    Code::Xchg_rm64_r64,
    Code::Mov_rm8_r8,
    Code::Mov_rm16_r16,
    Code::Mov_rm32_r32,
    Code::Mov_rm64_r64,
    Code::Mov_r8_rm8,
    Code::Mov_r32_rm32,
    Code::Mov_r64_rm64,
    Code::Lea_r32_m,
    Code::Lea_r64_m,
    Code::Nopw,
    Code::Nopd,
    Code::Cdqe,
    Code::Movsb_m8_m8,
    Code::Test_AL_imm8,
    Code::Stosb_m8_AL,
    Code::Mov_r8_imm8,
    Code::Mov_r32_imm32,
    Code::Mov_r64_imm64,
    Code::Shl_rm32_imm8,
    Code::Shl_rm64_imm8,
    Code::Shr_rm32_imm8,
    Code::Shr_rm64_imm8,
    Code::Retnq,
    Code::Mov_rm8_imm8,
    Code::Mov_rm16_imm16,
    Code::Mov_rm32_imm32,
    Code::Mov_rm64_imm32,
    Code::Leaveq,
    Code::Int3,
    Code::Shr_rm64_1,
    Code::Call_rel32_64,
    Code::Jmp_rel32_64,
    Code::Jmp_rel8_64,
    Code::Test_rm8_imm8,
    Code::Neg_rm32,
    Code::Neg_rm64,
    Code::Mul_rm64,
    Code::Div_rm64,
    Code::Inc_rm64,
    Code::Dec_rm64,
    Code::Call_rm64,
    Code::Syscall,
    Code::Nop_rm16,
    Code::Nop_rm32,
    Code::Cmovb_r64_rm64,
    Code::Cmovae_r64_rm64,
    Code::Cmove_r32_rm32,
    Code::Cmove_r64_rm64,
    Code::Cmovne_r32_rm32,
    Code::Cmovne_r64_rm64,
    Code::Cmova_r64_rm64,
    Code::Jb_rel32_64,
    Code::Jae_rel32_64,
    Code::Je_rel32_64,
    Code::Jne_rel32_64,
    Code::Jbe_rel32_64,
    Code::Ja_rel32_64,
    Code::Jg_rel32_64,
    Code::Setb_rm8,
    Code::Setae_rm8,
    Code::Sete_rm8,
    Code::Setne_rm8,
    Code::Seta_rm8,
    Code::Setle_rm8,
    Code::Shld_rm64_r64_imm8,
    Code::Imul_r32_rm32,
    Code::Imul_r64_rm64,
    Code::Movzx_r32_rm8,
    Code::Movzx_r32_rm16,
    Code::Movsx_r32_rm8,
    Code::Movsx_r64_rm8,
    Code::Xadd_rm32_r32,
    Code::Xadd_rm64_r64,
];

/// The implemented forms the lockstep cannot execute, each pinned by a
/// kernel probe that runs it through the model instead: no golden program
/// executes `int3`, which K-TRAP-1's breakpoints do.
#[cfg(test)]
pub const PROBED_THROUGH_THE_KERNEL: [Code; 1] = [Code::Int3];

const SET_ON_CONDITION: [Mnemonic; 16] = [
    Mnemonic::Seto,
    Mnemonic::Setno,
    Mnemonic::Setb,
    Mnemonic::Setae,
    Mnemonic::Sete,
    Mnemonic::Setne,
    Mnemonic::Setbe,
    Mnemonic::Seta,
    Mnemonic::Sets,
    Mnemonic::Setns,
    Mnemonic::Setp,
    Mnemonic::Setnp,
    Mnemonic::Setl,
    Mnemonic::Setge,
    Mnemonic::Setle,
    Mnemonic::Setg,
];

const MOVE_ON_CONDITION: [Mnemonic; 16] = [
    Mnemonic::Cmovo,
    Mnemonic::Cmovno,
    Mnemonic::Cmovb,
    Mnemonic::Cmovae,
    Mnemonic::Cmove,
    Mnemonic::Cmovne,
    Mnemonic::Cmovbe,
    Mnemonic::Cmova,
    Mnemonic::Cmovs,
    Mnemonic::Cmovns,
    Mnemonic::Cmovp,
    Mnemonic::Cmovnp,
    Mnemonic::Cmovl,
    Mnemonic::Cmovge,
    Mnemonic::Cmovle,
    Mnemonic::Cmovg,
];

#[expect(
    clippy::too_many_lines,
    reason = "one arm per instruction keeps each meaning in one place"
)]
pub(super) fn execute(
    instruction: &Instruction,
    registers: &mut Registers,
    memory: &mut AddressSpace,
    accesses: &Cell<Accesses>,
) -> Step {
    // Each arm below gives a mnemonic its meaning, but only the forms the
    // lockstep checked are implemented: one that shares the mnemonic but
    // works on other registers, as SSE's `movsd` shares the string
    // move's, or on other widths, is a different instruction.
    if IMPLEMENTED.binary_search(&instruction.code()).is_err() {
        return Err(Stop::Unsupported);
    }

    let mut cpu = Cpu {
        instruction,
        registers,
        memory,
        accesses,
    };

    if instruction.is_jcc_short_or_near() {
        if cpu.holds()? {
            cpu.registers.rip = instruction.near_branch_target();
        }

        return Ok(Outcome::Completed);
    }

    if SET_ON_CONDITION.contains(&instruction.mnemonic()) {
        let value = u64::from(cpu.holds()?);
        cpu.write(0, value)?;
        return Ok(Outcome::Completed);
    }

    if MOVE_ON_CONDITION.contains(&instruction.mnemonic()) {
        let value = cpu.read(1)?;
        if cpu.holds()? {
            cpu.write(0, value)?;
        } else {
            // A 32-bit cmov clears the upper half whatever the condition.
            let unchanged = cpu.read(0)?;
            cpu.write(0, unchanged)?;
        }

        return Ok(Outcome::Completed);
    }

    match instruction.mnemonic() {
        Mnemonic::Nop => {}

        // Reads zero-extend from the source's width.
        Mnemonic::Mov | Mnemonic::Movzx => {
            let value = cpu.read(1)?;
            cpu.write(0, value)?;
        }
        Mnemonic::Movsx | Mnemonic::Movsxd => {
            let value = sign_extend(cpu.read(1)?, cpu.bits(1)?);
            cpu.write(0, value)?;
        }
        Mnemonic::Cdqe => {
            let value = sign_extend(cpu.registers.general[RAX], 32);
            cpu.registers.general[RAX] = value;
        }
        Mnemonic::Lea => {
            let address = cpu.address(1)?;
            cpu.write(0, address)?;
        }
        Mnemonic::Xchg => {
            let (first, second) = (cpu.read(0)?, cpu.read(1)?);
            cpu.write(0, second)?;
            cpu.write(1, first)?;
        }
        Mnemonic::Push => {
            let value = cpu.read(0)?;
            cpu.push(value)?;
        }
        Mnemonic::Pop => {
            let value = cpu.pop()?;
            cpu.write(0, value)?;
        }
        Mnemonic::Leave => {
            cpu.registers.general[RSP] = cpu.registers.general[RBP];
            let saved = cpu.pop()?;
            cpu.registers.general[RBP] = saved;
        }
        Mnemonic::Call => {
            let target = cpu.branch_target()?;
            cpu.push(cpu.registers.rip)?;
            cpu.registers.rip = target;
        }
        Mnemonic::Ret => cpu.registers.rip = cpu.pop()?,
        Mnemonic::Jmp => cpu.registers.rip = cpu.branch_target()?,
        Mnemonic::Add | Mnemonic::Sub | Mnemonic::Cmp => {
            let bits = cpu.bits(0)?;
            let (a, b) = (cpu.read(0)?, cpu.read(1)?);

            let (result, status) = if instruction.mnemonic() == Mnemonic::Add {
                flags::add(a, b, false, bits)
            } else {
                flags::sub(a, b, false, bits)
            };

            if instruction.mnemonic() != Mnemonic::Cmp {
                cpu.write(0, result)?;
            }

            cpu.set_status(status);
        }
        Mnemonic::Adc | Mnemonic::Sbb => {
            let bits = cpu.bits(0)?;
            let (a, b) = (cpu.read(0)?, cpu.read(1)?);
            let carry = cpu.registers.rflags & CARRY != 0;

            let (result, status) = if instruction.mnemonic() == Mnemonic::Adc {
                flags::add(a, b, carry, bits)
            } else {
                flags::sub(a, b, carry, bits)
            };

            cpu.write(0, result)?;
            cpu.set_status(status);
        }

        // A locked exchange-and-add is atomic here as every instruction is:
        // threads interleave whole instructions.
        Mnemonic::Xadd => {
            let bits = cpu.bits(0)?;
            let (destination, source) = (cpu.read(0)?, cpu.read(1)?);
            let (result, status) = flags::add(destination, source, false, bits);
            cpu.write(1, destination)?;
            cpu.write(0, result)?;
            cpu.set_status(status);
        }
        Mnemonic::And | Mnemonic::Or | Mnemonic::Xor | Mnemonic::Test => {
            let bits = cpu.bits(0)?;
            let (a, b) = (cpu.read(0)?, cpu.read(1)?);

            let result = match instruction.mnemonic() {
                Mnemonic::Or => a | b,
                Mnemonic::Xor => a ^ b,
                _ => a & b,
            };

            if instruction.mnemonic() != Mnemonic::Test {
                cpu.write(0, result)?;
            }

            cpu.set_status(flags::logic(result, bits));
        }
        Mnemonic::Inc | Mnemonic::Dec => {
            let bits = cpu.bits(0)?;
            let value = cpu.read(0)?;

            let (result, status) = if instruction.mnemonic() == Mnemonic::Inc {
                flags::add(value, 1, false, bits)
            } else {
                flags::sub(value, 1, false, bits)
            };

            cpu.write(0, result)?;

            // CF is the one status flag these keep.
            cpu.set_status((status & !CARRY) | (cpu.registers.rflags & CARRY));
        }
        Mnemonic::Neg => {
            let bits = cpu.bits(0)?;
            let (result, status) = flags::sub(0, cpu.read(0)?, false, bits);
            cpu.write(0, result)?;
            cpu.set_status(status);
        }
        Mnemonic::Shl | Mnemonic::Shr => {
            let bits = cpu.bits(0)?;
            let count = cpu.shift_count(1, bits)?;
            if count != 0 {
                let value = cpu.read(0)?;
                let (result, status) = if instruction.mnemonic() == Mnemonic::Shr {
                    flags::shift_right(value, count, bits)
                } else {
                    flags::shift_left(value, count, bits)
                };

                cpu.write(0, result)?;
                cpu.set_status(status);
            }
        }
        Mnemonic::Shld => {
            let bits = cpu.bits(0)?;
            let count = cpu.shift_count(2, bits)?;
            if count as usize > bits {
                // The result and flags are undefined.
                return Err(Stop::Unsupported);
            }

            if count != 0 {
                let (destination, source) = (cpu.read(0)?, cpu.read(1)?);
                let (result, status) = flags::shift_left_double(destination, source, count, bits);
                cpu.write(0, result)?;
                cpu.set_status(status);
            }
        }
        Mnemonic::Imul => cpu.signed_multiply()?,
        Mnemonic::Mul => cpu.unsigned_multiply()?,
        Mnemonic::Div => cpu.unsigned_divide()?,
        Mnemonic::Syscall => {
            cpu.registers.general[RCX] = cpu.registers.rip;
            cpu.registers.general[R11] = cpu.registers.rflags;
            return Ok(Outcome::Syscall);
        }
        Mnemonic::Int3 => return Ok(Outcome::Breakpoint),
        Mnemonic::Stosb => cpu.repeated_string(false)?,
        Mnemonic::Movsb => cpu.repeated_string(true)?,
        _ => return Err(Stop::Unsupported),
    }

    Ok(Outcome::Completed)
}

struct Cpu<'a> {
    instruction: &'a Instruction,
    registers: &'a mut Registers,
    memory: &'a mut AddressSpace,
    accesses: &'a Cell<Accesses>,
}

impl Cpu<'_> {
    /// Records a data access that completed.
    fn accessed(&self, address: u64, size: usize, write: bool) {
        let mut accesses = self.accesses.get();
        accesses.push(Access {
            address,
            size: size as u64,
            write,
        });
        self.accesses.set(accesses);
    }

    /// Reads `size` bytes at `address`, zero-extended.
    fn load(&self, address: u64, size: usize) -> Result<u64, Stop> {
        let mut bytes = [0; 8];
        self.memory.read(address, &mut bytes[..size])?;
        self.accessed(address, size, false);
        Ok(u64::from_le_bytes(bytes))
    }

    /// Writes the low `size` bytes of `value` at `address`.
    fn store(&mut self, address: u64, size: usize, value: u64) -> Result<(), Stop> {
        self.memory.write(address, &value.to_le_bytes()[..size])?;
        self.accessed(address, size, true);
        Ok(())
    }

    /// One iteration of `rep stosb` or `rep movsb`, or all of one that has
    /// none left. The CPU traps between iterations, with `rip` still at
    /// the instruction. Only the forms the lockstep executes are
    /// implemented: one without `rep`, with `repne`, or with 32-bit
    /// addresses is another instruction.
    fn repeated_string(&mut self, copies: bool) -> Result<(), Stop> {
        if !self.instruction.has_rep_prefix()
            || self.instruction.has_repne_prefix()
            || self.instruction.op0_kind() != OpKind::MemoryESRDI
        {
            return Err(Stop::Unsupported);
        }

        if self.registers.general[RCX] == 0 {
            return Ok(());
        }

        let delta = if self.registers.rflags & DIRECTION == 0 {
            1
        } else {
            1_u64.wrapping_neg()
        };

        let destination = self.registers.general[RDI];

        let value = if copies {
            let source = self.registers.general[RSI];
            let value = self.load(source, 1)?;
            self.registers.general[RSI] = source.wrapping_add(delta);
            value
        } else {
            self.registers.general[RAX] & 0xff
        };

        self.store(destination, 1, value)?;
        self.registers.general[RDI] = destination.wrapping_add(delta);
        self.registers.general[RCX] -= 1;
        if self.registers.general[RCX] != 0 {
            self.registers.rip = self.instruction.ip();
        }

        Ok(())
    }

    /// The width in bits of an operand.
    fn bits(&self, operand: u32) -> Result<usize, Stop> {
        Ok(match self.instruction.op_kind(operand) {
            OpKind::Register => self.instruction.op_register(operand).size() * 8,
            OpKind::Memory => self.instruction.memory_size().size() * 8,
            OpKind::Immediate8 => 8,
            OpKind::Immediate16 | OpKind::Immediate8to16 => 16,
            OpKind::Immediate32 | OpKind::Immediate8to32 => 32,
            OpKind::Immediate64 | OpKind::Immediate8to64 | OpKind::Immediate32to64 => 64,
            _ => return Err(Stop::Unsupported),
        })
    }

    /// An operand's value, zero-extended from its width.
    fn read(&self, operand: u32) -> Result<u64, Stop> {
        match self.instruction.op_kind(operand) {
            OpKind::Register => self
                .registers
                .read(self.instruction.op_register(operand))
                .ok_or(Stop::Unsupported),
            OpKind::Memory => {
                let address = self.address(operand)?;
                let size = self.instruction.memory_size().size();
                if size > 8 {
                    return Err(Stop::Unsupported);
                }

                self.load(address, size)
            }
            _ => {
                let bits = self.bits(operand)?;
                Ok(self.instruction.immediate(operand) & mask(bits))
            }
        }
    }

    fn write(&mut self, operand: u32, value: u64) -> Result<(), Stop> {
        match self.instruction.op_kind(operand) {
            OpKind::Register => self
                .registers
                .write(self.instruction.op_register(operand), value)
                .ok_or(Stop::Unsupported),
            OpKind::Memory => {
                let address = self.address(operand)?;
                let size = self.instruction.memory_size().size();
                if size > 8 {
                    return Err(Stop::Unsupported);
                }

                self.store(address, size, value)
            }
            _ => Err(Stop::Unsupported),
        }
    }

    /// The address a memory operand names.
    fn address(&self, operand: u32) -> Result<u64, Stop> {
        let registers = &*self.registers;
        self.instruction
            .virtual_address(operand, 0, |register, _, _| match register {
                Register::FS => Some(registers.fs_base),
                Register::GS => Some(registers.gs_base),
                Register::ES | Register::CS | Register::SS | Register::DS => Some(0),
                other => registers.read(other),
            })
            .ok_or(Stop::Unsupported)
    }

    fn branch_target(&self) -> Result<u64, Stop> {
        match self.instruction.op_kind(0) {
            OpKind::NearBranch64 => Ok(self.instruction.near_branch_target()),
            OpKind::Register | OpKind::Memory if self.bits(0)? == 64 => self.read(0),
            _ => Err(Stop::Unsupported),
        }
    }

    fn holds(&self) -> Result<bool, Stop> {
        flags::condition(self.instruction.condition_code(), self.registers.rflags)
            .ok_or(Stop::Unsupported)
    }

    const fn set_status(&mut self, status: u64) {
        self.registers.rflags = with_status(self.registers.rflags, status);
    }

    /// A shift count, masked as the CPU masks it.
    fn shift_count(&self, operand: u32, bits: usize) -> Result<u32, Stop> {
        let limit = if bits == 64 { 0x3f } else { 0x1f };
        Ok(u32::try_from(self.read(operand)? & limit).expect("masked count fits"))
    }

    fn push(&mut self, value: u64) -> Result<(), Stop> {
        let top = self.registers.general[RSP].wrapping_sub(8);
        self.store(top, 8, value)?;
        self.registers.general[RSP] = top;
        Ok(())
    }

    fn pop(&mut self) -> Result<u64, Stop> {
        let top = self.registers.general[RSP];
        let value = self.load(top, 8)?;
        self.registers.general[RSP] = top.wrapping_add(8);
        Ok(value)
    }

    /// `imul` in its one-, two-, and three-operand forms.
    fn signed_multiply(&mut self) -> Result<(), Stop> {
        let bits = self.bits(0)?;
        let signed = |value: u64| i128::from(sign_extend(value, bits).cast_signed());

        let (product, low) = match self.instruction.op_count() {
            1 => {
                if bits != 64 {
                    return Err(Stop::Unsupported);
                }

                let product = signed(self.registers.general[RAX]) * signed(self.read(0)?);
                #[expect(
                    clippy::cast_possible_truncation,
                    clippy::cast_sign_loss,
                    reason = "the halves' bits are wanted"
                )]
                let (low, high) = (product as u64, (product >> 64) as u64);
                self.registers.general[RAX] = low;
                self.registers.general[RDX] = high;
                (product, low)
            }
            count => {
                let (first, second) = if count == 2 { (0, 1) } else { (1, 2) };
                let product = signed(self.read(first)?) * signed(self.read(second)?);
                #[expect(
                    clippy::cast_possible_truncation,
                    clippy::cast_sign_loss,
                    reason = "the low bits are wanted"
                )]
                let low = product as u64 & mask(bits);
                self.write(0, low)?;
                (product, low)
            }
        };

        let fits = i128::from(sign_extend(low, bits).cast_signed()) == product;
        self.set_status(flags::sign_zero_parity(low, bits) | flags::multiply_overflow(!fits));
        Ok(())
    }

    /// One-operand `mul` of RAX, or of the low part of RAX, into RDX:RAX.
    fn unsigned_multiply(&mut self) -> Result<(), Stop> {
        let bits = self.bits(0)?;
        if bits < 16 {
            return Err(Stop::Unsupported);
        }

        let product =
            u128::from(self.registers.general[RAX] & mask(bits)) * u128::from(self.read(0)?);
        #[expect(clippy::cast_possible_truncation, reason = "the halves are wanted")]
        let (low, high) = (
            product as u64 & mask(bits),
            (product >> bits) as u64 & mask(bits),
        );

        self.write_pair(bits, low, high)?;
        self.set_status(flags::sign_zero_parity(low, bits) | flags::multiply_overflow(high != 0));

        Ok(())
    }

    /// One-operand `div` of RDX:RAX, or their low parts, by the operand.
    fn unsigned_divide(&mut self) -> Result<(), Stop> {
        let bits = self.bits(0)?;
        if bits < 16 {
            return Err(Stop::Unsupported);
        }

        let divisor = u128::from(self.read(0)?);
        let dividend = (u128::from(self.registers.general[RDX] & mask(bits)) << bits)
            | u128::from(self.registers.general[RAX] & mask(bits));
        let quotient = dividend.checked_div(divisor);
        let Some(quotient) = quotient.filter(|&quotient| quotient <= u128::from(mask(bits))) else {
            return Err(Stop::Fault(Fault {
                signal: SIGFPE,
                code: FPE_INTDIV,
                address: Some(self.instruction.ip()),
            }));
        };

        #[expect(
            clippy::cast_possible_truncation,
            reason = "both fit the operand width"
        )]
        let (low, high) = (quotient as u64, (dividend % divisor) as u64);

        self.write_pair(bits, low, high)?;
        Ok(())
    }

    /// Writes the RAX and RDX parts of an operand width, as `mul` and `div`
    /// leave them.
    fn write_pair(&mut self, bits: usize, rax: u64, rdx: u64) -> Result<(), Stop> {
        let (low_register, high_register) = match bits {
            16 => (Register::AX, Register::DX),
            32 => (Register::EAX, Register::EDX),
            64 => (Register::RAX, Register::RDX),
            _ => return Err(Stop::Unsupported),
        };

        self.registers
            .write(low_register, rax)
            .and_then(|()| self.registers.write(high_register, rdx))
            .ok_or(Stop::Unsupported)
    }
}
