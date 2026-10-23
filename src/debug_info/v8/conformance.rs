//! The generated layouts checked against their two sources: V8's Torque
//! output for each supported version, which `layouts.gen.rs` is generated
//! from here, and each build in the dev shell's Node matrix, whose own
//! `v8dbg_` constants every layout must agree with.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use super::headers::Headers;
use super::{layouts, requires_constant, requires_field};
use crate::ImageAddress;
use crate::image::v8::{Constant, DYNAMIC_SYMBOLS, DispatchTable, Field};

const GENERATED_FILE: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/src/debug_info/v8/layouts.gen.rs"
);

/// The names older versions' Torque gives a field the enum names by its
/// newest.
const RENAMED: [(Field, &str); 4] = [
    (Field::ByteArrayValues, "ByteArray::Bytes"),
    (Field::TrustedByteArrayValues, "TrustedByteArray::Bytes"),
    (Field::ScriptInfos, "Script::SharedFunctionInfos"),
    (Field::FixedDoubleArrayValues, "FixedDoubleArray::Floats"),
];

/// The class each bit-field constant names, and the macro of
/// `bit-fields.h` that defines its bit fields. A context local's
/// `VARIABLE_PROPERTIES` are `ScopeInfo`'s, which `scope-info.h` defines.
const BIT_FIELDS: [(&str, &str); 15] = [
    ("SharedFunctionInfo", "SHARED_FUNCTION_INFO_FLAGS"),
    ("ScopeInfo", "SCOPE_FLAGS"),
    ("Script", "SCRIPT_FLAGS"),
    ("FeedbackVector", "FEEDBACK_VECTOR_FLAGS"),
    ("FeedbackVector", "OSR_STATE"),
    ("Map", "MAP_BIT_FIELDS1"),
    ("Map", "MAP_BIT_FIELDS2"),
    ("Map", "MAP_BIT_FIELDS3"),
    ("Symbol", "SYMBOL_FLAGS"),
    ("JSPromise", "JS_PROMISE_FLAGS"),
    ("JSArrayBuffer", "JS_ARRAY_BUFFER_FLAGS"),
    ("JSArrayBufferView", "JS_ARRAY_BUFFER_VIEW_FLAGS"),
    ("JSRegExp", "JS_REG_EXP_FLAGS"),
    ("CallSiteInfo", "CALL_SITE_INFO_FLAGS"),
    ("ScopeInfo", "VARIABLE_PROPERTIES"),
];

/// V8's license, which the generated layouts carry because they derive
/// from its Torque output.
const V8_NOTICE: &str = "\
Copyright 2014, the V8 project authors. All rights reserved.
Redistribution and use in source and binary forms, with or without
modification, are permitted provided that the following conditions are
met:

    * Redistributions of source code must retain the above copyright
      notice, this list of conditions and the following disclaimer.
    * Redistributions in binary form must reproduce the above
      copyright notice, this list of conditions and the following
      disclaimer in the documentation and/or other materials provided
      with the distribution.
    * Neither the name of Google Inc. nor the names of its
      contributors may be used to endorse or promote products derived
      from this software without specific prior written permission.

THIS SOFTWARE IS PROVIDED BY THE COPYRIGHT HOLDERS AND CONTRIBUTORS
\"AS IS\" AND ANY EXPRESS OR IMPLIED WARRANTIES, INCLUDING, BUT NOT
LIMITED TO, THE IMPLIED WARRANTIES OF MERCHANTABILITY AND FITNESS FOR
A PARTICULAR PURPOSE ARE DISCLAIMED. IN NO EVENT SHALL THE COPYRIGHT
OWNER OR CONTRIBUTORS BE LIABLE FOR ANY DIRECT, INDIRECT, INCIDENTAL,
SPECIAL, EXEMPLARY, OR CONSEQUENTIAL DAMAGES (INCLUDING, BUT NOT
LIMITED TO, PROCUREMENT OF SUBSTITUTE GOODS OR SERVICES; LOSS OF USE,
DATA, OR PROFITS; OR BUSINESS INTERRUPTION) HOWEVER CAUSED AND ON ANY
THEORY OF LIABILITY, WHETHER IN CONTRACT, STRICT LIABILITY, OR TORT
(INCLUDING NEGLIGENCE OR OTHERWISE) ARISING IN ANY WAY OUT OF THE USE
OF THIS SOFTWARE, EVEN IF ADVISED OF THE POSSIBILITY OF SUCH DAMAGE.
";

/// Each V8 version's Torque output the flake provides, by major.minor, in
/// order.
fn torque_versions() -> Vec<((u32, u32), PathBuf)> {
    let root = std::env::var_os("MICROSCOPE_V8_TORQUE")
        .expect("MICROSCOPE_V8_TORQUE names V8's Torque output; run tests in the dev shell");
    let mut versions: Vec<_> = std::fs::read_dir(&root)
        .expect("read MICROSCOPE_V8_TORQUE")
        .map(|entry| {
            let entry = entry.expect("an entry of MICROSCOPE_V8_TORQUE");
            let name = entry.file_name().into_string().expect("a UTF-8 name");
            let (major, minor) = name.split_once('.').expect("a major.minor version");
            let version = (
                major.parse().expect("a major version"),
                minor.parse().expect("a minor version"),
            );
            (version, entry.path().join("torque-generated"))
        })
        .collect();
    versions.sort();
    assert!(
        !versions.is_empty(),
        "MICROSCOPE_V8_TORQUE names no version"
    );
    versions
}

fn read(directory: &Path, file: &str) -> String {
    std::fs::read_to_string(directory.join(file))
        .unwrap_or_else(|error| panic!("read {}: {error}", directory.join(file).display()))
}

/// Every field `class-debug-readers.cc` reads, as `Class::Field`, at its
/// offset: each reader is `uintptr_t TqClass::GetFieldAddress() const`
/// returning `address_ - i::kHeapObjectTag + offset`. Any other shape of
/// reader fails.
fn torque_fields(readers: &str) -> BTreeMap<String, u32> {
    let mut fields = BTreeMap::new();
    let mut lines = readers.lines();
    while let Some(line) = lines.next() {
        let Some(getter) = line
            .strip_prefix("uintptr_t Tq")
            .and_then(|rest| rest.strip_suffix("Address() const {"))
        else {
            continue;
        };

        let (class, field) = getter.split_once("::Get").expect("TqClass::GetField");
        let body = lines.next().unwrap_or_default();
        let offset = body
            .strip_prefix("  return address_ - i::kHeapObjectTag + ")
            .and_then(|rest| rest.strip_suffix(';'))
            .and_then(|offset| offset.parse().ok())
            .unwrap_or_else(|| panic!("{class}::{field}'s reader is {body:?}"));
        let previous = fields.insert(format!("{class}::{field}"), offset);
        assert!(
            previous.is_none_or(|previous| previous == offset),
            "{class}::{field} is read at two offsets"
        );
    }

    assert!(fields.len() > 500, "class-debug-readers.cc reads fields");
    fields
}

