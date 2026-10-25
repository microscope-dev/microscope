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

const FILES: [&str; 53] = [
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
    "src/objects/code-inl.h",
    "src/codegen/safepoint-table-base.h",
    "src/codegen/safepoint-table.h",
    "src/codegen/maglev-safepoint-table.h",
    "src/deoptimizer/translation-opcode.h",
    "src/base/vlq.h",
    "src/flags/flag-definitions.h",
    "src/utils/utils.h",
    "src/objects/heap-object.h",
    "src/objects/primitive-heap-object.h",
    "src/objects/instance-type.h",
    "src/objects/bigint.h",
    "src/objects/struct.h",
    "src/objects/oddball.h",
    "src/objects/name.h",
    "src/objects/elements-kind.h",
    "src/objects/function-kind.h",
    "src/objects/ordered-hash-table.h",
    "src/objects/dictionary-inl.h",
    "src/objects/contexts.h",
    "src/objects/contexts-inl.h",
    "src/objects/map-inl.h",
    "src/objects/call-site-info.h",
    "src/objects/script.h",
    "include/v8-promise.h",
    "src/execution/frame-constants.h",
    "src/objects/js-generator.h",
    "src/builtins/builtins-promise.h",
    "src/execution/isolate-data.h",
    "src/execution/isolate-data-fields.h",
    "src/execution/stack-guard.h",
    "src/execution/thread-local-top.h",
    "src/execution/thread-id.h",
    "src/execution/x64/frame-constants-x64.h",
    "src/codegen/x64/constants-x64.h",
    "include/v8-unwinder.h",
    "include/v8-callbacks.h",
    "src/execution/isolate.h",
];

/// The headers a version may lack: 12.4 has no JS dispatch table, and
/// before 14.6 `isolate-data.h` lists the isolate's fields itself.
fn optional(file: &str, leaptiering: bool) -> bool {
    match file {
        "src/sandbox/js-dispatch-table.h" => !leaptiering,
        "src/execution/isolate-data-fields.h" => true,
        _ => false,
    }
}

