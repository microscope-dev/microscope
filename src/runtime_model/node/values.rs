//! Reads JavaScript values from V8's heap into the graph [`super::inspect`]
//! formats, as deep as inspect shows: objects to level 2 with contents,
//! level 3 by header only.
//!
//! A failed read becomes an unreadable value with its reason, and reading
//! continues. Unsupported instance types are reported, never guessed.

use std::collections::BTreeMap;

use super::inspect::{
    Array, BigInt, Constructor, Element, Function, FunctionKind, JsString, Key, Kind,
    Object as Row, ObjectId, PromiseState, Property, Slot, Symbol, Value, ValueGraph,
};
use super::lookup::{self, Found};
use super::objects::{Object, Problem, Reader};
use super::own::{self, Held};
use crate::VirtualAddress;
use crate::image::v8::{Constant, Field};

/// The deepest level at which inspect shows an object's contents.
const FULL_LEVEL: u32 = 2;

/// Bounds paths that do not increase inspect's level, such as proxy targets
/// and an error's object-valued stack.
const MAX_READ_DEPTH: usize = 64;

/// inspect's `maxStringLength`, in UTF-16 units.
const SHOWN_UNITS: u64 = 10_000;

/// The most units read of a string needed whole, such as a key or name.
const WHOLE_UNITS: u64 = 1 << 20;

/// Reads the values reachable from a root value into a graph.
pub(super) struct Builder<'r> {
    pub(super) reader: Reader<'r>,
    pub(super) graph: ValueGraph,

    /// Each object read, by tagged address, and whether its contents were
    /// read too.
    rows: BTreeMap<u64, (ObjectId, bool)>,
    depth: usize,
}

impl<'r> Builder<'r> {
    pub(super) const fn new(reader: Reader<'r>) -> Self {
        Self {
            reader,
            graph: ValueGraph::new(),
            rows: BTreeMap::new(),
            depth: 0,
        }
    }

    /// The value of the tagged word `word`, at `level` below the root.
    pub(super) fn value(&mut self, word: u64, level: u32) -> Value {
        if self.depth >= MAX_READ_DEPTH {
            return Value::Unreadable("the object or proxy chain is too long".to_owned());
        }
        self.depth += 1;
        let value = self
            .read(word, level)
            .unwrap_or_else(|problem| Value::Unreadable(problem.to_string()));
        self.depth -= 1;
        value
    }

    fn read(&mut self, word: u64, level: u32) -> Result<Value, Problem> {
        let reader = self.reader;
        if let Some(integer) = reader.smi(word)? {
            #[allow(clippy::cast_precision_loss, reason = "a Smi has 32 bits")]
            return Ok(Value::Number(integer as f64));
        }
        let object = reader
            .object(word)?
            .ok_or_else(|| format!("{word:#x} is neither a Smi nor an object"))?;
        let ty = reader.instance_type(object)?;
        if ty < reader.constant(Constant::FirstNonstringType)? {
            return string(&reader, object, SHOWN_UNITS).map(Value::String);
        }
        let is = |constant| reader.constant(constant).ok() == Some(ty);
        if is(Constant::HeapNumberType) {
            return number(&reader, object).map(Value::Number);
        }
        if is(Constant::OddballType) {
            return oddball(&reader, object);
        }
        if is(Constant::BigIntBaseType) {
            return bigint(&reader, object).map(Value::BigInt);
        }
        if is(Constant::SymbolType) {
            return symbol(&reader, object).map(Value::Symbol);
        }
        if is(Constant::HoleType) {
            return Err("the value is V8's hole marker".into());
        }
        if (reader.constant(Constant::FirstJSReceiverType)?
            ..=reader.constant(Constant::LastJSReceiverType)?)
            .contains(&ty)
        {
            return self.object(object, level).map(Value::Object);
        }
        Err(format!(
            "{:#x} has instance type {ty}, which is not a JavaScript value",
            object.0
        )
        .into())
    }

