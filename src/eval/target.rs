//! The only ways the evaluator reaches a program: a [`Scope`] names things
//! while binding, and a [`Machine`] reads one stop while running.

use std::fmt;
use std::sync::Arc;

use super::error::ErrorKind;
use super::interp::Value;
use super::number::{Exact, Float};
use super::syntax::ast::Tag;
use super::types::TypeSource;
use crate::{
    InspectedValue, ScalarValue, TextCompletion, TextSummary, TypeInfo, TypeReference,
    VariableState, VariableUnavailableReason, VariableValue,
};

/// Why the debugger refuses part of an expression, without where; the
/// evaluator adds the span.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refusal {
    pub kind: ErrorKind,
    pub message: String,
}

impl Refusal {
    pub fn new(kind: ErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }
}

/// What a name means in a scope.
#[derive(Debug, Clone)]
pub enum Lookup<O> {
    /// A local, parameter, or global, with its type unless that is
    /// malformed.
    Object {
        object: O,
        ty: Result<TypeReference, std::sync::Arc<str>>,
    },

    /// An enumerator's value, typed by its enumeration.
    Enumerator {
        value: Exact,
        ty: TypeReference,
    },

    /// An exact integer the scope knows, such as an argument a view's
    /// pattern captured.
    Constant(Exact),

    /// A value the scope's machine computes once and keeps, of any type,
    /// such as a view's `let`: never a place.
    Bound {
        object: O,
        ty: super::types::Ty,
    },

    /// A variable whose type only its value knows, as a Python variable's
    /// is: always a place, of type [`Ty::Dynamic`](super::types::Ty).
    Dynamic(O),

    /// Several things the name could mean, each written as it is selected.
    Ambiguous(Vec<String>),
    NotFound,
}

/// A program type to look up, by name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TypeQuery {
    /// The name as written, or a C base type's canonical spelling.
    pub name: String,

    /// The tag written before the name, as in `struct S`.
    pub tag: Option<Tag>,
}

/// What a type name means in a scope.
#[derive(Debug, Clone)]
pub enum TypeLookup {
    Found(TypeReference),
    Ambiguous(Vec<String>),
    NotFound,
}

/// One structural step a scope can plan.
#[derive(Debug, Clone, Copy)]
pub enum StepKind<'a> {
    /// Through a pointer or reference stored in program memory.
    Deref,

    /// To a member of a record, union, or variant.
    Member(&'a str),

    /// To an element of an array or slice, holding `available` indices.
    Index { available: usize },

    /// To the one base class subobject of a record of the given type.
    Base(TypeReference),

    /// To the value a map holds for a key, as the view that presents the
    /// map gives its entries.
    Entry,
}

/// A step a scope planned from types alone.
#[derive(Debug, Clone)]
pub struct Planned<S> {
    pub step: S,

    /// The type the step reaches, or `None` when it reaches nothing usable.
    pub result: Option<TypeReference>,

    /// How many index values the step takes.
    pub consumed: usize,
}

/// A register a scope names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Register {
    /// The scope's own number for it.
    pub number: u16,

    /// Its width in bits.
    pub width: u8,
}

/// What a system call's stop knows of the call, which `$arg0` to `$arg5`,
/// `$ret`, and `$path` read.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CallValues {
    /// The arguments, as the call's registers held them.
    pub args: [u64; 6],

    /// What the call returned, once it has.
    pub returned: Option<i64>,

    /// The path the call names, when it names one and it could be read.
    pub path: Option<Vec<u8>>,
}

/// Bind-time view of one frame: what names mean there, and what types are.
pub trait Scope: TypeSource {
    /// A data object the scope can name.
    type Object: Clone + fmt::Debug;

    /// A planned structural step.
    type Step: Clone + fmt::Debug;

    /// What `name` means. A name of the outermost scope skips the frame's
    /// own locals.
    fn lookup(&self, name: &str, outermost: bool) -> Result<Lookup<Self::Object>, Refusal>;

    /// The program type a name means.
    fn lookup_type(&self, query: &TypeQuery) -> TypeLookup;

