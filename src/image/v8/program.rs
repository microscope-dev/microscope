//! The programs `slices.gen.rs` holds, and their interpreter: V8's
//! Torque compiler emits, in `debug-macros.cc`, a function for each slice
//! of a heap object whose offset and length depend on the object's flags
//! and counts. Each is translated mechanically, statement by statement,
//! into a [`Function`] of [`Op`]s over numbered registers, which this
//! interpreter runs with every read of memory through its caller.
//!
//! Every value is 64 bits: an `intptr_t` or `uintptr_t` as itself, a
//! `uint32_t` zero-extended, a `bool` as 0 or 1, and a tagged word as its
//! bits. The object is the tagged pointer [`OBJECT`], whose untagged
//! address is 0, so a field's address is its offset from the object's
//! start; a read of any other object is refused, as is a Smi that is not
//! one, an overflow, and a jump that does not go forward.

/// The tagged pointer of the object a slice function is given: V8's heap
/// object tag, 1, on the address 0.
pub(super) const OBJECT: i64 = 1;

/// The most calls one slice may nest. Translation refuses recursion, so
/// this only bounds a damaged program.
const MAX_DEPTH: usize = 32;

/// A register of a [`Function`]: its parameters come first.
pub(super) type Register = u8;

/// A value an [`Op`] reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Operand {
    Register(Register),
    Value(i64),
}

/// A `CodeStubAssembler` shim of two values.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Binary {
    IntPtrAdd,
    IntPtrMul,
    IntPtrLessThan,
    IntPtrLessThanOrEqual,
    UintPtrLessThan,
    Word32Equal,
    Word32NotEqual,
}

/// One statement of a translated function.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Op {
    /// `to = from`, as a copy, a constant, or a cast that keeps the bits.
    Set(Register, Operand),
    Binary(Binary, Register, Operand, Operand),

    /// `CodeStubAssembler::SmiUntag`: the Smi's value.
    SmiUntag(Register, Operand),

    /// `Internals::IntToSmi`: the Smi of a 32-bit value.
    IntToSmi(Register, Operand),

    /// `base::BitField<_, shift, size, uint32_t>::decode`, of a Smi's
    /// value when `smi` is set (`Internals::SmiValue`).
    Decode {
        to: Register,
        from: Operand,
        smi: bool,
        shift: u8,
        size: u8,
    },

    /// `READ_TAGGED_FIELD_OR_FAIL`: the tagged word at `offset`.
    ReadTagged {
        to: Register,
        object: Operand,
        offset: Operand,
    },

    /// `READ_FIELD_OR_FAIL(uint32_t, ...)`.
    ReadU32 {
        to: Register,
        object: Operand,
        offset: Operand,
    },

    /// `ASSIGN_OR_RETURN` of another translated function, by its index in
    /// the version's functions.
    Call {
        function: u16,
        arguments: &'static [Operand],
        results: &'static [Register],
    },

    /// `if (condition) goto then; else goto otherwise;`, by op index.
    Branch {
        condition: Operand,
        then: u16,
        otherwise: u16,
    },
    Goto(u16),
    Return(&'static [Operand]),

    /// `UNREACHABLE()`.
    Unreachable,
}

/// One translated function of `debug-macros.cc`.
#[derive(Debug)]
pub(super) struct Function {
    pub name: &'static str,
    pub parameters: u8,
    pub registers: u8,
    pub ops: &'static [Op],
}

/// One version's translation: its functions, and the one of each slice
/// of a `ScopeInfo`, by index.
#[derive(Debug)]
pub(super) struct Translated {
    pub major: u32,
    pub minor: u32,
    pub functions: &'static [Function],
    pub scope_info: &'static [(super::ScopeInfoSlice, u16)],
}

/// Reads the little-endian 64-bit word at a byte offset from the
/// object's untagged start, which is a multiple of 8.
pub(super) type Read<'a> = dyn FnMut(u64) -> Option<u64> + 'a;

/// The most values a call passes or a function returns.
const MAX_VALUES: usize = 8;

/// The values a call passes or a function returns, without allocating.
#[derive(Clone, Copy)]
struct Values {
    values: [i64; MAX_VALUES],
    length: usize,
}

