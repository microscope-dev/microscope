//! Running a bound program at one stop.
//!
//! Places stay unread until a value is needed, `&&`, `||`, and `?:` run only
//! the side they need, and a value the program state cannot provide
//! poisons what depends on it, remembering the operand that caused it.

use std::cmp::Ordering;

use super::dynamic::{self, Number};
use super::error::{ErrorKind, ExpressionError};
use super::ir::{Capacity, Comparison, Constant, Conversion, KeyNode, Length, Node, Op, Program};
use super::number::{Bits, Exact, Float, FloatFormat, IntType, Integer, NumberError};
use super::syntax::Span;
use super::syntax::ast::BinaryOp;
use super::target::{
    Key, Loaded, LoadedValue, MAX_TUPLE_DEPTH, MAX_TUPLE_KEYS, Machine, Refusal, Register, Stop,
};
use super::types::{Category, Ty, category, size_of, type_info};
use crate::{
    ByteOrder, DereferenceState, InspectedValue, ScalarValue, TextCompletion,
    ValueAccessUnavailableReason, ValueChildren, VariableMalformedKind, VariableMalformedReason,
    VariableState, VariableUnavailableReason, VariableValue, VariableValueSource, VirtualAddress,
};

/// What running a program produced.
#[derive(Debug)]
pub enum Outcome<P> {
    /// A value, or the state that stood in for one, with the operand whose
    /// state the program could not provide.
    Value {
        value: InspectedValue,
        cause: Option<Span>,
    },

    /// A value to store: the target's place, the bytes of the value in the
    /// target's type, and whether the place is a whole variable rather
    /// than part of one.
    Assign {
        target: P,
        bytes: Vec<u8>,
        whole: bool,
        span: Span,
    },

    /// A value to give a variable of a dynamic type, which its runtime
    /// stores.
    AssignDynamic { value: Assigned<P>, span: Span },

    /// A value to store in a register: its bytes, as wide as the register.
    AssignRegister {
        register: Register,
        bytes: Vec<u8>,
        span: Span,
    },

    /// `base[start..end]`: the array or slice, and the range to page through.
    Range {
        base: InspectedValue,
        start: i128,
        end: i128,
    },
}

/// A value a variable of a dynamic type is given: one the evaluator
/// computed, or an object its runtime keeps, as another variable holds it.
#[derive(Debug, Clone, PartialEq)]
pub enum Assigned<P> {
    Integer(Exact),
    Float(f64),
    Bool(bool),
    Text(String),
    Nothing,
    Object(P),
}

/// Why running produced nothing.
#[derive(Debug)]
pub enum Failure {
    Expression(ExpressionError),
    Debugger(crate::Error),
}

/// What a node evaluates to before it is presented.
#[derive(Debug, Clone)]
pub enum Value<P> {
    /// Storage of a program type, read only when a value is needed.
    Place(P),

    /// A language-typed value in memory.
    Raw(u64),
    Int(Integer),
    Float(Float),
    Bool(bool),
    Pointer(u64),
    Text(Vec<u8>),

    /// A value of a dynamic type, loaded for an operation.
    Loaded(Loaded),
}

enum Halt {
    Missing {
        state: Box<VariableState>,
        cause: Span,
    },
    Error(ExpressionError),
    Failed(crate::Error),
}

/// Runs `program` on `machine`.
pub fn run<M: Machine>(
    program: &Program<M::Object, M::Step>,
    machine: &mut M,
) -> Result<Outcome<M::Place>, Failure> {
    let mut interpreter = Interpreter { machine };
    let root = &program.root;
    let result = if let Op::Range { base, start, end } = &root.op {
        interpreter.range(base, start.as_deref(), end.as_deref(), root.span)
    } else if let Op::Assign { target, value } = &root.op {
        interpreter.assign(target, value, root.span)
    } else {
        interpreter
            .eval(root)
            .and_then(|value| interpreter.present(root, value))
            .map(|value| Outcome::Value { value, cause: None })
    };

    match result {
        Ok(outcome) => Ok(outcome),
        Err(Halt::Missing { state, cause }) => Ok(Outcome::Value {
            value: interpreter
                .machine
                .finish(Some(type_info(interpreter.machine, &root.ty)), *state),
            cause: Some(cause),
        }),
        Err(Halt::Error(error)) => Err(Failure::Expression(error)),
        Err(Halt::Failed(error)) => Err(Failure::Debugger(error)),
    }
}

/// Evaluates `program` to its value without presenting it: a place stays
/// a place, and a scalar is read. A machine computes the values it binds
/// for a scope this way.
pub fn value<M: Machine>(
    program: &Program<M::Object, M::Step>,
    machine: &mut M,
) -> Result<Value<M::Place>, Stop> {
    let mut interpreter = Interpreter { machine };
    interpreter.eval(&program.root).map_err(|halt| match halt {
        Halt::Missing { state, .. } => Stop::Missing(state),
        Halt::Error(error) => Stop::Refused(Refusal::new(error.kind, error.message)),
        Halt::Failed(error) => Stop::Failed(error),
    })
}

struct Interpreter<'m, M: Machine> {
    machine: &'m mut M,
}

type Evaluated<P> = Result<Value<P>, Halt>;

const fn unavailable(reason: VariableUnavailableReason) -> VariableState {
    VariableState::Unavailable(reason)
}

fn malformed(description: &str) -> VariableState {
    VariableState::Malformed(VariableMalformedReason {
        kind: VariableMalformedKind::InvalidExpression,
        description: description.into(),
    })
}