/// Whether `class-debug-readers.cc` reads `class`'s `field` as a raw
/// `double`, not as a tagged word: its reader is
/// `Value<double ...> TqClass::GetFieldValue(...)` or
/// `Value<uintptr_t> TqClass::GetFieldValue(...)`. Any other type fails.
fn torque_reads_double(readers: &str, class: &str, field: &str) -> bool {
    let getter = format!("> Tq{class}::Get{field}Value(d::MemoryAccessor accessor) const {{");
    let line = readers
        .lines()
        .find(|line| line.starts_with("Value<") && line.ends_with(&getter))
        .unwrap_or_else(|| panic!("class-debug-readers.cc reads no {class}::{field}"));
    if line.starts_with("Value<double ") || line.starts_with("Value<double>") {
        return true;
    }
    assert!(
        line.starts_with("Value<uintptr_t>"),
        "{class}::{field} is read as {line:?}"
    );
    false
}

/// Every instance type `instance-types.h`'s `TORQUE_ASSIGNED_INSTANCE_TYPES`
/// lists, as `V(NAME, number)`, in its order.
fn torque_instance_types(header: &str) -> Vec<(String, u16)> {
    let list = header
        .split_once("#define TORQUE_ASSIGNED_INSTANCE_TYPES(V) \\\n")
        .expect("instance-types.h assigns instance types")
        .1;
    let mut types = Vec::new();
    for line in list.lines().take_while(|line| !line.trim().is_empty()) {
        let entry = line
            .trim()
            .strip_prefix("V(")
            .and_then(|rest| rest.split_once(')'))
            .and_then(|(entry, _)| entry.split_once(", "))
            .unwrap_or_else(|| panic!("an instance type is {line:?}"));
        let value = entry.1.parse().expect("an instance type's number");
        assert!(
            types.iter().all(|(name, _)| name != entry.0),
            "{} is assigned twice",
            entry.0
        );
        types.push((entry.0.to_owned(), value));
    }

    assert!(types.len() > 300, "instance-types.h assigns instance types");
    types
}

/// Each bit field of the class [`BIT_FIELDS`] names, as `Class::NameBits`,
/// with where it begins and its width: `bit-fields.h` defines each as
/// `using Name = base::BitField<Type, shift, size, Storage>;`. Where every
/// field is one bit, an `enum Flag` follows that must name each field's
/// bit as `kName`.
fn torque_bit_fields(header: &str) -> BTreeMap<String, (u32, u32)> {
    let mut fields = BTreeMap::new();
    for (class, flags) in BIT_FIELDS {
        let start = format!("#define DEFINE_TORQUE_GENERATED_{flags}() \\\n");
        let list = header
            .split_once(&start)
            .unwrap_or_else(|| panic!("bit-fields.h defines {flags}"))
            .1;
        let mut lines = list.lines().take_while(|line| !line.trim().is_empty());
        let mut own = Vec::new();
        for line in lines.by_ref() {
            if let Some(storage) = line
                .trim()
                .strip_prefix("enum Flag: ")
                .and_then(|rest| rest.strip_suffix(" { \\"))
            {
                check_flag_enum(flags, &own, storage, lines.by_ref());
                break;
            }
            let (name, shift, size) = line
                .trim()
                .strip_prefix("using ")
                .and_then(|rest| rest.split_once(" = base::BitField<"))
                .and_then(|(name, rest)| {
                    let arguments = rest.strip_suffix(">; \\")?;
                    let mut parts = arguments.rsplitn(4, ", ");
                    let _storage = parts.next()?;
                    let size = parts.next()?.parse().ok()?;
                    let shift = parts.next()?.parse().ok()?;
                    Some((name, shift, size))
                })
                .unwrap_or_else(|| panic!("a bit field of {flags} is {line:?}"));
            fields.insert(format!("{class}::{name}"), (shift, size));
            own.push((name, shift, size));
        }
    }

    fields
}

/// Checks the `enum Flag` of `storage` that follows the one-bit fields
/// `fields` of `flags`: `kNone = 0`, then each field `NameBit` as `kName =
/// storage{1} << shift`, in order, then `Flags` and `kFlagCount`, the
/// number of fields.
fn check_flag_enum<'a>(
    flags: &str,
    fields: &[(&str, u32, u32)],
    storage: &str,
    lines: impl Iterator<Item = &'a str>,
) {
    let lines: Vec<&str> = lines
        .map(|line| line.trim().trim_end_matches('\\').trim_end())
        .collect();
    let mut expected = vec!["kNone = 0,".to_owned()];
    for &(name, shift, size) in fields {
        assert_eq!(
            size, 1,
            "{flags} has a Flag enum but its {name} is {size} bits"
        );
        let flag = name.strip_suffix("Bit").unwrap_or(name);
        expected.push(format!("k{flag} = {storage}{{1}} << {shift},"));
    }
    expected.extend([
        "};".to_owned(),
        "using Flags = base::Flags<Flag>;".to_owned(),
        format!("static constexpr int kFlagCount = {};", fields.len()),
    ]);
    assert_eq!(lines, expected, "{flags}'s Flag enum");
}

/// The value a Torque constant has in the version whose Torque output
/// gave `types` and `bits`: an instance type's number, or a bit field's
/// `kShift` or `kMask`.
fn torque_constant(
    constant: Constant,
    types: &[(String, u16)],
    bits: &BTreeMap<String, (u32, u32)>,
) -> Option<i64> {
    let name = constant.name();
    if let Some(field) = name.strip_suffix("::kShift") {
        return bits.get(field).map(|&(shift, _)| i64::from(shift));
    }

    if let Some(field) = name.strip_suffix("::kMask") {
        return bits
            .get(field)
            .map(|&(shift, size)| ((1i64 << size) - 1) << shift);
    }

    types
        .iter()
        .find(|(own, _)| own == name)
        .map(|&(_, value)| i64::from(value))
}

