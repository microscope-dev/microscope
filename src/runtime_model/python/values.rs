//! The values of Python's built-in types, as the debugger presents them:
//! `None`, `bool`, `int` at any size, `float`, `str`, `bytes`, `list`,
//! `tuple`, and `dict`, each by its exact type. An object of any other
//! type, a subclass of one of those included, is shown by its type's name
//! and its address.
//!
//! A value's summary is its text in Python's notation ([`super::text`]),
//! nesting containers to [`SUMMARY_DEPTH`] and showing [`SUMMARY_ITEMS`]
//! of each; what lies past either shows as `...`, and a container within
//! itself as Python's `repr` shows it. Every read is charged to the
//! inspection's budget, and an object that is not what its type says ends
//! its summary with a problem, never as an empty container.

use std::cell::RefCell;
use std::sync::Arc;

use super::code::{CodeId, CodeTable};
use super::objects::{Charge, Head, Problem, Reader, Record, malformed};
use super::text;
use crate::VirtualAddress;
use crate::image::python::{Constant, Field};
use crate::inspection::InspectionBudget;
use crate::runtime_model::{
    InterpretedChild, InterpretedKind, InterpretedLocal, InterpretedSlot, InterpretedValue,
};
use crate::{InspectionExhaustion, VariableUnavailableReason};

/// How deep a summary nests containers, and how many items of each it
/// shows.
const SUMMARY_DEPTH: usize = 6;
const SUMMARY_ITEMS: u64 = 64;

/// The most objects one summary reads.
const SUMMARY_OBJECTS: usize = 512;

/// The most characters of a `str`, or bytes of a `bytes`, a summary shows.
const SUMMARY_TEXT: u64 = 1024;

/// The most digits of an `int` read: one with more is shown by its size.
const MAX_INT_DIGITS: u64 = 1 << 10;

/// The most entries of a `dict`'s table read to find its items.
const MAX_DICT_ENTRIES: u64 = 1 << 20;

/// How many of a `dict`'s entries one read takes.
const DICT_ENTRIES_READ: u64 = 64;

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
enum Object {
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

    /// A `dict` whose values its instances' objects keep apart from its
    /// keys, which is not read.
    SplitDict {
        length: u64,
    },
    Other {
        name: Arc<str>,
    },
}

/// A combined `dict`'s table of entries, in insertion order.
#[derive(Clone, Copy)]
struct Dict {
    object: VirtualAddress,
    length: u64,
    entries: VirtualAddress,
    count: u64,

    /// Each entry's size, and where its key and value are within it.
    size: u64,
    key: u64,
    value: u64,
}

/// Where a summary is: how many objects it has read, the containers it is
/// within, and why it shows less than all, if it does.
#[derive(Default)]
struct Summary {
    objects: usize,
    within: Vec<VirtualAddress>,
    problem: Option<Problem>,
}

/// The values of one stop's objects, and the budget that pays for them.
pub(super) struct Values<'a, 'b> {
    pub(super) reader: Reader<'a>,
    pub(super) types: Types,
    pub(super) budget: &'a Budget<'b>,
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

            let slot = if local.kind & shared != 0 {
                InterpretedSlot::Cell
            } else {
                self.local(u64::from_le_bytes(*slot))
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
        if self.reader.layout.constant(Constant::StackRefs) == 0 {
            return match bits {
                0 => InterpretedSlot::Unbound,
                object => self.value(VirtualAddress::new(object)),
            };
        }

        // A stack reference: a pointer whose low bit counts references,
        // or a tagged integer, which only the interpreter's own values are.
        match bits & 3 {
            0 | 1 if bits & !1 == 0 => InterpretedSlot::Unbound,
            0 | 1 => self.value(VirtualAddress::new(bits & !1)),
            3 => InterpretedSlot::Unreadable(
                "the variable holds a tagged integer, which only the interpreter uses".into(),
            ),
            _ => InterpretedSlot::Unreadable(
                format!("the variable holds {bits:#x}, which is no reference").into(),
            ),
        }
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
    fn failed(&self, problem: Problem) -> InterpretedSlot {
        self.budget.exhaustion().map_or_else(
            || InterpretedSlot::Unreadable(problem),
            InterpretedSlot::Limit,
        )
    }

    fn presented(&self, address: VirtualAddress) -> Result<InterpretedValue, Problem> {
        let object = self.object(address)?;
        let (module, type_name) = Self::type_name(&object);
        let kind = match &object {
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
            Object::SplitDict { .. } | Object::Other { .. } => InterpretedKind::Other,
        };

        let mut summary = Summary {
            objects: 1,
            within: vec![address],
            problem: None,
        };
        let text = self.text(address, &object, 0, &mut summary);
        Ok(InterpretedValue {
            object: address,
            module,
            type_name,
            kind,
            summary: text.into(),
            problem: summary.problem,
        })
    }

    /// The children `range` of the container at `object`: a sequence's
    /// elements, or a `dict`'s entries in insertion order.
    pub(super) fn children(
        &self,
        object: VirtualAddress,
        range: std::ops::Range<u64>,
    ) -> Result<Vec<InterpretedChild>, Problem> {
        match self.object(object)? {
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
                self.entries(dict, range, |key, value| {
                    children.push(InterpretedChild::Entry {
                        key: self.value(key),
                        value: self.value(value),
                    });
                })?;
                Ok(children)
            }
            Object::SplitDict { .. } => Err(split_dict()),
            _ => Err(format!("the object at {object:#x} holds no items").into()),
        }
    }

