//! The simulated CPU: executes one x86-64 instruction of one thread at a
//! time against its registers and address space.
//!
//! iced-x86 decodes; [`ops`] gives each instruction its meaning. Only the
//! instruction forms in [`IMPLEMENTED`] are, which the lockstep test
//! (`conformance::cpu`) requires to be exactly those the golden corpus
//! executes on the real CPU, but for [`PROBED_THROUGH_THE_KERNEL`]. Any
//! other instruction is reported as [`Outcome::Unsupported`], never
//! guessed. The
//! CPU knows nothing of ptrace or signals: the kernel turns each
//! [`Outcome`] into what Linux would do.

mod flags;
mod ops;

#[cfg(test)]
pub use ops::{IMPLEMENTED, PROBED_THROUGH_THE_KERNEL};

use iced_x86::{
    Decoder, DecoderOptions, FlowControl, Formatter as _, GasFormatter, Instruction, Mnemonic,
    Register,
};

use super::memory::{AddressSpace, MemoryError, MemoryFault, USER_HALF_END};

/// The longest x86-64 instruction.
const MAX_INSTRUCTION_BYTES: usize = 15;

pub const SIGFPE: i32 = 8;

pub const SIGSEGV: i32 = 11;

/// `si_code` of an integer division by zero or overflow.
const FPE_INTDIV: i32 = 1;

/// `si_code` of a fault the kernel raised without an address, such as a
/// general-protection fault.
pub const SI_KERNEL: i32 = 0x80;

pub const CARRY: u64 = 1 << 0;

pub const PARITY: u64 = 1 << 2;

pub const ADJUST: u64 = 1 << 4;

pub const ZERO: u64 = 1 << 6;

/// The trap flag, which raises a debug exception after each instruction.
pub const TRAP: u64 = 1 << 8;

pub const SIGN: u64 = 1 << 7;

pub const DIRECTION: u64 = 1 << 10;

pub const OVERFLOW: u64 = 1 << 11;

/// The flags arithmetic instructions compute.
pub const STATUS_FLAGS: u64 = CARRY | PARITY | ADJUST | ZERO | SIGN | OVERFLOW;

/// General registers in encoding order, as indices into
/// [`Registers::general`].
pub const RAX: usize = 0;

pub const RCX: usize = 1;

pub const RDX: usize = 2;

pub const RSP: usize = 4;

pub const RBP: usize = 5;

pub const RSI: usize = 6;

pub const RDI: usize = 7;

pub const R8: usize = 8;

pub const R9: usize = 9;

pub const R10: usize = 10;

pub const R11: usize = 11;

/// One thread's user-visible register state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Registers {
    /// RAX, RCX, RDX, RBX, RSP, RBP, RSI, RDI, R8 to R15.
    pub general: [u64; 16],
    pub rip: u64,
    pub rflags: u64,
    pub fs_base: u64,
    pub gs_base: u64,
}

/// A signal the CPU raised for an instruction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Fault {
    pub signal: i32,
    pub code: i32,
    pub address: Option<u64>,
}

impl From<MemoryFault> for Fault {
    fn from(fault: MemoryFault) -> Self {
        Self {
            signal: SIGSEGV,
            code: fault.code,
            address: Some(fault.address),
        }
    }
}

/// What executing one instruction did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// The instruction ran; `rip` names the next one.
    Completed,

    /// A `syscall`: `rip` is past it, `rcx` and `r11` hold what the CPU
    /// saved, and the kernel must now serve the call.
    Syscall,

    /// An `int3`, with `rip` past it.
    Breakpoint,

    /// The instruction faulted without changing anything; `rip` names it.
    Fault(Fault),

    /// The simulator does not model this instruction, described as text.
    Unsupported(String),
}

/// How a completed instruction moved between functions, which the kernel's
/// shadow call stacks follow.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Flow {
    Other,

    /// A call, which pushed `return_address` at `slot`.
    Call {
        return_address: u64,
        slot: u64,
    },

    /// A return, to the address now in `rip`.
    Return,

    /// A jump taken from the instruction at `from`, which ends at `next`,
    /// to the address now in `rip`.
    Jump {
        from: u64,
        next: u64,
    },
}

