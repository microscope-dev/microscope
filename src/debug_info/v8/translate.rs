//! The translator of V8's `debug-macros.cc` into the programs of
//! `src/image/v8/slices.gen.rs`: each function a `ScopeInfo` slice needs,
//! and every function it calls, statement by statement, for each version.
//!
//! Torque writes these functions in a small subset of C++: declarations of
//! `tmpN` and `phi_bbN_M` locals, `blockN:` labels, `goto`s, `if (x) goto a;
//! else goto b;`, `ASSIGN_OR_RETURN` of a call, field reads, bit-field
//! decodes, and returns. Each statement shape this file knows becomes one
//! or more ops; any other line fails the translation, naming it. Names V8
//! defines in its headers rather than in Torque, such as
//! `kScopeInfoMaxInlinedLocalNamesSize` and `ScopeType`'s values, are read
//! from each version's headers, and a missing one fails too.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use regex::Regex;

use crate::image::v8::ScopeInfoSlice;

/// What each `ScopeInfo` slice function's name begins with.
const SCOPE_INFO_SLICE: &str = "TqDebugFieldSliceScopeInfo";

/// One function of `debug-macros.cc`, as written.
#[derive(Debug, PartialEq, Eq)]
struct Source {
    parameters: Vec<String>,
    returns: usize,
    body: Vec<String>,
}

fn regex(pattern: &str) -> Regex {
    Regex::new(pattern).unwrap_or_else(|error| panic!("{pattern}: {error}"))
}

static SIGNATURE: LazyLock<Regex> = LazyLock::new(|| {
    regex(r"^Value<(.+)> (TqDebug\w+)\(d::MemoryAccessor accessor((?:, \w+ \w+)*)\) \{$")
});

/// Every function `text` defines, by name. Each is guarded by its own
/// `#ifndef`, so its first definition is the one compiled; any later one
/// must be the same.
fn functions(text: &str) -> BTreeMap<String, Source> {
    let mut functions = BTreeMap::new();
    let mut lines = text.lines();
    while let Some(line) = lines.next() {
        if !line.starts_with("Value<") {
            continue;
        }

        let captures = SIGNATURE
            .captures(line)
            .unwrap_or_else(|| panic!("a signature is {line:?}"));
        let returns = captures[1]
            .strip_prefix("std::tuple<")
            .map_or(1, |types| types.matches(", ").count() + 1);
        let parameters = captures[3]
            .split(", ")
            .filter(|parameter| !parameter.is_empty())
            .map(|parameter| {
                let (_, name) = parameter.split_once(' ').expect("a type and a name");
                name.to_owned()
            })
            .collect();
        let body = lines
            .by_ref()
            .take_while(|line| *line != "}")
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(str::to_owned)
            .collect();
        let source = Source {
            parameters,
            returns,
            body,
        };
        let name = captures[2].to_owned();
        if let Some(first) = functions.get(&name) {
            assert_eq!(first, &source, "{name} is defined twice, differently");
        } else {
            functions.insert(name, source);
        }
    }

    functions
}

/// The value of each name `debug-macros.cc` takes from V8's headers, from
/// `src/common/globals.h` under `v8`.
fn header_constants(v8: &Path) -> BTreeMap<String, i64> {
    let path = v8.join("src/common/globals.h");
    let globals = std::fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("read {}: {error}", path.display()));
    let mut constants = BTreeMap::new();

    let inline = regex(r"(?m)^constexpr int kScopeInfoMaxInlinedLocalNamesSize = (\d+);$");
    let found: Vec<_> = inline.captures_iter(&globals).collect();
    let [limit] = found.as_slice() else {
        panic!(
            "globals.h defines kScopeInfoMaxInlinedLocalNamesSize {} times",
            found.len()
        );
    };
    constants.insert(
        "kScopeInfoMaxInlinedLocalNamesSize".to_owned(),
        limit[1].parse().expect("a number"),
    );

    for (enumeration, start) in [
        ("ScopeType", "\nenum ScopeType : uint8_t {"),
        (
            "VariableAllocationInfo",
            "\nenum class VariableAllocationInfo {",
        ),
    ] {
        let (_, rest) = globals
            .split_once(start)
            .unwrap_or_else(|| panic!("globals.h defines no {enumeration}"));
        let (list, _) = rest.split_once('}').expect("the enum's end");
        let names = list
            .lines()
            .map(|line| line.split("//").next().unwrap_or_default())
            .flat_map(|line| line.split(','))
            .map(str::trim)
            .filter(|name| !name.is_empty());
        for (value, name) in names.enumerate() {
            assert!(
                name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'),
                "{enumeration} names {name:?}, which is not a plain enumerator"
            );
            let value = i64::try_from(value).expect("a small enum");
            constants.insert(format!("{enumeration}::{name}"), value);
        }
    }

    // Tagged values are 8 bytes in a build without pointer compression,
    // the only kind the layouts bind to, on x86-64.
    for line in [
        "constexpr int kSystemPointerSize = sizeof(void*);",
        "constexpr int kTaggedSize = kInt32Size;",
        "constexpr int kTaggedSize = kSystemPointerSize;",
    ] {
        assert_eq!(
            globals.lines().filter(|own| *own == line).count(),
            1,
            "globals.h has no single {line:?}"
        );
    }
    constants.insert("kTaggedSize".to_owned(), 8);
    constants
}

