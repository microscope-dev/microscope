//! Trees for the values Node printed in `tests/expected.rs`, checked against
//! that text.

mod expected;

use super::{
    Array, BigInt, Constructor, Element, ElementType, ErrorDetails, Function, FunctionKind,
    Inspected, JsString, Key, Kind, Layout, NodeRelease, Object, ObjectId, PromiseState, Property,
    Slot, Symbol, Value, ValueGraph, ViewRange, inspect,
};
use crate::VirtualAddress;
use expected::{GALLERY, NODE};

/// The fixture whose stacks the gallery's errors show.
const FIXTURE: &str =
    "/home/jcalabro/go/src/github.com/microscope-dev/microscope/tests/fixtures/node/values.js";

/// Node 26's frames below the fixture's top level.
const LOADER: &str = "    at Module._compile (node:internal/modules/cjs/loader:1968:14)
    at Object..js (node:internal/modules/cjs/loader:2108:10)
    at Module.load (node:internal/modules/cjs/loader:1690:32)
    at Module._load (node:internal/modules/cjs/loader:1480:12)
    at wrapModuleLoad (node:internal/modules/cjs/loader:261:19)
    at Module.executeUserEntryPoint [as runMain] (node:internal/modules/run_main:171:5)
    at node:internal/main/run_main_module:33:47";

fn s(text: &str) -> Value {
    Value::String(JsString::from(text))
}

fn units(units: &[u16]) -> Value {
    Value::String(JsString::complete(units.to_vec()))
}

const fn n(value: f64) -> Value {
    Value::Number(value)
}

fn big(value: i128) -> Value {
    Value::BigInt(BigInt::from_i128(value))
}

fn symbol(description: &str) -> Symbol {
    Symbol {
        description: Some(JsString::from(description)),
    }
}

fn named(name: &str) -> Constructor {
    Constructor::Named(JsString::from(name))
}

fn null_prototype(internal: &str) -> Constructor {
    Constructor::NullPrototype {
        internal: JsString::from(internal),
    }
}

fn prop(key: &str, value: Value) -> Property {
    Property {
        key: Key::String(JsString::from(key)),
        slot: Slot::Data(value),
    }
}

fn symbol_prop(description: &str, value: Value) -> Property {
    Property {
        key: Key::Symbol(symbol(description)),
        slot: Slot::Data(value),
    }
}

fn accessor(key: &str, getter: bool, setter: bool) -> Property {
    Property {
        key: Key::String(JsString::from(key)),
        slot: Slot::Accessor { getter, setter },
    }
}

fn props(entries: Vec<(&str, Value)>) -> Vec<Property> {
    entries
        .into_iter()
        .map(|(key, value)| prop(key, value))
        .collect()
}

