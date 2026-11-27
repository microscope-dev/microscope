//! Parameterized by `O` (object handle) and `S` (navigation step) so debugger
//! expressions and `.views` expressions share a single IR and interpreter.

use std::sync::Arc;

use super::number::{BitOperator, Exact, Float, FloatFormat, FloatOperator, IntType, Integer};
use super::syntax::Span;
use super::syntax::ast::{BinaryOp, UnaryOp};
use super::target::Register;
use super::types::Ty;
use crate::TypeReference;

#[derive(Debug, Clone)]
pub struct Program<O, S> {
    pub(super) root: Node<O, S>,
}

impl<O, S> Program<O, S> {
    pub const fn result(&self) -> &Ty {
        &self.root.ty
    }

    pub const fn is_place(&self) -> bool {
        self.root.is_place()
    }

    pub const fn named_object(&self) -> Option<&O> {
        match &self.root.op {
            Op::Object(object) => Some(object),
            _ => None,
        }
    }

    pub fn assigned_object(&self) -> Option<&O> {
        match &self.root.op {
            Op::Assign { target, .. } => match &target.op {
                Op::Object(object) => Some(object),
                _ => None,
            },
            _ => None,
        }
    }

    pub fn root_object(&self) -> Option<&O> {
        let mut node = &self.root;
        loop {
            match &node.op {
                Op::Object(object) => return Some(object),
                Op::Step {
                    base,
                    follows: false,
                    ..
                } => node = base,
                _ => return None,
            }
        }
    }
}

impl<O: Clone, S: Clone> Program<O, S> {
    /// Recognizes `*(base + index)` where `index` satisfies the predicate.
    ///
    /// Views use this to detect contiguous stride indexing and avoid re-running
    /// the full expression for every element in a range.
    pub fn indexed_pointer(
        &self,
        index: impl Fn(&O) -> bool,
    ) -> Option<(Self, u64, TypeReference)> {
        let Op::At { address, pointee } = &self.root.op else {
            return None;
        };

        let Op::Offset {
            pointer,
            count,
            scale,
            backward: false,
        } = &address.op
        else {
            return None;
        };

        let indexes = match &count.op {
            Op::Bound(object) | Op::Object(object) => index(object),
            Op::Load(place) => matches!(&place.op, Op::Object(object) if index(object)),
            _ => false,
        };

        indexes.then(|| {
            (
                Self {
                    root: (**pointer).clone(),
                },
                *scale,
                *pointee,
            )
        })
    }
}

impl<O, S> Program<O, S> {
    pub fn mentions(&self, test: &impl Fn(&O) -> bool) -> bool {
        self.root.mentions(test)
    }
}

impl<O, S> Node<O, S> {
    fn mentions(&self, test: &impl Fn(&O) -> bool) -> bool {
        let any = |nodes: &[&Self]| nodes.iter().any(|node| node.mentions(test));
        match &self.op {
            Op::Object(object) | Op::Bound(object) => test(object),
            Op::Task | Op::Register(_) | Op::Constant(_) => false,
            Op::Entry { base, key, .. } => base.mentions(test) || key.mentions(test),
            Op::Step { base, indices, .. } => {
                base.mentions(test) || indices.iter().any(|index| index.mentions(test))
            }
            Op::Holds { base, .. } => base.mentions(test),
            Op::At { address, .. } | Op::Raw { address } | Op::Interpreted { address, .. } => {
                address.mentions(test)
            }
            Op::Load(operand)
            | Op::AddressOf(operand)
            | Op::Decay(operand)
            | Op::Negate(operand)
            | Op::FloatNegate(operand)
            | Op::Not(operand)
            | Op::BitNot(operand)
            | Op::Fit(operand)
            | Op::Convert { operand, .. }
            | Op::Length { operand, .. }
            | Op::Capacity { operand, .. }
            | Op::DynamicUnary { operand, .. } => operand.mentions(test),
            Op::Arithmetic { left, right, .. }
            | Op::DynamicArithmetic { left, right, .. }
            | Op::FloatArithmetic { left, right, .. }
            | Op::Bitwise { left, right, .. }
            | Op::Compare { left, right, .. }
            | Op::Logical { left, right, .. }
            | Op::Difference { left, right, .. } => any(&[left, right]),
            Op::Shift { value, amount, .. } => any(&[value, amount]),
            Op::Choose {
                condition,
                then,
                otherwise,
                ..
            } => any(&[condition, then, otherwise]),
            Op::Offset { pointer, count, .. } => any(&[pointer, count]),
            Op::Assign { target, value } => any(&[target, value]),
            Op::Range { base, start, end } | Op::TextSlice { base, start, end } => {
                base.mentions(test) || start.iter().chain(end).any(|bound| bound.mentions(test))
            }
        }
    }
}

#[derive(Debug, Clone)]
pub enum KeyNode<O, S> {
    Value(Box<Node<O, S>>),
    Tuple(Vec<Self>),
}

