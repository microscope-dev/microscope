mod error;
mod format;
mod text;

pub(super) use text::bigint_to_string;

#[cfg(test)]
mod tests;

use crate::VirtualAddress;
pub use crate::image::v8::NodeRelease;

crate::model::id_type!(ObjectId, "An object's row in one [`ValueGraph`].");

#[derive(Debug, Default)]
pub struct ValueGraph {
    objects: Vec<Object>,
}

impl ValueGraph {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            objects: Vec::new(),
        }
    }

    pub fn push(&mut self, object: Object) -> Option<ObjectId> {
        let id = ObjectId::new(u32::try_from(self.objects.len()).ok()?);
        self.objects.push(object);
        debug_assert_eq!(
            id.index() + 1,
            self.objects.len(),
            "row {id} is not the last of the graph",
        );
        Some(id)
    }

    pub fn object_mut(&mut self, id: ObjectId) -> Option<&mut Object> {
        self.objects.get_mut(id.index())
    }

    #[must_use]
    pub fn row(&self, id: ObjectId) -> Option<&Object> {
        self.objects.get(id.index())
    }

    fn object(&self, id: ObjectId) -> Option<&Object> {
        self.objects.get(id.index())
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Undefined,
    Null,
    Boolean(bool),
    Number(f64),
    BigInt(BigInt),
    String(JsString),
    Symbol(Symbol),
    Object(ObjectId),

    Unreadable(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JsString {
    units: Vec<u16>,
    length: u32,
}

impl JsString {
    #[must_use]
    pub fn new(units: Vec<u16>, length: u32) -> Self {
        let read = u32::try_from(units.len()).unwrap_or(u32::MAX);
        let string = Self {
            units,
            length: length.max(read),
        };
        debug_assert!(
            string.units.len() <= string.length as usize,
            "a string of length {} holds {} units",
            string.length,
            string.units.len(),
        );
        string
    }

    #[must_use]
    pub fn complete(units: Vec<u16>) -> Self {
        Self::new(units, 0)
    }

    #[must_use]
    pub fn units(&self) -> &[u16] {
        &self.units
    }

    #[must_use]
    pub const fn length(&self) -> u32 {
        self.length
    }

    fn is_complete(&self) -> bool {
        u32::try_from(self.units.len()).is_ok_and(|read| read == self.length)
    }

    fn whole(&self) -> Option<String> {
        self.is_complete()
            .then(|| String::from_utf16_lossy(&self.units))
    }
}

impl From<&str> for JsString {
    fn from(text: &str) -> Self {
        Self::complete(text.encode_utf16().collect())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BigInt {
    negative: bool,
    magnitude: Vec<u64>,
}

impl BigInt {
    #[must_use]
    pub const fn new(negative: bool, magnitude: Vec<u64>) -> Self {
        Self {
            negative,
            magnitude,
        }
    }

    #[must_use]
    pub fn from_i128(value: i128) -> Self {
        let magnitude = value.unsigned_abs();
        #[allow(
            clippy::cast_possible_truncation,
            reason = "each half is taken by a mask or a shift"
        )]
        let digits = vec![magnitude as u64, (magnitude >> 64) as u64];
        Self::new(value < 0, digits)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Symbol {
    pub description: Option<JsString>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Object {
    pub address: VirtualAddress,
    pub constructor: Constructor,

    pub tag: Option<JsString>,
    pub kind: Kind,

    pub properties: Vec<Property>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Constructor {
    Named(JsString),

    NullPrototype {
        internal: JsString,
    },

    Unnamed {
        names: Vec<JsString>,
        end: JsString,
        end_has_keys: bool,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub struct Property {
    pub key: Key,
    pub slot: Slot,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Key {
    String(JsString),
    Symbol(Symbol),
}

#[derive(Debug, Clone, PartialEq)]
pub enum Slot {
    Data(Value),
    Accessor { getter: bool, setter: bool },
}

#[derive(Debug, Clone, PartialEq)]
pub enum Kind {
    Ordinary,
    Array(Array),
    Function(Function),

    Error(ErrorDetails),
    Map {
        size: u32,

        entries: Vec<(Value, Value)>,
    },
    Set {
        size: u32,

        values: Vec<Value>,
    },
    WeakMap,
    WeakSet,
    Date {
        time: f64,
    },
    RegExp {
        source: JsString,

        flags: JsString,
    },
    Promise(PromiseState),
    TypedArray {
        element: ElementType,
        length: u64,

        bytes: Vec<u8>,
    },
    ArrayBuffer {
        shared: bool,
        byte_length: u64,

        bytes: Option<Vec<u8>>,
    },
    DataView {
        view: Option<ViewRange>,

        buffer: Value,
    },

    Boxed(Value),

    Proxy {
        target: Value,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub struct Array {
    pub length: u32,

    pub elements: Vec<Element>,

    pub listed_below: u32,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Element {
    pub index: u32,
    pub slot: Slot,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Function {
    pub kind: FunctionKind,

    pub name: Value,
}

#[derive(Debug, Clone, PartialEq)]
pub enum FunctionKind {
    Plain,
    Async,
    Generator,
    AsyncGenerator,
    Class {
        /// for a base class, whose prototype is `Function.prototype`.
        extends: Value,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub struct ErrorDetails {
    pub stack: Value,

    pub name: Value,

    pub message: Value,

    pub hidden: Vec<Property>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum PromiseState {
    Pending,
    Fulfilled(Value),
    Rejected(Value),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ElementType {
    Int8,
    Uint8,
    Uint8Clamped,
    Int16,
    Uint16,
    Int32,
    Uint32,
    Float16,
    Float32,
    Float64,
    BigInt64,
    BigUint64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ViewRange {
    pub byte_length: u64,
    pub byte_offset: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Inspected {
    pub text: String,

    pub approximate: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Layout {
    Default,

    Line,
}

#[must_use]
pub fn inspect(graph: &ValueGraph, value: &Value, node: NodeRelease, layout: Layout) -> Inspected {
    format::inspect(graph, value, node, layout)
}
