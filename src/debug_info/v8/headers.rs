//! What V8's C++ headers say that its Torque output does not, parsed for
//! `layouts.gen.rs`: the fields of `Code` and `CodeWrapper`, which no
//! Torque reader reads; the kinds of code; the slots of deoptimization
//! data, hash tables, name dictionaries, and descriptor arrays; the bits of
//! a property's details and of a source position; an inlining position's
//! layout; the encoding of weak references; and the JS dispatch table's
//! entries. Each value is computed from the header's own expressions for
//! the configuration the layouts require, and any construct or name the
//! parser does not know fails, rather than being guessed.

use std::collections::BTreeMap;
use std::path::Path;

use regex::Regex;

/// The configuration the layouts are generated for, each macro with why it
/// has its value.
///
/// - `V8_ENABLE_SANDBOX_BOOL` is false: binding refuses a build with the
///   sandbox, and every build in the matrix says `"v8_enable_sandbox": 0`.
/// - `V8_EMBEDDED_CONSTANT_POOL_BOOL` is false: `globals.h` makes it true
///   only on PPC, which [`configuration`] checks.
/// - `V8_ENABLE_LEAPTIERING_BOOL` is true where Torque's readers give
///   `JSFunction::DispatchHandle`, from 13.6.
/// - `V8_BUILTIN_JUMP_TABLE_INFO_BOOL` (13.6) is false: it needs
///   `V8_ENABLE_BUILTIN_JUMP_TABLE_SWITCH`, a build argument off by
///   default that no header defines; Node 24's code objects keep their
///   parameter count at 84 and builtin id at 86, as without it.
/// - `V8_JUMP_TABLE_INFO_BOOL` (14.6) is true: it is `ENABLE_DISASSEMBLER`
///   on x64, and every 14.6 build in the matrix defines
///   `Code::Disassemble`, which `code.h` declares only with the
///   disassembler; Node 26's code objects keep their builtin id at 90.
/// - `V8_LOWER_LIMITS_MODE_BOOL` is false, the build argument's default:
///   every dispatch handle in Node 24's and 26's processes is a multiple
///   of 256, not 4096.
pub(super) fn configuration(globals: &str, leaptiering: bool) -> BTreeMap<&'static str, bool> {
    let guard = globals
        .split_once("#define V8_EMBEDDED_CONSTANT_POOL_BOOL true")
        .and_then(|(before, _)| before.lines().rev().find(|line| line.starts_with("#if")))
        .expect("globals.h defines V8_EMBEDDED_CONSTANT_POOL_BOOL under a condition");
    assert!(
        guard.contains("PPC") && !guard.contains("X64"),
        "V8_EMBEDDED_CONSTANT_POOL_BOOL is true under {guard:?}"
    );

    BTreeMap::from([
        ("V8_ENABLE_SANDBOX_BOOL", false),
        ("V8_EMBEDDED_CONSTANT_POOL_BOOL", false),
        ("V8_ENABLE_LEAPTIERING_BOOL", leaptiering),
        ("V8_BUILTIN_JUMP_TABLE_INFO_BOOL", false),
        ("V8_JUMP_TABLE_INFO_BOOL", true),
        ("V8_LOWER_LIMITS_MODE_BOOL", false),
    ])
}

/// One version's headers, under `MICROSCOPE_V8_TORQUE/<version>/v8`.
pub(super) struct Headers {
    files: BTreeMap<&'static str, String>,
    config: BTreeMap<&'static str, bool>,
}

const FILES: [&str; 15] = [
    "src/common/globals.h",
    "include/v8-internal.h",
    "src/objects/trusted-object.h",
    "src/objects/code.h",
    "src/objects/code-kind.h",
    "src/objects/deoptimization-data.h",
    "src/codegen/source-position.h",
    "src/objects/hash-table.h",
    "src/objects/dictionary.h",
    "src/objects/descriptor-array.h",
    "src/objects/property-details.h",
    "src/sandbox/js-dispatch-table.h",
    "src/objects/js-function.h",
    "src/objects/property-array.h",
    "src/objects/property-array.tq",
];

/// The headers of 12.4, which has no JS dispatch table.
const OPTIONAL: [&str; 1] = ["src/sandbox/js-dispatch-table.h"];