/// The constants V8's headers give, by the name [`Constant`] gives each,
/// for a version with or without leaptiering.
fn header_constants(headers: &Headers, leaptiering: bool) -> BTreeMap<String, i64> {
    let mut constants: BTreeMap<String, i64> = headers
        .code_kinds()
        .into_iter()
        .map(|(kind, value)| (format!("CodeKind::{kind}"), value))
        .collect();
    // Optimized code's kind is TURBOFAN through 12.4, then TURBOFAN_JS.
    if let Some(&turbofan) = constants.get("CodeKind::TURBOFAN") {
        let previous = constants.insert("CodeKind::TURBOFAN_JS".to_owned(), turbofan);
        assert!(previous.is_none(), "both TURBOFAN and TURBOFAN_JS");
    }

    let deopt = "src/objects/deoptimization-data.h";
    for index in DEOPTIMIZATION_INDICES {
        let name = format!("k{index}Index");
        if let Some(value) = headers.class_constant(deopt, &["DeoptimizationData"], &name) {
            constants.insert(format!("DeoptimizationData::{name}"), value);
        }
    }

    let (size, offsets) = headers.inlining_position();
    constants.insert("sizeof(InliningPosition)".to_owned(), size);
    for (member, offset) in offsets {
        constants.insert(format!("offsetof(InliningPosition, {member})"), offset);
    }

    let positions = "src/codegen/source-position.h";
    for (field, (shift, size)) in headers.bit_fields(positions, "SourcePosition") {
        constants.insert(format!("SourcePosition::{field}::kShift"), shift);
        constants.insert(
            format!("SourcePosition::{field}::kMask"),
            ((1i64 << size) - 1) << shift,
        );
    }
    // The field holds the inlining id plus one, so that 0 is kNotInlined.
    assert!(
        headers.contains(positions, "return InliningIdField::decode(value_) - 1;"),
        "SourcePosition's inlining id is no longer the field less one"
    );
    let not_inlined = headers.class_constant(positions, &["SourcePosition"], "kNotInlined");
    constants.insert(
        "SourcePosition::kNotInlined".to_owned(),
        not_inlined.expect("kNotInlined"),
    );

    for (class, chain, file, names) in HEADER_CLASS_CONSTANTS {
        for name in names {
            let value = headers
                .class_constant(file, chain, name)
                .unwrap_or_else(|| panic!("{file} gives {class} no {name}"));
            constants.insert(format!("{class}::{name}"), value);
        }
    }

    for (file, name) in [
        ("include/v8-internal.h", "kWeakHeapObjectTag"),
        ("src/common/globals.h", "kWeakHeapObjectMask"),
        ("src/common/globals.h", "kClearedWeakHeapObjectLower32"),
    ] {
        constants.insert(format!("internal::{name}"), headers.constant(file, name));
    }

    if leaptiering {
        add_dispatch_constants(headers, &mut constants);
    }
    super::deoptimization::add_constants(headers, &mut constants);
    add_value_constants(headers, &mut constants);
    constants
}

/// The constants of JavaScript's values the headers give: their kinds and
/// states, the bits of a `BigInt`'s and a property's details, and the
/// layouts of ordered hash tables and contexts.
fn add_value_constants(headers: &Headers, constants: &mut BTreeMap<String, i64>) {
    let mut add = |name: String, value: i64| {
        let previous = constants.insert(name.clone(), value);
        assert!(previous.is_none(), "{name} is given twice");
    };

    let enums: [(&str, &str, &str, bool); 7] = [
        (
            "ElementsKind",
            "src/objects/elements-kind.h",
            "enum ElementsKind",
            false,
        ),
        (
            "FunctionKind",
            "src/objects/function-kind.h",
            "enum class FunctionKind",
            true,
        ),
        ("Promise", "include/v8-promise.h", "enum PromiseState", true),
        (
            "VariableMode",
            "src/common/globals.h",
            "enum class VariableMode",
            true,
        ),
        (
            "InitializationFlag",
            "src/common/globals.h",
            "enum InitializationFlag",
            true,
        ),
        // From 14.6.
        (
            "PrivateSymbolKind",
            "src/objects/name.h",
            "enum class PrivateSymbolKind",
            true,
        ),
        // From 14.6, the slots of a raw frame of an error's stack data.
        (
            "CallSiteInfo",
            "src/objects/call-site-info.h",
            "enum Fields",
            true,
        ),
    ];
    for (class, file, head, plain) in enums {
        if !headers.contains(file, head) {
            continue;
        }
        let values = if plain {
            headers.enumerators(file, head)
        } else {
            headers.elements_kinds()
        };
        for (name, value) in values {
            add(format!("{class}::{name}"), value);
        }
    }

    for class in ["OrderedHashMap", "OrderedHashSet"] {
        for (name, value) in headers.ordered_hash_table(class) {
            add(name, value);
        }
    }
    for (name, value) in headers.context_slots() {
        add(format!("Context::{name}"), value);
    }
    add(
        "internal::kHoleNanInt64".to_owned(),
        headers.hole_nan_int64(),
    );
    add_frame_constants(headers, &mut add);

    assert!(
        headers.global_dictionary_cell_is_key(),
        "a global dictionary's entry is no longer the property cell at its key"
    );
    for (file, class, fields) in [
        (
            "src/objects/bigint.h",
            "BigIntBase",
            &["SignBits", "LengthBits"][..],
        ),
        (
            "src/objects/property-details.h",
            "PropertyDetails",
            &["DictionaryStorageField"][..],
        ),
    ] {
        let bits = headers.bit_fields(file, class);
        for field in fields {
            let (shift, size) = bits
                .get(*field)
                .copied()
                .unwrap_or_else(|| panic!("{class} has no {field}"));
            add(format!("{class}::{field}::kShift"), shift);
            add(
                format!("{class}::{field}::kMask"),
                ((1i64 << size) - 1) << shift,
            );
        }
    }
}

/// Where JavaScript frames keep their slots, from `frame-constants.h`,
/// and the receiver's place in their argument count, from `globals.h`.
fn add_frame_constants(headers: &Headers, add: &mut impl FnMut(String, i64)) {
    for (name, value) in headers.frame_constants(&[
        ("StandardFrameConstants", "kArgCOffset"),
        ("UnoptimizedFrameConstants", "kRegisterFileFromFp"),
        ("UnoptimizedFrameConstants", "kFirstParamFromFp"),
        ("UnoptimizedFrameConstants", "kFeedbackVectorFromFp"),
        ("UnoptimizedFrameConstants", "kBytecodeArrayFromFp"),
        ("InterpreterFrameConstants", "kBytecodeOffsetFromFp"),
    ]) {
        add(name, value);
    }
    let globals = "src/common/globals.h";
    assert_eq!(
        headers
            .file(globals)
            .matches("kJSArgcReceiverSlots =")
            .count(),
        1,
        "globals.h defines kJSArgcReceiverSlots once"
    );
    add(
        "internal::kJSArgcReceiverSlots".to_owned(),
        headers.constant(globals, "kJSArgcReceiverSlots"),
    );
}

