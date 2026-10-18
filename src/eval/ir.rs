//! A bound expression: a tree whose every node has a type, whose every
//! conversion is explicit, and whose names are resolved.

use std::sync::Arc;

use super::number::{BitOperator, Float, FloatFormat, FloatOperator, IntType, Integer};
use super::syntax::Span;
use super::syntax::ast::{BinaryOp, UnaryOp};
use super::target::Register;
use super::types::Ty;
use crate::TypeReference;

/// A program bound in one scope, which runs at any stop in that scope.
#[derive(Debug, Clone)]
pub struct Program<O, S> {
    pub(super) root: Node<O, S>,
}

impl<O, S> Program<O, S> {
    /// The type of the program's result.
    pub const fn result(&self) -> &Ty {
        &self.root.ty
    }

    /// Whether the program's result is a place: storage that is read only
    /// when a value is needed.
    pub const fn is_place(&self) -> bool {
        self.root.is_place()
    }

    /// The data object the program names, when it names nothing more.
    pub const fn named_object(&self) -> Option<&O> {
        match &self.root.op {
            Op::Object(object) => Some(object),
            _ => None,
        }
    }

    /// The data object an assignment stores in, when it names it alone.
    pub fn assigned_object(&self) -> Option<&O> {
        match &self.root.op {
            Op::Assign { target, .. } => match &target.op {
                Op::Object(object) => Some(object),
                _ => None,
            },
            _ => None,
        }
    }

    /// The data object whose storage holds the result, when the result is
    /// part of one: reached from it through members and array elements
    /// only, never through a pointer.
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
    /// When the program is `p[i]`, a pointer `p` moved by a value `index`
    /// accepts and nothing else, as a view's elements often are: `p`, the
    /// size of each element, and the type it points to.
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
    /// Whether any part of the program names an object `test` accepts.
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
            Op::Entry { base, key, .. } => any(&[base, key]),
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
pub struct Node<O, S> {
    pub op: Op<O, S>,
    pub ty: Ty,
    pub span: Span,
}

impl<O, S> Node<O, S> {
    /// Whether the node is a place: storage that is read only when a value
    /// is needed.
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

/// A constant the binder computed.
#[derive(Debug, Clone)]
pub enum Constant {
    Integer(Integer),
    Float(Float),
    Bool(bool),

    /// A pointer's address, `0` for `null`.
    Pointer(u64),
    Text(Vec<u8>),
}

/// How a comparison compares.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Comparison {
    Integers,
    Floats,

    /// A float on the left, an integer on the right.
    FloatInteger,

    /// An integer on the left, a float on the right.
    IntegerFloat,
    Addresses,
    Bools,

    /// A place's text on the left, a string on the right.
    Text,

    /// Two strings.
    Texts,

    /// Values one of which has a dynamic type, compared by the types they
    /// have when they run.
    Dynamic,
}

/// A conversion of a value to another type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Conversion {
    /// Truncates an integer to a type's width.
    Truncate(IntType),

    /// Rounds an integer to the nearest float.
    IntegerToFloat(FloatFormat),

    /// Truncates toward zero and saturates; NaN is refused.
    FloatToInteger(IntType),
    FloatToFloat(FloatFormat),

    /// Whether a value is nonzero.
    Truth,
    BoolToInteger(IntType),

    /// An address as an integer of a type's width.
    PointerToInteger(IntType),

    /// An integer as an address.
    IntegerToPointer,

    /// A pointer as another pointer: the address is kept.
    PointerToPointer,
}

/// How `len` measures.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Length {
    /// A slice's run-time length, or the first dimension's of an array
    /// bounded at run time.
    Slice,

    /// The length in bytes of text.
    Text,

    /// The length of a string the expression computed.
    Bytes,

    /// The length a value of a dynamic type has, as its language measures
    /// it.
    Dynamic,
}

/// How `cap` measures.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Capacity {
    /// The capacity a slice's descriptor records.
    Slice,

    /// The `capacity` field of the view that presents the value.
    Presented,
}