impl Values {
    fn of(registers: &[i64], operands: &[Operand]) -> Result<Self, String> {
        let mut values = Self {
            values: [0; MAX_VALUES],
            length: operands.len(),
        };
        let slots = values
            .values
            .get_mut(..operands.len())
            .ok_or_else(|| format!("{} values at once", operands.len()))?;
        for (slot, &operand) in slots.iter_mut().zip(operands) {
            *slot = get(registers, operand)?;
        }
        Ok(values)
    }

    fn as_slice(&self) -> &[i64] {
        &self.values[..self.length]
    }
}

/// Runs `functions[function]` on `arguments`, giving what it returns.
pub(super) fn run(
    functions: &[Function],
    function: u16,
    arguments: &[i64],
    read: &mut Read<'_>,
) -> Result<Vec<i64>, String> {
    let mut passed = Values {
        values: [0; MAX_VALUES],
        length: arguments.len(),
    };
    passed
        .values
        .get_mut(..arguments.len())
        .ok_or_else(|| format!("{} arguments", arguments.len()))?
        .copy_from_slice(arguments);
    let mut interpreter = Interpreter {
        functions,
        read,
        stack: Vec::with_capacity(64),
    };
    Ok(interpreter.call(function, passed, 0)?.as_slice().to_vec())
}

struct Interpreter<'a, 'r> {
    functions: &'a [Function],
    read: &'a mut Read<'r>,

    /// Every running function's registers, the innermost last.
    stack: Vec<i64>,
}

impl Interpreter<'_, '_> {
    fn call(&mut self, index: u16, arguments: Values, depth: usize) -> Result<Values, String> {
        let function = self
            .functions
            .get(usize::from(index))
            .ok_or_else(|| format!("no translated function {index}"))?;
        if depth > MAX_DEPTH {
            return Err(format!("{} nests too deeply", function.name));
        }

        let arguments = arguments.as_slice();
        if arguments.len() != usize::from(function.parameters) {
            return Err(format!(
                "{} takes {} arguments, given {}",
                function.name,
                function.parameters,
                arguments.len()
            ));
        }

        let base = self.stack.len();
        self.stack.resize(base + usize::from(function.registers), 0);
        self.stack[base..]
            .get_mut(..arguments.len())
            .ok_or_else(|| format!("{} has too few registers", function.name))?
            .copy_from_slice(arguments);

        let mut pc = 0usize;
        loop {
            let op = function
                .ops
                .get(pc)
                .ok_or_else(|| format!("{} runs past its end", function.name))?;
            let next = pc.checked_add(1).ok_or("a program too long")?;
            let jump = |target: u16| {
                let target = usize::from(target);
                if target > pc {
                    Ok(target)
                } else {
                    Err(format!("{} jumps backward at {pc}", function.name))
                }
            };

            pc = match *op {
                Op::Branch {
                    condition,
                    then,
                    otherwise,
                } => jump(if get(&self.stack[base..], condition)? != 0 {
                    then
                } else {
                    otherwise
                })?,
                Op::Goto(target) => jump(target)?,
                Op::Return(values) => {
                    let returned = Values::of(&self.stack[base..], values)?;
                    self.stack.truncate(base);
                    return Ok(returned);
                }
                Op::Unreachable => {
                    return Err(format!("{} reached UNREACHABLE", function.name));
                }
                Op::Call {
                    function: callee,
                    arguments,
                    results,
                } => {
                    let values = Values::of(&self.stack[base..], arguments)?;
                    let returned = self.call(callee, values, depth + 1)?;
                    if returned.length != results.len() {
                        return Err(format!("{} misreads a call's results", function.name));
                    }

                    for (&register, &value) in results.iter().zip(returned.as_slice()) {
                        set(&mut self.stack[base..], register, value)?;
                    }
                    next
                }
                op => {
                    self.step(op, base)
                        .map_err(|error| format!("{}: {error}", function.name))?;
                    next
                }
            };
        }
    }