/// The JS dispatch table's constants, from the 64-bit branches of
/// `globals.h` and `js-dispatch-table.h`.
fn add_dispatch_constants(headers: &Headers, constants: &mut BTreeMap<String, i64>) {
    let globals = "src/common/globals.h";
    let table = "src/sandbox/js-dispatch-table.h";
    for name in ["kJSDispatchTableEntrySize", "kJSDispatchHandleShift"] {
        if name == "kJSDispatchHandleShift" {
            assert!(headers.first_is_64_bit(globals, name), "{name} is 64-bit's");
        }
        constants.insert(format!("internal::{name}"), headers.constant(globals, name));
    }
    for name in [
        "kEntrypointOffset",
        "kCodeObjectOffset",
        "kObjectPointerShift",
        "kParameterCountMask",
    ] {
        if matches!(name, "kObjectPointerShift" | "kParameterCountMask") {
            assert!(headers.first_is_64_bit(table, name), "{name} is 64-bit's");
        }
        let value = headers
            .class_constant(table, &["JSDispatchEntry"], name)
            .unwrap_or_else(|| panic!("JSDispatchEntry has no {name}"));
        constants.insert(format!("JSDispatchEntry::{name}"), value);
    }
}

/// The slots of `DeoptimizationData` the headers may give, as
/// `k{Name}Index`; the binary exports some as `DeoptimizationData{Name}Index`.
const DEOPTIMIZATION_INDICES: [&str; 12] = [
    "FrameTranslation",
    "InlinedFunctionCount",
    "ProtectedLiteralArray",
    "LiteralArray",
    "OsrBytecodeOffset",
    "OsrPcOffset",
    "OptimizationId",
    "SharedFunctionInfo",
    "WrappedSharedFunctionInfo",
    "InliningPositions",
    "DeoptExitStart",
    "FirstDeoptEntry",
];

/// Class constants the generated layouts take from the headers: the name
/// [`Constant`] gives the class, the classes searched for each, in order
/// from the class to those it derives from, its header, and the names.
const HEADER_CLASS_CONSTANTS: [(&str, &[&str], &str, &[&str]); 9] = [
    (
        "Oddball",
        &["Oddball"],
        "src/objects/oddball.h",
        &["kUndefined", "kNull", "kTrue", "kFalse"],
    ),
    (
        "NumberDictionary",
        &["NumberDictionary"],
        "src/objects/dictionary.h",
        &["kEntryValueIndex", "kEntryDetailsIndex"],
    ),
    (
        "GlobalDictionaryShape",
        &["GlobalDictionaryShape"],
        "src/objects/dictionary.h",
        &["kPrefixSize"],
    ),
    (
        "HashTableBase",
        &["HashTableBase"],
        "src/objects/hash-table.h",
        &[
            "kNumberOfElementsIndex",
            "kNumberOfDeletedElementsIndex",
            "kCapacityIndex",
            "kPrefixStartIndex",
        ],
    ),
    (
        "HashTable",
        &["HashTable"],
        "src/objects/hash-table.h",
        &["kEntryKeyIndex"],
    ),
    (
        "BaseNameDictionary",
        &["BaseNameDictionary"],
        "src/objects/dictionary.h",
        &["kNextEnumerationIndexIndex", "kObjectHashIndex"],
    ),
    (
        "NameDictionary",
        &["NameDictionary", "BaseNameDictionary"],
        "src/objects/dictionary.h",
        &["kFlagsIndex", "kEntryValueIndex", "kEntryDetailsIndex"],
    ),
    (
        "PropertyArray",
        &["PropertyArray"],
        "src/objects/property-array.h",
        &["kLengthFieldSize"],
    ),
    (
        "JSFunction",
        &["JSFunction", "JSFunctionOrBoundFunctionOrWrappedFunction"],
        "src/objects/js-function.h",
        &["kNameDescriptorIndex", "kMinDescriptorsForFastBindAndWrap"],
    ),
];

/// The value V8's headers and Torque output give each `v8dbg_` constant
/// they name too, by the constant's name without `v8dbg_`.
fn checked_constants(
    headers: &Headers,
    constants: &BTreeMap<String, i64>,
    bits: &BTreeMap<String, (u32, u32)>,
) -> Vec<(String, i64)> {
    let mut checked = Vec::new();
    let mut check = |name: &str, value: i64| checked.push((name.to_owned(), value));
    for (name, kind) in [
        ("CodeKindInterpretedFunction", "INTERPRETED_FUNCTION"),
        ("CodeKindBaseline", "BASELINE"),
        ("CodeKindBytecodeHandler", "BYTECODE_HANDLER"),
    ] {
        check(name, constants[&format!("CodeKind::{kind}")]);
    }
    let (shift, size) = headers.code_kind_field();
    check("CodeKindFieldShift", shift);
    check("CodeKindFieldMask", ((1 << size) - 1) << shift);

    for index in DEOPTIMIZATION_INDICES {
        if let Some(&value) = constants.get(&format!("DeoptimizationData::k{index}Index")) {
            check(&format!("DeoptimizationData{index}Index"), value);
        }
    }

    let descriptors = "src/objects/descriptor-array.h";
    for (name, own) in [
        ("prop_desc_key", "kEntryKeyIndex"),
        ("prop_desc_details", "kEntryDetailsIndex"),
        ("prop_desc_value", "kEntryValueIndex"),
        ("prop_desc_size", "kEntrySize"),
    ] {
        let value = headers.class_constant(descriptors, &["DescriptorArray"], own);
        check(
            name,
            value.unwrap_or_else(|| panic!("DescriptorArray has no {own}")),
        );
    }

    add_property_details(headers, &mut check);

    let map = |name: &str| {
        bits.get(name)
            .map(|&(shift, size)| (i64::from(shift), i64::from(size)))
    };
    let (dictionary_map, _) = map("Map::IsDictionaryMapBit").expect("Map::IsDictionaryMapBit");
    check("bit_field3_is_dictionary_map_shift", dictionary_map);
    let (shift, size) = map("Map::NumberOfOwnDescriptorsBits").expect("NumberOfOwnDescriptorsBits");
    check("bit_field3_number_of_own_descriptors_shift", shift);
    check(
        "bit_field3_number_of_own_descriptors_mask",
        ((1 << size) - 1) << shift,
    );

    add_value_checks(constants, bits, &mut check);

    check(
        "namedictionary_prefix_start_index",
        constants["HashTableBase::kPrefixStartIndex"],
    );
    let dictionaries = "src/objects/dictionary.h";
    for (name, class, own) in [
        (
            "namedictionaryshape_entry_size",
            "NameDictionaryShape",
            "kEntrySize",
        ),
        (
            "namedictionaryshape_prefix_size",
            "NameDictionaryShape",
            "kPrefixSize",
        ),
        (
            "globaldictionaryshape_entry_size",
            "GlobalDictionaryShape",
            "kEntrySize",
        ),
        (
            "numberdictionaryshape_entry_size",
            "NumberDictionaryShape",
            "kEntrySize",
        ),
        (
            "numberdictionaryshape_prefix_size",
            "NumberDictionaryShape",
            "kPrefixSize",
        ),
    ] {
        let value = headers.class_constant(dictionaries, &[class], own);
        check(
            name,
            value.unwrap_or_else(|| panic!("{class} has no {own}")),
        );
    }

    check(
        "HeapObjectTag",
        headers.constant("include/v8-internal.h", "kHeapObjectTag"),
    );
    checked
}

