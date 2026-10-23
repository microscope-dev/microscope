//! A JavaScript value as Node's `util.inspect(value, { customInspect: false })`
//! writes it, with every other option at its default: depth 2, colors off,
//! `showHidden` false, `breakLength` 80, `compact` 3, `maxArrayLength` 100,
//! `maxStringLength` 10000, unsorted, no getters, no numeric separators, and
//! `showProxy` false.
//!
//! The reader of a stopped process builds a [`ValueGraph`] from V8's heap;
//! [`inspect`] is a port of Node's `lib/internal/util/inspect.js` over it.
//! The port follows Node 26's file and takes the Node release for the few
//! places where 22, 24, or an earlier release of a line write something
//! else.
//!
//! # What the reader provides
//!
//! Every object is a row in the graph, and a value refers to it by
//! [`ObjectId`]. Identity is the object's address: two rows with one address
//! are one object, so a reference back to an ancestor (a cycle) is a value
//! naming that ancestor's row, or any row with its address.
//!
//! inspect counts levels from the root, which is level 0. Each step from an
//! object to a value it holds (a property, an element, a Map key or value, a
//! Set member, a promise's result, a `DataView`'s buffer, an error's cause) adds
//! one. An object at level 2 or less is printed from its contents. An object
//! at level 3 is printed from its header alone: its constructor, tag, kind
//! with the kind's sizes, function or error details, and its property keys.
//! There the reader may leave out elements, entries, and bytes, and may give
//! property values as [`Value::Unreadable`]. Nothing at level 4 is printed,
//! except a reference to an ancestor.
//!
//! Strings carry their UTF-16 units. A string value needs its first 10000
//! units; a string used whole needs all of them: property keys, symbol
//! descriptions, constructor names and tags, a `RegExp`'s source and flags, and
//! an error's stack, name, and message. A part the reader could not read, or
//! a string short of the units the formatter needs, prints as
//! `<unreadable: REASON>`. A state this port does not reproduce prints as
//! `<unsupported: REASON>`. Neither text is ever Node's.

mod error;
mod format;
mod text;

#[cfg(test)]
mod tests;

use crate::VirtualAddress;
pub use crate::image::v8::NodeRelease;

crate::model::id_type!(ObjectId, "An object's row in one [`ValueGraph`].");

/// The objects a value reaches, as the reader read them from the heap.
#[derive(Debug, Default)]
pub struct ValueGraph {
    objects: Vec<Object>,
}

impl ValueGraph {
    /// An empty graph.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            objects: Vec::new(),
        }
    }

    /// Adds an object and returns its row, or `None` once the rows run out.
    pub fn push(&mut self, object: Object) -> Option<ObjectId> {
        let id = ObjectId::new(u32::try_from(self.objects.len()).ok()?);
        self.objects.push(object);
        Some(id)
    }

    /// The object in a row, to fill in after its row was handed out, as when
    /// its contents refer back to it.
    pub fn object_mut(&mut self, id: ObjectId) -> Option<&mut Object> {
        self.objects.get_mut(id.index())
    }

    /// The object in a row.
    #[must_use]
    pub fn row(&self, id: ObjectId) -> Option<&Object> {
        self.objects.get(id.index())
    }

    fn object(&self, id: ObjectId) -> Option<&Object> {
        self.objects.get(id.index())
    }
}

/// A JavaScript value.
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
    /// A value the reader could not read, and why.
    Unreadable(String),
}

/// A JavaScript string: its first UTF-16 units and its full length.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JsString {
    units: Vec<u16>,
    length: u32,
}

impl JsString {
    /// A string of `length` units, of which `units` are the first. A
    /// `length` shorter than `units` is taken as `units`' length.
    #[must_use]
    pub fn new(units: Vec<u16>, length: u32) -> Self {
        let read = u32::try_from(units.len()).unwrap_or(u32::MAX);
        Self {
            units,
            length: length.max(read),
        }
    }

    /// A string whose units were all read.
    #[must_use]
    pub fn complete(units: Vec<u16>) -> Self {
        Self::new(units, 0)
    }