/// A data access an instruction made.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Access {
    pub address: u64,
    pub size: u64,
    pub write: bool,
}

/// The data accesses one instruction made, in order: few enough for a
/// fixed array, so executing allocates nothing.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Accesses {
    items: [Option<Access>; 4],
}

impl Accesses {
    pub fn push(&mut self, access: Access) {
        let slot = self
            .items
            .iter_mut()
            .find(|slot| slot.is_none())
            .expect("an instruction makes at most four data accesses");
        *slot = Some(access);
    }

    pub fn iter(&self) -> impl Iterator<Item = Access> + '_ {
        self.items.iter().map_while(|access| *access)
    }
}

/// What executing one instruction did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Executed {
    pub outcome: Outcome,
    pub flow: Flow,

    /// The accesses of an instruction that completed.
    pub accesses: Accesses,
}

/// The effect of an instruction that did not complete normally.
enum Stop {
    Fault(Fault),

    /// The model does not implement the instruction.
    Unsupported,

    /// The model does not know what Linux does with the instruction's
    /// access at `address`, as `what` says.
    Unmodeled {
        address: u64,
        what: &'static str,
    },
}

impl From<MemoryError> for Stop {
    fn from(error: MemoryError) -> Self {
        match error {
            MemoryError::Fault(fault) => Self::Fault(fault.into()),
            MemoryError::Unmodeled { address, what } => Self::Unmodeled { address, what },
        }
    }
}

/// Executes the instruction at `registers.rip`, saying whether it was a
/// call or a return and what data it accessed.
///
/// A string instruction with a repeat prefix executes one iteration,
/// leaving `rip` at itself until its count runs out, as the CPU does when
/// it traps between iterations.
///
/// `trap` sets the trap flag while the instruction runs, as ptrace sets
/// it to single-step a thread. `rflags` never shows it, as
/// `PTRACE_GETREGS` hides a trap flag ptrace set, but `syscall` saves it
/// in r11 with the other flags.
pub fn execute(registers: &mut Registers, memory: &mut AddressSpace, trap: bool) -> Executed {
    let failed = |outcome| Executed {
        outcome,
        flow: Flow::Other,
        accesses: Accesses::default(),
    };

    let instruction = match decode(registers.rip, memory) {
        Ok(instruction) => instruction,
        Err(outcome) => return failed(outcome),
    };

    let mut next = *registers;
    next.rip = instruction.next_ip();
    if trap {
        next.rflags |= TRAP;
    }

    let accesses = std::cell::Cell::new(Accesses::default());
    let executed = ops::execute(&instruction, &mut next, memory, &accesses).and_then(|outcome| {
        // A branch to a non-canonical address faults on the branch; one
        // to the kernel's half faults at its target, by a page fault or
        // a general-protection fault by the machine.
        if next.rip >= USER_HALF_END {
            return Err(Stop::Unmodeled {
                address: next.rip,
                what: "a branch past the user half of the address space",
            });
        }

        Ok(outcome)
    });

    match executed {
        Ok(outcome) => {
            next.rflags &= !TRAP;
            *registers = next;

            let flow = match instruction.mnemonic() {
                Mnemonic::Call => Flow::Call {
                    return_address: instruction.next_ip(),
                    slot: registers.general[RSP],
                },
                Mnemonic::Ret => Flow::Return,
                _ if registers.rip != instruction.next_ip()
                    && matches!(
                        instruction.flow_control(),
                        FlowControl::UnconditionalBranch
                            | FlowControl::ConditionalBranch
                            | FlowControl::IndirectBranch
                    ) =>
                {
                    Flow::Jump {
                        from: instruction.ip(),
                        next: instruction.next_ip(),
                    }
                }
                _ => Flow::Other,
            };

            Executed {
                outcome,
                flow,
                accesses: accesses.get(),
            }
        }
        Err(Stop::Fault(fault)) => failed(Outcome::Fault(fault)),
        Err(Stop::Unsupported) => failed(Outcome::Unsupported(format!(
            "instruction {}",
            describe(&instruction)
        ))),
        Err(Stop::Unmodeled { address, what }) => failed(Outcome::Unsupported(format!(
            "{what} at {address:#x}, by {}",
            describe(&instruction)
        ))),
    }
}