impl Headers {
    pub(super) fn read(v8: &Path, leaptiering: bool) -> Self {
        let mut files = BTreeMap::new();
        for file in FILES {
            let path = v8.join(file);
            match std::fs::read_to_string(&path) {
                Ok(text) => {
                    files.insert(file, strip_comments(&text));
                }
                Err(error) => assert!(
                    OPTIONAL.contains(&file) && !leaptiering,
                    "read {}: {error}",
                    path.display()
                ),
            }
        }

        let config = configuration(&files["src/common/globals.h"], leaptiering);
        Self { files, config }
    }

    /// Whether `file`, without its comments, has `text`.
    pub(super) fn contains(&self, file: &str, text: &str) -> bool {
        self.file(file).contains(text)
    }

    fn file(&self, file: &str) -> &str {
        self.files
            .get(file)
            .unwrap_or_else(|| panic!("{file} is not read"))
    }

    /// The size in bytes the name of a size in a field list stands for.
    fn size(&self, name: &str) -> i64 {
        let globals = self.file("src/common/globals.h");
        for (name, value) in [
            ("kTaggedSize", "kSystemPointerSize"),
            ("kTrustedPointerSize", "kTaggedSize"),
            ("kCodePointerSize", "kTrustedPointerSize"),
        ] {
            assert!(
                globals.contains(&format!("constexpr int {name} = {value};")),
                "globals.h does not define {name} as {value}"
            );
        }

        match name {
            "kTaggedSize" | "kTrustedPointerSize" | "kCodePointerSize" => {
                self.size("kSystemPointerSize")
            }
            _ => {
                let pattern = format!(r"constexpr int {name} = sizeof\(([\w*]+)\);");
                let ty = Regex::new(&pattern)
                    .expect("a pattern")
                    .captures(globals)
                    .unwrap_or_else(|| panic!("globals.h gives no size of {name}"))[1]
                    .to_owned();
                type_size(globals, &ty)
            }
        }
    }

    /// The value an expression of `file` has, its names looked up in
    /// `classes`' constants in order, then in the file's and the other
    /// headers' namespace constants, and the configuration.
    fn eval(&self, file: &str, classes: &[&str], expression: &str) -> i64 {
        let lookup = |name: &str| self.lookup(file, classes, name);
        eval(expression, &lookup)
            .unwrap_or_else(|| panic!("{file}: cannot evaluate {expression:?}"))
    }

    fn lookup(&self, file: &str, classes: &[&str], name: &str) -> Option<i64> {
        if let Some(value) = self.config.get(name) {
            return Some(i64::from(*value));
        }

        if let Some((class, member)) = name.rsplit_once("::") {
            let file = self.declaring(class)?;
            return self.class_constant(file, &[class], member);
        }

        if let Some(value) = self.class_constant(file, classes, name) {
            return Some(value);
        }

        if name == "kSystemPointerSize" {
            return Some(self.size(name));
        }

        FILES
            .iter()
            .filter(|own| self.files.contains_key(**own))
            .find_map(|own| namespace_constant(self.file(own), name))
            .map(|expression| self.eval(file, &[], &expression))
    }