    /// Plans a step from a value of program type `from`.
    fn plan(&self, from: TypeReference, step: StepKind<'_>)
    -> Result<Planned<Self::Step>, Refusal>;

    /// The register a name, without its `$`, means.
    fn register(&self, name: &str) -> Option<Register>;

    /// Plans a step from a value of a dynamic type, which reaches another:
    /// [`StepKind::Member`] to an attribute, or [`StepKind::Entry`] to an
    /// item by its key or index. The machine takes it at run time, through
    /// [`Machine::step`] or [`Machine::entry`], by the value's type then.
    fn plan_dynamic(&self, step: StepKind<'_>) -> Result<Self::Step, Refusal> {
        let _ = step;
        Err(Refusal::new(
            ErrorKind::Unsupported,
            "no value here has a dynamic type",
        ))
    }

    /// Whether `name` is the dynamic type of an interpreter's objects, to
    /// which a cast takes an address to reach into the object there, as
    /// `py` is CPython's.
    fn dynamic_type(&self, name: &str) -> bool {
        let _ = name;
        false
    }

    /// The call that stopped the thread, where a system call did.
    fn call_values(&self) -> Option<&CallValues> {
        None
    }

    /// Whether a view presents values of `ty` with a length, as one
    /// presents a Go map, which is a pointer.
    fn has_view(&self, ty: TypeReference) -> bool {
        let _ = ty;
        false
    }

    /// Whether `ty`, a pointer, only stands for a container its language
    /// gives a kind of its own, such as a Go map or channel: it is indexed,
    /// measured, and sized through the view that presents the container,
    /// never as a pointer, whether or not views are on.
    fn stands_for_container(&self, ty: TypeReference) -> bool {
        let _ = ty;
        false
    }

    /// The types whose identity has this base, in a stable order, for
    /// views that construct a type from its arguments.
    fn types_with_base(&self, base: &str) -> Vec<TypeReference> {
        let _ = base;
        Vec::new()
    }

    /// The global `name` of the module a view presents a value of, for a
    /// view's `global(NAME)`; nothing for any other scope.
    fn global(&self, name: &str) -> Result<Lookup<Self::Object>, Refusal> {
        let _ = name;
        Ok(Lookup::NotFound)
    }

    /// The global `name` of the module whose types this scope binds views
    /// against, as a step that reaches it from anywhere, and its type.
    fn global_step(&self, name: &str) -> Result<Option<(Self::Step, TypeReference)>, Refusal> {
        let _ = name;
        Ok(None)
    }
}

/// The most keys one tuple key may hold, and how deep tuples may nest in
/// one. A lookup hashes and compares a tuple key item by item, recursively,
/// reading the program's items for each, so these bound its work and its
/// recursion; a map's composite keys hold a few values.
pub const MAX_TUPLE_KEYS: usize = 64;
pub const MAX_TUPLE_DEPTH: usize = 8;

/// A key a map is indexed by: the value written in its brackets.
#[derive(Debug, Clone, PartialEq)]
pub enum Key {
    Integer(Exact),
    Float(Float),
    Bool(bool),
    Address(u64),
    Text(Vec<u8>),

    /// A dynamic language's value for nothing, such as Python's `None`.
    Nothing,

    /// Several keys in order, written `(a, b)`: a tuple.
    Tuple(Vec<Self>),