/// An op whose jumps still name labels.
enum Pending {
    Text(String),
    Goto(String),
    Branch(String, String, String),
}

/// The translation of one version.
struct Translator<'a> {
    sources: &'a BTreeMap<String, Source>,
    constants: &'a BTreeMap<String, i64>,
    used: BTreeSet<String>,
    indices: BTreeMap<String, u16>,

    /// Each function's text, by index; `None` while it is translated.
    functions: Vec<Option<String>>,
}

impl Translator<'_> {
    /// The index of the function `name`, translating it first if needed.
    fn function(&mut self, name: &str) -> u16 {
        if let Some(&index) = self.indices.get(name) {
            assert!(
                self.functions[usize::from(index)].is_some(),
                "{name} calls itself"
            );
            return index;
        }

        let source = self
            .sources
            .get(name)
            .unwrap_or_else(|| panic!("{name} is called but not defined"));
        let index = u16::try_from(self.functions.len()).expect("few functions");
        self.functions.push(None);
        self.indices.insert(name.to_owned(), index);
        let text = Body::new(self, name, source).translate();
        self.functions[usize::from(index)] = Some(text);
        index
    }

    /// The value of a constant argument: a number, an `IntegerLiteral`, a
    /// `bool`, or a name from V8's headers.
    fn constant(&mut self, text: &str) -> Option<i64> {
        static LITERAL: LazyLock<Regex> =
            LazyLock::new(|| regex(r"^IntegerLiteral\((false|true), 0x([0-9a-f]+)ull\)$"));
        static CAST: LazyLock<Regex> =
            LazyLock::new(|| regex(r"^CastIfEnumClass<uint32_t>\(([\w:]+)\)$"));

        if let Ok(value) = text.parse() {
            return Some(value);
        }

        if let Some(captures) = LITERAL.captures(text) {
            let value = i64::from_str_radix(&captures[2], 16).expect("a literal that fits");
            return Some(if &captures[1] == "true" {
                -value
            } else {
                value
            });
        }

        match text {
            "true" => return Some(1),
            "false" => return Some(0),
            _ => {}
        }

        let name = CAST.captures(text).map_or(text, |captures| {
            captures.get(1).map_or(text, |name| name.as_str())
        });
        let value = *self.constants.get(name)?;
        self.used.insert(name.to_owned());
        Some(value)
    }
}

/// The translation of one function.
struct Body<'t, 'a> {
    translator: &'t mut Translator<'a>,
    name: &'t str,
    registers: BTreeMap<String, u8>,
    ops: Vec<Pending>,
    labels: BTreeMap<String, usize>,
}

static DECLARATION: LazyLock<Regex> = LazyLock::new(|| {
    regex(r"^(?:intptr_t|uintptr_t|uint32_t|int32_t|bool) (\w+)\{\}; USE\((\w+)\);$")
});
static CALL: LazyLock<Regex> =
    LazyLock::new(|| regex(r"^ASSIGN_OR_RETURN\((.+?), ([\w:]+)\(accessor((?:, .+)?)\)\);$"));