    /// The header that declares `class`, of those read.
    fn declaring(&self, class: &str) -> Option<&'static str> {
        FILES.iter().copied().find(|file| {
            self.files
                .get(file)
                .is_some_and(|text| class_body(text, class).is_some())
        })
    }

    /// A `static const` constant of the first of `classes`, a class and the
    /// classes it derives from, that defines it; the first definition, as
    /// the 64-bit branch of a header that defines one per architecture
    /// puts it.
    pub(super) fn class_constant(&self, file: &str, classes: &[&str], name: &str) -> Option<i64> {
        let pattern = format!(r"static\s+(?:constexpr|const)\s+[\w:]+\s+{name}\s*=\s*([^;]+);");
        let pattern = Regex::new(&pattern).expect("a pattern");
        let expression = classes.iter().find_map(|class| {
            let body = class_body(self.file(file), class)?;
            Some(pattern.captures(body)?[1].to_owned())
        })?;
        Some(self.eval(file, classes, &expression))
    }

    /// A namespace constant of `file`, `const T name = value;`.
    pub(super) fn constant(&self, file: &str, name: &str) -> i64 {
        let expression = namespace_constant(self.file(file), name)
            .unwrap_or_else(|| panic!("{file} defines no {name}"));
        self.eval(file, &[], &expression)
    }

    /// Where a `PropertyArray`'s properties begin, given where its
    /// `length_and_hash` is: just after it, the one field
    /// `property-array.tq` declares, as Torque's `kHeaderSize` puts them.
    pub(super) fn property_array_objects(&self, length_and_hash: i64) -> i64 {
        let declared =
            Regex::new(r"extern\s+class\s+PropertyArray\s+extends\s+HeapObject\s*\{([^}]*)\}")
                .expect("a pattern")
                .captures(self.file("src/objects/property-array.tq"))
                .expect("property-array.tq declares PropertyArray")[1]
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ");
        assert_eq!(
            declared, "length_and_hash: Smi;",
            "PropertyArray declares fields beside length_and_hash"
        );
        length_and_hash + self.size("kTaggedSize")
    }

    /// The fields of `Code`, from `code.h`'s `CODE_DATA_FIELDS`, which
    /// begins at `ExposedTrustedObject::kHeaderSize`: `HeapObject`'s header,
    /// its map, without the sandbox. Each is named `Code::Name` from its
    /// `kNameOffset`; one of no size, a marker, is left out.
    pub(super) fn code_fields(&self) -> Vec<(String, i64)> {
        let trusted = self.file("src/objects/trusted-object.h");
        let unsandboxed = Regex::new(
            r"#else\s*\n\s*static constexpr int kHeaderSize = TrustedObject::kHeaderSize;",
        )
        .expect("a pattern");
        assert!(
            trusted.contains("static constexpr int kHeaderSize = HeapObject::kHeaderSize;")
                && unsandboxed.is_match(trusted),
            "trusted-object.h's headers are not HeapObject's without the sandbox"
        );

        let code = self.file("src/objects/code.h");
        assert!(
            code.contains("DEFINE_FIELD_OFFSET_CONSTANTS(ExposedTrustedObject::kHeaderSize,"),
            "Code's fields do not follow ExposedTrustedObject's header"
        );
        let list = macro_body(code, "#define CODE_DATA_FIELDS(V)");
        self.field_list("Code", &list, self.size("kTaggedSize"))
    }

    /// The fields of `CodeWrapper`, from its `FIELD_LIST`, after
    /// `Struct::kHeaderSize`, `HeapObject`'s.
    pub(super) fn code_wrapper_fields(&self) -> Vec<(String, i64)> {
        let code = self.file("src/objects/code.h");
        let body = class_body(code, "CodeWrapper").expect("code.h declares CodeWrapper");
        assert!(
            body.contains("DEFINE_FIELD_OFFSET_CONSTANTS(Struct::kHeaderSize, FIELD_LIST)"),
            "CodeWrapper's fields do not follow Struct's header"
        );
        let list = macro_body(body, "#define FIELD_LIST(V)");
        self.field_list("CodeWrapper", &list, self.size("kTaggedSize"))
    }

    /// The fields of a `V(kNameOffset, size)` list from `start`, until its
    /// `kUnalignedSize`, `kHeaderSize`, or `kSize`.
    fn field_list(&self, class: &str, list: &str, start: i64) -> Vec<(String, i64)> {
        let mut fields = Vec::new();
        let mut offset = start;
        for (name, size) in v_entries(list) {
            if matches!(name.as_str(), "kUnalignedSize" | "kHeaderSize" | "kSize") {
                return fields;
            }

            let field = name
                .strip_prefix('k')
                .and_then(|name| name.strip_suffix("Offset"))
                .unwrap_or_else(|| panic!("{class}'s field {name} is not kNameOffset"));
            let size = self.field_size(&size);
            if size > 0 {
                fields.push((format!("{class}::{field}"), offset));
            }
            offset += size;
        }

        panic!("{class}'s field list does not end")
    }

    /// The size of a field list's entry: a size's name, 0, or `CONFIG ?
    /// size : size`.
    fn field_size(&self, size: &str) -> i64 {
        if let Some((condition, choices)) = size.split_once('?') {
            let (then, otherwise) = choices.split_once(':').expect("a ternary size");
            let chosen = self
                .config
                .get(condition.trim())
                .unwrap_or_else(|| panic!("no configuration of {condition:?}"));
            return self.field_size(if *chosen { then } else { otherwise });
        }

        match size.trim() {
            "0" => 0,
            name if name.starts_with('k') && name.ends_with("Size") => {
                if name == "kJSDispatchHandleSize" {
                    let globals = self.file("src/common/globals.h");
                    assert!(globals.contains(
                        "constexpr int kJSDispatchHandleSize = sizeof(JSDispatchHandle);"
                    ));
                    let alias =
                        Regex::new(r"using JSDispatchHandle = base::StrongAlias<\w+, (\w+)>;")
                            .expect("a pattern")
                            .captures(globals)
                            .expect("globals.h defines JSDispatchHandle")[1]
                            .to_owned();
                    type_size(globals, &alias)
                } else {
                    self.size(name)
                }
            }
            other => panic!("a field's size is {other:?}"),
        }
    }

    /// Each code kind's number: its place in `code-kind.h`'s
    /// `CODE_KIND_LIST`, by its name.
    pub(super) fn code_kinds(&self) -> Vec<(String, i64)> {
        let list = macro_body(
            self.file("src/objects/code-kind.h"),
            "#define CODE_KIND_LIST(V)",
        );
        let pattern = Regex::new(r"^V\((\w+)\)$").expect("a pattern");
        list.split_whitespace()
            .enumerate()
            .map(|(at, entry)| {
                let name = &pattern
                    .captures(entry)
                    .unwrap_or_else(|| panic!("a code kind is {entry:?}"))[1];
                (name.to_owned(), i64::try_from(at).unwrap_or(i64::MAX))
            })
            .collect()
    }

    /// Where a code's flags keep its kind: the first of `code.h`'s
    /// `FLAGS_BIT_FIELDS`, as `(shift, size)`.
    pub(super) fn code_kind_field(&self) -> (i64, i64) {
        let list = macro_body(
            self.file("src/objects/code.h"),
            "#define FLAGS_BIT_FIELDS(V, _)",
        );
        let first = v_entries(&list).into_iter().next().expect("a code's flags");
        let size = first
            .1
            .split(',')
            .nth(1)
            .expect("a bit field's size")
            .trim();
        assert_eq!(first.0, "KindField", "a code's flags begin with its kind");
        (0, size.parse().expect("a bit field's size"))
    }

    /// Each bit field of a class, `using Name = base::BitField<T, shift,
    /// size>` or `BitField64`, or `using Name = Previous::Next<T, size>`, as
    /// `(shift, size)`.
    pub(super) fn bit_fields(&self, file: &str, class: &str) -> BTreeMap<String, (i64, i64)> {
        let body = class_body(self.file(file), class)
            .unwrap_or_else(|| panic!("{file} declares no {class}"));
        let first = Regex::new(r"using\s+(\w+)\s*=\s*base::BitField(?:64)?<[^,]+,\s*([^,>]+),\s*([^,>]+)(?:,[^>]*)?>\s*;")
            .expect("a pattern");
        let next = Regex::new(r"using\s+(\w+)\s*=\s*(\w+)::Next<[^,]+,\s*([^>]+)>\s*;")
            .expect("a pattern");
        let mut fields = BTreeMap::new();
        for captures in first.captures_iter(body) {
            let shift = self.eval(file, &[class], &captures[2]);
            let size = self.eval(file, &[class], &captures[3]);
            fields.insert(captures[1].to_owned(), (shift, size));
        }
        for captures in next.captures_iter(body) {
            let &(shift, size) = fields
                .get(&captures[2])
                .unwrap_or_else(|| panic!("{class}::{} follows no field", &captures[1]));
            let own = self.eval(file, &[class], &captures[3]);
            fields.insert(captures[1].to_owned(), (shift + size, own));
        }
        fields
    }

    /// The enumerators of the enum whose declaration is `head`, in `file`,
    /// with their values.
    pub(super) fn enumerators(&self, file: &str, head: &str) -> BTreeMap<String, i64> {
        let text = self.file(file);
        let body = text
            .split_once(head)
            .and_then(|(_, rest)| rest.split_once('{'))
            .and_then(|(_, rest)| rest.split_once('}'))
            .unwrap_or_else(|| panic!("{file} declares no {head}"))
            .0;
        let mut values = BTreeMap::new();
        let mut next = 0;
        for enumerator in body
            .split(',')
            .map(str::trim)
            .filter(|item| !item.is_empty())
        {
            let (name, value) = match enumerator.split_once('=') {
                Some((name, value)) => (name.trim(), self.eval(file, &[], value)),
                None => (enumerator, next),
            };
            values.insert(name.to_owned(), value);
            next = value + 1;
        }
        values
    }

    /// `struct InliningPosition`'s size and its members' offsets, laid out
    /// as C does: a `SourcePosition`, whose only member is a `uint64_t`, and
    /// an `int`.
    pub(super) fn inlining_position(&self) -> (i64, BTreeMap<String, i64>) {
        let file = "src/codegen/source-position.h";
        let text = self.file(file);
        let globals = self.file("src/common/globals.h");
        let source_position = members(class_body(text, "SourcePosition").expect("SourcePosition"));
        let [(ty, _)] = source_position.as_slice() else {
            panic!("SourcePosition has members {source_position:?}, not one");
        };
        let source_position = type_size(globals, ty);

        let mut offsets = BTreeMap::new();
        let (mut offset, mut alignment): (i64, i64) = (0, 1);
        for (ty, name) in members(class_body(text, "InliningPosition").expect("InliningPosition")) {
            let size = match ty.as_str() {
                "SourcePosition" => source_position,
                ty => type_size(globals, ty),
            };
            offset = align(offset, size);
            alignment = alignment.max(size);
            offsets.insert(name, offset);
            offset += size;
        }
        (align(offset, alignment), offsets)
    }

    /// Whether the first definition of `name` in `file` is in its 64-bit
    /// branch, `#if defined(V8_TARGET_ARCH_64_BIT)` or `#ifdef
    /// V8_TARGET_ARCH_64_BIT`, before its 32-bit one.
    pub(super) fn first_is_64_bit(&self, file: &str, name: &str) -> bool {
        let text = self.file(file);
        let Some(at) = text.find(&format!("{name} =")) else {
            return false;
        };
        let before = &text[..at];
        let Some(open) = before
            .rfind("#if defined(V8_TARGET_ARCH_64_BIT)")
            .max(before.rfind("#ifdef V8_TARGET_ARCH_64_BIT"))
        else {
            return false;
        };

        // Within the branch, every conditional the definition is not in
        // has closed.
        let mut depth = 0;
        for line in before[open..].lines().skip(1).map(str::trim_start) {
            if line.starts_with("#if") {
                depth += 1;
            } else if line.starts_with("#endif") {
                if depth == 0 {
                    return false;
                }
                depth -= 1;
            } else if line.starts_with("#el") && depth == 0 {
                return false;
            }
        }
        true
    }
}

