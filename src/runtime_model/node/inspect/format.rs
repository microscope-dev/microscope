//! `formatValue`, `formatRaw`, and the formatters and line breaking they
//! call, from Node's `lib/internal/util/inspect.js`.

use super::text::{
    bigint_to_string, escape, f16_to_f64, is_identifier, iso_date, js_len, number_to_string,
    pad_end, pad_start, str_escape, string_width,
};
use super::{
    Array, BigInt, Constructor, ElementType, Function, FunctionKind, Inspected, JsString, Key,
    Kind, Layout, NodeRelease, Object, ObjectId, PromiseState, Property, Slot, Symbol, Value,
    ValueGraph,
};
use std::fmt::Write as _;

use crate::VirtualAddress;

/// `depth`: how many levels below the root inspect prints contents.
pub const DEPTH: usize = 2;
/// `breakLength`.
const BREAK_LENGTH: usize = 80;
/// `compact`: how many inner levels inspect lines up on one line.
const COMPACT: usize = 3;
/// `maxArrayLength`, which also bounds Set, Map, and buffer entries.
const MAX_ARRAY_LENGTH: usize = 100;
/// `maxStringLength`.
const MAX_STRING_LENGTH: usize = 10_000;
/// `kMinLineLength`: strings this long or shorter are never split.
const MIN_LINE_LENGTH: usize = 16;

pub fn inspect(graph: &ValueGraph, value: &Value, node: NodeRelease, layout: Layout) -> Inspected {
    let mut ctx = Ctx {
        graph,
        major: node.major,
        node,
        line: layout == Layout::Line,
        indentation: 0,
        seen: Vec::new(),
        circular: Vec::new(),
        current_depth: 0,
        approximate: false,
    };
    let text = ctx.format_value(value, 0);
    Inspected {
        text,
        approximate: ctx.approximate,
    }
}

/// The text for a part the reader could not read.
pub fn unreadable(reason: &str) -> String {
    format!("<unreadable: {reason}>")
}

/// The text for a state this port does not reproduce.
pub fn unsupported(what: &str) -> String {
    format!("<unsupported: {what}>")
}

/// Why a JavaScript conversion gave no text.
pub enum Fault {
    /// Node's code throws here.
    Throws,
    /// The text to print instead.
    Visible(String),
}

/// `kObjectType`, `kArrayType`, and `kArrayExtrasType`.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Entries {
    Object,
    ArrayExtras,
}

/// An own property inspect lists, and whether it is enumerable.
pub type Listed<'g> = (&'g Property, bool);

