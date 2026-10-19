//! The x86-64 register file in the forms unwinding and inspection consume.

use std::sync::Arc;

use nix::libc;

use crate::backend::linux::core_dump;
use crate::debug_info::{VariableRegister, VariableRuntimeError};
use crate::unwind::{DwarfRegister, RegisterFile};
use crate::{
    ExecutionContext, RegisterDescriptor, RegisterId, RegisterRole, RegisterSnapshot,
    RegisterValue, UnsupportedVariableFeature, VariableUnavailableReason,
};

/// The general registers: DWARF number, name, and role, indexed by
/// [`RegisterId`] in the order snapshots present them.
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

/// The id of the register at `index` in one of this module's tables, which
/// have a few dozen entries at most.
#[expect(
    clippy::cast_possible_truncation,
    reason = "register tables are far shorter than u32::MAX"
)]
const fn table_id(index: usize) -> u32 {
    index as u32
}

fn general_descriptor(index: usize) -> RegisterDescriptor {
    let (_, name, role) = GENERAL_REGISTERS[index];
    RegisterDescriptor {
        id: RegisterId::new(table_id(index)),
        name: name.into(),
        bits: 64,
        role,
    }
}

fn general_index(dwarf: DwarfRegister) -> Option<usize> {
    GENERAL_REGISTERS
        .iter()
        .position(|(number, ..)| *number == dwarf)
}

/// The segment, thread-pointer, and system-call registers, after the
/// general ones: each name, width, and whether every frame shares it.
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

/// Every general register's value, indexed as [`GENERAL_REGISTERS`].
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

/// Whether a stopped thread is inside an interrupted system call that the
/// kernel restarts, executing `syscall` again, when the thread next runs
/// with no signal to handle.
pub(super) const fn restarts_call(registers: &libc::user_regs_struct) -> bool {
    const ERESTARTSYS: i64 = 512;
    const ERESTARTNOINTR: i64 = 513;
    const ERESTARTNOHAND: i64 = 514;
    const ERESTART_RESTARTBLOCK: i64 = 516;
    registers.orig_rax.cast_signed() >= 0
        && matches!(
            registers.rax.cast_signed().checked_neg(),
            Some(ERESTARTSYS | ERESTARTNOINTR | ERESTARTNOHAND | ERESTART_RESTARTBLOCK)
        )
}

pub(super) fn x86_64_registers(registers: &libc::user_regs_struct) -> RegisterFile {
    RegisterFile::new(
        GENERAL_REGISTERS
            .iter()
            .zip(general_values(registers))
            .map(|((dwarf, ..), value)| (*dwarf, value)),
    )
}

pub(super) fn x86_64_general_variable_register(
    registers: &libc::user_regs_struct,
    dwarf: DwarfRegister,
) -> Option<VariableRegister> {
    let index = general_index(dwarf)?;
    Some(VariableRegister {
        descriptor: general_descriptor(index),
        bytes: Arc::from(general_values(registers)[index].to_le_bytes()),
    })
}

/// Reads a register of a caller's activation from the registers the
/// unwinder reconstructed for it.
pub(super) fn x86_64_caller_variable_register(
    registers: &RegisterFile,
    dwarf: DwarfRegister,
) -> Result<VariableRegister, VariableRuntimeError> {
    if let Some(index) = general_index(dwarf) {
        let descriptor = general_descriptor(index);
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
        // Callees may overwrite every SSE register without saving it.
        return Err(VariableUnavailableReason::RegisterNotSaved(
            format!("xmm{}", number - 17).into(),
        )
        .into());
    }

    Err(VariableUnavailableReason::Unsupported(UnsupportedVariableFeature::RegisterClass).into())
}

/// The id of the program counter's descriptor.
pub(super) const PROGRAM_COUNTER: u32 = 16;

pub(super) const fn is_program_counter(register: RegisterId) -> bool {
    register.get() == PROGRAM_COUNTER
}

