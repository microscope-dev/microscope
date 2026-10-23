//! The sizes of V8's bytecodes, generated from `bytecodes.h` and
//! `bytecode-operands.h` for `src/image/v8/bytecodes.gen.rs`: the table V8
//! keeps as `Bytecodes::kBytecodeSizes`, three rows, one per operand scale
//! (1, 2, 4), of one size per bytecode in `BYTECODE_LIST`'s order. A
//! bytecode is one byte, and each operand adds its type's unscaled size,
//! times the scale where the type is scalable (`bytecode-traits.h`).
//!
//! The lists are macros that call one another; each is expanded only as
//! far as its own callbacks and the lists it names, and any other call, an
//! unknown operand type or size, or two `#ifdef` definitions of a list
//! that disagree fails, rather than being guessed.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::PathBuf;

use regex::Regex;

/// The parameters and body of each definition of each function-like macro
/// of `text`, its comments removed and its lines joined.
fn defines(text: &str) -> BTreeMap<String, Vec<(Vec<String>, String)>> {
    let text = Regex::new(r"(?s)/\*.*?\*/")
        .expect("a pattern")
        .replace_all(text, " ");
    let text = Regex::new(r"//[^\n]*")
        .expect("a pattern")
        .replace_all(&text, "");
    let text = text.replace("\\\n", " ");
    let mut macros: BTreeMap<String, Vec<_>> = BTreeMap::new();
    let pattern = Regex::new(r"(?m)^[ \t]*#define[ \t]+(\w+)\(([^)]*)\)(.*)$").expect("a pattern");
    for define in pattern.captures_iter(&text) {
        let parameters = define[2].split(',').map(|p| p.trim().to_owned()).collect();
        macros
            .entry(define[1].to_owned())
            .or_default()
            .push((parameters, define[3].to_owned()));
    }
    macros
}

/// Each call `NAME(arguments)` at the top level of a macro's body.
fn calls(body: &str) -> Vec<(String, Vec<String>)> {
    let head = Regex::new(r"(\w+)\s*\(").expect("a pattern");
    let mut found = Vec::new();
    let mut at = 0;
    while let Some(call) = head.captures_at(body, at) {
        let whole = call.get(0).expect("the match");
        let mut depth = 1;
        let mut end = whole.end();
        for (index, c) in body[whole.end()..].char_indices() {
            depth += match c {
                '(' => 1,
                ')' => -1,
                _ => 0,
            };
            if depth == 0 {
                end = whole.end() + index;
                break;
            }
        }
        assert_eq!(depth, 0, "an unbalanced call in {body:?}");
        let arguments = body[whole.end()..end]
            .split(',')
            .map(|argument| argument.trim().to_owned())
            .collect();
        found.push((call[1].to_owned(), arguments));
        at = end + 1;
    }
    found
}

/// The entries the list macro `name` gives its callbacks, each by its
/// arguments, through the lists it calls.
fn expand(macros: &BTreeMap<String, Vec<(Vec<String>, String)>>, name: &str) -> Vec<Vec<String>> {
    let definitions = macros
        .get(name)
        .unwrap_or_else(|| panic!("no definition of {name}"));
    let expansions: Vec<Vec<Vec<String>>> = definitions
        .iter()
        .map(|(parameters, body)| {
            let mut entries = Vec::new();
            for (callee, arguments) in calls(body) {
                if parameters.contains(&callee) {
                    entries.push(arguments);
                } else if macros.contains_key(&callee) {
                    assert!(
                        arguments.iter().all(|argument| parameters.contains(argument)),
                        "{name} calls {callee} with {arguments:?}"
                    );
                    entries.extend(expand(macros, &callee));
                } else {
                    panic!("{name} calls the unknown {callee}");
                }
            }
            entries
        })
        .collect();
    assert!(
        expansions.iter().all(|entries| *entries == expansions[0]),
        "the definitions of {name} disagree"
    );
    expansions.into_iter().next().unwrap_or_default()
}

/// Each bytecode's name and its sizes at the scales 1, 2, and 4, from a
/// version's `bytecodes.h` and `bytecode-operands.h`.
pub(super) fn sizes(bytecodes: &str, operands: &str) -> Vec<(String, [u8; 3])> {
    let operand_macros = defines(operands);
    let mut infos = BTreeMap::new();
    for entry in expand(&operand_macros, "OPERAND_TYPE_INFO_LIST") {
        let [name, scalable, _, size] = entry.as_slice() else {
            panic!("an operand type's information {entry:?}");
        };
        let scalable = match scalable.as_str() {
            "true" => true,
            "false" => false,
            other => panic!("{name} is scalable as {other:?}"),
        };
        let size: u8 = match size.as_str() {
            "OperandSize::kNone" => 0,
            "OperandSize::kByte" => 1,
            "OperandSize::kShort" => 2,
            "OperandSize::kQuad" => 4,
            other => panic!("{name} has the size {other:?}"),
        };
        infos.insert(format!("OperandTypeInfo::k{name}"), (scalable, size));
    }
    let mut types = BTreeMap::new();
    for entry in expand(&operand_macros, "OPERAND_TYPE_LIST") {
        let [name, info] = entry.as_slice() else {
            panic!("an operand type {entry:?}");
        };
        let info = infos
            .get(info)
            .unwrap_or_else(|| panic!("{name} has the unknown {info}"));
        types.insert(format!("OperandType::k{name}"), *info);
    }

    expand(&defines(bytecodes), "BYTECODE_LIST")
        .into_iter()
        .map(|entry| {
            let [name, _uses, operands @ ..] = entry.as_slice() else {
                panic!("a bytecode {entry:?}");
            };
            let mut sizes = [1u8; 3];
            for operand in operands {
                let (scalable, size) = types
                    .get(operand)
                    .unwrap_or_else(|| panic!("{name} has the unknown operand {operand}"));
                for (row, scale) in sizes.iter_mut().zip([1, 2, 4]) {
                    *row += size * if *scalable { scale } else { 1 };
                }
            }
            (name.clone(), sizes)
        })
        .collect()
}