impl Headers {
    pub(super) fn read(v8: &Path, leaptiering: bool) -> Self {
        let mut files = BTreeMap::new();
        for file in FILES {
            let path = v8.join(file);
            match std::fs::read_to_string(&path) {
                Ok(text) => {
                    files.insert(file, release(&strip_comments(&text)));
                }
                Err(error) => assert!(
                    optional(file, leaptiering),
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

    pub(super) fn file(&self, file: &str) -> &str {
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

        if name == "kSystemPointerSize"
            || self
                .file("src/common/globals.h")
                .contains(&format!("constexpr int {name} = sizeof("))
        {
            return Some(self.size(name));
        }

        FILES
            .iter()
            .filter(|own| self.files.contains_key(**own))
            .find_map(|own| namespace_constant(self.file(own), name))
            .map(|expression| {
                // The API's sizes, as `kApiSystemPointerSize = sizeof(void*)`.
                expression
                    .strip_prefix("sizeof(")
                    .and_then(|ty| ty.strip_suffix(')'))
                    .map_or_else(
                        || self.eval(file, &[], &expression),
                        |ty| type_size(self.file("src/common/globals.h"), ty.trim()),
                    )
            })
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
        self.enumerator_values(file, &self.enum_body(file, head))
    }

    /// The body of the enum whose declaration is `head`, in `file`, between
    /// its braces. The file's first `head` must be a definition.
    fn enum_body(&self, file: &str, head: &str) -> String {
        let text = self.file(file);
        let (before, body) = text
            .split_once(head)
            .and_then(|(_, rest)| rest.split_once('{'))
            .and_then(|(before, rest)| Some((before, rest.split_once('}')?.0)))
            .unwrap_or_else(|| panic!("{file} declares no {head}"));
        assert!(
            !before.contains(';'),
            "{file}'s first {head} is a declaration"
        );
        body.to_owned()
    }

    /// The values of the enumerators of an enum's `body`, which may name
    /// those before them.
    fn enumerator_values(&self, file: &str, body: &str) -> BTreeMap<String, i64> {
        assert!(
            !body.contains('#'),
            "{file}'s enum has a preprocessor directive: {body:?}"
        );
        let mut values = BTreeMap::new();
        let mut next = 0;
        for enumerator in body
            .split(',')
            .map(str::trim)
            .filter(|item| !item.is_empty())
        {
            let (name, value) = match enumerator.split_once('=') {
                Some((name, value)) => {
                    let lookup = |name: &str| {
                        values
                            .get(name)
                            .copied()
                            .or_else(|| self.lookup(file, &[], name))
                    };
                    let value = eval(value, &lookup)
                        .unwrap_or_else(|| panic!("{file}: cannot evaluate {value:?}"));
                    (name.trim(), value)
                }
                None => (enumerator, next),
            };
            let previous = values.insert(name.to_owned(), value);
            assert!(previous.is_none(), "{file} enumerates {name} twice");
            next = value + 1;
        }
        values
    }

    /// The size of a heap object's header, `HeapObjectLayout`, whose one
    /// member is its map, a tagged value.
    fn heap_object_header_size(&self) -> i64 {
        let file = "src/objects/heap-object.h";
        assert!(
            self.contains(
                file,
                "static_assert(sizeof(HeapObjectLayout) == kTaggedSize);"
            ),
            "HeapObjectLayout is no longer a tagged value"
        );
        let members = data_members(self.class_text(file, "HeapObjectLayout"));
        assert_eq!(
            members,
            ["TaggedMember<Map> map_;"],
            "HeapObjectLayout's members"
        );
        self.size("kTaggedSize")
    }

    /// The fields of `BigIntBase` (`bigint.h`), which Torque has no readers
    /// of: after the `HeapObject` header, a 32-bit bit field, 4 bytes of
    /// padding where `BIGINT_NEEDS_PADDING` (a 64-bit build without pointer
    /// compression), then the digits.
    pub(super) fn bigint_fields(&self) -> Vec<(String, i64)> {
        let file = "src/objects/bigint.h";
        let padding = Regex::new(
            r"#if V8_HOST_ARCH_64_BIT && !V8_COMPRESS_POINTERS\s*\n#define BIGINT_NEEDS_PADDING 1\s*\n#endif",
        )
        .expect("a pattern");
        assert!(
            padding.is_match(self.file(file)),
            "bigint.h no longer pads a 64-bit uncompressed BigInt"
        );
        assert!(
            self.contains(
                file,
                "V8_OBJECT class BigIntBase : public PrimitiveHeapObject {"
            ) && self.contains(
                "src/objects/primitive-heap-object.h",
                "static_assert(sizeof(PrimitiveHeapObject) == sizeof(HeapObjectLayout));"
            ),
            "BigIntBase does not follow HeapObject's header"
        );
        let body = normalized(self.class_text(file, "BigIntBase"));
        let tail = "std::atomic_uint32_t bitfield_; #ifdef BIGINT_NEEDS_PADDING \
                    char padding_[4]; #endif \
                    FLEXIBLE_ARRAY_MEMBER(UnalignedValueMember<digit_t>, raw_digits);";
        assert!(
            body.ends_with(tail)
                && data_members(self.class_text(file, "BigIntBase"))
                    == ["std::atomic_uint32_t bitfield_;", "char padding_[4];"],
            "BigIntBase's members are not its bit field, padding, and digits"
        );
        assert!(
            self.contains(file, "using digit_t = uintptr_t;"),
            "a BigInt's digit is no longer a uintptr_t"
        );

        let bitfield = self.heap_object_header_size();
        let digits = bitfield + 4 + 4;
        assert_eq!(digits % 8, 0, "a BigInt's digits are not 8-byte aligned");
        vec![
            ("BigIntBase::Bitfield".to_owned(), bitfield),
            ("BigIntBase::Digits".to_owned(), digits),
        ]
    }

    /// An `AccessorPair`'s tagged getter and setter after the `HeapObject`
    /// header. From 14.6 `struct.h` lays them out in C++ and Torque has no
    /// readers of them. `None` where Torque defines the class, as before 14.6.
    pub(super) fn accessor_pair_fields(&self) -> Option<Vec<(String, i64)>> {
        let file = "src/objects/struct.h";
        if self.contains(
            file,
            "class AccessorPair : public TorqueGeneratedAccessorPair<AccessorPair, Struct> {",
        ) {
            return None;
        }

        assert!(
            self.contains(file, "V8_OBJECT class AccessorPair : public StructLayout {")
                && self.contains(
                    file,
                    "V8_OBJECT class StructLayout : public HeapObjectLayout {"
                )
                && self.contains(
                    file,
                    "static_assert(sizeof(StructLayout) == sizeof(HeapObjectLayout));"
                )
                && data_members(self.class_text(file, "StructLayout")).is_empty(),
            "AccessorPair does not follow HeapObject's header"
        );
        let members = data_members(self.class_text(file, "AccessorPair"));
        assert_eq!(
            members,
            [
                "TaggedMember<Object> getter_;",
                "TaggedMember<Object> setter_;"
            ],
            "AccessorPair's members"
        );
        let header = self.heap_object_header_size();
        Some(vec![
            ("AccessorPair::Getter".to_owned(), header),
            (
                "AccessorPair::Setter".to_owned(),
                header + self.size("kTaggedSize"),
            ),
        ])
    }

    /// The layout of `class`, an `OrderedHashTable<class, entrysize>`, as
    /// `ordered-hash-table.h` gives it, in elements of its `FixedArray`, by
    /// each constant's or function's name.
    pub(super) fn ordered_hash_table(&self, class: &str) -> Vec<(String, i64)> {
        let file = "src/objects/ordered-hash-table.h";
        let pattern = format!(
            r"class\s+V8_EXPORT_PRIVATE\s+{class}\s*:\s*public\s+OrderedHashTable<{class},\s*(\d+)>\s*\{{"
        );
        let entry_size: i64 = Regex::new(&pattern)
            .expect("a pattern")
            .captures(self.file(file))
            .unwrap_or_else(|| panic!("{file} declares no {class}"))[1]
            .parse()
            .expect("an entry size");
        let base = normalized(self.class_text(file, "OrderedHashTable"));
        for expected in [
            "static const int kEntrySize = entrysize + 1;",
            "static const int kChainOffset = entrysize;",
            "static constexpr int NumberOfElementsIndex() { return Derived::kPrefixSize; }",
            "static constexpr int NumberOfDeletedElementsIndex() { \
             return NumberOfElementsIndex() + 1; }",
            "static constexpr int NumberOfBucketsIndex() { \
             return NumberOfDeletedElementsIndex() + 1; }",
            "static constexpr int HashTableStartIndex() { return NumberOfBucketsIndex() + 1; }",
            "int EntryToIndexRaw(int entry) { \
             return HashTableStartIndex() + NumberOfBuckets() + (entry * kEntrySize); }",
        ] {
            assert!(
                base.contains(expected),
                "OrderedHashTable no longer says {expected:?}"
            );
        }

        let constant = |classes: &[&str], name: &str| {
            self.class_constant(file, classes, name)
                .unwrap_or_else(|| panic!("{classes:?} has no {name}"))
        };
        let prefix = constant(&[class], "kPrefixSize");
        let mut layout = vec![
            ("kEntrySize", entry_size + 1),
            ("kChainOffset", entry_size),
            ("NumberOfElementsIndex()", prefix),
            ("NumberOfDeletedElementsIndex()", prefix + 1),
            ("NumberOfBucketsIndex()", prefix + 2),
            ("HashTableStartIndex()", prefix + 3),
            (
                "kClearedTableSentinel",
                constant(&["OrderedHashTable"], "kClearedTableSentinel"),
            ),
            ("kNotFound", constant(&["OrderedHashTable"], "kNotFound")),
        ];
        if class == "OrderedHashMap" {
            layout.push(("kValueOffset", constant(&[class], "kValueOffset")));
        }
        layout
            .into_iter()
            .map(|(name, value)| (format!("{class}::{name}"), value))
            .collect()
    }

    /// Each elements kind of `enum ElementsKind` (`elements-kind.h`), with
    /// the typed arrays' kinds it expands from `TYPED_ARRAYS` and
    /// `RAB_GSAB_TYPED_ARRAYS`, as a build without pointer compression
    /// numbers them.
    pub(super) fn elements_kinds(&self) -> BTreeMap<String, i64> {
        let file = "src/objects/elements-kind.h";
        let body = normalized(&self.enum_body(file, "enum ElementsKind : uint8_t"));
        let typed = "#define TYPED_ARRAY_ELEMENTS_KIND(Type, type, TYPE, ctype) TYPE##_ELEMENTS, \
                     TYPED_ARRAYS(TYPED_ARRAY_ELEMENTS_KIND) \
                     RAB_GSAB_TYPED_ARRAYS(TYPED_ARRAY_ELEMENTS_KIND) \
                     #undef TYPED_ARRAY_ELEMENTS_KIND";
        let pointer = "#ifdef V8_COMPRESS_POINTERS \
                       SYSTEM_POINTER_ELEMENTS = PACKED_DOUBLE_ELEMENTS, #else \
                       SYSTEM_POINTER_ELEMENTS = PACKED_ELEMENTS, #endif";
        assert!(
            body.matches(typed).count() == 1 && body.matches(pointer).count() == 1,
            "ElementsKind no longer expands its typed arrays as microscope does: {body:?}"
        );

        let macros = super::bytecodes::defines(self.file(file));
        let kinds: Vec<String> = ["TYPED_ARRAYS", "RAB_GSAB_TYPED_ARRAYS"]
            .into_iter()
            .flat_map(|list| super::bytecodes::expand(&macros, list))
            .map(|entry| {
                let [_, _, name, _] = entry.as_slice() else {
                    panic!("a typed array {entry:?}");
                };
                format!("{name}_ELEMENTS")
            })
            .collect();
        let body = body
            .replace(typed, &(kinds.join(", ") + ","))
            .replace(pointer, "SYSTEM_POINTER_ELEMENTS = PACKED_ELEMENTS,");
        self.enumerator_values(file, &body)
    }

    /// The slots of a context, `Context::Field` (`contexts.h`), with the
    /// native context's it expands from `NATIVE_CONTEXT_FIELDS`, and how a
    /// context reaches its native context: through its map's
    /// `ConstructorOrBackPointerOrNativeContext`.
    pub(super) fn context_slots(&self) -> BTreeMap<String, i64> {
        let file = "src/objects/contexts.h";
        assert_eq!(
            self.file(file).matches("enum Field {").count(),
            1,
            "{file} has one enum Field"
        );
        let body = normalized(&self.enum_body(file, "enum Field"));
        let native = "#define NATIVE_CONTEXT_SLOT(index, type, name) index, \
                      NATIVE_CONTEXT_FIELDS(NATIVE_CONTEXT_SLOT) #undef NATIVE_CONTEXT_SLOT";
        assert!(
            body.starts_with(&format!(
                "SCOPE_INFO_INDEX, PREVIOUS_INDEX, EXTENSION_INDEX, {native}"
            )),
            "a context's slots no longer begin as microscope reads them: {body:?}"
        );
        let macros = super::bytecodes::defines(self.file(file));
        let slots: Vec<String> = super::bytecodes::expand(&macros, "NATIVE_CONTEXT_FIELDS")
            .into_iter()
            .map(|entry| {
                // The type may hold commas the expander splits on, but the
                // index, the first argument, is a plain name.
                let index = entry
                    .first()
                    .filter(|index| {
                        entry.len() >= 3
                            && index
                                .chars()
                                .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
                    })
                    .unwrap_or_else(|| panic!("a native context's slot {entry:?}"));
                index.clone()
            })
            .collect();

        for (file, expected) in [
            (
                "src/objects/contexts-inl.h",
                "Tagged<NativeContext> Context::native_context() const { \
                 return this->map()->native_context(); }",
            ),
            (
                "src/objects/map-inl.h",
                "ACCESSORS_CHECKED(Map, native_context, Tagged<NativeContext>, \
                 kConstructorOrBackPointerOrNativeContextOffset,",
            ),
        ] {
            assert!(
                normalized(self.file(file)).contains(expected),
                "{file} no longer says {expected:?}"
            );
        }
        // From 14.6 the header defines what 12.4's and 13.6's contexts.cc
        // does: a native context's extension is its global object.
        let inline = normalized(self.file("src/objects/contexts-inl.h"));
        let global = "Tagged<JSGlobalObject> NativeContext::global_object() const {";
        if inline.contains(global) {
            assert!(
                inline.contains(&format!(
                    "{global} return Cast<JSGlobalObject>(extension()); }}"
                )),
                "a native context's global object is not its extension"
            );
        }

        self.enumerator_values(file, &body.replace(native, &(slots.join(", ") + ",")))
    }

    /// The slots of the contexts of `PromiseBuiltins`' closures
    /// (`builtins-promise.h`), each enum's first at a context's first
    /// local, `min_context_slots`, as `Context::MIN_CONTEXT_SLOTS` puts it.
    pub(super) fn promise_context_slots(&self, min_context_slots: i64) -> BTreeMap<String, i64> {
        let file = "src/builtins/builtins-promise.h";
        let mut slots = BTreeMap::new();
        for head in [
            "enum PromiseResolvingFunctionContextSlot",
            "enum PromiseAllResolveElementContextSlots",
            "enum PromiseAnyRejectElementContextSlots",
        ] {
            let body = self.enum_body(file, head);
            assert!(
                body.trim_start()
                    .split_once(',')
                    .is_some_and(|(first, _)| first.ends_with("= Context::MIN_CONTEXT_SLOTS")),
                "{file}'s {head} no longer begins at a context's first local: {body:?}"
            );
            let body = body.replace("Context::MIN_CONTEXT_SLOTS", &min_context_slots.to_string());
            slots.extend(self.enumerator_values(file, &body));
        }
        slots
    }

    /// The bits of the hole in a double array, `kHoleNanInt64`, as
    /// `globals.h` defines them on x64. The first definitions of the hole's
    /// halves are for MIPS; the `#else` branch covers every other target.
    pub(super) fn hole_nan_int64(&self) -> i64 {
        let globals = self.file("src/common/globals.h");
        let halves = "constexpr uint32_t kHoleNanUpper32 = ";
        let (before, after) = globals
            .split_once(&format!("#else\n{halves}"))
            .expect("globals.h defines the hole's halves under #else");
        let guard = before[..before.find(halves).expect("MIPS's halves")]
            .lines()
            .rev()
            .find(|line| line.starts_with("#if"))
            .expect("a condition");
        assert!(
            guard.starts_with("#if (V8_TARGET_ARCH_MIPS64 &&"),
            "the hole's first halves are under {guard:?}"
        );
        let half = |name: &str| {
            let text = format!("{halves}{after}");
            let expression = namespace_constant(&text, name)
                .unwrap_or_else(|| panic!("globals.h's #else defines no {name}"));
            u32::try_from(self.eval("src/common/globals.h", &[], &expression))
                .expect("a 32-bit half")
        };
        let expression = namespace_constant(globals, "kHoleNanInt64").expect("kHoleNanInt64");
        assert_eq!(
            normalized(&expression),
            "(static_cast<uint64_t>(kHoleNanUpper32) << 32) | kHoleNanLower32",
            "kHoleNanInt64 is no longer its halves"
        );
        let bits = (u64::from(half("kHoleNanUpper32")) << 32) | u64::from(half("kHoleNanLower32"));
        i64::from_ne_bytes(bits.to_ne_bytes())
    }

    /// Whether `GlobalDictionary::CellAt` reads an entry's property cell
    /// from its key (`dictionary-inl.h`).
    pub(super) fn global_dictionary_cell_is_key(&self) -> bool {
        let pattern = Regex::new(
            r"Tagged<PropertyCell> GlobalDictionary::CellAt\(PtrComprCageBase cage_base,\s*InternalIndex entry\) \{\s*DCHECK\(IsPropertyCell\(KeyAt\(cage_base, entry\), cage_base\)\);\s*return (?:PropertyCell::cast|Cast<PropertyCell>)\(KeyAt\(cage_base, entry\)\);\s*\}",
        )
        .expect("a pattern");
        pattern.is_match(self.file("src/objects/dictionary-inl.h"))
    }

    /// Each `class::name` of `names`, a constant of a frame class in
    /// `frame-constants.h`, with the header's frame macros such as
    /// `DEFINE_STANDARD_FRAME_SIZES` expanded. A name is looked up as C++
    /// does: in its class, then the classes it derives from, then
    /// `globals.h`'s constants and the configuration, which has no constant
    /// pool for `kCPSlotSize`.
    pub(super) fn frame_constants(&self, names: &[(&str, &str)]) -> Vec<(String, i64)> {
        let file = "src/execution/frame-constants.h";
        let text = self.file(file);
        let macros = super::bytecodes::defines(text);
        let call = Regex::new(r"(\w+)\s*\(").expect("a pattern");
        let mut bodies = BTreeMap::new();
        for (class, parent) in FRAME_CLASSES {
            assert!(
                text.contains(&format!("class {class} : public {parent} {{")),
                "{file}'s {class} no longer derives from {parent}"
            );
            let body = expand_macros(self.class_text(file, class), &macros);
            assert!(
                !body.contains('#'),
                "{file}'s {class} has a conditional: {body:?}"
            );
            if let Some(left) = call
                .captures_iter(&body)
                .find(|captures| macros.contains_key(&captures[1]))
            {
                panic!("{file}'s {class} still calls {}", &left[1]);
            }
            bodies.insert(class, body);
        }

        names
            .iter()
            .map(|&(class, name)| {
                let value = self
                    .frame_constant(&bodies, class, name, 0)
                    .unwrap_or_else(|| panic!("{file} gives {class} no {name}"));
                (format!("{class}::{name}"), value)
            })
            .collect()
    }

    /// The value of `name` in `class`, from `frame_constants`' expanded
    /// `bodies`: its definition in the class or the nearest class it derives
    /// from, evaluated in that class.
    fn frame_constant(
        &self,
        bodies: &BTreeMap<&str, String>,
        class: &str,
        name: &str,
        depth: usize,
    ) -> Option<i64> {
        assert!(depth < 32, "{class}::{name} is defined through itself");
        let file = "src/execution/frame-constants.h";
        let pattern = format!(r"static\s+constexpr\s+int\s+{name}\s*=\s*([^;]+);");
        let pattern = Regex::new(&pattern).expect("a pattern");
        let mut own = Some(class);
        while let Some(class) = own {
            let body = bodies.get(class)?;
            let definitions: Vec<_> = pattern.captures_iter(body).collect();
            assert!(definitions.len() <= 1, "{class} defines {name} twice");
            if let Some(definition) = definitions.first() {
                let lookup = |used: &str| match used.rsplit_once("::") {
                    Some((owner, member)) if bodies.contains_key(owner) => {
                        self.frame_constant(bodies, owner, member, depth + 1)
                    }
                    Some(_) => self.lookup(file, &[], used),
                    None => self
                        .frame_constant(bodies, class, used, depth + 1)
                        .or_else(|| self.lookup(file, &[], used)),
                };
                let expression = &definition[1];
                return Some(eval(expression, &lookup).unwrap_or_else(|| {
                    panic!("{file}: cannot evaluate {class}::{name} = {expression:?}")
                }));
            }
            own = FRAME_CLASSES
                .iter()
                .find(|(own, _)| *own == class)
                .map(|(_, parent)| *parent)
                .filter(|parent| bodies.contains_key(parent));
        }
        None
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

    /// The body of `class` in `file`, without its comments.
    pub(super) fn class_text(&self, file: &str, class: &str) -> &str {
        class_body(self.file(file), class).unwrap_or_else(|| panic!("{file} declares no {class}"))
    }

    /// The offsets `class`'s header layout gives its fields, by their
    /// `kNameOffset` names and `kHeaderSize`: its `static constexpr`
    /// constants, as 12.4 gives a safepoint table's, or its `FIELD_LIST`
    /// from 0, as 13.6 does.
    pub(super) fn header_offsets(&self, file: &str, class: &str) -> BTreeMap<String, i64> {
        let body = self.class_text(file, class);
        if !body.contains("#define FIELD_LIST(V)") {
            let pattern = Regex::new(r"static\s+constexpr\s+int\s+(k\w*(?:Offset|HeaderSize))\s*=")
                .expect("a pattern");
            return pattern
                .captures_iter(body)
                .map(|captures| {
                    let name = captures[1].to_owned();
                    let value = self
                        .class_constant(file, &[class], &name)
                        .unwrap_or_else(|| panic!("{class}::{name}"));
                    (name, value)
                })
                .collect();
        }

        assert!(
            body.contains("DEFINE_FIELD_OFFSET_CONSTANTS(0, FIELD_LIST)"),
            "{class}'s fields do not begin at 0"
        );
        let mut offsets = BTreeMap::new();
        let mut offset = 0;
        for (name, size) in v_entries(&macro_body(body, "#define FIELD_LIST(V)")) {
            offsets.insert(name.clone(), offset);
            if name == "kHeaderSize" {
                return offsets;
            }
            offset += size
                .strip_prefix("sizeof(")
                .and_then(|ty| ty.strip_suffix(')'))
                .map_or_else(|| self.size(&size), |alias| self.alias_size(alias));
        }
        panic!("{class}'s field list has no kHeaderSize")
    }

    /// The size of a type `safepoint-table-base.h` names, as
    /// `SafepointTableStackSlotsField_t`.
    fn alias_size(&self, alias: &str) -> i64 {
        let base = self.file("src/codegen/safepoint-table-base.h");
        let pattern = format!(r"using\s+{alias}\s*=\s*(\w+);");
        let ty = Regex::new(&pattern)
            .expect("a pattern")
            .captures(base)
            .map_or_else(
                || panic!("no type {alias}"),
                |captures| captures[1].to_owned(),
            );
        type_size(self.file("src/common/globals.h"), &ty)
    }

    /// `TRANSLATION_OPCODE_LIST`, each opcode by its name with its operand
    /// count, in their order, which numbers them; and how many of the first
    /// are JS frames' and frames'. WebAssembly's are among them: every
    /// build defines `V8_ENABLE_WEBASSEMBLY`.
    pub(super) fn translation_opcodes(&self) -> (Vec<(String, i64)>, usize, usize) {
        let file = "src/deoptimizer/translation-opcode.h";
        let text = self.file(file);
        let list = |name: &str| self.opcode_list(&macro_body(text, &format!("#define {name}(V)")));
        let js_frames = list("TRANSLATION_JS_FRAME_OPCODE_LIST");
        let frames = list("TRANSLATION_FRAME_OPCODE_LIST");
        let all = list("TRANSLATION_OPCODE_LIST");
        assert!(
            all.starts_with(&js_frames) && all[js_frames.len()..].starts_with(&frames),
            "the opcode list does not begin with its frames'"
        );
        for expected in [
            "static constexpr int kNumTranslationOpcodes =\n    0 TRANSLATION_OPCODE_LIST(PLUS_ONE);",
            "static constexpr int kNumTranslationFrameOpcodes =\n    \
             kNumTranslationJsFrameOpcodes TRANSLATION_FRAME_OPCODE_LIST(PLUS_ONE);",
            "return static_cast<int>(o) < kNumTranslationFrameOpcodes;",
            "return static_cast<int>(o) < kNumTranslationJsFrameOpcodes;",
            "#define CASE(name, ...) name,\n  TRANSLATION_OPCODE_LIST(CASE)",
        ] {
            assert!(
                text.contains(expected),
                "{file} no longer says {expected:?}"
            );
        }
        let (js_frames, frames) = (js_frames.len(), js_frames.len() + frames.len());
        (all, js_frames, frames)
    }

    /// The entries of an opcode list's body: `V(NAME, count)`,
    /// `IF_WASM(V, NAME, count)`, and other lists' `NAME(V)`.
    fn opcode_list(&self, body: &str) -> Vec<(String, i64)> {
        let text = self.file("src/deoptimizer/translation-opcode.h");
        let entry =
            Regex::new(r"IF_WASM\(V,\s*(\w+),\s*(\d+)\)|V\((\w+),\s*(\d+)\)|(\w+_LIST)\(V\)")
                .expect("a pattern");
        assert!(
            entry.replace_all(body, "").trim().is_empty(),
            "an opcode list has more than its entries: {body:?}"
        );
        let mut opcodes = Vec::new();
        for captures in entry.captures_iter(body) {
            if let Some(list) = captures.get(5) {
                let head = format!("#define {}(V)", list.as_str());
                opcodes.extend(self.opcode_list(&macro_body(text, &head)));
                continue;
            }
            let (name, count) = match (captures.get(1), captures.get(3)) {
                (Some(name), _) => (name, &captures[2]),
                (None, Some(name)) => (name, &captures[4]),
                (None, None) => panic!("an opcode entry {:?}", &captures[0]),
            };
            opcodes.push((name.as_str().to_owned(), count.parse().expect("a count")));
        }
        opcodes
    }

    /// What a running thread's frames are read from: where the isolate
    /// keeps its `ThreadLocalTop`, its fast C call's frame pointer and
    /// return address, and whether its stack is iterable; the root
    /// register's bias; `ThreadLocalTop`'s members; the VM state of a
    /// collection; and the slots of x64's entry frame. Each by the name
    /// [`crate::image::v8::Constant`] gives it.
    pub(super) fn isolate_constants(&self) -> Vec<(String, i64)> {
        let internals = "include/v8-internal.h";
        let internal = |name: &str| -> i64 {
            let body = self.class_text(internals, "Internals");
            let pattern = format!(r"static\s+(?:constexpr|const)\s+int\s+{name}\s*=");
            let definitions = Regex::new(&pattern)
                .expect("a pattern")
                .find_iter(body)
                .count();
            assert_eq!(
                definitions, 1,
                "{internals} defines Internals::{name} {definitions} times"
            );
            self.class_constant(internals, &["Internals"], name)
                .unwrap_or_else(|| panic!("{internals}: Internals::{name}"))
        };

        let mut constants = Vec::new();
        for name in [
            "kIsolateThreadLocalTopOffset",
            "kIsolateFastCCallCallerFpOffset",
            "kIsolateFastCCallCallerPcOffset",
        ] {
            constants.push((format!("Internals::{name}"), internal(name)));
        }

        // The stack guard is the second field, and the byte flags follow it.
        let stack_guard = self
            .class_constant(
                "src/execution/stack-guard.h",
                &["StackGuard"],
                "kSizeInBytes",
            )
            .expect("StackGuard::kSizeInBytes");
        assert_eq!(
            stack_guard,
            internal("kStackGuardSize"),
            "the stack guard's sizes disagree"
        );
        assert_eq!(
            internal("kVariousBooleanFlagsOffset"),
            internal("kIsolateStackGuardOffset") + stack_guard,
            "the isolate's flags do not follow its stack guard"
        );
        let flag = self.isolate_byte_flag("StackIsIterable");
        assert!(
            flag < internal("kNumberOfBooleanFlags"),
            "stack_is_iterable is not among the isolate's boolean flags"
        );
        constants.push((
            "IsolateData::kStackIsIterableOffset".to_owned(),
            internal("kVariousBooleanFlagsOffset") + flag,
        ));

        let bias = self
            .class_constant(
                "src/execution/isolate-data.h",
                &["IsolateData"],
                "kIsolateRootBias",
            )
            .expect("IsolateData::kIsolateRootBias");
        constants.push(("IsolateData::kIsolateRootBias".to_owned(), bias));

        let (size, members) = self.thread_local_top();
        assert_eq!(
            size,
            internal("kThreadLocalTopSize"),
            "ThreadLocalTop's sizes disagree"
        );
        for member in [
            "c_entry_fp_",
            "isolate_",
            "js_entry_sp_",
            "current_vm_state_",
        ] {
            let offset = members
                .get(member)
                .unwrap_or_else(|| panic!("ThreadLocalTop has no {member}"));
            constants.push((format!("offsetof(ThreadLocalTop, {member})"), *offset));
        }

        let item = self.entry_stack_item();
        for member in ["previous_isolate", "previous_item"] {
            let offset = item
                .get(member)
                .unwrap_or_else(|| panic!("Isolate::EntryStackItem has no {member}"));
            constants.push((
                format!("offsetof(Isolate::EntryStackItem, {member})"),
                *offset,
            ));
        }

        let states = self.enumerators("include/v8-unwinder.h", "enum StateTag");
        constants.push(("StateTag::GC".to_owned(), states["GC"]));

        constants.extend(self.entry_frame_constants());
        constants
    }

    /// Where `IsolateData`'s byte flag `name` is among the flags that
    /// follow its stack guard, which `ISOLATE_DATA_FIELDS` lists: 12.4
    /// names each `kNameOffset` or `kName`, later versions `Name`.
    fn isolate_byte_flag(&self, name: &str) -> i64 {
        let head = "#define ISOLATE_DATA_FIELDS(V)";
        let defining: Vec<&str> = [
            "src/execution/isolate-data.h",
            "src/execution/isolate-data-fields.h",
        ]
        .into_iter()
        .filter(|file| self.files.get(file).is_some_and(|text| text.contains(head)))
        .collect();
        let [file] = defining.as_slice() else {
            panic!("{} headers define ISOLATE_DATA_FIELDS", defining.len());
        };
        let list = macro_body(self.file(file), head);
        let bare = |entry: &str| -> String {
            let entry = entry.strip_suffix("Offset").unwrap_or(entry);
            match entry.strip_prefix('k') {
                Some(rest) if rest.starts_with(char::is_uppercase) => rest.to_owned(),
                _ => entry.to_owned(),
            }
        };
        // Nothing but V( entries may come between the guard and the flag:
        // another macro's fields would be unknown.
        let guard_at = list.find("V(").expect("a field");
        let entries = v_entries(&list[guard_at..]);
        let names: Vec<String> = entries.iter().map(|(entry, _)| bare(entry)).collect();
        assert_eq!(
            names.first().map(String::as_str),
            Some("CageBase"),
            "{file}: the first field"
        );
        assert_eq!(
            names.get(1).map(String::as_str),
            Some("StackGuard"),
            "{file}: the second field"
        );
        let at = names
            .iter()
            .position(|entry| entry == name)
            .unwrap_or_else(|| panic!("{file} lists no {name}"));
        let between = &list[guard_at..];
        let flag_text = format!("V({}", entries[at].0);
        let end = between.find(&flag_text).expect("the flag's entry");
        let listed = between[..end].matches("V(").count();
        assert_eq!(listed, at, "{file}: a macro lists fields before {name}");
        for (entry, rest) in &entries[2..=at] {
            let size = rest.split(',').next().expect("a size").trim();
            assert_eq!(
                size, "kUInt8Size",
                "{file}: {entry} before {name} is not a byte"
            );
        }
        i64::try_from(at - 2).expect("a small index")
    }

    /// `ThreadLocalTop`'s size and its data members' offsets, laid out as
    /// C++ lays out the types it declares: x64, without pointer
    /// compression. Any statement or type it does not know fails, as does
    /// a member under a preprocessor conditional, and a size that differs
    /// from the class's `kSizeInBytes`.
    pub(super) fn thread_local_top(&self) -> (i64, BTreeMap<String, i64>) {
        let file = "src/execution/thread-local-top.h";
        let (size, offsets) = self.lay_out(file, "ThreadLocalTop", Names::V8);
        let declared = self
            .class_constant(file, &["ThreadLocalTop"], "kSizeInBytes")
            .expect("ThreadLocalTop::kSizeInBytes");
        assert_eq!(
            size, declared,
            "{file}: the members take {size} bytes, not kSizeInBytes"
        );
        (size, offsets)
    }

    /// The offsets of the data members of `Isolate::EntryStackItem`, each
    /// item of the stack of entries an isolate keeps for the thread that
    /// entered it, as [`Self::thread_local_top`] lays them out. Its
    /// members are named without V8's trailing underscore.
    pub(super) fn entry_stack_item(&self) -> BTreeMap<String, i64> {
        self.lay_out("src/execution/isolate.h", "EntryStackItem", Names::Plain)
            .1
    }

    /// The size and the data members' offsets of `class`, which `file`
    /// declares, laid out as C++ lays out the types it declares: x64,
    /// without pointer compression. Any statement or type it does not
    /// know fails, as does a member under a preprocessor conditional.
    fn lay_out(&self, file: &str, class: &str, names: Names) -> (i64, BTreeMap<String, i64>) {
        let body = self.class_text(file, class);
        let mut offsets = BTreeMap::new();
        let (mut offset, mut alignment) = (0, 1);
        for (statement, conditional) in class_statements(body) {
            let Some((ty, name)) = Self::data_member(&statement, names) else {
                continue;
            };
            assert!(!conditional, "{file}: {name} is under a conditional");
            let size = self.member_type_size(&ty);
            offset = align(offset, size);
            alignment = alignment.max(size);
            let previous = offsets.insert(name.clone(), offset);
            assert!(previous.is_none(), "{file} declares {name} twice");
            offset += size;
        }
        (align(offset, alignment), offsets)
    }

    /// The type and name of a class's data member `statement`, `None` for
    /// a statement known to be no data member, failing on any other.
    fn data_member(statement: &str, names: Names) -> Option<(String, String)> {
        let statement = normalized(statement);
        let statement = statement.trim();
        for label in ["public:", "private:", "protected:"] {
            if let Some(rest) = statement.strip_prefix(label) {
                return Self::data_member(rest, names);
            }
        }
        if statement.is_empty()
            || statement.starts_with("static ")
            || statement.starts_with("static_assert(")
            || statement.starts_with("friend ")
            || statement.starts_with("using ")
        {
            return None;
        }
        // A member of a function's pointer type, written out, would read as
        // a function.
        assert!(
            !statement.contains("(*") && !statement.contains("(&"),
            "an unknown statement {statement:?}"
        );
        // A function, an operator among them: a template, a declaration,
        // or what is left of a definition whose body was dropped.
        let function = Regex::new(
            r"^(?:template\s*<[^;]*>\s*)?(?:[\w:<>,*&~]+\s+)*~?(?:\w+|operator\s*[^\s(\w]+)\s*\([^;]*\)\s*(?:const\s*)?(?:override\s*)?(?:=\s*(?:default|delete|0)\s*)?$",
        )
        .expect("a pattern");
        if function.is_match(statement) {
            return None;
        }
        // A member, its type with no parentheses, and its name V8's way or
        // plainly, perhaps with an initializer.
        let name = match names {
            Names::V8 => r"\w+_",
            Names::Plain => r"[a-z]\w*",
        };
        let member =
            Regex::new(&format!(r"^([^()=;]+?)\s*\b({name})\s*(?:=[^=].*)?$")).expect("a pattern");
        let captures = member
            .captures(statement)
            .unwrap_or_else(|| panic!("an unknown statement {statement:?}"));
        Some((captures[1].trim().to_owned(), captures[2].to_owned()))
    }

    /// The size, and alignment, of a type `ThreadLocalTop` or
    /// `Isolate::EntryStackItem` declares a member of.
    fn member_type_size(&self, ty: &str) -> i64 {
        let globals = self.file("src/common/globals.h");
        if ty.ends_with('*') {
            return self.size("kSystemPointerSize");
        }
        if ty.starts_with("Tagged<") && ty.ends_with('>') {
            return self.size("kTaggedSize");
        }
        match ty {
            "Address" => {
                assert!(
                    self.contains("include/v8-internal.h", "typedef uintptr_t Address;"),
                    "Address is no longer a uintptr_t"
                );
                type_size(globals, "uintptr_t")
            }
            "uintptr_t" | "uint8_t" | "int" => type_size(globals, ty),
            "StateTag" => {
                let unwinder = self.file("include/v8-unwinder.h");
                assert!(
                    unwinder.contains("enum StateTag : uint16_t {"),
                    "StateTag is no longer a uint16_t"
                );
                2
            }
            "std::atomic<ThreadId>" => {
                // A lock-free atomic of a type has its size.
                let thread_id = class_body(self.file("src/execution/thread-id.h"), "ThreadId")
                    .expect("thread-id.h declares ThreadId");
                let members = data_members(thread_id);
                assert_eq!(members, ["int id_;"], "ThreadId's members");
                4
            }
            "v8::FailedAccessCheckCallback" => {
                assert!(
                    self.contains(
                        "include/v8-callbacks.h",
                        "using FailedAccessCheckCallback = void (*)("
                    ),
                    "FailedAccessCheckCallback is no longer a function pointer"
                );
                self.size("kSystemPointerSize")
            }
            _ => panic!("no size of the member type {ty:?}"),
        }
    }

    /// x64's `EntryFrameConstants`, outside Windows: where `JSEntry` saves
    /// the exit frame pointer it clears, and from 13.6 the fast C call's
    /// frame pointer and return address. Every other conditional fails.
    fn entry_frame_constants(&self) -> Vec<(String, i64)> {
        let file = "src/execution/x64/frame-constants-x64.h";
        let body = without_windows(self.class_text(file, "EntryFrameConstants"));
        assert!(
            !body.contains('#'),
            "{file}'s EntryFrameConstants has a conditional"
        );
        assert!(
            self.contains(file, "class EntryFrameConstants : public AllStatic {"),
            "{file}'s EntryFrameConstants derives from a class"
        );
        let constant = |name: &str| -> Option<i64> {
            let pattern = format!(r"static\s+constexpr\s+int\s+{name}\s*=\s*([^;]+);");
            let pattern = Regex::new(&pattern).expect("a pattern");
            let definitions: Vec<_> = pattern.captures_iter(&body).collect();
            assert!(definitions.len() <= 1, "{file} defines {name} twice");
            let expression = definitions.first()?[1].to_owned();
            Some(self.eval_in(file, &body, &expression))
        };
        let mut constants = Vec::new();
        for (name, required) in [
            ("kNextExitFrameFPOffset", true),
            ("kNextFastCallFrameFPOffset", false),
            ("kNextFastCallFramePCOffset", false),
        ] {
            match constant(name) {
                Some(value) => constants.push((format!("EntryFrameConstants::{name}"), value)),
                None => assert!(!required, "{file} defines no EntryFrameConstants::{name}"),
            }
        }
        constants
    }

    /// An expression of a class whose `body` defines the constants it
    /// names, or the headers do.
    fn eval_in(&self, file: &str, body: &str, expression: &str) -> i64 {
        let lookup = |name: &str| {
            let pattern = format!(r"static\s+constexpr\s+int\s+{name}\s*=\s*([^;]+);");
            Regex::new(&pattern).ok()?.captures(body).map_or_else(
                || self.lookup(file, &[], name),
                |own| Some(self.eval_in(file, body, &own[1])),
            )
        };
        eval(expression, &lookup)
            .unwrap_or_else(|| panic!("{file}: cannot evaluate {expression:?}"))
    }
}

/// How a class names its data members: V8's way, with a trailing
/// underscore, or plainly, as `Isolate::EntryStackItem` does.
#[derive(Debug, Clone, Copy)]
enum Names {
    V8,
    Plain,
}

/// A class body's statements outside its functions' bodies, each with
/// whether a preprocessor conditional encloses it. A dropped body ends
/// its statement, so that what follows is a statement of its own.
fn class_statements(body: &str) -> Vec<(String, bool)> {
    let mut statements = Vec::new();
    let mut current = String::new();
    let (mut depth, mut conditionals) = (0, 0_usize);
    let mut conditional = false;
    for line in body.lines() {
        let directive = line.trim_start();
        if depth == 0 && directive.starts_with('#') {
            if directive.starts_with("#if") {
                conditionals += 1;
            } else if directive.starts_with("#endif") {
                conditionals = conditionals.checked_sub(1).expect("a balanced #endif");
            }
            continue;
        }
        for c in line.chars().chain(std::iter::once('\n')) {
            match c {
                '{' => depth += 1,
                '}' => {
                    depth -= 1;
                    if depth == 0 {
                        statements.push((std::mem::take(&mut current), conditional));
                        conditional = false;
                    }
                }
                ';' if depth == 0 => {
                    statements.push((
                        std::mem::take(&mut current),
                        conditional || conditionals > 0,
                    ));
                    conditional = false;
                }
                c if depth == 0 => {
                    if !c.is_whitespace() && conditionals > 0 {
                        conditional = true;
                    }
                    current.push(c);
                }
                _ => {}
            }
        }
    }
    assert!(
        current.trim().is_empty(),
        "a class body ends in {current:?}"
    );
    statements
}

/// A class body with each `#ifdef V8_TARGET_OS_WIN` replaced by its other
/// branch.
fn without_windows(body: &str) -> String {
    let mut kept = String::new();
    let mut windows: Option<bool> = None;
    for line in body.lines() {
        match (line.trim(), windows) {
            ("#ifdef V8_TARGET_OS_WIN", None) => windows = Some(true),
            ("#else", Some(true)) => windows = Some(false),
            (endif, Some(_)) if endif.starts_with("#endif") => windows = None,
            (_, Some(true)) => {}
            _ => {
                kept.push_str(line);
                kept.push('\n');
            }
        }
    }
    assert!(windows.is_none(), "an unclosed #ifdef V8_TARGET_OS_WIN");
    kept
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
        "int64_t" | "uint64_t" | "void*" | "intptr_t" | "uintptr_t" | "size_t" => 8,
        "uint8_t" | "int8_t" => 1,
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

/// The classes of frames `Headers::frame_constants` reads, each with the
/// class it derives from, which the header must declare.
const FRAME_CLASSES: [(&str, &str); 4] = [
    ("InterpreterFrameConstants", "UnoptimizedFrameConstants"),
    ("UnoptimizedFrameConstants", "StandardFrameConstants"),
    ("StandardFrameConstants", "CommonFrameConstants"),
    ("CommonFrameConstants", "AllStatic"),
];

/// `text` with each call of a function-like macro of `macros` expanded, and
/// the result expanded again. These macros neither stringize nor paste. A
/// macro with more than one definition, or a call with the wrong number of
/// arguments, fails.
fn expand_macros(text: &str, macros: &BTreeMap<String, Vec<(Vec<String>, String)>>) -> String {
    let call = Regex::new(r"\b(\w+)\s*\(").expect("a pattern");
    let identifier = Regex::new(r"\w+").expect("a pattern");
    let mut text = text.to_owned();
    let mut at = 0;
    for _ in 0..10_000 {
        let Some(found) = call.captures_at(&text, at) else {
            return text;
        };
        let whole = found.get(0).expect("the match").range();
        let name = found[1].to_owned();
        let Some(definitions) = macros.get(&name) else {
            at = whole.end;
            continue;
        };
        let [(parameters, body)] = definitions.as_slice() else {
            panic!("{name} has {} definitions", definitions.len());
        };
        assert!(!body.contains('#'), "{name} stringizes or pastes: {body:?}");

        let mut depth = 1;
        let mut arguments = vec![String::new()];
        let mut end = None;
        for (index, c) in text[whole.end..].char_indices() {
            match c {
                '(' => depth += 1,
                ')' => depth -= 1,
                ',' if depth == 1 => {
                    arguments.push(String::new());
                    continue;
                }
                _ => {}
            }
            if depth == 0 {
                end = Some(whole.end + index + 1);
                break;
            }
            arguments.last_mut().expect("an argument").push(c);
        }
        let end = end.unwrap_or_else(|| panic!("an unbalanced call of {name}"));
        let arguments: Vec<&str> = arguments.iter().map(|argument| argument.trim()).collect();
        assert_eq!(
            arguments.len(),
            parameters.len(),
            "{name} is called with {arguments:?}"
        );
        let expanded = identifier.replace_all(body, |token: &regex::Captures<'_>| {
            parameters
                .iter()
                .position(|parameter| *parameter == token[0])
                .map_or_else(|| token[0].to_owned(), |at| arguments[at].to_owned())
        });
        text.replace_range(whole.start..end, expanded.trim());
        at = whole.start;
    }
    panic!("the macros do not finish expanding")
}

/// `text` with each run of whitespace collapsed to one space, for comparing
/// source shapes.
fn normalized(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The data members of a class's body outside its functions' bodies, as
/// V8 names them, `Type name_;` or `Type name_[N];`, each as written with
/// its whitespace normalized.
fn data_members(body: &str) -> Vec<String> {
    let mut depth = 0;
    let outside: String = body
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
    let pattern =
        Regex::new(r"(?m)^\s*([\w:]+(?:<[^;()=]*>)?\s+\w+_(?:\[\d+\])?;)\s*$").expect("a pattern");
    pattern
        .captures_iter(&outside)
        .map(|captures| normalized(&captures[1]))
        .collect()
}

/// The text without its comments.
fn strip_comments(text: &str) -> String {
    let block = Regex::new(r"(?s)/\*.*?\*/").expect("a pattern");
    let line = Regex::new(r"//[^\n]*").expect("a pattern");
    line.replace_all(&block.replace_all(text, ""), "")
        .into_owned()
}

/// The text as a release build compiles it: without the branches of `#ifdef
/// DEBUG` and `#if DEBUG`, nor the other branches of `#ifndef DEBUG`, as
/// `DeoptimizationData::kDeoptEntrySize` has one per build type. Every
/// other conditional is kept, with its directives.
fn release(text: &str) -> String {
    // Each open conditional: whether its current branch is compiled, for one
    // about DEBUG, or `None` for another, which is kept.
    let mut open: Vec<Option<bool>> = Vec::new();
    let compiled = |open: &[Option<bool>]| open.iter().all(|branch| branch.unwrap_or(true));
    let debug = |condition: &str| matches!(condition.trim(), "DEBUG" | "defined(DEBUG)");
    let mut kept = String::with_capacity(text.len());
    for line in text.lines() {
        let directive = line.trim_start();
        let outer = compiled(&open);
        let keep = if let Some(condition) = directive
            .strip_prefix("#ifdef ")
            .or_else(|| directive.strip_prefix("#if "))
        {
            open.push(debug(condition).then_some(false));
            outer && !debug(condition)
        } else if let Some(condition) = directive.strip_prefix("#ifndef ") {
            open.push(debug(condition).then_some(true));
            outer && !debug(condition)
        } else if directive.starts_with("#el") {
            match open.last_mut() {
                Some(Some(branch)) => {
                    assert!(
                        directive.starts_with("#else"),
                        "a DEBUG conditional has {line:?}"
                    );
                    *branch = !*branch;
                    false
                }
                _ => compiled(&open[..open.len().saturating_sub(1)]),
            }
        } else if directive.starts_with("#endif") {
            match open.pop() {
                Some(Some(_)) => false,
                _ => compiled(&open),
            }
        } else {
            outer
        };
        if keep {
            kept.push_str(line);
            kept.push('\n');
        }
    }
    kept
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
/// unary minus, `* + - << == |`, parentheses, and `condition ? a : b`.
pub(super) fn eval(expression: &str, lookup: &dyn Fn(&str) -> Option<i64>) -> Option<i64> {
    let tokens = Regex::new(r"[A-Za-z_][\w:]*|0x[0-9a-fA-F]+[uUlL]*|\d+[uUlL]*|<<|==|[-+*|()?:]")
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
        let mut value = self.equality()?;
        while self.eat("|") {
            value |= self.equality()?;
        }
        Some(value)
    }

    fn equality(&mut self) -> Option<i64> {
        let mut value = self.shift()?;
        while self.eat("==") {
            value = i64::from(value == self.shift()?);
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
    assert_eq!(eval("kTen == 8 ? 0 : kTen", &lookup), Some(10));
    assert_eq!(eval("2 * 4 == 8 ? 0 : 1", &lookup), Some(0));
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

#[test]
fn thread_local_top_fails_on_what_it_does_not_know() {
    let headers = |members: &str| Headers {
        files: BTreeMap::from([
            ("src/common/globals.h", String::new()),
            (
                "include/v8-internal.h",
                "typedef uintptr_t Address;".to_owned(),
            ),
            (
                "src/execution/thread-local-top.h",
                format!(
                    "class ThreadLocalTop {{\n public:\n  static constexpr uint32_t kSizeInBytes = 24;\n  void Clear();\n{members}\n}};\n"
                ),
            ),
        ]),
        config: BTreeMap::new(),
    };
    let known = "  Address c_entry_fp_;\n  uint8_t flag_ = 0;\n  Address js_entry_sp_;";
    let (size, offsets) = headers(known).thread_local_top();
    assert_eq!(size, 24);
    assert_eq!(
        offsets,
        BTreeMap::from([
            ("c_entry_fp_".to_owned(), 0),
            ("flag_".to_owned(), 8),
            ("js_entry_sp_".to_owned(), 16),
        ])
    );

    for (what, members, says) in [
        (
            "an unknown statement",
            "  Address c_entry_fp_;\n  int (*table_)[2];\n  Address js_entry_sp_;",
            "an unknown statement",
        ),
        (
            "a member of a function's pointer type",
            "  Address c_entry_fp_;\n  void (*callback_)(int);\n  Address js_entry_sp_;",
            "an unknown statement",
        ),
        (
            "an unknown member type",
            "  Address c_entry_fp_;\n  Unknown flag_;\n  Address js_entry_sp_;",
            "no size of the member type",
        ),
        (
            "a member under a conditional",
            "  Address c_entry_fp_;\n#if V8_SOMETHING\n  uint8_t flag_;\n#endif\n  Address js_entry_sp_;",
            "flag_ is under a conditional",
        ),
        (
            "a size that is not kSizeInBytes",
            "  Address c_entry_fp_;\n  Address js_entry_sp_;",
            "not kSizeInBytes",
        ),
    ] {
        let headers = headers(members);
        let failure = std::panic::catch_unwind(|| headers.thread_local_top()).expect_err(what);
        let message = failure
            .downcast_ref::<String>()
            .cloned()
            .unwrap_or_default();
        assert!(message.contains(says), "{what}: {message}");
    }
}