static READ_TAGGED: LazyLock<Regex> = LazyLock::new(|| {
    regex(r"^READ_TAGGED_FIELD_OR_FAIL\((\w+), accessor, (\w+), static_cast<int>\((\w+)\)\);$")
});
static READ_U32: LazyLock<Regex> =
    LazyLock::new(|| regex(r"^READ_FIELD_OR_FAIL\(uint32_t, (\w+), accessor, (\w+), (\w+)\);$"));
static TIE: LazyLock<Regex> =
    LazyLock::new(|| regex(r"^std::tie\(([\w, ]+)\) = \(std::make_tuple\(([\w, ]+)\)\);$"));
static ASSIGN: LazyLock<Regex> = LazyLock::new(|| regex(r"^(\w+) = (.+);$"));
static DECODE: LazyLock<Regex> = LazyLock::new(|| {
    regex(
        r"^CastToUnderlyingTypeIfEnum\(base::BitField<\w+, (\d+), (\d+), uint32_t>::decode\((.+)\)\)$",
    )
});
static RETURN: LazyLock<Regex> =
    LazyLock::new(|| regex(r"^return \{d::MemoryAccessResult::kOk, (.+)\};$"));

impl<'t, 'a> Body<'t, 'a> {
    fn new(translator: &'t mut Translator<'a>, name: &'t str, source: &Source) -> Self {
        let registers = source
            .parameters
            .iter()
            .enumerate()
            .map(|(index, name)| (name.clone(), u8::try_from(index).expect("few parameters")))
            .collect();
        Self {
            translator,
            name,
            registers,
            ops: Vec::new(),
            labels: BTreeMap::new(),
        }
    }

    fn translate(mut self) -> String {
        let sources: &BTreeMap<String, Source> = self.translator.sources;
        let source = &sources[self.name];
        let mut lines = source.body.iter().map(String::as_str);
        while let Some(line) = lines.next() {
            if line.starts_with("if (") {
                let branch: Vec<&str> = std::iter::once(line)
                    .chain(lines.by_ref().take(4))
                    .collect();
                self.branch(&branch);
            } else {
                self.statement(line);
            }
        }

        self.finish(source.parameters.len())
    }

    fn fail(&self, line: &str) -> ! {
        panic!("{}: no translation of {line:?}", self.name)
    }

    /// `if (x) {`, `goto a;`, `} else {`, `goto b;`, `}`.
    fn branch(&mut self, lines: &[&str]) {
        let joined = lines.join(" ");
        let pattern = regex(r"^if \((\w+)\) \{ goto (block\d+); \} else \{ goto (block\d+); \}$");
        let Some(captures) = pattern.captures(&joined) else {
            self.fail(&joined)
        };

        let condition = self.operand(&captures[1]);
        self.ops.push(Pending::Branch(
            condition,
            captures[2].to_owned(),
            captures[3].to_owned(),
        ));
    }

    fn statement(&mut self, line: &str) {
        if let Some(captures) = DECLARATION.captures(line) {
            assert_eq!(&captures[1], &captures[2], "{}: {line}", self.name);
            let register = u8::try_from(self.registers.len()).expect("few registers");
            let previous = self.registers.insert(captures[1].to_owned(), register);
            assert!(previous.is_none(), "{}: {line} redeclares", self.name);
        } else if let Some(label) = line.strip_suffix(':').filter(|label| is_label(label)) {
            let previous = self.labels.insert(label.to_owned(), self.ops.len());
            assert!(previous.is_none(), "{}: {line} twice", self.name);
        } else if let Some(label) = line
            .strip_prefix("goto ")
            .and_then(|rest| rest.strip_suffix(';'))
            .filter(|label| is_label(label))
        {
            self.ops.push(Pending::Goto(label.to_owned()));
        } else if line == "UNREACHABLE();" {
            self.ops.push(Pending::Text("Op::Unreachable".to_owned()));
        } else if let Some(captures) = CALL.captures(line) {
            self.call(line, &captures[1], &captures[2], &captures[3]);
        } else if let Some(captures) = READ_TAGGED.captures(line) {
            self.read("ReadTagged", &captures);
        } else if let Some(captures) = READ_U32.captures(line) {
            self.read("ReadU32", &captures);
        } else if let Some(captures) = TIE.captures(line) {
            self.tie(line, &captures[1], &captures[2]);
        } else if let Some(captures) = RETURN.captures(line) {
            let values = captures[1]
                .strip_prefix("std::make_tuple(")
                .and_then(|values| values.strip_suffix(')'))
                .unwrap_or(&captures[1]);
            let values = self.operands(&split(values));
            self.ops.push(Pending::Text(format!(
                "Op::Return(&[{}])",
                values.join(", ")
            )));
        } else if let Some(captures) = ASSIGN.captures(line) {
            self.assign(line, &captures[1], &captures[2]);
        } else {
            self.fail(line);
        }
    }

