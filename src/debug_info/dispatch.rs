//! Where the function that runs a coroutine goes for each of its states.
//!
//! A coroutine's function begins by dispatching on the state number the
//! coroutine stores: rustc loads it and jumps through a table, or compares
//! it against each state's number. Each state's code begins where the
//! dispatch leaves for it, and the first instructions there, up to the
//! state's first work, only lead into it, such as reloading what the
//! previous poll spilled.
//!
//! Each state is found by running the dispatch with the state's number, on
//! a small model of the processor: registers and stack slots hold numbers,
//! addresses within the coroutine, or addresses on the function's stack,
//! each marked when its value came from the state number. The dispatch is
//! every instruction run before control first moves on a marked value; a
//! state's code begins where that move lands, and its lead ends at the
//! first call, return, store outside the stack, or branch on an unmarked
//! value. Anything the model does not know ends the run: a dispatch that
//! cannot be followed for every state leaves the function's resume points
//! unknown, with the reason.

use std::collections::BTreeSet;
use std::sync::Arc;

use iced_x86::{
    Code, ConditionCode, Decoder, DecoderOptions, FlowControl, Instruction, Mnemonic, OpKind,
    Register,
};

use crate::{AddressRange, ImageAddress, ResumePoint, ResumePoints, StateMember};

/// The most instructions one state's run executes.
const MAX_STEPS: usize = 96;

/// The longest instruction, which every read of code covers.
const MAX_INSTRUCTION: usize = 15;

/// The bytes of the image a dispatch is decoded from.
pub trait DispatchImage {
    /// The code bytes at `address`, up to `length`; fewer at the end of the
    /// code, and none outside it.
    fn code(&self, address: u64, length: usize) -> Option<&[u8]>;

    /// `length` bytes of read-only data at `address`, such as a jump
    /// table's.
    fn data(&self, address: u64, length: usize) -> Option<&[u8]>;
}

/// Decodes the dispatch at the start of the function whose code is
/// `ranges`, which is passed the coroutine in its first argument, for each
/// state number in `states`.
pub fn decode(
    image: &dyn DispatchImage,
    ranges: &[AddressRange<ImageAddress>],
    entry: ImageAddress,
    state: StateMember,
    states: &[u64],
) -> Result<ResumePoints, Arc<str>> {
    if states.is_empty() {
        return Err("the coroutine has no states".into());
    }

    let contains = |address: u64| {
        ranges
            .iter()
            .any(|range| range.contains(ImageAddress::new(address)))
    };

    let mut dispatch = BTreeSet::new();
    let mut points = Vec::with_capacity(states.len());

    for &value in states {
        let run = Run::new(image, state, value);
        let (landing, lead_end, executed) = run.execute(entry.get(), &contains)?;
        dispatch.extend(executed);
        points.push(ResumePoint {
            state: value,
            address: ImageAddress::new(landing),
            resumption: [AddressRange {
                start: ImageAddress::new(landing),
                end: ImageAddress::new(lead_end),
            }]
            .into(),
        });
    }

    Ok(ResumePoints {
        dispatch: ranges_of(dispatch),
        points: points.into(),
    })
}

/// The most instructions a flood visits.
const MAX_FLOOD: usize = 4096;

/// The code reachable from `start` without leaving the instructions
/// `within` admits: every branch is followed both ways, and a call returns.
/// An indirect branch or an instruction that cannot be decoded ends that
/// path.
pub fn flood(
    image: &dyn DispatchImage,
    start: ImageAddress,
    within: &dyn Fn(u64) -> bool,
) -> Arc<[AddressRange<ImageAddress>]> {
    flood_all(image, start, within).0
}