    /// The graph row for `object`, read at `level`.
    fn object(&mut self, object: Object, level: u32) -> Result<ObjectId, Problem> {
        let object = global_object(&self.reader, object)?;
        let full = level <= FULL_LEVEL;
        if let Some(&(row, read)) = self.rows.get(&object.0)
            && (read || !full)
        {
            return Ok(row);
        }
        let tag = self
            .reader
            .constant(Constant::HeapObjectTag)?
            .cast_unsigned();
        let address = VirtualAddress::new(object.0 - tag);
        // Register the row before reading contents so cycles find it.
        let row = self
            .graph
            .push(Row {
                address,
                constructor: Constructor::Named(JsString::complete(Vec::new())),
                tag: None,
                kind: Kind::Ordinary,
                properties: Vec::new(),
            })
            .ok_or("the value refers to too many objects")?;
        self.rows.insert(object.0, (row, full));
        let read = self
            .contents(object, level, full)
            .unwrap_or_else(|problem| Row {
                address,
                constructor: Constructor::Named(JsString::complete(Vec::new())),
                tag: None,
                kind: Kind::Boxed(Value::Unreadable(problem.to_string())),
                properties: Vec::new(),
            });
        if let Some(slot) = self.graph.object_mut(row) {
            *slot = read;
        }
        Ok(row)
    }

    /// The object `object`, at `level`, with its contents where `full`.
    fn contents(&mut self, object: Object, level: u32, full: bool) -> Result<Row, Problem> {
        let reader = self.reader;
        let tag = reader.constant(Constant::HeapObjectTag)?.cast_unsigned();
        let address = VirtualAddress::new(object.0 - tag);
        let ty = reader.instance_type(object)?;
        let is = |constant| reader.constant(constant).ok() == Some(ty);
        let next = level + 1;

        if is(Constant::JSProxyType) {
            let target = reader.field(object, Field::JSProxyTarget)?;
            let target = if reader
                .object(target)?
                .is_some_and(|target| reader.is(target, Constant::OddballType).unwrap_or(false))
            {
                Value::Null
            } else {
                self.value(target, level)
            };
            return Ok(Row {
                address,
                constructor: Constructor::Named(JsString::complete(Vec::new())),
                tag: None,
                kind: Kind::Proxy { target },
                properties: Vec::new(),
            });
        }

        let constructor = self.constructor(object)?;
        let tag_text = self.tag(object, ty)?;
        let mut indexed = false;
        let kind = if is(Constant::JSArrayType) {
            indexed = true;
            Kind::Array(self.array(object, next, full)?)
        } else if (reader.constant(Constant::FirstJSFunctionType)?
            ..=reader.constant(Constant::LastJSFunctionType)?)
            .contains(&ty)
            || is(Constant::JSBoundFunctionType)
        {
            Kind::Function(self.function(object)?)
        } else if is(Constant::JSMapType) || is(Constant::JSSetType) {
            self.collection(object, next, full, is(Constant::JSMapType))?
        } else if is(Constant::JSWeakMapType) {
            Kind::WeakMap
        } else if is(Constant::JSWeakSetType) {
            Kind::WeakSet
        } else if is(Constant::JSDateType) {
            let time = reader.field(object, Field::JSDateValue)?;
            Kind::Date {
                time: if reader.constant(Constant::JSDateValueIsFloat64)? == 1 {
                    f64::from_bits(time)
                } else {
                    self.number_word(time)?
                },
            }
        } else if is(Constant::JSRegExpType) {
            self.regexp(object)?
        } else if is(Constant::JSPromiseType) {
            Kind::Promise(self.promise(object, next)?)
        } else if is(Constant::JSTypedArrayType) {
            indexed = true;
            super::typed::typed_array(&reader, object, full)?
        } else if is(Constant::JSArrayBufferType) {
            super::typed::array_buffer(&reader, object, full)?
        } else if is(Constant::JSDataViewType) || is(Constant::JSRabGsabDataViewType) {
            super::typed::data_view(&reader, object, |word| self.value(word, next))?
        } else if is(Constant::JSPrimitiveWrapperType) {
            let value = reader.field(object, Field::JSPrimitiveWrapperValue)?;
            // A String wrapper's characters are its indexed properties.
            indexed = reader
                .object(value)?
                .map(|value| reader.instance_type(value))
                .transpose()?
                .is_some_and(|value| {
                    reader
                        .constant(Constant::FirstNonstringType)
                        .is_ok_and(|end| value < end)
                });
            Kind::Boxed(self.value(value, next))
        } else if is(Constant::JSErrorType) {
            Kind::Error(super::errors::details(self, object, next)?)
        } else if is(Constant::JSObjectType)
            || is(Constant::JSGlobalObjectType)
            || is(Constant::JSObjectPrototypeType)
            || is(Constant::JSApiObjectType)
            || is(Constant::JSSpecialApiObjectType)
        {
            Kind::Ordinary
        } else {
            return Err(format!("microscope can't read objects of instance type {ty} yet").into());
        };

        let properties = self.properties(object, next, indexed)?;
        Ok(Row {
            address,
            constructor,
            tag: tag_text,
            kind,
            properties,
        })
    }

