//! The values of Python's built-in types, as the debugger presents them:
//! `None`, `bool`, `int` at any size, `float`, `str`, `bytes`, `list`,
//! `tuple`, `dict`, `set`, and `frozenset`, each by its exact type, and
//! classes and modules by their names, and the standard library's types
//! that only C defines as their own `repr` writes them ([`super::library`]).
//! An object of any other type, a subclass of one of those included, is
//! shown as Python's default `repr` shows it, by its type's name and its
//! address, with its attributes for children ([`super::instances`]).
//!
//! A value's summary is its text in Python's notation ([`super::text`]),
//! nesting containers to [`SUMMARY_DEPTH`] and showing [`SUMMARY_ITEMS`]
//! of each; what lies past either shows as `...`, and a container within
//! itself as Python's `repr` shows it. Every read is charged to the
//! inspection's budget, and an object that is not what its type says ends
//! its summary with a problem, never as an empty container.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::sync::Arc;

use super::code::{CodeId, CodeTable};
use super::instances::Class;
use super::library::Library;
use super::objects::{Charge, Head, Problem, Reader, Record, malformed};
use super::text;
use crate::VirtualAddress;
use crate::image::python::{Constant, Field, PythonLayout};
use crate::inspection::InspectionBudget;
use crate::runtime_model::{
    InterpretedChild, InterpretedKind, InterpretedLocal, InterpretedSlot, InterpretedValue,
};
use crate::{InspectionExhaustion, VariableUnavailableReason};

/// How deep a summary nests containers, and how many items of each it
/// shows.
pub(super) const SUMMARY_DEPTH: usize = 6;
pub(super) const SUMMARY_ITEMS: u64 = 64;

/// The most objects one summary reads.
pub(super) const SUMMARY_OBJECTS: usize = 512;

/// The most characters of a `str`, or bytes of a `bytes`, a summary shows.
const SUMMARY_TEXT: u64 = 1024;

/// The most digits of an `int` read: one with more is shown by its size.
const MAX_INT_DIGITS: u64 = 1 << 10;

/// The most entries of a `dict`'s table read to find its items.
const MAX_DICT_ENTRIES: u64 = 1 << 20;

/// How many of a `dict`'s entries, or a `set`'s, one read takes.
const DICT_ENTRIES_READ: u64 = 64;

/// The most slots of a `set`'s table read to find its items.
const MAX_SET_SLOTS: u64 = 1 << 20;

/// Where the process keeps the type objects of the built-in types the
/// debugger presents.
#[derive(Debug, Clone, Copy)]
pub(super) struct Types {
    pub(super) none: VirtualAddress,
    pub(super) bool: VirtualAddress,
    pub(super) int: VirtualAddress,
    pub(super) float: VirtualAddress,
    pub(super) str: VirtualAddress,
    pub(super) bytes: VirtualAddress,
    pub(super) list: VirtualAddress,
    pub(super) tuple: VirtualAddress,
    pub(super) dict: VirtualAddress,
    pub(super) set: VirtualAddress,
    pub(super) frozenset: VirtualAddress,
    pub(super) cell: VirtualAddress,
    pub(super) module: VirtualAddress,
    pub(super) function: VirtualAddress,

    /// The type of built-in functions and methods, as `len`.
    pub(super) builtin: VirtualAddress,

    /// The types of coroutines, and of generators, which asyncio's
    /// coroutines may be.
    pub(super) coroutine: VirtualAddress,
    pub(super) generator: VirtualAddress,
    pub(super) code: VirtualAddress,

    /// `type`, whose instances are classes.
    pub(super) class: VirtualAddress,

    /// The type of a slot's descriptor, and of the wrapper of a type's C
    /// function for a special method.
    pub(super) member: VirtualAddress,
    pub(super) wrapper: VirtualAddress,

    /// The type of a type's C method, as `list.append`.
    pub(super) method: VirtualAddress,

    /// `PyObject_GenericGetAttr`, how a type that reads its instances'
    /// attributes as `object` does reads them.
    pub(super) generic_getattr: VirtualAddress,
}

/// An inspection's budget, which pays for each read of its values.
pub(super) struct Budget<'b>(RefCell<&'b mut InspectionBudget>);

impl<'b> Budget<'b> {
    pub(super) const fn new(budget: &'b mut InspectionBudget) -> Self {
        Self(RefCell::new(budget))
    }

    fn exhaustion(&self) -> Option<InspectionExhaustion> {
        self.0.borrow().exhaustion()
    }
}

impl Charge for Budget<'_> {
    fn charge(&self, bytes: usize) -> Result<(), Problem> {
        self.0
            .borrow_mut()
            .consume_memory(bytes)
            .map_err(|exhaustion| {
                VariableUnavailableReason::InspectionLimit(exhaustion)
                    .to_string()
                    .into()
            })
    }
}

/// An object, as far as presenting it needs it read.
pub(super) enum Object {
    Nothing,
    Bool(bool),
    Int {
        negative: bool,
        digits: Vec<u32>,
    },

    /// An `int` of more digits than are read.
    LargeInt {
        digits: u64,
    },
    Float(f64),
    Str {
        points: Vec<u32>,
        length: u64,
    },
    Bytes {
        contents: Vec<u8>,
        length: u64,
    },
    List {
        items: VirtualAddress,
        length: u64,
    },
    Tuple {
        items: VirtualAddress,
        length: u64,
    },
    Dict(Dict),

    /// A `set` or a `frozenset`: its table of `mask + 1` slots, of which
    /// `used` hold items.
    Set {
        frozen: bool,
        table: VirtualAddress,
        mask: u64,
        used: u64,
    },

    /// A class, `this`, whose type is `meta`.
    Class {
        this: Arc<Class>,
        meta: Arc<Class>,
    },

    /// A module, whose dict holds its globals.
    Module {
        dict: VirtualAddress,
    },

    /// A function, by its qualified name.
    Function {
        name: Arc<str>,
    },

    /// A built-in function or method, by its name, and for a method, the
    /// name of its object's type and the object's address.
    Builtin {
        name: Arc<str>,
        bound: Option<(Arc<str>, VirtualAddress)>,
    },

    /// A code object: its name, the file it is compiled from, and its first
    /// line, or -1 for none.
    Code {
        name: Arc<str>,
        file: Arc<str>,
        line: i32,
    },

    /// An object of one of the standard library's types that only C
    /// defines ([`super::library`]), `class`, by the name the C of its
    /// own type gives that type, and its value.
    Library {
        class: Arc<Class>,
        name: Arc<str>,
        value: Library,
    },

    /// An object of any other type, and the value of a built-in type it
    /// holds, as one of a class deriving from `int` or `tuple` does, or
    /// of a library type.
    Instance {
        class: Arc<Class>,
        base: Option<Box<Self>>,
    },
}