    /// `ASSIGN_OR_RETURN(results, callee(accessor, arguments));`.
    fn call(&mut self, line: &str, results: &str, callee: &str, arguments: &str) {
        let results = results
            .strip_prefix("std::tie(")
            .and_then(|results| results.strip_suffix(')'))
            .unwrap_or(results);
        let results: Vec<String> = split(results)
            .iter()
            .map(|result| self.register(result).to_string())
            .collect();
        let arguments = split(arguments.strip_prefix(", ").unwrap_or(arguments));
        let operands = self.operands(&arguments);

        if let Some(shim) = callee.strip_prefix("TorqueDebugMacroShims::CodeStubAssembler::") {
            let [to] = results.as_slice() else {
                self.fail(line)
            };
            let text = match (shim, operands.as_slice()) {
                (
                    "IntPtrAdd"
                    | "IntPtrMul"
                    | "IntPtrLessThan"
                    | "IntPtrLessThanOrEqual"
                    | "UintPtrLessThan"
                    | "Word32Equal"
                    | "Word32NotEqual",
                    [left, right],
                ) => format!("Op::Binary(Binary::{shim}, {to}, {left}, {right})"),
                ("SmiUntag", [from]) => format!("Op::SmiUntag({to}, {from})"),
                // Casts between C++ integer types and from a constant
                // bool, which keep a value's bits as the interpreter
                // holds them.
                ("Unsigned" | "BoolConstant", [from]) => format!("Op::Set({to}, {from})"),
                _ => self.fail(line),
            };
            self.ops.push(Pending::Text(text));
            return;
        }

        if !callee.starts_with("TqDebug") {
            self.fail(line);
        }

        let function = self.translator.function(callee);
        let source = &self.translator.sources[callee];
        assert_eq!(
            source.parameters.len(),
            operands.len(),
            "{}: {line}",
            self.name
        );
        assert_eq!(source.returns, results.len(), "{}: {line}", self.name);
        self.ops.push(Pending::Text(format!(
            "Op::Call {{ function: {function}, arguments: &[{}], results: &[{}] }}",
            operands.join(", "),
            results.join(", ")
        )));
    }

    fn read(&mut self, op: &str, captures: &regex::Captures<'_>) {
        let to = self.register(&captures[1]);
        let object = self.operand(&captures[2]);
        let offset = self.operand(&captures[3]);
        self.ops.push(Pending::Text(format!(
            "Op::{op} {{ to: {to}, object: {object}, offset: {offset} }}"
        )));
    }

    /// `std::tie(a, b) = (std::make_tuple(x, y));`, which the parameters
    /// of a slice's or a reference's constructor are copied by.
    fn tie(&mut self, line: &str, targets: &str, sources: &str) {
        let targets = split(targets);
        let sources = split(sources);
        if targets.len() != sources.len() || targets.iter().any(|target| sources.contains(target)) {
            self.fail(line);
        }

        for (target, source) in targets.iter().zip(&sources) {
            let to = self.register(target);
            let from = self.operand(source);
            self.ops
                .push(Pending::Text(format!("Op::Set({to}, {from})")));
        }
    }