/// `value` rounded up to a multiple of `alignment`, which is positive.
fn align(value: i64, alignment: i64) -> i64 {
    (value + alignment - 1) / alignment * alignment
}

/// The size of a C type `globals.h` or the platform gives.
fn type_size(globals: &str, ty: &str) -> i64 {
    match ty {
        "int16_t" | "uint16_t" => 2,
        "int" | "int32_t" | "uint32_t" => 4,
        "int64_t" | "uint64_t" | "void*" | "intptr_t" | "uintptr_t" => 8,
        "Address" if globals.contains("using Address = uintptr_t;") => 8,
        _ => panic!("no size of the type {ty:?}"),
    }
}

/// The data members of a class's body, `Type name;` or `Type name =
/// value;`, as `(type, name)`.
fn members(body: &str) -> Vec<(String, String)> {
    let mut depth = 0;
    let body: String = body
        .chars()
        .filter(|&c| {
            let outside = depth == 0;
            match c {
                '{' => depth += 1,
                '}' => depth -= 1,
                _ => {}
            }
            outside && depth == 0 && c != '}'
        })
        .collect();
    let body = body.as_str();
    let pattern = Regex::new(r"(?m)^\s*([\w:]+)\s+(\w+)\s*(?:=[^;\n]*)?;\s*$").expect("a pattern");
    pattern
        .captures_iter(body)
        .filter(|captures| {
            !matches!(
                &captures[1],
                "using" | "return" | "static" | "friend" | "typedef" | "class" | "struct"
            )
        })
        .map(|captures| (captures[1].to_owned(), captures[2].to_owned()))
        .collect()
}