/// A `dict`'s table of entries, in insertion order, or the keys an
/// instance's values share with its class's other instances'.
#[derive(Clone, Copy)]
pub(super) struct Dict {
    pub(super) object: VirtualAddress,
    pub(super) length: u64,

    /// Its hash table of indices into its entries, of `1 << log2_size`
    /// slots each `index_width` bytes wide, which its entries follow.
    pub(super) indices: VirtualAddress,
    pub(super) log2_size: u64,
    pub(super) index_width: u64,
    entries: VirtualAddress,
    pub(super) count: u64,

    /// Each entry's size, and where its key and value are within it, and
    /// the hash of its key, which a general table's entries keep.
    size: u64,
    key: u64,
    value: u64,
    hash: Option<u64>,

    /// The values, where the table's entries share their keys and keep
    /// their values apart.
    split: Option<Split>,
}

/// The values a `dict`, or an instance, keeps apart from the keys its
/// class's instances share: the value of the keys' entry `ix` at
/// `values[ix]`, for each `ix` below `capacity`, and the entries holding
/// values in their insertion order, which CPython keeps as a byte each.
#[derive(Clone, Copy)]
pub(super) struct Split {
    values: VirtualAddress,
    capacity: u64,

    /// Where the order's first byte is, and whether the order's bytes run
    /// down from there, as in 3.12, which keeps them before the values.
    order: VirtualAddress,
    descending: bool,
    size: u64,
}

/// One item of a `dict`: its key, its value, and the hash its entry keeps
/// of its key, where it keeps one.
#[derive(Clone, Copy)]
pub(super) struct Entry {
    pub(super) key: VirtualAddress,
    pub(super) value: VirtualAddress,
    pub(super) hash: Option<i64>,
}

/// Where a summary is: how many objects it has read, the containers it is
/// within, and why it shows less than all, if it does.
#[derive(Default)]
pub(super) struct Summary {
    pub(super) objects: usize,
    pub(super) within: Vec<VirtualAddress>,
    pub(super) problem: Option<Problem>,
}

/// The values of one stop's objects, and the budget that pays for them.
pub(super) struct Values<'a, 'b> {
    pub(super) reader: Reader<'a>,
    pub(super) types: Types,
    pub(super) budget: &'a Budget<'b>,

    /// Where the process keeps the secret its `str` hashes are keyed by,
    /// `_Py_HashSecret`, and whether hashing with it gives the hashes the
    /// process's own strings keep, once that is known.
    pub(super) secret: Option<VirtualAddress>,
    pub(super) hashing: std::cell::Cell<Option<bool>>,

    /// `_PyRuntime`, whose interpreters keep the static types' dicts, and
    /// the type objects read so far.
    pub(super) runtime: VirtualAddress,
    pub(super) classes: RefCell<BTreeMap<VirtualAddress, Arc<Class>>>,
}

impl Values<'_, '_> {
    /// The local variables of the frame `frame`, which runs the code in
    /// row `code` of `table`, in the order the code keeps them. Those an
    /// inlined comprehension hides are left out, as CPython's own
    /// `f_locals` leaves them out.
    pub(super) fn locals(
        &self,
        table: &CodeTable,
        code: CodeId,
        frame: &Record<'_>,
    ) -> Result<Vec<InterpretedLocal>, Problem> {
        let layout = self.reader.layout;
        let locals = table.locals(code);
        let start = frame
            .address
            .wrapping_add(self.reader.offset(Field::FrameLocalsplus)?);
        let slots = self.reader.bytes(start, locals.len() as u64 * 8)?;
        let hidden = layout.constant(Constant::FastHidden);
        let shared = layout.constant(Constant::FastCell) | layout.constant(Constant::FastFree);
        let parameters = usize::try_from(table.row(code).parameters).unwrap_or(usize::MAX);
        let mut variables = Vec::with_capacity(locals.len());

        for (index, (local, slot)) in locals.iter().zip(slots.as_chunks::<8>().0).enumerate() {
            if local.kind & hidden != 0 {
                continue;
            }

            let bits = u64::from_le_bytes(*slot);
            let slot = if local.kind & shared != 0 {
                match self.reference(bits).and_then(|object| self.cell(object)) {
                    Ok(Some(object)) => self.value(object),
                    Ok(None) => InterpretedSlot::Unbound,
                    Err(problem) => InterpretedSlot::Unreadable(problem),
                }
            } else {
                self.local(bits)
            };

            variables.push(InterpretedLocal {
                name: Arc::clone(table.name(local.name)),
                parameter: index < parameters,
                slot,
            });
        }

        Ok(variables)
    }

    /// What a frame's slot for a local holds: an object, or nothing.
    fn local(&self, bits: u64) -> InterpretedSlot {
        match self.reference(bits) {
            Ok(Some(object)) => self.value(object),
            Ok(None) => InterpretedSlot::Unbound,
            Err(problem) => InterpretedSlot::Unreadable(problem),
        }
    }

    /// The object a variable a frame shares with closures holds, where its
    /// slot holds `object`: what the cell holds, or the object itself, for
    /// a parameter the frame has not yet put in its cell.
    pub(super) fn cell(
        &self,
        object: Option<VirtualAddress>,
    ) -> Result<Option<VirtualAddress>, Problem> {
        let Some(object) = object else {
            return Ok(None);
        };

        if self.reader.field_word(object, Field::ObjectType)? != self.types.cell.get() {
            return Ok(Some(object));
        }

        let held = self.reader.field_word(object, Field::CellRef)?;
        Ok(Some(held)
            .filter(|held| *held != 0)
            .map(VirtualAddress::new))
    }

    /// The object a frame's slot for a local refers to, or `None` when it
    /// refers to none.
    pub(super) fn reference(&self, bits: u64) -> Result<Option<VirtualAddress>, Problem> {
        reference(self.reader.layout, bits)
    }

    /// The object at `object`, presented.
    pub(super) fn value(&self, object: VirtualAddress) -> InterpretedSlot {
        match self.presented(object) {
            Ok(value) => InterpretedSlot::Value(value),
            Err(problem) => self.failed(problem),
        }
    }

    /// Why a read failed: the inspection's limits, once they are reached,
    /// which is what stopped it, as every read past them fails.
    pub(super) fn failed(&self, problem: Problem) -> InterpretedSlot {
        self.budget.exhaustion().map_or_else(
            || InterpretedSlot::Unreadable(problem),
            InterpretedSlot::Limit,
        )
    }