/// What fills an object's braces before its properties.
enum Contents<'g> {
    None,
    Array(&'g Array),
    Set(u32, &'g [Value]),
    Map(u32, &'g [(Value, Value)]),
    TypedArray(ElementType, u64, &'g [u8]),
    ArrayBuffer(u64, Option<&'g [u8]>),
    Promise(&'g PromiseState),
    Weak,
}

/// A change to `inspect.js` within a release line, made in the 24 and 26
/// releases listed in [`Ctx::has`] and later.
#[derive(Clone, Copy)]
pub enum Change {
    /// `isBelowBreakLength` returns early for an infinite `breakLength`,
    /// refusing only entries with line breaks.
    InfiniteBreakLength,
    /// `getPrefix` leaves out a `Symbol.toStringTag` equal to the
    /// constructor's name, or to the tag's last dotted part.
    TagByName,
    /// `formatError` lists a `SuppressedError`'s own `error` and
    /// `suppressed`.
    SuppressedKeys,
    /// `formatExtraProperties` reads a getter before indenting two columns,
    /// so a detached `DataView`'s throw leaves the indentation unchanged.
    ExtraBeforeIndent,
}

/// inspect's `ctx`, with the options fixed at their defaults or at
/// [`Layout::Line`]'s.
pub struct Ctx<'g> {
    pub graph: &'g ValueGraph,
    pub major: u32,
    node: NodeRelease,
    /// `compact: true` and `breakLength: Infinity`.
    line: bool,
    pub indentation: usize,
    /// The objects being formatted, innermost last, by address.
    seen: Vec<VirtualAddress>,
    /// `ctx.circular`: objects that refer back to themselves, numbered from
    /// 1 in the order found.
    circular: Vec<VirtualAddress>,
    /// `ctx.currentDepth`: the level of the object last entered.
    current_depth: usize,
    pub approximate: bool,
}

impl<'g> Ctx<'g> {
    /// Whether the target Node release has `change`. Each landed in 26, some
    /// also in 24, none in 22. Modules for 23 and 25 are refused earlier.
    pub const fn has(&self, change: Change) -> bool {
        let (on_24, on_26) = match change {
            Change::InfiniteBreakLength => (Some(21), 8),
            Change::TagByName => (Some(22), 9),
            Change::SuppressedKeys | Change::ExtraBeforeIndent => (None, 11),
        };
        match (self.node.major, on_24) {
            (24, Some(minor)) => self.node.minor >= minor,
            (26, _) => self.node.minor >= on_26,
            (major, _) => major > 26,
        }
    }

    /// `formatValue`.
    pub fn format_value(&mut self, value: &Value, level: usize) -> String {
        match value {
            Value::Undefined => "undefined".to_owned(),
            Value::Null => "null".to_owned(),
            Value::Boolean(value) => value.to_string(),
            Value::Number(value) => format_number(*value),
            Value::BigInt(value) => format_bigint(value),
            Value::String(value) => self.format_string(value),
            Value::Symbol(symbol) => {
                symbol_text(symbol).unwrap_or_else(|| unreadable("a truncated symbol description"))
            }
            Value::Object(id) => self.format_object(*id, level),
            Value::Unreadable(reason) => unreadable(reason),
        }
    }

    /// `formatValue` past the primitives: proxies, cycles, then `formatRaw`.
    fn format_object(&mut self, id: ObjectId, level: usize) -> String {
        let graph = self.graph;
        let Some(mut object) = graph.object(id) else {
            return unreadable(&format!("no object in row {id}"));
        };
        let mut proxies = 0;
        if let Kind::Proxy { target } = &object.kind {
            let mut target = target;
            loop {
                let next = match target {
                    Value::Null => return wrap_proxies("<Revoked Proxy>".to_owned(), proxies),
                    Value::Object(id) => graph.object(*id),
                    Value::Unreadable(reason) => return unreadable(reason),
                    _ => None,
                };
                let Some(next) = next else {
                    return unreadable("a proxy's target");
                };
                if self.major < 26 {
                    // Node 22 and 24 format the target directly, running
                    // its traps if it is a proxy.
                    if matches!(next.kind, Kind::Proxy { .. }) {
                        return unsupported(&format!(
                            "a proxy of a proxy, whose traps Node {} runs",
                            self.major
                        ));
                    }
                    object = next;
                    break;
                }
                object = next;
                proxies += 1;
                match &next.kind {
                    Kind::Proxy { target: inner } => target = inner,
                    _ => break,
                }
            }
        }

        if self.seen.contains(&object.address) {
            let found = self
                .circular
                .iter()
                .position(|seen| *seen == object.address);
            let index = if let Some(index) = found {
                index + 1
            } else {
                self.circular.push(object.address);
                self.circular.len()
            };
            return format!("[Circular *{index}]");
        }

        let formatted = self.format_raw(object, level);
        wrap_proxies(formatted, proxies)
    }

    /// `formatRaw`.
    #[allow(
        clippy::too_many_lines,
        reason = "one branch per kind, in the order inspect.js tests them"
    )]
    fn format_raw(&mut self, object: &'g Object, level: usize) -> String {
        let constructor = match Self::constructor_name(&object.constructor, level) {
            Ok(constructor) => constructor,
            Err(text) => return text,
        };
        let constructor = constructor.as_deref();
        let tag = match &object.tag {
            None => String::new(),
            Some(tag) => match tag.whole() {
                Some(tag) => tag,
                None => return unreadable("a truncated Symbol.toStringTag"),
            },
        };

        let mut keys: Vec<Listed<'g>> = object.properties.iter().map(|p| (p, true)).collect();
        let mut base = String::new();
        let mut open = "{".to_owned();
        let mut close = "}";
        let mut contents = Contents::None;
        let mut entries = Entries::Object;
        let mut extra_keys: &[&str] = &[];

        match &object.kind {
            Kind::Array(array) => {
                let prefix = if constructor != Some("Array") || !tag.is_empty() {
                    self.prefix(constructor, &tag, "Array", &format!("({})", array.length))
                } else {
                    String::new()
                };
                open = format!("{prefix}[");
                close = "]";
                if array.length == 0 && keys.is_empty() {
                    return format!("{open}]");
                }
                entries = Entries::ArrayExtras;
                contents = Contents::Array(array);
            }
            Kind::Set { size, values } => {
                let prefix = self.prefix(constructor, &tag, "Set", &format!("({size})"));
                if *size == 0 && keys.is_empty() {
                    return format!("{prefix}{{}}");
                }
                open = format!("{prefix}{{");
                contents = Contents::Set(*size, values);
            }
            Kind::Map { size, entries: map } => {
                let prefix = self.prefix(constructor, &tag, "Map", &format!("({size})"));
                if *size == 0 && keys.is_empty() {
                    return format!("{prefix}{{}}");
                }
                open = format!("{prefix}{{");
                contents = Contents::Map(*size, map);
            }
            Kind::TypedArray {
                element,
                length,
                bytes,
            } => {
                let fallback = if constructor.is_none() {
                    element.name()
                } else {
                    ""
                };
                let prefix = self.prefix(constructor, &tag, fallback, &format!("({length})"));
                open = format!("{prefix}[");
                close = "]";
                if *length == 0 && keys.is_empty() {
                    return format!("{open}]");
                }
                contents = Contents::TypedArray(*element, *length, bytes);
                entries = Entries::ArrayExtras;
            }
            Kind::Function(function) => {
                base = self.function_base(function, constructor, &tag);
                if keys.is_empty() {
                    return base;
                }
            }
            _ if constructor == Some("Object") => {
                if !tag.is_empty() {
                    open = format!("{}{{", self.prefix(constructor, &tag, "Object", ""));
                }
                if keys.is_empty() {
                    return format!("{open}}}");
                }
            }
            Kind::RegExp { source, flags } => {
                let (Some(source), Some(flags)) = (source.whole(), flags.whole()) else {
                    return unreadable("a truncated RegExp source");
                };
                base = format!("/{source}/{flags}");
                let prefix = self.prefix(constructor, &tag, "RegExp", "");
                if prefix != "RegExp " {
                    base = format!("{prefix}{base}");
                }
                if keys.is_empty() || level > DEPTH {
                    return base;
                }
            }
            Kind::Date { time } => {
                base = if time.is_nan() {
                    "Invalid Date".to_owned()
                } else {
                    match iso_date(*time) {
                        Some(text) => text,
                        None => return unreadable("a Date's time value out of range"),
                    }
                };
                let prefix = self.prefix(constructor, &tag, "Date", "");
                if prefix != "Date " {
                    base = format!("{prefix}{base}");
                }
                if keys.is_empty() {
                    return base;
                }
            }
            Kind::Error(details) => {
                base = self.format_error(object, details, constructor, &tag, &mut keys);
                if keys.is_empty() {
                    return base;
                }
            }
            Kind::ArrayBuffer {
                shared,
                byte_length,
                bytes,
            } => {
                let name = if *shared {
                    "SharedArrayBuffer"
                } else {
                    "ArrayBuffer"
                };
                let prefix = self.prefix(constructor, &tag, name, "");
                contents = Contents::ArrayBuffer(*byte_length, bytes.as_deref());
                open = format!("{prefix}{{");
                extra_keys = &["byteLength"];
            }
            Kind::DataView { .. } => {
                open = format!("{}{{", self.prefix(constructor, &tag, "DataView", ""));
                extra_keys = &["byteLength", "byteOffset", "buffer"];
            }
            Kind::Promise(state) => {
                open = format!("{}{{", self.prefix(constructor, &tag, "Promise", ""));
                contents = Contents::Promise(state);
            }
            Kind::WeakSet => {
                open = format!("{}{{", self.prefix(constructor, &tag, "WeakSet", ""));
                contents = Contents::Weak;
            }
            Kind::WeakMap => {
                open = format!("{}{{", self.prefix(constructor, &tag, "WeakMap", ""));
                contents = Contents::Weak;
            }
            Kind::Boxed(primitive) => {
                base = self.boxed_base(primitive, constructor, &tag);
                if keys.is_empty() {
                    return base;
                }
            }
            Kind::Ordinary | Kind::Proxy { .. } => {
                let style = self.ctx_style(object, constructor, &tag);
                if keys.is_empty() {
                    return format!("{style}{{}}");
                }
                open = format!("{style}{{");
            }
        }

        if level > DEPTH {
            let mut name = self.ctx_style(object, constructor, &tag);
            name.pop();
            return if constructor.is_some() {
                format!("[{name}]")
            } else {
                name
            };
        }
        let level = level + 1;

        self.seen.push(object.address);
        self.current_depth = level;
        let mut output = match contents {
            Contents::None => Vec::new(),
            Contents::Array(array) => self.format_array(array, level),
            Contents::Set(size, values) => self.format_set(size, values, level),
            Contents::Map(size, map) => self.format_map(size, map, level),
            Contents::TypedArray(element, length, bytes) => {
                format_typed_array(element, length, bytes)
            }
            Contents::ArrayBuffer(byte_length, bytes) => format_array_buffer(byte_length, bytes),
            Contents::Promise(state) => self.format_promise(state, level),
            Contents::Weak => vec!["<items unknown>".to_owned()],
        };
        for key in extra_keys {
            let entry = self.format_extra_property(object, key, level);
            output.push(entry);
        }
        for (property, enumerable) in &keys {
            let entry = self.format_property(property, *enumerable, entries, level);
            output.push(entry);
        }
        if let Some(index) = self
            .circular
            .iter()
            .position(|seen| *seen == object.address)
        {
            let reference = format!("<ref *{}>", index + 1);
            if self.line {
                open = format!("{reference} {open}");
            } else if base.is_empty() {
                base = reference;
            } else {
                base = format!("{reference} {base}");
            }
        }
        self.seen.pop();

        self.reduce_to_single_string(output, &base, (&open, close), entries, level, Some(object))
    }

    /// `getConstructorName`, or the text to print instead of the object.
    fn constructor_name(constructor: &Constructor, level: usize) -> Result<Option<String>, String> {
        match constructor {
            Constructor::Named(name) => name
                .whole()
                .map(Some)
                .ok_or_else(|| unreadable("a truncated constructor name")),
            Constructor::NullPrototype { .. } => Ok(None),
            Constructor::Unnamed {
                names,
                end,
                end_has_keys,
            } => {
                let names: Option<Vec<String>> = names.iter().map(JsString::whole).collect();
                let (Some(names), Some(end)) = (names, end.whole()) else {
                    return Err(unreadable("a truncated constructor name"));
                };
                // `inspect(end, { depth: -1 })` of an ordinary object whose
                // prototype is null.
                let end = if *end_has_keys {
                    format!("[{end}: null prototype]")
                } else {
                    format!("[{end}: null prototype] {{}}")
                };
                unnamed_constructor(&names, &end, level)
                    .map(Some)
                    .ok_or_else(|| unreadable("an unnamed prototype chain without names"))
            }
        }
    }

    /// `getPrefix`.
    pub fn prefix(
        &self,
        constructor: Option<&str>,
        tag: &str,
        fallback: &str,
        size: &str,
    ) -> String {
        let Some(constructor) = constructor else {
            if !tag.is_empty() && fallback != tag {
                return format!("[{fallback}{size}: null prototype] [{tag}] ");
            }
            return format!("[{fallback}{size}: null prototype] ");
        };
        if !self.has(Change::TagByName) {
            let mut result = format!("{constructor}{size} ");
            if !tag.is_empty() {
                match constructor.find(tag) {
                    None => {
                        let _ = write!(result, "[{tag}] ");
                    }
                    Some(position) => {
                        let after = constructor[position + tag.len()..].chars().next();
                        if after.is_some_and(|c| c.to_lowercase().eq(std::iter::once(c))) {
                            let _ = write!(result, "[{tag}] ");
                        }
                    }
                }
            }
            return result;
        }
        if !tag.is_empty() {
            let name = match tag.rfind('.') {
                Some(dot) if dot >= 1 => &tag[dot + 1..],
                _ => tag,
            };
            if constructor == name {
                return format!("{tag}{size} ");
            }
            return format!("{constructor}{size} [{tag}] ");
        }
        format!("{constructor}{size} ")
    }

    /// `getCtxStyle`.
    fn ctx_style(&self, object: &Object, constructor: Option<&str>, tag: &str) -> String {
        let mut fallback = String::new();
        if constructor.is_none() {
            fallback = match &object.constructor {
                Constructor::NullPrototype { internal } => internal
                    .whole()
                    .unwrap_or_else(|| unreadable("a truncated constructor name")),
                _ => "Object".to_owned(),
            };
            if fallback == tag {
                "Object".clone_into(&mut fallback);
            }
        }
        self.prefix(constructor, tag, &fallback, "")
    }

    /// `getFunctionBase` and `getClassBase`.
    fn function_base(
        &mut self,
        function: &Function,
        constructor: Option<&str>,
        tag: &str,
    ) -> String {
        let tagged = !tag.is_empty() && constructor != Some(tag);
        let kind = match &function.kind {
            FunctionKind::Class { extends } => {
                let name = if truthy(&function.name) {
                    template_text(&function.name)
                } else {
                    "(anonymous)".to_owned()
                };
                let mut base = format!("class {name}");
                if let Some(constructor) = constructor
                    && constructor != "Function"
                {
                    let _ = write!(base, " [{constructor}]");
                }
                if tagged {
                    let _ = write!(base, " [{tag}]");
                }
                if constructor.is_some() {
                    if truthy(extends) {
                        let _ = write!(base, " extends {}", template_text(extends));
                    }
                } else {
                    base.push_str(" extends [null prototype]");
                }
                return format!("[{base}]");
            }
            FunctionKind::Plain => "Function",
            FunctionKind::Async => "AsyncFunction",
            FunctionKind::Generator => "GeneratorFunction",
            FunctionKind::AsyncGenerator => "AsyncGeneratorFunction",
        };
        let mut base = format!("[{kind}");
        if constructor.is_none() {
            base.push_str(" (null prototype)");
        }
        match &function.name {
            Value::String(name) if name.length() == 0 => base.push_str(" (anonymous)"),
            Value::String(name) => {
                let name = name
                    .whole()
                    .unwrap_or_else(|| unreadable("a truncated function name"));
                let _ = write!(base, ": {name}");
            }
            Value::Object(_) => {
                let _ = write!(
                    base,
                    ": {}",
                    unsupported("a function name that is an object")
                );
            }
            name => {
                let name = self.format_value(name, 0);
                let _ = write!(base, ": {name}");
            }
        }
        base.push(']');
        if let Some(constructor) = constructor
            && constructor != kind
        {
            let _ = write!(base, " {constructor}");
        }
        if tagged {
            let _ = write!(base, " [{tag}]");
        }
        base
    }

    /// `getBoxedBase`.
    fn boxed_base(&mut self, primitive: &Value, constructor: Option<&str>, tag: &str) -> String {
        let kind = match primitive {
            Value::Number(_) => "Number",
            Value::String(_) => "String",
            Value::Boolean(_) => "Boolean",
            Value::BigInt(_) => "BigInt",
            Value::Symbol(_) => "Symbol",
            Value::Unreadable(reason) => return unreadable(reason),
            _ => return unsupported("a wrapper of a value that is not a primitive"),
        };
        let mut base = format!("[{kind}");
        if constructor != Some(kind) {
            match constructor {
                None => base.push_str(" (null prototype)"),
                Some(constructor) => {
                    let _ = write!(base, " ({constructor})");
                }
            }
        }
        let value = self.format_value(primitive, 0);
        let _ = write!(base, ": {value}]");
        if !tag.is_empty() && constructor != Some(tag) {
            let _ = write!(base, " [{tag}]");
        }
        base
    }

    /// `formatPrimitive` of a string.
    fn format_string(&self, value: &JsString) -> String {
        let length = usize::try_from(value.length()).unwrap_or(usize::MAX);
        let shown = length.min(MAX_STRING_LENGTH);
        let Some(units) = value.units().get(..shown) else {
            return unreadable("a string whose first units were not read");
        };
        let trailer = if length > MAX_STRING_LENGTH {
            let remaining = length - MAX_STRING_LENGTH;
            format!("... {remaining} more character{}", plural(remaining))
        } else {
            String::new()
        };
        let room = i64::try_from(BREAK_LENGTH).unwrap_or(i64::MAX)
            - i64::try_from(self.indentation).unwrap_or(i64::MAX)
            - 4;
        if !self.line
            && units.len() > MIN_LINE_LENGTH
            && i64::try_from(units.len()).unwrap_or(i64::MAX) > room
        {
            let separator = format!(" +\n{}", " ".repeat(self.indentation + 2));
            let lines: Vec<String> = split_after_newlines(units).map(str_escape).collect();
            return lines.join(&separator) + &trailer;
        }
        str_escape(units) + &trailer
    }

    /// `formatProperty` of an object's own property.
    fn format_property(
        &mut self,
        property: &Property,
        enumerable: bool,
        entries: Entries,
        level: usize,
    ) -> String {
        // `compact: true` indents an object's values three columns. With an
        // infinite `breakLength`, no value starts on its own line.
        let extra = usize::from(self.line && entries == Entries::Object);
        self.indentation += extra;
        let text = self.format_slot(&property.slot, level);
        self.indentation -= extra;
        let name = self.property_name(&property.key, enumerable);
        format!("{name}: {text}")
    }

    /// `formatProperty`'s value: the slot formatted two columns further in.
    fn format_slot(&mut self, slot: &Slot, level: usize) -> String {
        match slot {
            Slot::Data(Value::Undefined)
            | Slot::Accessor {
                getter: false,
                setter: false,
            } => "undefined".to_owned(),
            Slot::Data(value) => {
                self.indentation += 2;
                let text = self.format_value(value, level);
                self.indentation -= 2;
                text
            }
            Slot::Accessor {
                getter: true,
                setter: true,
            } => "[Getter/Setter]".to_owned(),
            Slot::Accessor {
                getter: true,
                setter: false,
            } => "[Getter]".to_owned(),
            Slot::Accessor {
                getter: false,
                setter: true,
            } => "[Setter]".to_owned(),
        }
    }

    /// `formatProperty`'s name for a key.
    fn property_name(&self, key: &Key, enumerable: bool) -> String {
        if self.major < 24 {
            return match key {
                Key::Symbol(symbol) => symbol_units(symbol).map_or_else(
                    || unreadable("a truncated symbol description"),
                    |units| format!("[{}]", escape(&units, true)),
                ),
                Key::String(key) => match key.whole() {
                    None => unreadable("a truncated property key"),
                    Some(text) if text == "__proto__" => "['__proto__']".to_owned(),
                    Some(_) if !enumerable => format!("[{}]", escape(key.units(), true)),
                    Some(text) if is_identifier(&text) => text,
                    Some(_) => str_escape(key.units()),
                },
            };
        }
        let name = match key {
            Key::Symbol(symbol) => symbol_units(symbol).map_or_else(
                || unreadable("a truncated symbol description"),
                |units| escape(&units, true),
            ),
            Key::String(key) => match key.whole() {
                None => unreadable("a truncated property key"),
                Some(text) if text == "__proto__" => "['__proto__']".to_owned(),
                Some(text) if is_identifier(&text) => text,
                Some(_) => str_escape(key.units()),
            },
        };
        if enumerable {
            name
        } else {
            format!("[{name}]")
        }
    }

    /// `formatExtraProperties` for an `ArrayBuffer`'s or a `DataView`'s getters.
    fn format_extra_property(&mut self, object: &Object, key: &str, level: usize) -> String {
        let value = match (&object.kind, key) {
            (Kind::ArrayBuffer { byte_length, .. }, "byteLength") => number(*byte_length),
            (Kind::DataView { buffer, .. }, "buffer") => buffer.clone(),
            (
                Kind::DataView {
                    view: Some(view), ..
                },
                "byteLength",
            ) => number(view.byte_length),
            (
                Kind::DataView {
                    view: Some(view), ..
                },
                "byteOffset",
            ) => number(view.byte_offset),
            (Kind::DataView { view: None, buffer }, key) => {
                // The getter throws. Before 26, Node has already indented two
                // columns and never undoes it.
                if !self.has(Change::ExtraBeforeIndent) {
                    self.indentation += 2;
                }
                if key == "byteLength" {
                    self.buffer_byte_length(buffer)
                } else {
                    Value::Undefined
                }
            }
            _ => Value::Unreadable(format!("the {key} of an object without one")),
        };
        self.indentation += 2;
        let text = self.format_value(&value, level);
        self.indentation -= 2;
        format!("[{key}]: {text}")
    }

    /// `value.buffer.byteLength`.
    fn buffer_byte_length(&self, buffer: &Value) -> Value {
        if let Value::Object(id) = buffer
            && let Some(object) = self.graph.object(*id)
            && let Kind::ArrayBuffer { byte_length, .. } = object.kind
        {
            return number(byte_length);
        }
        Value::Unreadable("a DataView's buffer".to_owned())
    }

    /// `formatArray` and `formatSpecialArray`.
    fn format_array(&mut self, array: &Array, level: usize) -> Vec<String> {
        let length = usize::try_from(array.length).unwrap_or(usize::MAX);
        let listed_below = usize::try_from(array.listed_below).unwrap_or(usize::MAX);
        let shown = MAX_ARRAY_LENGTH.min(length);
        let mut output = Vec::new();
        for i in 0..shown {
            if i >= listed_below {
                output.push(unreadable(&format!("the elements from index {i} on")));
                return output;
            }
            match array.elements.get(i) {
                Some(element) if usize::try_from(element.index).ok() == Some(i) => {
                    let text = self.format_slot(&element.slot, level);
                    output.push(text);
                }
                _ => return self.format_special_array(array, level, shown, output, i),
            }
        }
        if length > shown {
            output.push(remaining_text(length - shown));
        }
        output
    }

    /// `formatSpecialArray`: an array with holes from `start` on.
    fn format_special_array(
        &mut self,
        array: &Array,
        level: usize,
        max_length: usize,
        mut output: Vec<String>,
        start: usize,
    ) -> Vec<String> {
        let length = usize::try_from(array.length).unwrap_or(usize::MAX);
        let mut index = start;
        let mut listed = array.elements.iter().skip(start);
        let mut exhausted = false;
        while output.len() < max_length {
            let Some(element) = listed.next() else {
                exhausted = true;
                break;
            };
            let at = usize::try_from(element.index).unwrap_or(usize::MAX);
            if at != index {
                let Some(empty) = at.checked_sub(index) else {
                    output.push(unreadable("array elements out of order"));
                    return output;
                };
                output.push(format!("<{empty} empty item{}>", plural(empty)));
                index = at;
                if output.len() == max_length {
                    break;
                }
            }
            let text = self.format_slot(&element.slot, level);
            output.push(text);
            index += 1;
        }
        if exhausted && array.listed_below < array.length {
            let listed_below = usize::try_from(array.listed_below).unwrap_or(usize::MAX);
            if let Some(empty) = listed_below.checked_sub(index).filter(|empty| *empty > 0) {
                output.push(format!("<{empty} empty item{}>", plural(empty)));
            }
            output.push(unreadable(&format!(
                "the elements from index {} on",
                array.listed_below
            )));
            return output;
        }
        let remaining = length.saturating_sub(index);
        if output.len() != max_length {
            if remaining > 0 {
                output.push(format!("<{remaining} empty item{}>", plural(remaining)));
            }
        } else if remaining > 0 {
            output.push(remaining_text(remaining));
        }
        output
    }

    /// `formatSet`.
    fn format_set(&mut self, size: u32, values: &[Value], level: usize) -> Vec<String> {
        let size = usize::try_from(size).unwrap_or(usize::MAX);
        let shown = MAX_ARRAY_LENGTH.min(size);
        let mut output = Vec::new();
        self.indentation += 2;
        for value in values.iter().take(shown) {
            let text = self.format_value(value, level);
            output.push(text);
        }
        if values.len() < shown {
            output.push(unreadable("the remaining members"));
        }
        if size > shown {
            output.push(remaining_text(size - shown));
        }
        self.indentation -= 2;
        output
    }

    /// `formatMap`.
    fn format_map(&mut self, size: u32, entries: &[(Value, Value)], level: usize) -> Vec<String> {
        let size = usize::try_from(size).unwrap_or(usize::MAX);
        let shown = MAX_ARRAY_LENGTH.min(size);
        let mut output = Vec::new();
        self.indentation += 2;
        for (key, value) in entries.iter().take(shown) {
            let key = self.format_value(key, level);
            let value = self.format_value(value, level);
            output.push(format!("{key} => {value}"));
        }
        if entries.len() < shown {
            output.push(unreadable("the remaining entries"));
        }
        if size > shown {
            output.push(remaining_text(size - shown));
        }
        self.indentation -= 2;
        output
    }

    /// `formatPromise`.
    fn format_promise(&mut self, state: &PromiseState, level: usize) -> Vec<String> {
        let (value, rejected) = match state {
            PromiseState::Pending => return vec!["<pending>".to_owned()],
            PromiseState::Fulfilled(value) => (value, false),
            PromiseState::Rejected(value) => (value, true),
        };
        self.indentation += 2;
        let text = self.format_value(value, level);
        self.indentation -= 2;
        vec![if rejected {
            format!("<rejected> {text}")
        } else {
            text
        }]
    }

    /// `reduceToSingleString`.
    fn reduce_to_single_string(
        &mut self,
        output: Vec<String>,
        base: &str,
        (open, close): (&str, &str),
        entries: Entries,
        level: usize,
        value: Option<&Object>,
    ) -> String {
        if self.line {
            return self.reduce_to_line(&output, base, (open, close));
        }
        let count = output.len();
        let output = if entries == Entries::ArrayExtras && count > 6 {
            self.group_array_elements(output, value)
        } else {
            output
        };
        let base_space = if base.is_empty() {
            String::new()
        } else {
            format!("{base} ")
        };
        if self.current_depth.saturating_sub(level) < COMPACT && count == output.len() {
            let start = output.len() + self.indentation + js_len(open) + js_len(base) + 10;
            if is_below_break_length(&output, start, base) {
                let joined = output.join(", ");
                if !joined.contains('\n') {
                    return format!("{base_space}{open} {joined} {close}");
                }
            }
        }
        let indentation = format!("\n{}", " ".repeat(self.indentation));
        let joined = output.join(&format!(",{indentation}  "));
        format!("{base_space}{open}{indentation}  {joined}{indentation}{close}")
    }

    /// `reduceToSingleString` with `compact: true` and an infinite
    /// `breakLength`.
    fn reduce_to_line(&self, output: &[String], base: &str, (open, close): (&str, &str)) -> String {
        let space_base = if base.is_empty() {
            String::new()
        } else {
            format!(" {base}")
        };
        // `isBelowBreakLength`. With the infinite `breakLength` early return,
        // entries with line breaks are refused too; before, only `base` was.
        let below = !base.contains('\n')
            && (!self.has(Change::InfiniteBreakLength)
                || !output.iter().any(|entry| entry.contains('\n')));
        if below {
            return format!("{open}{space_base} {} {close}", output.join(", "));
        }
        let indentation = " ".repeat(self.indentation);
        let first = if base.is_empty() && js_len(open) == 1 {
            " ".to_owned()
        } else {
            format!("{space_base}\n{indentation}  ")
        };
        let joined = output.join(&format!(",\n{indentation}  "));
        format!("{open}{first}{joined} {close}")
    }

    /// `groupArrayElements`: an array's entries in aligned columns.
    #[allow(
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "inspect.js computes the columns in doubles from small counts"
    )]
    fn group_array_elements(&mut self, output: Vec<String>, value: Option<&Object>) -> Vec<String> {
        const SEPARATOR_SPACE: usize = 2;
        let mut total_length = 0;
        let mut max_length = 0;
        let mut output_length = output.len();
        if MAX_ARRAY_LENGTH < output.len() {
            // Leaves "... n more items" out.
            output_length -= 1;
        }
        let mut data_len = Vec::with_capacity(output_length);
        for entry in &output[..output_length] {
            let (width, exact) = string_width(entry);
            self.approximate |= !exact;
            data_len.push(width);
            total_length += width + SEPARATOR_SPACE;
            max_length = max_length.max(width);
        }
        let actual_max = max_length + SEPARATOR_SPACE;
        if actual_max * 3 + self.indentation < BREAK_LENGTH
            && (total_length as f64 / actual_max as f64 > 5.0 || max_length <= 6)
        {
            let average_bias =
                (actual_max as f64 - total_length as f64 / output.len() as f64).sqrt();
            let biased_max = (actual_max as f64 - 3.0 - average_bias).max(1.0);
            let columns = ((2.5 * biased_max * output_length as f64).sqrt() / biased_max)
                .round()
                .min(((BREAK_LENGTH - self.indentation) as f64 / actual_max as f64).floor())
                .min((COMPACT * 4) as f64)
                .min(15.0);
            if columns <= 1.0 {
                return output;
            }
            let columns = columns as usize;
            let max_line_length: Vec<usize> = (0..columns)
                .map(|i| {
                    let widest = data_len.iter().skip(i).step_by(columns).max();
                    widest.copied().unwrap_or(0) + SEPARATOR_SPACE
                })
                .collect();
            let numeric = value
                .is_some_and(|value| (0..output.len()).all(|i| self.is_numeric_element(value, i)));
            let mut grouped = Vec::new();
            for row in (0..output_length).step_by(columns) {
                let max = (row + columns).min(output_length);
                let mut line = String::new();
                for j in row..max - 1 {
                    let padding = max_line_length[j - row] + js_len(&output[j]) - data_len[j];
                    let cell = format!("{}, ", output[j]);
                    line.push_str(&if numeric {
                        pad_start(&cell, padding)
                    } else {
                        pad_end(&cell, padding)
                    });
                }
                let j = max - 1;
                if numeric {
                    let padding = max_line_length[j - row] + js_len(&output[j])
                        - data_len[j]
                        - SEPARATOR_SPACE;
                    line.push_str(&pad_start(&output[j], padding));
                } else {
                    line.push_str(&output[j]);
                }
                grouped.push(line);
            }
            if MAX_ARRAY_LENGTH < output.len()
                && let Some(last) = output.get(output_length)
            {
                grouped.push(last.clone());
            }
            return grouped;
        }
        output
    }

    /// Whether `typeof value[i]` is `number` or `bigint`.
    fn is_numeric_element(&mut self, object: &Object, i: usize) -> bool {
        match &object.kind {
            Kind::TypedArray { length, .. } => u64::try_from(i).is_ok_and(|i| i < *length),
            Kind::Array(array) => {
                let Ok(index) = u32::try_from(i) else {
                    return false;
                };
                if index >= array.length {
                    return false;
                }
                if index >= array.listed_below {
                    self.approximate = true;
                    return false;
                }
                match array
                    .elements
                    .binary_search_by_key(&index, |element| element.index)
                {
                    Ok(position) => match &array.elements[position].slot {
                        Slot::Data(Value::Number(_) | Value::BigInt(_)) => true,
                        Slot::Data(_) => false,
                        Slot::Accessor { getter, .. } => {
                            // Node calls the getter.
                            self.approximate |= *getter;
                            false
                        }
                    },
                    Err(_) => false,
                }
            }
            _ => false,
        }
    }
}