/// The text without its comments.
fn strip_comments(text: &str) -> String {
    let block = Regex::new(r"(?s)/\*.*?\*/").expect("a pattern");
    let line = Regex::new(r"//[^\n]*").expect("a pattern");
    line.replace_all(&block.replace_all(text, ""), "")
        .into_owned()
}

/// The body of the macro `head` defines, its lines joined.
fn macro_body(text: &str, head: &str) -> String {
    let rest = text
        .split_once(head)
        .unwrap_or_else(|| panic!("no {head}"))
        .1;
    let mut body = String::new();
    for line in rest.lines() {
        let trimmed = line.trim_end();
        if let Some(continued) = trimmed.strip_suffix('\\') {
            body.push_str(continued);
            body.push(' ');
        } else {
            body.push_str(trimmed);
            break;
        }
    }
    body
}

/// Each `V(name, rest)` of a macro's body, its arguments' parentheses
/// balanced.
fn v_entries(body: &str) -> Vec<(String, String)> {
    let mut entries = Vec::new();
    let mut rest = body;
    while let Some(at) = rest.find("V(") {
        let arguments = &rest[at + 2..];
        let mut depth = 1;
        let end = arguments
            .char_indices()
            .find(|&(_, c)| {
                depth += match c {
                    '(' => 1,
                    ')' => -1,
                    _ => 0,
                };
                depth == 0
            })
            .map_or_else(|| panic!("an unbalanced V( in {body:?}"), |(end, _)| end);
        let (name, value) = arguments[..end]
            .split_once(',')
            .unwrap_or_else(|| panic!("V({}) has one argument", &arguments[..end]));
        entries.push((name.trim().to_owned(), value.trim().to_owned()));
        rest = &arguments[end..];
    }
    entries
}