    fn presented(&self, address: VirtualAddress) -> Result<InterpretedValue, Problem> {
        let object = self.object(address)?;
        let class = VirtualAddress::new(self.reader.field_word(address, Field::ObjectType)?);
        let (module, type_name) = Self::type_name(&object);

        let (kind, attributes) = match &object {
            Object::Instance { class, base } => (
                base.as_deref()
                    .map_or(InterpretedKind::Instance, |base| self.kind(base)),
                self.attribute_count(address, class)?,
            ),
            object => (self.kind(object), 0),
        };

        let mut summary = Summary {
            objects: 1,
            within: vec![address],
            problem: None,
        };

        let text = self.text(address, &object, 0, &mut summary);

        let array = match &object {
            Object::Library {
                value: Library::Array(array),
                ..
            } => array.interpreted(),
            Object::Instance {
                base: Some(base), ..
            } => match &**base {
                Object::Library {
                    value: Library::Array(array),
                    ..
                } => array.interpreted(),
                _ => None,
            },
            _ => None,
        };

        Ok(InterpretedValue {
            object: address,
            class,
            module,
            type_name,
            kind,
            attributes,
            summary: text.into(),
            problem: summary.problem,
            part: Arc::from([]),
            array,
        })
    }

    /// What kind of value `object` is.
    fn kind(&self, object: &Object) -> InterpretedKind {
        match object {
            Object::Nothing => InterpretedKind::Nothing,
            Object::Bool(value) => InterpretedKind::Bool(*value),
            Object::Int { negative, digits } => {
                InterpretedKind::Int(exact(*negative, digits, self.shift()))
            }
            Object::LargeInt { .. } => InterpretedKind::Int(None),
            Object::Float(value) => InterpretedKind::Float(*value),
            Object::Str { .. } => InterpretedKind::Text,
            Object::Bytes { .. } => InterpretedKind::Bytes,
            Object::List { length, .. } | Object::Tuple { length, .. } => {
                InterpretedKind::Sequence { length: *length }
            }
            Object::Dict(dict) => InterpretedKind::Map {
                length: dict.length,
            },
            Object::Set { used, .. } => InterpretedKind::Sequence { length: *used },
            Object::Library { value, .. } => value.kind(),
            Object::Instance { .. } => InterpretedKind::Instance,
            Object::Class { .. }
            | Object::Module { .. }
            | Object::Function { .. }
            | Object::Builtin { .. }
            | Object::Code { .. } => InterpretedKind::Other,
        }
    }

    /// The children `range` of the object at `object`, or of its part at
    /// `part`: a sequence's elements, a `dict`'s entries in insertion order,
    /// or an instance's attributes, after those of the built-in value it
    /// holds.
    pub(super) fn children(
        &self,
        object: VirtualAddress,
        part: &[u64],
        range: std::ops::Range<u64>,
    ) -> Result<Vec<InterpretedChild>, Problem> {
        let read = self.object(object)?;
        if !part.is_empty() {
            let (class, array) = match &read {
                Object::Library {
                    class,
                    value: Library::Array(array),
                    ..
                } => (class, array),
                Object::Instance {
                    class,
                    base: Some(base),
                } => match &**base {
                    Object::Library {
                        value: Library::Array(array),
                        ..
                    } => (class, array),
                    _ => return Err(malformed(object, "object")),
                },
                _ => return Err(malformed(object, "object")),
            };
            return self.array_children((object, class), array, part, range);
        }

        let (class, base) = match read {
            Object::Instance { class, base } => (class, base),
            read => return self.contents(object, &read, range),
        };

        let (mut children, elements) = match base {
            Some(base) => {
                let elements = match self.kind(&base) {
                    InterpretedKind::Sequence { length } | InterpretedKind::Map { length } => {
                        length
                    }
                    _ => 0,
                };
                let held = range.start.min(elements)..range.end.min(elements);
                let children = if held.is_empty() {
                    Vec::new()
                } else {
                    self.contents(object, &base, held)?
                };
                (children, elements)
            }
            None => (Vec::new(), 0),
        };

        let attributes = range.start.saturating_sub(elements)..range.end.saturating_sub(elements);
        if !attributes.is_empty() {
            children.extend(
                self.attributes(object, &class, attributes)?
                    .into_iter()
                    .map(|(name, value)| InterpretedChild::Attribute {
                        name,
                        value: self.value(value),
                    }),
            );
        }

        Ok(children)
    }

    /// The elements or entries `range` of the built-in container `read`,
    /// at `object`.
    fn contents(
        &self,
        object: VirtualAddress,
        read: &Object,
        range: std::ops::Range<u64>,
    ) -> Result<Vec<InterpretedChild>, Problem> {
        match *read {
            Object::List { items, length } | Object::Tuple { items, length } => {
                let end = range.end.min(length);
                let start = range.start.min(end);
                let items = self.items(items, start, end)?;
                Ok(items
                    .into_iter()
                    .map(|item| InterpretedChild::Element(self.value(item)))
                    .collect())
            }
            Object::Dict(dict) => {
                let mut children = Vec::new();
                self.entries(dict, range, |entry| {
                    children.push(InterpretedChild::Entry {
                        key: self.value(entry.key),
                        value: self.value(entry.value),
                    });
                    Ok(())
                })?;
                Ok(children)
            }
            Object::Set {
                table, mask, used, ..
            } => {
                let mut children = Vec::new();
                self.set_items(object, table, mask, used, range, |item| {
                    children.push(InterpretedChild::Element(self.value(item)));
                    Ok(())
                })?;
                Ok(children)
            }
            Object::Library {
                value:
                    Library::Deque {
                        block,
                        index,
                        length,
                        ..
                    },
                ..
            } => Ok(self
                .deque_items((block, index, length), range)?
                .into_iter()
                .map(|item| InterpretedChild::Element(self.value(item)))
                .collect()),
            Object::Library {
                value: Library::DefaultDict { dict, .. },
                ..
            } => self.contents(object, &Object::Dict(dict), range),
            Object::Library {
                ref class,
                value: Library::Array(ref array),
                ..
            } => self.array_children((object, class), array, &[], range),
            Object::Library {
                value: Library::OrderedDict { dict, first },
                ..
            } => {
                let mut children = Vec::new();
                self.linked_entries((dict, first), range, |entry| {
                    children.push(InterpretedChild::Entry {
                        key: self.value(entry.key),
                        value: self.value(entry.value),
                    });
                    Ok(())
                })?;
                Ok(children)
            }
            _ => Err(format!("the object at {object:#x} holds no items").into()),
        }
    }