/// [`flood`]'s code, and whether it is all the code reachable: a path an
/// indirect branch, an undecodable instruction, or the bound on work ends
/// may reach more.
pub fn flood_all(
    image: &dyn DispatchImage,
    start: ImageAddress,
    within: &dyn Fn(u64) -> bool,
) -> (Arc<[AddressRange<ImageAddress>]>, bool) {
    let mut visited = BTreeSet::new();
    let mut extents = BTreeSet::new();
    let mut pending = vec![start.get()];
    let mut complete = true;

    while let Some(ip) = pending.pop() {
        if !within(ip) || visited.contains(&ip) {
            continue;
        }

        if visited.len() >= MAX_FLOOD {
            complete = false;
            continue;
        }

        visited.insert(ip);
        let Some(bytes) = image
            .code(ip, MAX_INSTRUCTION)
            .filter(|bytes| !bytes.is_empty())
        else {
            complete = false;
            continue;
        };

        let instruction = Decoder::with_ip(64, bytes, ip, DecoderOptions::NONE).decode();
        if instruction.code() == Code::INVALID {
            complete = false;
            continue;
        }

        extents.insert((ip, instruction.next_ip()));
        match instruction.flow_control() {
            FlowControl::Next | FlowControl::Call | FlowControl::IndirectCall => {
                pending.push(instruction.next_ip());
            }
            FlowControl::UnconditionalBranch if instruction.op0_kind() == OpKind::NearBranch64 => {
                pending.push(instruction.near_branch_target());
            }
            FlowControl::ConditionalBranch => {
                pending.push(instruction.next_ip());
                pending.push(instruction.near_branch_target());
            }
            FlowControl::UnconditionalBranch | FlowControl::IndirectBranch => complete = false,
            _ => {}
        }
    }

    (ranges_of(extents), complete)
}

/// The first instruction `within` does not admit that execution from
/// `start` reaches, preferring each branch's fall-through: the first
/// instruction of new work after code that only leads into it.
pub fn first_beyond(
    image: &dyn DispatchImage,
    start: ImageAddress,
    within: &dyn Fn(u64) -> bool,
) -> Option<ImageAddress> {
    let mut ip = start.get();
    let mut visited = BTreeSet::new();

    for _ in 0..MAX_STEPS {
        if !within(ip) {
            return Some(ImageAddress::new(ip));
        }

        if !visited.insert(ip) {
            return None;
        }

        let bytes = image
            .code(ip, MAX_INSTRUCTION)
            .filter(|bytes| !bytes.is_empty())?;
        let instruction = Decoder::with_ip(64, bytes, ip, DecoderOptions::NONE).decode();
        if instruction.code() == Code::INVALID {
            return None;
        }

        ip = match instruction.flow_control() {
            FlowControl::Next
            | FlowControl::Call
            | FlowControl::IndirectCall
            | FlowControl::ConditionalBranch => instruction.next_ip(),
            FlowControl::UnconditionalBranch if instruction.op0_kind() == OpKind::NearBranch64 => {
                instruction.near_branch_target()
            }
            _ => return None,
        };
    }

    None
}

/// Merges instruction extents into ranges.
fn ranges_of(extents: BTreeSet<(u64, u64)>) -> Arc<[AddressRange<ImageAddress>]> {
    let mut ranges = Vec::<AddressRange<ImageAddress>>::new();
    for (start, end) in extents {
        match ranges.last_mut() {
            Some(last) if last.end.get() >= start => {
                last.end = ImageAddress::new(last.end.get().max(end));
            }
            _ => ranges.push(AddressRange {
                start: ImageAddress::new(start),
                end: ImageAddress::new(end),
            }),
        }
    }

    ranges.into()
}

/// What a register or stack slot holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Value {
    Unknown,
    Number(u64),

    /// An address this far into the coroutine.
    Object(u64),

    /// An address this far from the stack pointer at the function's entry.
    Stack(i64),
}

/// A value, and whether it came from the state number.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Cell {
    value: Value,
    state: bool,
}

const UNKNOWN: Cell = Cell {
    value: Value::Unknown,
    state: false,
};

/// The flags the last comparison set, and whether its operands came from
/// the state number.
#[derive(Debug, Clone, Copy)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "the processor's flags are bits, each read on its own"
)]
struct Flags {
    zero: bool,
    carry: bool,
    sign: bool,
    overflow: bool,
    state: bool,
}

/// Where a run's state begins, where its lead ends, and the extents of the
/// dispatch's instructions.
type Ran = (u64, u64, Vec<(u64, u64)>);

struct Run<'a> {
    image: &'a dyn DispatchImage,
    member: StateMember,
    value: u64,
    registers: [Cell; 16],
    stack: Vec<(i64, Cell)>,
    flags: Option<Flags>,
}

