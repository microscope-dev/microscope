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

#[derive(Debug, Clone)]
pub enum Lookup<O> {
    Object {
        object: O,
        ty: Result<TypeReference, std::sync::Arc<str>>,
    },

    Enumerator {
        value: Exact,
        ty: TypeReference,
    },

    Constant(Exact),

    Bound {
        object: O,
        ty: super::types::Ty,
    },

    Dynamic(O),

    Ambiguous(Vec<String>),
    NotFound,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TypeQuery {
    pub name: String,

    pub tag: Option<Tag>,
}

#[derive(Debug, Clone)]
pub enum TypeLookup {
    Found(TypeReference),
    Ambiguous(Vec<String>),
    NotFound,
}

#[derive(Debug, Clone, Copy)]
pub enum StepKind<'a> {
    Deref,

    Member(&'a str),

    Index { available: usize },

    Base(TypeReference),

    Entry,
}

#[derive(Debug, Clone)]
pub struct Planned<S> {
    pub step: S,

    pub result: Option<TypeReference>,

    pub consumed: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Register {
    pub number: u16,

    pub width: u8,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CallValues {
    pub args: [u64; 6],

    pub returned: Option<i64>,

    pub path: Option<Vec<u8>>,
}

pub trait Scope: TypeSource {
    type Object: Clone + fmt::Debug;

    type Step: Clone + fmt::Debug;

    fn lookup(&self, name: &str, outermost: bool) -> Result<Lookup<Self::Object>, Refusal>;

    fn lookup_type(&self, query: &TypeQuery) -> TypeLookup;

    fn plan(&self, from: TypeReference, step: StepKind<'_>)
    -> Result<Planned<Self::Step>, Refusal>;

    fn register(&self, name: &str) -> Option<Register>;

    fn plan_dynamic(&self, step: StepKind<'_>) -> Result<Self::Step, Refusal> {
        let _ = step;
        Err(Refusal::new(
            ErrorKind::Unsupported,
            "no value here has a dynamic type",
        ))
    }

    fn dynamic_type(&self, name: &str) -> bool {
        let _ = name;
        false
    }

    fn call_values(&self) -> Option<&CallValues> {
        None
    }

    fn has_view(&self, ty: TypeReference) -> bool {
        let _ = ty;
        false
    }

    fn stands_for_container(&self, ty: TypeReference) -> bool {
        let _ = ty;
        false
    }

    fn types_with_base(&self, base: &str) -> Vec<TypeReference> {
        let _ = base;
        Vec::new()
    }

    fn global(&self, name: &str) -> Result<Lookup<Self::Object>, Refusal> {
        let _ = name;
        Ok(Lookup::NotFound)
    }

    fn global_step(&self, name: &str) -> Result<Option<(Self::Step, TypeReference)>, Refusal> {
        let _ = name;
        Ok(None)
    }
}

pub const MAX_TUPLE_KEYS: usize = 64;
pub const MAX_TUPLE_DEPTH: usize = 8;

#[derive(Debug, Clone, PartialEq)]
pub enum Key {
    Integer(Exact),
    Float(Float),
    Bool(bool),
    Address(u64),
    Text(Vec<u8>),

    Nothing,

    Tuple(Vec<Self>),

    Object(u64),
}

impl Key {
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

                    _ if !wanted.starts_with(read) => Ok(false),
                    _ => Err(Stop::missing(VariableState::Unavailable(
                        VariableUnavailableReason::EvaluationLimit,
                    ))),
                }
            }
        }
    }

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

#[derive(Debug, Clone, PartialEq)]
pub struct Loaded {
    pub type_name: Arc<str>,