/// The `v8dbg_` constants of JavaScript's values the headers and Torque
/// output give too: the bits of a map's elements kind and of a detached
/// array buffer, the oddballs' kinds, three elements kinds, a context's
/// first slots, and where an unoptimized frame keeps its arguments, its
/// bytecode array, and its bytecode offset.
fn add_value_checks(
    constants: &BTreeMap<String, i64>,
    bits: &BTreeMap<String, (u32, u32)>,
    check: &mut impl FnMut(&str, i64),
) {
    for (prefix, name) in [
        ("bit_field2_elements_kind", "Map::ElementsKindBits"),
        (
            "jsarray_buffer_was_detached",
            "JSArrayBuffer::WasDetachedBit",
        ),
    ] {
        let (shift, size) = bits.get(name).unwrap_or_else(|| panic!("no {name}"));
        let (shift, size) = (i64::from(*shift), i64::from(*size));
        check(&format!("{prefix}_shift"), shift);
        check(&format!("{prefix}_mask"), ((1 << size) - 1) << shift);
    }
    for (name, own) in [
        ("OddballUndefined", "Oddball::kUndefined"),
        ("OddballNull", "Oddball::kNull"),
        ("OddballTrue", "Oddball::kTrue"),
        ("OddballFalse", "Oddball::kFalse"),
        ("elements_fast_elements", "ElementsKind::PACKED_ELEMENTS"),
        (
            "elements_fast_holey_elements",
            "ElementsKind::HOLEY_ELEMENTS",
        ),
        (
            "elements_dictionary_elements",
            "ElementsKind::DICTIONARY_ELEMENTS",
        ),
        ("context_idx_scope_info", "Context::SCOPE_INFO_INDEX"),
        ("context_idx_prev", "Context::PREVIOUS_INDEX"),
        ("context_min_slots", "Context::MIN_CONTEXT_SLOTS"),
        (
            "off_fp_args",
            "UnoptimizedFrameConstants::kFirstParamFromFp",
        ),
        (
            "off_fp_bytecode_array",
            "UnoptimizedFrameConstants::kBytecodeArrayFromFp",
        ),
        (
            "off_fp_bytecode_offset",
            "InterpreterFrameConstants::kBytecodeOffsetFromFp",
        ),
    ] {
        check(name, constants[own]);
    }
}

/// The `prop_` constants `property-details.h` gives: the kinds, locations,
/// and representations, and where each field of a property's details is.
fn add_property_details(headers: &Headers, check: &mut impl FnMut(&str, i64)) {
    let file = "src/objects/property-details.h";
    let kinds = headers.enumerators(file, "enum class PropertyKind");
    check("prop_kind_Data", kinds["kData"]);
    check("prop_kind_Accessor", kinds["kAccessor"]);
    let locations = headers.enumerators(file, "enum class PropertyLocation");
    check("prop_location_Field", locations["kField"]);
    check("prop_location_Descriptor", locations["kDescriptor"]);
    let representations = headers.enumerators(file, "enum Kind");
    for (name, own) in [
        ("smi", "kSmi"),
        ("double", "kDouble"),
        ("heapobject", "kHeapObject"),
        ("tagged", "kTagged"),
    ] {
        check(&format!("prop_representation_{name}"), representations[own]);
    }

    let fields = headers.bit_fields(file, "PropertyDetails");
    let field = |name: &str| {
        fields
            .get(name)
            .copied()
            .unwrap_or_else(|| panic!("no {name}"))
    };
    let (_, kind_size) = field("KindField");
    check("prop_kind_mask", (1 << kind_size) - 1);
    for (prefix, name) in [
        ("prop_location", "LocationField"),
        ("prop_representation", "RepresentationField"),
        ("prop_attributes", "AttributesField"),
        ("prop_index", "FieldIndexField"),
    ] {
        let (shift, size) = field(name);
        check(&format!("{prefix}_shift"), shift);
        check(&format!("{prefix}_mask"), ((1 << size) - 1) << shift);
    }
}

/// One version's entry of `layouts.gen.rs`, and where its accessor pair
/// keeps its getter and setter.
fn generate_version(text: &mut String, (major, minor): (u32, u32), torque: &Path) -> [u32; 2] {
    let readers = read(torque, "class-debug-readers.cc");
    let mut fields = torque_fields(&readers);
    let types = torque_instance_types(&read(torque, "instance-types.h"));
    let bits = torque_bit_fields(&read(torque, "bit-fields.h"));
    let leaptiering = fields.contains_key("JSFunction::DispatchHandle");
    let v8 = torque.parent().expect("a version's directory").join("v8");
    let headers = Headers::read(&v8, leaptiering);
    for (name, offset) in headers
        .code_fields()
        .into_iter()
        .chain(headers.code_wrapper_fields())
        .chain(headers.bigint_fields())
        .chain(headers.accessor_pair_fields().into_iter().flatten())
    {
        let offset = u32::try_from(offset).expect("an offset");
        let previous = fields.insert(name.clone(), offset);
        assert!(
            previous.is_none_or(|previous| previous == offset),
            "{name} is at two offsets"
        );
    }
    let length_and_hash = fields["PropertyArray::LengthAndHash"];
    let objects = headers.property_array_objects(i64::from(length_and_hash));
    fields.insert(
        "PropertyArray::Objects".to_owned(),
        u32::try_from(objects).expect("an offset"),
    );
    let mut constants = header_constants(&headers, leaptiering);
    constants.insert(
        "JSDate::value is float64".to_owned(),
        i64::from(torque_reads_double(&readers, "JSDate", "Value")),
    );
    let checked = checked_constants(&headers, &constants, &bits);

    writeln!(text, "    Generated {{").expect("write");
    writeln!(text, "        major: {major},\n        minor: {minor},").expect("write");
    writeln!(text, "        fields: &[").expect("write");
    for &field in Field::ALL {
        let names = std::iter::once(field.name()).chain(
            RENAMED
                .iter()
                .filter(|(own, _)| *own == field)
                .map(|(_, name)| *name),
        );
        let found: Vec<u32> = names.filter_map(|name| fields.get(name).copied()).collect();
        match found.as_slice() {
            [] => assert!(
                !requires_field(field, (major, minor)),
                "V8 {major}.{minor}'s Torque output has no {}",
                field.name()
            ),
            [offset] => writeln!(text, "            (Field::{field:?}, {offset}),").expect("write"),
            _ => panic!("V8 {major}.{minor} names {} twice", field.name()),
        }
    }

    writeln!(text, "        ],\n        constants: &[").expect("write");
    for &constant in Constant::ALL.iter().filter(|constant| constant.is_torque()) {
        let value = constants
            .get(constant.name())
            .copied()
            .or_else(|| torque_constant(constant, &types, &bits));
        match value {
            Some(value) => {
                writeln!(text, "            (Constant::{constant:?}, {value}),").expect("write");
            }
            None => assert!(
                !requires_constant(constant, (major, minor)),
                "V8 {major}.{minor}'s Torque output has no {}",
                constant.name()
            ),
        }
    }

    writeln!(text, "        ],\n        instance_types: &[").expect("write");
    for (name, value) in types {
        writeln!(text, "            ({name:?}, {value}),").expect("write");
    }

    writeln!(text, "        ],\n        checked: &[").expect("write");
    for (name, value) in checked {
        writeln!(text, "            ({name:?}, {value}),").expect("write");
    }

    writeln!(text, "        ],\n    }},").expect("write");
    [Field::AccessorPairGetter, Field::AccessorPairSetter].map(|field| fields[field.name()])
}