    /// Reads as much of the object at `address` as presenting it needs.
    pub(super) fn object(&self, address: VirtualAddress) -> Result<Object, Problem> {
        if address == VirtualAddress::new(0) {
            return Err("the reference is NULL".into());
        }

        let head = self.reader.head(address)?;
        let ty = VirtualAddress::new(head.field_word(Field::ObjectType)?);
        if let Some(object) = self.built_in(&head, ty)? {
            return Ok(object);
        }

        if self.is_class(ty)? {
            return Ok(Object::Class {
                this: self.class(address)?,
                meta: self.class(ty)?,
            });
        }

        let class = self.class(ty)?;
        if let Some(object) = self.library(&head, &class, ty)? {
            return Ok(object);
        }

        let base = match self.static_base(&class)? {
            Some(base) => match self.built_in(&head, base)? {
                Some(base) => Some(base),
                None => self.library(&head, &self.class(base)?, ty)?,
            },
            None => None,
        };
        Ok(Object::Instance {
            class,
            base: base.map(Box::new),
        })
    }

    /// The object the head is of, read as the built-in type `ty` keeps its
    /// objects, or `None` where `ty` is no type whose objects are read.
    fn built_in(&self, head: &Head<'_, '_>, ty: VirtualAddress) -> Result<Option<Object>, Problem> {
        let (address, reader, types) = (head.address(), &self.reader, &self.types);
        let object = if ty == types.none {
            Object::Nothing
        } else if ty == types.bool {
            let (negative, digits) = self.int(head)?;
            Object::Bool(!negative && digits.iter().any(|digit| *digit != 0))
        } else if ty == types.int {
            let tag = head.field_word(Field::LongTag)?;
            let count = tag >> reader.layout.constant(Constant::LongNonSizeBits);
            if count > MAX_INT_DIGITS {
                return Ok(Some(Object::LargeInt { digits: count }));
            }

            let (negative, digits) = self.int(head)?;
            Object::Int { negative, digits }
        } else if ty == types.float {
            Object::Float(f64::from_bits(head.field_word(Field::FloatValue)?))
        } else if ty == types.str {
            let (points, length) = head.code_points(SUMMARY_TEXT)?;
            Object::Str { points, length }
        } else if ty == types.bytes {
            let length = length(head.field_word(Field::BytesSize)?, address)?;
            let contents = head.field_bytes(Field::BytesData, length.min(SUMMARY_TEXT))?;
            Object::Bytes { contents, length }
        } else if ty == types.list {
            let length = length(head.field_word(Field::ListSize)?, address)?;
            let items = VirtualAddress::new(head.field_word(Field::ListItems)?);
            if length > 0 && items == VirtualAddress::new(0) {
                return Err(malformed(address, "list"));
            }

            Object::List { items, length }
        } else if ty == types.tuple {
            let length = length(head.field_word(Field::TupleSize)?, address)?;
            let items = address.wrapping_add(reader.offset(Field::TupleItems)?);
            Object::Tuple { items, length }
        } else if ty == types.dict {
            self.dict(head)?
        } else if ty == types.set || ty == types.frozenset {
            let used = length(head.field_word(Field::SetUsed)?, address)?;
            let mask = head.field_word(Field::SetMask)?;
            let table = VirtualAddress::new(head.field_word(Field::SetTable)?);
            if mask
                .checked_add(1)
                .is_none_or(|slots| !slots.is_power_of_two())
                || used > mask
            {
                return Err(malformed(address, "set"));
            }

            Object::Set {
                frozen: ty == types.frozenset,
                table,
                mask,
                used,
            }
        } else if ty == types.module {
            Object::Module {
                dict: VirtualAddress::new(head.field_word(Field::ModuleDict)?),
            }
        } else if ty == types.function {
            let name = VirtualAddress::new(head.field_word(Field::FunctionQualname)?);
            Object::Function {
                name: self.name_text(name, address, "function")?,
            }
        } else if ty == types.builtin {
            self.builtin(head)?
        } else if ty == types.code {
            let name = VirtualAddress::new(head.field_word(Field::CodeName)?);
            let file = VirtualAddress::new(head.field_word(Field::CodeFilename)?);
            let line = head.field_bytes(Field::CodeFirstlineno, 4)?;
            let line = i32::from_le_bytes(
                line.try_into()
                    .map_err(|_| malformed(address, "code object"))?,
            );
            Object::Code {
                name: self.name_text(name, address, "code object")?,
                file: self.name_text(file, address, "code object")?,
                line: if line == 0 { -1 } else { line },
            }
        } else {
            return Ok(None);
        };

        Ok(Some(object))
    }

    /// The text of the `str` at `name`, which names the object at `owner`,
    /// a `what`.
    fn name_text(
        &self,
        name: VirtualAddress,
        owner: VirtualAddress,
        what: &str,
    ) -> Result<Arc<str>, Problem> {
        self.str_text(name).ok_or_else(|| malformed(owner, what))
    }

    /// The built-in function or method the head is of: a function where it
    /// is bound to nothing or to a module, as CPython names it.
    fn builtin(&self, head: &Head<'_, '_>) -> Result<Object, Problem> {
        let reader = &self.reader;
        let method = VirtualAddress::new(head.field_word(Field::BuiltinMethod)?);
        let name = reader.c_string(VirtualAddress::new(
            reader.field_word(method, Field::MethodDefName)?,
        ))?;

        let bound = VirtualAddress::new(head.field_word(Field::BuiltinSelf)?);
        if bound == VirtualAddress::new(0) {
            return Ok(Object::Builtin { name, bound: None });
        }

        let ty = VirtualAddress::new(reader.field_word(bound, Field::ObjectType)?);
        if self.derives(ty, self.types.module)? {
            return Ok(Object::Builtin { name, bound: None });
        }

        let ty_name =
            reader.c_string(VirtualAddress::new(reader.field_word(ty, Field::TypeName)?))?;
        Ok(Object::Builtin {
            name,
            bound: Some((ty_name, bound)),
        })
    }

    /// The type of the built-in value `object` is, which an instance's
    /// class derives from; `None` for an instance or a class.
    pub(super) fn built_in_type(&self, object: &Object) -> Option<VirtualAddress> {
        let types = &self.types;
        Some(match object {
            Object::Nothing => types.none,
            Object::Bool(_) => types.bool,
            Object::Int { .. } | Object::LargeInt { .. } => types.int,
            Object::Float(_) => types.float,
            Object::Str { .. } => types.str,
            Object::Bytes { .. } => types.bytes,
            Object::List { .. } => types.list,
            Object::Tuple { .. } => types.tuple,
            Object::Dict(_) => types.dict,
            Object::Set { frozen: false, .. } => types.set,
            Object::Set { frozen: true, .. } => types.frozenset,
            Object::Module { .. } => types.module,
            Object::Function { .. } => types.function,
            Object::Builtin { .. } => types.builtin,
            Object::Code { .. } => types.code,
            Object::Library { class, .. } => class.address,
            Object::Class { .. } | Object::Instance { .. } => return None,
        })
    }