/// The body of the class or struct named `name`, between its braces.
fn class_body<'a>(text: &'a str, name: &str) -> Option<&'a str> {
    let pattern = format!(
        r"(?:class|struct)\s+(?:[A-Z_0-9]+(?:\([A-Z_0-9]+\))?\s+)*{name}\b\s*(?:final\s*)?(?::[^;{{]*)?\{{"
    );
    let open = Regex::new(&pattern).expect("a pattern").find(text)?.end();
    let mut depth = 1;
    let body = &text[open..];
    let end = body.char_indices().find(|&(_, c)| {
        depth += match c {
            '{' => 1,
            '}' => -1,
            _ => 0,
        };
        depth == 0
    })?;
    Some(&body[..end.0])
}

/// The expression a namespace constant `const T name = value;` or
/// `constexpr T name = value;` has, the first a file defines.
fn namespace_constant(text: &str, name: &str) -> Option<String> {
    let pattern =
        format!(r"(?m)^(?:static\s+)?(?:const|constexpr)\s+[\w:]+\s+{name}\s*=\s*([^;]+);");
    Some(
        Regex::new(&pattern).ok()?.captures(text)?[1]
            .trim()
            .to_owned(),
    )
}

/// The value of a C++ integer expression: literals, names `lookup` gives,
/// unary minus, `* + - << |`, parentheses, and `condition ? a : b`.
pub(super) fn eval(expression: &str, lookup: &dyn Fn(&str) -> Option<i64>) -> Option<i64> {
    let tokens = Regex::new(r"[A-Za-z_][\w:]*|0x[0-9a-fA-F]+[uUlL]*|\d+[uUlL]*|<<|[-+*|()?:]")
        .ok()?
        .find_iter(expression)
        .map(|token| token.as_str())
        .collect::<Vec<_>>();
    let joined: String = tokens.concat();
    let spaceless: String = expression.chars().filter(|c| !c.is_whitespace()).collect();
    if joined != spaceless {
        return None;
    }

    let mut parser = Parser {
        tokens,
        at: 0,
        lookup,
    };
    let value = parser.ternary()?;
    (parser.at == parser.tokens.len()).then_some(value)
}

struct Parser<'a> {
    tokens: Vec<&'a str>,
    at: usize,
    lookup: &'a dyn Fn(&str) -> Option<i64>,
}

