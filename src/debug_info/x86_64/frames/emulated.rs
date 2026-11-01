//! The frame proofs and epilogue reads checked against a small emulator of
//! the instructions they reason about, run down every path of generated
//! code.

use std::collections::{BTreeMap, BTreeSet};

use proptest::prelude::*;

use super::*;
use crate::debug_info::x86_64::Epilogues;

/// One instruction of generated code. A jump names the instruction it
/// goes to by index; past the last is past the code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Op {
    PushRbp,
    PopRbp,
    PushRbx,
    PopRbx,
    /// `sub rsp, 8 * n`.
    Allocate(u8),
    /// `add rsp, 8 * n`.
    Free(u8),
    MovRbpRsp,
    MovRspRbp,
    Leave,
    Nop,
    Call,
    Jump(usize),
    Branch(usize),
    Ret,
}

impl Op {
    fn length(self) -> u64 {
        match self {
            Self::PushRbp | Self::PopRbp | Self::PushRbx | Self::PopRbx => 1,
            Self::Leave | Self::Nop | Self::Ret => 1,
            Self::Allocate(_) | Self::Free(_) => 4,
            Self::MovRbpRsp | Self::MovRspRbp => 3,
            Self::Call => 5,
            Self::Jump(_) | Self::Branch(_) => 2,
        }
    }
}

/// Generated code: its instructions, where each starts, and its bytes.
#[derive(Debug)]
struct Program {
    ops: Vec<Op>,
    offsets: Vec<u64>,
    code: Vec<u8>,
}

impl Program {
    fn new(ops: Vec<Op>) -> Self {
        let mut offsets = vec![0];
        for op in &ops {
            offsets.push(offsets.last().copied().unwrap_or(0) + op.length());
        }

        let mut code = Vec::new();
        for (index, op) in ops.iter().enumerate() {
            let next = offsets[index + 1];
            let relative = |target: usize| {
                i8::try_from(offsets[target] as i64 - next as i64)
                    .expect("short code")
                    .cast_unsigned()
            };
            match *op {
                Op::PushRbp => code.push(0x55),
                Op::PopRbp => code.push(0x5d),
                Op::PushRbx => code.push(0x53),
                Op::PopRbx => code.push(0x5b),
                Op::Allocate(n) => code.extend([0x48, 0x83, 0xec, 8 * n]),
                Op::Free(n) => code.extend([0x48, 0x83, 0xc4, 8 * n]),
                Op::MovRbpRsp => code.extend([0x48, 0x89, 0xe5]),
                Op::MovRspRbp => code.extend([0x48, 0x89, 0xec]),
                Op::Leave => code.push(0xc9),
                Op::Nop => code.push(0x90),
                Op::Call => code.extend([0xe8, 0x00, 0x10, 0x00, 0x00]),
                Op::Jump(target) => code.extend([0xeb, relative(target)]),
                Op::Branch(target) => code.extend([0x74, relative(target)]),
                Op::Ret => code.push(0xc3),
            }
        }

        debug_assert_eq!(code.len() as u64, offsets[ops.len()]);
        Self { ops, offsets, code }
    }
}

/// Where the stack pointer is at the entry, which holds the return address.
const ENTRY_RSP: u64 = 0x7000_0000;
const RETURN: u64 = 0x4141_4141;
const CALLER_RBP: u64 = 0x5150_5150;
const CALLER_RBX: u64 = 0xb0b0_b0b0;

/// What a callee leaves below the stack pointer.
const CLOBBERED: u64 = 0xdead_0000;

/// The registers and stack of a thread running generated code.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
struct Machine {
    rsp: u64,
    rbp: u64,
    rbx: u64,
    stack: BTreeMap<u64, u64>,
}

impl Machine {
    fn entry() -> Self {
        Self {
            rsp: ENTRY_RSP,
            rbp: CALLER_RBP,
            rbx: CALLER_RBX,
            stack: BTreeMap::from([(ENTRY_RSP, RETURN)]),
        }
    }

    fn push(&mut self, value: u64) {
        self.rsp = self.rsp.wrapping_sub(8);
        self.stack.insert(self.rsp, value);
    }