/// How a run continues after an instruction.
enum Next {
    /// To the next instruction.
    Fall,

    /// To an address, having moved on a value the state number decided.
    Decided(u64),

    /// To an address, on values the state number did not decide.
    Jump(u64),

    /// The model cannot follow this instruction, for the reason.
    Stop(&'static str),
}

impl<'a> Run<'a> {
    fn new(image: &'a dyn DispatchImage, member: StateMember, value: u64) -> Self {
        let mut registers = [UNKNOWN; 16];
        if let Some(rdi) = index(Register::RDI) {
            registers[rdi] = Cell {
                value: Value::Object(0),
                state: false,
            };
        }

        if let Some(rsp) = index(Register::RSP) {
            registers[rsp] = Cell {
                value: Value::Stack(0),
                state: false,
            };
        }

        Self {
            image,
            member,
            value,
            registers,
            stack: Vec::new(),
            flags: None,
        }
    }

    /// Runs from `entry`: where the state's code begins, where its lead
    /// ends, and the extents of the dispatch's instructions.
    fn execute(mut self, entry: u64, contains: &dyn Fn(u64) -> bool) -> Result<Ran, Arc<str>> {
        let mut ip = entry;
        let mut landing = None;
        let mut executed = Vec::new();

        for _ in 0..MAX_STEPS {
            if !contains(ip) {
                return Err(format!("the dispatch leaves the function at {ip:#x}").into());
            }

            let instruction = self.decode(ip)?;
            let next = self.step(&instruction);
            let after = instruction.next_ip();

            match (next, landing) {
                // Before the state decides anything, the dispatch goes on.
                (Next::Fall, None) => {
                    executed.push((ip, after));
                    ip = after;
                }
                (Next::Jump(target), None) => {
                    executed.push((ip, after));
                    ip = target;
                }
                (Next::Decided(target), _) => {
                    if landing.is_none() {
                        executed.push((ip, after));
                    }

                    if !contains(target) {
                        return Err(format!(
                            "state {} leaves the function for {target:#x}",
                            self.value
                        )
                        .into());
                    }

                    landing = Some(target);
                    ip = target;
                }
                (Next::Fall, Some(_)) => ip = after,
                (Next::Jump(_) | Next::Stop(_), Some(at)) => return Ok((at, after, executed)),
                (Next::Stop(reason), None) => {
                    return Err(format!(
                        "the dispatch for state {} cannot be followed at {ip:#x}: {reason}",
                        self.value
                    )
                    .into());
                }
            }
        }

        landing.map_or_else(
            || Err(format!("no dispatch on the state within {MAX_STEPS} instructions").into()),
            |at| Ok((at, ip, executed)),
        )
    }

    fn decode(&self, ip: u64) -> Result<Instruction, Arc<str>> {
        let bytes = self
            .image
            .code(ip, MAX_INSTRUCTION)
            .filter(|bytes| !bytes.is_empty())
            .ok_or_else(|| Arc::from(format!("no code at {ip:#x}")))?;
        let mut decoder = Decoder::with_ip(64, bytes, ip, DecoderOptions::NONE);
        let instruction = decoder.decode();
        if instruction.code() == Code::INVALID {
            return Err(format!("no instruction at {ip:#x}").into());
        }

        Ok(instruction)
    }