    /// `to = expression;`.
    fn assign(&mut self, line: &str, to: &str, expression: &str) {
        let to = self.register(to);
        let inner = |text: &str, prefix: &str| {
            text.strip_prefix(prefix)
                .and_then(|rest| rest.strip_suffix(')'))
                .map(str::to_owned)
        };

        let text = if let Some(captures) = DECODE.captures(expression) {
            let (from, smi) = inner(&captures[3], "Internals::SmiValue(")
                .map_or_else(|| (captures[3].to_owned(), false), |from| (from, true));
            format!(
                "Op::Decode {{ to: {to}, from: {}, smi: {smi}, shift: {}, size: {} }}",
                self.operand(&from),
                &captures[1],
                &captures[2]
            )
        } else if let Some(from) = inner(expression, "Internals::IntToSmi(")
            .and_then(|from| inner(&from, "CastToUnderlyingTypeIfEnum("))
        {
            format!("Op::IntToSmi({to}, {})", self.operand(&from))
        } else if let Some(from) = inner(expression, "(")
            .map(|from| inner(&from, "CastToUnderlyingTypeIfEnum(").unwrap_or(from))
            .or_else(|| is_identifier(expression).then(|| expression.to_owned()))
        {
            format!("Op::Set({to}, {})", self.operand(&from))
        } else {
            self.fail(line)
        };

        self.ops.push(Pending::Text(text));
    }

    fn register(&self, name: &str) -> u8 {
        *self
            .registers
            .get(name)
            .unwrap_or_else(|| panic!("{}: {name} is not declared", self.name))
    }

    /// An argument as an operand: a local, a parameter, a constant, or
    /// Torque's conversion of a constexpr parameter, which keeps it.
    fn operand(&mut self, text: &str) -> String {
        static CONVERSION: LazyLock<Regex> = LazyLock::new(|| {
            regex(
                r"^\(TorqueDebugMacroShims::CodeStubAssembler::ConstexprIntegerLiteralTo(?:Intptr|Int31)\(accessor, (\w+)\)\)$",
            )
        });

        let text = CONVERSION
            .captures(text)
            .and_then(|captures| captures.get(1))
            .map_or(text, |name| name.as_str());
        if let Some(register) = self.registers.get(text) {
            return format!("R({register})");
        }

        match self.translator.constant(text) {
            Some(value) => format!("V({value})"),
            None => panic!("{}: no translation of the argument {text:?}", self.name),
        }
    }

    fn operands(&mut self, texts: &[String]) -> Vec<String> {
        texts.iter().map(|text| self.operand(text)).collect()
    }

    /// The function's text, with each label resolved to the op it
    /// precedes; every jump must go forward.
    fn finish(self, parameters: usize) -> String {
        let target = |from: usize, label: &str| {
            let to = *self
                .labels
                .get(label)
                .unwrap_or_else(|| panic!("{}: no {label}", self.name));
            assert!(
                to > from,
                "{}: {label} is not ahead of op {from}",
                self.name
            );
            to
        };

        let mut text = format!(
            "            Function {{\n                name: {:?},\n                parameters: {parameters},\n                registers: {},\n                ops: &[\n",
            self.name,
            self.registers.len()
        );
        for (index, op) in self.ops.iter().enumerate() {
            let op = match op {
                Pending::Text(op) => op.clone(),
                Pending::Goto(label) => format!("Op::Goto({})", target(index, label)),
                Pending::Branch(condition, then, otherwise) => format!(
                    "Op::Branch {{ condition: {condition}, then: {}, otherwise: {} }}",
                    target(index, then),
                    target(index, otherwise)
                ),
            };
            writeln!(text, "                    /* {index} */ {op},").expect("write");
        }

        text.push_str("                ],\n            },\n");
        text
    }
}

fn is_label(text: &str) -> bool {
    text.strip_prefix("block")
        .is_some_and(|number| !number.is_empty() && number.bytes().all(|b| b.is_ascii_digit()))
}