fn generate() -> String {
    let mut text = String::from(
        "// @generated from V8's Torque output for each supported version, the\n\
         // class-debug-readers.cc, instance-types.h, and bit-fields.h that\n\
         // MICROSCOPE_V8_TORQUE holds, and from the V8 headers beside them\n\
         // (code.h, deoptimization-data.h, source-position.h, hash-table.h,\n\
         // dictionary.h, dictionary-inl.h, descriptor-array.h,\n\
         // property-details.h, globals.h, v8-internal.h, js-dispatch-table.h,\n\
         // heap-object.h, primitive-heap-object.h, bigint.h, struct.h,\n\
         // oddball.h, name.h, elements-kind.h, function-kind.h,\n\
         // ordered-hash-table.h, contexts.h, contexts-inl.h, map-inl.h,\n\
         // call-site-info.h, v8-promise.h, frame-constants.h), by\n\
         // `debug_info::v8::conformance::layouts_match_torque`. Do not edit;\n\
         // rerun that test with MICROSCOPE_UPDATE_V8=1.\n\
         //\n\
         // The values derive from V8, which carries this notice:\n\
         //\n",
    );
    for line in V8_NOTICE.lines() {
        if line.is_empty() {
            text.push_str("//\n");
        } else {
            writeln!(text, "// {line}").expect("write");
        }
    }

    text.push_str(
        "\n#![allow(\n    \
         clippy::unreadable_literal,\n    \
         reason = \"generated numbers, written as Torque gives them\"\n)]\n\n\
         use super::Generated;\n\
         use crate::image::v8::{Constant, Field};\n\n\
         pub(super) const GENERATED: &[Generated] = &[\n",
    );
    // Where Torque reads an accessor pair's fields and where struct.h lays
    // them out, from 14.6, they agree.
    let mut accessor_pairs = Vec::new();
    for (version, torque) in torque_versions() {
        accessor_pairs.push((version, generate_version(&mut text, version, &torque)));
    }
    assert!(
        accessor_pairs
            .iter()
            .all(|(_, fields)| *fields == accessor_pairs[0].1),
        "the versions' accessor pairs disagree: {accessor_pairs:?}"
    );

    text.push_str("];\n");
    text
}

/// Fails when `layouts.gen.rs` no longer matches the Torque output in
/// `MICROSCOPE_V8_TORQUE`, or when that output lacks a field or constant a
/// version must have. Set `MICROSCOPE_UPDATE_V8=1` to rewrite it.
#[test]
fn layouts_match_torque() {
    let expected = generate();
    if std::env::var_os("MICROSCOPE_UPDATE_V8").is_some() {
        std::fs::write(GENERATED_FILE, &expected).expect("write the generated layouts");
        return;
    }

    let actual = std::fs::read_to_string(GENERATED_FILE).unwrap_or_default();
    assert!(
        actual == expected,
        "src/debug_info/v8/layouts.gen.rs is stale; rerun with MICROSCOPE_UPDATE_V8=1"
    );
}

const SLICES_FILE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/src/image/v8/slices.gen.rs");

/// Fails when `src/image/v8/slices.gen.rs` no longer matches the
/// translation of `debug-macros.cc` and the headers in
/// `MICROSCOPE_V8_TORQUE`, or when the translator meets a construct or a
/// name it does not know. Set `MICROSCOPE_UPDATE_V8=1` to rewrite it.
#[test]
fn slices_match_torque() {
    let expected = super::translate::generate(&torque_versions(), V8_NOTICE);
    if std::env::var_os("MICROSCOPE_UPDATE_V8").is_some() {
        std::fs::write(SLICES_FILE, &expected).expect("write the generated slices");
        return;
    }

    let actual = std::fs::read_to_string(SLICES_FILE).unwrap_or_default();
    assert!(
        actual == expected,
        "src/image/v8/slices.gen.rs is stale; rerun with MICROSCOPE_UPDATE_V8=1"
    );
}

const TRANSLATIONS_FILE: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/src/image/v8/translation.gen.rs"
);

/// Fails when `src/image/v8/translation.gen.rs` no longer matches the
/// translation opcodes of the headers in `MICROSCOPE_V8_TORQUE`, or when a
/// header no longer encodes a translation or a safepoint table as the
/// runtime model decodes it. Set `MICROSCOPE_UPDATE_V8=1` to rewrite it.
#[test]
fn translations_match_torque() {
    let expected = super::deoptimization::generate(&torque_versions(), V8_NOTICE);
    if std::env::var_os("MICROSCOPE_UPDATE_V8").is_some() {
        std::fs::write(TRANSLATIONS_FILE, &expected).expect("write the generated opcodes");
        return;
    }

    let actual = std::fs::read_to_string(TRANSLATIONS_FILE).unwrap_or_default();
    assert!(
        actual == expected,
        "src/image/v8/translation.gen.rs is stale; rerun with MICROSCOPE_UPDATE_V8=1"
    );
}

const BYTECODES_FILE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/src/image/v8/bytecodes.gen.rs");

/// Fails when `src/image/v8/bytecodes.gen.rs` no longer matches the
/// bytecodes of the headers in `MICROSCOPE_V8_TORQUE`. Set
/// `MICROSCOPE_UPDATE_V8=1` to rewrite it.
#[test]
fn bytecode_sizes_match_torque() {
    let expected = super::bytecodes::generate(&torque_versions(), V8_NOTICE);
    if std::env::var_os("MICROSCOPE_UPDATE_V8").is_some() {
        std::fs::write(BYTECODES_FILE, &expected).expect("write the generated sizes");
        return;
    }

    let actual = std::fs::read_to_string(BYTECODES_FILE).unwrap_or_default();
    assert!(
        actual == expected,
        "src/image/v8/bytecodes.gen.rs is stale; rerun with MICROSCOPE_UPDATE_V8=1"
    );
}