    #[expect(
        clippy::too_many_lines,
        reason = "one match models every instruction a dispatch uses"
    )]
    fn step(&mut self, instruction: &Instruction) -> Next {
        match instruction.mnemonic() {
            Mnemonic::Endbr64 | Mnemonic::Nop => Next::Fall,
            Mnemonic::Push => {
                let value = self.operand(instruction, 0);
                let Some(stack) = self.adjust_stack(-8) else {
                    return Next::Stop("the stack pointer is unknown");
                };

                self.store_stack(stack, value);
                Next::Fall
            }
            Mnemonic::Pop => {
                let Value::Stack(at) = self.register(Register::RSP).value else {
                    return Next::Stop("the stack pointer is unknown");
                };

                let value = self.load_stack(at);
                self.adjust_stack(8);
                self.write(instruction, 0, value)
            }
            Mnemonic::Mov | Mnemonic::Movzx | Mnemonic::Movsxd | Mnemonic::Movsx => {
                let size = instruction.memory_size().size();
                let source = if instruction.op1_kind() == OpKind::Memory {
                    let sign = matches!(instruction.mnemonic(), Mnemonic::Movsxd | Mnemonic::Movsx);
                    self.load(instruction, size, sign)
                } else {
                    self.operand(instruction, 1)
                };

                let source = match (instruction.mnemonic(), source.value) {
                    // A narrower register's value, extended.
                    (Mnemonic::Movzx, Value::Number(number))
                        if instruction.op1_kind() == OpKind::Register =>
                    {
                        let bits = instruction.op1_register().size() * 8;
                        Cell {
                            value: Value::Number(number & mask(bits)),
                            state: source.state,
                        }
                    }
                    _ => source,
                };

                self.write(instruction, 0, source)
            }
            Mnemonic::Lea => {
                let address = self.address(instruction);
                self.write(instruction, 0, address)
            }
            Mnemonic::Add | Mnemonic::Sub | Mnemonic::And | Mnemonic::Or | Mnemonic::Xor => {
                let left = self.operand(instruction, 0);
                let right = self.operand(instruction, 1);
                let zeroing = instruction.mnemonic() == Mnemonic::Xor
                    && instruction.op0_kind() == OpKind::Register
                    && instruction.op1_kind() == OpKind::Register
                    && instruction.op0_register() == instruction.op1_register();
                let state = (left.state || right.state) && !zeroing;
                let bits = operand_bits(instruction);

                let value = match (instruction.mnemonic(), left.value, right.value) {
                    _ if zeroing => Value::Number(0),
                    (Mnemonic::Add, Value::Number(a), Value::Number(b)) => {
                        Value::Number(a.wrapping_add(b) & mask(bits))
                    }

                    (Mnemonic::Add, Value::Stack(a), Value::Number(b))
                        if instruction.op0_register() == Register::RSP =>
                    {
                        Value::Stack(a.wrapping_add(b.cast_signed()))
                    }

                    (Mnemonic::Sub, Value::Stack(a), Value::Number(b))
                        if instruction.op0_register() == Register::RSP =>
                    {
                        Value::Stack(a.wrapping_sub(b.cast_signed()))
                    }
                    (Mnemonic::Add, Value::Object(a), Value::Number(b)) => {
                        Value::Object(a.wrapping_add(b))
                    }
                    (Mnemonic::Sub, Value::Number(a), Value::Number(b)) => {
                        Value::Number(a.wrapping_sub(b) & mask(bits))
                    }
                    (Mnemonic::And, Value::Number(a), Value::Number(b)) => Value::Number(a & b),
                    (Mnemonic::Or, Value::Number(a), Value::Number(b)) => Value::Number(a | b),
                    (Mnemonic::Xor, Value::Number(a), Value::Number(b)) => Value::Number(a ^ b),
                    _ => Value::Unknown,
                };

                self.flags = match value {
                    Value::Number(result) => Some(arithmetic_flags(
                        instruction.mnemonic(),
                        left.value,
                        right.value,
                        result,
                        bits,
                        state,
                    )),
                    _ => None,
                };

                self.write(instruction, 0, Cell { value, state })
            }
            Mnemonic::Cmp | Mnemonic::Test => {
                let left = self.operand(instruction, 0);
                let right = self.operand(instruction, 1);
                let bits = operand_bits(instruction);

                self.flags = match (left.value, right.value) {
                    (Value::Number(a), Value::Number(b)) => {
                        let state = left.state || right.state;
                        Some(if instruction.mnemonic() == Mnemonic::Cmp {
                            subtraction_flags(a, b, bits, state)
                        } else {
                            let result = a & b & mask(bits);
                            Flags {
                                zero: result == 0,
                                carry: false,
                                sign: result >> (bits - 1) & 1 == 1,
                                overflow: false,
                                state,
                            }
                        })
                    }
                    _ => None,
                };

                Next::Fall
            }
            _ => self.control(instruction),
        }
    }

    /// Branches, calls, returns, and every instruction the model does not
    /// know.
    fn control(&self, instruction: &Instruction) -> Next {
        match instruction.flow_control() {
            FlowControl::UnconditionalBranch => match instruction.op0_kind() {
                OpKind::NearBranch64 => Next::Jump(instruction.near_branch_target()),
                _ => Next::Stop("an unusual branch"),
            },
            FlowControl::ConditionalBranch => {
                let Some(flags) = self.flags else {
                    return Next::Stop("a branch on flags the model does not know");
                };

                let Some(taken) = condition(instruction.condition_code(), flags) else {
                    return Next::Stop("a branch on a condition the model does not know");
                };

                let target = if taken {
                    instruction.near_branch_target()
                } else {
                    instruction.next_ip()
                };

                if flags.state {
                    Next::Decided(target)
                } else {
                    Next::Jump(target)
                }
            }
            FlowControl::IndirectBranch => {
                let target = if instruction.op0_kind() == OpKind::Register {
                    self.register(instruction.op0_register())
                } else {
                    return Next::Stop("an indirect branch through memory");
                };

                match target {
                    Cell {
                        value: Value::Number(address),
                        state: true,
                    } => Next::Decided(address),
                    Cell {
                        value: Value::Number(address),
                        state: false,
                    } => Next::Jump(address),
                    _ => Next::Stop("an indirect branch to an unknown address"),
                }
            }
            FlowControl::Call | FlowControl::IndirectCall => Next::Stop("a call"),
            FlowControl::Return => Next::Stop("a return"),
            _ => Next::Stop("an instruction the model does not know"),
        }
    }

    fn register(&self, register: Register) -> Cell {
        index(register).map_or(UNKNOWN, |index| self.registers[index])
    }

    fn adjust_stack(&mut self, by: i64) -> Option<i64> {
        let slot = &mut self.registers[index(Register::RSP)?];
        let Value::Stack(at) = slot.value else {
            return None;
        };

        let moved = at.checked_add(by)?;
        slot.value = Value::Stack(moved);
        Some(moved)
    }

    fn load_stack(&self, at: i64) -> Cell {
        self.stack
            .iter()
            .rev()
            .find(|(slot, _)| *slot == at)
            .map_or(UNKNOWN, |(_, cell)| *cell)
    }

    fn store_stack(&mut self, at: i64, cell: Cell) {
        self.stack.retain(|(slot, _)| *slot != at);
        self.stack.push((at, cell));
    }

    /// The value of operand `operand` that is a register or an immediate.
    fn operand(&self, instruction: &Instruction, operand: u32) -> Cell {
        match instruction.op_kind(operand) {
            OpKind::Register => {
                let register = instruction.op_register(operand);
                let cell = self.register(register);
                match (cell.value, register.size()) {
                    (Value::Number(number), size @ 1..=4) => Cell {
                        value: Value::Number(number & mask(size * 8)),
                        state: cell.state,
                    },
                    (_, 8) => cell,
                    _ => Cell {
                        value: Value::Unknown,
                        state: cell.state,
                    },
                }
            }

            OpKind::Immediate8
            | OpKind::Immediate16
            | OpKind::Immediate32
            | OpKind::Immediate64
            | OpKind::Immediate8to16
            | OpKind::Immediate8to32
            | OpKind::Immediate8to64
            | OpKind::Immediate32to64 => Cell {
                value: Value::Number(instruction.immediate(operand)),
                state: false,
            },
            OpKind::Memory => self.load(instruction, instruction.memory_size().size(), false),
            _ => UNKNOWN,
        }
    }

    /// The address a memory operand names.
    fn address(&self, instruction: &Instruction) -> Cell {
        let displacement = instruction.memory_displacement64();
        if instruction.is_ip_rel_memory_operand() {
            return Cell {
                value: Value::Number(instruction.ip_rel_memory_address()),
                state: false,
            };
        }

        let base = match instruction.memory_base() {
            Register::None => Cell {
                value: Value::Number(0),
                state: false,
            },
            register => self.register(register),
        };

        let index = match instruction.memory_index() {
            Register::None => Cell {
                value: Value::Number(0),
                state: false,
            },
            register => self.register(register),
        };

        let scale = u64::from(instruction.memory_index_scale());
        let state = base.state || index.state;
        let value = match (base.value, index.value) {
            (Value::Number(base), Value::Number(index)) => Value::Number(
                base.wrapping_add(index.wrapping_mul(scale))
                    .wrapping_add(displacement),
            ),
            (Value::Object(base), Value::Number(0)) => {
                Value::Object(base.wrapping_add(displacement))
            }
            (Value::Stack(base), Value::Number(0)) => {
                Value::Stack(base.wrapping_add(displacement.cast_signed()))
            }
            _ => Value::Unknown,
        };

        Cell { value, state }
    }

    /// Loads `size` bytes from the memory operand, extending by sign when
    /// `sign`.
    fn load(&self, instruction: &Instruction, size: usize, sign: bool) -> Cell {
        let address = self.address(instruction);

        match address.value {
            Value::Stack(at) => self.load_stack(at),
            Value::Object(at) => {
                if at == self.member.offset && size as u64 <= self.member.size && size > 0 {
                    Cell {
                        value: Value::Number(self.value & mask(size * 8)),
                        state: true,
                    }
                } else {
                    UNKNOWN
                }
            }
            Value::Number(at) if (1..=8).contains(&size) => {
                let Some(bytes) = self.image.data(at, size) else {
                    return UNKNOWN;
                };

                let mut word = [0_u8; 8];
                word[..size].copy_from_slice(bytes);
                let mut number = u64::from_le_bytes(word);
                if sign && size < 8 && number >> (size * 8 - 1) & 1 == 1 {
                    number |= !mask(size * 8);
                }

                Cell {
                    value: Value::Number(number),
                    state: address.state,
                }
            }
            _ => UNKNOWN,
        }
    }

    /// Writes `cell` to operand `operand`: a register, or a slot on the
    /// stack. Writing anywhere else is the state's own work.
    fn write(&mut self, instruction: &Instruction, operand: u32, cell: Cell) -> Next {
        match instruction.op_kind(operand) {
            OpKind::Register => {
                let register = instruction.op_register(operand);
                let Some(slot) = index(register) else {
                    return Next::Stop("a write to a register the model does not know");
                };

                self.registers[slot] = match (register.size(), cell.value) {
                    (8, _) => cell,
                    (4, Value::Number(number)) => Cell {
                        value: Value::Number(number & mask(32)),
                        state: cell.state,
                    },
                    _ => Cell {
                        value: Value::Unknown,
                        state: cell.state,
                    },
                };

                Next::Fall
            }
            OpKind::Memory => match self.address(instruction).value {
                Value::Stack(at) => {
                    let width = instruction.memory_size().size();
                    let stored = if width == 8 {
                        cell
                    } else {
                        Cell {
                            value: match cell.value {
                                Value::Number(number) => Value::Number(number & mask(width * 8)),
                                _ => Value::Unknown,
                            },
                            state: cell.state,
                        }
                    };

                    self.store_stack(at, stored);
                    Next::Fall
                }
                _ => Next::Stop("a store outside the stack"),
            },
            _ => Next::Stop("a write the model does not know"),
        }
    }
}