    /// A program's object of a dynamic type, by its [`Machine::dynamic_key`]
    /// value: its runtime finds the item its language's indexing finds,
    /// which may compare the object's identity before its value.
    Object(u64),
}

impl Key {
    /// Whether a map's key, as inspection presents it, equals this one as
    /// `==` would compare them. A key the program state cannot provide
    /// stops the search; one of a type this key cannot equal is refused.
    pub fn matches(&self, key: &InspectedValue) -> Result<bool, Stop> {
        let mismatch = || self.mismatch(key.type_info.as_ref());

        let VariableState::Available { value, text, .. } = &key.state else {
            return Err(Stop::missing(key.state.clone()));
        };

        let exact = match value {
            VariableValue::Scalar(ScalarValue::Signed(value)) => Some(Exact::from(*value)),
            VariableValue::Scalar(ScalarValue::Unsigned(value)) => Some(Exact::from(*value)),
            VariableValue::Enumeration { value, .. } => Some(Exact::from(*value)),
            _ => None,
        };

        let float = match value {
            VariableValue::Scalar(ScalarValue::Floating(value)) => Some(Float::from_value(*value)),
            _ => None,
        };

        let equal =
            |ordering: Option<std::cmp::Ordering>| ordering == Some(std::cmp::Ordering::Equal);
        match self {
            Self::Integer(wanted) => match (exact, float) {
                (Some(exact), _) => Ok(exact == *wanted),
                (_, Some(float)) => Ok(equal(float.compare_exact(*wanted))),
                _ => Err(mismatch()),
            },
            Self::Float(wanted) => match (exact, float) {
                (Some(exact), _) => Ok(equal(wanted.compare_exact(exact))),
                (_, Some(float)) => Ok(equal(float.compare(*wanted))),
                _ => Err(mismatch()),
            },
            Self::Bool(wanted) => match value {
                VariableValue::Scalar(ScalarValue::Boolean(value)) => Ok(value == wanted),
                _ => Err(mismatch()),
            },
            Self::Address(wanted) => match value {
                VariableValue::Address(address) => Ok(address.address.get() == *wanted),
                _ => Err(mismatch()),
            },
            Self::Nothing | Self::Tuple(_) | Self::Object(_) => Err(mismatch()),
            Self::Text(wanted) => {
                let Some(text) = text else {
                    return Err(mismatch());
                };

                let read = text.bytes.as_ref();
                match text.completion {
                    TextCompletion::Complete => Ok(read == wanted.as_slice()),
                    // Text known to differ before what could not be read
                    // differs.
                    _ if !wanted.starts_with(read) => Ok(false),
                    _ => Err(Stop::missing(VariableState::Unavailable(
                        VariableUnavailableReason::EvaluationLimit,
                    ))),
                }
            }
        }
    }

    /// Why a map whose keys are of the type `info` describes holds no key
    /// equal to this one.
    fn mismatch(&self, info: Option<&TypeInfo>) -> Stop {
        let name = info.map_or_else(|| "?".to_owned(), |info| info.name.to_string());
        Stop::Refused(Refusal::new(
            ErrorKind::Type,
            format!(
                "the map's keys are `{name}`, which {} cannot equal",
                self.describe()
            ),
        ))
    }

    /// The key as a message describes it.
    fn describe(&self) -> String {
        match self {
            Self::Integer(value) => format!("the integer {value}"),
            Self::Float(value) => format!("the float {value}"),
            Self::Bool(value) => format!("`{value}`"),
            Self::Address(value) => format!("the address {value:#x}"),
            Self::Text(_) => "a string".to_owned(),
            Self::Nothing => "the value for nothing".to_owned(),
            Self::Tuple(items) if items.len() == 1 => "a tuple of 1 key".to_owned(),
            Self::Tuple(items) => format!("a tuple of {} keys", items.len()),
            Self::Object(address) => format!("the object at {address:#x}"),
        }
    }
}

impl fmt::Display for Key {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Integer(value) => write!(formatter, "{value}"),
            Self::Float(value) => write!(formatter, "{value}"),
            Self::Bool(value) => write!(formatter, "{value}"),
            Self::Address(value) => write!(formatter, "{value:#x}"),
            Self::Text(bytes) => write!(formatter, "{:?}", String::from_utf8_lossy(bytes)),
            Self::Nothing => formatter.write_str("nothing"),
            Self::Tuple(items) => {
                formatter.write_str("(")?;
                for (position, item) in items.iter().enumerate() {
                    if position > 0 {
                        formatter.write_str(", ")?;
                    }

                    write!(formatter, "{item}")?;
                }

                formatter.write_str(if items.len() == 1 { ",)" } else { ")" })
            }
            Self::Object(address) => write!(formatter, "the object at {address:#x}"),
        }
    }
}