/// V8's own table of its bytecodes' sizes, which only a symbol table names.
const BYTECODE_SIZES: &str = "_ZN2v88internal11interpreter9Bytecodes14kBytecodeSizesE";

/// The bytecode sizes generated for each build's V8 are its own table's,
/// byte for byte, where its symbols name the table; Debian's libnode,
/// which names none, holds them once, in its read-only data.
#[test]
fn each_builds_bytecode_sizes_are_the_generated_ones() {
    use object::{Object, ObjectSection, ObjectSymbol, SectionKind};

    for (entry, path, (major, minor)) in matrix() {
        let data = std::fs::read(&path)
            .unwrap_or_else(|error| panic!("{entry}: read {}: {error}", path.display()));
        let object = object::File::parse(data.as_slice()).expect("an ELF file");
        let version = crate::image::v8::V8Version {
            major,
            minor,
            build: 0,
            patch: 0,
        };
        let generated = crate::image::v8::bytecode_sizes(version)
            .unwrap_or_else(|problem| panic!("{entry}: {problem}"));

        let symbol = object
            .symbols()
            .find(|symbol| symbol.name() == Ok(BYTECODE_SIZES));
        if let Some(symbol) = symbol {
            let section = object
                .section_by_index(symbol.section_index().expect("a section"))
                .expect("the symbol's section");
            let own = section
                .data_range(symbol.address(), symbol.size())
                .expect("the section's data")
                .expect("the symbol's bytes");
            assert_eq!(own, generated, "{entry}: kBytecodeSizes");
            continue;
        }

        assert_eq!(entry, "debian-24", "{entry}: no {BYTECODE_SIZES}");
        let found: usize = object
            .sections()
            .filter(|section| section.kind() == SectionKind::ReadOnlyData)
            .map(|section| {
                let bytes = section.data().expect("the section's data");
                bytes
                    .windows(generated.len())
                    .filter(|window| *window == generated)
                    .count()
            })
            .sum();
        assert_eq!(found, 1, "{entry}: copies of the generated sizes");
    }
}

/// Each build of the dev shell's Node matrix, the file that holds its V8,
/// and V8's major.minor version: Node 26 has 14.6, 24 has 13.6, and 22
/// has 12.4.
fn matrix() -> Vec<(&'static str, PathBuf, (u32, u32))> {
    let root = PathBuf::from(
        std::env::var_os("MICROSCOPE_NODE")
            .expect("MICROSCOPE_NODE names the Node builds; run tests in the dev shell"),
    );
    [
        ("26", (14, 6)),
        ("24", (13, 6)),
        ("22", (12, 4)),
        ("nixpkgs-26", (14, 6)),
        ("nixpkgs-24", (13, 6)),
        ("nixpkgs-22", (12, 4)),
        ("debian-24", (13, 6)),
    ]
    .into_iter()
    .map(|(entry, version)| {
        let path = if entry == "debian-24" {
            root.join("debian-24/root/usr/lib/x86_64-linux-gnu/libnode.so.137")
        } else {
            root.join(entry).join("node")
        };
        (entry, path, version)
    })
    .collect()
}

/// A loaded image of V8 carries its runtime as its provider read it, and
/// another module's carries none.
#[test]
fn a_loaded_v8_image_carries_its_runtime() {
    use crate::debug_info::{DebugFileSearch, load_module};
    use crate::{DebugFileOptions, ModuleImageId};

    let (_, libnode, _) = matrix()
        .into_iter()
        .find(|(entry, _, _)| *entry == "debian-24")
        .expect("Debian's libnode is in the matrix");
    let search = DebugFileSearch::new(&DebugFileOptions::default());
    let load = |path: &Path| {
        load_module(path, ModuleImageId::new(0), &search)
            .unwrap_or_else(|error| panic!("{}: {error}", path.display()))
    };

    let image = load(&libnode).image;
    let data = std::fs::read(&libnode).expect("read libnode");
    let object = object::File::parse(data.as_slice()).expect("an ELF file");
    let runtime = image.v8_runtime().expect("libnode's V8 runtime");
    assert_eq!(Some(runtime), super::runtime(&object));

    let libuv = libnode.with_file_name("libuv.so.1.0.0");
    assert_eq!(load(&libuv).image.v8_runtime(), None);
}

/// Every build in the matrix, Debian's libnode among them, binds to the
/// layout generated for its V8 version, which agrees with every field and
/// instance type the build's own constants give.
#[test]
fn every_build_in_the_matrix_binds_to_its_layout() {
    for (entry, path, (major, minor)) in matrix() {
        let data = std::fs::read(&path)
            .unwrap_or_else(|error| panic!("{entry}: read {}: {error}", path.display()));
        let object = object::File::parse(data.as_slice()).expect("an ELF file");
        let runtime = super::runtime(&object).unwrap_or_else(|| panic!("{entry}: no V8 runtime"));
        assert_eq!(
            (runtime.version.major, runtime.version.minor),
            (major, minor),
            "{entry}: V8 {}",
            runtime.version
        );
        assert_eq!(
            runtime.flags & DYNAMIC_SYMBOLS != 0,
            entry == "debian-24",
            "{entry}: only Debian's libnode names its constants only dynamically"
        );

        let layout = runtime
            .layout
            .unwrap_or_else(|problem| panic!("{entry}: {problem}"));
        let generated = layouts::GENERATED
            .iter()
            .find(|generated| (generated.major, generated.minor) == (major, minor))
            .expect("a generated layout");

        // The fields the build's own constants give, which binding has
        // checked, are where they were found on every line.
        assert_eq!(layout.field(Field::JSFunctionSharedFunctionInfo), Some(32));
        assert_eq!(layout.field(Field::HeapObjectMap), Some(0));
        assert_eq!(layout.field(Field::StringLength), Some(12));
        let exports = super::exports(&object);
        let checked: usize = super::CHECKED_FIELDS
            .iter()
            .map(|(_, name)| super::class_values(&exports.values, name).count())
            .sum();
        let types = exports
            .values
            .keys()
            .filter_map(|name| name.strip_prefix("v8dbg_type_")?.rsplit_once("__"))
            .filter(|(_, name)| generated.instance_type(name).is_some())
            .count();
        // Every type Torque assigns that the build exports agrees, as F21
        // found for the release configuration.
        let exported_types = match minor {
            4 => 204,
            6 if major == 13 => 215,
            _ => 217,
        };
        // Every constant the headers give that the build exports too.
        let constants = generated
            .checked
            .iter()
            .filter(|(name, _)| exports.values.contains_key(&format!("v8dbg_{name}")))
            .count();
        assert!(checked >= 55, "{entry}: {checked} fields checked");
        assert!(
            types >= exported_types,
            "{entry}: {types} instance types checked"
        );
        // The binary exports each one but a few of deoptimization data's
        // slots.
        let unexported: Vec<_> = generated
            .checked
            .iter()
            .filter(|(name, _)| !exports.values.contains_key(&format!("v8dbg_{name}")))
            .map(|(name, _)| *name)
            .filter(|name| !name.starts_with("DeoptimizationData"))
            .collect();
        assert!(
            unexported.is_empty(),
            "{entry}: {unexported:?} are not exported"
        );

        check_code_fields(entry, &object, &layout, (major, minor));

        check_accessors(
            entry,
            &object,
            &runtime.dispatch_table,
            &runtime.embedded_blob,
        );

        assert_eq!(layout.constant(Constant::TaggedSize), Some(8), "{entry}");
        // v8dbg_SmiTag is in .bss, which keeps no bytes in the file.
        assert_eq!(layout.constant(Constant::SmiTag), Some(0), "{entry}");
        assert!(
            layout
                .constant(Constant::OffFpFunction)
                .is_some_and(|offset| offset < 0),
            "{entry}"
        );
        eprintln!(
            "{entry}: V8 {}, {checked} field constants, {types} instance types, and \
             {constants} header constants agree; dispatch table {:?}; embedded blob {:?}",
            runtime.version, runtime.dispatch_table, runtime.embedded_blob
        );
    }
}