impl<O, S> KeyNode<O, S> {
    fn mentions(&self, test: &impl Fn(&O) -> bool) -> bool {
        match self {
            Self::Value(node) => node.mentions(test),
            Self::Tuple(items) => items.iter().any(|item| item.mentions(test)),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Node<O, S> {
    pub op: Op<O, S>,
    pub ty: Ty,
    pub span: Span,
}

impl<O, S> Node<O, S> {
    pub const fn is_place(&self) -> bool {
        matches!(
            self.op,
            Op::Object(_)
                | Op::Step { .. }
                | Op::Entry { .. }
                | Op::At { .. }
                | Op::Raw { .. }
                | Op::Interpreted { .. }
        ) || matches!(&self.op, Op::Choose { places: true, .. })
    }
}

#[derive(Debug, Clone)]
pub enum Constant {
    Integer(Integer),
    Float(Float),
    Bool(bool),

    Pointer(u64),
    Text(Vec<u8>),

    BigInt(Exact),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Comparison {
    Integers,
    Floats,

    FloatInteger,

    IntegerFloat,
    Addresses,
    Bools,

    Text,

    Texts,

    Dynamic,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Conversion {
    Truncate(IntType),

    IntegerToFloat(FloatFormat),

    FloatToInteger(IntType),
    FloatToFloat(FloatFormat),

    Truth,
    BoolToInteger(IntType),

    PointerToInteger(IntType),

    IntegerToPointer,

    PointerToPointer,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Length {
    Slice,

    Text,

    Bytes,

    Dynamic,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Capacity {
    Slice,

    Presented,
}

#[derive(Debug, Clone)]
pub enum Op<O, S> {
    /// Target place resolved via `Machine::locate`.
    Object(O),

    /// Pre-computed value (e.g. a view `let` or generator variable) resolved via `Machine::bound`.
    Bound(O),

    Entry {
        base: Box<Node<O, S>>,
        step: S,
        key: KeyNode<O, S>,
    },

    Task,

    Step {
        base: Box<Node<O, S>>,
        step: S,
        indices: Vec<Node<O, S>>,
        follows: bool,
    },

    Holds {
        base: Box<Node<O, S>>,
        step: S,
        negate: bool,
    },

    At {
        address: Box<Node<O, S>>,
        pointee: TypeReference,
    },

    Raw {
        address: Box<Node<O, S>>,
    },

    Interpreted {
        ty: Arc<str>,
        address: Box<Node<O, S>>,
    },

    Load(Box<Node<O, S>>),
    Register(Register),
    Constant(Constant),

    AddressOf(Box<Node<O, S>>),

    Decay(Box<Node<O, S>>),

    Negate(Box<Node<O, S>>),
    FloatNegate(Box<Node<O, S>>),
    Not(Box<Node<O, S>>),
    BitNot(Box<Node<O, S>>),

    Arithmetic {
        op: BinaryOp,
        left: Box<Node<O, S>>,
        right: Box<Node<O, S>>,
    },

    DynamicArithmetic {
        op: BinaryOp,
        left: Box<Node<O, S>>,
        right: Box<Node<O, S>>,
    },

    DynamicUnary {
        op: UnaryOp,
        operand: Box<Node<O, S>>,
    },

    FloatArithmetic {
        op: FloatOperator,
        left: Box<Node<O, S>>,
        right: Box<Node<O, S>>,
    },
    Bitwise {
        op: BitOperator,
        left: Box<Node<O, S>>,
        right: Box<Node<O, S>>,
    },
    Shift {
        left: bool,
        value: Box<Node<O, S>>,
        amount: Box<Node<O, S>>,
    },
    Compare {
        op: BinaryOp,
        how: Comparison,
        left: Box<Node<O, S>>,
        right: Box<Node<O, S>>,
    },

    Logical {
        and: bool,
        left: Box<Node<O, S>>,
        right: Box<Node<O, S>>,
    },

    Choose {
        condition: Box<Node<O, S>>,
        then: Box<Node<O, S>>,
        otherwise: Box<Node<O, S>>,
        places: bool,
    },
    Convert {
        operand: Box<Node<O, S>>,
        conversion: Conversion,
    },

    Offset {
        pointer: Box<Node<O, S>>,
        count: Box<Node<O, S>>,
        scale: u64,
        backward: bool,
    },

    Difference {
        left: Box<Node<O, S>>,
        right: Box<Node<O, S>>,
        scale: u64,
    },
    Length {
        operand: Box<Node<O, S>>,
        how: Length,
    },
    Capacity {
        operand: Box<Node<O, S>>,
        how: Capacity,
    },

    Fit(Box<Node<O, S>>),

    Assign {
        target: Box<Node<O, S>>,
        value: Box<Node<O, S>>,
    },

    Range {
        base: Box<Node<O, S>>,
        start: Option<Box<Node<O, S>>>,
        end: Option<Box<Node<O, S>>>,
    },

    TextSlice {
        base: Box<Node<O, S>>,
        start: Option<Box<Node<O, S>>>,
        end: Option<Box<Node<O, S>>>,
    },
}