    pub language: Option<Rules>,
    pub value: LoadedValue,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rules {
    Python,

    JavaScript,
}

impl Rules {
    pub const fn name(self) -> &'static str {
        match self {
            Self::Python => "Python",
            Self::JavaScript => "JavaScript",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum LoadedValue {
    Nothing,

    Undefined,
    Bool(bool),
    Int(Exact),
    Float(Float),

    Text(TextSummary),
    Bytes(TextSummary),

    Collection {
        length: u64,
    },

    /// Any other value, which is neither compared nor computed with, since
    Object,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TextSpan {
    pub address: u64,
    pub length: u64,
}

#[derive(Debug)]
pub enum Stop {
    Missing(Box<VariableState>),

    Refused(Refusal),

    Failed(crate::Error),
}

impl Stop {
    pub fn missing(state: VariableState) -> Self {
        Self::Missing(Box::new(state))
    }
}

impl From<Refusal> for Stop {
    fn from(refusal: Refusal) -> Self {
        Self::Refused(refusal)
    }
}

pub trait Machine: TypeSource {
    type Object;
    type Step;

    type Place: Clone + fmt::Debug;

    fn charge(&mut self) -> Result<(), Stop>;

    fn locate(&mut self, object: &Self::Object) -> Result<Self::Place, Stop>;

    fn check_indices(&self, step: &Self::Step, indices: &[i128]) -> Result<(), Stop>;

    fn step(
        &mut self,
        from: &Self::Place,
        step: &Self::Step,
        indices: &[i128],
    ) -> Result<Self::Place, Stop>;

    fn place_at(&mut self, address: u64, ty: TypeReference) -> Result<Self::Place, Stop>;

    fn address(&self, at: &Self::Place) -> Result<u64, Stop>;

    fn load(&mut self, at: &Self::Place) -> Result<VariableValue, Stop>;

    fn read(&mut self, address: u64, size: usize) -> Result<Vec<u8>, Stop>;

    fn text(&mut self, at: &Self::Place) -> Result<Option<TextSummary>, Stop>;

    fn length(&mut self, at: &Self::Place) -> Result<u64, Stop>;

    fn capacity(&mut self, at: &Self::Place) -> Result<u64, Stop> {
        let _ = at;
        Err(Stop::Refused(Refusal::new(
            ErrorKind::Type,
            "the slice records no capacity",
        )))
    }

    fn register(&mut self, register: &Register) -> Result<u128, Stop>;

    fn present(&mut self, at: &Self::Place) -> Result<InspectedValue, Stop>;

    fn present_stored(&mut self, at: &Self::Place) -> Result<InspectedValue, Stop> {
        let _ = at;
        Err(Stop::Refused(Refusal::new(
            ErrorKind::Unsupported,
            "this machine holds no interpreter's objects",
        )))
    }

    fn present_bytes(&mut self, ty: TypeReference, bytes: &[u8]) -> Result<InspectedValue, Stop>;

    fn present_pointer(
        &mut self,
        address: u64,
        pointee: Option<TypeReference>,
        type_info: TypeInfo,
    ) -> Result<InspectedValue, Stop>;

    fn finish(&self, type_info: Option<TypeInfo>, state: VariableState) -> InspectedValue;

    fn presented_length(&mut self, at: &Self::Place) -> Result<Option<u64>, Stop> {
        let _ = at;
        Ok(None)
    }

    fn presented_capacity(&mut self, at: &Self::Place) -> Result<Option<u64>, Stop> {
        let _ = at;
        Ok(None)
    }

    fn presented_element(
        &mut self,
        at: &Self::Place,
        index: i128,
    ) -> Result<Option<Self::Place>, Stop> {
        let _ = (at, index);
        Ok(None)
    }

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

    fn tuple_elements(&mut self, at: &Self::Place) -> Result<Option<Vec<Self::Place>>, Stop> {
        let _ = at;
        Ok(None)
    }

    fn text_span(&mut self, at: &Self::Place) -> Result<Option<TextSpan>, Stop> {
        let _ = at;
        Ok(None)
    }

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

    fn function_generics(&mut self, address: u64) -> Result<Vec<(Arc<str>, TypeReference)>, Stop> {
        Err(Stop::Refused(Refusal::new(
            ErrorKind::Unsupported,
            format!("no function the debug information describes has its code at {address:#x}"),
        )))
    }

    fn task(&mut self) -> Result<u64, Stop> {
        Err(Stop::Refused(Refusal::new(
            ErrorKind::Unsupported,
            "no task is known here",
        )))
    }

    fn dynamic_at(&mut self, ty: &str, address: u64) -> Result<Self::Place, Stop> {
        let _ = (ty, address);
        Err(Stop::Refused(Refusal::new(
            ErrorKind::Unsupported,
            "this machine reads no interpreter's objects",
        )))
    }

    fn dynamic_key(&mut self, at: &Self::Place) -> Result<u64, Stop> {
        self.address(at)
    }

    fn load_dynamic(&mut self, at: &Self::Place) -> Result<Loaded, Stop> {
        let _ = at;
        Err(Stop::Refused(Refusal::new(
            ErrorKind::Unsupported,
            "this machine holds no values of a dynamic type",
        )))
    }

    fn bound(&mut self, object: &Self::Object) -> Result<Value<Self::Place>, Stop> {
        let _ = object;
        Err(Stop::Refused(Refusal::new(
            ErrorKind::Unsupported,
            "this machine computes no bound values",
        )))
    }
}