/// `getConstructorName` along a chain without constructors: each object's
/// V8 name, then its prototype's text in angle brackets.
fn unnamed_constructor(names: &[String], end: &str, level: usize) -> Option<String> {
    let (first, rest) = names.split_first()?;
    if level > DEPTH {
        return Some(format!("{first} <Complex prototype>"));
    }
    let inner = if rest.is_empty() {
        end.to_owned()
    } else {
        unnamed_constructor(rest, end, level + 1)?
    };
    Some(format!("{first} <{inner}>"))
}

/// Node 26 wraps the target's text in `Proxy(...)` once per proxy.
fn wrap_proxies(mut text: String, proxies: usize) -> String {
    for _ in 0..proxies {
        text = format!("Proxy({text})");
    }
    text
}

/// `isBelowBreakLength`.
fn is_below_break_length(output: &[String], start: usize, base: &str) -> bool {
    let mut total = output.len() + start;
    if total + output.len() > BREAK_LENGTH {
        return false;
    }
    for entry in output {
        total += js_len(entry);
        if total > BREAK_LENGTH {
            return false;
        }
    }
    base.is_empty() || !base.contains('\n')
}

/// `remainingText`.
fn remaining_text(remaining: usize) -> String {
    format!("... {remaining} more item{}", plural(remaining))
}