/// The field of any register a snapshot presents, by its descriptor's id:
/// a general register, or one of [`SPECIAL_REGISTERS`] after them.
pub(super) const fn x86_64_register_slot(
    registers: &mut libc::user_regs_struct,
    register: RegisterId,
) -> Option<&mut u64> {
    Some(match register.get() {
        18 => &mut registers.cs,
        19 => &mut registers.ss,
        20 => &mut registers.ds,
        21 => &mut registers.es,
        22 => &mut registers.fs,
        23 => &mut registers.gs,
        24 => &mut registers.fs_base,
        25 => &mut registers.gs_base,
        26 => &mut registers.orig_rax,
        _ => return x86_64_general_register_slot(registers, register),
    })
}

/// The field of a general register, by its descriptor's id.
pub(super) const fn x86_64_general_register_slot(
    registers: &mut libc::user_regs_struct,
    register: RegisterId,
) -> Option<&mut u64> {
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
        _ => return None,
    })
}

/// The 512-byte x86-64 FXSAVE image saved by ptrace and by `NT_FPREGSET`.
pub(super) type Fxsave = Arc<[u8; core_dump::FXSAVE_SIZE]>;

const FXSAVE_X87_OFFSET: usize = 32;
const FXSAVE_XMM_OFFSET: usize = 160;

pub(super) fn x86_64_xmm_variable_register(
    registers: &Fxsave,
    dwarf: DwarfRegister,
) -> VariableRegister {
    let index = usize::from(dwarf.number() - 17);
    let start = FXSAVE_XMM_OFFSET + index * 16;
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

/// One of the x87 registers st0 through st7, DWARF's 33 through 40, which
/// FXSAVE stores from the stack's top, each in the low ten bytes of
/// sixteen.
pub(super) fn x86_64_x87_variable_register(
    registers: &Fxsave,
    dwarf: DwarfRegister,
) -> VariableRegister {
    let index = usize::from(dwarf.number() - 33);
    let start = FXSAVE_X87_OFFSET + index * 16;
    VariableRegister {
        descriptor: RegisterDescriptor {
            id: RegisterId::new(43 + u32::from(dwarf.number() - 33)),
            name: format!("st{index}").into(),
            bits: 80,
            role: None,
        },
        bytes: registers[start..start + 10].to_vec().into(),
    }
}

/// Describes a frame's general registers: the thread's own for the
/// innermost activation, or the registers the unwinder reconstructed for a
/// caller's. A caller's register that a callee may have overwritten without
/// saving it has no value; the segment and thread-pointer registers are the
/// same in every frame, and the system-call register belongs to the thread.
/// A parked task has no thread, so only the registers its runtime saved,
/// given as `caller`, have values.
pub(super) fn x86_64_register_snapshot(
    revision: u64,
    context: ExecutionContext,
    target: crate::TargetDescription,
    native: Option<&libc::user_regs_struct>,
    caller: Option<&RegisterFile>,
) -> RegisterSnapshot {
    let general = native.map(general_values);
    let registers = (0..GENERAL_REGISTERS.len())
        .map(|index| RegisterValue {
            register: general_descriptor(index),
            bytes: caller
                .map_or_else(
                    || general.map(|values| values[index]),
                    |file| file.get(GENERAL_REGISTERS[index].0),
                )
                .map(|value| Arc::from(value.to_le_bytes())),
        })
        .chain(SPECIAL_REGISTERS.into_iter().enumerate().map(
            |(offset, (name, bits, every_frame))| {
                RegisterValue {
                    register: RegisterDescriptor {
                        id: RegisterId::new(18 + table_id(offset)),
                        name: name.into(),
                        bits,
                        role: None,
                    },
                    bytes: native
                        .filter(|_| caller.is_none() || every_frame)
                        .map(|native| {
                            let value = special_values(native)[offset];
                            Arc::from(&value.to_le_bytes()[..usize::from(bits / 8)])
                        }),
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
