//! What reading an optimized frame's deoptimization data needs of V8's
//! headers, as V8's `OptimizedJSFrame::Summarize` reads it: the safepoint
//! tables that give a call's deoptimization index, the slots of a
//! deoptimization entry, and the opcodes of the translations it indexes,
//! with the checks that the headers still encode them as the runtime model
//! decodes them. The constants join `layouts.gen.rs`; the opcodes, a table
//! per version, are `src/image/v8/translation.gen.rs`.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::PathBuf;

use super::headers::Headers;
use crate::image::v8::TranslationOpcode;

/// The fields of both safepoint tables' entry configurations the runtime
/// model reads; the Turbofan table's tagged slots' bytes are not.
const CONFIGURATION: [&str; 4] = [
    "HasDeoptDataField",
    "RegisterIndexesSizeField",
    "PcSizeField",
    "DeoptIndexSizeField",
];

/// The safepoint tables, by class, with their header.
const SAFEPOINT_TABLES: [(&str, &str); 2] = [
    ("SafepointTable", "src/codegen/safepoint-table.h"),
    (
        "MaglevSafepointTable",
        "src/codegen/maglev-safepoint-table.h",
    ),
];

/// Adds to `constants`, by the names [`crate::image::v8::Constant`] gives
/// them, what reading a deoptimization translation needs.
pub(super) fn add_constants(headers: &Headers, constants: &mut BTreeMap<String, i64>) {
    check_code_metadata(headers);
    for (class, file) in SAFEPOINT_TABLES {
        let offsets = headers.header_offsets(file, class);
        for name in ["kLengthOffset", "kEntryConfigurationOffset", "kHeaderSize"] {
            let value = offsets
                .get(name)
                .unwrap_or_else(|| panic!("{class} has no {name}"));
            constants.insert(format!("{class}::{name}"), *value);
        }
        let fields = headers.bit_fields(file, class);
        for name in CONFIGURATION {
            let &(shift, size) = fields
                .get(name)
                .unwrap_or_else(|| panic!("{class} has no {name}"));
            constants.insert(format!("{class}::{name}::kShift"), shift);
            constants.insert(
                format!("{class}::{name}::kMask"),
                ((1 << size) - 1) << shift,
            );
        }
        check_entries(headers, class, file);
    }
    let maglev = headers.class_text(SAFEPOINT_TABLES[1].1, SAFEPOINT_TABLES[1].0);
    let pushed = regex::Regex::new(r"const int num_pushed_registers_size = (\d+);")
        .expect("a pattern")
        .captures(maglev)
        .expect("a Maglev safepoint entry counts its extra spill slots")[1]
        .parse()
        .expect("a size");
    constants.insert(
        "MaglevSafepointTable::num_pushed_registers_size".to_owned(),
        pushed,
    );

    let deopt = "src/objects/deoptimization-data.h";
    check_says(
        headers,
        deopt,
        "return kFirstDeoptEntryIndex + (i * kDeoptEntrySize);",
    );
    for name in ["kDeoptEntrySize", "kTranslationIndexOffset"] {
        let value = headers
            .class_constant(deopt, &["DeoptimizationData"], name)
            .unwrap_or_else(|| panic!("DeoptimizationData has no {name}"));
        constants.insert(format!("DeoptimizationData::{name}"), value);
    }

    constants.insert(
        "internal::kFunctionEntryBytecodeOffset".to_owned(),
        headers.constant("src/common/globals.h", "kFunctionEntryBytecodeOffset"),
    );
    let first = headers
        .class_constant(
            "src/utils/utils.h",
            &["BytecodeOffset"],
            "kFirstBuiltinContinuationId",
        )
        .expect("BytecodeOffset::kFirstBuiltinContinuationId");
    constants.insert(
        "BytecodeOffset::kFirstBuiltinContinuationId".to_owned(),
        first,
    );
    check_translation_encoding(headers);
}