    /// Pops a word; one never written is a value of its own.
    fn pop(&mut self) -> u64 {
        let value = self.stack.get(&self.rsp).copied().unwrap_or(!self.rsp);
        self.rsp = self.rsp.wrapping_add(8);
        value
    }

    /// Runs `op`, at `offset`.
    fn run(&mut self, op: Op, offset: u64) {
        match op {
            Op::PushRbp => self.push(self.rbp),
            Op::PopRbp => self.rbp = self.pop(),
            Op::PushRbx => self.push(self.rbx),
            Op::PopRbx => self.rbx = self.pop(),
            Op::Allocate(n) => self.rsp = self.rsp.wrapping_sub(8 * u64::from(n)),
            Op::Free(n) => self.rsp = self.rsp.wrapping_add(8 * u64::from(n)),
            Op::MovRbpRsp => self.rbp = self.rsp,
            Op::MovRspRbp => self.rsp = self.rbp,
            Op::Leave => {
                self.rsp = self.rbp;
                self.rbp = self.pop();
            }

            // The callee returns with rsp, rbx, and rbp as they were, as
            // the ABI requires, and may have written anything below rsp:
            // the call itself writes its return address there.
            Op::Call => {
                let rsp = self.rsp;
                for (_, word) in self.stack.range_mut(..rsp) {
                    *word = CLOBBERED;
                }
                self.stack.insert(rsp.wrapping_sub(8), offset + 5);
            }
            Op::Nop | Op::Jump(_) | Op::Branch(_) | Op::Ret => {}
        }
    }
}

impl MemoryReader for Machine {
    fn read_u64(&mut self, address: VirtualAddress) -> Option<u64> {
        self.stack.get(&address.get()).copied()
    }
}

/// Whether the proof goes on from the instruction at `offset`, whose frame
/// is `frame`, to the instructions it may run next.
fn proof_goes_on(frame: CodeFrame, code: &[u8], offset: u64) -> bool {
    let bytes = &code[usize::try_from(offset).expect("short code")..];
    let instruction = Decoder::with_ip(64, bytes, offset, DecoderOptions::NONE).decode();
    instruction.flow_control() == FlowControl::UnconditionalBranch
        || matches!(
            next(frame, &instruction, &mut InstructionInfoFactory::new()),
            Next::Frame(_)
        )
}

/// Every instruction each path from the entry reaches, by index, with the
/// machine as it arrives, along the paths the proof follows. Like
/// call-frame information, the proof assumes a frame is a function of the
/// instruction, so a path it cannot follow, past code whose frame it does
/// not know, is assumed to arrive anywhere with the frame proved there.
/// Paths are cut at `MAX_DEPTH` instructions, so loops end.
fn reached(program: &Program, frames: &CodeFrames) -> BTreeSet<(usize, Machine)> {
    const MAX_DEPTH: usize = 40;

    let mut seen = BTreeSet::new();
    let mut work = vec![(0_usize, Machine::entry(), 0_usize)];
    while let Some((at, machine, depth)) = work.pop() {
        let Some(&op) = program.ops.get(at) else {
            continue;
        };
        if depth > MAX_DEPTH || !seen.insert((at, machine.clone())) {
            continue;
        }

        let offset = program.offsets[at];
        if frames
            .at(offset)
            .is_none_or(|frame| !proof_goes_on(frame, &program.code, offset))
        {
            continue;
        }

        let mut after = machine;
        after.run(op, program.offsets[at]);
        match op {
            Op::Ret => {}
            Op::Jump(target) => work.push((target, after, depth + 1)),
            Op::Branch(target) => {
                work.push((target, after.clone(), depth + 1));
                work.push((at + 1, after, depth + 1));
            }
            _ => work.push((at + 1, after, depth + 1)),
        }
    }

    seen
}