    /// Reads as much of the object at `address` as presenting it needs.
    fn object(&self, address: VirtualAddress) -> Result<Object, Problem> {
        if address == VirtualAddress::new(0) {
            return Err("the reference is NULL".into());
        }

        let reader = &self.reader;
        let head = reader.head(address)?;
        let ty = VirtualAddress::new(head.field_word(Field::ObjectType)?);
        let types = &self.types;

        if ty == types.none {
            Ok(Object::Nothing)
        } else if ty == types.bool {
            let (negative, digits) = self.int(&head)?;
            Ok(Object::Bool(
                !negative && digits.iter().any(|digit| *digit != 0),
            ))
        } else if ty == types.int {
            let tag = head.field_word(Field::LongTag)?;
            let count = tag >> reader.layout.constant(Constant::LongNonSizeBits);
            if count > MAX_INT_DIGITS {
                return Ok(Object::LargeInt { digits: count });
            }

            let (negative, digits) = self.int(&head)?;
            Ok(Object::Int { negative, digits })
        } else if ty == types.float {
            let bits = head.field_word(Field::FloatValue)?;
            Ok(Object::Float(f64::from_bits(bits)))
        } else if ty == types.str {
            let (points, length) = head.code_points(SUMMARY_TEXT)?;
            Ok(Object::Str { points, length })
        } else if ty == types.bytes {
            let length = length(head.field_word(Field::BytesSize)?, address)?;
            let contents = head.field_bytes(Field::BytesData, length.min(SUMMARY_TEXT))?;
            Ok(Object::Bytes { contents, length })
        } else if ty == types.list {
            let length = length(head.field_word(Field::ListSize)?, address)?;
            let items = VirtualAddress::new(head.field_word(Field::ListItems)?);
            if length > 0 && items == VirtualAddress::new(0) {
                return Err(malformed(address, "list"));
            }

            Ok(Object::List { items, length })
        } else if ty == types.tuple {
            let length = length(head.field_word(Field::TupleSize)?, address)?;
            let items = address.wrapping_add(reader.offset(Field::TupleItems)?);
            Ok(Object::Tuple { items, length })
        } else if ty == types.dict {
            self.dict(&head)
        } else {
            let name = VirtualAddress::new(reader.field_word(ty, Field::TypeName)?);
            Ok(Object::Other {
                name: reader.c_string(name)?,
            })
        }
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

    fn shift(&self) -> u32 {
        u32::from(self.reader.layout.constant(Constant::LongShift))
    }

    /// The `dict` the head is of, as far as reading its entries needs it.
    fn dict(&self, head: &Head<'_, '_>) -> Result<Object, Problem> {
        let (address, reader) = (head.address(), &self.reader);
        let length = length(head.field_word(Field::DictUsed)?, address)?;
        if head.field_word(Field::DictValues)? != 0 {
            return Ok(Object::SplitDict { length });
        }

        let keys = VirtualAddress::new(head.field_word(Field::DictKeys)?);
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
        let (size, key, value) = if kind == u64::from(layout.constant(Constant::DictKeysGeneral)) {
            (
                reader.offset(Field::DictEntrySize)?,
                reader.offset(Field::DictEntryKey)?,
                reader.offset(Field::DictEntryValue)?,
            )
        } else if kind == u64::from(layout.constant(Constant::DictKeysUnicode)) {
            (
                reader.offset(Field::DictUnicodeEntrySize)?,
                reader.offset(Field::DictUnicodeEntryKey)?,
                reader.offset(Field::DictUnicodeEntryValue)?,
            )
        } else {
            // Split keys belong to a dict that keeps its values apart.
            return Err(bad());
        };

        let indices = keys.wrapping_add(reader.offset(Field::DictKeysIndices)?);
        Ok(Object::Dict(Dict {
            object: address,
            length,
            entries: indices.wrapping_add(1 << log2_index_bytes),
            count,
            size,
            key,
            value,
        }))
    }

    /// Calls `each` with the key and value of each of the `dict`'s items
    /// in `range`, in insertion order. Deleted entries keep their place
    /// in the table with no value, and are passed over.
    fn entries(
        &self,
        dict: Dict,
        range: std::ops::Range<u64>,
        mut each: impl FnMut(VirtualAddress, VirtualAddress),
    ) -> Result<(), Problem> {
        if dict.count > MAX_DICT_ENTRIES {
            return Err(format!(
                "the dict at {:#x} has a table of {} entries, more than {MAX_DICT_ENTRIES} are read",
                dict.object, dict.count
            )
            .into());
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
                    each(key, value);
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

    /// The `end - start` item pointers from `start` of the array at
    /// `items`.
    fn items(
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
    fn type_name(object: &Object) -> (Option<Arc<str>>, Arc<str>) {
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
            Object::Dict(_) | Object::SplitDict { .. } => builtin("dict"),
            // A static type's name is qualified by its module, where its
            // module is not `builtins`; a class's is not qualified.
            Object::Other { name } => match name.rsplit_once('.') {
                Some((module, name)) => (Some(module.into()), name.into()),
                None => (None, Arc::clone(name)),
            },
        }
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
                note(
                    summary,
                    format!("an int of more than {MAX_INT_DIGITS} digits is shown by its size"),
                );
                format!("<int of {digits} digits>")
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
            Object::SplitDict { length } => {
                note(summary, split_dict().to_string());
                format!("<dict of {length} items>")
            }
            Object::Other { name } => format!("<{name} object at {address:#x}>"),
        }
    }

    /// The text of the object at `address` as an item of a container in
    /// a summary, where it is `depth` containers deep.
    fn item_text(&self, address: VirtualAddress, depth: usize, summary: &mut Summary) -> String {
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

        let container = matches!(
            object,
            Object::List { .. } | Object::Tuple { .. } | Object::Dict(_)
        );
        if !container {
            return self.text(address, &object, depth, summary);
        }

        // A container within itself, as Python's repr shows it.
        if summary.within.contains(&address) {
            return match object {
                Object::List { .. } => "[...]",
                Object::Dict(_) => "{...}",
                _ => "...",
            }
            .to_owned();
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
        (open, close): (&str, &str),
        depth: usize,
        summary: &mut Summary,
    ) -> String {
        let shown = length.min(SUMMARY_ITEMS);
        let (mut texts, whole) = match self.items(items, 0, shown) {
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

    fn dict_text(&self, dict: Dict, depth: usize, summary: &mut Summary) -> String {
        let mut pairs = Vec::new();
        let read = self.entries(dict, 0..SUMMARY_ITEMS, |key, value| {
            pairs.push((key, value));
        });
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
        } else if dict.length > SUMMARY_ITEMS {
            note(
                summary,
                format!("a summary shows {SUMMARY_ITEMS} items of each container"),
            );
            texts.push("...".to_owned());
        }

        format!("{{{}}}", texts.join(", "))
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
fn note(summary: &mut Summary, problem: String) {
    if summary.problem.is_none() {
        summary.problem = Some(problem.into());
    }
}

fn split_dict() -> Problem {
    "the dict keeps its values apart from its keys, as an instance's may, which is not read yet"
        .into()
}

/// A container's length, which CPython keeps as a signed size.
fn length(size: u64, address: VirtualAddress) -> Result<u64, Problem> {
    if size.cast_signed() < 0 {
        return Err(malformed(address, "object"));
    }

    Ok(size)
}

/// An int's value, where it fits in 128 bits.
fn exact(negative: bool, digits: &[u32], shift: u32) -> Option<i128> {
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