impl<M: Machine> Interpreter<'_, M> {
    /// Attributes a machine's stop to the node that asked for it.
    fn at<T>(span: Span, result: Result<T, Stop>) -> Result<T, Halt> {
        result.map_err(|stop| match stop {
            Stop::Missing(state) => Halt::Missing { state, cause: span },
            Stop::Refused(Refusal { kind, message }) => {
                Halt::Error(ExpressionError::new(kind, span, message))
            }
            Stop::Failed(error) => Halt::Failed(error),
        })
    }

    fn error(span: Span, kind: ErrorKind, message: impl Into<String>) -> Halt {
        Halt::Error(ExpressionError::new(kind, span, message))
    }

    fn arithmetic(span: Span, error: NumberError) -> Halt {
        Self::error(span, ErrorKind::Arithmetic, error.to_string())
    }

    /// A broken invariant of the binder, which no expression can cause.
    fn invariant<T>(fact: &'static str) -> Result<T, Halt> {
        Err(Halt::Failed(crate::Error::internal(fact)))
    }

    fn integer(&mut self, node: &Node<M::Object, M::Step>) -> Result<Integer, Halt> {
        match self.eval(node)? {
            Value::Int(integer) => Ok(integer),
            _ => Self::invariant("the binder made this operand an integer"),
        }
    }

    fn truth(&mut self, node: &Node<M::Object, M::Step>) -> Result<bool, Halt> {
        match self.eval(node)? {
            Value::Bool(value) => Ok(value),
            _ => Self::invariant("the binder made this operand a truth value"),
        }
    }

    fn pointer(&mut self, node: &Node<M::Object, M::Step>) -> Result<u64, Halt> {
        match self.eval(node)? {
            Value::Pointer(address) => Ok(address),
            _ => Self::invariant("the binder made this operand a pointer"),
        }
    }

    fn float(&mut self, node: &Node<M::Object, M::Step>) -> Result<Float, Halt> {
        match self.eval(node)? {
            Value::Float(value) => Ok(value),
            _ => Self::invariant("the binder made this operand a float"),
        }
    }

    /// An operand of an operation on a value of a dynamic type, as it
    /// runs: that value loaded, or a number, truth value, or string.
    fn dynamic(&mut self, node: &Node<M::Object, M::Step>) -> Result<Loaded, Halt> {
        let value = match self.eval(node)? {
            Value::Loaded(loaded) => return Ok(loaded),
            Value::Int(integer) => LoadedValue::Int(integer.value()),
            Value::Float(value) => LoadedValue::Float(value),
            Value::Bool(value) => LoadedValue::Bool(value),
            Value::Text(bytes) => LoadedValue::Text(crate::TextSummary {
                bytes: bytes.into(),
                completion: TextCompletion::Complete,
            }),
            _ => return Self::invariant("the binder loads the operands of dynamic operations"),
        };

        let ty = match (&node.ty, &value) {
            (Ty::Dynamic, LoadedValue::Float(value)) => Ty::Float(value.format()),
            (Ty::Dynamic, _) => Ty::Exact,
            (ty, _) => ty.clone(),
        };

        Ok(Loaded {
            type_name: super::types::type_name(self.machine, &ty).into(),
            value,
        })
    }

    const fn number(number: Number) -> Value<M::Place> {
        match number {
            Number::Int(value) => Value::Int(Integer::Exact(value)),
            Number::Float(value) => Value::Float(value),
        }
    }

    fn place(&mut self, node: &Node<M::Object, M::Step>) -> Result<M::Place, Halt> {
        match self.eval(node)? {
            Value::Place(place) => Ok(place),
            _ => Self::invariant("the binder made this operand a program place"),
        }
    }

    /// The address a pointer points at, unavailable when it is null.
    fn pointee_address(
        &mut self,
        node: &Node<M::Object, M::Step>,
        span: Span,
    ) -> Result<u64, Halt> {
        match self.pointer(node)? {
            0 => Err(Halt::Missing {
                state: Box::new(unavailable(VariableUnavailableReason::ValueAccess(
                    ValueAccessUnavailableReason::NullPointer,
                ))),
                cause: span,
            }),
            address => Ok(address),
        }
    }

    /// An index value as `i128`, which every index the debugger can reach
    /// fits.
    fn index(&mut self, node: &Node<M::Object, M::Step>) -> Result<i128, Halt> {
        let value = self.integer(node)?.value();
        value.to_i128().ok_or_else(|| {
            Self::error(
                node.span,
                ErrorKind::Bounds,
                format!("the index {value} is out of bounds"),
            )
        })
    }

    fn address_width(&self) -> u32 {
        u32::from(self.machine.pointer_size()) * 8
    }

    #[expect(clippy::too_many_lines, reason = "one arm per operation")]
    fn eval(&mut self, node: &Node<M::Object, M::Step>) -> Evaluated<M::Place> {
        let span = node.span;
        Self::at(span, self.machine.charge())?;
        Ok(match &node.op {
            Op::Object(object) => Value::Place(Self::at(span, self.machine.locate(object))?),
            Op::Bound(object) => Self::at(span, self.machine.bound(object))?,
            Op::Step {
                base,
                step,
                indices,
                ..
            } => {
                let mut values = Vec::with_capacity(indices.len());
                for index in indices {
                    values.push(self.index(index)?);
                }

                Self::at(span, self.machine.check_indices(step, &values))?;
                let base = self.place(base)?;
                Value::Place(Self::at(span, self.machine.step(&base, step, &values))?)
            }
            Op::Holds { base, step, negate } => {
                let base = self.place(base)?;
                let holds = match self.machine.step(&base, step, &[]) {
                    Ok(_) => true,
                    Err(Stop::Missing(state))
                        if matches!(
                            *state,
                            VariableState::Unavailable(VariableUnavailableReason::ValueAccess(
                                ValueAccessUnavailableReason::InactiveVariant(_)
                            ))
                        ) =>
                    {
                        false
                    }
                    Err(stop) => return Self::at(span, Err(stop)),
                };

                Value::Bool(holds != *negate)
            }
            Op::Entry { base, step, key } => {
                let key = self.key(key)?;
                let map = self.place(base)?;
                match Self::at(span, self.machine.entry(&map, step, &key))? {
                    Some(place) => Value::Place(place),
                    None => {
                        return Err(Self::error(
                            span,
                            ErrorKind::MissingKey,
                            format!(
                                "`{}` holds no key {key}",
                                super::types::type_name(self.machine, &base.ty)
                            ),
                        ));
                    }
                }
            }
            Op::Task => Value::Int(Integer::Exact(Exact::from(u128::from(Self::at(
                span,
                self.machine.task(),
            )?)))),
            Op::At { address, pointee } => {
                let address = self.pointee_address(address, span)?;
                Value::Place(Self::at(span, self.machine.place_at(address, *pointee))?)
            }
            Op::Raw { address } => Value::Raw(self.pointee_address(address, span)?),
            Op::Interpreted { ty, address } => {
                let address = self.pointee_address(address, span)?;
                Value::Place(Self::at(span, self.machine.dynamic_at(ty, address))?)
            }
            Op::Load(place) => match self.eval(place)? {
                Value::Place(at) if matches!(node.ty, Ty::Dynamic) => {
                    Value::Loaded(Self::at(span, self.machine.load_dynamic(&at))?)
                }
                Value::Place(at) => {
                    let loaded = Self::at(span, self.machine.load(&at))?;
                    Self::at(span, self.loaded(&node.ty, loaded))?
                }
                Value::Raw(address) => self.read_raw(&node.ty, address, span)?,
                other => other,
            },
            Op::Register(register) => {
                let raw = Self::at(span, self.machine.register(register))?;
                let Ty::Int(int) = node.ty else {
                    return Self::invariant("registers are unsigned integers");
                };

                Value::Int(Integer::Typed(Bits::from_raw(int, raw)))
            }
            Op::Constant(constant) => match constant {
                Constant::Integer(integer) => Value::Int(*integer),
                Constant::Float(value) => Value::Float(*value),
                Constant::Bool(value) => Value::Bool(*value),
                Constant::Pointer(address) => Value::Pointer(*address),
                Constant::Text(bytes) => Value::Text(bytes.clone()),
            },
            Op::AddressOf(place) | Op::Decay(place) => match self.eval(place)? {
                Value::Place(at) => {
                    Value::Pointer(Self::at(place.span, self.machine.address(&at))?)
                }
                Value::Raw(address) => Value::Pointer(address),
                _ => return Self::invariant("the binder takes addresses of places only"),
            },
            Op::Negate(operand) => {
                let value = self.integer(operand)?.value();
                Value::Int(Integer::Exact(
                    value.neg().map_err(|error| Self::arithmetic(span, error))?,
                ))
            }
            Op::FloatNegate(operand) => Value::Float(self.float(operand)?.neg()),
            Op::Not(operand) => Value::Bool(!self.truth(operand)?),
            Op::BitNot(operand) => Value::Int(
                self.integer(operand)?
                    .not()
                    .map_err(|error| Self::arithmetic(span, error))?,
            ),
            Op::Arithmetic { op, left, right } => {
                let left = self.integer(left)?.value();
                let right = self.integer(right)?.value();
                let result = match op {
                    BinaryOp::Add => left.add(right),
                    BinaryOp::Sub => left.sub(right),
                    BinaryOp::Mul => left.mul(right),
                    BinaryOp::Div => left.div(right),
                    _ => left.rem(right),
                };

                Value::Int(Integer::Exact(
                    result.map_err(|error| Self::arithmetic(span, error))?,
                ))
            }
            Op::FloatArithmetic { op, left, right } => {
                let left = self.float(left)?;
                let right = self.float(right)?;
                Value::Float(Float::binary(*op, left, right))
            }
            Op::DynamicArithmetic { op, left, right } => {
                let left = self.dynamic(left)?;
                let right = self.dynamic(right)?;
                Self::number(Self::at(span, dynamic::binary(*op, &left, &right))?)
            }
            Op::DynamicUnary { op, operand } => {
                let operand = self.dynamic(operand)?;
                Self::number(Self::at(span, dynamic::unary(*op, &operand))?)
            }
            Op::Bitwise { op, left, right } => {
                let left_value = self.integer(left)?;
                let right_value = self.integer(right)?;
                let combined = left_value.bitwise(*op, right_value).map_err(|error| {
                    // Point at the exact operand that does not fit.
                    let culprit = if matches!(left_value, Integer::Exact(_)) {
                        left.span
                    } else {
                        right.span
                    };

                    Self::arithmetic(culprit, error)
                })?;
                Value::Int(self.retype(&node.ty, combined))
            }

            Op::Shift {
                left,
                value,
                amount,
            } => {
                let shifted = self.integer(value)?;
                let amount = self.integer(amount)?.value();
                let result = if *left {
                    shifted.shl(amount)
                } else {
                    shifted.shr(amount)
                };

                Value::Int(result.map_err(|error| Self::arithmetic(span, error))?)
            }

            Op::Compare {
                op,
                how,
                left,
                right,
            } => Value::Bool(self.compare(*op, *how, left, right)?),
            Op::Logical { and, left, right } => {
                let left = self.truth(left)?;
                if left == *and {
                    Value::Bool(self.truth(right)?)
                } else {
                    Value::Bool(left)
                }
            }

            Op::Choose {
                condition,
                then,
                otherwise,
                ..
            } => {
                if self.truth(condition)? {
                    self.eval(then)?
                } else {
                    self.eval(otherwise)?
                }
            }

            Op::Convert {
                operand,
                conversion,
            } => self.convert(&node.ty, operand, *conversion, span)?,
            Op::Offset {
                pointer,
                count,
                scale,
                backward,
            } => {
                let address = Exact::from(u128::from(self.pointer(pointer)?));
                let count = self.integer(count)?.value();
                let bytes = count
                    .mul(Exact::from(u128::from(*scale)))
                    .map_err(|error| Self::arithmetic(span, error))?;
                let moved = if *backward {
                    address.sub(bytes)
                } else {
                    address.add(bytes)
                }
                .map_err(|error| Self::arithmetic(span, error))?;
                Value::Pointer(self.address_value(moved, span)?)
            }
            Op::Difference { left, right, scale } => {
                let left = Exact::from(u128::from(self.pointer(left)?));
                let right = Exact::from(u128::from(self.pointer(right)?));
                let bytes = left
                    .sub(right)
                    .map_err(|error| Self::arithmetic(span, error))?;
                let scale = Exact::from(u128::from(*scale));
                let apart = bytes
                    .rem(scale)
                    .map_err(|error| Self::arithmetic(span, error))?;
                if !apart.is_zero() {
                    return Err(Self::error(
                        span,
                        ErrorKind::Arithmetic,
                        "the pointers are not a whole number of elements apart",
                    ));
                }

                Value::Int(Integer::Exact(
                    bytes
                        .div(scale)
                        .map_err(|error| Self::arithmetic(span, error))?,
                ))
            }

            Op::Length {
                operand,
                how: Length::Bytes,
            } => {
                let Value::Text(bytes) = self.eval(operand)? else {
                    return Self::invariant("the binder measures strings as strings");
                };

                Value::Int(Integer::Exact(Exact::from(bytes.len() as u128)))
            }
            Op::Length { operand, how } => {
                let place = self.place(operand)?;
                let length = match how {
                    Length::Slice => Self::at(span, self.machine.length(&place))?,
                    Length::Dynamic => Self::at(span, self.machine.presented_length(&place))?
                        .ok_or_else(|| {
                            Self::error(span, ErrorKind::Type, "the value has no length")
                        })?,
                    _ => self.text_length(&place, operand.span)?,
                };

                Value::Int(Integer::Exact(Exact::from(u128::from(length))))
            }
            Op::Capacity { operand, how } => {
                let place = self.place(operand)?;
                let capacity = match how {
                    Capacity::Slice => Self::at(span, self.machine.capacity(&place))?,
                    Capacity::Presented => {
                        Self::at(operand.span, self.machine.presented_capacity(&place))?
                            .ok_or_else(|| {
                                Self::error(
                                    operand.span,
                                    ErrorKind::Type,
                                    format!(
                                        "no view of `{}` gives it a capacity",
                                        super::types::type_name(self.machine, &operand.ty)
                                    ),
                                )
                            })?
                    }
                };

                Value::Int(Integer::Exact(Exact::from(u128::from(capacity))))
            }
            Op::TextSlice { base, start, end } => {
                self.text_slice(base, start.as_deref(), end.as_deref(), span)?
            }
            Op::Fit(operand) => self.fit(&node.ty, operand)?,
            Op::Range { .. } | Op::Assign { .. } => {
                return Self::invariant(
                    "ranges and assignments are only ever the whole expression",
                );
            }
        })
    }

    /// An integer result of a bit operation, in the node's type.
    fn retype(&self, ty: &Ty, integer: Integer) -> Integer {
        match (category(self.machine, ty), integer) {
            (Category::Integer { int: Some(int), .. }, Integer::Typed(bits))
                if bits.ty() != int =>
            {
                Integer::Typed(bits.cast(int))
            }
            (_, integer) => integer,
        }
    }

    /// An address as a pointer value, which must fit the target's width.
    fn address_value(&self, address: Exact, span: Span) -> Result<u64, Halt> {
        let width = self.address_width();
        address
            .to_u128()
            .filter(|&address| width >= 128 || address >> width == 0)
            .and_then(|address| u64::try_from(address).ok())
            .ok_or_else(|| {
                Self::error(
                    span,
                    ErrorKind::Arithmetic,
                    format!("the address {address} is outside the address space"),
                )
            })
    }

    /// A value decoded from a place of type `ty`.
    fn loaded(&self, ty: &Ty, value: VariableValue) -> Result<Value<M::Place>, Stop> {
        let integer = Exact::from;
        Ok(match (category(self.machine, ty), value) {
            (
                Category::Integer { int: Some(int), .. },
                VariableValue::Scalar(ScalarValue::Signed(value)),
            ) => Value::Int(Integer::Typed(Bits::truncate(int, Exact::from(value)))),
            (
                Category::Integer { int: Some(int), .. },
                VariableValue::Scalar(ScalarValue::Unsigned(value)),
            ) => Value::Int(Integer::Typed(Bits::truncate(int, Exact::from(value)))),
            (
                Category::Integer { int: Some(int), .. },
                VariableValue::Enumeration { value, .. },
            ) => Value::Int(Integer::Typed(Bits::truncate(int, integer(value)))),
            (Category::Float(format), VariableValue::Scalar(ScalarValue::Floating(value))) => {
                Value::Float(Float::from_value(value).convert(format))
            }
            (Category::Bool, VariableValue::Scalar(ScalarValue::Boolean(value))) => {
                Value::Bool(value)
            }
            (Category::Pointer(_), VariableValue::Address(address)) => {
                Value::Pointer(address.address.get())
            }
            (Category::Pointer(_), VariableValue::ImplicitPointer) => {
                return Err(Stop::missing(unavailable(
                    crate::UnsupportedVariableFeature::ImplicitPointer.into(),
                )));
            }
            _ => {
                return Err(Stop::missing(malformed(
                    "the value's representation does not match its type",
                )));
            }
        })
    }

    fn read_raw(&mut self, ty: &Ty, address: u64, span: Span) -> Evaluated<M::Place> {
        let size = size_of(self.machine, ty).and_then(|size| usize::try_from(size).ok());
        let Some(size) = size else {
            return Err(Self::error(span, ErrorKind::Type, "the value has no size"));
        };

        let bytes = Self::at(span, self.machine.read(address, size))?;
        Self::at(span, self.decode(ty, &bytes))
    }

    /// Decodes a language type's bytes.
    fn decode(&self, ty: &Ty, bytes: &[u8]) -> Result<Value<M::Place>, Stop> {
        let mut ordered = bytes.to_vec();
        if self.machine.byte_order() == ByteOrder::Big {
            ordered.reverse();
        }

        let mut wide = [0_u8; 16];
        let length = ordered.len().min(16);
        wide[..length].copy_from_slice(&ordered[..length]);
        let raw = u128::from_le_bytes(wide);
        Ok(match category(self.machine, ty) {
            Category::Integer { int: Some(int), .. } => {
                Value::Int(Integer::Typed(Bits::from_raw(int, raw)))
            }
            Category::Bool => Value::Bool(raw != 0),
            Category::Pointer(_) => Value::Pointer(u64::try_from(raw).unwrap_or(u64::MAX)),
            Category::Float(format) => {
                let value = match format {
                    FloatFormat::Binary16 => {
                        crate::FloatValue::Binary16(u16::try_from(raw).unwrap_or_default())
                    }
                    FloatFormat::BFloat16 => {
                        crate::FloatValue::BFloat16(u16::try_from(raw).unwrap_or_default())
                    }
                    FloatFormat::Binary128 => crate::FloatValue::Binary128(raw),
                    FloatFormat::Binary32 => {
                        crate::FloatValue::Binary32(u32::try_from(raw).unwrap_or_default())
                    }
                    FloatFormat::Binary64 => {
                        crate::FloatValue::Binary64(u64::try_from(raw).unwrap_or_default())
                    }
                    FloatFormat::X87Extended => crate::FloatValue::X87Extended {
                        significand: u64::try_from(raw & u128::from(u64::MAX)).unwrap_or_default(),
                        sign_exponent: u16::try_from((raw >> 64) & 0xffff).unwrap_or_default(),
                    },
                };

                Value::Float(Float::from_value(value))
            }
            _ => return Err(Stop::missing(malformed("the value is not a scalar"))),
        })
    }

    fn convert(
        &mut self,
        ty: &Ty,
        operand: &Node<M::Object, M::Step>,
        conversion: Conversion,
        span: Span,
    ) -> Evaluated<M::Place> {
        Ok(match conversion {
            Conversion::Truncate(int) => Value::Int(Integer::Typed(Bits::truncate(
                int,
                self.integer(operand)?.value(),
            ))),
            Conversion::IntegerToFloat(format) => {
                Value::Float(Float::from_exact(self.integer(operand)?.value(), format))
            }
            Conversion::FloatToInteger(int) => Value::Int(Integer::Typed(
                self.float(operand)?
                    .to_int(int)
                    .map_err(|error| Self::arithmetic(span, error))?,
            )),
            Conversion::FloatToFloat(format) => Value::Float(self.float(operand)?.convert(format)),
            Conversion::Truth => Value::Bool(match self.eval(operand)? {
                Value::Int(integer) => !integer.value().is_zero(),
                Value::Float(value) => !value.is_zero(),
                Value::Pointer(address) => address != 0,
                Value::Bool(value) => value,
                Value::Loaded(loaded) => Self::at(span, dynamic::truth(&loaded))?,
                _ => return Self::invariant("the binder converts only scalars to truth values"),
            }),
            Conversion::BoolToInteger(int) => {
                let value = u128::from(self.truth(operand)?);
                Value::Int(Integer::Typed(Bits::from_raw(int, value)))
            }
            Conversion::PointerToInteger(int) => {
                let address = u128::from(self.pointer(operand)?);
                Value::Int(Integer::Typed(Bits::truncate(int, Exact::from(address))))
            }
            Conversion::IntegerToPointer => {
                let value = self.integer(operand)?.value();
                let width = u8::try_from(self.address_width()).unwrap_or(64);
                let Some(int) = IntType::new(width, false) else {
                    return Err(Self::error(
                        span,
                        ErrorKind::Unsupported,
                        format!("a {width}-bit address is not a width microscope computes with"),
                    ));
                };

                let bits = Bits::truncate(int, value);
                Value::Pointer(u64::try_from(bits.raw()).unwrap_or(u64::MAX))
            }
            Conversion::PointerToPointer => Value::Pointer(self.pointer(operand)?),
        })
        .map(|value| {
            // A cast to an enumeration or typedef keeps its own type.
            if let Value::Int(integer) = value {
                Value::Int(self.retype(ty, integer))
            } else {
                value
            }
        })
    }

    fn compare(
        &mut self,
        op: BinaryOp,
        how: Comparison,
        left: &Node<M::Object, M::Step>,
        right: &Node<M::Object, M::Step>,
    ) -> Result<bool, Halt> {
        let ordering = match how {
            Comparison::Integers => {
                let left = self.integer(left)?.value();
                Some(left.cmp(&self.integer(right)?.value()))
            }
            Comparison::Floats => {
                let left = self.float(left)?;
                left.compare(self.float(right)?)
            }
            Comparison::FloatInteger => {
                let left = self.float(left)?;
                left.compare_exact(self.integer(right)?.value())
            }
            Comparison::IntegerFloat => {
                let left = self.integer(left)?.value();
                self.float(right)?
                    .compare_exact(left)
                    .map(Ordering::reverse)
            }
            Comparison::Addresses => {
                let left = self.pointer_or_zero(left)?;
                Some(left.cmp(&self.pointer_or_zero(right)?))
            }
            Comparison::Bools => {
                let left = self.truth(left)?;
                Some(left.cmp(&self.truth(right)?))
            }
            Comparison::Texts => {
                let (Value::Text(left), Value::Text(right)) = (self.eval(left)?, self.eval(right)?)
                else {
                    return Self::invariant("the binder compares strings with strings");
                };

                Some(if left == right {
                    Ordering::Equal
                } else {
                    Ordering::Less
                })
            }
            Comparison::Dynamic => {
                let span = left.span.to(right.span);
                let left = self.dynamic(left)?;
                let right = self.dynamic(right)?;
                return Self::at(span, dynamic::compare(op, &left, &right));
            }
            Comparison::Text => {
                let place = self.place(left)?;
                let Value::Text(literal) = self.eval(right)? else {
                    return Self::invariant("the binder compares text with a string");
                };

                let equal = self.text_equals(&place, &literal, left.span)?;
                Some(if equal {
                    Ordering::Equal
                } else {
                    Ordering::Less
                })
            }
        };

        Ok(match (op, ordering) {
            (BinaryOp::Ne, None) => true,
            (_, None) => false,
            (BinaryOp::Eq, Some(order)) => order == Ordering::Equal,
            (BinaryOp::Ne, Some(order)) => order != Ordering::Equal,
            (BinaryOp::Lt, Some(order)) => order == Ordering::Less,
            (BinaryOp::Le, Some(order)) => order != Ordering::Greater,
            (BinaryOp::Gt, Some(order)) => order == Ordering::Greater,
            (BinaryOp::Ge, Some(order)) => order != Ordering::Less,
            _ => return Self::invariant("only comparisons compare"),
        })
    }

    /// A pointer, or an integer the binder allowed only as zero.
    fn pointer_or_zero(&mut self, node: &Node<M::Object, M::Step>) -> Result<u64, Halt> {
        match self.eval(node)? {
            Value::Pointer(address) => Ok(address),
            Value::Int(_) => Ok(0),
            _ => Self::invariant("the binder compares pointers with pointers and zero"),
        }
    }

    fn text_equals(&mut self, place: &M::Place, literal: &[u8], span: Span) -> Result<bool, Halt> {
        let Some(text) = Self::at(span, self.machine.text(place))? else {
            return Err(Self::error(
                span,
                ErrorKind::Type,
                "the value holds no text",
            ));
        };

        let read = text.bytes.as_ref();
        match text.completion {
            TextCompletion::Complete => Ok(read == literal),
            // Text known to differ before what could not be read differs.
            _ if !literal.starts_with(read) => Ok(false),
            completion => Err(Halt::Missing {
                state: Box::new(unavailable(incomplete(completion))),
                cause: span,
            }),
        }
    }

    fn text_length(&mut self, place: &M::Place, span: Span) -> Result<u64, Halt> {
        if let Some(length) = Self::at(span, self.machine.presented_length(place))? {
            return Ok(length);
        }

        let Some(text) = Self::at(span, self.machine.text(place))? else {
            return Err(Self::error(
                span,
                ErrorKind::Type,
                "the value holds no text",
            ));
        };

        match text.completion {
            TextCompletion::Complete => Ok(u64::try_from(text.bytes.len()).unwrap_or(u64::MAX)),
            TextCompletion::Truncated {
                length: Some(length),
            }
            | TextCompletion::Limited {
                length: Some(length),
                ..
            } => Ok(length),
            completion => Err(Halt::Missing {
                state: Box::new(unavailable(incomplete(completion))),
                cause: span,
            }),
        }
    }

    fn assign(
        &mut self,
        target: &Node<M::Object, M::Step>,
        value: &Node<M::Object, M::Step>,
        span: Span,
    ) -> Result<Outcome<M::Place>, Halt> {
        if matches!(target.ty, Ty::Dynamic) {
            let assigned = self.eval(value)?;
            return Ok(Outcome::AssignDynamic {
                value: Self::assigned(assigned, value.span)?,
                span,
            });
        }

        let fitted = self.eval(value)?;
        if let Op::Register(register) = &target.op {
            return Ok(Outcome::AssignRegister {
                register: register.clone(),
                bytes: self.encode(&fitted, usize::from(register.width / 8)),
                span,
            });
        }

        let place = self.place(target)?;
        let size = size_of(self.machine, &target.ty)
            .and_then(|size| usize::try_from(size).ok())
            .ok_or_else(|| Self::error(target.span, ErrorKind::Type, "the target has no size"))?;
        Ok(Outcome::Assign {
            target: place,
            bytes: self.encode(&fitted, size),
            whole: matches!(target.op, Op::Object(_)),
            span,
        })
    }

    /// What a variable of a dynamic type is given for `value`: a number
    /// exactly as computed, text that is whole UTF-8, or an object as
    /// another variable holds it.
    fn assigned(value: Value<M::Place>, span: Span) -> Result<Assigned<M::Place>, Halt> {
        let refuse = |what: &str| {
            Self::error(
                span,
                ErrorKind::Assignment,
                format!("{what} cannot be given to a variable of a dynamic type"),
            )
        };
        let float = |float: Float| {
            let wide = float.convert(FloatFormat::Binary64);
            let same = float.is_nan()
                || wide.convert(float.format()).compare(float) == Some(std::cmp::Ordering::Equal);
            match (same, wide.to_value()) {
                (true, crate::FloatValue::Binary64(bits)) => {
                    Ok(Assigned::Float(f64::from_bits(bits)))
                }
                _ => Err(refuse("a float wider than 64 bits")),
            }
        };

        match value {
            Value::Int(integer) => Ok(Assigned::Integer(integer.value())),
            Value::Float(value) => float(value),
            Value::Bool(value) => Ok(Assigned::Bool(value)),
            Value::Text(bytes) => String::from_utf8(bytes)
                .map(Assigned::Text)
                .map_err(|_| refuse("text that is not UTF-8")),
            Value::Place(place) => Ok(Assigned::Object(place)),
            Value::Loaded(loaded) => match loaded.value {
                LoadedValue::Nothing => Ok(Assigned::Nothing),
                LoadedValue::Bool(value) => Ok(Assigned::Bool(value)),
                LoadedValue::Int(value) => Ok(Assigned::Integer(value)),
                LoadedValue::Float(value) => float(value),
                _ => Err(refuse("a value of this type")),
            },
            Value::Raw(_) | Value::Pointer(_) => Err(refuse("a value of a program's type")),
        }
    }

    /// A value converted to `ty` only if `ty` holds it exactly.
    fn fit(&mut self, ty: &Ty, operand: &Node<M::Object, M::Step>) -> Evaluated<M::Place> {
        let span = operand.span;
        let value = self.eval(operand)?;
        let shown = match &value {
            Value::Int(integer) => integer.value().to_string(),
            Value::Float(float) => float.to_string(),
            Value::Bool(value) => value.to_string(),
            Value::Pointer(address) => format!("{address:#x}"),
            _ => "the value".to_owned(),
        };

        let refuse = |interpreter: &Self| {
            Self::error(
                span,
                ErrorKind::Assignment,
                format!(
                    "{shown} does not fit `{}` exactly",
                    super::types::type_name(interpreter.machine, ty)
                ),
            )
        };

        let exact_integer = |value: &Value<M::Place>| match value {
            Value::Int(integer) => Some(integer.value()),
            Value::Bool(value) => Some(Exact::from(u128::from(*value))),
            Value::Pointer(address) => Some(Exact::from(u128::from(*address))),
            Value::Float(float) => float.to_int(IntType::I128).ok().and_then(|bits| {
                (float.compare_exact(bits.value()) == Some(std::cmp::Ordering::Equal))
                    .then(|| bits.value())
            }),
            _ => None,
        };

        Ok(match category(self.machine, ty) {
            Category::Integer { int: Some(int), .. } => {
                let bits = exact_integer(&value)
                    .and_then(|exact| Bits::exactly(int, exact).ok())
                    .ok_or_else(|| refuse(self))?;
                Value::Int(Integer::Typed(bits))
            }
            Category::Float(format) => {
                let fitted = match value {
                    Value::Float(float) => {
                        let converted = float.convert(format);
                        let round_trip = converted.convert(float.format());
                        let same = float.is_nan()
                            || round_trip.compare(float) == Some(std::cmp::Ordering::Equal);
                        same.then_some(converted)
                    }
                    Value::Int(integer) => {
                        let converted = Float::from_exact(integer.value(), format);
                        (converted.compare_exact(integer.value())
                            == Some(std::cmp::Ordering::Equal))
                        .then_some(converted)
                    }
                    _ => None,
                };

                Value::Float(fitted.ok_or_else(|| refuse(self))?)
            }
            Category::Bool => match exact_integer(&value) {
                Some(exact) if exact.is_zero() => Value::Bool(false),
                Some(exact) if exact == Exact::from(1_u128) => Value::Bool(true),
                _ => return Err(refuse(self)),
            },
            Category::Pointer(_) => {
                let exact = exact_integer(&value).ok_or_else(|| refuse(self))?;
                Value::Pointer(self.address_value(exact, span)?)
            }
            _ => return Err(refuse(self)),
        })
    }

    /// The elements `start..end` of an array or slice, checked against
    /// its bounds when its value says what they are; a bound left out is
    /// the first element, or the end.
    fn range(
        &mut self,
        base: &Node<M::Object, M::Step>,
        start: Option<&Node<M::Object, M::Step>>,
        end: Option<&Node<M::Object, M::Step>>,
        span: Span,
    ) -> Result<Outcome<M::Place>, Halt> {
        let start = start.map(|start| self.index(start)).transpose()?;
        let end = end.map(|end| self.index(end)).transpose()?;
        let place = self.place(base)?;
        let presented = Self::at(base.span, self.machine.present(&place))?;
        let bounds = match &presented.state {
            VariableState::Available {
                value: VariableValue::Array { dimensions },
                ..
            } => dimensions
                .first()
                .map(|dimension| (dimension.lower_bound, dimension.count)),
            VariableState::Available {
                value: VariableValue::Slice { length, .. },
                ..
            } => Some((0, *length)),
            _ => None,
        };

        let (start, end) = match bounds {
            Some((lower, count)) => {
                let upper = lower + i128::from(count);
                let (start, end) = (start.unwrap_or(lower), end.unwrap_or(upper));
                if start < lower || start > end || end > upper {
                    return Err(Self::error(
                        span,
                        ErrorKind::Bounds,
                        format!("{start}..{end} is outside {lower}..{upper}"),
                    ));
                }

                (start, end)
            }

            // A value that could not be read, or is not one dimension, is
            // the page's to report.
            None => (start.unwrap_or(0), end.unwrap_or(0)),
        };

        Ok(Outcome::Range {
            base: presented,
            start,
            end,
        })
    }

    /// The bytes of text between two bounds, as a string; a bound left out
    /// is the start, or the end.
    fn text_slice(
        &mut self,
        base: &Node<M::Object, M::Step>,
        start: Option<&Node<M::Object, M::Step>>,
        end: Option<&Node<M::Object, M::Step>>,
        span: Span,
    ) -> Evaluated<M::Place> {
        let start = start.map(|start| self.index(start)).transpose()?;
        let end = end.map(|end| self.index(end)).transpose()?;

        // A string the expression computed is sliced as it is; the
        // program's text is read where it is.
        let (computed, stored) = match self.eval(base)? {
            Value::Text(bytes) => (Some(bytes), None),
            Value::Place(place) => {
                let stored =
                    Self::at(base.span, self.machine.text_span(&place))?.ok_or_else(|| {
                        Self::error(base.span, ErrorKind::Type, "the value holds no text")
                    })?;
                (None, Some(stored))
            }
            _ => return Self::invariant("the binder slices text and places only"),
        };

        let length = match (&computed, &stored) {
            (Some(bytes), _) => bytes.len() as u64,
            (_, Some(stored)) => stored.length,
            _ => return Self::invariant("text is computed or stored"),
        };

        let (start, end) = (
            start.unwrap_or(0),
            end.unwrap_or_else(|| i128::from(length)),
        );
        if start < 0 || start > end || end > i128::from(length) {
            return Err(Self::error(
                span,
                ErrorKind::Bounds,
                format!("{start}..{end} is outside the text's {length} bytes"),
            ));
        }

        let (first, last) = (
            usize::try_from(start).unwrap_or(usize::MAX),
            usize::try_from(end).unwrap_or(usize::MAX),
        );
        Ok(Value::Text(match (computed, stored) {
            (Some(bytes), _) => bytes[first..last].to_vec(),
            (_, Some(stored)) => {
                let address = stored
                    .address
                    .checked_add(u64::try_from(start).unwrap_or(u64::MAX))
                    .ok_or_else(|| {
                        Self::error(span, ErrorKind::Arithmetic, "the text is outside memory")
                    })?;
                Self::at(span, self.machine.read(address, last - first))?
            }
            _ => return Self::invariant("text is computed or stored"),
        }))
    }

    /// A map's key: a tuple of keys, or one key's value. A place holds
    /// text, or is a tuple of the program's, or, of a dynamic type, is the
    /// object it holds, which its runtime compares.
    fn key(&mut self, key: &KeyNode<M::Object, M::Step>) -> Result<Key, Halt> {
        let node = match key {
            KeyNode::Value(node) => node,
            KeyNode::Tuple(items) => {
                let mut keys = Vec::with_capacity(items.len());
                for item in items {
                    keys.push(self.key(item)?);
                }

                return Ok(Key::Tuple(keys));
            }
        };

        Ok(match self.eval(node)? {
            Value::Int(integer) => Key::Integer(integer.value()),
            Value::Float(value) => Key::Float(value),
            Value::Bool(value) => Key::Bool(value),
            Value::Pointer(address) => Key::Address(address),
            Value::Text(bytes) => Key::Text(bytes),
            Value::Loaded(loaded) => Self::at(node.span, dynamic::key(&loaded))?,
            Value::Place(place) if matches!(node.ty, Ty::Dynamic) => {
                Key::Object(Self::at(node.span, self.machine.dynamic_key(&place))?)
            }
            Value::Place(place) => self.held_key(&place, node.span, 0)?,
            Value::Raw(_) => return Self::invariant("the binder loads raw keys"),
        })
    }

    /// The key a place of the program's holds: its text, or a tuple of
    /// the keys its elements hold. An element may also hold a number,
    /// truth value, or pointer; the binder loads those where it can see
    /// them, so a key itself holds text or is a tuple.
    fn held_key(&mut self, place: &M::Place, span: Span, depth: usize) -> Result<Key, Halt> {
        if let Some(text) = Self::at(span, self.machine.text(place))? {
            if text.completion != TextCompletion::Complete {
                return Err(Halt::Missing {
                    state: Box::new(unavailable(incomplete(text.completion))),
                    cause: span,
                });
            }

            return Ok(Key::Text(text.bytes.to_vec()));
        }

        if let Some(elements) = Self::at(span, self.machine.tuple_elements(place))? {
            if elements.len() > MAX_TUPLE_KEYS || depth + 1 >= MAX_TUPLE_DEPTH {
                return Err(Self::error(
                    span,
                    ErrorKind::Limit,
                    format!(
                        "a tuple key may hold at most {MAX_TUPLE_KEYS} keys, in tuples nested \
                         at most {MAX_TUPLE_DEPTH} deep"
                    ),
                ));
            }

            let mut keys = Vec::with_capacity(elements.len());
            for element in &elements {
                Self::at(span, self.machine.charge())?;
                keys.push(self.held_key(element, span, depth + 1)?);
            }

            return Ok(Key::Tuple(keys));
        }

        let value = if depth == 0 {
            None
        } else {
            Some(Self::at(span, self.machine.load(place))?)
        };

        Ok(match value {
            Some(VariableValue::Scalar(ScalarValue::Signed(value))) => {
                Key::Integer(Exact::from(value))
            }
            Some(VariableValue::Scalar(ScalarValue::Unsigned(value))) => {
                Key::Integer(Exact::from(value))
            }
            Some(VariableValue::Enumeration { value, .. }) => Key::Integer(Exact::from(value)),
            Some(VariableValue::Scalar(ScalarValue::Floating(value))) => {
                Key::Float(Float::from_value(value))
            }
            Some(VariableValue::Scalar(ScalarValue::Boolean(value))) => Key::Bool(value),
            Some(VariableValue::Address(address)) => Key::Address(address.address.get()),
            _ => {
                return Err(Self::error(
                    span,
                    ErrorKind::Type,
                    if depth == 0 {
                        "the key holds no text and is no tuple, number, truth value, or pointer"
                    } else {
                        "an element of the key holds no text and is no tuple, number, truth \
                         value, or pointer"
                    },
                ));
            }
        })
    }

    /// Presents a result as inspection presents values.
    fn present(
        &mut self,
        node: &Node<M::Object, M::Step>,
        value: Value<M::Place>,
    ) -> Result<InspectedValue, Halt> {
        let span = node.span;
        let ty = &node.ty;
        let value = match value {
            Value::Place(place) => return Self::at(span, self.machine.present(&place)),
            Value::Raw(address) => {
                let loaded = self.read_raw(ty, address, span)?;
                return self.present_computed(ty, &loaded, Some(address), span);
            }
            Value::Text(bytes) => return Ok(self.present_text(ty, bytes)),
            value => value,
        };

        // A number computed from values of a dynamic type has the type it
        // has as it runs.
        let ty = match (ty, &value) {
            (Ty::Dynamic, Value::Int(_)) => &Ty::Exact,
            (Ty::Dynamic, Value::Float(value)) => &Ty::Float(value.format()),
            (Ty::Dynamic, Value::Bool(_)) => &Ty::Bool,
            (ty, _) => ty,
        };

        self.present_computed(ty, &value, None, span)
    }

    fn present_computed(
        &mut self,
        ty: &Ty,
        value: &Value<M::Place>,
        address: Option<u64>,
        span: Span,
    ) -> Result<InspectedValue, Halt> {
        let size = size_of(self.machine, ty).and_then(|size| usize::try_from(size).ok());
        if let (Ty::Program(reference), Some(size)) = (ty, size) {
            let bytes = self.encode(value, size);
            return Self::at(span, self.machine.present_bytes(*reference, &bytes));
        }

        if let (Ty::Pointer(pointee), Value::Pointer(address)) = (ty, value) {
            let pointee = match pointee.as_ref() {
                Ty::Program(reference) => Some(*reference),
                _ => None,
            };

            let info = type_info(self.machine, ty);
            return Self::at(span, self.machine.present_pointer(*address, pointee, info));
        }

        let scalar = match value {
            Value::Int(integer) => {
                let integer = *integer;
                let value = integer.value();
                let signed = matches!(integer, Integer::Typed(bits) if bits.ty().is_signed());
                match (signed, value.to_u128(), value.to_i128()) {
                    (false, Some(unsigned), _) => {
                        VariableValue::Scalar(ScalarValue::Unsigned(unsigned))
                    }
                    (_, _, Some(signed)) => VariableValue::Scalar(ScalarValue::Signed(signed)),
                    _ => return Self::invariant("exact integers fit i128 or u128"),
                }
            }
            Value::Float(value) => VariableValue::Scalar(ScalarValue::Floating(value.to_value())),
            Value::Bool(value) => VariableValue::Scalar(ScalarValue::Boolean(*value)),
            Value::Pointer(address) => VariableValue::Address(crate::AddressValue {
                function: None,
                address: VirtualAddress::new(*address),
            }),
            Value::Place(_) | Value::Raw(_) | Value::Text(_) | Value::Loaded(_) => {
                return Self::invariant("places and strings are presented elsewhere");
            }
        };

        let raw = size.map(|size| std::sync::Arc::from(self.encode_scalar(&scalar, size)));
        let state = VariableState::Available {
            source: address.map_or(VariableValueSource::Computed, |address| {
                VariableValueSource::Memory(VirtualAddress::new(address))
            }),
            raw,
            value: scalar,
            dereference: DereferenceState::NotApplicable,
            children: ValueChildren::NotApplicable,
            text: None,
            presentation: None,
        };

        Ok(self
            .machine
            .finish(Some(type_info(self.machine, ty)), state))
    }

    /// A string the expression computed, as text.
    fn present_text(&self, ty: &Ty, bytes: Vec<u8>) -> InspectedValue {
        let text = crate::TextSummary {
            bytes: bytes.clone().into(),
            completion: TextCompletion::Complete,
        };

        let state = VariableState::Available {
            source: VariableValueSource::Computed,
            value: VariableValue::Slice {
                length: bytes.len() as u64,
                capacity: None,
            },
            raw: Some(bytes.into()),
            dereference: DereferenceState::NotApplicable,
            children: ValueChildren::NotApplicable,
            text: Some(std::sync::Arc::new(text)),
            presentation: None,
        };

        self.machine
            .finish(Some(type_info(self.machine, ty)), state)
    }

    /// A value's bytes in target order, `size` long.
    fn encode(&self, value: &Value<M::Place>, size: usize) -> Vec<u8> {
        let raw = match value {
            Value::Int(integer) => match integer {
                Integer::Typed(bits) => bits.raw(),
                Integer::Exact(value) => {
                    let width = u8::try_from(size.saturating_mul(8).min(128)).unwrap_or(128);
                    IntType::new(width.max(1), false)
                        .map_or(0, |int| Bits::truncate(int, *value).raw())
                }
            },
            Value::Bool(value) => u128::from(*value),
            Value::Pointer(address) => u128::from(*address),
            Value::Float(value) => float_bits(value.to_value()),
            Value::Place(_) | Value::Raw(_) | Value::Text(_) | Value::Loaded(_) => 0,
        };

        self.ordered(raw, size)
    }

    fn encode_scalar(&self, scalar: &VariableValue, size: usize) -> Vec<u8> {
        let raw = match scalar {
            VariableValue::Scalar(ScalarValue::Signed(value)) => value.cast_unsigned(),
            VariableValue::Scalar(ScalarValue::Unsigned(value)) => *value,
            VariableValue::Scalar(ScalarValue::Boolean(value)) => u128::from(*value),
            VariableValue::Address(address) => u128::from(address.address.get()),
            VariableValue::Scalar(ScalarValue::Floating(value)) => float_bits(*value),
            _ => 0,
        };

        self.ordered(raw, size)
    }

    fn ordered(&self, raw: u128, size: usize) -> Vec<u8> {
        let mut bytes = raw.to_le_bytes().to_vec();
        bytes.resize(size, 0);
        if self.machine.byte_order() == ByteOrder::Big {
            bytes.reverse();
        }

        bytes
    }
}

/// A float's bit pattern, zero-extended.
fn float_bits(value: crate::FloatValue) -> u128 {
    match value {
        crate::FloatValue::Binary16(bits) | crate::FloatValue::BFloat16(bits) => u128::from(bits),
        crate::FloatValue::Binary128(bits) => bits,
        crate::FloatValue::Binary32(bits) => u128::from(bits),
        crate::FloatValue::Binary64(bits) => u128::from(bits),
        crate::FloatValue::X87Extended {
            significand,
            sign_exponent,
        } => u128::from(sign_exponent) << 64 | u128::from(significand),
    }
}

/// Why text read in part has no value an operation can use.
pub(super) const fn incomplete(completion: TextCompletion) -> VariableUnavailableReason {
    match completion {
        TextCompletion::Unreadable { address } => VariableUnavailableReason::MemoryInaccessible {
            address,
            requested: 1,
            completed: 0,
            next_address: address,
        },
        TextCompletion::Limited { exhaustion, .. } => {
            VariableUnavailableReason::InspectionLimit(exhaustion)
        }
        _ => VariableUnavailableReason::EvaluationLimit,
    }
}
