use std::sync::Arc;

use nix::libc;

use crate::debug_info::{VariableRegister, VariableRuntimeError};
use crate::unwind::x86_64::{FS_BASE, GS_BASE, RAX};
use crate::unwind::{DwarfRegister, RegisterFile};
use crate::{
    ExecutionContext, RegisterDescriptor, RegisterId, RegisterRole, RegisterSnapshot,
    RegisterValue, UnsupportedVariableFeature, VariableUnavailableReason, VirtualAddress,
};

pub(in crate::backend::linux) const AUDIT_ARCH_X86_64: u32 = 0xc000_003e;

pub(in crate::backend::linux) const NATIVE_AUDIT_ARCH: u32 = AUDIT_ARCH_X86_64;

const AUDIT_ARCH_I386: u32 = 0x4000_0003;

pub(in crate::backend::linux) const ELF_MACHINE: u16 = object::elf::EM_X86_64;

pub(in crate::backend::linux) const PRSTATUS_SIZE: usize = 336;

pub(in crate::backend::linux) const GENERAL_REGISTER_COUNT: usize = 27;

pub(in crate::backend::linux) const BREAKPOINT_OPCODE: u8 = 0xcc;

const BREAKPOINT_LENGTH: u64 = 1;

pub(in crate::backend::linux) const SYSCALL_INSTRUCTION: [u8; 2] = [0x0f, 0x05];

const RETURN_ADDRESS_BYTES: u64 = 8;

pub(in crate::backend::linux) const fn stack_after_return(entry_stack: u64) -> u64 {
    entry_stack.wrapping_add(RETURN_ADDRESS_BYTES)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::backend::linux) struct PendingReturn {
    pub returns_to: VirtualAddress,
    pub stack: u64,
}

#[derive(Clone, Copy)]
pub(in crate::backend::linux) struct Registers(libc::user_regs_struct);

impl std::fmt::Debug for Registers {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Registers")
            .field("rip", &format_args!("{:#x}", self.0.rip))
            .field("rsp", &format_args!("{:#x}", self.0.rsp))
            .finish_non_exhaustive()
    }
}

impl Registers {
    pub(in crate::backend::linux) const fn from_native(native: libc::user_regs_struct) -> Self {
        Self(native)
    }

    pub(in crate::backend::linux) const fn from_general_registers(
        values: [u64; GENERAL_REGISTER_COUNT],
    ) -> Self {
        let [
            r15,
            r14,
            r13,
            r12,
            rbp,
            rbx,
            r11,
            r10,
            r9,
            r8,
            rax,
            rcx,
            rdx,
            rsi,
            rdi,
            orig_rax,
            rip,
            cs,
            eflags,
            rsp,
            ss,
            fs_base,
            gs_base,
            ds,
            es,
            fs,
            gs,
        ] = values;
        Self(libc::user_regs_struct {
            r15,
            r14,
            r13,
            r12,
            rbp,
            rbx,
            r11,
            r10,
            r9,
            r8,
            rax,
            rcx,
            rdx,
            rsi,
            rdi,
            orig_rax,
            rip,
            cs,
            eflags,
            rsp,
            ss,
            fs_base,
            gs_base,
            ds,
            es,
            fs,
            gs,
        })
    }

    #[cfg(test)]
    pub(in crate::backend::linux) const fn general_registers(
        &self,
    ) -> [u64; GENERAL_REGISTER_COUNT] {
        let r = &self.0;
        [
            r.r15, r.r14, r.r13, r.r12, r.rbp, r.rbx, r.r11, r.r10, r.r9, r.r8, r.rax, r.rcx,
            r.rdx, r.rsi, r.rdi, r.orig_rax, r.rip, r.cs, r.eflags, r.rsp, r.ss, r.fs_base,
            r.gs_base, r.ds, r.es, r.fs, r.gs,
        ]
    }

    pub(in crate::backend::linux) const fn native(&self) -> &libc::user_regs_struct {
        &self.0
    }

    pub(in crate::backend::linux) const fn program_counter(&self) -> VirtualAddress {
        VirtualAddress::new(self.0.rip)
    }