/// The table `Bytecodes::kBytecodeSizes` holds: each scale's row in turn.
pub(super) fn table(sizes: &[(String, [u8; 3])]) -> Vec<u8> {
    (0..3)
        .flat_map(|row| sizes.iter().map(move |(_, own)| own[row]))
        .collect()
}

/// The generated file, with each version of `versions` (its major.minor
/// and its Torque output's directory) beside its headers.
pub(super) fn generate(versions: &[((u32, u32), PathBuf)], notice: &str) -> String {
    let mut text = String::from(
        "// @generated from V8's src/interpreter/bytecodes.h and\n\
         // bytecode-operands.h for each supported version, which\n\
         // MICROSCOPE_V8_TORQUE holds, by\n\
         // `debug_info::v8::conformance::bytecode_sizes_match_torque`. Do not\n\
         // edit; rerun that test with MICROSCOPE_UPDATE_V8=1.\n\
         //\n\
         // The sizes are V8's, which carries this notice:\n\
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
        "\n/// Each version's `Bytecodes::kBytecodeSizes`: its major and minor\n\
         /// version, and three rows, at the operand scales 1, 2, and 4, of one\n\
         /// size per bytecode.\n\
         pub(super) const BYTECODE_SIZES: &[(u32, u32, &[u8])] = &[\n",
    );
    for ((major, minor), torque) in versions {
        let interpreter = torque
            .parent()
            .expect("a version's directory")
            .join("v8/src/interpreter");
        let read = |file: &str| {
            std::fs::read_to_string(interpreter.join(file))
                .unwrap_or_else(|error| panic!("read {file} of V8 {major}.{minor}: {error}"))
        };
        let sizes = sizes(&read("bytecodes.h"), &read("bytecode-operands.h"));
        writeln!(text, "    ({major}, {minor}, &[").expect("write");
        for row in table(&sizes).chunks(sizes.len()) {
            for line in row.chunks(24) {
                let bytes: Vec<String> = line.iter().map(u8::to_string).collect();
                writeln!(text, "        {},", bytes.join(", ")).expect("write");
            }
        }
        writeln!(text, "    ]),").expect("write");
    }
    text.push_str("];\n");
    text
}

/// A list's entries come from its callbacks and from the lists it calls,
/// in order, and a call of anything else fails.
#[test]
fn lists_expand_through_the_lists_they_call() {
    let header = "\
#define INNER(V) V(B, x) /* comment */ V(C, y)
#define OUTER(V, V_TSA) \\
  V(A, w)               \\
  INNER(V)              \\
  V_TSA(D, z)
";
    let macros = defines(header);
    let names: Vec<String> = expand(&macros, "OUTER")
        .into_iter()
        .map(|entry| entry[0].clone())
        .collect();
    assert_eq!(names, ["A", "B", "C", "D"]);

    let unknown = defines("#define LIST(V) V(A, x) OTHER(V)\n");
    assert!(std::panic::catch_unwind(|| expand(&unknown, "LIST")).is_err());
}

/// A bytecode's size at each scale is one byte and its operands', only
/// the scalable ones scaled.
#[test]
fn a_bytecode_is_its_byte_and_its_operands() {
    let operands = "\
#define OPERAND_TYPE_INFO_LIST(V) \\
  V(None, false, false, OperandSize::kNone) \\
  V(ScalableSignedByte, true, false, OperandSize::kByte) \\
  V(FixedUnsignedShort, false, true, OperandSize::kShort)
#define OPERAND_TYPE_LIST(V) \\
  V(Reg, OperandTypeInfo::kScalableSignedByte) \\
  V(Flag16, OperandTypeInfo::kFixedUnsignedShort)
";
    let bytecodes = "\
#define BYTECODE_LIST(V) \\
  V(Nop, ImplicitRegisterUse::kNone) \\
  V(Mov, ImplicitRegisterUse::kNone, OperandType::kReg, OperandType::kReg) \\
  V(Jump, ImplicitRegisterUse::kNone, OperandType::kFlag16, OperandType::kReg)
";
    let sizes = sizes(bytecodes, operands);
    assert_eq!(
        sizes,
        [
            ("Nop".to_owned(), [1, 1, 1]),
            ("Mov".to_owned(), [3, 5, 9]),
            ("Jump".to_owned(), [4, 5, 7]),
        ]
    );
    assert_eq!(table(&sizes), [1, 3, 4, 1, 5, 5, 1, 9, 7]);
}