/// Whether `object` defines a symbol named `name`.
fn defines(object: &object::File<'_>, name: &str) -> bool {
    use object::{Object, ObjectSymbol};

    object
        .symbols()
        .chain(object.dynamic_symbols())
        .any(|symbol| symbol.is_definition() && symbol.name() == Ok(name))
}

/// The address of the local variable whose symbol's name has `part`, as
/// `_ZN2v88internal12_GLOBAL__N_1{L,}27current_embedded_blob_code_E{.0,}`.
fn variable(object: &object::File<'_>, part: &str) -> Option<ImageAddress> {
    use object::{Object, ObjectSymbol};

    object
        .symbols()
        .find(|symbol| {
            symbol.name().is_ok_and(|name| {
                name.starts_with("_ZN2v88internal12_GLOBAL__N_1") && name.contains(part)
            })
        })
        .map(|symbol| ImageAddress::new(symbol.address()))
}

/// Each build's dispatch table and embedded blob are where its code says:
/// as found by reading each build's instructions, and, where the build
/// names the blob's variables, at those variables.
fn check_accessors(
    entry: &str,
    object: &object::File<'_>,
    dispatch: &Result<Option<DispatchTable>, std::sync::Arc<str>>,
    blob: &Result<crate::image::v8::EmbeddedBlob, std::sync::Arc<str>>,
) {
    let group = {
        use object::{Object, ObjectSymbol};
        object
            .symbols()
            .find(|symbol| {
                symbol.name() == Ok("_ZN2v88internal12IsolateGroup22default_isolate_group_E")
            })
            .map(|symbol| ImageAddress::new(symbol.address()))
    };
    match entry {
        "22" | "nixpkgs-22" => assert_eq!(dispatch, &Ok(None), "{entry}"),
        "26" | "nixpkgs-26" => {
            assert_eq!(
                dispatch,
                &Ok(Some(DispatchTable::Isolate { offset: 0x268 })),
                "{entry}"
            );
        }
        "24" | "nixpkgs-24" => {
            let offset = if entry == "24" { 0x3270 } else { 0x3268 };
            let group = group.expect("IsolateGroup::default_isolate_group_");
            assert_eq!(
                dispatch,
                &Ok(Some(DispatchTable::Group { group, offset })),
                "{entry}"
            );
            // Where the symbol is named, the exported code says the same.
            assert_eq!(
                super::accessors::exported_dispatch_table(object).map(Some),
                *dispatch,
                "{entry}: from exported code"
            );
        }
        _ => {
            // No symbol names js_dispatch_table_address, read from exported
            // code; its unnamed code at 0xdb5140 says the same.
            let what = "Debian's js_dispatch_table_address";
            let accessor = super::accessors::instructions(object, 0xdb_5140)
                .and_then(|code| super::accessors::decode_dispatch_table(&code, what))
                .map(Some);
            let group = ImageAddress::new(0x465_e7e0);
            assert_eq!(
                accessor,
                Ok(Some(DispatchTable::Group {
                    group,
                    offset: 0x3270
                })),
                "{entry}"
            );
            assert_eq!(dispatch, &accessor, "{entry}");
        }
    }

    let blob = blob
        .as_ref()
        .unwrap_or_else(|problem| panic!("{entry}: {problem}"));
    let names = [
        ("27current_embedded_blob_code_E", blob.code),
        ("32current_embedded_blob_code_size_E", blob.code_size),
        ("27current_embedded_blob_data_E", blob.data),
        ("32current_embedded_blob_data_size_E", blob.data_size),
    ];
    if entry == "debian-24" {
        // Its symbols are gone; the accessors its code calls read these.
        let addresses = names.map(|(_, at)| at.get());
        assert_eq!(addresses, [0x465_c518, 0x465_c510, 0x465_c508, 0x465_c500]);
        return;
    }
    for (part, decoded) in names {
        assert_eq!(variable(object, part), Some(decoded), "{entry}: {part}");
    }
}

/// The code fields the build's constants give and those only the headers
/// do, which the cores of tests/fixtures/node/tiers.js confirm: a builtin's
/// id, an optimized code's parameter count.
fn check_code_fields(
    entry: &str,
    object: &object::File<'_>,
    layout: &crate::image::v8::V8Layout,
    (major, minor): (u32, u32),
) {
    let code = |field: Field| layout.field(field).unwrap_or(u64::MAX);
    assert_eq!(code(Field::CodeInstructionStart), 40, "{entry}");
    assert_eq!(code(Field::CodeWrapperCode), 8, "{entry}");
    let (flags, builtin_id) = match (major, minor) {
        (12, 4) => (48, 80),
        (13, 6) => (52, 86),
        _ => (52, 90),
    };
    assert_eq!(
        (code(Field::CodeFlags), code(Field::CodeBuiltinId)),
        (flags, builtin_id),
        "{entry}"
    );
    if (major, minor) == (14, 6) {
        // V8_JUMP_TABLE_INFO_BOOL is ENABLE_DISASSEMBLER, which every
        // 14.6 build has, as its Code::Disassemble says.
        assert!(
            defines(
                object,
                "_ZN2v88internal4Code11DisassembleEPKcRSoPNS0_7IsolateEm"
            ),
            "{entry}: built without the disassembler the layout assumes"
        );
    }
}