    /// The units read, the first of the string's.
    #[must_use]
    pub fn units(&self) -> &[u16] {
        &self.units
    }

    /// The string's length in UTF-16 units.
    #[must_use]
    pub const fn length(&self) -> u32 {
        self.length
    }

    /// Whether every unit was read.
    fn is_complete(&self) -> bool {
        u32::try_from(self.units.len()).is_ok_and(|read| read == self.length)
    }

    /// The whole string, if every unit was read.
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

/// A `BigInt` as a sign and a magnitude of 64-bit digits, least significant
/// first, as V8 stores it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BigInt {
    negative: bool,
    magnitude: Vec<u64>,
}

impl BigInt {
    /// A `BigInt` of the sign and the magnitude's digits, least significant
    /// first. Zero has no sign.
    #[must_use]
    pub const fn new(negative: bool, magnitude: Vec<u64>) -> Self {
        Self {
            negative,
            magnitude,
        }
    }

    /// The `BigInt` whose value is `value`.
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

/// A symbol and its description.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Symbol {
    /// `None` for a symbol made without one, as `Symbol()`.
    pub description: Option<JsString>,
}

/// An object as inspect sees it.
#[derive(Debug, Clone, PartialEq)]
pub struct Object {
    /// Where the object lives, which is its identity.
    pub address: VirtualAddress,
    pub constructor: Constructor,
    /// `value[Symbol.toStringTag]` when it is a string that is not an own
    /// enumerable property. `None` when there is none, or when it equals the
    /// constructor's name exactly; then inspect writes the same text.
    pub tag: Option<JsString>,
    pub kind: Kind,
    /// The own enumerable properties: string keys in V8's order, then symbol
    /// keys. For an array or a typed array, only those whose keys are not
    /// indices; for a String wrapper, only those past the string's indices.
    pub properties: Vec<Property>,
}

/// The constructor name inspect's `getConstructorName` finds: walking the
/// prototype chain from the object itself, the first object that is one of
/// V8's builtin prototypes (`Object.prototype`, `Array.prototype`, ...) gives
/// that builtin's name, and otherwise the first that has an own `constructor`
/// data property holding a function with a nonempty name gives that name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Constructor {
    Named(JsString),
    /// The object's own prototype is null. `internal` is what V8's
    /// `GetConstructorName` gives for it, `Object` for an ordinary object.
    NullPrototype {
        internal: JsString,
    },
    /// No object on a chain longer than one has a constructor. `names` holds
    /// V8's `GetConstructorName` for the object and each prototype up to the
    /// last before the one whose prototype is null; that one, the end, is
    /// taken as an ordinary object, with V8's name `end` and with or without
    /// own enumerable properties.
    Unnamed {
        names: Vec<JsString>,
        end: JsString,
        end_has_keys: bool,
    },
}

/// An own property.
#[derive(Debug, Clone, PartialEq)]
pub struct Property {
    pub key: Key,
    pub slot: Slot,
}

/// A property key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Key {
    String(JsString),
    Symbol(Symbol),
}

/// What a property holds. inspect never calls an accessor.
#[derive(Debug, Clone, PartialEq)]
pub enum Slot {
    Data(Value),
    Accessor { getter: bool, setter: bool },
}