    /// The sign and magnitude digits of the `int` the head is of.
    fn int(&self, head: &Head<'_, '_>) -> Result<(bool, Vec<u32>), Problem> {
        let (address, reader) = (head.address(), &self.reader);
        let tag = head.field_word(Field::LongTag)?;
        let count = tag >> reader.layout.constant(Constant::LongNonSizeBits);
        let (negative, zero) = match tag & 3 {
            0 => (false, false),
            1 => (false, true),
            2 => (true, false),
            _ => return Err(malformed(address, "int")),
        };

        if zero != (count == 0) || count > MAX_INT_DIGITS {
            return Err(malformed(address, "int"));
        }

        let shift = self.shift();
        let width = if shift > 16 { 4 } else { 2 };
        let bytes = head.field_bytes(Field::LongDigits, count * width)?;

        let digits: Vec<u32> = if width == 4 {
            bytes
                .as_chunks::<4>()
                .0
                .iter()
                .map(|digit| u32::from_le_bytes(*digit))
                .collect()
        } else {
            bytes
                .as_chunks::<2>()
                .0
                .iter()
                .map(|digit| u32::from(u16::from_le_bytes(*digit)))
                .collect()
        };

        if digits.iter().any(|digit| digit >> shift != 0) {
            return Err(malformed(address, "int"));
        }

        Ok((negative, digits))
    }

    pub(super) fn shift(&self) -> u32 {
        u32::from(self.reader.layout.constant(Constant::LongShift))
    }

    /// The `dict` the head is of, as far as reading its entries needs it.
    pub(super) fn dict(&self, head: &Head<'_, '_>) -> Result<Object, Problem> {
        let address = head.address();
        let length = length(head.field_word(Field::DictUsed)?, address)?;
        let values = head.field_word(Field::DictValues)?;

        let split = if values == 0 {
            None
        } else {
            let (split, size) = self.split(VirtualAddress::new(values))?;
            if size != length {
                return Err(malformed(address, "dict"));
            }

            Some(split)
        };

        let keys = VirtualAddress::new(head.field_word(Field::DictKeys)?);
        self.table(address, keys, length, split).map(Object::Dict)
    }

    /// The values at `values`, which a `dict` or an instance keeps apart
    /// from its keys, and how many of them it holds.
    pub(super) fn split(&self, values: VirtualAddress) -> Result<(Split, u64), Problem> {
        let reader = &self.reader;
        let byte = |at: VirtualAddress| -> Result<u64, Problem> {
            Ok(u64::from(
                reader.bytes(at, 1)?.first().copied().unwrap_or(0),
            ))
        };

        let split = if reader.layout.has(Field::DictValuesCapacity) {
            let capacity = byte(values.wrapping_add(reader.offset(Field::DictValuesCapacity)?))?;
            let size = byte(values.wrapping_add(reader.offset(Field::DictValuesSize)?))?;
            let array = values.wrapping_add(reader.offset(Field::DictValuesValues)?);
            Split {
                values: array,
                capacity,
                order: array.wrapping_add(capacity * 8),
                descending: false,
                size,
            }
        } else {
            // 3.12 keeps how many values it holds two bytes before them,
            // and their order in the bytes before that, down from the
            // third; the keys bound which entries it has values of.
            Split {
                values,
                capacity: u64::MAX,
                order: VirtualAddress::new(values.get().wrapping_sub(3)),
                descending: true,
                size: byte(VirtualAddress::new(values.get().wrapping_sub(2)))?,
            }
        };

        if split.size > split.capacity {
            return Err(malformed(values, "dict values"));
        }

        Ok((split, split.size))
    }

    /// The table of the keys at `keys`, which `owner` holds `length` items
    /// of, whose values `split` keeps where the keys share them.
    pub(super) fn table(
        &self,
        owner: VirtualAddress,
        keys: VirtualAddress,
        length: u64,
        split: Option<Split>,
    ) -> Result<Dict, Problem> {
        let (address, reader) = (owner, &self.reader);
        let keys_head = reader.head(keys)?;
        let byte = |field| keys_head.field_byte(field).map(u64::from);
        let log2_size = byte(Field::DictKeysLog2Size)?;
        let log2_index_bytes = byte(Field::DictKeysLog2IndexBytes)?;
        let kind = byte(Field::DictKeysKind)?;
        let count = keys_head.field_word(Field::DictKeysNentries)?;
        let bad = || malformed(address, "dict");
        if log2_size >= 32 || log2_index_bytes < log2_size || log2_index_bytes > log2_size + 3 {
            return Err(bad());
        }

        if count.cast_signed() < 0 || count > 1 << log2_size || length > count {
            return Err(bad());
        }

        let layout = reader.layout;
        let (size, key, value, hash) =
            if kind == u64::from(layout.constant(Constant::DictKeysGeneral)) {
                (
                    reader.offset(Field::DictEntrySize)?,
                    reader.offset(Field::DictEntryKey)?,
                    reader.offset(Field::DictEntryValue)?,
                    layout.field(Field::DictEntryHash),
                )
            } else if kind == u64::from(layout.constant(Constant::DictKeysUnicode))
                || kind == u64::from(layout.constant(Constant::DictKeysSplit))
            {
                (
                    reader.offset(Field::DictUnicodeEntrySize)?,
                    reader.offset(Field::DictUnicodeEntryKey)?,
                    reader.offset(Field::DictUnicodeEntryValue)?,
                    None,
                )
            } else {
                return Err(bad());
            };

        // Split keys belong to values kept apart, and only to them.
        if split.is_some() != (kind == u64::from(layout.constant(Constant::DictKeysSplit))) {
            return Err(bad());
        }

        let indices = keys.wrapping_add(reader.offset(Field::DictKeysIndices)?);
        Ok(Dict {
            object: address,
            length,
            indices,
            log2_size,
            index_width: 1 << (log2_index_bytes - log2_size),
            entries: indices.wrapping_add(1 << log2_index_bytes),
            count,
            size,
            key,
            value,
            hash,
            split,
        })
    }

    /// The entry at `index` of the `dict`'s table, which may be deleted:
    /// a deleted entry holds no value.
    pub(super) fn entry(&self, dict: Dict, index: u64) -> Result<Entry, Problem> {
        if index >= dict.count {
            return Err(malformed(dict.object, "dict"));
        }

        let bytes = self
            .reader
            .bytes(dict.entries.wrapping_add(index * dict.size), dict.size)?;
        let word = |offset| {
            super::objects::read_le(&bytes, offset, 8).ok_or_else(|| malformed(dict.object, "dict"))
        };

        let value = match dict.split {
            Some(split) if index < split.capacity => {
                self.reader.word(split.values.wrapping_add(index * 8))?
            }
            Some(_) => 0,
            None => word(dict.value)?,
        };

        Ok(Entry {
            key: VirtualAddress::new(word(dict.key)?),
            value: VirtualAddress::new(value),
            hash: match dict.hash {
                Some(offset) => Some(word(offset)?.cast_signed()),
                None => None,
            },
        })
    }