    /// The own enumerable properties of `object`, values at `level`: elements
    /// by index (skipped if `indexed`, as the kind lists them), then named.
    fn properties(
        &mut self,
        object: Object,
        level: u32,
        indexed: bool,
    ) -> Result<Vec<Property>, Problem> {
        let reader = self.reader;
        let mut properties = Vec::new();
        if !indexed {
            for element in own::elements(&reader, object, 0..u32::MAX)? {
                if !element.enumerable {
                    continue;
                }
                let key: Vec<u16> = element.index.to_string().encode_utf16().collect();
                properties.push(Property {
                    key: Key::String(JsString::complete(key)),
                    slot: self.slot(element.held, level),
                });
            }
        }
        for named in own::named(&reader, object)? {
            if !named.enumerable {
                continue;
            }
            let key = if named.symbol {
                Key::Symbol(symbol(&reader, named.key)?)
            } else {
                Key::String(string(&reader, named.key, WHOLE_UNITS)?)
            };
            properties.push(Property {
                key,
                slot: self.slot(named.held, level),
            });
        }
        Ok(properties)
    }

    /// The slot for a property holding `held`, its value read at `level`.
    pub(super) fn slot(&mut self, held: Held, level: u32) -> Slot {
        if level > FULL_LEVEL + 1 {
            return Slot::Data(Value::Unreadable(
                "a property beyond inspect's depth".to_owned(),
            ));
        }
        let reader = self.reader;
        match held {
            Held::Data(word) => Slot::Data(self.value(word, level)),
            Held::Double { bits, .. } => Slot::Data(Value::Number(f64::from_bits(bits))),
            Held::Accessor(accessor) => match reader.is(accessor, Constant::AccessorPairType) {
                Ok(true) => {
                    let present = |field| -> bool {
                        reader.field(accessor, field).is_ok_and(|word| {
                            // An unset getter or setter holds null or
                            // undefined.
                            reader.object(word).ok().flatten().is_some_and(|part| {
                                !reader.is(part, Constant::OddballType).unwrap_or(true)
                            })
                        })
                    };
                    Slot::Accessor {
                        getter: present(Field::AccessorPairGetter),
                        setter: present(Field::AccessorPairSetter),
                    }
                }
                Ok(false) => Slot::Data(Value::Unreadable(
                    "microscope does not compute V8's native accessors".to_owned(),
                )),
                Err(problem) => Slot::Data(Value::Unreadable(problem.to_string())),
            },
        }
    }

    /// The number in a Smi or a `HeapNumber`.
    fn number_word(&self, word: u64) -> Result<f64, Problem> {
        let reader = self.reader;
        if let Some(integer) = reader.smi(word)? {
            #[allow(clippy::cast_precision_loss, reason = "a Smi has 32 bits")]
            return Ok(integer as f64);
        }
        let object = reader
            .object(word)?
            .filter(|object| {
                reader
                    .is(*object, Constant::HeapNumberType)
                    .unwrap_or(false)
            })
            .ok_or_else(|| format!("{word:#x} is not a number"))?;
        number(&reader, object)
    }

