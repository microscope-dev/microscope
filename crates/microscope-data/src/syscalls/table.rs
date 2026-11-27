//! Generated Linux x86 (`x86_64`, `x86_32`, `x32`) syscall number and kernel entry tables.
//!
//! Compile-time assertions verify ascending order so number-keyed lookups can
//! use binary search.

use super::model::{Abi, SyscallId};

mod generated {
    include!("table.gen.rs");
}

mod entries {
    include!("entries.gen.rs");
}

const _: () = {
    assert!(
        ascending(generated::X86_64),
        "the x86-64 call table is out of order"
    );
    assert!(
        ascending(generated::X86_32),
        "the x86-32 call table is out of order"
    );
    assert!(
        ascending(generated::X32),
        "the x32 call table is out of order"
    );
    assert!(
        ascending(entries::X86_64),
        "the x86-64 entry table is out of order"
    );
    assert!(
        ascending(entries::X32),
        "the x32 entry table is out of order"
    );

    let mut rest = entries::X86_32;
    while let [first, tail @ ..] = rest {
        if let [second, ..] = tail {
            assert!(first.0 < second.0, "the x86-32 entry table is out of order");
        }
        rest = tail;
    }
};

const fn ascending(mut table: &[(u32, &str)]) -> bool {
    while let [first, rest @ ..] = table {
        if let [second, ..] = rest
            && first.0 >= second.0
        {
            return false;
        }

        table = rest;
    }

    true
}

pub(super) fn kernel_function(id: SyscallId) -> Option<String> {
    let find = |table: &[(u32, &'static str)]| {
        table
            .binary_search_by_key(&id.number, |&(number, _)| number)
            .ok()
            .and_then(|index| table.get(index))
            .map(|&(_, entry)| entry)
    };

    match id.abi {
        Abi::X86_64 => find(entries::X86_64).map(|entry| format!("__x64_{entry}")),
        Abi::X32 => find(entries::X32).map(|entry| format!("__x64_{entry}")),
        Abi::X86_32 => entries::X86_32
            .binary_search_by_key(&id.number, |&(number, _, _)| number)
            .ok()
            .and_then(|index| entries::X86_32.get(index))
            .map(|&(_, entry, compat)| {
                format!("__ia32_{}", if compat.is_empty() { entry } else { compat })
            }),
    }
}

#[must_use]
pub const fn calls(abi: Abi) -> &'static [(u32, &'static str)] {
    match abi {
        Abi::X86_64 => generated::X86_64,
        Abi::X86_32 => generated::X86_32,
        Abi::X32 => generated::X32,
    }
}

pub const X32_SYSCALL_BIT: u32 = 0x4000_0000;

pub(super) fn name(id: SyscallId) -> Option<&'static str> {
    index(id)
        .and_then(|index| calls(id.abi).get(index))
        .map(|&(_, name)| name)
}

pub(super) fn index(id: SyscallId) -> Option<usize> {
    calls(id.abi)
        .binary_search_by_key(&id.number, |&(number, _)| number)
        .ok()
}

pub(super) fn number(abi: Abi, name: &str) -> Option<u32> {
    calls(abi)
        .iter()
        .find(|&&(_, candidate)| candidate == name)
        .map(|&(number, _)| number)
}

#[must_use]
pub fn limit(abi: Abi) -> u32 {
    calls(abi).last().map_or(0, |&(number, _)| number + 1)
}

pub fn all() -> impl Iterator<Item = SyscallId> {
    Abi::ALL.into_iter().flat_map(|abi| {
        calls(abi)
            .iter()
            .map(move |&(number, _)| SyscallId::new(abi, number))
    })
}

#[cfg(test)]
mod tests {
    use std::fmt::Write as _;
    use std::path::Path;

    use super::*;

    const GENERATED: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/src/syscalls/table.gen.rs");

