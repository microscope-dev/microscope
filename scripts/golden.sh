#!/usr/bin/env bash
















set -euo pipefail

readonly golden_dir="tests/golden"
readonly build_dir="build/golden"

readonly source_root="/microscope"


readonly determinism_runs=20
readonly common_flags="-std=c11 -g -nostdlib -ffreestanding -fno-builtin -fno-stack-protector -fcf-protection=none -Wall -Wextra -Werror"
readonly static="-static -fno-pie"
readonly static_pie="-static-pie -fpie"



readonly gcc_O2="-O2 -fno-tree-vectorize"
readonly clang_O2="-O2 -fno-vectorize -fno-slp-vectorize"
readonly variants=(
    "gcc-O0 gcc -O0 -fno-omit-frame-pointer ${static}"
    "gcc-O0-nofp gcc -O0 -fomit-frame-pointer ${static}"
    "gcc-O2 gcc ${gcc_O2} -fno-omit-frame-pointer ${static}"
    "gcc-O2-nofp gcc ${gcc_O2} -fomit-frame-pointer ${static}"
    "gcc-O2-pie gcc ${gcc_O2} -fomit-frame-pointer ${static_pie}"
    "clang-O0 clang -O0 -fno-omit-frame-pointer ${static}"
    "clang-O0-nofp clang -O0 -fomit-frame-pointer ${static}"
    "clang-O0-pie clang -O0 -fno-omit-frame-pointer ${static_pie}"
    "clang-O2 clang ${clang_O2} -fno-omit-frame-pointer ${static}"
    "clang-O2-nofp clang ${clang_O2} -fomit-frame-pointer ${static}"
)

die() {
    printf 'golden: %s\n' "$*" >&2
    exit 1
}