    /// An array's length and elements, as inspect lists them.
    fn array(&mut self, object: Object, level: u32, full: bool) -> Result<Array, Problem> {
        let reader = self.reader;
        let length_word = reader.field(object, Field::JSArrayLength)?;
        let length = self.number_word(length_word)?;
        if length.fract() != 0.0 || !(0.0..=4_294_967_295.0).contains(&length) {
            return Err(format!("an array's length is {length}").into());
        }
        #[allow(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "a whole number from 0 to 2^32 - 1"
        )]
        let length = length as u32;
        if !full {
            return Ok(Array {
                length,
                elements: Vec::new(),
                listed_below: 0,
            });
        }
        // Read every present element, however sparse, so the list is
        // complete up to the length.
        let present = own::elements(&reader, object, 0..length)?;
        let elements = present
            .into_iter()
            .map(|element| Element {
                index: element.index,
                slot: if element.enumerable {
                    self.slot(element.held, level)
                } else {
                    Slot::Data(Value::Unreadable(
                        "the element is not enumerable; inspect would still show its value"
                            .to_owned(),
                    ))
                },
            })
            .collect();
        Ok(Array {
            length,
            elements,
            listed_below: length,
        })
    }

    /// A function's kind and name, as inspect reads them.
    fn function(&mut self, object: Object) -> Result<Function, Problem> {
        let reader = self.reader;
        let name = match lookup::get(&reader, object, lookup::Key::Name("name"))? {
            Some(lookup::Value::Word(word)) => self.value(word, FULL_LEVEL + 1),
            Some(lookup::Value::Text(units)) => Value::String(JsString::complete(units)),
            None => Value::Undefined,
        };
        if reader.is(object, Constant::JSBoundFunctionType)? {
            return Ok(Function {
                kind: FunctionKind::Plain,
                name,
            });
        }
        let shared = reader.typed_field(
            object,
            Field::JSFunctionSharedFunctionInfo,
            Constant::SharedFunctionInfoType,
        )?;
        let flags = i64::from(reader.int32_field(shared, Field::SharedFunctionInfoFlags)?);
        let class = flags & reader.constant(Constant::SfiIsClassConstructorMask)? != 0;
        if class {
            // Prefer the class's own `name` property if it has one.
            let own_name = match lookup::own(&reader, object, lookup::Key::Name("name"))? {
                Some((Held::Data(word), _)) => self.value(word, FULL_LEVEL + 1),
                Some((Held::Accessor(_), _)) => {
                    match lookup::get(&reader, object, lookup::Key::Name("name"))? {
                        Some(lookup::Value::Text(units)) => {
                            Value::String(JsString::complete(units))
                        }
                        Some(lookup::Value::Word(word)) => self.value(word, FULL_LEVEL + 1),
                        None => Value::Undefined,
                    }
                }
                _ => Value::Undefined,
            };
            let extends = match lookup::prototype(&reader, object)? {
                Some(parent) => match lookup::get(&reader, parent, lookup::Key::Name("name"))? {
                    Some(lookup::Value::Word(word)) => self.value(word, FULL_LEVEL + 1),
                    Some(lookup::Value::Text(units)) => Value::String(JsString::complete(units)),
                    None => Value::Undefined,
                },
                None => Value::Null,
            };
            return Ok(Function {
                kind: FunctionKind::Class { extends },
                name: own_name,
            });
        }
        let kind = (flags & reader.constant(Constant::SfiFunctionKindMask)?)
            >> reader.constant(Constant::SfiFunctionKindShift)?;
        let is = |constants: &[Constant]| {
            constants
                .iter()
                .any(|constant| reader.constant(*constant).ok() == Some(kind))
        };
        let kind = if is(&[
            Constant::FunctionKindAsyncArrowFunction,
            Constant::FunctionKindAsyncFunction,
            Constant::FunctionKindAsyncConciseMethod,
            Constant::FunctionKindStaticAsyncConciseMethod,
        ]) {
            FunctionKind::Async
        } else if is(&[
            Constant::FunctionKindAsyncConciseGeneratorMethod,
            Constant::FunctionKindStaticAsyncConciseGeneratorMethod,
            Constant::FunctionKindAsyncGeneratorFunction,
        ]) {
            FunctionKind::AsyncGenerator
        } else if is(&[
            Constant::FunctionKindGeneratorFunction,
            Constant::FunctionKindConciseGeneratorMethod,
            Constant::FunctionKindStaticConciseGeneratorMethod,
        ]) {
            FunctionKind::Generator
        } else {
            FunctionKind::Plain
        };
        Ok(Function { kind, name })
    }

    /// A Map's or a Set's size and first entries, in insertion order.
    fn collection(
        &mut self,
        object: Object,
        level: u32,
        full: bool,
        map: bool,
    ) -> Result<Kind, Problem> {
        let reader = self.reader;
        let (entries, size) = super::collections::entries(&reader, object, map, full)?;
        let size = u32::try_from(size).map_err(|_| "a collection's size is out of range")?;
        Ok(if map {
            Kind::Map {
                size,
                entries: entries
                    .into_iter()
                    .map(|(key, value)| {
                        (
                            self.value(key, level),
                            self.value(value.unwrap_or(0), level),
                        )
                    })
                    .collect(),
            }
        } else {
            Kind::Set {
                size,
                values: entries
                    .into_iter()
                    .map(|(key, _)| self.value(key, level))
                    .collect(),
            }
        })
    }

    /// A `RegExp`'s source and flags, as its getters give them.
    fn regexp(&self, object: Object) -> Result<Kind, Problem> {
        let reader = self.reader;
        let source = reader
            .object(reader.field(object, Field::JSRegExpSource)?)?
            .ok_or("a RegExp's source is not a string")?;
        let bits = reader
            .smi(reader.field(object, Field::JSRegExpFlags)?)?
            .ok_or("a RegExp's flags are not a Smi")?;
        // In the order the `flags` getter uses.
        let order = [
            ('d', Constant::JSRegExpHasIndicesShift),
            ('g', Constant::JSRegExpGlobalShift),
            ('i', Constant::JSRegExpIgnoreCaseShift),
            ('l', Constant::JSRegExpLinearShift),
            ('m', Constant::JSRegExpMultilineShift),
            ('s', Constant::JSRegExpDotAllShift),
            ('u', Constant::JSRegExpUnicodeShift),
            ('v', Constant::JSRegExpUnicodeSetsShift),
            ('y', Constant::JSRegExpStickyShift),
        ];
        let mut flags = Vec::new();
        for (letter, shift) in order {
            if (bits >> reader.constant(shift)?) & 1 != 0 {
                flags.push(u16::try_from(u32::from(letter)).unwrap_or(0));
            }
        }
        Ok(Kind::RegExp {
            source: string(&reader, source, WHOLE_UNITS)?,
            flags: JsString::complete(flags),
        })
    }

    /// A promise's state and result.
    fn promise(&mut self, object: Object, level: u32) -> Result<PromiseState, Problem> {
        let reader = self.reader;
        let flags = reader
            .smi(reader.field(object, Field::JSPromiseFlags)?)?
            .ok_or("a promise's flags are not a Smi")?;
        let status = super::properties::part(
            &reader,
            flags,
            Constant::JSPromiseStatusMask,
            Some(Constant::JSPromiseStatusShift),
        )?;
        let result = reader.field(object, Field::JSPromiseReactionsOrResult)?;
        Ok(if status == reader.constant(Constant::PromisePending)? {
            PromiseState::Pending
        } else if status == reader.constant(Constant::PromiseFulfilled)? {
            PromiseState::Fulfilled(self.value(result, level))
        } else if status == reader.constant(Constant::PromiseRejected)? {
            PromiseState::Rejected(self.value(result, level))
        } else {
            return Err(format!("a promise's status is {status}").into());
        })
    }

    /// inspect's `getConstructorName`: the first own `constructor` data
    /// property on the prototype chain that is a named function `object` is
    /// an instance of.
    fn constructor(&self, object: Object) -> Result<Constructor, Problem> {
        let reader = self.reader;
        let mut holder = object;
        let mut first: Option<Option<Object>> = None;
        for _ in 0..64 {
            if let Some((Held::Data(word), _)) =
                lookup::own(&reader, holder, lookup::Key::Name("constructor"))?
                && let Some(function) = reader.object(word)?
                && lookup::is_callable(&reader, function)?
                && let Some(name) = self.name_of(function)?
                && !name.units().is_empty()
                && lookup::instance_of(&reader, object, function)?
            {
                return Ok(Constructor::Named(name));
            }
            let next = lookup::prototype(&reader, holder)?;
            if first.is_none() {
                first = Some(next);
            }
            match next {
                Some(next) => holder = next,
                None => break,
            }
        }
        match first {
            Some(Some(first)) => self.unnamed(object, first),
            Some(None) if reader.is(object, Constant::JSObjectType)? => {
                // V8's `GetConstructorName` for `Object.create(null)`.
                Ok(Constructor::NullPrototype {
                    internal: JsString::from("Object"),
                })
            }
            _ => Err("no prototype names the object's constructor".into()),
        }
    }

    /// `getConstructorName` when no prototype names a constructor: V8's
    /// name for `object` and each prototype from `first` but the last, then
    /// the last as `inspect` prints it at depth -1. The last must be an
    /// ordinary untagged object.
    fn unnamed(&self, object: Object, first: Object) -> Result<Constructor, Problem> {
        let reader = self.reader;
        let name =
            |object| super::constructors::constructor_name(&reader, object).map(JsString::complete);
        let mut names = vec![name(object)?];
        let mut end = first;
        for _ in 0..64 {
            let Some(next) = lookup::prototype(&reader, end)? else {
                if !reader.is(end, Constant::JSObjectType)? {
                    return Err("the last prototype is not an ordinary object".into());
                }
                let own = |key| lookup::own(&reader, end, key).map(|found| found.is_some());
                if own(lookup::Key::WellKnown("Symbol.toStringTag"))? {
                    return Err("the last prototype has its own Symbol.toStringTag".into());
                }
                if own(lookup::Key::Name("href"))? {
                    // inspect may take it for a URL.
                    return Err("the last prototype has its own href".into());
                }
                let end_has_keys = own::elements(&reader, end, 0..u32::MAX)?
                    .iter()
                    .any(|element| element.enumerable)
                    || own::named(&reader, end)?
                        .iter()
                        .any(|named| named.enumerable);
                return Ok(Constructor::Unnamed {
                    names,
                    end: name(end)?,
                    end_has_keys,
                });
            };
            names.push(name(end)?);
            end = next;
        }
        Err(format!("the prototype chain of {:#x} is too long", object.0).into())
    }

    /// `function.name`, or `None` if it is not a string.
    fn name_of(&self, function: Object) -> Result<Option<JsString>, Problem> {
        let reader = self.reader;
        Ok(
            match lookup::get(&reader, function, lookup::Key::Name("name"))? {
                Some(lookup::Value::Text(units)) => Some(JsString::complete(units)),
                Some(lookup::Value::Word(word)) => match reader.object(word)? {
                    Some(name)
                        if reader.instance_type(name)?
                            < reader.constant(Constant::FirstNonstringType)? =>
                    {
                        Some(string(&reader, name, WHOLE_UNITS)?)
                    }
                    _ => None,
                },
                None => None,
            },
        )
    }

    /// `object[Symbol.toStringTag]` if it is a string and not an own
    /// enumerable property. Getters are refused except
    /// `%TypedArray%.prototype`'s.
    fn tag(&self, object: Object, ty: i64) -> Result<Option<JsString>, Problem> {
        let reader = self.reader;
        let key = lookup::Key::WellKnown("Symbol.toStringTag");
        if let Some((_, true)) = lookup::own(&reader, object, key)? {
            return Ok(None);
        }
        match lookup::find(&reader, object, key)? {
            Found::Absent => Ok(None),
            Found::Data(word) => Ok(match reader.object(word)? {
                Some(tag)
                    if reader.instance_type(tag)?
                        < reader.constant(Constant::FirstNonstringType)? =>
                {
                    Some(string(&reader, tag, WHOLE_UNITS)?)
                }
                _ => None,
            }),
            Found::Accessor { accessor, .. } => {
                if reader.is(accessor, Constant::AccessorPairType)? {
                    let getter = reader.field(accessor, Field::AccessorPairGetter)?;
                    if let Some(getter) = reader.object(getter)?
                        && lookup::is_builtin(&reader, getter, "get [Symbol.toStringTag]")?
                    {
                        // This getter returns the constructor name for a
                        // typed array and undefined for anything else.
                        if ty != reader.constant(Constant::JSTypedArrayType)? {
                            return Ok(None);
                        }
                        let kind = own::elements_kind(&reader, reader.map(object)?)?;
                        return Ok(super::typed::element_type(&reader, kind).map(|element| {
                            JsString::from(super::typed::constructor_name(element))
                        }));
                    }
                }
                Err(
                    "the object's Symbol.toStringTag is a getter, which microscope does not call"
                        .into(),
                )
            }
        }
    }
}