    pub(in crate::backend::linux) const fn set_program_counter(&mut self, address: VirtualAddress) {
        self.0.rip = address.get();
    }

    pub(in crate::backend::linux) const fn stack_pointer(&self) -> u64 {
        self.0.rsp
    }

    #[cfg(test)]
    pub(in crate::backend::linux) const fn set_stack_pointer(&mut self, value: u64) {
        self.0.rsp = value;
    }

    pub(in crate::backend::linux) const fn frame_pointer(&self) -> u64 {
        self.0.rbp
    }

    pub(in crate::backend::linux) const fn thread_pointer(&self) -> u64 {
        self.0.fs_base
    }

    pub(in crate::backend::linux) const fn set_thread_pointer(&mut self, value: u64) {
        self.0.fs_base = value;
    }

    pub(in crate::backend::linux) const fn arguments(&self) -> [u64; 6] {
        let r = &self.0;
        [r.rdi, r.rsi, r.rdx, r.rcx, r.r8, r.r9]
    }

    pub(in crate::backend::linux) const fn set_first_argument(&mut self, value: u64) {
        self.0.rdi = value;
    }

    pub(in crate::backend::linux) const fn return_value(&self) -> u64 {
        self.0.rax
    }

    pub(in crate::backend::linux) fn breakpoint_address(&self) -> Option<VirtualAddress> {
        self.0
            .rip
            .checked_sub(BREAKPOINT_LENGTH)
            .map(VirtualAddress::new)
    }

    pub(in crate::backend::linux) fn pending_return(
        &self,
        read_word: impl FnOnce(u64) -> crate::Result<u64>,
    ) -> crate::Result<PendingReturn> {
        Ok(PendingReturn {
            returns_to: VirtualAddress::new(read_word(self.0.rsp)?),
            stack: stack_after_return(self.0.rsp),
        })
    }

    pub(in crate::backend::linux) const fn return_now(
        &mut self,
        pending: PendingReturn,
        value: u64,
    ) {
        self.0.rax = value;
        self.0.rip = pending.returns_to.get();
        self.0.rsp = pending.stack;
    }

    pub(in crate::backend::linux) const fn syscall_number(&self) -> u64 {
        self.0.orig_rax
    }

    /// Identifies 32-bit compat syscalls (`AUDIT_ARCH_I386`) from the `__USER32_CS` segment (`0x23`).
    pub(in crate::backend::linux) const fn syscall_audit_arch(&self) -> u32 {
        const COMPAT_CODE_SEGMENT: u64 = 0x23;
        if self.0.cs == COMPAT_CODE_SEGMENT {
            AUDIT_ARCH_I386
        } else {
            AUDIT_ARCH_X86_64
        }
    }

    pub(in crate::backend::linux) const fn syscall_arguments(&self, audit_arch: u32) -> [u64; 6] {
        let r = &self.0;
        if audit_arch == AUDIT_ARCH_X86_64 {
            [r.rdi, r.rsi, r.rdx, r.r10, r.r8, r.r9]
        } else {
            let mask = 0xffff_ffff;
            [
                r.rbx & mask,
                r.rcx & mask,
                r.rdx & mask,
                r.rsi & mask,
                r.rdi & mask,
                r.rbp & mask,
            ]
        }
    }

    pub(in crate::backend::linux) const fn syscall_result(&self) -> u64 {
        self.0.rax
    }

    /// Clears `orig_rax` to `-1` so the kernel will not auto-restart an interrupted syscall on resume.
    pub(in crate::backend::linux) const fn cancel_syscall_restart(&mut self) {
        self.0.orig_rax = u64::MAX;
    }

    /// `orig_rax` is set to `-1` so a signal arriving before entry cannot trigger syscall restart.
    pub(in crate::backend::linux) const fn prepare_syscall(
        &mut self,
        number: libc::c_long,
        arguments: [u64; 6],
    ) {
        self.0.rax = number.cast_unsigned();
        self.0.orig_rax = u64::MAX;
        [
            self.0.rdi, self.0.rsi, self.0.rdx, self.0.r10, self.0.r8, self.0.r9,
        ] = arguments;
    }