/// Decodes the instruction at `address`, or says what trying to execute
/// it does instead: a fault fetching it, or a model gap.
pub fn decode(address: u64, memory: &AddressSpace) -> Result<Instruction, Outcome> {
    let unfetchable = |error| match error {
        MemoryError::Fault(fault) => Outcome::Fault(fault.into()),
        MemoryError::Unmodeled { address, what } => {
            Outcome::Unsupported(format!("{what} at {address:#x}, fetching an instruction"))
        }
    };

    let mut bytes = [0; MAX_INSTRUCTION_BYTES];
    let fetched = memory.fetch(address, &mut bytes).map_err(unfetchable)?;
    let mut decoder = Decoder::with_ip(64, &bytes[..fetched], address, DecoderOptions::NONE);
    let instruction = decoder.decode();

    match decoder.last_error() {
        iced_x86::DecoderError::None => Ok(instruction),
        iced_x86::DecoderError::NoMoreBytes if fetched < MAX_INSTRUCTION_BYTES => {
            // The instruction runs into memory that cannot be executed,
            // where fetching stopped.
            let end = address + fetched as u64;
            Err(memory.fetch(end, &mut [0]).map_or_else(unfetchable, |_| {
                Outcome::Unsupported(format!("instruction fetching stopped at {end:#x}"))
            }))
        }

        // An undefined opcode raises SIGILL, and an instruction longer
        // than fifteen bytes, which runs out of the bytes fetched, a
        // general-protection fault's SIGSEGV. No probe pins either.
        _ => Err(Outcome::Unsupported(format!(
            "bytes that do not decode at {address:#x}: {:02x?}",
            &bytes[..fetched]
        ))),
    }
}

/// An instruction in AT&T syntax, with its address.
#[must_use]
pub fn describe(instruction: &Instruction) -> String {
    let mut text = format!("{:#x}: ", instruction.ip());
    GasFormatter::new().format(instruction, &mut text);
    text
}

/// The index into [`Registers::general`] of a general register of any
/// size, and whether it names bits 8 to 15 (AH, CH, DH, or BH).
fn general_index(register: Register) -> Option<(usize, bool)> {
    let high = matches!(
        register,
        Register::AH | Register::CH | Register::DH | Register::BH
    );

    let full = register.full_register();
    let index = (full as usize).checked_sub(Register::RAX as usize)?;

    (index < 16 && full.is_gpr64()).then_some((index, high))
}

impl Registers {
    /// Reads a general register of any size, zero-extended.
    fn read(&self, register: Register) -> Option<u64> {
        let (index, high) = general_index(register)?;
        let value = self.general[index];

        Some(if high {
            (value >> 8) & 0xff
        } else {
            value & flags::mask(register.size() * 8)
        })
    }