    /// Calls `each` with each of the `dict`'s items in `range`, in
    /// insertion order, until it fails. Deleted entries keep their place in
    /// the table with no value, and are passed over.
    pub(super) fn entries(
        &self,
        dict: Dict,
        range: std::ops::Range<u64>,
        mut each: impl FnMut(Entry) -> Result<(), Problem>,
    ) -> Result<(), Problem> {
        if dict.count > MAX_DICT_ENTRIES {
            return Err(format!(
                "the dict at {:#x} has a table of {} entries, more than {MAX_DICT_ENTRIES} are read",
                dict.object, dict.count
            )
            .into());
        }

        if let Some(split) = dict.split {
            return self.split_entries(dict, split, range, each);
        }

        let pointer = |entry: &[u8], offset: u64| {
            super::objects::read_le(entry, offset, 8).map(VirtualAddress::new)
        };
        let mut item = 0;
        let mut first = 0;

        while first < dict.count && item < range.end {
            let take = DICT_ENTRIES_READ.min(dict.count - first);
            let bytes = self.reader.bytes(
                dict.entries.wrapping_add(first * dict.size),
                take * dict.size,
            )?;

            for entry in bytes.chunks_exact(usize::try_from(dict.size).unwrap_or(usize::MAX)) {
                let (Some(key), Some(value)) =
                    (pointer(entry, dict.key), pointer(entry, dict.value))
                else {
                    return Err(malformed(dict.object, "dict"));
                };

                if value == VirtualAddress::new(0) {
                    continue;
                }

                if key == VirtualAddress::new(0) {
                    return Err(malformed(dict.object, "dict"));
                }

                if range.contains(&item) {
                    let hash = match dict.hash {
                        Some(offset) => Some(
                            super::objects::read_le(entry, offset, 8)
                                .ok_or_else(|| malformed(dict.object, "dict"))?
                                .cast_signed(),
                        ),
                        None => None,
                    };
                    each(Entry { key, value, hash })?;
                }

                item += 1;
            }

            first += take;
        }

        // Every entry read: the table must hold the items the dict counts.
        if first == dict.count && item != dict.length {
            return Err(malformed(dict.object, "dict"));
        }

        Ok(())
    }

    /// [`Self::entries`] of a table whose values `split` keeps, in the
    /// order it keeps.
    fn split_entries(
        &self,
        dict: Dict,
        split: Split,
        range: std::ops::Range<u64>,
        mut each: impl FnMut(Entry) -> Result<(), Problem>,
    ) -> Result<(), Problem> {
        let order = if split.descending {
            let mut order = self.reader.bytes(
                VirtualAddress::new(split.order.get().wrapping_sub(split.size.saturating_sub(1))),
                split.size,
            )?;
            order.reverse();
            order
        } else {
            self.reader.bytes(split.order, split.size)?
        };

        if order.len() as u64 != dict.length {
            return Err(malformed(dict.object, "dict"));
        }

        let end = range.end.min(dict.length);

        for position in range.start.min(end)..end {
            let index = u64::from(order[usize::try_from(position).unwrap_or(usize::MAX)]);
            let entry = self.entry(dict, index)?;
            if entry.key == VirtualAddress::new(0) || entry.value == VirtualAddress::new(0) {
                return Err(malformed(dict.object, "dict"));
            }

            each(entry)?;
        }

        Ok(())
    }

    /// Calls `each` with each item in `range` of the `set` at `object`,
    /// whose table at `table` has `mask + 1` slots, `used` of them holding
    /// items, in the table's order, until it fails. An empty slot holds no
    /// key, and a deleted one the hash -1, which no key has.
    pub(super) fn set_items(
        &self,
        object: VirtualAddress,
        table: VirtualAddress,
        mask: u64,
        used: u64,
        range: std::ops::Range<u64>,
        mut each: impl FnMut(VirtualAddress) -> Result<(), Problem>,
    ) -> Result<(), Problem> {
        let reader = &self.reader;
        let slots = mask + 1;
        if slots > MAX_SET_SLOTS {
            return Err(format!(
                "the set at {object:#x} has a table of {slots} slots, more than the \
                 {MAX_SET_SLOTS} read"
            )
            .into());
        }

        let size = reader.offset(Field::SetEntrySize)?;
        let (key, hash) = (
            reader.offset(Field::SetEntryKey)?,
            reader.offset(Field::SetEntryHash)?,
        );
        let mut item = 0;
        let mut first = 0;

        while first < slots && item < range.end {
            let take = DICT_ENTRIES_READ.min(slots - first);
            let bytes = reader.bytes(table.wrapping_add(first * size), take * size)?;

            for entry in bytes.chunks_exact(usize::try_from(size).unwrap_or(usize::MAX)) {
                let (Some(entry_key), Some(entry_hash)) = (
                    super::objects::read_le(entry, key, 8),
                    super::objects::read_le(entry, hash, 8),
                ) else {
                    return Err(malformed(object, "set"));
                };

                if entry_key == 0 || entry_hash.cast_signed() == -1 {
                    continue;
                }

                if range.contains(&item) {
                    each(VirtualAddress::new(entry_key))?;
                }

                item += 1;
            }

            first += take;
        }

        if first == slots && item != used {
            return Err(malformed(object, "set"));
        }

        Ok(())
    }

    /// The `end - start` item pointers from `start` of the array at
    /// `items`.
    pub(super) fn items(
        &self,
        items: VirtualAddress,
        start: u64,
        end: u64,
    ) -> Result<Vec<VirtualAddress>, Problem> {
        let bytes = self
            .reader
            .bytes(items.wrapping_add(start * 8), (end - start) * 8)?;
        Ok(bytes
            .as_chunks::<8>()
            .0
            .iter()
            .map(|item| VirtualAddress::new(u64::from_le_bytes(*item)))
            .collect())
    }

