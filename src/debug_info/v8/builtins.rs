use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::Path;

use object::{Object, ObjectSection, ObjectSymbol};

use crate::image::v8::{BUILTIN_METADATA as METADATA, Builtin};

const ENTRY: u64 = 24;

const FIRST: [&str; 2] = ["DeoptimizationEntry_Eager", "DeoptimizationEntry_Lazy"];

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