/// `text` with each run of whitespace one space.
fn collapsed(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Fails unless `file` says `text`, whitespace aside.
fn check_says(headers: &Headers, file: &str, text: &str) {
    assert!(
        collapsed(headers.file(file)).contains(&collapsed(text)),
        "{file} no longer says {text:?}"
    );
}

/// Checks that optimized code's safepoint table is where the runtime model
/// looks: at the start of its metadata, which follows its instructions, up
/// to its handler table.
fn check_code_metadata(headers: &Headers) {
    for (file, text) in [
        (
            "src/objects/code.h",
            "inline int safepoint_table_offset() const { return 0; }",
        ),
        (
            "src/objects/code-inl.h",
            "if (has_instruction_stream()) {\n    \
             static_assert(InstructionStream::kOnHeapBodyIsContiguous);\n    \
             return instruction_start() + instruction_size();",
        ),
        (
            "src/objects/code-inl.h",
            "return metadata_start() + safepoint_table_offset();",
        ),
        (
            "src/objects/code-inl.h",
            "return handler_table_offset() - safepoint_table_offset();",
        ),
        (
            "src/objects/code-inl.h",
            "inline bool Code::is_maglevved() const { return kind() == CodeKind::MAGLEV; }",
        ),
    ] {
        check_says(headers, file, text);
    }
}

/// Checks that a safepoint table's entries are laid out as the runtime
/// model reads them: each `entry_size()` bytes from the header, its pc,
/// then with deoptimization data its index and trampoline pc, each plus
/// one, then for Maglev its extra spill slots' byte, then its registers.
fn check_entries(headers: &Headers, class: &str, file: &str) {
    let body = collapsed(headers.class_text(file, class));
    let maglev = class == "MaglevSafepointTable";
    let mut expected = vec![
        "Address entry_ptr = safepoint_table_address_ + kHeaderSize + index * entry_size();",
        "int pc = read_bytes(&entry_ptr, pc_size());",
        "if (has_deopt_data()) {",
        "deopt_index = read_bytes(&entry_ptr, deopt_index_size()) - 1;",
        "trampoline_pc = read_bytes(&entry_ptr, pc_size()) - 1;",
    ];
    if maglev {
        expected.push("uint8_t num_extra_spill_slots = read_byte(&entry_ptr);");
    }
    expected.extend([
        "read_bytes(&entry_ptr, register_indexes_size());",
        "int deopt_data_size = has_deopt_data() ? pc_size() + deopt_index_size() : 0;",
    ]);
    expected.push(if maglev {
        "return pc_size() + deopt_data_size + num_pushed_registers_size + register_indexes_size();"
    } else {
        "return pc_size() + deopt_data_size + register_indexes_size();"
    });
    expected.push("result |= uint32_t{");

    let mut rest = body.as_str();
    for text in expected {
        let at = rest
            .find(text)
            .unwrap_or_else(|| panic!("{class} no longer says {text:?} where expected"));
        rest = &rest[at + text.len()..];
    }
    assert!(
        rest.starts_with("*reinterpret_cast<uint8_t*>(*ptr)} << (8 * b);")
            || rest.starts_with("read_byte(ptr)} << (8 * b);"),
        "{class} no longer reads its entries little-endian"
    );
}

/// Checks that a translation is a byte per opcode with VLQ operands, as
/// `base/vlq.h` decodes them, and that no build compresses it unless asked:
/// `--turbo-compress-frame-translations` is off by default with zlib and
/// without.
fn check_translation_encoding(headers: &Headers) {
    let flags = "src/flags/flag-definitions.h";
    let text = collapsed(headers.file(flags));
    assert_eq!(
        text.matches("turbo_compress_frame_translations, false,")
            .count(),
        2,
        "{flags} no longer defaults turbo_compress_frame_translations to false"
    );
    assert!(!text.contains("turbo_compress_frame_translations, true"));

    let vlq = "src/base/vlq.h";
    for expected in [
        "static constexpr uint32_t kContinueShift = 7;",
        "static constexpr uint32_t kDataMask = kContinueBit - 1;",
        "if (cur_byte <= kDataMask) {\n    return cur_byte;\n  }",
        "for (int shift = kContinueShift; shift <= 32; shift += kContinueShift) {\n    \
         cur_byte = get_next();\n    bits |= (cur_byte & kDataMask) << shift;\n    \
         if (cur_byte <= kDataMask) break;\n  }",
        "bool is_negative = (bits & 1) == 1;\n  int32_t result = bits >> 1;\n  \
         return is_negative ? -result : result;",
    ] {
        check_says(headers, vlq, expected);
    }
}

/// `src/image/v8/translation.gen.rs`: each version's translation opcodes.
pub(super) fn generate(versions: &[((u32, u32), PathBuf)], notice: &str) -> String {
    let mut text = String::from(
        "// @generated from V8's src/deoptimizer/translation-opcode.h for each\n\
         // supported version, which MICROSCOPE_V8_TORQUE holds, by\n\
         // `debug_info::v8::conformance::translations_match_torque`. Do not\n\
         // edit; rerun that test with MICROSCOPE_UPDATE_V8=1.\n\
         //\n\
         // The opcodes are V8's, which carries this notice:\n\
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
        "\nuse super::TranslationOpcode as T;\n\
         use super::Translations;\n\n\
         pub(super) const TRANSLATIONS: &[Translations] = &[\n",
    );
    for ((major, minor), torque) in versions {
        let v8 = torque.parent().expect("a version's directory").join("v8");
        // The opcodes do not depend on leaptiering.
        let headers = Headers::read(&v8, *major > 12);
        let (opcodes, js_frames, frames) = headers.translation_opcodes();
        writeln!(text, "    Translations {{").expect("write");
        writeln!(text, "        major: {major},\n        minor: {minor},").expect("write");
        writeln!(text, "        js_frame_opcodes: {js_frames},").expect("write");
        writeln!(text, "        frame_opcodes: {frames},").expect("write");
        writeln!(text, "        opcodes: &[").expect("write");
        for (name, operands) in opcodes {
            // 12.4 spells JavaScript JAVA_SCRIPT.
            let own = name.replace("JAVA_SCRIPT_", "JAVASCRIPT_");
            let opcode = TranslationOpcode::named(&own)
                .unwrap_or_else(|| panic!("V8 {major}.{minor} has an unknown opcode {name}"));
            assert!(operands <= 6, "{name} has {operands} operands");
            writeln!(text, "            (T::{opcode:?}, {operands}),").expect("write");
        }
        writeln!(text, "        ],\n    }},").expect("write");
    }
    text.push_str("];\n");
    text
}