    pub(in crate::backend::linux) const fn after_syscall_instruction(
        &self,
        at: VirtualAddress,
    ) -> bool {
        self.0.rip == at.get().wrapping_add(SYSCALL_INSTRUCTION.len() as u64)
    }

    /// Returns true if the kernel stopped inside a syscall that will rewind `rip` and restart on resume.
    pub(in crate::backend::linux) const fn restarts_call(&self) -> bool {
        const ERESTARTSYS: i64 = 512;
        const ERESTARTNOINTR: i64 = 513;
        const ERESTARTNOHAND: i64 = 514;
        const ERESTART_RESTARTBLOCK: i64 = 516;
        self.0.orig_rax.cast_signed() >= 0
            && matches!(
                self.0.rax.cast_signed().checked_neg(),
                Some(ERESTARTSYS | ERESTARTNOINTR | ERESTARTNOHAND | ERESTART_RESTARTBLOCK)
            )
    }

    pub(in crate::backend::linux) fn register_file(&self) -> RegisterFile {
        RegisterFile::new(
            GENERAL_REGISTERS
                .iter()
                .zip(general_values(&self.0))
                .map(|((dwarf, ..), value)| (*dwarf, value)),
        )
    }

    pub(in crate::backend::linux) fn next_instruction_file(&self) -> RegisterFile {
        let mut registers = self.register_file();
        registers.set(FS_BASE, self.0.fs_base);
        registers.set(GS_BASE, self.0.gs_base);
        if self.restarts_call() {
            // The kernel has not finalized the return value yet and will clobber `rax` on restart.
            registers.remove(RAX);
        }

        registers
    }

    pub(in crate::backend::linux) fn variable_register(
        &self,
        dwarf: DwarfRegister,
    ) -> Option<VariableRegister> {
        let (index, (register, value)) = GENERAL_REGISTERS
            .iter()
            .zip(general_values(&self.0))
            .enumerate()
            .find(|(_, ((number, ..), _))| *number == dwarf)?;
        Some(VariableRegister {
            descriptor: general_descriptor(index, register),
            bytes: Arc::from(value.to_le_bytes()),
        })
    }

    pub(in crate::backend::linux) const fn slot(
        &mut self,
        register: RegisterId,
    ) -> Option<&mut u64> {
        let registers = &mut self.0;
        Some(match register.get() {
            0 => &mut registers.rax,
            1 => &mut registers.rbx,
            2 => &mut registers.rcx,
            3 => &mut registers.rdx,
            4 => &mut registers.rsi,
            5 => &mut registers.rdi,
            6 => &mut registers.rbp,
            7 => &mut registers.rsp,
            8 => &mut registers.r8,
            9 => &mut registers.r9,
            10 => &mut registers.r10,
            11 => &mut registers.r11,
            12 => &mut registers.r12,
            13 => &mut registers.r13,
            14 => &mut registers.r14,
            15 => &mut registers.r15,
            16 => &mut registers.rip,
            17 => &mut registers.eflags,
            18 => &mut registers.cs,
            19 => &mut registers.ss,
            20 => &mut registers.ds,
            21 => &mut registers.es,
            22 => &mut registers.fs,
            23 => &mut registers.gs,
            24 => &mut registers.fs_base,
            25 => &mut registers.gs_base,
            26 => &mut registers.orig_rax,
            _ => return None,
        })
    }

    pub(in crate::backend::linux) fn snapshot(
        registers: Option<&Self>,
        revision: u64,
        context: ExecutionContext,
        target: crate::TargetDescription,
        caller: Option<&RegisterFile>,
    ) -> RegisterSnapshot {
        register_snapshot(
            revision,
            context,
            target,
            registers.map(|registers| &registers.0),
            caller,
        )
    }
}

