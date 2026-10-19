//! The seccomp filter the ptrace engine installs in a program it launches.
//!
//! `SECCOMP_RET_TRACE` for the chosen calls, which stop for the tracer,
//! and `SECCOMP_RET_ALLOW` for every other, which runs at full speed.

use crate::syscalls::table::X32_SYSCALL_BIT;
use crate::syscalls::{Abi, CallSet};

/// One classic BPF instruction, laid out as the kernel's `sock_filter`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Instruction {
    pub code: u16,
    pub jt: u8,
    pub jf: u8,
    pub k: u32,
}

const LOAD_WORD: u16 = 0x20; // BPF_LD | BPF_W | BPF_ABS
const JUMP: u16 = 0x05; // BPF_JMP | BPF_JA
const JUMP_EQUAL: u16 = 0x15; // BPF_JMP | BPF_JEQ | BPF_K
const JUMP_SET: u16 = 0x45; // BPF_JMP | BPF_JSET | BPF_K
const RETURN: u16 = 0x06; // BPF_RET | BPF_K

pub const RET_ALLOW: u32 = 0x7fff_0000;

pub const RET_TRACE: u32 = 0x7ff0_0000;
const AUDIT_ARCH_X86_64: u32 = 0xc000_003e;
const AUDIT_ARCH_I386: u32 = 0x4000_0003;

/// `seccomp_data`'s fields.
const NUMBER: u32 = 0;
const ARCH: u32 = 4;

const fn op(code: u16, k: u32) -> Instruction {
    Instruction {
        code,
        jt: 0,
        jf: 0,
        k,
    }
}

/// `ret TRACE` for each number, after `jeq number`: two instructions each,
/// so no jump reaches further than the next instruction.
fn traced(program: &mut Vec<Instruction>, numbers: impl Iterator<Item = u32>) {
    for number in numbers {
        program.push(Instruction {
            code: JUMP_EQUAL,
            jt: 0,
            jf: 1,
            k: number,
        });
        program.push(op(RETURN, RET_TRACE));
    }

    program.push(op(RETURN, RET_ALLOW));
}

/// A filter that stops at exactly `calls`.
#[must_use]
pub fn filter(calls: &CallSet) -> Vec<Instruction> {
    if calls.is_all() {
        return vec![op(RETURN, RET_TRACE)];
    }

    let mut x86_64 = vec![op(LOAD_WORD, NUMBER)];
    let mut x32 = Vec::new();
    traced(
        &mut x32,
        calls
            .numbers(Abi::X32)
            .map(|number| number | X32_SYSCALL_BIT),
    );

    // An x32 call sets a bit of its number; the jump past the 64-bit calls
    // lands on the x32 ones.
    x86_64.push(Instruction {
        code: JUMP_SET,
        jt: 0,
        jf: 1,
        k: X32_SYSCALL_BIT,
    });
    let mut sixty_four = Vec::new();
    traced(&mut sixty_four, calls.numbers(Abi::X86_64));
    #[expect(
        clippy::cast_possible_truncation,
        reason = "filters are far shorter than 4 GiB"
    )]
    x86_64.push(op(JUMP, sixty_four.len() as u32));
    x86_64.extend(sixty_four);
    x86_64.extend(x32);
    let mut i386 = vec![op(LOAD_WORD, NUMBER)];
    traced(&mut i386, calls.numbers(Abi::X86_32));

    // The architecture decides which list applies; any other is allowed.
    let mut program = vec![
        op(LOAD_WORD, ARCH),
        Instruction {
            code: JUMP_EQUAL,
            jt: 0,
            jf: 1,
            k: AUDIT_ARCH_X86_64,
        },
        op(JUMP, 3),
        Instruction {
            code: JUMP_EQUAL,
            jt: 0,
            jf: 1,
            k: AUDIT_ARCH_I386,
        },
    ];
    #[expect(
        clippy::cast_possible_truncation,
        reason = "filters are far shorter than 4 GiB"
    )]
    program.push(op(JUMP, 1 + x86_64.len() as u32));
    program.push(op(RETURN, RET_ALLOW));
    program.extend(x86_64);
    program.extend(i386);

    program
}

/// Runs a filter over a call, as the kernel would: what it returns, or
/// `None` for a program the kernel would reject.
#[must_use]
pub fn run(program: &[Instruction], arch: u32, number: u32) -> Option<u32> {
    let mut accumulator = 0;
    let mut at = 0;
    loop {
        let instruction = program.get(at)?;
        at += 1;
        match instruction.code {
            LOAD_WORD => {
                accumulator = match instruction.k {
                    NUMBER => number,
                    ARCH => arch,
                    _ => return None,
                };
            }
            JUMP => at += instruction.k as usize,
            JUMP_EQUAL | JUMP_SET => {
                let taken = if instruction.code == JUMP_EQUAL {
                    accumulator == instruction.k
                } else {
                    accumulator & instruction.k != 0
                };

                at += usize::from(if taken {
                    instruction.jt
                } else {
                    instruction.jf
                });
            }
            RETURN => return Some(instruction.k),
            _ => return None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::syscalls::{Filter, SyscallId};

    fn calls(expressions: &[&str]) -> CallSet {
        let expressions: Vec<String> = expressions.iter().map(|&text| text.to_owned()).collect();
        Filter::parse(&expressions, &|_| None)
            .expect("parses")
            .calls
    }

    #[test]
    fn the_filter_traces_exactly_the_chosen_calls_in_every_abi() {
        let chosen = calls(&["trace=openat,connect,%process"]);
        let program = filter(&chosen);
        assert!(program.len() < 4096, "the kernel's limit");
        for abi in Abi::ALL {
            for &(number, _) in crate::syscalls::table::calls(abi) {
                let id = SyscallId::new(abi, number);
                let (arch, raw) = match abi {
                    Abi::X86_64 => (AUDIT_ARCH_X86_64, number),
                    Abi::X32 => (AUDIT_ARCH_X86_64, number | X32_SYSCALL_BIT),
                    Abi::X86_32 => (AUDIT_ARCH_I386, number),
                };

                let expected = if chosen.contains(id) {
                    RET_TRACE
                } else {
                    RET_ALLOW
                };

                assert_eq!(
                    run(&program, arch, raw),
                    Some(expected),
                    "{id:?} {:?}",
                    id.name()
                );
            }
        }

        // Another architecture's calls run.
        assert_eq!(run(&program, 0xdead, 257), Some(RET_ALLOW));
        assert_eq!(filter(&calls(&["trace=all"])), [op(RETURN, RET_TRACE)]);
        let none = filter(&calls(&["trace=none"]));
        assert_eq!(run(&none, AUDIT_ARCH_X86_64, 0), Some(RET_ALLOW));
    }
}