/// The text of the string `object`, its first `limit` units.
pub(super) fn string(reader: &Reader<'_>, object: Object, limit: u64) -> Result<JsString, Problem> {
    let length = u32::try_from(reader.int32_field(object, Field::StringLength)?)
        .map_err(|_| format!("the string {:#x} has a negative length", object.0))?;
    let mut units = Vec::new();
    reader.string_into(object, 0, limit, 0, &mut units)?;
    Ok(JsString::new(units, length))
}

/// The number a `HeapNumber` holds.
fn number(reader: &Reader<'_>, object: Object) -> Result<f64, Problem> {
    Ok(f64::from_bits(reader.word(
        reader.field_address(object, Field::HeapNumberValue)?,
    )?))
}

/// The value an oddball stands for.
fn oddball(reader: &Reader<'_>, object: Object) -> Result<Value, Problem> {
    let kind = reader.smi_field(object, Field::OddballKind)?;
    let is = |constant| reader.constant(constant).ok() == Some(kind);
    Ok(if is(Constant::OddballUndefined) {
        Value::Undefined
    } else if is(Constant::OddballNull) {
        Value::Null
    } else if is(Constant::OddballTrue) {
        Value::Boolean(true)
    } else if is(Constant::OddballFalse) {
        Value::Boolean(false)
    } else {
        return Err(format!("an internal V8 oddball of kind {kind}").into());
    })
}