const GENERAL_REGISTERS: [(DwarfRegister, &str, Option<RegisterRole>); 18] = [
    (DwarfRegister::new(0), "rax", None),
    (DwarfRegister::new(3), "rbx", None),
    (DwarfRegister::new(2), "rcx", None),
    (DwarfRegister::new(1), "rdx", None),
    (DwarfRegister::new(4), "rsi", None),
    (DwarfRegister::new(5), "rdi", None),
    (
        DwarfRegister::new(6),
        "rbp",
        Some(RegisterRole::FramePointer),
    ),
    (
        DwarfRegister::new(7),
        "rsp",
        Some(RegisterRole::StackPointer),
    ),
    (DwarfRegister::new(8), "r8", None),
    (DwarfRegister::new(9), "r9", None),
    (DwarfRegister::new(10), "r10", None),
    (DwarfRegister::new(11), "r11", None),
    (DwarfRegister::new(12), "r12", None),
    (DwarfRegister::new(13), "r13", None),
    (DwarfRegister::new(14), "r14", None),
    (DwarfRegister::new(15), "r15", None),
    (
        DwarfRegister::new(16),
        "rip",
        Some(RegisterRole::ProgramCounter),
    ),
    (DwarfRegister::new(49), "rflags", None),
];

#[expect(
    clippy::cast_possible_truncation,
    reason = "register tables are far shorter than u32::MAX"
)]
const fn table_id(index: usize) -> u32 {
    index as u32
}

type GeneralRegister = (DwarfRegister, &'static str, Option<RegisterRole>);

fn general_descriptor(index: usize, &(_, name, role): &GeneralRegister) -> RegisterDescriptor {
    RegisterDescriptor {
        id: RegisterId::new(table_id(index)),
        name: name.into(),
        bits: 64,
        role,
    }
}

fn general_descriptor_of(dwarf: DwarfRegister) -> Option<RegisterDescriptor> {
    GENERAL_REGISTERS
        .iter()
        .enumerate()
        .find(|(_, (number, ..))| *number == dwarf)
        .map(|(index, register)| general_descriptor(index, register))
}

const SPECIAL_REGISTERS: [(&str, u16, bool); 9] = [
    ("cs", 16, true),
    ("ss", 16, true),
    ("ds", 16, true),
    ("es", 16, true),
    ("fs", 16, true),
    ("gs", 16, true),
    ("fs_base", 64, true),
    ("gs_base", 64, true),
    ("orig_rax", 64, false),
];

const fn special_values(native: &libc::user_regs_struct) -> [u64; 9] {
    [
        native.cs,
        native.ss,
        native.ds,
        native.es,
        native.fs,
        native.gs,
        native.fs_base,
        native.gs_base,
        native.orig_rax,
    ]
}

const fn general_values(registers: &libc::user_regs_struct) -> [u64; 18] {
    [
        registers.rax,
        registers.rbx,
        registers.rcx,
        registers.rdx,
        registers.rsi,
        registers.rdi,
        registers.rbp,
        registers.rsp,
        registers.r8,
        registers.r9,
        registers.r10,
        registers.r11,
        registers.r12,
        registers.r13,
        registers.r14,
        registers.r15,
        registers.rip,
        registers.eflags,
    ]
}

pub(in crate::backend::linux) fn caller_variable_register(
    registers: &RegisterFile,
    dwarf: DwarfRegister,
) -> Result<VariableRegister, VariableRuntimeError> {
    if let Some(descriptor) = general_descriptor_of(dwarf) {
        return match registers.get(dwarf) {
            Some(value) => Ok(VariableRegister {
                descriptor,
                bytes: Arc::from(value.to_le_bytes()),
            }),
            None => Err(VariableUnavailableReason::RegisterNotSaved(descriptor.name).into()),
        };
    }

    let number = dwarf.number();
    if (17..=32).contains(&number) {
        return Err(VariableUnavailableReason::RegisterNotSaved(
            format!("xmm{}", number - 17).into(),
        )
        .into());
    }

    Err(VariableUnavailableReason::Unsupported(UnsupportedVariableFeature::RegisterClass).into())
}

pub(in crate::backend::linux) const PROGRAM_COUNTER: u32 = 16;

pub(in crate::backend::linux) const fn is_program_counter(register: RegisterId) -> bool {
    register.get() == PROGRAM_COUNTER
}

pub(in crate::backend::linux) const FLOATING_REGISTERS_SIZE: usize = 512;

/// Raw 512-byte `FXSAVE` area returned by `PTRACE_GETFPREGS`.
pub(in crate::backend::linux) type FloatingRegisters = Arc<[u8; FLOATING_REGISTERS_SIZE]>;

const FXSAVE_X87_OFFSET: usize = 32;
const FXSAVE_XMM_OFFSET: usize = 160;

pub(in crate::backend::linux) const fn is_floating_register(dwarf: DwarfRegister) -> bool {
    matches!(dwarf.number(), 17..=40)
}

pub(in crate::backend::linux) fn floating_variable_register(
    registers: &FloatingRegisters,
    dwarf: DwarfRegister,
) -> VariableRegister {
    if dwarf.number() <= 32 {
        xmm_variable_register(registers, dwarf)
    } else {
        x87_variable_register(registers, dwarf)
    }
}

fn xmm_variable_register(registers: &FloatingRegisters, dwarf: DwarfRegister) -> VariableRegister {
    debug_assert!(
        (17..=32).contains(&dwarf.number()),
        "DWARF register {dwarf} is not an SSE register"
    );
    let index = usize::from(dwarf.number() - 17);
    let start = FXSAVE_XMM_OFFSET + index * 16;
    #[expect(
        clippy::indexing_slicing,
        reason = "callers pass xmm0 through xmm15, which end at byte 416 of 512"
    )]
    let bytes = registers[start..start + 16].to_vec();
    VariableRegister {
        descriptor: RegisterDescriptor {
            id: RegisterId::new(27 + u32::from(dwarf.number() - 17)),
            name: format!("xmm{index}").into(),
            bits: 128,
            role: None,
        },
        bytes: bytes.into(),
    }
}