impl<'a> Parser<'a> {
    fn peek(&self) -> Option<&'a str> {
        self.tokens.get(self.at).copied()
    }

    fn eat(&mut self, token: &str) -> bool {
        let found = self.peek() == Some(token);
        self.at += usize::from(found);
        found
    }

    fn ternary(&mut self) -> Option<i64> {
        let condition = self.or()?;
        if !self.eat("?") {
            return Some(condition);
        }
        let then = self.ternary()?;
        self.eat(":").then_some(())?;
        let otherwise = self.ternary()?;
        Some(if condition != 0 { then } else { otherwise })
    }

    fn or(&mut self) -> Option<i64> {
        let mut value = self.shift()?;
        while self.eat("|") {
            value |= self.shift()?;
        }
        Some(value)
    }

    fn shift(&mut self) -> Option<i64> {
        let mut value = self.sum()?;
        while self.eat("<<") {
            value = value.checked_shl(u32::try_from(self.sum()?).ok()?)?;
        }
        Some(value)
    }

    fn sum(&mut self) -> Option<i64> {
        let mut value = self.product()?;
        loop {
            if self.eat("+") {
                value = value.checked_add(self.product()?)?;
            } else if self.eat("-") {
                value = value.checked_sub(self.product()?)?;
            } else {
                return Some(value);
            }
        }
    }

    fn product(&mut self) -> Option<i64> {
        let mut value = self.unary()?;
        while self.eat("*") {
            value = value.checked_mul(self.unary()?)?;
        }
        Some(value)
    }

    fn unary(&mut self) -> Option<i64> {
        if self.eat("-") {
            return self.unary()?.checked_neg();
        }
        if self.eat("(") {
            let value = self.ternary()?;
            return self.eat(")").then_some(value);
        }

        let token = self.peek()?;
        self.at += 1;
        let digits = token.trim_end_matches(['u', 'U', 'l', 'L']);
        if let Some(hex) = digits.strip_prefix("0x") {
            return u64::from_str_radix(hex, 16)
                .ok()
                .and_then(|value| i64::try_from(value).ok());
        }
        if digits.starts_with(|c: char| c.is_ascii_digit()) {
            return digits.parse().ok();
        }
        (self.lookup)(token)
    }
}

#[test]
fn expressions_evaluate_as_cpp_does() {
    let lookup = |name: &str| match name {
        "kTen" => Some(10),
        "A::kOne" | "ON_BOOL" => Some(1),
        _ => None,
    };
    assert_eq!(eval("1 << 1", &lookup), Some(2));
    assert_eq!(eval("A::kOne + 1", &lookup), Some(2));
    assert_eq!(eval("ON_BOOL ? 12 : 8", &lookup), Some(12));
    assert_eq!(eval("-1", &lookup), Some(-1));
    assert_eq!(eval("0xffffull", &lookup), Some(0xffff));
    assert_eq!(eval("(kTen - 2) * 2 | 1", &lookup), Some(17));
    assert_eq!(eval("kUnknown + 1", &lookup), None);
    assert_eq!(eval("sizeof(int)", &lookup), None);
}

#[test]
fn field_lists_fail_on_what_they_do_not_know() {
    let entries = v_entries(
        "V(kAOffset, kTaggedSize) V(kBOffset, X ? 0 : kIntSize) V(kUnalignedSize, OBJECT_POINTER_PADDING(kUnalignedSize))",
    );
    assert_eq!(
        entries,
        [
            ("kAOffset".to_owned(), "kTaggedSize".to_owned()),
            ("kBOffset".to_owned(), "X ? 0 : kIntSize".to_owned()),
            (
                "kUnalignedSize".to_owned(),
                "OBJECT_POINTER_PADDING(kUnalignedSize)".to_owned()
            ),
        ]
    );
    let headers = Headers {
        files: BTreeMap::new(),
        config: BTreeMap::from([("X", true)]),
    };
    assert_eq!(headers.field_size("X ? 0 : 4ignored"), 0);
    let unknown = std::panic::catch_unwind(|| headers.field_size("UNKNOWN_BOOL ? 0 : 0"));
    assert!(unknown.is_err(), "an unknown configuration fails");
    let odd = std::panic::catch_unwind(|| headers.field_size("sizeof(Foo)"));
    assert!(odd.is_err(), "an unknown size fails");
}