    /// The module and name of the object's type.
    pub(super) fn type_name(object: &Object) -> (Option<Arc<str>>, Arc<str>) {
        let builtin = |name: &str| (Some(Arc::from("builtins")), Arc::from(name));
        match object {
            Object::Nothing => builtin("NoneType"),
            Object::Bool(_) => builtin("bool"),
            Object::Int { .. } | Object::LargeInt { .. } => builtin("int"),
            Object::Float(_) => builtin("float"),
            Object::Str { .. } => builtin("str"),
            Object::Bytes { .. } => builtin("bytes"),
            Object::List { .. } => builtin("list"),
            Object::Tuple { .. } => builtin("tuple"),
            Object::Dict(_) => builtin("dict"),
            Object::Set { frozen: false, .. } => builtin("set"),
            Object::Set { frozen: true, .. } => builtin("frozenset"),
            Object::Module { .. } => builtin("module"),
            Object::Function { .. } => builtin("function"),
            Object::Builtin { .. } => builtin("builtin_function_or_method"),
            Object::Code { .. } => builtin("code"),
            Object::Class { meta: class, .. }
            | Object::Library { class, .. }
            | Object::Instance { class, .. } => (class.module.clone(), Arc::clone(&class.name)),
        }
    }

    /// The object at `address` as a summary shows it.
    pub(super) fn summary(&self, address: VirtualAddress) -> Arc<str> {
        let mut summary = Summary {
            objects: 1,
            within: vec![address],
            problem: None,
        };

        self.object(address).map_or_else(
            |_| format!("<unreadable object at {address:#x}>").into(),
            |object| self.text(address, &object, 0, &mut summary).into(),
        )
    }

    /// The object's text, where it is `depth` containers deep in a
    /// summary.
    fn text(
        &self,
        address: VirtualAddress,
        object: &Object,
        depth: usize,
        summary: &mut Summary,
    ) -> String {
        match object {
            Object::Nothing => "None".to_owned(),
            Object::Bool(true) => "True".to_owned(),
            Object::Bool(false) => "False".to_owned(),
            Object::Int { negative, digits } => text::int(*negative, digits, self.shift()),
            Object::LargeInt { digits } => {
                // CPython's digits, each of `shift` bits, not decimal ones.
                let shift = self.shift();
                note(
                    summary,
                    format!(
                        "an int of more than {MAX_INT_DIGITS} {shift}-bit digits is shown by its \
                         size"
                    ),
                );
                format!("<int of {digits} {shift}-bit digits>")
            }
            Object::Float(value) => text::float(*value),
            Object::Str { points, length } => {
                let mut text = text::string(points);
                if *length > points.len() as u64 {
                    note(
                        summary,
                        format!("a str shows {} of its {length} characters", points.len()),
                    );
                    text.push_str("...");
                }

                text
            }
            Object::Bytes { contents, length } => {
                let mut text = text::bytes(contents);
                if *length > contents.len() as u64 {
                    note(
                        summary,
                        format!("a bytes shows {} of its {length} bytes", contents.len()),
                    );
                    text.push_str("...");
                }

                text
            }
            Object::List { items, length } => {
                self.sequence(*items, *length, ("[", "]"), depth, summary)
            }
            Object::Tuple { items, length } => {
                self.sequence(*items, *length, ("(", ")"), depth, summary)
            }
            Object::Dict(dict) => self.dict_text(*dict, depth, summary),
            Object::Set {
                frozen,
                table,
                mask,
                used,
            } => self.set_text(address, (*table, *mask, *used), *frozen, depth, summary),
            Object::Class { this, .. } => format!("<class '{}'>", this.qualified()),
            Object::Module { dict } => self.module_text(address, *dict, summary),
            Object::Function { name } => format!("<function {name} at {address:#x}>"),
            Object::Builtin { name, bound: None } => format!("<built-in function {name}>"),
            Object::Builtin {
                name,
                bound: Some((ty, bound)),
            } => format!("<built-in method {name} of {ty} object at {bound:#x}>"),
            Object::Code { name, file, line } => {
                format!("<code object {name} at {address:#x}, file \"{file}\", line {line}>")
            }
            Object::Library { name, value, .. } => self.library_text(name, value, depth, summary),
            Object::Instance {
                base: Some(base), ..
            } => self.text(address, base, depth, summary),
            Object::Instance { class, base: None } => {
                format!("<{} object at {address:#x}>", class.qualified())
            }
        }
    }

    /// The text of the object at `address` as an item of a container in
    /// a summary, where it is `depth` containers deep.
    pub(super) fn item_text(
        &self,
        address: VirtualAddress,
        depth: usize,
        summary: &mut Summary,
    ) -> String {
        if summary.objects >= SUMMARY_OBJECTS {
            note(
                summary,
                format!("a summary shows {SUMMARY_OBJECTS} objects"),
            );
            return "...".to_owned();
        }

        summary.objects += 1;
        let object = match self.object(address) {
            Ok(object) => object,
            Err(problem) => {
                note(summary, problem.to_string());
                return format!("<unreadable object at {address:#x}>");
            }
        };

        // An object of a class deriving from a container is that
        // container, as its type's `repr` shows it.
        let held = match &object {
            Object::Instance {
                base: Some(base), ..
            } => base,
            object => object,
        };
        let container = match held {
            Object::List { .. } | Object::Tuple { .. } | Object::Dict(_) | Object::Set { .. } => {
                true
            }
            Object::Library { value, .. } => value.contains(),
            _ => false,
        };
        if !container {
            return self.text(address, &object, depth, summary);
        }

        // A container within itself, as Python's repr shows it.
        if summary.within.contains(&address) {
            return match held {
                Object::List { .. } => "[...]".to_owned(),
                Object::Dict(_) => "{...}".to_owned(),
                Object::Library { name, value, .. } => {
                    self.recurring_library_text(name, value, depth, summary)
                }
                _ => "...".to_owned(),
            };
        }

        if depth >= SUMMARY_DEPTH {
            note(
                summary,
                format!("a summary nests {SUMMARY_DEPTH} containers deep"),
            );
            return "...".to_owned();
        }

        summary.within.push(address);
        let text = self.text(address, &object, depth, summary);
        summary.within.pop();
        text
    }

    fn sequence(
        &self,
        items: VirtualAddress,
        length: u64,
        brackets: (&str, &str),
        depth: usize,
        summary: &mut Summary,
    ) -> String {
        let items = self.items(items, 0, length.min(SUMMARY_ITEMS));
        self.listed(items, length, brackets, depth, summary)
    }

    /// The text of a sequence of `length` items, of which `items` are the
    /// first read, between `open` and `close`.
    pub(super) fn listed(
        &self,
        items: Result<Vec<VirtualAddress>, Problem>,
        length: u64,
        (open, close): (&str, &str),
        depth: usize,
        summary: &mut Summary,
    ) -> String {
        let shown = length.min(SUMMARY_ITEMS);
        let (mut texts, whole) = match items {
            Ok(items) => self.until_spent(items.len(), summary, |index, summary| {
                self.item_text(items[index], depth + 1, summary)
            }),
            Err(problem) => {
                note(summary, problem.to_string());
                (Vec::new(), false)
            }
        };

        if !whole {
            texts.push("...".to_owned());
        } else if shown < length {
            note(
                summary,
                format!("a summary shows {SUMMARY_ITEMS} items of each container"),
            );
            texts.push("...".to_owned());
        }

        let mut text = format!("{open}{}", texts.join(", "));
        if whole && open == "(" && length == 1 {
            text.push(',');
        }

        text.push_str(close);
        text
    }