fn x87_variable_register(registers: &FloatingRegisters, dwarf: DwarfRegister) -> VariableRegister {
    debug_assert!(
        (33..=40).contains(&dwarf.number()),
        "DWARF register {dwarf} is not an x87 register"
    );
    let index = usize::from(dwarf.number() - 33);
    let start = FXSAVE_X87_OFFSET + index * 16;
    #[expect(
        clippy::indexing_slicing,
        reason = "callers pass st0 through st7, which end at byte 154 of 512"
    )]
    let bytes = registers[start..start + 10].to_vec();
    VariableRegister {
        descriptor: RegisterDescriptor {
            id: RegisterId::new(43 + u32::from(dwarf.number() - 33)),
            name: format!("st{index}").into(),
            bits: 80,
            role: None,
        },
        bytes: bytes.into(),
    }
}

#[expect(
    clippy::indexing_slicing,
    reason = "register tables hold widths of 16 or 64 bits"
)]
fn low_bytes(value: u64, bits: u16) -> Arc<[u8]> {
    debug_assert!(
        bits.is_multiple_of(8) && bits <= 64,
        "a register is {bits} bits wide"
    );
    Arc::from(&value.to_le_bytes()[..usize::from(bits / 8)])
}

fn register_snapshot(
    revision: u64,
    context: ExecutionContext,
    target: crate::TargetDescription,
    native: Option<&libc::user_regs_struct>,
    caller: Option<&RegisterFile>,
) -> RegisterSnapshot {
    let general = native.map_or([None; GENERAL_REGISTERS.len()], |native| {
        general_values(native).map(Some)
    });
    let special = native.map_or([None; SPECIAL_REGISTERS.len()], |native| {
        special_values(native).map(Some)
    });
    let registers = GENERAL_REGISTERS
        .iter()
        .zip(general)
        .enumerate()
        .map(|(index, (register, value))| RegisterValue {
            register: general_descriptor(index, register),
            bytes: caller
                .map_or(value, |file| file.get(register.0))
                .map(|value| Arc::from(value.to_le_bytes())),
        })
        .chain(SPECIAL_REGISTERS.into_iter().zip(special).enumerate().map(
            |(offset, ((name, bits, every_frame), value))| {
                RegisterValue {
                    register: RegisterDescriptor {
                        id: RegisterId::new(18 + table_id(offset)),
                        name: name.into(),
                        bits,
                        role: None,
                    },
                    bytes: value
                        .filter(|_| caller.is_none() || every_frame)
                        .map(|value| low_bytes(value, bits)),
                }
            },
        ))
        .collect::<Vec<_>>()
        .into();

    RegisterSnapshot {
        revision,
        context,
        target,
        registers,
    }
}