/// Whether the key a map holds at `at` equals `key`, a tuple, as the
/// language's `==` would compare them: element by element, each element of
/// the map's key found by [`Machine::tuple_elements`], and each element that
/// is no tuple compared as `present` presents it. Comparing a program's
/// data runs none of its code, so every element is compared, and an
/// element of a type its key cannot equal is refused whatever the others
/// hold. An unequal element makes the keys unequal even where another
/// cannot be read; a key that is no tuple, or a tuple of another length,
/// is refused.
pub fn tuple_matches<M: Machine>(
    key: &[Key],
    at: &M::Place,
    machine: &mut M,
    present: &mut impl FnMut(&mut M, &M::Place) -> Result<InspectedValue, Stop>,
) -> Result<bool, Stop> {
    tuple_matches_in(key, at, machine, present, 0)
}

fn tuple_matches_in<M: Machine>(
    key: &[Key],
    at: &M::Place,
    machine: &mut M,
    present: &mut impl FnMut(&mut M, &M::Place) -> Result<InspectedValue, Stop>,
    depth: usize,
) -> Result<bool, Stop> {
    let Some(elements) = machine.tuple_elements(at)? else {
        let info = present(machine, at)?.type_info;
        let name = info.map_or_else(|| "?".to_owned(), |info| info.name.to_string());
        return Err(Stop::Refused(Refusal::new(
            ErrorKind::Type,
            format!(
                "the map's keys are `{name}`, which is no tuple: a key of several values \
                     matches a tuple's elements by position, never a record's members"
            ),
        )));
    };

    if elements.len() != key.len() {
        let info = present(machine, at)?.type_info;
        let name = info.map_or_else(|| "?".to_owned(), |info| info.name.to_string());
        return Err(Stop::Refused(Refusal::new(
            ErrorKind::Type,
            format!(
                "the map's keys are `{name}`, tuples of {} elements, which {} cannot equal",
                elements.len(),
                Key::Tuple(key.to_vec()).describe()
            ),
        )));
    }

    let mut unequal = false;
    let mut unreadable = None;
    for (wanted, element) in key.iter().zip(&elements) {
        machine.charge()?;
        let equal = match wanted {
            Key::Tuple(items) if depth + 1 < MAX_TUPLE_DEPTH => {
                tuple_matches_in(items, element, machine, present, depth + 1)
            }
            Key::Tuple(_) => Err(Stop::Refused(Refusal::new(
                ErrorKind::Limit,
                format!("tuples in a key may nest at most {MAX_TUPLE_DEPTH} deep"),
            ))),
            wanted => {
                let presented = present(machine, element)?;
                wanted.matches(&presented)
            }
        };

        match equal {
            Ok(equal) => unequal |= !equal,
            Err(Stop::Missing(state)) => {
                unreadable.get_or_insert(Stop::Missing(state));
            }
            Err(stop) => return Err(stop),
        }
    }

    match unreadable {
        Some(stop) if !unequal => Err(stop),
        _ => Ok(!unequal),
    }
}

/// A value of a dynamic type, loaded where an operation needs its value:
/// its type's name, which messages show, and what it holds.
#[derive(Debug, Clone, PartialEq)]
pub struct Loaded {
    pub type_name: Arc<str>,
    pub value: LoadedValue,
}

/// What a value of a dynamic type holds, as far as the evaluator computes
/// with it.
#[derive(Debug, Clone, PartialEq)]
pub enum LoadedValue {
    /// The value its language has for no value, as Python's `None`, which
    /// equals only itself.
    Nothing,
    Bool(bool),
    Int(Exact),
    Float(Float),

    /// Text, encoded as UTF-8, as far as it was read.
    Text(TextSummary),
    Bytes(TextSummary),

    /// A built-in collection of `length` items, true when it holds any.
    Collection {
        length: u64,
    },

    /// Any other value, which is neither compared nor computed with, since
    /// its type's own code decides what those mean.
    Object,
}

/// Where the bytes of text are: its first byte's address, and how many.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TextSpan {
    pub address: u64,
    pub length: u64,
}

/// Why running stopped short of a value.
#[derive(Debug)]
pub enum Stop {
    /// The program state cannot provide the value: an unavailable or
    /// malformed state.
    Missing(Box<VariableState>),

    /// The expression asks for something the debugger refuses.
    Refused(Refusal),

    /// The debugger itself failed.
    Failed(crate::Error),
}