    pub(super) fn dict_text(&self, dict: Dict, depth: usize, summary: &mut Summary) -> String {
        let mut pairs = Vec::new();
        let read = self.entries(dict, 0..SUMMARY_ITEMS, |entry| {
            pairs.push((entry.key, entry.value));
            Ok(())
        });

        self.pairs_text((&pairs, read), dict.length, depth, summary)
    }

    /// The text of a mapping of `length` items, of which `pairs` are the
    /// keys and values of the first read, and `read` why no more were, if
    /// it failed.
    pub(super) fn pairs_text(
        &self,
        (pairs, read): (&[(VirtualAddress, VirtualAddress)], Result<(), Problem>),
        length: u64,
        depth: usize,
        summary: &mut Summary,
    ) -> String {
        let (mut texts, whole) = self.until_spent(pairs.len(), summary, |index, summary| {
            let (key, value) = pairs[index];
            let key = self.item_text(key, depth + 1, summary);
            let value = self.item_text(value, depth + 1, summary);
            format!("{key}: {value}")
        });
        if let Err(problem) = read {
            note(summary, problem.to_string());
            texts.push("...".to_owned());
        } else if !whole {
            texts.push("...".to_owned());
        } else if length > SUMMARY_ITEMS {
            note(
                summary,
                format!("a summary shows {SUMMARY_ITEMS} items of each container"),
            );
            texts.push("...".to_owned());
        }

        format!("{{{}}}", texts.join(", "))
    }

    /// A `set`'s text, as Python's `repr` gives it, of its items in its
    /// table's order.
    fn set_text(
        &self,
        address: VirtualAddress,
        (table, mask, used): (VirtualAddress, u64, u64),
        frozen: bool,
        depth: usize,
        summary: &mut Summary,
    ) -> String {
        let (open, close) = if frozen {
            ("frozenset({", "})")
        } else {
            ("{", "}")
        };
        if used == 0 {
            return if frozen { "frozenset()" } else { "set()" }.to_owned();
        }

        let mut items = Vec::new();
        let read = self.set_items(address, table, mask, used, 0..SUMMARY_ITEMS, |item| {
            items.push(item);
            Ok(())
        });

        let (mut texts, whole) = self.until_spent(items.len(), summary, |index, summary| {
            self.item_text(items[index], depth + 1, summary)
        });
        if let Err(problem) = read {
            note(summary, problem.to_string());
            texts.push("...".to_owned());
        } else if !whole {
            texts.push("...".to_owned());
        } else if used > SUMMARY_ITEMS {
            note(
                summary,
                format!("a summary shows {SUMMARY_ITEMS} items of each container"),
            );
            texts.push("...".to_owned());
        }

        format!("{open}{}{close}", texts.join(", "))
    }

    /// A module's text: its name, which its dict holds, or why it has none.
    fn module_text(
        &self,
        address: VirtualAddress,
        dict: VirtualAddress,
        summary: &mut Summary,
    ) -> String {
        let name = match self.object(dict) {
            Ok(Object::Dict(dict)) => match self.lookup(
                dict,
                &crate::runtime_model::InterpretedKey::Text(b"__name__".to_vec()),
            ) {
                Ok(name) => name.and_then(|name| self.str_text(name)),
                Err(failure) => {
                    note(
                        summary,
                        format!(
                            "a module's name cannot be read: {}",
                            super::instances::problem(failure)
                        ),
                    );
                    None
                }
            },
            Ok(_) => {
                note(summary, format!("the module at {address:#x} holds no dict"));
                None
            }
            Err(problem) => {
                note(
                    summary,
                    format!("a module's dict cannot be read: {problem}"),
                );
                None
            }
        };

        name.map_or_else(
            || format!("<module object at {address:#x}>"),
            |name| {
                format!(
                    "<module {}>",
                    text::string(&name.chars().map(u32::from).collect::<Vec<_>>())
                )
            },
        )
    }

    /// The texts of the first of `count` items, each from `text`, until
    /// the budget is spent, and whether it lasted for every item. An item
    /// whose reads spent it is left out, since its text is not whole.
    fn until_spent(
        &self,
        count: usize,
        summary: &mut Summary,
        mut text: impl FnMut(usize, &mut Summary) -> String,
    ) -> (Vec<String>, bool) {
        let mut texts = Vec::with_capacity(count);
        for index in 0..count {
            let item = text(index, summary);
            if self.budget.exhaustion().is_some() {
                return (texts, false);
            }

            texts.push(item);
        }

        (texts, true)
    }
}

/// Notes why a summary shows less than all, keeping the first reason.
pub(super) fn note(summary: &mut Summary, problem: String) {
    if summary.problem.is_none() {
        summary.problem = Some(problem.into());
    }
}

/// A container's length, which CPython keeps as a signed size.
fn length(size: u64, address: VirtualAddress) -> Result<u64, Problem> {
    if size.cast_signed() < 0 {
        return Err(malformed(address, "object"));
    }

    Ok(size)
}

/// An int's value, where it fits in 128 bits.
pub(super) fn exact(negative: bool, digits: &[u32], shift: u32) -> Option<i128> {
    let mut magnitude: u128 = 0;

    for &digit in digits.iter().rev() {
        magnitude = magnitude
            .checked_mul(1 << shift)?
            .checked_add(u128::from(digit))?;
    }

    if negative {
        0_i128.checked_sub_unsigned(magnitude)
    } else {
        i128::try_from(magnitude).ok()
    }
}

/// The object a variable's word `bits` refers to, as `layout` lays a
/// variable out, none for an unbound one.
pub(super) fn reference(
    layout: &PythonLayout,
    bits: u64,
) -> Result<Option<VirtualAddress>, Problem> {
    if layout.constant(Constant::StackRefs) == 0 {
        return Ok(Some(bits)
            .filter(|bits| *bits != 0)
            .map(VirtualAddress::new));
    }

    // A stack reference: a pointer whose low bit counts references, or a
    // tagged integer, which only the interpreter's own values are.
    match bits & 3 {
        0 | 1 if bits & !1 == 0 => Ok(None),
        0 | 1 => Ok(Some(VirtualAddress::new(bits & !1))),
        3 => Err("the variable holds a tagged integer, which only the interpreter uses".into()),
        _ => Err(format!("the variable holds {bits:#x}, which is no reference").into()),
    }
}
