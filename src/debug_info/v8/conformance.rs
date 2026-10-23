//! The generated layouts checked against their two sources: V8's Torque
//! output for each supported version, which `layouts.gen.rs` is generated
//! from here, and each build in the dev shell's Node matrix, whose own
//! `v8dbg_` constants every layout must agree with.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use super::{BINARY_FIELDS, layouts, requires_constant, requires_field};
use crate::image::v8::{Constant, DYNAMIC_SYMBOLS, Field};

const GENERATED_FILE: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/src/debug_info/v8/layouts.gen.rs"
);

/// The names older versions' Torque gives a field the enum names by its
/// newest.
const RENAMED: [(Field, &str); 3] = [
    (Field::ByteArrayValues, "ByteArray::Bytes"),
    (Field::TrustedByteArrayValues, "TrustedByteArray::Bytes"),
    (Field::ScriptInfos, "Script::SharedFunctionInfos"),
];

/// The class each bit-field constant names, and the macro of
/// `bit-fields.h` that defines its bit fields.
const BIT_FIELDS: [(&str, &str); 3] = [
    ("SharedFunctionInfo", "SHARED_FUNCTION_INFO_FLAGS"),
    ("ScopeInfo", "SCOPE_FLAGS"),
    ("Script", "SCRIPT_FLAGS"),
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
/// `using Name = base::BitField<Type, shift, size, Storage>;`.
fn torque_bit_fields(header: &str) -> BTreeMap<String, (u32, u32)> {
    let mut fields = BTreeMap::new();
    for (class, flags) in BIT_FIELDS {
        let start = format!("#define DEFINE_TORQUE_GENERATED_{flags}() \\\n");
        let list = header
            .split_once(&start)
            .unwrap_or_else(|| panic!("bit-fields.h defines {flags}"))
            .1;
        for line in list.lines().take_while(|line| !line.trim().is_empty()) {
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
        }
    }

    fields
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

/// One version's entry of `layouts.gen.rs`.
fn generate_version(text: &mut String, (major, minor): (u32, u32), torque: &Path) {
    let fields = torque_fields(&read(torque, "class-debug-readers.cc"));
    let types = torque_instance_types(&read(torque, "instance-types.h"));
    let bits = torque_bit_fields(&read(torque, "bit-fields.h"));

    writeln!(text, "    Generated {{").expect("write");
    writeln!(text, "        major: {major},\n        minor: {minor},").expect("write");
    writeln!(text, "        fields: &[").expect("write");
    for &field in Field::ALL {
        if BINARY_FIELDS.iter().any(|(own, _)| *own == field) {
            continue;
        }

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
        match torque_constant(constant, &types, &bits) {
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

    writeln!(text, "        ],\n    }},").expect("write");
}

fn generate() -> String {
    let mut text = String::from(
        "// @generated from V8's Torque output for each supported version, the\n\
         // class-debug-readers.cc, instance-types.h, and bit-fields.h that\n\
         // MICROSCOPE_V8_TORQUE holds, by\n\
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
    for (version, torque) in torque_versions() {
        generate_version(&mut text, version, &torque);
    }

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
        assert!(checked >= 47, "{entry}: {checked} fields checked");
        assert!(
            types >= exported_types,
            "{entry}: {types} instance types checked"
        );

        for (field, _) in BINARY_FIELDS {
            assert!(layout.has(field), "{entry}: {}", field.name());
        }

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
            "{entry}: V8 {}, {checked} field constants and {types} instance types agree",
            runtime.version
        );
    }
}