impl Stop {
    /// The program state cannot provide the value.
    pub fn missing(state: VariableState) -> Self {
        Self::Missing(Box::new(state))
    }
}

impl From<Refusal> for Stop {
    fn from(refusal: Refusal) -> Self {
        Self::Refused(refusal)
    }
}

/// Run-time access to one validated stop.
pub trait Machine: TypeSource {
    type Object;
    type Step;

    /// Where a program-typed value is.
    type Place: Clone + fmt::Debug;

    /// Accounts one unit of evaluation work.
    fn charge(&mut self) -> Result<(), Stop>;

    fn locate(&mut self, object: &Self::Object) -> Result<Self::Place, Stop>;

    /// Checks index values against static bounds, before any storage is
    /// read.
    fn check_indices(&self, step: &Self::Step, indices: &[i128]) -> Result<(), Stop>;

    fn step(
        &mut self,
        from: &Self::Place,
        step: &Self::Step,
        indices: &[i128],
    ) -> Result<Self::Place, Stop>;

    /// The place of a value of `ty` in memory at `address`.
    fn place_at(&mut self, address: u64, ty: TypeReference) -> Result<Self::Place, Stop>;

    /// The address a place is stored at, refused when it is in a register
    /// or computed.
    fn address(&self, at: &Self::Place) -> Result<u64, Stop>;

    /// Decodes the value at a place of scalar type.
    fn load(&mut self, at: &Self::Place) -> Result<VariableValue, Stop>;

    /// Reads memory the expression addresses directly.
    fn read(&mut self, address: u64, size: usize) -> Result<Vec<u8>, Stop>;

    /// The text a place holds, or `None` when it is not text.
    fn text(&mut self, at: &Self::Place) -> Result<Option<TextSummary>, Stop>;

    /// A slice's run-time length.
    fn length(&mut self, at: &Self::Place) -> Result<u64, Stop>;

    /// The capacity a slice's descriptor records.
    fn capacity(&mut self, at: &Self::Place) -> Result<u64, Stop> {
        let _ = at;
        Err(Stop::Refused(Refusal::new(
            ErrorKind::Type,
            "the slice records no capacity",
        )))
    }

    /// A register's value, zero-extended.
    fn register(&mut self, register: &Register) -> Result<u128, Stop>;

    /// The value at a place, as inspection presents it.
    fn present(&mut self, at: &Self::Place) -> Result<InspectedValue, Stop>;

    /// The interpreter's object at a place as the debugger presents it
    /// without a view, as the view that presents it and does not `show`
    /// lends it.
    fn present_stored(&mut self, at: &Self::Place) -> Result<InspectedValue, Stop> {
        let _ = at;
        Err(Stop::Refused(Refusal::new(
            ErrorKind::Unsupported,
            "this machine holds no interpreter's objects",
        )))
    }

    /// A computed value of program type, from its bytes.
    fn present_bytes(&mut self, ty: TypeReference, bytes: &[u8]) -> Result<InspectedValue, Stop>;

    /// A computed pointer to `pointee`, which dereferences.
    fn present_pointer(
        &mut self,
        address: u64,
        pointee: Option<TypeReference>,
        type_info: TypeInfo,
    ) -> Result<InspectedValue, Stop>;

    /// The inspection's completion and usage so far, for results the
    /// evaluator builds itself.
    fn finish(&self, type_info: Option<TypeInfo>, state: VariableState) -> InspectedValue;

    /// How many elements a value with no length of its own holds, or the
    /// length of its text, as a view presents it; `None` when no view
    /// presents its type. A value of a dynamic type has the length its
    /// language gives it, and one that has none is refused.
    fn presented_length(&mut self, at: &Self::Place) -> Result<Option<u64>, Stop> {
        let _ = at;
        Ok(None)
    }

    /// How many elements a value has room for, as the `capacity` field of
    /// the view that presents it says; `None` when no view gives it one.
    fn presented_capacity(&mut self, at: &Self::Place) -> Result<Option<u64>, Stop> {
        let _ = at;
        Ok(None)
    }

    /// The place of element `index` of a value, as the view that presents
    /// it says, for a view that presents a value as another; `None` when no
    /// view presents its type.
    fn presented_element(
        &mut self,
        at: &Self::Place,
        index: i128,
    ) -> Result<Option<Self::Place>, Stop> {
        let _ = (at, index);
        Ok(None)
    }