/// The index of a general register's full width, or `None` for another.
fn index(register: Register) -> Option<usize> {
    let full = register.full_register();
    let index = full as usize;
    let first = Register::RAX as usize;
    (full.is_gpr64() && index >= first).then(|| index - first)
}

const fn mask(bits: usize) -> u64 {
    if bits >= 64 {
        u64::MAX
    } else {
        (1 << bits) - 1
    }
}

fn operand_bits(instruction: &Instruction) -> usize {
    match instruction.op0_kind() {
        OpKind::Register => instruction.op0_register().size() * 8,
        OpKind::Memory => instruction.memory_size().size() * 8,
        _ => 64,
    }
    .clamp(8, 64)
}

fn subtraction_flags(a: u64, b: u64, bits: usize, state: bool) -> Flags {
    let (a, b) = (a & mask(bits), b & mask(bits));
    let result = a.wrapping_sub(b) & mask(bits);
    let sign_bit = |value: u64| value >> (bits - 1) & 1 == 1;

    Flags {
        zero: result == 0,
        carry: a < b,
        sign: sign_bit(result),
        overflow: sign_bit(a) != sign_bit(b) && sign_bit(result) != sign_bit(a),
        state,
    }
}

fn arithmetic_flags(
    mnemonic: Mnemonic,
    left: Value,
    right: Value,
    result: u64,
    bits: usize,
    state: bool,
) -> Flags {
    match (mnemonic, left, right) {
        (Mnemonic::Sub, Value::Number(a), Value::Number(b)) => subtraction_flags(a, b, bits, state),
        (Mnemonic::Add, Value::Number(a), Value::Number(b)) => {
            let sign_bit = |value: u64| value >> (bits - 1) & 1 == 1;
            let (a, b) = (a & mask(bits), b & mask(bits));
            Flags {
                zero: result == 0,
                carry: result < a,
                sign: sign_bit(result),
                overflow: sign_bit(a) == sign_bit(b) && sign_bit(result) != sign_bit(a),
                state,
            }
        }
        _ => Flags {
            zero: result == 0,
            carry: false,
            sign: result >> (bits - 1) & 1 == 1,
            overflow: false,
            state,
        },
    }
}