/// A `BigInt`, from its sign and its digits.
fn bigint(reader: &Reader<'_>, object: Object) -> Result<BigInt, Problem> {
    let bits = i64::from(
        reader
            .int32_field(object, Field::BigIntBaseBitfield)?
            .cast_unsigned(),
    );
    let negative = super::properties::part(
        reader,
        bits,
        Constant::BigIntSignMask,
        Some(Constant::BigIntSignShift),
    )? != 0;
    let length = super::properties::part(
        reader,
        bits,
        Constant::BigIntLengthMask,
        Some(Constant::BigIntLengthShift),
    )?;
    // inspect prints a BigInt in full; refuse ones past about a million bits.
    if length > 1 << 14 {
        return Err(format!("the BigInt is too large to show ({length} digits)").into());
    }
    let digits = own::words(
        reader,
        reader.field_address(object, Field::BigIntBaseDigits)?,
        length,
    )?;
    Ok(BigInt::new(negative, digits))
}

/// A symbol and its description.
fn symbol(reader: &Reader<'_>, object: Object) -> Result<Symbol, Problem> {
    let word = reader.field(object, Field::SymbolDescription)?;
    let description = match reader.object(word)? {
        Some(text)
            if reader.instance_type(text)? < reader.constant(Constant::FirstNonstringType)? =>
        {
            Some(string(reader, text, WHOLE_UNITS)?)
        }
        _ => None,
    };
    Ok(Symbol { description })
}