json_string() {
    local text="$1"
    text=${text//\\/\\\\}
    text=${text//\"/\\\"}
    text=${text//$'\n'/\\n}
    text=${text//$'\t'/\\t}
    [[ "$text" != *[[:cntrl:]]* ]] || die "unsupported control character in ${1@Q}"
    printf '"%s"' "$text"
}

hash_of() {
    sha256sum "$1" | cut -d' ' -f1
}


binary_of() {
    printf '%s/%s/%s-%s' "$build_dir" "$1" "$1" "$2"
}

toolchain() {
    gcc --version | head -n1
    clang --version | head -n1
}



sources_of() {
    local name="$1"
    printf '%s\n' "${name}.c" ../rt/rt.c ../rt/rt.h
    if grep -q '^#include "../rt/thread.h"$' "${golden_dir}/${name}/${name}.c"; then
        printf '%s\n' ../rt/thread.c ../rt/thread.h
    fi
    if grep -q '^#include "../rt/process.h"$' "${golden_dir}/${name}/${name}.c"; then
        printf '%s\n' ../rt/process.c ../rt/process.h
    fi
}



manifest() {
    local name="$1" gcc_version="$2" clang_version="$3"
    local directory="${golden_dir}/${name}"
    local separator

    printf '{\n  "program": %s,\n' "$(json_string "$name")"
    printf '  "toolchain": {\n    "gcc": %s,\n    "clang": %s\n  },\n' \
        "$(json_string "$gcc_version")" "$(json_string "$clang_version")"

    printf '  "sources": [\n'
    separator=""
    while read -r source; do
        printf '%s    {"path": %s, "sha256": "%s"}' "$separator" \
            "$(json_string "$source")" "$(hash_of "${directory}/${source}")"
        separator=$',\n'
    done < <(sources_of "$name")
    printf '\n  ],\n'

    printf '  "variants": [\n'
    separator=""
    local variant
    for variant in "${variants[@]}"; do
        read -r variant_name compiler flags <<<"$variant"
        local binary
        binary=$(binary_of "$name" "$variant_name")
        printf '%s    {"name": %s, "compiler": %s, "flags": %s, "sha256": "%s"}' "$separator" \
            "$(json_string "$variant_name")" "$(json_string "$compiler")" \
            "$(json_string "${common_flags} ${flags}")" "$(hash_of "$binary")"
        separator=$',\n'
    done
    printf '\n  ],\n'

    printf '  "runs": [\n'
    separator=""
    local arguments
    while IFS= read -r arguments; do
        local -a argv=()
        read -r -a argv <<<"$arguments"
        local expected_output="" expected_status=""
        for variant in "${variants[@]}"; do
            read -r variant_name _ <<<"$variant"
            local binary output status=0
            binary=$(binary_of "$name" "$variant_name")
            output=$("./${binary}" "${argv[@]}"; printf x) || status=$?
            if [[ $status -eq 0 ]]; then

                "./${binary}" "${argv[@]}" >/dev/null || status=$?
            fi
            output=${output%x}
            if [[ -z "$expected_status" ]]; then
                expected_output=$output
                expected_status=$status
            elif [[ "$output" != "$expected_output" || "$status" != "$expected_status" ]]; then
                die "${binary} ${arguments} disagrees with the other variants"
            fi
        done
        local json_arguments="" argument_separator=""
        local argument
        for argument in "${argv[@]}"; do
            json_arguments+="${argument_separator}$(json_string "$argument")"
            argument_separator=", "
        done
        printf '%s    {"arguments": [%s], "exit_code": %s, "output": %s}' "$separator" \
            "$json_arguments" "$expected_status" "$(json_string "$expected_output")"
        separator=$',\n'
    done <"${directory}/arguments"
    printf '\n  ]\n}\n'
}








entry_values() {
    awk 'FNR == NR {
            if (/<End of list>/) { list = ""; next }
            if (/location view pair/) next
            if (list == "" && match($0, /^ +([0-9a-f]{8}) /, found)) list = strtonum("0x" found[1])
            if (match($0, /([0-9a-f]{16}) ([0-9a-f]{16}) \(DW_OP_(GNU_)?entry_value: \(DW_OP_reg[0-9]+ \(([a-z0-9]+)\)\); DW_OP_stack_value\)$/, found)) {
                ranges[list] = ranges[list] " " strtonum("0x" found[1]) ":" strtonum("0x" found[2]) ":" found[4]
            }
            next
         }
         function flush() {
            if (die != "" && location != "" && !inlined[depth]) lists[die] = location
            if (die != "" && name != "") names[die] = name
            if (die != "" && origin != "") origins[die] = origin
            die = name = origin = location = ""
         }
         match($0, /^ *<([0-9]+)><([0-9a-f]+)>: Abbrev Number/, found) {
            flush()
            depth = found[1] + 0
            inlined[depth] = (depth > 0 && inlined[depth - 1]) || index($0, "(DW_TAG_inlined_subroutine)") > 0
            die = strtonum("0x" found[2])
            next
         }
         /DW_AT_name / { name = $NF }
         match($0, /DW_AT_abstract_origin: \([a-z_0-9]+\) <0x([0-9a-f]+)>/, found) { origin = strtonum("0x" found[1]) }
         match($0, /DW_AT_location *:.* (0x[0-9a-f]+) \(location list\)/, found) { location = strtonum(found[1]) }
         END {
            flush()
            PROCINFO["sorted_in"] = "@ind_num_asc"
            for (die in lists) {
                variable = die
                for (hops = 0; !(variable in names) && (variable in origins) && hops < 8; hops++) {
                    variable = origins[variable]
                }
                if (!(variable in names) || !(lists[die] in ranges)) continue
                count = split(substr(ranges[lists[die]], 2), pairs, " ")
                for (entry = 1; entry <= count; entry++) {
                    split(pairs[entry], bounds, ":")
                    printf "%s        [\"%s\", %d, %d, \"%s\"]", separator, names[variable], bounds[1], bounds[2], bounds[3]
                    separator = ",\n"
                }
            }
            printf "\n"
         }' <(readelf -W --debug-dump=loc "$1") <(readelf -W --debug-dump=info "$1")
}







facts() {
    local name="$1"
    local separator="" variant
    printf '{\n  "program": %s,\n  "variants": [\n' "$(json_string "$name")"
    for variant in "${variants[@]}"; do
        read -r variant_name _ flags <<<"$variant"
        local binary
        binary=$(binary_of "$name" "$variant_name")
        local optimized=false
        [[ " ${flags} " == *" -O0 "* ]] || optimized=true
        local inlined
        inlined=$(readelf -W --debug-dump=info "$binary" | grep -c 'DW_TAG_inlined_subroutine' || true)
        printf '%s    {\n      "name": %s,\n      "optimized": %s,\n      "inlined": %s,\n' \
            "$separator" "$(json_string "$variant_name")" "$optimized" "$inlined"
        printf '      "functions": [\n'
        nm --defined-only --print-size "$binary" |
            awk '$3 == "T" || $3 == "t" {
                    printf "%s        [\"%s\", %d, %d]", separator, $4, strtonum("0x" $1), strtonum("0x" $2)
                    separator = ",\n"
                 }
                 END { printf "\n" }'
        printf '      ],\n      "lines": [\n'
        readelf -W --debug-dump=decodedline "$binary" |
            awk '$3 ~ /^0x[0-9a-f]+$/ && ($2 == "-" || $2 ~ /^[0-9]+$/) {
                    file = $1
                    sub(/.*\//, "", file)
                    line = $2 == "-" ? -1 : $2
                    statement = $NF == "x" ? "true" : "false"
                    printf "%s        [%d, \"%s\", %d, %s]", separator, strtonum($3), file, line, statement
                    separator = ",\n"
                 }
                 END { printf "\n" }'
        printf '      ],\n      "epilogues": ['


        readelf -W --debug-dump=rawline "$binary" |
            awk 'match($0, /(set Address|advance Address by [0-9]+|Advance PC by [a-z ]*[0-9]+) to (0x[0-9a-f]+)/, found) {
                    address = strtonum(gensub(/.* to (0x[0-9a-f]+).*/, "\\1", 1, found[0]))
                 }
                 /Set epilogue_begin to true/ { pending = 1; next }
                 pending && (/Special opcode/ || /\]  Copy/) {
                    printf "%s%d", separator, address
                    separator = ", "
                    pending = 0
                 }'
        printf '],\n      "entry_values": [\n'
        entry_values "$binary"
        printf '      ]\n    }'
        separator=$',\n'
    done
    printf '\n  ]\n}\n'
}