    /// The place of the entry for `key` of a value, as the view that
    /// presents it as a map says, for a view that presents a value as
    /// another; `None` when the map holds no such key, and refused when no
    /// view presents the value as a map.
    fn presented_entry(
        &mut self,
        at: &Self::Place,
        key: &Key,
    ) -> Result<Option<Self::Place>, Stop> {
        let _ = (at, key);
        Err(Stop::Refused(Refusal::new(
            ErrorKind::Type,
            "the value the view presents is no map",
        )))
    }

    /// The places of the elements of the tuple at `at`, in order, or `None`
    /// when its type is no tuple: a type its language makes one, such as a
    /// Rust or Zig tuple, or one a view presents as a tuple, such as a C++
    /// `std::pair`. A struct is no tuple, though its members have an order.
    fn tuple_elements(&mut self, at: &Self::Place) -> Result<Option<Vec<Self::Place>>, Stop> {
        let _ = at;
        Ok(None)
    }

    /// Where the bytes of the text a place holds are, or `None` when it
    /// holds no text.
    fn text_span(&mut self, at: &Self::Place) -> Result<Option<TextSpan>, Stop> {
        let _ = at;
        Ok(None)
    }

    /// The place of the value a map at `from` holds for `key`, through a
    /// step a scope planned with [`StepKind::Entry`], or `None` when the
    /// map holds no such key. For a value of a dynamic type, through a step
    /// [`Scope::plan_dynamic`] planned, the item its language's indexing
    /// finds, and a missing one is refused with why.
    fn entry(
        &mut self,
        from: &Self::Place,
        step: &Self::Step,
        key: &Key,
    ) -> Result<Option<Self::Place>, Stop> {
        let _ = (from, step, key);
        Err(Stop::Refused(Refusal::new(
            ErrorKind::Unsupported,
            "this machine indexes no maps",
        )))
    }

    /// The type arguments of the generic function whose code is at
    /// `address`, each with its parameter's name; refused when no
    /// function's debug information describes the code there.
    fn function_generics(&mut self, address: u64) -> Result<Vec<(Arc<str>, TypeReference)>, Stop> {
        Err(Stop::Refused(Refusal::new(
            ErrorKind::Unsupported,
            format!("no function the debug information describes has its code at {address:#x}"),
        )))
    }

    /// The id of the task the stopped thread runs, such as a goroutine's:
    /// unavailable when the thread runs none, and refused where the program
    /// has no tasks or the debugger cannot tell.
    fn task(&mut self) -> Result<u64, Stop> {
        Err(Stop::Refused(Refusal::new(
            ErrorKind::Unsupported,
            "no task is known here",
        )))
    }

    /// The place of the interpreter's object at `address`, as a cast to its
    /// dynamic type `ty` names it.
    fn dynamic_at(&mut self, ty: &str, address: u64) -> Result<Self::Place, Stop> {
        let _ = (ty, address);
        Err(Stop::Refused(Refusal::new(
            ErrorKind::Unsupported,
            "this machine reads no interpreter's objects",
        )))
    }

    /// The value by which its runtime names the dynamic-typed object at
    /// `at`, as [`Key::Object`] carries it. By default, its address.
    fn dynamic_key(&mut self, at: &Self::Place) -> Result<u64, Stop> {
        self.address(at)
    }

    /// What the place of a value of a dynamic type holds, for an operation
    /// that needs its value.
    fn load_dynamic(&mut self, at: &Self::Place) -> Result<Loaded, Stop> {
        let _ = at;
        Err(Stop::Refused(Refusal::new(
            ErrorKind::Unsupported,
            "this machine holds no values of a dynamic type",
        )))
    }

    /// The value a scope bound to `object` with [`Lookup::Bound`].
    fn bound(&mut self, object: &Self::Object) -> Result<Value<Self::Place>, Stop> {
        let _ = object;
        Err(Stop::Refused(Refusal::new(
            ErrorKind::Unsupported,
            "this machine computes no bound values",
        )))
    }
}