    #[test]
    fn entry_points_are_of_numbered_calls_and_named_as_the_kernel_names_them() {
        for (abi, numbers) in [
            (
                Abi::X86_64,
                entries::X86_64.iter().map(|&(n, _)| n).collect::<Vec<_>>(),
            ),
            (Abi::X32, entries::X32.iter().map(|&(n, _)| n).collect()),
            (
                Abi::X86_32,
                entries::X86_32.iter().map(|&(n, _, _)| n).collect(),
            ),
        ] {
            assert!(numbers.is_sorted(), "{abi:?}");

            let newer = limit(if abi == Abi::X32 { Abi::X86_64 } else { abi });
            for number in numbers {
                assert!(
                    SyscallId::new(abi, number).name().is_some() || number >= newer,
                    "{abi:?} {number} has an entry but no name"
                );
            }
        }

        let function = |abi, name| SyscallId::named(abi, name).and_then(SyscallId::kernel_function);

        assert_eq!(
            function(Abi::X86_64, "fstat").as_deref(),
            Some("__x64_sys_newfstat")
        );
        assert_eq!(
            function(Abi::X86_64, "umount2").as_deref(),
            Some("__x64_sys_umount")
        );
        assert_eq!(
            function(Abi::X32, "rt_sigaction").as_deref(),
            Some("__x64_compat_sys_rt_sigaction")
        );
        assert_eq!(
            function(Abi::X86_32, "fstat").as_deref(),
            Some("__ia32_compat_sys_newfstat")
        );
        assert_eq!(
            function(Abi::X86_32, "getpid").as_deref(),
            Some("__ia32_sys_getpid")
        );
    }

    fn uapi() -> std::path::PathBuf {
        let include = std::env::var("MICROSCOPE_BPF_INCLUDE")
            .expect("MICROSCOPE_BPF_INCLUDE names the kernel headers; run in the dev shell");
        include
            .split(':')
            .map(Path::new)
            .find(|path| path.join("asm/unistd_64.h").exists())
            .expect("one MICROSCOPE_BPF_INCLUDE directory holds asm/unistd_64.h")
            .to_owned()
    }

    fn parse(header: &str) -> Vec<(u32, String)> {
        let mut calls: Vec<(u32, String)> = header
            .lines()
            .filter_map(|line| {
                let rest = line.strip_prefix("#define __NR_")?;
                let (name, value) = rest.split_once(' ')?;
                let value = value.trim();
                let value = value
                    .strip_prefix("(__X32_SYSCALL_BIT + ")
                    .and_then(|value| value.strip_suffix(')'))
                    .unwrap_or(value);
                Some((value.parse().ok()?, name.to_owned()))
            })
            .collect();
        calls.sort();
        calls
    }

    fn generate() -> String {
        let headers = uapi().join("asm");
        let mut text = String::from(
            "// Generated from the kernel's asm/unistd_{64,32,x32}.h by\n\
             // `syscalls::table::tests::tables_match_the_kernel_headers`; do not edit.\n\
             // Rerun it with MICROSCOPE_UPDATE_SYSCALLS=1 after the headers change.\n",
        );
        for (constant, file) in [
            ("X86_64", "unistd_64.h"),
            ("X86_32", "unistd_32.h"),
            ("X32", "unistd_x32.h"),
        ] {
            let header = std::fs::read_to_string(headers.join(file)).expect("read a header");
            let calls = parse(&header);
            assert!(calls.len() > 300, "{file} lists the calls");
            writeln!(text, "\npub(super) const {constant}: &[(u32, &str)] = &[").expect("write");
            for (number, name) in calls {
                writeln!(text, "    ({number}, {name:?}),").expect("write");
            }

            text.push_str("];\n");
        }

        text
    }

    #[test]
    fn tables_match_the_kernel_headers() {
        let expected = generate();
        if std::env::var_os("MICROSCOPE_UPDATE_SYSCALLS").is_some() {
            std::fs::write(GENERATED, &expected).expect("write the generated tables");
            return;
        }

        let actual = std::fs::read_to_string(GENERATED).unwrap_or_default();
        assert!(
            actual == expected,
            "src/syscalls/table.gen.rs is stale; rerun with MICROSCOPE_UPDATE_SYSCALLS=1"
        );
    }
}
