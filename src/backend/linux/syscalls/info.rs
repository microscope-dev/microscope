use nix::errno::Errno;
use nix::libc;
use nix::unistd::Pid;

use crate::syscalls::table::X32_SYSCALL_BIT;
use crate::syscalls::{Abi, SyscallId};

pub const OP_ENTRY: u8 = 1;

pub const OP_EXIT: u8 = 2;

pub const OP_SECCOMP: u8 = 3;

pub const AUDIT_ARCH_X86_64: u32 = 0xc000_003e;

#[derive(Debug, Clone, Copy)]
pub struct SyscallInfo {
    pub op: u8,
    pub arch: u32,

    pub number: u64,
    pub args: [u64; 6],

    pub value: i64,
}

pub fn syscall_info(pid: Pid) -> Result<SyscallInfo, Errno> {
    let buffer = super::super::sys::syscall_info(pid)?;
    let word = |at: usize| {
        buffer
            .get(at..at + 8)
            .and_then(|bytes| bytes.try_into().ok())
            .map_or(0, u64::from_le_bytes)
    };
    let mut args = [0; 6];
    for (index, arg) in args.iter_mut().enumerate() {
        *arg = word(32 + index * 8);
    }

    Ok(SyscallInfo {
        op: buffer[0],
        arch: u32::from_le_bytes(buffer[4..8].try_into().unwrap_or_default()),
        number: word(24),
        args,
        value: word(24).cast_signed(),
    })
}

#[must_use]
pub fn from_registers(arch: u32, registers: &libc::user_regs_struct) -> (SyscallId, [u64; 6]) {
    let r = registers;
    let info = SyscallInfo {
        op: OP_EXIT,
        arch,
        number: r.orig_rax,
        args: if arch == AUDIT_ARCH_X86_64 {
            [r.rdi, r.rsi, r.rdx, r.r10, r.r8, r.r9]
        } else {
            [r.rbx, r.rcx, r.rdx, r.rsi, r.rdi, r.rbp].map(|word| word & 0xffff_ffff)
        },
        value: 0,
    };

    (call_id(&info), info.args)
}

#[must_use]
pub const fn call_id(info: &SyscallInfo) -> SyscallId {
    #[expect(clippy::cast_possible_truncation, reason = "call numbers are small")]
    let number = info.number as u32;
    if info.arch == AUDIT_ARCH_X86_64 {
        if number & X32_SYSCALL_BIT != 0 {
            SyscallId::new(Abi::X32, number & !X32_SYSCALL_BIT)
        } else {
            SyscallId::new(Abi::X86_64, number)
        }
    } else {
        SyscallId::new(Abi::X86_32, number)
    }
}