const fn plural(count: usize) -> &'static str {
    if count > 1 { "s" } else { "" }
}

/// `formatNumber`.
pub fn format_number(value: f64) -> String {
    crate::view::summary::javascript_number(value)
}

/// `formatBigInt`.
fn format_bigint(value: &BigInt) -> String {
    format!("{}n", bigint_to_string(value))
}

/// A JavaScript number of a byte count.
#[allow(
    clippy::cast_precision_loss,
    reason = "JavaScript holds the count as a double"
)]
const fn number(count: u64) -> Value {
    Value::Number(count as f64)
}

/// `formatTypedArray`.
fn format_typed_array(element: ElementType, length: u64, bytes: &[u8]) -> Vec<String> {
    let length = usize::try_from(length).unwrap_or(usize::MAX);
    let shown = MAX_ARRAY_LENGTH.min(length);
    let size = element.size();
    let mut output = Vec::with_capacity(shown + 1);
    for i in 0..shown {
        let Some(bytes) = bytes.get(i * size..(i + 1) * size) else {
            output.push(unreadable(&format!("the elements from index {i} on")));
            return output;
        };
        output.push(element.format(bytes));
    }
    if length > shown {
        output.push(remaining_text(length - shown));
    }
    output
}

/// `formatArrayBuffer`.
fn format_array_buffer(byte_length: u64, bytes: Option<&[u8]>) -> Vec<String> {
    let Some(bytes) = bytes else {
        return vec!["(detached)".to_owned()];
    };
    let length = usize::try_from(byte_length).unwrap_or(usize::MAX);
    let shown = MAX_ARRAY_LENGTH.min(length);
    let Some(bytes) = bytes.get(..shown) else {
        return vec![unreadable("the buffer's bytes")];
    };
    let mut hex: Vec<String> = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
    if length > MAX_ARRAY_LENGTH {
        let remaining = length - MAX_ARRAY_LENGTH;
        hex.push(format!("... {remaining} more byte{}", plural(remaining)));
    }
    vec![format!("[Uint8Contents]: <{}>", hex.join(" "))]
}