#[derive(Debug, Clone)]
pub enum Op<O, S> {
    /// A data object's storage.
    Object(O),

    /// A value the machine computes once and keeps, which is not a place.
    Bound(O),

    /// The value a map at `base` holds for `key`, through a planned step.
    Entry {
        base: Box<Node<O, S>>,
        step: S,
        key: Box<Node<O, S>>,
    },

    /// The id of the stopped thread's task.
    Task,

    /// A structural step from a place, with its index values. A step that
    /// `follows` a pointer, as a dereference or a slice's element does,
    /// leaves the storage of the place it starts from.
    Step {
        base: Box<Node<O, S>>,
        step: S,
        indices: Vec<Node<O, S>>,
        follows: bool,
    },

    /// Whether the tagged union at `base` holds the variant whose member
    /// `step` reaches, `negate`d for `!=`.
    Holds {
        base: Box<Node<O, S>>,
        step: S,
        negate: bool,
    },

    /// The place a computed pointer to a program type points at.
    At {
        address: Box<Node<O, S>>,
        pointee: TypeReference,
    },

    /// The place a computed pointer to a language type points at.
    Raw {
        address: Box<Node<O, S>>,
    },

    /// The interpreter's object a computed pointer points at, as a cast to
    /// the dynamic type `ty` takes it.
    Interpreted {
        ty: Arc<str>,
        address: Box<Node<O, S>>,
    },

    /// The value at a place.
    Load(Box<Node<O, S>>),
    Register(Register),
    Constant(Constant),

    /// The address of a place.
    AddressOf(Box<Node<O, S>>),

    /// The address of an array's first element.
    Decay(Box<Node<O, S>>),

    /// Exact negation of an integer.
    Negate(Box<Node<O, S>>),
    FloatNegate(Box<Node<O, S>>),
    Not(Box<Node<O, S>>),
    BitNot(Box<Node<O, S>>),

    /// Exact `+ - * / %`.
    Arithmetic {
        op: BinaryOp,
        left: Box<Node<O, S>>,
        right: Box<Node<O, S>>,
    },

    /// `+ - * / % & | ^ << >>` of values one of which has a dynamic type,
    /// by the rules of the types they have when they run.
    DynamicArithmetic {
        op: BinaryOp,
        left: Box<Node<O, S>>,
        right: Box<Node<O, S>>,
    },

    /// `-` or `~` of a value of a dynamic type.
    DynamicUnary {
        op: UnaryOp,
        operand: Box<Node<O, S>>,
    },

    /// Float arithmetic in the node's format.
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

    /// `&&` when `and`, otherwise `||`, of truth values.
    Logical {
        and: bool,
        left: Box<Node<O, S>>,
        right: Box<Node<O, S>>,
    },

    /// `?:`; `places` when both branches are places of one type.
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

    /// A pointer moved by a number of elements of `scale` bytes.
    Offset {
        pointer: Box<Node<O, S>>,
        count: Box<Node<O, S>>,
        scale: u64,
        backward: bool,
    },

    /// How many elements of `scale` bytes lie between two pointers.
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

    /// A value converted to the node's type, refused unless the type holds
    /// it exactly.
    Fit(Box<Node<O, S>>),

    /// Stores a fitted value in a place: only ever the whole expression.
    Assign {
        target: Box<Node<O, S>>,
        value: Box<Node<O, S>>,
    },

    /// `base[start..end]`, or `base[start:end]` of an array or slice: a
    /// bound left out is the first element, or the end.
    Range {
        base: Box<Node<O, S>>,
        start: Option<Box<Node<O, S>>>,
        end: Option<Box<Node<O, S>>>,
    },

    /// `base[start:end]` of text: the bytes between, as a string.
    TextSlice {
        base: Box<Node<O, S>>,
        start: Option<Box<Node<O, S>>>,
        end: Option<Box<Node<O, S>>>,
    },
}
