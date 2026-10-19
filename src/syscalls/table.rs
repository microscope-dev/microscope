//! System call names and numbers for each ABI, generated from the kernel's
//! UAPI headers into `table.gen.rs`.

use super::model::{Abi, SyscallId};

mod generated {
    include!("table.gen.rs");
}

/// The kernel's entry points, from its x86 syscall tables.
mod entries {
    include!("entries.gen.rs");
}

// Every table is searched by number, which needs each in ascending order.
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

    let mut index = 1;
    while index < entries::X86_32.len() {
        assert!(
            entries::X86_32[index - 1].0 < entries::X86_32[index].0,
            "the x86-32 entry table is out of order"
        );
        index += 1;
    }
};

/// Whether a table's numbers strictly ascend.
const fn ascending(table: &[(u32, &str)]) -> bool {
    let mut index = 1;

    while index < table.len() {
        if table[index - 1].0 >= table[index].0 {
            return false;
        }

        index += 1;
    }

    true
}

/// The kernel function `id` enters through, with its ABI's prefix.
pub(super) fn kernel_function(id: SyscallId) -> Option<String> {
    let find = |table: &[(u32, &'static str)]| {
        table
            .binary_search_by_key(&id.number, |&(number, _)| number)
            .ok()
            .map(|index| table[index].1)
    };

    match id.abi {
        // x32's own calls are compat entries, which x86-64 kernels also
        // name `__x64_`.
        Abi::X86_64 => find(entries::X86_64).map(|entry| format!("__x64_{entry}")),
        Abi::X32 => find(entries::X32).map(|entry| format!("__x64_{entry}")),
        Abi::X86_32 => entries::X86_32
            .binary_search_by_key(&id.number, |&(number, _, _)| number)
            .ok()
            .map(|index| {
                let (_, entry, compat) = entries::X86_32[index];
                format!("__ia32_{}", if compat.is_empty() { entry } else { compat })
            }),
    }
}

/// Every call of an ABI, as (number, name), in ascending number order.
#[must_use]
pub const fn calls(abi: Abi) -> &'static [(u32, &'static str)] {
    match abi {
        Abi::X86_64 => generated::X86_64,
        Abi::X86_32 => generated::X86_32,
        Abi::X32 => generated::X32,
    }
}

/// The bit x32 calls set in their numbers, which the tables leave out.
pub const X32_SYSCALL_BIT: u32 = 0x4000_0000;

pub(super) fn name(id: SyscallId) -> Option<&'static str> {
    index(id).map(|index| calls(id.abi)[index].1)
}

/// Where its ABI's table lists a call.
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

/// The highest number any ABI's table lists, plus one: the size of a
/// bitmap over one ABI's calls.
#[must_use]
pub fn limit(abi: Abi) -> u32 {
    calls(abi).last().map_or(0, |&(number, _)| number + 1)
}

/// Every call in every ABI.
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

    /// Every entry point is of a call the headers number, past those a
    /// newer kernel adds, and a call's
    /// function is named as the kernel names it, which is not always the
    /// call's name.
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

            // x32 shares x86-64's numbers below its own calls.
            let newer = limit(if abi == Abi::X32 { Abi::X86_64 } else { abi });
            for number in numbers {
                // A newer kernel's tables go past the headers'.
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

    /// The kernel UAPI headers the flake provides, among the BPF include
    /// directories.
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

    /// Reads `#define __NR_name number` lines, with x32's numbers written as
    /// `(__X32_SYSCALL_BIT + number)`.
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

    /// Fails when `table.gen.rs` no longer matches the pinned kernel
    /// headers. Set `MICROSCOPE_UPDATE_SYSCALLS=1` to rewrite it.
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

    #[test]
    fn names_and_numbers_round_trip_in_each_abi() {
        let openat = SyscallId::named(Abi::X86_64, "openat").expect("openat");
        assert_eq!(openat.number, 257);
        assert_eq!(
            SyscallId::named(Abi::X86_32, "openat").map(|id| id.number),
            Some(295)
        );
        assert_eq!(
            SyscallId::named(Abi::X32, "openat").map(|id| id.number),
            Some(257)
        );
        assert_eq!(SyscallId::new(Abi::X86_32, 1).name(), Some("exit"));
        assert_eq!(
            SyscallId::new(Abi::X86_64, 9999).display_name(),
            "syscall_0x270f"
        );
    }
}