impl ElementType {
    /// The constructor's name, which is also the tag.
    pub const fn name(self) -> &'static str {
        match self {
            Self::Int8 => "Int8Array",
            Self::Uint8 => "Uint8Array",
            Self::Uint8Clamped => "Uint8ClampedArray",
            Self::Int16 => "Int16Array",
            Self::Uint16 => "Uint16Array",
            Self::Int32 => "Int32Array",
            Self::Uint32 => "Uint32Array",
            Self::Float16 => "Float16Array",
            Self::Float32 => "Float32Array",
            Self::Float64 => "Float64Array",
            Self::BigInt64 => "BigInt64Array",
            Self::BigUint64 => "BigUint64Array",
        }
    }

    /// The bytes in an element.
    pub const fn size(self) -> usize {
        match self {
            Self::Int8 | Self::Uint8 | Self::Uint8Clamped => 1,
            Self::Int16 | Self::Uint16 | Self::Float16 => 2,
            Self::Int32 | Self::Uint32 | Self::Float32 => 4,
            Self::Float64 | Self::BigInt64 | Self::BigUint64 => 8,
        }
    }

    /// An element's text, from exactly `size` little-endian bytes.
    fn format(self, bytes: &[u8]) -> String {
        let mut raw = [0u8; 8];
        raw[..bytes.len().min(8)].copy_from_slice(&bytes[..bytes.len().min(8)]);
        let [b0, b1, b2, b3, ..] = raw;
        let half = [b0, b1];
        let word = [b0, b1, b2, b3];
        match self {
            Self::Int8 => format_number(f64::from(i8::from_le_bytes([b0]))),
            Self::Uint8 | Self::Uint8Clamped => format_number(f64::from(b0)),
            Self::Int16 => format_number(f64::from(i16::from_le_bytes(half))),
            Self::Uint16 => format_number(f64::from(u16::from_le_bytes(half))),
            Self::Int32 => format_number(f64::from(i32::from_le_bytes(word))),
            Self::Uint32 => format_number(f64::from(u32::from_le_bytes(word))),
            Self::Float16 => format_number(f16_to_f64(u16::from_le_bytes(half))),
            Self::Float32 => format_number(f64::from(f32::from_le_bytes(word))),
            Self::Float64 => format_number(f64::from_le_bytes(raw)),
            Self::BigInt64 => {
                format_bigint(&BigInt::from_i128(i128::from(i64::from_le_bytes(raw))))
            }
            Self::BigUint64 => {
                format_bigint(&BigInt::from_i128(i128::from(u64::from_le_bytes(raw))))
            }
        }
    }
}