/// The flags of the symbol `symbol`.
fn symbol_flags(reader: &Reader<'_>, symbol: Object) -> Result<i64, Problem> {
    Ok(i64::from(
        reader
            .int32_field(symbol, Field::SymbolFlags)?
            .cast_unsigned(),
    ))
}

/// Whether `symbol` is private and so never enumerated: V8's internal
/// private symbols, and class private names and brands.
pub(super) fn is_private(reader: &Reader<'_>, symbol: Object) -> Result<bool, Problem> {
    let flags = symbol_flags(reader, symbol)?;
    // Through V8 13.6 a flag bit; from 14.6 any kind but public.
    if let Ok(mask) = reader.constant(Constant::SymbolIsPrivateMask) {
        return Ok(flags & mask != 0);
    }
    let kind = super::properties::part(
        reader,
        flags,
        Constant::SymbolPrivateSymbolKindMask,
        Some(Constant::SymbolPrivateSymbolKindShift),
    )?;
    Ok(kind != reader.constant(Constant::PrivateSymbolKindPublic)?)
}

/// Whether `symbol` is a well-known symbol such as `Symbol.iterator`.
pub(super) fn is_well_known(reader: &Reader<'_>, symbol: Object) -> Result<bool, Problem> {
    let flags = symbol_flags(reader, symbol)?;
    Ok(flags & reader.constant(Constant::SymbolIsWellKnownSymbolMask)? != 0)
}

/// For a global proxy (`globalThis`), the global object behind it (its map's
/// prototype), as V8's `LookupIterator` does; any other object unchanged.
fn global_object(reader: &Reader<'_>, object: Object) -> Result<Object, Problem> {
    if !reader.is(object, Constant::JSGlobalProxyType)? {
        return Ok(object);
    }
    let map = reader.map(object)?;
    reader
        .object(reader.field(map, Field::MapPrototype)?)?
        .filter(|global| {
            reader
                .is(*global, Constant::JSGlobalObjectType)
                .unwrap_or(false)
        })
        .ok_or_else(|| "the global proxy is detached from its global object".into())
}
