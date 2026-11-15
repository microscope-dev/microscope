#!/usr/bin/env bash






set -euo pipefail

tables="$1/arch/x86/entry/syscalls"
out="$(dirname "$0")/../src/syscalls/entries.gen.rs"

{
    echo "// Generated from the kernel's arch/x86/entry/syscalls/syscall_{64,32}.tbl"
    echo "// by scripts/syscall-entries.sh; do not edit."
    echo
    echo "pub(super) const X86_64: &[(u32, &str)] = &["
    awk '!/^#/ && NF >= 4 && ($2 == "common" || $2 == "64") { printf "    (%s, \"%s\"),\n", $1, $4 }' \
        "$tables/syscall_64.tbl"
    echo "];"
    echo
    echo "pub(super) const X32: &[(u32, &str)] = &["
    awk '!/^#/ && NF >= 4 && ($2 == "common" || $2 == "x32") { printf "    (%s, \"%s\"),\n", $1, $4 }' \
        "$tables/syscall_64.tbl"
    echo "];"
    echo
    echo "/// Each call's entry and, when it has one, its compat entry."
    echo "pub(super) const X86_32: &[(u32, &str, &str)] = &["
    awk '!/^#/ && NF >= 4 && $2 == "i386" {
        compat = (NF >= 5 && $5 != "-") ? $5 : ""
        printf "    (%s, \"%s\", \"%s\"),\n", $1, $4, compat
    }' "$tables/syscall_32.tbl"
    echo "];"
} > "$out"