fn op(length: usize) -> impl Strategy<Value = Op> {
    prop_oneof![
        3 => Just(Op::PushRbp),
        3 => Just(Op::PopRbp),
        1 => Just(Op::PushRbx),
        1 => Just(Op::PopRbx),
        2 => (1_u8..=4).prop_map(Op::Allocate),
        2 => (1_u8..=4).prop_map(Op::Free),
        3 => Just(Op::MovRbpRsp),
        2 => Just(Op::MovRspRbp),
        2 => Just(Op::Leave),
        1 => Just(Op::Nop),
        3 => Just(Op::Call),
        1 => (0..=length).prop_map(Op::Jump),
        2 => (0..=length).prop_map(Op::Branch),
        1 => Just(Op::Ret),
    ]
}

fn program() -> impl Strategy<Value = Program> {
    (1_usize..=12)
        .prop_flat_map(|length| proptest::collection::vec(op(length), length))
        .prop_map(Program::new)
}

/// Bits of `x86_64::CALLEE_SAVED`.
const KEEPS_RBX: u32 = 1;
const KEEPS_RBP: u32 = 2;

proptest! {
    #![proptest_config(ProptestConfig::with_cases(1024))]

    /// Wherever a path from the entry arrives, a frame the analysis proves
    /// there unwinds to the true return address and caller's stack
    /// pointer, and to the caller's rbp or to none.
    #[test]
    fn a_proved_frame_unwinds_to_the_true_caller(program in program()) {
        let frames = CodeFrames::analyze(&program.code);
        for (at, mut machine) in reached(&program, &frames) {
            let offset = program.offsets[at];
            let Some(frame) = frames.at(offset) else {
                continue;
            };

            let registers = RegisterFile::new([(RSP, machine.rsp), (RBP, machine.rbp)]);
            let caller = frame_caller(frame, &registers, &mut machine);
            let caller = caller.as_ref().map(|step| {
                (
                    step.registers.get(RETURN_ADDRESS),
                    step.registers.get(RSP),
                    step.registers.get(RBP).is_none_or(|rbp| rbp == CALLER_RBP),
                )
            });
            prop_assert_eq!(
                caller,
                Ok((Some(RETURN), Some(ENTRY_RSP + 8), true)),
                "{:?} at {} of {:?} with {:x?}",
                frame,
                offset,
                program.ops,
                machine
            );
        }
    }

    /// Code that the epilogue reader says runs straight to a return gets
    /// there with rsp, rbx, and rbp as they were, and code it says keeps
    /// registers keeps them at every instruction it names.
    #[test]
    fn epilogues_keep_what_they_say(program in program(), start in any::<prop::sample::Index>()) {
        let start = start.index(program.ops.len());
        let base = 0x1000;
        let code = &program.code[usize::try_from(program.offsets[start]).expect("short code")..];
        let pointer = base + program.offsets[start];
        let mut epilogues = Epilogues::new();

        // Each instruction from `start` in a straight line, until one
        // that may go elsewhere, with the machine as it arrives.
        let mut machine = Machine::entry();
        let mut line = Vec::new();
        for (at, op) in program.ops.iter().enumerate().skip(start) {
            line.push((base + program.offsets[at], *op, machine.clone()));
            machine.run(*op, program.offsets[at]);
            if matches!(op, Op::Jump(_) | Op::Branch(_) | Op::Ret | Op::Call) {
                break;
            }
        }

        let entry = Machine::entry();
        if let Some(end) = epilogues.runs_to_return(code, pointer) {
            let ret = line.iter().find(|(at, op, _)| *op == Op::Ret && at + 1 == end);
            let arrives = ret.map(|(_, _, machine)| (machine.rsp, machine.rbp, machine.rbx));
            prop_assert_eq!(arrives, Some((entry.rsp, entry.rbp, entry.rbx)), "{:?}", program.ops);
        }

        for kept in [0, KEEPS_RBX, KEEPS_RBP, KEEPS_RBX | KEEPS_RBP] {
            let end = epilogues.keeps(code, pointer, kept);
            for (at, _, machine) in line.iter().filter(|(at, ..)| *at < end) {
                let holds = machine.rsp == entry.rsp
                    && (kept & KEEPS_RBX == 0 || machine.rbx == entry.rbx)
                    && (kept & KEEPS_RBP == 0 || machine.rbp == entry.rbp);
                prop_assert!(holds, "{:#x} keeps {} of {:?}", at, kept, program.ops);
            }
        }
    }
}