    /// Writes a general register of any size. A 32-bit write clears the
    /// upper half; narrower writes keep the rest of the register.
    fn write(&mut self, register: Register, value: u64) -> Option<()> {
        let (index, high) = general_index(register)?;
        let slot = &mut self.general[index];
        *slot = match (register.size(), high) {
            (8, _) => value,
            (4, _) => value & 0xffff_ffff,
            (1, true) => (*slot & !0xff00) | ((value & 0xff) << 8),
            (size, _) => {
                let mask = flags::mask(size * 8);
                (*slot & !mask) | (value & mask)
            }
        };

        Some(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sim::memory::{Backing, Protection};

    /// Runs `code` at a fixed address from `registers` until it completes
    /// one instruction, returning the outcome.
    fn run(code: &[u8], registers: &mut Registers) -> Outcome {
        let mut memory = AddressSpace::default();
        memory.map(0x1000, 0x2000, Protection::READ_EXECUTE, Backing::Anonymous);
        memory.map(0x8000, 0x9000, Protection::READ_WRITE, Backing::Anonymous);
        assert!(memory.poke(
            0x1000,
            u64::from_le_bytes(code.try_into().expect("8 bytes"))
        ));

        registers.rip = 0x1000;
        execute(registers, &mut memory, false).outcome
    }

    /// A faulting instruction changes nothing, and the fault names the
    /// address the program touched; a division by zero faults at the
    /// instruction. Decoding never runs off executable memory.
    #[test]
    fn faults_leave_registers_untouched() {
        // push %rax with the stack pointer in unmapped memory.
        let mut registers = Registers::default();
        registers.general[RSP] = 0x4000;
        let before = registers;

        assert_eq!(
            run(&[0x50, 0, 0, 0, 0, 0, 0, 0], &mut registers),
            Outcome::Fault(Fault {
                signal: SIGSEGV,
                code: crate::sim::memory::SEGV_MAPERR,
                address: Some(0x3ff8),
            })
        );
        assert_eq!(
            registers,
            Registers {
                rip: 0x1000,
                ..before
            }
        );

        // div %rcx with rcx zero.
        let mut registers = Registers::default();
        assert_eq!(
            run(&[0x48, 0xf7, 0xf1, 0, 0, 0, 0, 0], &mut registers),
            Outcome::Fault(Fault {
                signal: SIGFPE,
                code: FPE_INTDIV,
                address: Some(0x1000),
            })
        );

        // A write to code is refused.
        let mut registers = Registers::default();
        registers.general[RAX] = 0x1000;
        assert!(matches!(
            run(&[0xc6, 0x00, 0xcc, 0, 0, 0, 0, 0], &mut registers),
            Outcome::Fault(Fault {
                code: crate::sim::memory::SEGV_ACCERR,
                ..
            })
        ));
    }

    /// What Linux does depends on what the model does not know, so each of
    /// these is a gap, never a fault, and changes nothing: an access in
    /// the stack's growth window, which Linux grows the stack into up to
    /// a limit; an address beyond the user half, which faults as a page
    /// fault, a general-protection fault, or not at all by the machine;
    /// a read of memory mapped writable or executable but not readable,
    /// which x86 allows unless protection keys refuse it; a branch beyond
    /// the user half, which faults on the branch or at its target by the
    /// machine; and bytes iced cannot decode, an undefined opcode's
    /// SIGILL or a too-long instruction's SIGSEGV.
    #[test]
    fn accesses_linux_answers_by_what_the_model_does_not_know_are_gaps() {
        // What the gap says, the code, and the registers it starts with.
        type Case<'a> = (&'a str, &'a [u8], &'a [(usize, u64)]);
        const GROWTH: &str = "an access the stack would grow over";
        const PAST: &str = "an access past the user half";
        const UNREADABLE: &str = "a read of memory mapped writable or executable";
        const UNDECODED: &str = "bytes that do not decode";

        let stack = 0x7fff_fffd_e000;
        let mut memory = AddressSpace::default();
        memory.map(0x1000, 0x2000, Protection::READ_EXECUTE, Backing::Anonymous);

        let write_only = Protection {
            read: false,
            write: true,
            execute: false,
        };
        let execute_only = Protection {
            read: false,
            write: false,
            execute: true,
        };

        memory.map(0x3000, 0x4000, write_only, Backing::Anonymous);
        memory.map(0x5000, 0x6000, execute_only, Backing::Anonymous);
        memory.map(
            stack,
            stack + 0x2_1000,
            Protection::READ_WRITE,
            Backing::Stack,
        );

        // A return to the address `ret` finds here.
        let slot = stack + 0x100;
        assert!(memory.poke(slot, 1 << 47));
        let cases: [Case<'_>; 10] = [
            // push %rax, below the stack.
            (GROWTH, &[0x50], &[(RSP, stack)]),
            // mov (%rax), %rcx, far below the stack.
            (GROWTH, &[0x48, 0x8b, 0x08], &[(RAX, 0x10_0000)]),
            (PAST, &[0x48, 0x8b, 0x08], &[(RAX, 1 << 47)]),
            (PAST, &[0x48, 0x8b, 0x08], &[(RAX, 0x8000_0000_0000_0000)]),
            (PAST, &[0x48, 0x8b, 0x08], &[(RAX, u64::MAX - 3)]),
            (UNREADABLE, &[0x48, 0x8b, 0x08], &[(RAX, 0x3000)]),
            (UNREADABLE, &[0x48, 0x8b, 0x08], &[(RAX, 0x5000)]),
            // ret, to past the user half.
            ("a branch past the user half", &[0xc3], &[(RSP, slot)]),
            // push %es, undefined in 64-bit mode.
            (UNDECODED, &[0x06], &[]),
            // Fifteen operand-size prefixes and a nop: sixteen bytes.
            (UNDECODED, &[0x66; 15], &[]),
        ];

        for (why, code, values) in cases {
            let mut code = code.to_vec();
            code.resize(16, 0x90);
            assert!(memory.poke_bytes(0x1000, &code));

            let mut registers = Registers {
                rip: 0x1000,
                ..Registers::default()
            };
            for &(index, value) in values {
                registers.general[index] = value;
            }

            let before = registers;
            let outcome = execute(&mut registers, &mut memory, false).outcome;

            assert!(
                matches!(&outcome, Outcome::Unsupported(text) if text.starts_with(why)),
                "{why} {values:x?}: {outcome:?}"
            );
            assert_eq!(registers, before, "{why} {values:x?}");
        }

        // Within the stack and the code, the same instructions run.
        let mut registers = Registers {
            rip: 0x1000,
            ..Registers::default()
        };
        registers.general[RSP] = stack + 8;

        assert!(memory.poke_bytes(0x1000, &[0x50]));
        let outcome = execute(&mut registers, &mut memory, false).outcome;
        assert_eq!(outcome, Outcome::Completed);
    }

    /// The implemented forms are sorted, as the search for one needs, and
    /// every form the kernel's probes run is one of them.
    #[test]
    fn the_implemented_forms_are_sorted() {
        assert!(
            IMPLEMENTED.is_sorted(),
            "IMPLEMENTED is out of iced's order"
        );
        assert!(
            PROBED_THROUGH_THE_KERNEL
                .iter()
                .all(|code| IMPLEMENTED.contains(code))
        );
    }

    /// `syscall` saves the trap flag a single step sets in r11, though
    /// `rflags` does not show it.
    #[test]
    fn syscall_saves_the_trap_flag_in_r11() {
        for trap in [false, true] {
            let mut memory = AddressSpace::default();
            memory.map(0x1000, 0x2000, Protection::READ_EXECUTE, Backing::Anonymous);
            assert!(memory.poke_bytes(0x1000, &[0x0f, 0x05]));
            let mut registers = Registers {
                rip: 0x1000,
                rflags: 0x206,
                ..Registers::default()
            };

            let outcome = execute(&mut registers, &mut memory, trap).outcome;
            assert_eq!(outcome, Outcome::Syscall);
            assert_eq!(registers.rflags, 0x206);
            assert_eq!(registers.general[RCX], 0x1002);
            assert_eq!(
                registers.general[R11],
                if trap { 0x306 } else { 0x206 },
                "trap {trap}"
            );
        }
    }

    /// A form the model does not implement is a gap, even when it shares
    /// its mnemonic with one it does: SSE's `movsd` is not the string move,
    /// a 16-bit push moves two bytes, and a 16-bit `shld` by more than 16
    /// is undefined.
    #[test]
    fn forms_beyond_the_model_are_unsupported() {
        for code in [
            // movsd (%rax), %xmm0
            [0xf2, 0x0f, 0x10, 0x00, 0, 0, 0, 0],
            // movsd %xmm0, %xmm1
            [0xf2, 0x0f, 0x10, 0xc8, 0, 0, 0, 0],
            // push %ax
            [0x66, 0x50, 0, 0, 0, 0, 0, 0],
            // pop %ax
            [0x66, 0x58, 0, 0, 0, 0, 0, 0],
            // shld $17, %ax, %ax
            [0x66, 0x0f, 0xa4, 0xc0, 0x11, 0, 0, 0],
        ] {
            let mut registers = Registers::default();
            registers.general[RAX] = 0x8000;
            registers.general[RSI] = 0x8000;
            registers.general[RDI] = 0x8100;
            registers.general[RSP] = 0x8800;
            let outcome = run(&code, &mut registers);
            assert!(
                matches!(outcome, Outcome::Unsupported(_)),
                "{code:02x?}: {outcome:?}"
            );
        }
    }
}