    /// Runs an op that computes one register of the frame at `base`.
    fn step(&mut self, op: Op, base: usize) -> Result<(), String> {
        let registers = &self.stack[base..];
        let (to, value) = match op {
            Op::Set(to, from) => (to, get(registers, from)?),
            Op::Binary(binary, to, left, right) => (
                to,
                apply(binary, get(registers, left)?, get(registers, right)?)?,
            ),
            Op::SmiUntag(to, from) => (to, smi_value(get(registers, from)?)?),
            Op::IntToSmi(to, from) => {
                let value = i32::try_from(get(registers, from)?)
                    .map_err(|_| "a Smi out of range".to_owned())?;
                (to, i64::from(value) << 32)
            }
            Op::Decode {
                to,
                from,
                smi,
                shift,
                size,
            } => {
                let mut word = get(registers, from)?;
                if smi {
                    word = smi_value(word)?;
                }

                let storage = low32(word);
                let mask = 1u32
                    .checked_shl(u32::from(size))
                    .map_or(u32::MAX, |bit| bit - 1);
                let value = storage.checked_shr(u32::from(shift)).unwrap_or(0) & mask;
                (to, i64::from(value))
            }
            Op::ReadTagged { to, object, offset } => {
                let offset = field_offset(get(registers, object)?, get(registers, offset)?)?;
                (to, self.word(offset)?.cast_signed())
            }
            Op::ReadU32 { to, object, offset } => {
                let offset = field_offset(get(registers, object)?, get(registers, offset)?)?;
                let within = offset % 8;
                if within > 4 {
                    return Err(format!("a uint32_t at +{offset} spans two words"));
                }

                let word = self.word(offset - within)?;
                (to, i64::from(low32((word >> (within * 8)).cast_signed())))
            }
            Op::Call { .. } | Op::Branch { .. } | Op::Goto(_) | Op::Return(_) | Op::Unreachable => {
                return Err("a control op as a statement".to_owned());
            }
        };

        set(&mut self.stack[base..], to, value)
    }

    fn word(&mut self, offset: u64) -> Result<u64, String> {
        (self.read)(offset).ok_or_else(|| format!("the word at +{offset} cannot be read"))
    }
}

fn get(registers: &[i64], operand: Operand) -> Result<i64, String> {
    match operand {
        Operand::Value(value) => Ok(value),
        Operand::Register(register) => registers
            .get(usize::from(register))
            .copied()
            .ok_or_else(|| format!("no register {register}")),
    }
}

fn set(registers: &mut [i64], register: Register, value: i64) -> Result<(), String> {
    let slot = registers
        .get_mut(usize::from(register))
        .ok_or_else(|| format!("no register {register}"))?;
    *slot = value;
    Ok(())
}

fn apply(binary: Binary, left: i64, right: i64) -> Result<i64, String> {
    let overflow = || format!("{binary:?}({left}, {right}) overflows");
    Ok(match binary {
        Binary::IntPtrAdd => left.checked_add(right).ok_or_else(overflow)?,
        Binary::IntPtrMul => left.checked_mul(right).ok_or_else(overflow)?,
        Binary::IntPtrLessThan => i64::from(left < right),
        Binary::IntPtrLessThanOrEqual => i64::from(left <= right),
        Binary::UintPtrLessThan => i64::from(left.cast_unsigned() < right.cast_unsigned()),
        Binary::Word32Equal => i64::from(low32(left) == low32(right)),
        Binary::Word32NotEqual => i64::from(low32(left) != low32(right)),
    })
}

/// The value of a 64-bit Smi, whose low 32 bits are zero.
fn smi_value(word: i64) -> Result<i64, String> {
    if word & 0xffff_ffff != 0 {
        return Err(format!("{:#x} is not a Smi", word.cast_unsigned()));
    }

    Ok(word >> 32)
}

/// The byte offset of a field of `object`, which must be the object a
/// slice function was given.
fn field_offset(object: i64, offset: i64) -> Result<u64, String> {
    if object != OBJECT {
        return Err(format!(
            "a read of another object, {:#x}",
            object.cast_unsigned()
        ));
    }

    u64::try_from(offset).map_err(|_| format!("a field at {offset}"))
}

/// The low 32 bits of a value, as C++ converts it to `uint32_t`.
#[expect(
    clippy::cast_possible_truncation,
    reason = "a conversion to uint32_t keeps the low 32 bits"
)]
const fn low32(value: i64) -> u32 {
    value.cast_unsigned() as u32
}