compile() {
    local name="$1"
    local directory="${golden_dir}/${name}"
    [[ -f "${directory}/${name}.c" ]] || die "no program ${directory}/${name}.c"
    mkdir -p "${build_dir}/${name}"
    local -a sources=()
    mapfile -t sources < <(sources_of "$name" | grep '\.c$' | sed "s|^|${directory}/|")
    printf '[golden] building %s\n' "$name"
    local variant
    for variant in "${variants[@]}"; do
        read -r variant_name compiler flags <<<"$variant"



        # shellcheck disable=SC2086
        NIX_HARDENING_ENABLE= NIX_CFLAGS_COMPILE= "$compiler" $common_flags $flags \
            "-ffile-prefix-map=${PWD}=${source_root}" \
            -o "$(binary_of "$name" "$variant_name")" "${sources[@]}"
    done
    facts "$name" >"${build_dir}/${name}/facts.json.tmp"
    mv "${build_dir}/${name}/facts.json.tmp" "${build_dir}/${name}/facts.json"
}



inputs_of() {
    local name="$1"
    local directory="${golden_dir}/${name}"
    {
        cat "${BASH_SOURCE[0]}" "${directory}/manifest.json" "${directory}/arguments"
        toolchain
        local source
        while read -r source; do
            cat "${directory}/${source}"
        done < <(sources_of "$name")
    } | sha256sum | cut -d' ' -f1
}



build_one() {
    local name="$1"
    local manifest_path="${golden_dir}/${name}/manifest.json"
    local stamp="${build_dir}/${name}/inputs" inputs
    inputs=$(inputs_of "$name")
    if [[ -f "$stamp" && "$(<"$stamp")" == "$inputs" ]]; then
        return
    fi
    rm -f "$stamp"
    compile "$name"
    local gcc_version clang_version
    gcc_version=$(sed -n 's/^    "gcc": "\(.*\)",$/\1/p' "$manifest_path")
    clang_version=$(sed -n 's/^    "clang": "\(.*\)"$/\1/p' "$manifest_path")
    if ! diff -u "$manifest_path" <(manifest "$name" "$gcc_version" "$clang_version"); then
        printf 'golden: %s does not match what %s builds and does.\n' "$manifest_path" "$name" >&2
        printf 'golden: recorded with %s and %s; built with %s and %s.\n' \
            "$gcc_version" "$clang_version" "$(gcc --version | head -n1)" "$(clang --version | head -n1)" >&2
        die "if its sources or the toolchain changed on purpose, run: just golden-record ${name}"
    fi
    printf '%s\n' "$inputs" >"$stamp"
}

build() {
    local manifest_path
    for manifest_path in "${golden_dir}"/*/manifest.json; do
        build_one "$(basename "$(dirname "$manifest_path")")"
    done
}

record() {
    local name="$1"
    local directory="${golden_dir}/${name}"
    rm -f "${build_dir}/${name}/inputs"
    compile "$name"
    local gcc_version clang_version
    gcc_version=$(gcc --version | head -n1)
    clang_version=$(clang --version | head -n1)
    manifest "$name" "$gcc_version" "$clang_version" >"${directory}/manifest.json.tmp"
    mv "${directory}/manifest.json.tmp" "${directory}/manifest.json"
    printf '[golden] wrote %s/manifest.json\n' "$directory"


    local run
    for run in $(seq "$determinism_runs"); do
        manifest "$name" "$gcc_version" "$clang_version" | cmp -s - "${directory}/manifest.json" ||
            die "${name} behaved differently on run ${run}: its output depends on scheduling"
    done
    printf '[golden] %s behaves the same in %s more runs\n' "$name" "$determinism_runs"
    inputs_of "$name" >"${build_dir}/${name}/inputs"
}

case "${1:-}" in
build)
    [[ $# -eq 1 ]] || die "usage: golden.sh build"
    build
    ;;
record)
    [[ $# -eq 2 ]] || die "usage: golden.sh record NAME"
    record "$2"
    ;;
*)
    die "usage: golden.sh build | golden.sh record NAME"
    ;;
esac