/// Whether a conditional branch is taken, or `None` for a condition on a
/// flag the model does not keep.
const fn condition(code: ConditionCode, flags: Flags) -> Option<bool> {
    Some(match code {
        ConditionCode::o => flags.overflow,
        ConditionCode::no => !flags.overflow,
        ConditionCode::b => flags.carry,
        ConditionCode::ae => !flags.carry,
        ConditionCode::e => flags.zero,
        ConditionCode::ne => !flags.zero,
        ConditionCode::be => flags.carry || flags.zero,
        ConditionCode::a => !flags.carry && !flags.zero,
        ConditionCode::s => flags.sign,
        ConditionCode::ns => !flags.sign,
        ConditionCode::l => flags.sign != flags.overflow,
        ConditionCode::ge => flags.sign == flags.overflow,
        ConditionCode::le => flags.zero || flags.sign != flags.overflow,
        ConditionCode::g => !flags.zero && flags.sign == flags.overflow,
        ConditionCode::None | ConditionCode::p | ConditionCode::np => return None,
    })
}

#[cfg(feature = "fuzzing")]
pub fn fuzz(data: &[u8]) {
    /// Code at 0x1000 from the input's tail, and data at 0x2000 from its
    /// head.
    struct Bytes<'a>(&'a [u8], &'a [u8]);

    impl DispatchImage for Bytes<'_> {
        fn code(&self, address: u64, length: usize) -> Option<&[u8]> {
            let at = usize::try_from(address.checked_sub(0x1000)?).ok()?;
            let bytes = self.1.get(at..)?;
            Some(&bytes[..bytes.len().min(length)])
        }

        fn data(&self, address: u64, length: usize) -> Option<&[u8]> {
            let at = usize::try_from(address.checked_sub(0x2000)?).ok()?;
            self.0.get(at..at.checked_add(length)?)
        }
    }

    let Some((&header, rest)) = data.split_first() else {
        return;
    };

    let split = rest.len() / 2;
    let image = Bytes(&rest[..split], &rest[split..]);
    let code = rest.len() - split;
    let ranges = [AddressRange {
        start: ImageAddress::new(0x1000),
        end: ImageAddress::new(0x1000 + code as u64),
    }];
    let state = StateMember {
        offset: u64::from(header & 0x3f),
        size: 1,
    };

    let states = (0..u64::from(header >> 6) + 3).collect::<Vec<_>>();

    if let Ok(points) = decode(&image, &ranges, ImageAddress::new(0x1000), state, &states) {
        assert_eq!(points.points.len(), states.len());
        for point in points.points.iter() {
            assert!(ranges[0].contains(point.address), "{point:?}");
            assert!(
                point
                    .resumption
                    .iter()
                    .all(|range| range.start <= range.end),
                "{point:?}"
            );
        }

        for range in points.dispatch.iter() {
            assert!(
                range.start < range.end && range.end <= ranges[0].end,
                "{range:?}"
            );
        }
    }
}