/// The kind of an object, from V8's instance type, with what inspect reads of
/// that kind.
#[derive(Debug, Clone, PartialEq)]
pub enum Kind {
    /// A plain object, a class instance, or any object inspect has no other
    /// branch for.
    Ordinary,
    Array(Array),
    Function(Function),
    /// A native error, or any object whose prototype chain holds
    /// `Error.prototype`.
    Error(ErrorDetails),
    Map {
        size: u32,
        /// The first `min(size, 100)` entries in insertion order.
        entries: Vec<(Value, Value)>,
    },
    Set {
        size: u32,
        /// The first `min(size, 100)` members in insertion order.
        values: Vec<Value>,
    },
    WeakMap,
    WeakSet,
    Date {
        /// The time value in milliseconds since the epoch, NaN when invalid.
        time: f64,
    },
    RegExp {
        /// As the `source` getter gives it, with `/` and line terminators
        /// escaped.
        source: JsString,
        /// As the `flags` getter gives it, as `giu`.
        flags: JsString,
    },
    Promise(PromiseState),
    TypedArray {
        element: ElementType,
        length: u64,
        /// The first `min(length, 100)` elements' bytes, little-endian.
        bytes: Vec<u8>,
    },
    ArrayBuffer {
        shared: bool,
        byte_length: u64,
        /// The first `min(byte_length, 100)` bytes, or `None` when detached.
        bytes: Option<Vec<u8>>,
    },
    DataView {
        /// `None` when the view's getters throw, as for a detached buffer.
        view: Option<ViewRange>,
        /// The `ArrayBuffer` or `SharedArrayBuffer` it views.
        buffer: Value,
    },
    /// A `Number`, `String`, `Boolean`, `BigInt`, or `Symbol` wrapper and the primitive
    /// it holds.
    Boxed(Value),
    /// A proxy, whose own fields other than the address inspect never reads.
    Proxy {
        /// Its target object, or null once revoked.
        target: Value,
    },
}

/// An array's length and elements.
#[derive(Debug, Clone, PartialEq)]
pub struct Array {
    pub length: u32,
    /// Present elements in ascending index order: every one whose index is
    /// below `listed_below`, and none at or above it.
    pub elements: Vec<Element>,
    /// To print exactly it is the length, or covers both the first 101 plus
    /// `properties.len()` indices and the first 100 present elements.
    pub listed_below: u32,
}

/// An array element that is present.
#[derive(Debug, Clone, PartialEq)]
pub struct Element {
    pub index: u32,
    pub slot: Slot,
}

/// A function.
#[derive(Debug, Clone, PartialEq)]
pub struct Function {
    pub kind: FunctionKind,
    /// `value.name` through the prototype chain, `''` when anonymous; for a
    /// class, its own `name` property, or undefined when it has none.
    pub name: Value,
}

/// What kind of function, as V8's `SharedFunctionInfo` says. Arrows,
/// methods, bound functions, and builtins are `Plain`.
#[derive(Debug, Clone, PartialEq)]
pub enum FunctionKind {
    Plain,
    Async,
    Generator,
    AsyncGenerator,
    Class {
        /// The `name` of the class's prototype, the class it extends: `''`
        /// for a base class, whose prototype is `Function.prototype`.
        extends: Value,
    },
}

/// What inspect reads of an error.
#[derive(Debug, Clone, PartialEq)]
pub struct ErrorDetails {
    /// `err.stack`, the formatted stack text when it is a string.
    pub stack: Value,
    /// `err.name` through the prototype chain.
    pub name: Value,
    /// `err.message` through the prototype chain.
    pub message: Value,
    /// The own non-enumerable properties named `cause`, `errors`, `error`,
    /// and `suppressed`.
    pub hidden: Vec<Property>,
}

/// A promise's state.
#[derive(Debug, Clone, PartialEq)]
pub enum PromiseState {
    Pending,
    Fulfilled(Value),
    Rejected(Value),
}

/// A typed array's element type.
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

/// A `DataView`'s `byteLength` and `byteOffset`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ViewRange {
    pub byte_length: u64,
    pub byte_offset: u64,
}

/// A value's text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Inspected {
    pub text: String,
    /// Some line layout rested on what the graph does not say exactly: the
    /// display width of a character from U+0300 on, which Node takes from
    /// ICU, or the type of an array element the reader did not list. The
    /// spacing may then differ from Node's.
    pub approximate: bool,
}

/// The options besides `customInspect: false` that inspect runs with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Layout {
    /// The defaults, as `console.log` writes a value.
    Default,
    /// `compact: true` and `breakLength: Infinity`: every entry on one line.
    /// Only an error's stack still breaks the text.
    Line,
}

/// `util.inspect(value, { customInspect: false })`, with `layout`'s
/// options, as Node release `node` writes it.
#[must_use]
pub fn inspect(graph: &ValueGraph, value: &Value, node: NodeRelease, layout: Layout) -> Inspected {
    format::inspect(graph, value, node, layout)
}