/// `count` frames `    at g0 (/a.js:0:1)`, ... as `c.js` wrote them.
fn frames(count: usize) -> String {
    (0..count)
        .map(|i| format!("    at g{i} (/a.js:{i}:1)"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// The release of each line whose text `tests/expected.rs` holds.
fn release(major: u32) -> NodeRelease {
    let (minor, patch) = match major {
        22 => (23, 4),
        24 => (22, 1),
        26 => (12, 1),
        _ => panic!("no expected text of Node {major}"),
    };
    NodeRelease {
        major,
        minor,
        patch,
    }
}

/// Builds a graph, handing out addresses.
struct Graph {
    graph: ValueGraph,
    next: u64,
}

impl Graph {
    fn new() -> Self {
        Self {
            graph: ValueGraph::new(),
            next: 0x1000,
        }
    }

    fn add(&mut self, constructor: Constructor, kind: Kind, properties: Vec<Property>) -> Value {
        let id = self.reserve();
        self.fill(id, constructor, kind, properties);
        Value::Object(id)
    }

    /// A row to fill in later, so that what fills it can refer to it.
    fn reserve(&mut self) -> ObjectId {
        self.next += 0x20;
        let object = Object {
            address: VirtualAddress::new(self.next),
            constructor: named("Object"),
            tag: None,
            kind: Kind::Ordinary,
            properties: Vec::new(),
        };
        self.graph.push(object).expect("a row")
    }

    fn fill(
        &mut self,
        id: ObjectId,
        constructor: Constructor,
        kind: Kind,
        properties: Vec<Property>,
    ) {
        let object = self.graph.object_mut(id).expect("a reserved row");
        object.constructor = constructor;
        object.kind = kind;
        object.properties = properties;
    }

    fn tag(&mut self, value: &Value, tag: &str) {
        let Value::Object(id) = value else {
            panic!("only an object has a tag");
        };
        self.graph.object_mut(*id).expect("a row").tag = Some(JsString::from(tag));
    }

    fn plain(&mut self, entries: Vec<(&str, Value)>) -> Value {
        self.add(named("Object"), Kind::Ordinary, props(entries))
    }

    fn instance(&mut self, class: &str, entries: Vec<(&str, Value)>) -> Value {
        self.add(named(class), Kind::Ordinary, props(entries))
    }

    fn array(&mut self, items: Vec<Value>) -> Value {
        self.array_with(named("Array"), items, Vec::new())
    }

    fn array_with(
        &mut self,
        constructor: Constructor,
        items: Vec<Value>,
        properties: Vec<Property>,
    ) -> Value {
        let length = u32::try_from(items.len()).expect("a short array");
        let elements = items
            .into_iter()
            .zip(0..)
            .map(|(value, index)| Element {
                index,
                slot: Slot::Data(value),
            })
            .collect();
        let kind = Kind::Array(Array {
            length,
            elements,
            listed_below: length,
        });
        self.add(constructor, kind, properties)
    }

    fn holey(&mut self, length: u32, items: Vec<(u32, Value)>) -> Value {
        let elements = items
            .into_iter()
            .map(|(index, value)| Element {
                index,
                slot: Slot::Data(value),
            })
            .collect();
        let kind = Kind::Array(Array {
            length,
            elements,
            listed_below: length,
        });
        self.add(named("Array"), kind, Vec::new())
    }

    /// An array at level 3, whose elements the reader leaves out.
    fn array_header(&mut self, constructor: &str, length: u32) -> Value {
        let kind = Kind::Array(Array {
            length,
            elements: Vec::new(),
            listed_below: 0,
        });
        self.add(named(constructor), kind, Vec::new())
    }

    fn numbers(&mut self, items: &[f64]) -> Value {
        self.array(items.iter().copied().map(Value::Number).collect())
    }

    fn strings(&mut self, items: &[&str]) -> Value {
        self.array(items.iter().copied().map(s).collect())
    }

    fn function(&mut self, constructor: &str, kind: FunctionKind, name: Value) -> Value {
        self.add(
            named(constructor),
            Kind::Function(Function { kind, name }),
            Vec::new(),
        )
    }

    fn plain_function(&mut self, name: &str) -> Value {
        self.function("Function", FunctionKind::Plain, s(name))
    }

    #[allow(clippy::too_many_arguments, reason = "an error has this many parts")]
    fn error_with(
        &mut self,
        constructor: Constructor,
        stack: Value,
        name: Value,
        message: Value,
        properties: Vec<Property>,
        hidden: Vec<Property>,
    ) -> Value {
        let details = ErrorDetails {
            stack,
            name,
            message,
            hidden,
        };
        self.add(constructor, Kind::Error(details), properties)
    }

    /// `c.js`'s `mk(stack)`: an `Error('x')` whose stack is `stack`.
    fn mk(&mut self, stack: &str) -> Value {
        self.error_with(
            named("Error"),
            s(stack),
            s("Error"),
            s("x"),
            Vec::new(),
            Vec::new(),
        )
    }

    /// `mk(stack)` with a non-enumerable own `cause`.
    fn mk_cause(&mut self, stack: &str, cause: Value) -> Value {
        self.error_with(
            named("Error"),
            s(stack),
            s("Error"),
            s("x"),
            Vec::new(),
            vec![prop("cause", cause)],
        )
    }

    fn map(&mut self, entries: Vec<(Value, Value)>) -> Value {
        let size = u32::try_from(entries.len()).expect("a short map");
        self.add(named("Map"), Kind::Map { size, entries }, Vec::new())
    }

    fn set(&mut self, values: Vec<Value>) -> Value {
        let size = u32::try_from(values.len()).expect("a short set");
        self.add(named("Set"), Kind::Set { size, values }, Vec::new())
    }

    fn date(&mut self, time: f64) -> Value {
        self.add(named("Date"), Kind::Date { time }, Vec::new())
    }

    fn regexp(&mut self, source: &str, flags: &str) -> Value {
        let kind = Kind::RegExp {
            source: JsString::from(source),
            flags: JsString::from(flags),
        };
        self.add(named("RegExp"), kind, Vec::new())
    }

    fn typed(&mut self, element: ElementType, length: u64, bytes: Vec<u8>) -> Value {
        let kind = Kind::TypedArray {
            element,
            length,
            bytes,
        };
        self.add(named(element.name()), kind, Vec::new())
    }

    fn buffer(&mut self, bytes: Vec<u8>) -> Value {
        let byte_length = u64::try_from(bytes.len()).expect("a short buffer");
        let kind = Kind::ArrayBuffer {
            shared: false,
            byte_length,
            bytes: Some(bytes.into_iter().take(100).collect()),
        };
        self.add(named("ArrayBuffer"), kind, Vec::new())
    }

    fn boxed(&mut self, constructor: &str, primitive: Value) -> Value {
        self.add(named(constructor), Kind::Boxed(primitive), Vec::new())
    }

    fn promise(&mut self, state: PromiseState) -> Value {
        self.add(named("Promise"), Kind::Promise(state), Vec::new())
    }

    fn proxy(&mut self, target: Value) -> Value {
        self.add(named("Object"), Kind::Proxy { target }, Vec::new())
    }

    fn inspect(&self, value: &Value, major: u32) -> Inspected {
        inspect(&self.graph, value, release(major), Layout::Default)
    }

    fn line(&self, value: &Value, node: NodeRelease) -> String {
        inspect(&self.graph, value, node, Layout::Line).text
    }
}

fn bytes_le<const N: usize>(values: impl IntoIterator<Item = [u8; N]>) -> Vec<u8> {
    values.into_iter().flatten().collect()
}

/// The value `b.js`, `c.js`, or `d.js` printed under `label`.
#[allow(clippy::too_many_lines, reason = "one tree per value")]
fn build(g: &mut Graph, label: &str) -> Value {
    match label {
        "mixedNumbers" => g.numbers(&[
            1.0,
            22.0,
            333.0,
            -4.0,
            5.5,
            66.0,
            7777.0,
            8.0,
            99.0,
            1e21,
            -0.0,
            f64::NAN,
            0.1,
            123e-7,
        ]),
        "mixedStrings" => g.strings(&[
            "a", "bb", "ccc", "dddd", "e", "ff", "ggg", "hhhhhhh", "i", "jj",
        ]),
        "mixedTypes" => {
            let items = vec![
                n(1.0),
                s("two"),
                n(3.0),
                n(4.0),
                n(5.0),
                n(6.0),
                n(7.0),
                n(8.0),
            ];
            g.array(items)
        }
        "str16" => g.plain(vec![("s", s("abcdefg\nhijklmno"))]),
        "str17" => g.plain(vec![("s", s("abcdefg\nhijklmnop"))]),
        "str74" => g.plain(vec![(
            "s",
            s(&format!("{}\n{}", "a".repeat(36), "b".repeat(37))),
        )]),
        "str75" => g.plain(vec![(
            "s",
            s(&format!("{}\n{}", "a".repeat(36), "b".repeat(38))),
        )]),
        "str75top" => s(&format!("{}\n{}", "a".repeat(36), "b".repeat(38))),
        "str77top" => s(&format!("{}\n{}\n", "a".repeat(36), "b".repeat(39))),
        "keys" => g.plain(vec![
            ("12", n(8.0)),
            ("a-b", n(1.0)),
            ("1x", n(2.0)),
            ("it's", n(3.0)),
            ("__proto__x", n(4.0)),
            ("__proto__", n(5.0)),
            ("", n(6.0)),
            ("é", n(7.0)),
            ("a\"b'c", n(9.0)),
        ]),
        "negzero" => g.numbers(&[-0.0, 0.0, -0.0]),
        "numbers" => g.numbers(&[
            1e21,
            1e20,
            123_456_789_012_345_680_000.0,
            123e-7,
            1.23e-7,
            1e-6,
            1e-7,
            0.000_001_234,
            1.5e300,
            -1e-7,
            f64::from_bits(1),
            f64::MAX,
            0.1 + 0.2,
            100.0,
            1e100,
        ]),
        "depth2" => {
            let b = g.plain(vec![("c", n(1.0))]);
            let a = g.plain(vec![("b", b)]);
            g.plain(vec![("a", a)])
        }
        "depth3" => {
            let c = g.plain(vec![]);
            let b = g.plain(vec![("c", c)]);
            let a = g.plain(vec![("b", b)]);
            g.plain(vec![("a", a)])
        }
        "depth3b" => {
            // Level 3: the reader leaves the property's value out.
            let c = g.plain(vec![(
                "d",
                Value::Unreadable("beyond the depth".to_owned()),
            )]);
            let b = g.plain(vec![("c", c)]);
            let a = g.plain(vec![("b", b)]);
            g.plain(vec![("a", a)])
        }
        "depth3arr" => {
            let inner = g.array_header("Array", 1);
            let two = g.array(vec![inner]);
            let one = g.array(vec![two]);
            g.array(vec![one])
        }
        "cycleTwice" => {
            let a = g.reserve();
            g.fill(
                a,
                named("Object"),
                Kind::Ordinary,
                props(vec![("x", Value::Object(a)), ("y", Value::Object(a))]),
            );
            Value::Object(a)
        }
        "twoCycles" => {
            let a = g.reserve();
            let b = g.reserve();
            let (va, vb) = (Value::Object(a), Value::Object(b));
            g.fill(
                a,
                named("Object"),
                Kind::Ordinary,
                props(vec![("name", s("a")), ("b", vb.clone())]),
            );
            g.fill(
                b,
                named("Object"),
                Kind::Ordinary,
                props(vec![
                    ("name", s("b")),
                    ("a", va.clone()),
                    ("self", vb.clone()),
                ]),
            );
            g.plain(vec![("a", va), ("b", vb)])
        }
        "cycleArr" => {
            let o = g.reserve();
            let a = g.array(vec![n(1.0), Value::Object(o)]);
            g.fill(
                o,
                named("Object"),
                Kind::Ordinary,
                props(vec![("list", a.clone()), ("again", a)]),
            );
            Value::Object(o)
        }
        "holes" => g.holey(8, vec![(2, n(1.0)), (6, n(2.0))]),
        "sparseLong" => {
            let items = (0..300).step_by(2).map(|i| (i, n(f64::from(i)))).collect();
            g.holey(299, items)
        }
        "arrExtras" => {
            let items = vec![n(1.0), n(2.0), n(3.0)];
            let extras = vec![prop("foo", s("bar")), symbol_prop("s", n(1.0))];
            g.array_with(named("Array"), items, extras)
        }
        "arrLongExtras" => {
            let items = (0..120).map(|i| n(f64::from(i))).collect();
            g.array_with(named("Array"), items, vec![prop("foo", s("bar"))])
        }
        "arr99Extras" => {
            let items = (0..99).map(|i| n(f64::from(i * 1000))).collect();
            let extras = vec![prop("foo", s("bar")), prop("baz", n(2.0))];
            g.array_with(named("Array"), items, extras)
        }
        "emptyHoley" => g.holey(5, vec![]),
        "sym" => g.array(vec![
            Value::Symbol(Symbol { description: None }),
            Value::Symbol(symbol("a b")),
            Value::Symbol(symbol("it's")),
        ]),
        "symKeys" => {
            let blank = Property {
                key: Key::Symbol(Symbol { description: None }),
                slot: Slot::Data(n(1.0)),
            };
            let properties = vec![
                blank,
                symbol_prop("it's", n(2.0)),
                symbol_prop("\n", n(3.0)),
            ];
            g.add(named("Object"), Kind::Ordinary, properties)
        }
        "strings" => g.array(vec![
            s("it's"),
            s("say \"hi\""),
            s("both ' and \""),
            s("all ' \" `"),
            s("all ' \" ${"),
            s("\u{0}\u{7f}\u{9f}\u{a0}"),
            units(&[0xd800, u16::from(b'x')]),
            units(&[u16::from(b'x'), 0xdc00]),
            s("😀"),
            s("tab\there"),
        ]),
        "bigints" => g.array(vec![
            big(0),
            big(-1),
            big(1 << 64),
            Value::BigInt(BigInt::new(true, vec![u64::MAX, u64::MAX])),
        ]),
        "map" => {
            let key = g.plain(vec![("a", n(1.0))]);
            let c = g.plain(vec![("c", n(1.0))]);
            let b = g.plain(vec![("b", c)]);
            let inner = g.map(vec![(n(1.0), b)]);
            let pair = g.numbers(&[1.0, 2.0]);
            let set = g.set(vec![pair]);
            g.map(vec![(key, inner), (s("k"), set)])
        }
        "weakset" => g.add(named("WeakSet"), Kind::WeakSet, vec![]),
        "dates" => {
            let mut dates: Vec<Value> = [
                0.0,
                -1.0,
                -62_198_755_200_000.0,
                253_402_300_800_000.0,
                8.64e15,
                -8.64e15,
            ]
            .into_iter()
            .map(|time| g.date(time))
            .collect();
            let keyed = g.add(
                named("Date"),
                Kind::Date { time: 0.0 },
                vec![prop("k", n(1.0))],
            );
            dates.push(keyed);
            dates.push(g.date(f64::NAN));
            g.array(dates)
        }
        "regexps" => {
            let items = vec![
                g.regexp("a\\/b", ""),
                g.regexp("(?:)", ""),
                g.regexp("\\n", ""),
                g.regexp("x", "dgimsy"),
                g.regexp("x", "v"),
                g.add(
                    named("RegExp"),
                    Kind::RegExp {
                        source: JsString::from("re"),
                        flags: JsString::from(""),
                    },
                    vec![prop("k", n(1.0))],
                ),
            ];
            g.array(items)
        }
        "functions" => {
            let items = vec![
                g.function(
                    "AsyncGeneratorFunction",
                    FunctionKind::AsyncGenerator,
                    s("ag"),
                ),
                g.function("AsyncFunction", FunctionKind::Async, s("")),
                g.function("GeneratorFunction", FunctionKind::Generator, s("")),
                g.function("Function", FunctionKind::Class { extends: s("") }, s("")),
                g.function(
                    "Function",
                    FunctionKind::Class {
                        extends: s("Array"),
                    },
                    s("A"),
                ),
                g.function(
                    "Function",
                    FunctionKind::Class {
                        extends: s("Object"),
                    },
                    s(""),
                ),
                g.add(
                    named("Function"),
                    Kind::Function(Function {
                        kind: FunctionKind::Plain,
                        name: s("f"),
                    }),
                    vec![prop("k", n(1.0))],
                ),
                g.function("Function", FunctionKind::Plain, n(42.0)),
                g.plain_function("f"),
            ];
            g.array(items)
        }
        "classNull" => g.function("Function", FunctionKind::Class { extends: s("") }, s("N")),
        "boxed" => {
            let items = vec![
                g.boxed("Number", n(-0.0)),
                g.boxed("String", s("ab")),
                g.boxed("Boolean", Value::Boolean(false)),
                g.boxed("BigInt", big(1)),
                g.boxed("Symbol", Value::Symbol(symbol("s"))),
                g.add(
                    named("String"),
                    Kind::Boxed(s("x")),
                    vec![prop("k", n(1.0))],
                ),
                g.add(null_prototype("Object"), Kind::Ordinary, vec![]),
            ];
            g.array(items)
        }
        "typed" => {
            let items = vec![
                g.typed(ElementType::Int8, 2, vec![0xff, 0x7f]),
                g.typed(ElementType::Uint8Clamped, 1, vec![255]),
                g.typed(ElementType::Int16, 1, (-300i16).to_le_bytes().to_vec()),
                g.typed(ElementType::Uint32, 1, u32::MAX.to_le_bytes().to_vec()),
                g.typed(
                    ElementType::Float32,
                    2,
                    bytes_le([0.1f32.to_le_bytes(), f32::INFINITY.to_le_bytes()]),
                ),
                g.typed(ElementType::BigUint64, 1, u64::MAX.to_le_bytes().to_vec()),
                g.typed(ElementType::Float64, 0, vec![]),
                g.typed(ElementType::Uint8, 150, vec![0; 100]),
            ];
            g.array(items)
        }
        "f16" => g.typed(
            ElementType::Float16,
            3,
            bytes_le([
                0x2e66u16.to_le_bytes(),
                0x7bffu16.to_le_bytes(),
                0x8000u16.to_le_bytes(),
            ]),
        ),
        "typedLong" => {
            let bytes = bytes_le((0..100u16).map(|i| (i * 300).to_le_bytes()));
            g.typed(ElementType::Uint16, 120, bytes)
        }
        "ab" => {
            let empty = g.buffer(vec![]);
            let long = g.buffer(vec![0; 120]);
            let shared = g.add(
                named("SharedArrayBuffer"),
                Kind::ArrayBuffer {
                    shared: true,
                    byte_length: 2,
                    bytes: Some(vec![0, 0]),
                },
                vec![],
            );
            g.array(vec![empty, long, shared])
        }
        "dataview" => {
            let buffer = g.buffer(vec![0; 4]);
            let view = Some(ViewRange {
                byte_length: 2,
                byte_offset: 1,
            });
            g.add(named("DataView"), Kind::DataView { view, buffer }, vec![])
        }
        "buffer" => {
            let buffer = g.add(
                named("Buffer"),
                Kind::TypedArray {
                    element: ElementType::Uint8,
                    length: 2,
                    bytes: b"hi".to_vec(),
                },
                vec![],
            );
            g.tag(&buffer, "Uint8Array");
            buffer
        }
        "promises" => {
            let promise = g.plain_function("Promise");
            let c = g.plain(vec![]);
            let b = g.plain(vec![("c", c)]);
            let a = g.plain(vec![("b", b)]);
            let object = g.plain(vec![("a", a)]);
            let items = vec![
                g.promise(PromiseState::Fulfilled(promise)),
                g.promise(PromiseState::Rejected(n(3.0))),
                g.promise(PromiseState::Fulfilled(object)),
            ];
            g.array(items)
        }
        "nullproto" => {
            let object = g.add(null_prototype("Object"), Kind::Ordinary, vec![]);
            let array = g.array_with(null_prototype("Array"), vec![n(1.0), n(2.0)], vec![]);
            let map = g.add(
                null_prototype("Map"),
                Kind::Map {
                    size: 1,
                    entries: vec![(n(1.0), n(2.0))],
                },
                vec![],
            );
            let typed = g.add(
                null_prototype("Uint8Array"),
                Kind::TypedArray {
                    element: ElementType::Uint8,
                    length: 2,
                    bytes: vec![0, 0],
                },
                vec![],
            );
            g.array(vec![object, array, map, typed])
        }
        "tagged" => {
            let own = g.add(
                named("Object"),
                Kind::Ordinary,
                vec![prop("a", n(1.0)), symbol_prop("Symbol.toStringTag", s("T"))],
            );
            let mut items = vec![own];
            for (class, tag) in [
                ("Object", "T"),
                ("X", "Y"),
                ("X", "a.X"),
                ("Xy", "X"),
                ("Xy", "y"),
            ] {
                let instance = g.instance(class, vec![]);
                g.tag(&instance, tag);
                items.push(instance);
            }
            g.array(items)
        }
        "getters" => {
            let properties = vec![
                accessor("a", true, false),
                accessor("b", false, true),
                accessor("c", true, true),
            ];
            g.add(named("Object"), Kind::Ordinary, properties)
        }
        "mapLong" => {
            let entries = (0..100)
                .map(|i| (n(f64::from(i)), n(f64::from(i))))
                .collect();
            g.add(named("Map"), Kind::Map { size: 102, entries }, vec![])
        }
        "setLong" => {
            let values = (0..100).map(|i| n(f64::from(i))).collect();
            g.add(named("Set"), Kind::Set { size: 102, values }, vec![])
        }
        "longObj" => g.plain(vec![
            ("aaaaaaaaaa", s("bbbbbbbbbbbbbbbbbbbb")),
            ("cccccccccc", s("dddddddddddddddddddd")),
            ("eeeeeeeee", s("fffffffffffffff")),
        ]),
        "manyKeys" => {
            let entries = ["a", "b", "c", "d", "e", "f", "g", "h"]
                .into_iter()
                .zip(1..)
                .map(|(key, value)| (key, n(f64::from(value))))
                .collect();
            g.plain(entries)
        }
        "nestedCompact" => {
            let c = g.plain(vec![("c", n(4.0))]);
            let inner = g.array(vec![n(3.0), c]);
            let b = g.plain(vec![("b", inner)]);
            let a = g.array(vec![n(1.0), n(2.0), b]);
            g.plain(vec![("a", a), ("d", s("x"))])
        }
        "arrOfObjs" => {
            let items = ["a", "b", "c", "d", "e", "f", "g"]
                .into_iter()
                .zip(1..)
                .map(|(key, value)| g.plain(vec![(key, n(f64::from(value)))]))
                .collect();
            g.array(items)
        }
        "wide" => g.strings(&["日本", "語", "abc", "d", "e", "f", "g"]),
        "noFrames" => g.mk("Error: x"),
        "noFramesMsg" => g.error_with(
            named("Error"),
            s("Error: boom"),
            s("Error"),
            s("boom"),
            vec![],
            vec![],
        ),
        "renamed" => g.error_with(
            named("Error"),
            s("Error: x\n    at f (/a.js:1:1)"),
            s("Custom"),
            s("x"),
            vec![prop("name", s("Custom"))],
            vec![],
        ),
        "renamedError" => g.error_with(
            named("Error"),
            s("Error: x\n    at f (/a.js:1:1)"),
            s("FooError"),
            s("x"),
            vec![],
            vec![],
        ),
        "subclassStack" => g.error_with(
            named("MyError"),
            s("Error: m\n    at f (/a.js:1:1)"),
            s("Error"),
            s("x"),
            vec![],
            vec![],
        ),
        "nullProtoErr" | "nullProtoWeird" | "nullProtoErrOnly" => {
            let stack = match label {
                "nullProtoErr" => "Error: m\n    at f (/a.js:1:1)",
                "nullProtoWeird" => "weird text",
                _ => "SomethingError",
            };
            g.error_with(
                null_prototype("Error"),
                s(stack),
                Value::Undefined,
                s("x"),
                vec![],
                vec![],
            )
        }
        "withKeys" => {
            let extra = g.plain(vec![("a", n(1.0))]);
            g.error_with(
                named("Error"),
                s("Error: x\n    at f (/a.js:1:1)"),
                s("Error"),
                s("x"),
                props(vec![("code", s("E1")), ("extra", extra)]),
                vec![],
            )
        }
        "withKeysNoFrames" => g.error_with(
            named("Error"),
            s("Error: x"),
            s("Error"),
            s("x"),
            vec![prop("code", s("E1"))],
            vec![],
        ),
        "msgKey" => g.error_with(
            named("Error"),
            s("Error: hello\n    at f (/a.js:1:1)"),
            s("Error"),
            s("hello"),
            props(vec![
                ("message", s("hello")),
                ("name", s("Error")),
                ("foo", n(1.0)),
            ]),
            vec![],
        ),
        "msgKeyNotInStack" => g.error_with(
            named("Error"),
            s("Error: hello\n    at f (/a.js:1:1)"),
            s("Error"),
            s("other"),
            vec![prop("message", s("other"))],
            vec![],
        ),
        "emptyStack" => g.error_with(named("Error"), s(""), s("Error"), s("m"), vec![], vec![]),
        "undefStack" => g.error_with(
            named("Error"),
            Value::Undefined,
            s("Error"),
            s("m"),
            vec![],
            vec![],
        ),
        "numStack" => g.error_with(named("Error"), n(5.0), s("Error"), s("x"), vec![], vec![]),
        "causeNonErr" => g.mk_cause("Error: x\n    at f (/a.js:1:1)", s("str")),
        "causeEnumerable" => g.error_with(
            named("Error"),
            s("Error: x\n    at f (/a.js:1:1)"),
            s("Error"),
            s("x"),
            vec![prop("cause", n(7.0))],
            vec![],
        ),
        "causeMatch" | "deepCause" => {
            let shared = frames(8);
            let cause = g.mk(&format!("RangeError: in\n    at c (/c.js:1:1)\n{shared}"));
            let error = g.mk_cause(
                &format!("Error: out\n    at e (/e.js:1:1)\n{shared}"),
                cause,
            );
            if label == "deepCause" {
                let b = g.plain(vec![("b", error)]);
                g.plain(vec![("a", b)])
            } else {
                error
            }
        }
        "causeMatchShort" => {
            let cause = g.mk(&format!("RangeError: in\n{}", frames(4)));
            g.mk_cause(
                &format!("Error: out\n    at e (/e.js:1:1)\n{}", frames(4)),
                cause,
            )
        }
        "recursion" | "recursionSingle" | "recursionMixed" => {
            let mut lines = vec!["    at top (/a.js:1:1)"];
            match label {
                "recursion" => {
                    for _ in 0..6 {
                        lines.extend(["    at a (/a.js:2:1)", "    at b (/a.js:3:1)"]);
                    }
                }
                "recursionSingle" => lines.extend(["    at r (/a.js:2:1)"; 12]),
                _ => {
                    for _ in 0..3 {
                        lines.extend([
                            "    at a (/a.js:2:1)",
                            "    at b (/a.js:3:1)",
                            "    at c (/a.js:3:1)",
                        ]);
                    }
                    lines.extend(["    at d (/a.js:2:1)"; 4]);
                }
            }
            lines.push("    at main (/a.js:4:1)");
            g.mk(&format!("Error: deep\n{}", lines.join("\n")))
        }
        "aggregate" => {
            let one = g.mk("Error: one");
            let errors = g.array(vec![one, n(2.0)]);
            g.error_with(
                named("AggregateError"),
                s("AggregateError: many\n    at f (/a.js:1:1)"),
                s("AggregateError"),
                s("many"),
                vec![],
                vec![prop("errors", errors)],
            )
        }
        "suppressed" => {
            let error = g.mk("Error: e1");
            let suppressed = g.mk("Error: e2");
            g.error_with(
                named("SuppressedError"),
                s("SuppressedError: sup\n    at f (/a.js:1:1)"),
                s("SuppressedError"),
                s("sup"),
                vec![],
                vec![prop("error", error), prop("suppressed", suppressed)],
            )
        }
        "errInArray" => {
            let error = g.mk("Error: x\n    at f (/a.js:1:1)");
            g.array(vec![error])
        }
        "errInMap" => {
            let error = g.mk("TypeError: t\n    at f (/a.js:1:1)");
            g.map(vec![(n(1.0), error)])
        }
        "errTag" => {
            let error = g.mk("Error: x\n    at f (/a.js:1:1)");
            g.tag(&error, "Tagged");
            error
        }
        "errNameNotErrorSuffix" => g.error_with(
            named("Error"),
            s("Oops: x\n    at f (/a.js:1:1)"),
            s("Oops"),
            s("x"),
            vec![prop("name", s("Oops"))],
            vec![],
        ),
        "proxyTop" => {
            let target = g.plain(vec![("a", n(1.0))]);
            g.proxy(target)
        }
        "proxyRevoked" => g.proxy(Value::Null),
        "proxyNestedRevoked" => {
            let revoked = g.proxy(Value::Null);
            g.proxy(revoked)
        }
        "proxyArrInObj" => {
            let target = g.numbers(&[1.0, 2.0]);
            let proxy = g.proxy(target);
            g.plain(vec![("x", proxy)])
        }
        "proxyCycle" => {
            let o = g.reserve();
            let px = g.proxy(Value::Object(o));
            g.fill(
                o,
                named("Object"),
                Kind::Ordinary,
                props(vec![("self", px.clone())]),
            );
            g.plain(vec![("o", Value::Object(o)), ("px", px)])
        }
        "proxyFunction" => {
            let target = g.plain_function("f");
            g.proxy(target)
        }
        "detachedView" => {
            let buffer = g.add(
                named("ArrayBuffer"),
                Kind::ArrayBuffer {
                    shared: false,
                    byte_length: 0,
                    bytes: None,
                },
                vec![],
            );
            let view = g.add(
                named("DataView"),
                Kind::DataView { view: None, buffer },
                vec![],
            );
            let list = g.numbers(&[1.0, 2.0, 3.0]);
            let b = g.plain(vec![("c", list)]);
            g.plain(vec![("a", view), ("b", b), ("long", s(&"x".repeat(70)))])
        }
        "complexTop" | "complexOne" | "complexKeys" | "complexDeep" => {
            let depth = if matches!(label, "complexOne" | "complexKeys") {
                1
            } else {
                2
            };
            let constructor = Constructor::Unnamed {
                names: vec![JsString::from("Object"); depth],
                end: JsString::from("Object"),
                end_has_keys: label == "complexKeys",
            };
            let object = g.add(constructor, Kind::Ordinary, vec![]);
            if label == "complexDeep" {
                let b = g.plain(vec![("c", object)]);
                let a = g.plain(vec![("b", b)]);
                g.plain(vec![("a", a)])
            } else {
                object
            }
        }
        "headers" => {
            // Every value here is at level 3, so the reader gives only the
            // headers: no elements, entries, bytes, or property values.
            let skipped = || Value::Unreadable("beyond the depth".to_owned());
            let keyed = || vec![prop("k", Value::Unreadable("beyond the depth".to_owned()))];
            let entries = vec![
                ("c", g.array_header("Array", 1)),
                (
                    "e",
                    g.add(
                        named("Map"),
                        Kind::Map {
                            size: 1,
                            entries: vec![],
                        },
                        vec![],
                    ),
                ),
                ("f", g.mk("Error: x\n    at f (/a.js:1:1)")),
                ("g", g.plain_function("f")),
                ("h", g.regexp("x", "g")),
                ("i", g.date(0.0)),
                ("j", g.typed(ElementType::Uint8, 2, vec![])),
                ("k", g.promise(PromiseState::Fulfilled(skipped()))),
                ("l", g.boxed("Number", n(3.0))),
                (
                    "m",
                    g.add(
                        null_prototype("Object"),
                        Kind::Ordinary,
                        vec![prop("a", skipped())],
                    ),
                ),
                ("n", g.array_header("Array", 0)),
                ("o", g.plain(vec![])),
                (
                    "p",
                    g.add(
                        named("Map"),
                        Kind::Map {
                            size: 0,
                            entries: vec![],
                        },
                        vec![],
                    ),
                ),
                (
                    "q",
                    g.error_with(
                        named("Error"),
                        s("Error: y\n    at f (/a.js:1:1)"),
                        s("Error"),
                        s("x"),
                        vec![prop("z", skipped())],
                        vec![],
                    ),
                ),
                ("r", g.mk_cause("Error: c\n    at f (/a.js:1:1)", skipped())),
                ("s", g.add(named("WeakMap"), Kind::WeakMap, vec![])),
                (
                    "t",
                    g.add(
                        named("ArrayBuffer"),
                        Kind::ArrayBuffer {
                            shared: false,
                            byte_length: 2,
                            bytes: Some(vec![]),
                        },
                        vec![],
                    ),
                ),
                (
                    "u",
                    g.add(
                        named("DataView"),
                        Kind::DataView {
                            view: None,
                            buffer: skipped(),
                        },
                        vec![],
                    ),
                ),
                (
                    "v",
                    g.add(
                        named("Function"),
                        Kind::Function(Function {
                            kind: FunctionKind::Plain,
                            name: s("g"),
                        }),
                        keyed(),
                    ),
                ),
                (
                    "w",
                    g.add(
                        named("RegExp"),
                        Kind::RegExp {
                            source: JsString::from("y"),
                            flags: JsString::from(""),
                        },
                        keyed(),
                    ),
                ),
                ("x", g.add(named("Date"), Kind::Date { time: 0.0 }, keyed())),
                ("y", g.add(named("Number"), Kind::Boxed(n(1.0)), keyed())),
                (
                    "z",
                    g.add(
                        named("Set"),
                        Kind::Set {
                            size: 1,
                            values: vec![],
                        },
                        vec![],
                    ),
                ),
                ("aa", g.instance("A", vec![])),
                ("ab", g.array_header("B", 0)),
                ("ac", {
                    let target = g.plain(vec![("a", skipped())]);
                    g.proxy(target)
                }),
                ("ad", g.boxed("String", s("ab"))),
                ("ae", g.add(named("String"), Kind::Boxed(s("ab")), keyed())),
                (
                    "af",
                    g.add(null_prototype("Object"), Kind::Ordinary, vec![]),
                ),
                ("ag", g.instance("C", vec![])),
                ("ah", g.add(named("D"), Kind::Ordinary, keyed())),
            ];
            let b = g.plain(entries);
            let a = g.plain(vec![("b", b)]);
            g.plain(vec![("a", a)])
        }
        "nestedSplit" => {
            let a = g.plain(vec![("s", s(&"line one is long enough\n".repeat(4)))]);
            g.plain(vec![("a", a)])
        }
        "mapSplit" => {
            let text = format!("{}\n{}", "a".repeat(40), "b".repeat(40));
            g.map(vec![(s("k"), s(&text))])
        }
        "longKeyStr" => g.plain(vec![(&"x".repeat(100), n(1.0))]),
        "circularDeep" | "circularBeyond" => {
            let o = g.reserve();
            let innermost = if label == "circularDeep" {
                g.plain(vec![("back", Value::Object(o))])
            } else {
                let d = g.plain(vec![("back", Value::Object(o))]);
                g.plain(vec![("d", d)])
            };
            let b = g.plain(vec![("c", innermost)]);
            let a = g.plain(vec![("b", b)]);
            g.fill(o, named("Object"), Kind::Ordinary, props(vec![("a", a)]));
            Value::Object(o)
        }
        "refTwiceSiblings" => {
            let o = g.reserve();
            g.fill(
                o,
                named("Object"),
                Kind::Ordinary,
                props(vec![("me", Value::Object(o))]),
            );
            g.array(vec![Value::Object(o), Value::Object(o)])
        }
        "boxedNull" => g.add(null_prototype("Number"), Kind::Boxed(n(4.0)), vec![]),
        "boxedSub" => g.boxed("N", n(5.0)),
        "fnNull" => g.add(
            null_prototype("Function"),
            Kind::Function(Function {
                kind: FunctionKind::Plain,
                name: s("f"),
            }),
            vec![],
        ),
        "classNullProto" => g.add(
            null_prototype("Function"),
            Kind::Function(Function {
                kind: FunctionKind::Class {
                    extends: Value::Undefined,
                },
                name: s("K"),
            }),
            vec![],
        ),
        "fnTag" => g.add(
            named("Function"),
            Kind::Function(Function {
                kind: FunctionKind::Plain,
                name: s("f"),
            }),
            vec![symbol_prop("Symbol.toStringTag", s("T"))],
        ),
        "fnSub" => {
            let function = g.plain_function("f");
            g.tag(&function, "T");
            function
        }
        "arrSub" => g.array_with(named("L"), vec![], vec![]),
        "arrTag" => {
            let array = g.numbers(&[1.0]);
            g.tag(&array, "T");
            array
        }
        "emptyArrKeys" => g.array_with(named("Array"), vec![], vec![prop("k", n(1.0))]),
        "setInSet" => {
            let innermost = g.add(
                named("Set"),
                Kind::Set {
                    size: 1,
                    values: vec![],
                },
                vec![],
            );
            let two = g.set(vec![innermost]);
            let one = g.set(vec![two]);
            g.set(vec![one])
        }
        "wideArray" => g.strings(&["é", "ü", "ö", "ä", "x", "y", "z", "w"]),
        "getterArr" => {
            let elements = vec![
                Element {
                    index: 0,
                    slot: Slot::Data(n(1.0)),
                },
                Element {
                    index: 1,
                    slot: Slot::Data(n(2.0)),
                },
                Element {
                    index: 2,
                    slot: Slot::Accessor {
                        getter: true,
                        setter: false,
                    },
                },
            ];
            let kind = Kind::Array(Array {
                length: 3,
                elements,
                listed_below: 3,
            });
            g.add(named("Array"), kind, vec![])
        }
        "bigArrNums" => {
            let items: Vec<f64> = (0..26).map(|i| f64::from(i * 7919 % 1000)).collect();
            g.numbers(&items)
        }
        "dateKeysNull" => g.add(null_prototype("Date"), Kind::Date { time: 0.0 }, vec![]),
        "regexpNull" => g.add(
            null_prototype("RegExp"),
            Kind::RegExp {
                source: JsString::from("a"),
                flags: JsString::from(""),
            },
            vec![],
        ),
        "promiseNested" => g.promise(PromiseState::Fulfilled(n(1.0))),
        "errNoName" => g.error_with(
            named("Error"),
            s("Error: x\n    at f (/a.js:1:1)"),
            s(""),
            s("x"),
            vec![],
            vec![],
        ),
        "bigintTyped" => g.typed(
            ElementType::BigInt64,
            2,
            bytes_le([i64::MIN.to_le_bytes(), i64::MAX.to_le_bytes()]),
        ),
        "i8Typed" => g.typed(ElementType::Int8, 8, vec![0x80, 0x7f, 0, 1, 2, 3, 4, 5]),
        other => panic!("no tree for {other}"),
    }
}

/// Labels whose text in some version is not a formatted value, because that
/// version lacks the type or threw.
fn node_lacks(label: &str, major: u32) -> bool {
    matches!(
        (label, major),
        ("f16" | "suppressed", 22) | ("proxyNestedRevoked", 22 | 24)
    )
}

/// The labels whose arrays hold characters whose widths Node takes from ICU.
fn needs_icu(label: &str) -> bool {
    matches!(label, "wide" | "strings")
}

#[test]
fn values_print_as_node_prints_them() {
    let mut failures = Vec::new();
    for (label, text26, text24, text22) in NODE {
        let text24 = text24.unwrap_or(text26);
        let text22 = text22.unwrap_or(text24);
        for (major, expected) in [(22, text22), (24, text24), (26, *text26)] {
            if node_lacks(label, major) {
                continue;
            }
            let mut graph = Graph::new();
            let value = build(&mut graph, label);
            let inspected = graph.inspect(&value, major);
            if inspected.text != expected || inspected.approximate != needs_icu(label) {
                failures.push(format!(
                    "{label} on Node {major}:\n  expected {expected:?}\n  printed  {:?} (approximate: {})",
                    inspected.text, inspected.approximate
                ));
            }
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[test]
fn a_proxy_of_a_proxy_is_unsupported_before_node_26() {
    let mut graph = Graph::new();
    let target = graph.plain(vec![("a", n(1.0))]);
    let inner = graph.proxy(target);
    let outer = graph.proxy(inner);
    for major in [22, 24] {
        let text = graph.inspect(&outer, major).text;
        assert_eq!(
            text,
            format!("<unsupported: a proxy of a proxy, whose traps Node {major} runs>")
        );
    }
    assert_eq!(graph.inspect(&outer, 26).text, "Proxy(Proxy({ a: 1 }))");
}

/// The stack of a gallery error thrown at `line:column` in the fixture's
/// `errors()` or `promises()`.
fn fixture_stack(head: &str, function: &str, line: u32, column: u32, caller_column: u32) -> String {
    format!(
        "{head}\n    at {function} ({FIXTURE}:{line}:{column})\n    at gallery ({FIXTURE}:{}:{caller_column})\n    at Object.<anonymous> ({FIXTURE}:211:1)\n{LOADER}",
        if function == "errors" { 194 } else { 195 },
    )
}

/// A value of the gallery in `tests/fixtures/node/values.js`.
#[allow(clippy::too_many_lines, reason = "one tree per value")]
fn gallery(g: &mut Graph, name: &str) -> Value {
    match name {
        "values.numbers.smi" => n(42.0),
        "values.numbers.negative" => n(-7.0),
        "values.numbers.heapNumber" => n(std::f64::consts::PI),
        "values.numbers.negativeZero" => n(-0.0),
        "values.numbers.notANumber" => n(f64::NAN),
        "values.numbers.infinity" => n(f64::NEG_INFINITY),
        "values.numbers.large" => n(1e21),
        "values.numbers.small" => n(5e-324),
        "values.numbers.unsafe" => n(9_007_199_254_740_994.0),
        "values.bigints.small" => big(1),
        "values.bigints.negative" => big(-12_345_678_901_234_567_890),
        "values.bigints.huge" => Value::BigInt(BigInt::new(false, vec![0, 0, 0, 1 << 8])),
        "values.primitives.undefined" => Value::Undefined,
        "values.primitives.null" => Value::Null,
        "values.primitives.true" => Value::Boolean(true),
        "values.primitives.symbol" => Value::Symbol(symbol("described")),
        "values.strings.oneByte" => s("hello"),
        "values.strings.twoByte" => s("h\u{e9}llo \u{2603} \u{1f600}"),
        "values.strings.cons" => s("concatenated-1577836800000"),
        "values.strings.sliced" => s("one-byte string that a "),
        "values.strings.thin" => s("thin-string-key"),
        "values.strings.internalized" => s("length"),
        "values.strings.empty" => s(""),
        "values.strings.escaped" => s("quote ' and \"double\" and \\ and \n"),
        "values.objects.fast" => {
            let c = g.numbers(&[3.0]);
            g.plain(vec![("a", n(1.0)), ("b", s("two")), ("c", c)])
        }
        "values.objects.slow" => g.plain(vec![("a", n(1.0)), ("c", n(3.0))]),
        "values.objects.dictionary" => {
            let keys: Vec<String> = (1995..2000).map(|i| format!("key{i}")).collect();
            let entries = keys
                .iter()
                .zip(1995..)
                .map(|(key, value)| (key.as_str(), n(f64::from(value))))
                .collect();
            g.plain(entries)
        }
        "values.objects.cyclic" => {
            let cyclic = g.reserve();
            let entries = vec![("name", s("cyclic")), ("self", Value::Object(cyclic))];
            g.fill(cyclic, named("Object"), Kind::Ordinary, props(entries));
            Value::Object(cyclic)
        }
        "values.objects.getters" => g.add(
            named("Object"),
            Kind::Ordinary,
            vec![
                accessor("computed", true, true),
                accessor("readOnly", true, false),
            ],
        ),
        "values.objects.nullPrototype" => g.add(
            null_prototype("Object"),
            Kind::Ordinary,
            vec![prop("bare", Value::Boolean(true))],
        ),
        "values.objects.symbolKeyed" => g.add(
            named("Object"),
            Kind::Ordinary,
            vec![prop("plain", n(1.0)), symbol_prop("tag", s("symbol value"))],
        ),
        "values.objects.nested" => {
            let level3 = g.plain(vec![(
                "level4",
                Value::Unreadable("beyond the depth".to_owned()),
            )]);
            let level2 = g.plain(vec![("level3", level3)]);
            let level1 = g.plain(vec![("level2", level2)]);
            g.plain(vec![("level1", level1)])
        }
        "values.objects.empty" => g.plain(vec![]),
        "values.objects.instance" => {
            g.instance("Point3", vec![("x", n(1.0)), ("y", n(2.0)), ("z", n(3.0))])
        }
        "values.arrays.packedSmi" => g.numbers(&[1.0, 2.0, 3.0]),
        "values.arrays.packedDouble" => g.numbers(&[1.5, 2.5, -0.0]),
        "values.arrays.packed" => {
            let three = g.plain(vec![("three", n(3.0))]);
            g.array(vec![n(1.0), s("two"), three])
        }
        "values.arrays.holey" => g.holey(3, vec![(0, n(1.0)), (2, n(3.0))]),
        "values.arrays.holeyDouble" => g.holey(3, vec![(0, n(1.5)), (2, n(3.5))]),
        "values.arrays.dictionary" => g.holey(100_001, vec![(0, s("near")), (100_000, s("far"))]),
        "values.arrays.cyclic" => {
            let cyclic = g.reserve();
            let elements = vec![
                Element {
                    index: 0,
                    slot: Slot::Data(n(1.0)),
                },
                Element {
                    index: 1,
                    slot: Slot::Data(Value::Object(cyclic)),
                },
            ];
            let kind = Kind::Array(Array {
                length: 2,
                elements,
                listed_below: 2,
            });
            g.fill(cyclic, named("Array"), kind, vec![]);
            Value::Object(cyclic)
        }
        "values.arrays.long" => {
            let items: Vec<f64> = (0..150).map(f64::from).collect();
            g.numbers(&items)
        }
        "values.arrays.empty" => g.array(vec![]),
        "values.collections.map" => {
            let key = g.plain(vec![("key", Value::Boolean(true))]);
            let two = g.numbers(&[2.0]);
            g.map(vec![(s("one"), n(1.0)), (key, two)])
        }
        "values.collections.set" => g.set(vec![n(1.0), s("two"), big(3)]),
        "values.collections.emptyMap" => g.map(vec![]),
        "values.collections.weakMap" => g.add(named("WeakMap"), Kind::WeakMap, vec![]),
        "values.collections.date" => g.date(1_709_210_096_789.0),
        "values.collections.invalidDate" => g.date(f64::NAN),
        "values.collections.regExp" => g.regexp("^(?<word>\\w+)\\s+\\d*$", "giu"),
        "values.functions.plain" => g.plain_function("plain"),
        "values.functions.arrow" => g.plain_function("arrow"),
        "values.functions.asyncFunction" => {
            g.function("AsyncFunction", FunctionKind::Async, s("fetchLater"))
        }
        "values.functions.generator" => {
            g.function("GeneratorFunction", FunctionKind::Generator, s("numbers"))
        }
        "values.functions.classValue" => g.function(
            "Function",
            FunctionKind::Class { extends: s("") },
            s("Point"),
        ),
        "values.functions.derived" => g.function(
            "Function",
            FunctionKind::Class {
                extends: s("Point"),
            },
            s("Point3"),
        ),
        "values.functions.bound" => g.plain_function("bound norm"),
        "values.functions.anonymous" => g.plain_function(""),
        "values.functions.builtin" => g.plain_function("max"),
        "values.errors.read" => {
            let stack = fixture_stack("TypeError: read stack", "errors", 143, 16, 13);
            g.error_with(
                named("TypeError"),
                s(&stack),
                s("TypeError"),
                s("read stack"),
                vec![],
                vec![],
            )
        }
        "values.errors.withCause" => {
            let inner = fixture_stack("RangeError: inner", "errors", 145, 48, 13);
            let cause = g.error_with(
                named("RangeError"),
                s(&inner),
                s("RangeError"),
                s("inner"),
                vec![],
                vec![],
            );
            let outer = fixture_stack("Error: outer", "errors", 145, 21, 13);
            g.error_with(
                named("Error"),
                s(&outer),
                s("Error"),
                s("outer"),
                vec![],
                vec![prop("cause", cause)],
            )
        }
        "values.promises.pending" => g.promise(PromiseState::Pending),
        "values.promises.fulfilled" => {
            let ok = g.plain(vec![("ok", Value::Boolean(true))]);
            g.promise(PromiseState::Fulfilled(ok))
        }
        "values.promises.rejected" => {
            let stack = fixture_stack("Error: handled", "promises", 151, 35, 15);
            let error = g.error_with(
                named("Error"),
                s(&stack),
                s("Error"),
                s("handled"),
                vec![],
                vec![],
            );
            g.promise(PromiseState::Rejected(error))
        }
        "values.binary.uint8" => g.typed(ElementType::Uint8, 3, vec![1, 2, 255]),
        "values.binary.int32" => g.typed(
            ElementType::Int32,
            3,
            bytes_le([
                (-1i32).to_le_bytes(),
                0i32.to_le_bytes(),
                i32::MAX.to_le_bytes(),
            ]),
        ),
        "values.binary.float64" => g.typed(
            ElementType::Float64,
            3,
            bytes_le([
                0.1f64.to_le_bytes(),
                (-0.0f64).to_le_bytes(),
                f64::NAN.to_le_bytes(),
            ]),
        ),
        "values.binary.bigInt64" => g.typed(
            ElementType::BigInt64,
            2,
            bytes_le([1i64.to_le_bytes(), (-1i64).to_le_bytes()]),
        ),
        "values.binary.view" => g.typed(ElementType::Uint16, 3, vec![0; 6]),
        "values.binary.arrayBuffer" => g.buffer(vec![0; 4]),
        "values.binary.buffer" => {
            let buffer = g.add(
                named("Buffer"),
                Kind::TypedArray {
                    element: ElementType::Uint8,
                    length: 12,
                    bytes: b"buffer bytes".to_vec(),
                },
                vec![],
            );
            g.tag(&buffer, "Uint8Array");
            buffer
        }
        "values.binary.dataView" => {
            let buffer = g.buffer(vec![0; 2]);
            let view = Some(ViewRange {
                byte_length: 2,
                byte_offset: 0,
            });
            g.add(named("DataView"), Kind::DataView { view, buffer }, vec![])
        }
        "unread" => {
            let stack = format!(
                "SyntaxError: never read\n    at Object.<anonymous> ({FIXTURE}:211:9)\n{LOADER}"
            );
            g.error_with(
                named("SyntaxError"),
                s(&stack),
                s("SyntaxError"),
                s("never read"),
                vec![],
                vec![],
            )
        }
        other => panic!("no tree for {other}"),
    }
}

#[test]
fn the_gallery_prints_as_node_prints_it() {
    for major in [22, 24, 26] {
        let mut failures = Vec::new();
        for (name, expected) in GALLERY {
            // Node 22 brackets a symbol key.
            let expected = if major == 22 && *name == "values.objects.symbolKeyed" {
                "{ plain: 1, [Symbol(tag)]: 'symbol value' }"
            } else {
                expected
            };
            let mut graph = Graph::new();
            let value = gallery(&mut graph, name);
            let inspected = graph.inspect(&value, major);
            if inspected.text != expected || inspected.approximate {
                failures.push(format!(
                    "{name} on Node {major}:\n  expected {expected:?}\n  printed  {:?}",
                    inspected.text
                ));
            }
        }
        assert!(failures.is_empty(), "{}", failures.join("\n"));
    }
}

#[test]
fn a_string_past_ten_thousand_units_says_how_many_more() {
    let graph = Graph::new();
    // The reader reads only the units inspect shows.
    let external = Value::String(JsString::new(vec![u16::from(b'x'); 10_000], 1_100_000));
    let expected = format!("'{}'... 1090000 more characters", "x".repeat(10_000));
    assert_eq!(graph.inspect(&external, 26).text, expected);
}

#[test]
fn what_the_reader_could_not_read_prints_visibly() {
    let mut graph = Graph::new();
    let short = Value::String(JsString::new(vec![u16::from(b'a'); 3], 20));
    let value = graph.plain(vec![
        ("gone", Value::Unreadable("unmapped memory".to_owned())),
        ("short", short),
    ]);
    assert_eq!(
        graph.inspect(&value, 26).text,
        "{\n  gone: <unreadable: unmapped memory>,\n  short: <unreadable: a string whose first units were not read>\n}"
    );

    let unlisted = graph.add(
        named("Array"),
        Kind::Array(Array {
            length: 5,
            elements: vec![Element {
                index: 0,
                slot: Slot::Data(n(1.0)),
            }],
            listed_below: 2,
        }),
        vec![],
    );
    assert_eq!(
        graph.inspect(&unlisted, 26).text,
        "[ 1, <1 empty item>, <unreadable: the elements from index 2 on> ]"
    );
}

/// What `util.inspect(value, { customInspect: false, compact: true,
/// breakLength: Infinity })` printed under each Node, where it differs.
#[test]
#[allow(
    clippy::too_many_lines,
    reason = "one value per way the layout departs"
)]
fn values_print_on_one_line_as_node_prints_them() {
    const STACK: &str = "RangeError: inner\n    at f (x.js:1:2)\n    at g (x.js:3:4)";
    fn mk(g: &mut Graph) -> Value {
        g.error_with(
            named("RangeError"),
            s(STACK),
            s("RangeError"),
            s("inner"),
            Vec::new(),
            Vec::new(),
        )
    }
    let indented = |columns: usize| STACK.replace('\n', &format!("\n{}", " ".repeat(columns)));
    let mut g = Graph::new();
    let mut cases: Vec<(Value, String, Option<String>)> = Vec::new();

    let error = mk(&mut g);
    let value = g.plain(vec![("a", error), ("b", n(2.0))]);
    let stack = indented(3);
    cases.push((
        value,
        format!("{{ a: {stack},\n  b: 2 }}"),
        Some(format!("{{ a: {stack}, b: 2 }}")),
    ));

    let error = mk(&mut g);
    let inner = g.plain(vec![("y", error), ("z", n(1.0))]);
    let value = g.plain(vec![("x", inner)]);
    let stack = indented(6);
    cases.push((
        value,
        format!("{{ x: {{ y: {stack},\n     z: 1 }} }}"),
        Some(format!("{{ x: {{ y: {stack}, z: 1 }} }}")),
    ));

    let error = mk(&mut g);
    let value = g.array(vec![error, n(1.0)]);
    let stack = indented(2);
    cases.push((
        value,
        format!("[ {stack},\n  1 ]"),
        Some(format!("[ {stack}, 1 ]")),
    ));

    let error = mk(&mut g);
    let value = g.map(vec![(s("k"), error)]);
    cases.push((
        value,
        format!("Map(1) {{\n  'k' => {stack} }}"),
        Some(format!("Map(1) {{ 'k' => {stack} }}")),
    ));

    let error = mk(&mut g);
    let value = g.array_with(named("Array"), vec![n(1.0)], vec![prop("e", error)]);
    cases.push((
        value,
        format!("[ 1,\n  e: {stack} ]"),
        Some(format!("[ 1, e: {stack} ]")),
    ));

    let value = g.error_with(
        named("RangeError"),
        s(STACK),
        s("RangeError"),
        s("inner"),
        vec![prop("code", s("E"))],
        Vec::new(),
    );
    cases.push((value, format!("{{ {STACK}\n  code: 'E' }}"), None));

    let id = g.reserve();
    g.fill(
        id,
        named("Object"),
        Kind::Ordinary,
        props(vec![("n", n(1.0)), ("self", Value::Object(id))]),
    );
    cases.push((
        Value::Object(id),
        "<ref *1> { n: 1, self: [Circular *1] }".to_owned(),
        None,
    ));

    let id = g.reserve();
    g.fill(
        id,
        named("Map"),
        Kind::Map {
            size: 1,
            entries: vec![(s("a"), n(1.0))],
        },
        props(vec![("self", Value::Object(id))]),
    );
    cases.push((
        Value::Object(id),
        "<ref *1> Map(1) { 'a' => 1, self: [Circular *1] }".to_owned(),
        None,
    ));

    let value = g.add(
        named("Function"),
        Kind::Function(Function {
            kind: FunctionKind::Plain,
            name: s("f"),
        }),
        props(vec![("a", n(1.0))]),
    );
    cases.push((value, "{ [Function: f] a: 1 }".to_owned(), None));

    let long = format!("{}\n{}", "x".repeat(100), "y".repeat(20));
    let value = s(&long);
    cases.push((
        value,
        format!("'{}\\n{}'", "x".repeat(100), "y".repeat(20)),
        None,
    ));

    let items: Vec<f64> = (0..120).map(f64::from).collect();
    let value = g.numbers(&items);
    let shown: Vec<String> = (0..100).map(|i| i.to_string()).collect();
    cases.push((
        value,
        format!("[ {}, ... 20 more items ]", shown.join(", ")),
        None,
    ));

    // Releases before `isBelowBreakLength`'s infinite `breakLength` early
    // return print as 22 does: 24.20.0 and 26.7.0 (the last without it),
    // and 24.16.0 and 26.5.0 (nixpkgs builds).
    let before = |major, minor| NodeRelease {
        major,
        minor,
        patch: 0,
    };
    let releases = [
        (release(22), true),
        (before(24, 16), true),
        (before(24, 20), true),
        (before(24, 21), false),
        (release(24), false),
        (before(26, 5), true),
        (before(26, 7), true),
        (before(26, 8), false),
        (release(26), false),
    ];
    let mut failures = Vec::new();
    for (value, newer, older) in &cases {
        for (node, old) in releases {
            let expected = if old {
                older.as_ref().unwrap_or(newer)
            } else {
                newer
            };
            let printed = g.line(value, node);
            if printed != *expected {
                failures.push(format!(
                    "Node {node}:\n  expected {expected:?}\n  printed  {printed:?}"
                ));
            }
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}