/// `split(/(?<=\n)/)`: the units cut after each line feed but the last unit.
fn split_after_newlines(units: &[u16]) -> impl Iterator<Item = &[u16]> {
    let mut rest = units;
    std::iter::from_fn(move || {
        if rest.is_empty() {
            return None;
        }
        let end = rest
            .iter()
            .position(|unit| *unit == u16::from(b'\n'))
            .map_or(rest.len(), |at| at + 1);
        let (line, tail) = rest.split_at(end);
        rest = tail;
        Some(line)
    })
}

/// A value as a template literal converts it, or the text to print when the
/// conversion throws or would call into the program.
fn template_text(value: &Value) -> String {
    to_js_string(value).unwrap_or_else(|fault| match fault {
        Fault::Throws => unsupported("a conversion that throws in Node's inspect"),
        Fault::Visible(text) => text,
    })
}

/// `Symbol.prototype.toString`'s units.
fn symbol_units(symbol: &Symbol) -> Option<Vec<u16>> {
    let mut units: Vec<u16> = "Symbol(".encode_utf16().collect();
    if let Some(description) = &symbol.description {
        if !description.is_complete() {
            return None;
        }
        units.extend_from_slice(description.units());
    }
    units.push(u16::from(b')'));
    Some(units)
}

/// `Symbol.prototype.toString`.
fn symbol_text(symbol: &Symbol) -> Option<String> {
    symbol_units(symbol).map(|units| String::from_utf16_lossy(&units))
}

