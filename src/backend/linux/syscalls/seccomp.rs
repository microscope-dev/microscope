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
    use proptest::prelude::*;

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

    /// The rules the kernel's `bpf_check_classic` and
    /// `seccomp_check_filter` hold a program to, apart from `run`: at
    /// most `BPF_MAXINSNS` instructions, only the codes seccomp allows,
    /// loads of aligned `seccomp_data` words, every jump landing inside
    /// the program, and a return last.
    fn check_structure(program: &[Instruction]) -> Result<(), TestCaseError> {
        prop_assert!(
            !program.is_empty() && program.len() <= 4096,
            "{} instructions",
            program.len()
        );

        for (at, instruction) in program.iter().enumerate() {
            let lands = |offset: usize| at + 1 + offset < program.len();
            match instruction.code {
                LOAD_WORD => prop_assert!(instruction.k % 4 == 0 && instruction.k < 64),
                JUMP => prop_assert!(
                    lands(instruction.k as usize),
                    "ja {} at {at}",
                    instruction.k
                ),
                JUMP_EQUAL | JUMP_SET => prop_assert!(
                    lands(usize::from(instruction.jt)) && lands(usize::from(instruction.jf)),
                    "{instruction:?} at {at}"
                ),
                RETURN => {}
                code => prop_assert!(false, "code {code:#x} at {at}"),
            }
        }

        prop_assert_eq!(program[program.len() - 1].code, RETURN);
        Ok(())
    }

    fn every_call() -> Vec<SyscallId> {
        crate::syscalls::table::all().collect()
    }

    /// A set built by `trace=` lists of single calls, some removing, so
    /// that both small sets and nearly every call come up.
    fn call_set() -> impl Strategy<Value = CallSet> {
        let list = prop::collection::vec(prop::sample::select(every_call()), 0..12);
        let first = prop::sample::select(vec!["trace=all", "trace=none", ""]);

        (first, prop::collection::vec((any::<bool>(), list), 0..4)).prop_map(|(first, lists)| {
            let mut expressions = vec![first.to_owned()];
            for (negated, ids) in lists {
                let names = ids
                    .iter()
                    .map(|id| format!("{}@{}", id.display_name(), id.abi.suffix()))
                    .collect::<Vec<_>>()
                    .join(",");
                let bang = if negated { "!" } else { "" };
                expressions.push(format!("trace={bang}{names}"));
            }

            let expressions: Vec<&str> = expressions
                .iter()
                .map(String::as_str)
                .filter(|text| !text.is_empty())
                .collect();
            calls(&expressions)
        })
    }

    /// Numbers to run a program on: listed calls, numbers past every
    /// table, and words with the x32 bit or others set.
    fn number() -> impl Strategy<Value = u32> {
        prop_oneof![
            3 => prop::sample::select(every_call()).prop_map(|id| id.number),
            1 => 0u32..1024,
            1 => (0u32..1024).prop_map(|number| number | X32_SYSCALL_BIT),
            1 => any::<u32>(),
        ]
    }

    proptest! {
        /// For any set of calls, the filter is a program the kernel
        /// accepts that traces exactly the set's calls, in each ABI, and
        /// lets everything else run.
        #[test]
        fn filters_trace_exactly_their_calls(
            calls in call_set(),
            members in prop::collection::vec(any::<prop::sample::Index>(), 32),
            numbers in prop::collection::vec(number(), 32),
            other_arch in any::<u32>(),
        ) {
            let program = filter(&calls);
            check_structure(&program)?;

            let traced = |traced: bool| if traced || calls.is_all() { RET_TRACE } else { RET_ALLOW };

            // Calls in the set, then numbers of any kind.
            let listed: Vec<SyscallId> = calls.iter().collect();
            let members = members.iter().filter(|_| !listed.is_empty()).map(|index| {
                let id = listed[index.index(listed.len())];
                match id.abi {
                    Abi::X86_64 => (AUDIT_ARCH_X86_64, id.number),
                    Abi::X32 => (AUDIT_ARCH_X86_64, id.number | X32_SYSCALL_BIT),
                    Abi::X86_32 => (AUDIT_ARCH_I386, id.number),
                }
            });
            let others = numbers.iter().flat_map(|&number| {
                [(AUDIT_ARCH_X86_64, number), (AUDIT_ARCH_I386, number), (other_arch, number)]
            });

            for (arch, number) in members.chain(others) {
                let expected = match arch {
                    AUDIT_ARCH_X86_64 if number & X32_SYSCALL_BIT != 0 => traced(
                        calls.contains(SyscallId::new(Abi::X32, number & !X32_SYSCALL_BIT)),
                    ),
                    AUDIT_ARCH_X86_64 => traced(calls.contains(SyscallId::new(Abi::X86_64, number))),
                    AUDIT_ARCH_I386 => traced(calls.contains(SyscallId::new(Abi::X86_32, number))),
                    _ => traced(false),
                };

                prop_assert_eq!(run(&program, arch, number), Some(expected), "{:#x} {:#x}", arch, number);
            }
        }
    }
}
