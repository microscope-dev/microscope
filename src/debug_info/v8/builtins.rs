//! The numbers of V8's builtins, generated from each release build's own
//! `builtin_metadata` table for `src/image/v8/builtins.gen.rs`. A
//! builtin's number is its index in `BUILTIN_LIST`, which a function that
//! runs it keeps as its shared info's function data, a Smi. The list's
//! entries depend on the build's configuration (Intl, Temporal,
//! WebAssembly, leaptiering), so the numbers come from the binary the
//! release ships rather than from the headers: `builtins.cc`'s
//! `builtin_metadata`, one 24-byte `BuiltinMetadata` per builtin in that
//! order, each beginning with a pointer to the builtin's name.
//!
//! A table's pointers are the file's words where the build is not
//! position-independent, and the addends of their relative relocations
//! where it is. A build that keeps no symbol table, as Debian's libnode,
//! is found by the one relocated pointer to the first builtin's name that
//! the second's follows.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::Path;

use object::{Object, ObjectSection, ObjectSymbol};

use crate::image::v8::{BUILTIN_METADATA as METADATA, Builtin};

/// The size of one `BuiltinMetadata`: its name, its kind, and its data.
const ENTRY: u64 = 24;

/// The first and second builtins of every supported version.
const FIRST: [&str; 2] = ["DeoptimizationEntry_Eager", "DeoptimizationEntry_Lazy"];

/// A build's builtin names, in number order, read from its table.
pub(super) fn names(path: &Path) -> Vec<String> {
    let data =
        std::fs::read(path).unwrap_or_else(|error| panic!("read {}: {error}", path.display()));
    let object = object::File::parse(data.as_slice()).expect("an ELF file");
    let relocated: BTreeMap<u64, u64> = object
        .dynamic_relocations()
        .into_iter()
        .flatten()
        .filter(|(_, relocation)| {
            relocation.flags()
                == (object::RelocationFlags::Elf {
                    r_type: object::elf::R_X86_64_RELATIVE,
                })
        })
        .map(|(offset, relocation)| (offset, relocation.addend().cast_unsigned()))
        .collect();
    let bytes = |address: u64, size: u64| -> Option<&[u8]> {
        object
            .sections()
            .find_map(|section| section.data_range(address, size).ok().flatten())
    };
    let pointer = |address: u64| -> Option<u64> {
        relocated.get(&address).copied().or_else(|| {
            let word = bytes(address, 8)?;
            Some(u64::from_le_bytes(word.try_into().ok()?))
        })
    };
    let string = |address: u64| -> Option<String> {
        let section = object.sections().find(|section| {
            section.address() <= address && address < section.address() + section.size()
        })?;
        let data = section.data().ok()?;
        let start = usize::try_from(address - section.address()).ok()?;
        let end = start + data.get(start..)?.iter().position(|byte| *byte == 0)?;
        String::from_utf8(data[start..end].to_vec()).ok()
    };
    let name_at = |entry: u64| pointer(entry).and_then(string);

    let symbol = object
        .symbols()
        .find(|symbol| symbol.name().is_ok_and(|name| METADATA.contains(&name)));
    let by_names = || {
        let bases: Vec<u64> = relocated
            .keys()
            .copied()
            .filter(|&entry| {
                name_at(entry).as_deref() == Some(FIRST[0])
                    && name_at(entry + ENTRY).as_deref() == Some(FIRST[1])
            })
            .collect();
        let [base] = bases[..] else {
            panic!("{}: {} tables of builtins", path.display(), bases.len());
        };
        (base, None)
    };
    let (base, count) = symbol.map_or_else(by_names, |symbol| {
        assert_eq!(
            symbol.size() % ENTRY,
            0,
            "{}: the table's size",
            path.display()
        );
        (symbol.address(), Some(symbol.size() / ENTRY))
    });

    let mut names = Vec::new();
    for index in 0.. {
        if count.is_some_and(|count| index == count) {
            break;
        }
        let entry = base + index * ENTRY;
        // Without a symbol's size, the table ends at the first entry whose
        // name is no relocated pointer to a name. Debian's leaves an entry
        // of a C++ builtin it does not build, DummyBuiltin, all zeros.
        let name = match count {
            Some(_) => name_at(entry)
                .unwrap_or_else(|| panic!("{}: builtin {index} has no name", path.display())),
            None => match relocated.get(&entry).copied().and_then(string) {
                Some(name)
                    if !name.is_empty() && name.bytes().all(|byte| byte.is_ascii_graphic()) =>
                {
                    name
                }
                None if bytes(entry, ENTRY)
                    .is_some_and(|entry| entry.iter().all(|byte| *byte == 0))
                    && !(0..ENTRY / 8).any(|word| relocated.contains_key(&(entry + 8 * word))) =>
                {
                    String::new()
                }
                _ => break,
            },
        };
        names.push(name);
    }
    while names.last().is_some_and(String::is_empty) {
        names.pop();
    }
    assert_eq!(
        names.first().map(String::as_str),
        Some(FIRST[0]),
        "{}",
        path.display()
    );
    names
}

/// The generated file, from the builtins of each version's release build
/// in `builds`.
pub(super) fn generate(builds: &[((u32, u32), Vec<String>)], notice: &str) -> String {
    let mut text = String::from(
        "// @generated from the builtin_metadata table of each supported V8\n\
         // version's official Node build in MICROSCOPE_NODE, by\n\
         // `debug_info::v8::conformance::builtin_ids_match_release`. Do not\n\
         // edit; rerun that test with MICROSCOPE_UPDATE_V8=1.\n\
         //\n\
         // The numbers are V8's, which carries this notice:\n\
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
        "\nuse super::{Builtin, Numbers};\n\n\
         /// Each version's major and minor version, how many builtins it has,\n\
         /// and the number of each builtin an await chain is read by.\n\
         pub(super) const BUILTINS: &[(u32, u32, u32, Numbers)] = &[\n",
    );
    for ((major, minor), names) in builds {
        writeln!(text, "    ({major}, {minor}, {}, &[", names.len()).expect("write");
        for &builtin in Builtin::ALL {
            let number = names
                .iter()
                .position(|name| name == builtin.name())
                .unwrap_or_else(|| panic!("V8 {major}.{minor} has no builtin {}", builtin.name()));
            writeln!(text, "        (Builtin::{builtin:?}, {number}),").expect("write");
        }
        writeln!(text, "    ]),").expect("write");
    }
    text.push_str("];\n");
    text
}