/// JavaScript's `ToBoolean`.
pub fn truthy(value: &Value) -> bool {
    match value {
        Value::Undefined | Value::Null => false,
        Value::Boolean(value) => *value,
        Value::Number(value) => *value != 0.0 && !value.is_nan(),
        Value::BigInt(value) => value.magnitude.iter().any(|digit| *digit != 0),
        Value::String(value) => value.length() != 0,
        Value::Symbol(_) | Value::Object(_) | Value::Unreadable(_) => true,
    }
}

/// JavaScript's `ToString` where it needs no call into the program.
pub fn to_js_string(value: &Value) -> Result<String, Fault> {
    match value {
        Value::Undefined => Ok("undefined".to_owned()),
        Value::Null => Ok("null".to_owned()),
        Value::Boolean(value) => Ok(value.to_string()),
        Value::Number(value) => Ok(number_to_string(*value)),
        Value::BigInt(value) => Ok(bigint_to_string(value)),
        Value::String(value) => value
            .whole()
            .ok_or_else(|| Fault::Visible(unreadable("a truncated string"))),
        Value::Symbol(_) => Err(Fault::Throws),
        Value::Object(_) => Err(Fault::Visible(unsupported(
            "an object converted to a string, which calls into the program",
        ))),
        Value::Unreadable(reason) => Err(Fault::Visible(unreadable(reason))),
    }
}