fn is_identifier(text: &str) -> bool {
    !text.is_empty() && text.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

/// Splits a list of arguments at its top-level commas.
fn split(text: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut depth = 0usize;
    let mut current = String::new();
    for c in text.chars() {
        match c {
            '(' | '<' => depth += 1,
            ')' | '>' => depth = depth.checked_sub(1).expect("balanced parentheses"),
            ',' if depth == 0 => {
                parts.push(current.trim().to_owned());
                current.clear();
                continue;
            }
            _ => {}
        }
        current.push(c);
    }

    if !current.trim().is_empty() {
        parts.push(current.trim().to_owned());
    }
    parts
}

/// One version's entry of `slices.gen.rs`, from its `debug-macros.cc`
/// and its headers.
fn generate_version(text: &mut String, (major, minor): (u32, u32), macros: &str, v8: &Path) {
    let sources = functions(macros);
    let constants = header_constants(v8);
    let mut translator = Translator {
        sources: &sources,
        constants: &constants,
        used: BTreeSet::new(),
        indices: BTreeMap::new(),
        functions: Vec::new(),
    };

    let mut slices = Vec::new();
    for name in sources
        .keys()
        .filter(|name| name.starts_with(SCOPE_INFO_SLICE))
    {
        let field = &name[SCOPE_INFO_SLICE.len()..];
        let slice = ScopeInfoSlice::ALL
            .iter()
            .find(|slice| format!("{slice:?}") == field)
            .unwrap_or_else(|| {
                panic!("V8 {major}.{minor}'s ScopeInfo has a slice {field} no ScopeInfoSlice names")
            });
        slices.push((*slice, translator.function(name)));
    }
    assert!(
        slices.len() >= 10,
        "V8 {major}.{minor} has ScopeInfo slices"
    );
    slices.sort();

    writeln!(
        text,
        "    Translated {{\n        major: {major},\n        minor: {minor},"
    )
    .expect("write");
    let used: Vec<String> = translator
        .used
        .iter()
        .map(|name| format!("{name} = {}", constants[name]))
        .collect();
    writeln!(text, "        // From V8's headers: {}.", used.join(", ")).expect("write");
    writeln!(text, "        scope_info: &[").expect("write");
    for (slice, function) in slices {
        writeln!(text, "            (ScopeInfoSlice::{slice:?}, {function}),").expect("write");
    }

    writeln!(text, "        ],\n        functions: &[").expect("write");
    for (index, function) in translator.functions.into_iter().enumerate() {
        writeln!(text, "            // {index}").expect("write");
        text.push_str(&function.expect("every function translated"));
    }
    writeln!(text, "        ],\n    }},").expect("write");
}

/// The text of `slices.gen.rs` for every version in `versions`, each its
/// major.minor and its `torque-generated` directory, beside which `v8`
/// holds its headers; `notice` is V8's license.
pub(super) fn generate(versions: &[((u32, u32), PathBuf)], notice: &str) -> String {
    let mut text = String::from(
        "// @generated from V8's debug-macros.cc and headers for each supported\n\
         // version, which MICROSCOPE_V8_TORQUE holds, by\n\
         // `debug_info::v8::conformance::slices_match_torque`. Do not edit;\n\
         // rerun that test with MICROSCOPE_UPDATE_V8=1.\n\
         //\n\
         // The programs translate V8's code, which carries this notice:\n\
         //\n",
    );
    for line in notice.lines() {
        if line.is_empty() {
            text.push_str("//\n");
        } else {
            writeln!(text, "// {line}").expect("write");
        }
    }

    text.push_str(
        "\nuse super::ScopeInfoSlice;\n\
         use super::program::Operand::{Register as R, Value as V};\n\
         use super::program::{Binary, Function, Op, Translated};\n\n\
         pub(super) const TRANSLATED: &[Translated] = &[\n",
    );
    for (version, torque) in versions {
        let path = torque.join("debug-macros.cc");
        let macros = std::fs::read_to_string(&path)
            .unwrap_or_else(|error| panic!("read {}: {error}", path.display()));
        let v8 = torque.parent().expect("a version's directory").join("v8");
        generate_version(&mut text, *version, &macros, &v8);
    }

    text.push_str("];\n");
    text
}

#[test]
#[should_panic(expected = "no translation of \"tmp0 = tmp1 << 2;\"")]
fn a_statement_the_translator_does_not_know_fails_it() {
    let macros = "\
Value<intptr_t> TqDebugUnknown_0(d::MemoryAccessor accessor, uintptr_t p_o) {
  intptr_t tmp0{}; USE(tmp0);
  intptr_t tmp1{}; USE(tmp1);
  goto block0;

  block0:
  tmp0 = tmp1 << 2;
  return {d::MemoryAccessResult::kOk, tmp0};
}
";
    let sources = functions(macros);
    let constants = BTreeMap::new();
    let mut translator = Translator {
        sources: &sources,
        constants: &constants,
        used: BTreeSet::new(),
        indices: BTreeMap::new(),
        functions: Vec::new(),
    };
    translator.function("TqDebugUnknown_0");
}
