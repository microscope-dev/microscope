#!/usr/bin/env bash

set -euo pipefail

readonly output_dir="build/test-programs"
readonly fixtures_dir="tests/fixtures"
readonly c_fixtures_dir="${fixtures_dir}/c"
readonly cpp_fixtures_dir="${fixtures_dir}/cpp"
readonly go_fixtures_dir="${fixtures_dir}/go"
readonly rust_fixtures_dir="${fixtures_dir}/rust"
readonly zig_fixtures_dir="${fixtures_dir}/zig"
readonly odin_fixtures_dir="${fixtures_dir}/odin"
readonly fortran_fixtures_dir="${fixtures_dir}/fortran"
readonly d_fixtures_dir="${fixtures_dir}/d"
readonly nim_fixtures_dir="${fixtures_dir}/nim"
readonly ada_fixtures_dir="${fixtures_dir}/ada"
readonly suite_stamp="${output_dir}/.suite.stamp"
readonly suite_outputs="${output_dir}/.suite.outputs"
readonly frame_oracle_script=scripts/frame-variables-oracle.py


readonly checkout="checkout=${PWD}"



shell_seed=$(grep -o -- '-frandom-seed=[^ ]*' <<<"${NIX_CFLAGS_COMPILE-}" || true)
if [[ -n "$shell_seed" ]]; then
    export NIX_CFLAGS_COMPILE="${NIX_CFLAGS_COMPILE//"$shell_seed"/}"
fi
readonly random_seed="random-seed=none"

declare -A dash_version_by_tool=()
declare -A rebuilt_outputs=()
dash_version=""
gdb_version=""
go_version=""
go_target=""
zig_version=""
odin_version=""
fortran_version=""
d_version=""
nim_version=""
ada_version=""

read_dash_version() {
    local tool="$1"
    if [[ ! -v "dash_version_by_tool[$tool]" ]]; then
        local version
        version=$("$tool" --version)
        dash_version_by_tool["$tool"]=${version%%$'\n'*}
    fi
    dash_version=${dash_version_by_tool["$tool"]}
}






readonly max_jobs="${MICROSCOPE_FIXTURE_JOBS:-$(nproc)}"
readonly background_builders=" build_program build_fixture build_c_fixture_directory \
build_cpp_fixture_directory build_cpp_fixture build_shared_fixture build_symbols_library \
build_disassembly_fixture build_tls_modules_fixture build_ubuntu_tls_fixture build_rust_fixture build_go_fixture \
build_go_command build_zig_fixture build_zig_self_hosted_fixture build_odin_fixture build_fortran_fixture build_d_fixture build_nim_fixture build_ada_fixture "

declare -A job_outputs=()
declare -A build_jobs=()
failed_jobs=()
trap 'running=$(jobs -pr); [[ -z "$running" ]] || kill $running 2>/dev/null || true' EXIT

reap_job() {
    local pid="$1"
    local status="$2"
    if (( status != 0 )); then
        failed_jobs+=("${job_outputs[$pid]}")
    fi
    unset "job_outputs[$pid]" "build_jobs[$pid]"
}

check_jobs() {
    if (( ${#failed_jobs[@]} > 0 )); then
        printf 'error: could not make %s\n' "${failed_jobs[@]}" >&2
        exit 1
    fi
}




spawn_job() {
    local kind="$1"
    local output="$2"
    shift 2
    while (( ${#job_outputs[@]} >= max_jobs )); do
        local pid="" status=0
        wait -n -p pid "${!job_outputs[@]}" || status=$?
        reap_job "$pid" "$status"
    done
    "$@" &
    job_outputs[$!]="$output"
    if [[ "$kind" == build ]]; then
        build_jobs[$!]="$output"
    fi
}


wait_builds() {
    local pid
    for pid in "${!build_jobs[@]}"; do
        local status=0
        wait "$pid" || status=$?
        reap_job "$pid" "$status"
    done
    check_jobs
}


wait_for() {
    local pid output
    for pid in "${!job_outputs[@]}"; do
        for output in "$@"; do
            if [[ "${job_outputs[$pid]}" == "$output" ]]; then
                local status=0
                wait "$pid" || status=$?
                reap_job "$pid" "$status"
                break
            fi
        done
    done
    check_jobs
}


wait_jobs() {
    local pid
    for pid in "${!job_outputs[@]}"; do
        local status=0
        wait "$pid" || status=$?
        reap_job "$pid" "$status"
    done
    check_jobs
}


called_by_builders() {
    local caller
    for caller in "${FUNCNAME[@]:2}"; do
        [[ "$caller" == main || "$background_builders" == *" $caller "* ]] || return 1
    done
}



wait_for_inputs() {
    local source="$1"
    local output="$2"
    shift 2
    local -a inputs=("$output") directories=() libraries=()
    [[ "$source" != *"$output_dir"* ]] || inputs+=("$source")
    local word
    for word in "$@"; do
        case "$word" in
            -l*) libraries+=("${word#-l}") ;;
            *"$output") ;;
            -L"$output_dir"*) directories+=("${word#-L}") ;;
            "$output_dir"/*) inputs+=("$word") ;;
            *"$output_dir"*)
                wait_builds
                return
                ;;
        esac
    done
    local directory library
    for directory in "${directories[@]}"; do
        for library in "${libraries[@]}"; do
            inputs+=("$directory/lib${library}.so" "$directory/lib${library}.a")
        done
    done
    wait_for "${inputs[@]}"
}


compile() {
    local output="$1"
    local signature="$2"
    shift 2
    NIX_HARDENING_ENABLE= "$@" || return
    printf '%s\n' "$signature" >"${output}.command.tmp"
    mv "${output}.command.tmp" "${output}.command"
}

source_changed_since_output() {
    local source="$1"
    local output="$2"
    local changed
    changed=$(find "$source" -newer "$output" -print -quit)
    [[ -n "$changed" ]]
}



built_input_changed_since_output() {
    local output="$1"
    shift
    local word path
    for word in "$@"; do
        path="${word#*=}"
        if [[ "$path" == "$output_dir"/* && "$path" != "$output" && -f "$path" \
            && "$path" -nt "$output" ]]; then
            return 0
        fi
    done
    return 1
}

run_cached_build() {
    local source="$1"
    local output="$2"
    local metadata="$3"
    shift 3
    local -a command=("$@")
    local command_text
    printf -v command_text '%q ' "${command[@]}"
    local signature="${checkout}"$'\n'"${random_seed}"$'\n'"${metadata}"$'\n'"command=${command_text}"
    local stamp="${output}.command"
    local previous=""

    wait_for_inputs "$source" "$output" "${command[@]}"
    if [[ -f "$stamp" ]]; then
        previous=$(<"$stamp")
    fi

    if [[ -x "$output" ]] \
        && ! source_changed_since_output "$source" "$output" \
        && ! built_input_changed_since_output "$output" "${command[@]}" \
        && [[ "$previous" == "$signature" ]]; then
        rebuilt_outputs["$output"]=false
        printf '[cached] %s\n' "$output"
        return
    fi

    printf '[build]  %s\n' "$output"
    rebuilt_outputs["$output"]=true
    if (( max_jobs > 1 )) && called_by_builders; then
        spawn_job build "$output" compile "$output" "$signature" "${command[@]}"
    else
        compile "$output" "$signature" "${command[@]}"
    fi
}

build_program() {
    local tool="$1"
    local source="$2"
    local output="$3"
    shift 3

    local -a command=(
        "$tool"
        "$@"
        "$source"
        -o "$output"
    )
    read_dash_version "$tool"
    run_cached_build "$source" "$output" \
        "compiler=${dash_version}"$'\n'"target=x86_64-linux"$'\n'"backend=${tool}" \
        "${command[@]}"
}

build_fixture() {
    local compiler="$1"
    local source="$2"
    local output="$3"
    shift 3
    build_program "$compiler" "$source" "$output" \
        -std=c17 -Wall -Wextra -Werror "$@"
}

build_c_fixture_directory() {
    local compiler="$1"
    local source_dir="$2"
    local output="$3"
    shift 3
    local -a sources=()
    mapfile -d '' sources < <(
        find "$source_dir" -maxdepth 1 -type f -name '*.c' -print0 | sort -z
    )
    if (( ${#sources[@]} == 0 )); then
        printf 'error: C fixture has no source files: %s\n' "$source_dir" >&2
        exit 1
    fi
    local -a command=(
        "$compiler" -std=c17 -Wall -Wextra -Werror "$@" "${sources[@]}" -o "$output"
    )
    read_dash_version "$compiler"
    run_cached_build "$source_dir" "$output" \
        "compiler=${dash_version}"$'\n'"target=x86_64-linux"$'\n'"backend=${compiler}" \
        "${command[@]}"
}

build_cpp_fixture_directory() {
    local compiler="$1"
    local source_dir="$2"
    local output="$3"
    shift 3
    local -a sources=()
    mapfile -d '' sources < <(
        find "$source_dir" -maxdepth 1 -type f -name '*.cpp' -print0 | sort -z
    )
    local -a command=(
        "$compiler" -std=c++20 -Wall -Wextra -Werror "$@" "${sources[@]}" -o "$output"
    )
    read_dash_version "$compiler"
    run_cached_build "$source_dir" "$output" \
        "compiler=${dash_version}"$'\n'"target=x86_64-linux"$'\n'"backend=${compiler}" \
        "${command[@]}"
}

build_cpp_fixture() {
    local compiler="$1"
    local source="$2"
    local output="$3"
    shift 3
    build_program "$compiler" "$source" "$output" \
        -std=c++20 -Wall -Wextra -Werror "$@"
}

build_shared_fixture() {
    local compiler="$1"
    local source="$2"
    local output="$3"
    shift 3
    build_program "$compiler" "$source" "$output" \
        -std=c17 -Wall -Wextra -Werror -shared -fPIC "$@"
}



build_symbols_library() {
    local compiler="$1"
    local output="$2"
    local soname="${3:-${output##*/}}"
    local source_dir="$c_fixtures_dir/elf-symbols"
    local -a command=(
        "$compiler" -std=c17 -Wall -Wextra -Werror -shared -fPIC -O2
        "-Wl,-soname,${soname}" "$source_dir/library.c" "$source_dir/layout.S" -o "$output"
    )
    read_dash_version "$compiler"
    run_cached_build "$source_dir" "$output" \
        "compiler=${dash_version}"$'\n'"target=x86_64-linux"$'\n'"backend=${compiler}" \
        "${command[@]}"
}



build_disassembly_fixture() {
    local compiler="$1"
    local output="$2"
    shift 2
    local source_dir="$c_fixtures_dir/disassembly"
    local -a command=(
        "$compiler" -std=c17 -Wall -Wextra -Werror -g3 -gdwarf-5 -Wl,-z,lazy "$@"
        "$source_dir/main.c" "$source_dir/layout.S" "$source_dir/indirect.S" -o "$output"
    )
    read_dash_version "$compiler"
    run_cached_build "$source_dir" "$output" \
        "compiler=${dash_version}"$'\n'"target=x86_64-linux"$'\n'"backend=${compiler}" \
        "${command[@]}"
}



build_tls_modules_fixture() {
    local compiler="$1"
    local output="$2"
    shift 2
    local -a command=(
        "$compiler" -std=c17 -Wall -Wextra -Werror -g3 -gdwarf-5 -pthread "$@" -o "$output"
    )
    read_dash_version "$compiler"
    run_cached_build "$c_fixtures_dir/tls-modules" "$output" \
        "compiler=${dash_version}"$'\n'"target=x86_64-linux"$'\n'"backend=${compiler}" \
        "${command[@]}"
}








build_ubuntu_tls_fixture() {
    local kind="$1"
    local version="$2"
    local output="$3"
    shift 3
    local root
    root=$(realpath "$MICROSCOPE_UBUNTU_GLIBC/$version")
    # shellcheck disable=SC2016
    local script='
        set -euo pipefail
        kind="$1"; root="$2"; version="$3"; output="$4"; shift 4
        compile=()
        while [[ "$1" != -- ]]; do compile+=("$1"); shift; done
        shift
        lib="$root/lib"; dev="$root/dev"; object="${output}.o"
        position=-fPIE
        [[ "$kind" == program ]] || position=-fPIC
        zig cc -target "x86_64-linux-gnu.${version}" -fno-sanitize=undefined -std=c17 \
            -Wall -Wextra -Werror -O0 -g3 -gdwarf-5 "$position" "${compile[@]}" -c -o "$object"
        if [[ "$kind" == program ]]; then
            start=(-pie "$dev/Scrt1.o")
            threads=("$lib/libpthread.so.0" "$lib/libdl.so.2")
        else
            start=(-shared "-Wl,-soname,${output##*/}")
            threads=()
        fi
        gcc -nostdlib "${start[@]}" "$dev/crti.o" "$(gcc -print-file-name=crtbeginS.o)" \
            "$object" -Wl,--as-needed "$@" "${threads[@]}" "$lib/libc.so.6" \
            "$dev"/lib*_nonshared.a "$lib/ld-linux-x86-64.so.2" \
            "$(gcc -print-file-name=crtendS.o)" "$dev/crtn.o" -o "$output"
        if [[ "$kind" == program ]]; then
            patchelf --set-interpreter "$(realpath "$lib/ld-linux-x86-64.so.2")" \
                --set-rpath "\$ORIGIN:$lib" "$output"
        else
            patchelf --remove-rpath "$output"
        fi
        rm "$object"
    '
    local -a command=(bash -c "$script" _ "$kind" "$root" "$version" "$output" "$@")
    if [[ -z "$zig_version" ]]; then
        zig_version=$(zig version)
    fi
    read_dash_version gcc
    run_cached_build "$c_fixtures_dir/tls-modules" "$output" \
        "compiler=zig ${zig_version}"$'\n'"linker=${dash_version}"$'\n'"target=x86_64-linux-gnu.${version}" \
        "${command[@]}"
}




derive_foreign_libc() {
    wait_builds
    local input="$1"
    local output="$2"
    local script
    # shellcheck disable=SC2016
    script='
        set -euo pipefail
        input="$1"; output="$2"
        byte_at() { od -An -tu1 -j "$1" -N1 "$output" | tr -d " "; }
        put_byte() { printf "\\$(printf %03o "$2")" | dd of="$output" bs=1 seek="$1" conv=notrunc status=none; }
        # Section numbers are bracketed and padded, so they are removed first.
        # awk reads all its input, printing only the first match: one that
        # exits early fails the pipeline when what feeds it is still writing.
        note=$(readelf -SW "$input" | sed "s/^ *\\[ *[0-9]*\\]//" \
            | awk "!found && \$1 == \".note.gnu.build-id\" { print \"0x\" \$4; found = 1 }")
        if [[ -z "$note" ]]; then
            printf "error: %s has no build-id note\n" "$input" >&2
            exit 1
        fi
        cp "$input" "$output.tmp"
        chmod u+w "$output.tmp"
        output="$output.tmp"
        # The build-id descriptor follows the 12-byte note header and "GNU".
        identifier=$(( note + 16 ))
        put_byte "$identifier" $(( $(byte_at "$identifier") ^ 255 ))
        mv "$output" "${output%.tmp}"
    '
    run_cached_build "$input" "$output" "derivation=foreign-libc-v2" \
        bash -c "$script" _ "$input" "$output"
}






derive_stripped_library() {
    wait_builds
    local input="$1"
    local output="$2"
    local embedded="$3"
    local script
    # shellcheck disable=SC2016
    script='
        set -euo pipefail
        input="$1"; output="$2"; embedded="$3"
        rm -f "$output" "$output.embedded" "$output.embedded.xz"
        strip --strip-all -o "$output" "$input"
        if [[ "$embedded" == yes ]]; then
            nm -D "$input" --format=posix --defined-only | awk "{ print \$1 }" | sort >"$output.dynsyms"
            nm "$input" --format=posix --defined-only \
                | awk "{ if (\$2 == \"T\" || \$2 == \"t\" || \$2 == \"D\") print \$1 }" \
                | sort >"$output.funcsyms"
            comm -13 "$output.dynsyms" "$output.funcsyms" >"$output.keep"
            objcopy --only-keep-debug "$input" "$output.embedded"
            objcopy -S --remove-section .gdb_index --remove-section .comment \
                "--keep-symbols=$output.keep" "$output.embedded"
            xz --keep "$output.embedded"
            objcopy --add-section ".gnu_debugdata=$output.embedded.xz" "$output"
            rm -f "$output.dynsyms" "$output.funcsyms" "$output.keep" "$output.embedded.xz"
        fi
    '
    run_cached_build "$input" "$output" \
        "derivation=stripped-v1"$'\n'"embedded=${embedded}" \
        bash -c "$script" _ "$input" "$output" "$embedded"
}






derive_split_debug() {
    wait_builds
    local input="$1"
    local output="$2"
    local layout="$3"
    local root="${4:-}"
    local script
    # shellcheck disable=SC2016
    script='
        set -euo pipefail
        input="$1"; output="$2"; layout="$3"; root="$4"
        objcopy --only-keep-debug "$input" "$output.debug.tmp"
        strip --strip-all -o "$output.tmp" "$input"
        case "$layout" in
            debuglink)
                directory="$(dirname "$output")/.debug"
                mkdir -p "$directory"
                debug="$directory/$(basename "$output").debug"
                mv "$output.debug.tmp" "$debug"
                objcopy "--add-gnu-debuglink=$debug" "$output.tmp"
                ;;
            build-id)
                id=$(readelf -n "$input" | awk "/Build ID:/ { print \$3 }")
                [[ -n "$id" ]] || { echo "$input has no build-id" >&2; exit 1; }
                # A rebuild changes the build-id; drop the last one'"'"'s file.
                rm -rf "$root/.build-id"
                mkdir -p "$root/.build-id/${id:0:2}"
                mv "$output.debug.tmp" "$root/.build-id/${id:0:2}/${id:2}.debug"
                ;;
        esac
        mv "$output.tmp" "$output"
    '
    run_cached_build "$input" "$output" \
        "derivation=split-debug-v2"$'\n'"layout=${layout}"$'\n'"root=${root}" \
        bash -c "$script" _ "$input" "$output" "$layout" "$root"
}



build_shapes_fixture() {
    local compiler="$1"
    local directory="$2"
    shift 2
    local source_dir="$cpp_fixtures_dir/shapes"
    local -a common=("$compiler" -std=c++20 -Wall -Wextra -Werror -g "$@" -Wl,--build-id)
    read_dash_version "$compiler"
    local signature="compiler=${dash_version}"$'\n'"target=x86_64-linux"$'\n'"backend=${compiler}"
    mkdir -p "$directory/plain"
    run_cached_build "$source_dir" "$directory/plain/libshapes.so" "$signature" \
        "${common[@]}" -shared -fPIC -Wl,-soname,libshapes.so "$source_dir/library.cpp" \
        "$source_dir/geometry.cpp" -o "$directory/plain/libshapes.so"
    run_cached_build "$source_dir" "$directory/plain/shapes" "$signature" \
        "${common[@]}" "$source_dir/main.cpp" "$source_dir/geometry.cpp" \
        "$directory/plain/libshapes.so" \
        '-Wl,-rpath,$ORIGIN' -o "$directory/plain/shapes"
}









derive_dwz() {
    wait_builds
    local directory="$1"
    local layout="$2"
    local script
    # shellcheck disable=SC2016
    script='
        set -euo pipefail
        directory="$1"; layout="$2"
        plain="$directory/plain"
        debug=()
        standard=()
        case "$layout" in
            whole|dwarf5)
                output="$directory/dwz"
                if [[ "$layout" == dwarf5 ]]; then
                    output="$directory/dwarf5"
                    standard=(-5)
                fi
                rm -rf "$output"
                mkdir -p "$output/.dwz"
                for name in shapes libshapes.so; do
                    cp "$plain/$name" "$output/$name.tmp"
                    debug+=("$output/$name.tmp")
                done
                common="$output/.dwz/shapes"
                ;;
            split)
                output="$directory/split"
                root="$directory/debug-root"
                rm -rf "$output" "$root"
                mkdir -p "$output" "$root/.dwz"
                for name in shapes libshapes.so; do
                    id=$(readelf -n "$plain/$name" 2>/dev/null | awk "/Build ID:/ { print \$3 }")
                    [[ -n "$id" ]] || { echo "$plain/$name has no build-id" >&2; exit 1; }
                    mkdir -p "$root/.build-id/${id:0:2}"
                    file="$root/.build-id/${id:0:2}/${id:2}.debug"
                    objcopy --only-keep-debug "$plain/$name" "$file"
                    strip --strip-all -o "$output/$name.tmp" "$plain/$name"
                    debug+=("$file")
                done
                common="$root/.dwz/shapes"
                ;;
        esac
        dwz "${standard[@]}" -m "$common" -r "${debug[@]}"
        link=.gnu_debugaltlink
        [[ "$layout" == dwarf5 ]] && link=.debug_sup
        for file in "${debug[@]}"; do
            readelf -S "$file" | grep -F " $link " >/dev/null \
                || { echo "dwz left $file sharing nothing" >&2; exit 1; }
        done
        for name in libshapes.so shapes; do
            mv "$output/$name.tmp" "$output/$name"
        done
    '
    local output="$directory/dwz/shapes"
    [[ "$layout" == split ]] && output="$directory/split/shapes"
    [[ "$layout" == dwarf5 ]] && output="$directory/dwarf5/shapes"
    local version


    version=$(dwz --version | sed -n 1p)
    run_cached_build "$directory/plain" "$output" \
        "derivation=dwz-v1"$'\n'"layout=${layout}"$'\n'"dwz=${version}" \
        bash -c "$script" _ "$directory" "$layout"
}




require_symbols_layout() {
    wait_for "$1"
    local library="$1"
    local tables="$2"
    local sections symbols
    sections=$(readelf -SW "$library")
    symbols=$(readelf -sW "$library")
    fail() {
        printf 'error: %s: %s\n' "$library" "$1" >&2
        exit 1
    }
    if grep -F .debug_info <<<"$sections" >/dev/null; then
        fail "has DWARF debug information"
    fi
    local has_symtab=no has_embedded=no
    grep -F ' .symtab ' <<<"$sections" >/dev/null && has_symtab=yes
    grep -F ' .gnu_debugdata ' <<<"$sections" >/dev/null && has_embedded=yes
    case "$tables" in
        full) [[ $has_symtab == yes && $has_embedded == no ]] || fail "lacks a static symbol table" ;;
        dynamic) [[ $has_symtab == no && $has_embedded == no ]] || fail "is not stripped" ;;
        embedded) [[ $has_symtab == no && $has_embedded == yes ]] || fail "lacks MiniDebugInfo" ;;
    esac
    local layout_symbols="$symbols"
    if [[ "$tables" == embedded ]]; then
        layout_symbols=$(readelf -sW "$library.embedded")
        if ! grep -E ' FUNC +LOCAL .* lib_static_helper$' <<<"$layout_symbols" >/dev/null; then
            fail "MiniDebugInfo lacks lib_static_helper"
        fi
        if grep -E ' asm_sized$' <<<"$layout_symbols" >/dev/null; then
            fail "MiniDebugInfo repeats exported symbols"
        fi
    fi
    if [[ "$tables" == dynamic || "$tables" == embedded ]] \
        && grep -F lib_static_helper <<<"$symbols" >/dev/null; then
        fail "still names lib_static_helper"
    fi



    awk '
        $8 == "asm_noreturn_caller" { caller_end = strtonum("0x" $2) + $3 }
        $8 == "asm_after_noreturn" { after = strtonum("0x" $2) }
        $8 == "asm_nested_outer" { outer = strtonum("0x" $2); outer_end = outer + $3 }
        $8 == "asm_nested_inner" { inner = strtonum("0x" $2); inner_end = inner + $3 }
        END {
            exit !(caller_end == after && outer < inner && inner_end < outer_end)
        }' <<<"$symbols" || fail "asm layout changed"
}

build_rust_fixture() {
    local source="$1"
    local output="$2"
    shift 2

    build_program rustc "$source" "$output" \
        --edition=2024 -D warnings -C debuginfo=2 -C codegen-units=1 -C panic=abort \
        -C link-arg=-lc "$@"
}


read_go_version() {
    if [[ -z "$go_version" ]]; then
        go_version=$(go version)
        go_target=$(go env GOOS GOARCH)
        go_target=${go_target//$'\n'//}
    fi
}



build_go_fixture() {
    local package_dir="$1"
    local output="$2"
    shift 2
    local -a sources=()
    mapfile -d '' sources < <(
        find "$package_dir" -maxdepth 1 -type f -name '*.go' -print0 | sort -z
    )
    if (( ${#sources[@]} == 0 )); then
        printf 'error: Go fixture has no source files: %s\n' "$package_dir" >&2
        exit 1
    fi
    local -a build=(build -buildvcs=false "$@" -o "$output" "${sources[@]}")


    if [[ -f "$package_dir/go.mod" ]]; then
        build=(-C "$package_dir" build -buildvcs=false "$@" -o "$PWD/$output" .)
    fi
    local -a command=(
        env "CGO_ENABLED=${GO_CGO:-0}" ${GO_CC:+"CC=$GO_CC"} ${GO_CFLAGS:+"CGO_CFLAGS=$GO_CFLAGS"}
        go "${build[@]}"
    )
    read_go_version
    run_cached_build "$package_dir" "$output" \
        "compiler=${go_version}"$'\n'"target=${go_target}"$'\n'"backend=gc" \
        "${command[@]}"
}


build_go_command() {
    local package="$1"
    local output="$2"
    shift 2
    local -a command=(
        env CGO_ENABLED=0 go build -buildvcs=false "$@" -o "$output" "$package"
    )
    read_go_version
    run_cached_build "$(go env GOROOT)/src/$package" "$output" \
        "compiler=${go_version}"$'\n'"target=${go_target}"$'\n'"backend=gc" \
        "${command[@]}"
}

build_zig_fixture() {
    local source="$1"
    local output="$2"
    shift 2
    local -a command=(
        zig build-exe "$source" -target x86_64-linux-gnu -fllvm -fno-strip
        -funwind-tables "$@" "-femit-bin=${output}"
    )
    if [[ -z "$zig_version" ]]; then
        zig_version=$(zig version)
    fi
    run_cached_build "$source" "$output" \
        "compiler=zig ${zig_version}"$'\n'"target=x86_64-linux-gnu"$'\n'"backend=llvm" \
        "${command[@]}"
}





build_zig_self_hosted_fixture() {
    local source="$1"
    local output="$2"
    shift 2
    local -a command=(
        zig build-exe "$source" -target x86_64-linux-gnu -fno-llvm -fno-strip -j1
        --global-cache-dir "$output_dir/zig-self-hosted-cache" "$@"
        "-femit-bin=${output}"
    )
    if [[ -z "$zig_version" ]]; then
        zig_version=$(zig version)
    fi
    run_cached_build "$source" "$output" \
        "compiler=zig ${zig_version}"$'\n'"target=x86_64-linux-gnu"$'\n'"backend=self-hosted" \
        "${command[@]}"
}


build_odin_fixture() {
    local source="$1"
    local output="$2"
    shift 2
    local -a command=(
        odin build "$source" -file -debug -vet -strict-style "$@" "-out:${output}"
    )
    if [[ -z "$odin_version" ]]; then
        odin_version=$(odin version)
    fi
    run_cached_build "$source" "$output" \
        "compiler=${odin_version}"$'\n'"target=x86_64-linux"$'\n'"backend=llvm" \
        "${command[@]}"
}


build_fortran_fixture() {
    local source="$1"
    local output="$2"
    shift 2
    local -a command=(
        env -u NIX_CFLAGS_COMPILE -u NIX_CFLAGS_COMPILE_FOR_BUILD
        gfortran -g -Wall -Werror -Wno-maybe-uninitialized -J "${output}.modules" "$@" -o "$output" "$source"
    )
    if [[ -z "$fortran_version" ]]; then
        fortran_version=$(gfortran -dumpfullversion)
    fi
    mkdir -p "${output}.modules"
    run_cached_build "$source" "$output" \
        "compiler=gfortran ${fortran_version}"$'\n'"target=x86_64-linux" \
        "${command[@]}"
}


build_d_fixture() {
    local source="$1"
    local output="$2"
    shift 2
    local -a command=(
        ldc2 -g -w "$@" "-od=${output}.objects" "-of=${output}" "$source"
    )
    if [[ -z "$d_version" ]]; then
        d_version=$(ldc2 --version | sed -n 1p)
    fi
    run_cached_build "$source" "$output" \
        "compiler=${d_version}"$'\n'"target=x86_64-linux" \
        "${command[@]}"
}



build_nim_fixture() {
    local source="$1"
    local output="$2"
    shift 2
    local -a command=(
        nim c --skipUserCfg --skipParentCfg --hints:off --warningAsError:on --debugger:native
        "$@" "--nimcache:${output}.nimcache" "--out:${output}" "$source"
    )
    if [[ -z "$nim_version" ]]; then
        nim_version=$(nim --version | sed -n 1p)
    fi
    run_cached_build "$source" "$output" \
        "compiler=${nim_version}"$'\n'"target=x86_64-linux" \
        "${command[@]}"
}



build_ada_fixture() {
    local source="$1"
    local output="$2"
    shift 2

    local -a command=(
        env -u NIX_CFLAGS_COMPILE -u NIX_CFLAGS_COMPILE_FOR_BUILD -C "${output}.objects"
        gnatmake -q -g -gnat2022 -gnatwa -gnatwe "$@" -o "${PWD}/${output}" "${PWD}/${source}"
    )
    if [[ -z "$ada_version" ]]; then
        ada_version=$(gnatmake --version | sed -n 1p)
    fi
    mkdir -p "${output}.objects"
    run_cached_build "$source" "$output" \
        "compiler=${ada_version}"$'\n'"target=x86_64-linux" \
        "${command[@]}"
}

validation_is_cached() {
    wait_for "$1"
    local output="$1"
    local stamp="$2"
    local signature="$3"
    local previous=""

    if [[ -f "$stamp" ]]; then
        previous=$(<"$stamp")
    fi
    [[ "${rebuilt_outputs[$output]:-true}" == false && "$previous" == "$signature" ]]
}

record_validation() {
    local stamp="$1"
    local signature="$2"
    printf '%s\n' "$signature" >"${stamp}.tmp"
    mv "${stamp}.tmp" "$stamp"
}



require_tail_jump() {
    wait_for "$1"
    local output="$1"
    local caller="$2"
    local callee="$3"
    local stamp="${output}.validation-tail-${caller}-${callee}"
    local signature="validator=tail-jump-v1"$'\n'"caller=${caller}"$'\n'"callee=${callee}"
    if validation_is_cached "$output" "$stamp" "$signature"; then
        return
    fi


    if ! objdump -d --no-show-raw-insn "$output" \
        | sed -n "/<${caller}>:/,/^\$/p" \
        | grep -E "[[:space:]]jmp[[:space:]]+[^<]*<${callee}>" >/dev/null; then
        printf 'error: %s does not tail-jump from %s to %s\n' \
            "$output" "$caller" "$callee" >&2
        exit 1
    fi
    record_validation "$stamp" "$signature"
}



require_instruction() {
    wait_for "$1"
    local output="$1"
    local function="$2"
    local pattern="$3"
    local key=${pattern//[^a-zA-Z0-9]/_}
    local stamp="${output}.validation-instruction-${function}-${key}"
    local signature="validator=instruction-v1"$'\n'"function=${function}"$'\n'"pattern=${pattern}"
    if validation_is_cached "$output" "$stamp" "$signature"; then
        return
    fi

    if ! objdump -d --no-show-raw-insn "$output" \
        | sed -n "/<${function}>:/,/^\$/p" \
        | grep -E "$pattern" >/dev/null; then
        printf 'error: %s function %s does not contain %s\n' \
            "$output" "$function" "$pattern" >&2
        exit 1
    fi
    record_validation "$stamp" "$signature"
}




require_musl() {
    wait_for "$1"
    local output="$1"
    local stamp="${output}.validation-musl"
    local signature="validator=musl-v1"
    if validation_is_cached "$output" "$stamp" "$signature"; then
        return
    fi
    local interpreter
    interpreter=$(readelf -lW "$output" \
        | sed -n 's/.*Requesting program interpreter: \(.*\)]$/\1/p')
    if [[ -n "$interpreter" ]]; then
        if [[ "${interpreter##*/}" != ld-musl-* ]]; then
            printf 'error: %s runs on %s, not musl\n' "$output" "$interpreter" >&2
            exit 1
        fi
    elif ! nm "$output" | grep -E ' __copy_tls$' >/dev/null; then
        printf 'error: %s is not statically linked with musl\n' "$output" >&2
        exit 1
    fi
    record_validation "$stamp" "$signature"
}





require_static_glibc() {
    wait_for "$1"
    local output="$1"
    local threads="$2"
    local stamp="${output}.validation-static-glibc"
    local signature="validator=static-glibc-v1"$'\n'"threads=${threads}"
    if validation_is_cached "$output" "$stamp" "$signature"; then
        return
    fi
    local symbols
    symbols=$(nm "$output")
    if readelf -lW "$output" | grep -F 'Requesting program interpreter' >/dev/null ||
        ! grep -E ' __libc_setup_tls$' <<<"$symbols" >/dev/null; then
        printf 'error: %s is not statically linked with glibc\n' "$output" >&2
        exit 1
    fi
    if grep -E ' __nptl_version$' <<<"$symbols" >/dev/null; then
        local linked=yes
    else
        local linked=no
    fi
    if [[ $linked != "$threads" ]]; then
        printf 'error: %s contains the thread library: %s, expected %s\n' \
            "$output" "$linked" "$threads" >&2
        exit 1
    fi
    record_validation "$stamp" "$signature"
}





require_ubuntu_glibc() {
    wait_for "$1"
    local output="$1"
    local version="$2"
    local pthread="$3"
    local stamp="${output}.validation-ubuntu-glibc"
    local libraries
    libraries=$(realpath "$MICROSCOPE_UBUNTU_GLIBC/$version/lib")
    local signature="validator=ubuntu-glibc-v1"$'\n'"libraries=${libraries}"$'\n'"pthread=${pthread}"
    if validation_is_cached "$output" "$stamp" "$signature"; then
        return
    fi
    local loaded path outputs
    loaded=$(LD_TRACE_LOADED_OBJECTS=1 "$output")
    outputs=$(realpath "$output_dir")

    while read -r path; do
        if [[ "$path" != "$libraries"/* && "$path" != "$outputs"/* ]]; then
            printf 'error: %s loads %s, not Ubuntu glibc %s\n' "$output" "$path" "$version" >&2
            exit 1
        fi
    done < <(awk '$1 != "linux-vdso.so.1" { print ($2 == "=>" ? $3 : $1) }' <<<"$loaded")
    if grep -E '^\s*libpthread\.so\.0 => ' <<<"$loaded" >/dev/null; then
        local linked=yes
    else
        local linked=no
    fi
    if [[ $linked != "$pthread" ]]; then
        printf 'error: %s loads libpthread: %s, expected %s\n' \
            "$output" "$linked" "$pthread" >&2
        exit 1
    fi
    record_validation "$stamp" "$signature"
}




require_no_code() {
    wait_for "$1"
    local output="$1"
    local stamp="${output}.validation-no-code"
    local signature="validator=no-code-v1"
    if validation_is_cached "$output" "$stamp" "$signature"; then
        return
    fi
    if readelf -lW "$output" | awk '$1 == "LOAD" && $(NF - 1) ~ /E/ { found = 1 } END { exit !found }'; then
        printf 'error: %s has an executable segment\n' "$output" >&2
        exit 1
    fi
    record_validation "$stamp" "$signature"
}

require_dwarf_operation() {
    wait_for "$1"
    local output="$1"
    local operation="$2"
    local key=${operation//[^a-zA-Z0-9]/_}
    local stamp="${output}.validation-dwarf-${key}"
    local signature="validator=dwarf-operation-v1"$'\n'"operation=${operation}"
    if validation_is_cached "$output" "$stamp" "$signature"; then
        return
    fi


    if ! objdump --dwarf=info,loc "$output" | grep "$operation" >/dev/null; then
        printf 'error: %s does not exercise %s\n' "$output" "$operation" >&2
        exit 1
    fi
    record_validation "$stamp" "$signature"
}






core_signature() {
    local signal="$1"
    local filter="$2"
    local inputs="$3"
    shift 3
    if [[ -z "$gdb_version" ]]; then
        gdb_version=$(gdb --version)
        gdb_version=${gdb_version%%$'\n'*}
    fi
    local -a input_paths
    read -r -a input_paths <<<"$inputs"
    local command_text
    printf -v command_text '%q ' "$@"
    printf '%s\ngenerator=gcore-v2\ngdb=%s\nsignal=%s\nfilter=%s\ncommand=%s\n' \
        "$checkout" "$gdb_version" "$signal" "$filter" "$command_text"
    stat -L --format='%n %Y %s' "${input_paths[@]}" "$stack_guard_mappings"
}

core_is_current() {
    wait_builds
    local core="$1"
    local signature="$2"
    [[ -s "$core" && -f "${core}.command" && "$(<"${core}.command")" == "$signature" ]]
}

generate_core() {
    wait_builds
    local core="$1"
    local signal="$2"
    local filter="$3"
    local inputs="$4"
    shift 4
    local signature
    signature=$(core_signature "$signal" "$filter" "$inputs" "$@")
    wait_for "$core"
    if core_is_current "$core" "$signature"; then
        rebuilt_outputs["$core"]=false
        printf '[cached] %s\n' "$core"
        return
    fi

    printf '[core]   %s\n' "$core"
    rebuilt_outputs["$core"]=true
    spawn_job leaf "$core" make_core "$core" "$signature" "$signal" "$filter" "$@"
}



make_core() {
    local core="$1"
    local signature="$2"
    local signal="$3"
    local filter="$4"
    shift 4
    local stamp="${core}.command"
    local temporary="${core}.tmp"
    rm -f "$temporary"
    local log



    log=$(bash -c 'printf "%s\n" "$1" >/proc/self/coredump_filter && shift && exec "$@"' \
        _ "$filter" \
        setarch "$(uname -m)" \
        gdb -nx -batch -q \
        -iex 'set auto-load off' \
        -iex 'set debuginfod enabled off' \
        -ex 'set disable-randomization off' \
        -ex 'set pagination off' \
        -ex 'set confirm off' \
        -ex "set exec-wrapper ${stack_guard_mappings}" \
        -ex 'run' \
        -ex 'printf "microscope-signal=%d\n", $_siginfo.si_signo' \
        -ex "gcore ${temporary}" \
        -ex 'kill' \
        --args "$@" 2>&1) || true
    if ! grep -Fx "microscope-signal=${signal}" <<<"$log" >/dev/null; then
        printf 'error: %s did not stop with signal %s for a core dump:\n%s\n' \
            "$1" "$signal" "$log" >&2
        rm -f "$temporary"
        exit 1
    fi


    if ! grep -Fx "Saved corefile ${temporary}" <<<"$log" >/dev/null || [[ ! -s "$temporary" ]]; then
        printf 'error: gcore did not write %s:\n%s\n' "$temporary" "$log" >&2
        rm -f "$temporary"
        exit 1
    fi



    if grep -F 'Memory read failed for corefile section' <<<"$log" |
        grep -Fv ' at 0xffffffffff600000.' >/dev/null; then
        printf 'error: gcore could not read all of %s:\n%s\n' "$temporary" "$log" >&2
        rm -f "$temporary"
        exit 1
    fi
    mv "$temporary" "$core"
    printf '%s\n' "$log" >"${core}.log"
    printf '%s\n' "$signature" >"${stamp}.tmp"
    mv "${stamp}.tmp" "$stamp"
}



suite_signature() {
    local -a paths=()
    local tool path
    for tool in gcc g++ clang clang++ clang++-libc++ musl-gcc musl-clang rustc cargo go zig odin gfortran ldc2 nim gnatmake \
        objdump gdb setarch dwz; do
        if path=$(type -P "$tool"); then
            paths+=("$path")
        fi
    done


    printf '%s\nsuite-v1\nGOOS=%s GOARCH=%s\nGLIBC_STATIC_LIBRARIES=%s\nMICROSCOPE_FIXTURE_CRATES=%s\nMICROSCOPE_NODE=%s\n' \
        "$checkout" "${GOOS-}" "${GOARCH-}" "$GLIBC_STATIC_LIBRARIES" "${MICROSCOPE_FIXTURE_CRATES-}" \
        "${MICROSCOPE_NODE-}"
    stat -L --format='%n %Y' "${paths[@]}"
}



suite_is_current() {
    local signature="$1"
    [[ -f "$suite_stamp" && -f "$suite_outputs" ]] || return 1
    [[ "$(<"$suite_stamp")" == "$signature" ]] || return 1
    [[ -z "$(find "$fixtures_dir" sdk views/kernels scripts/gosym-oracle \
        scripts/coroutine-oracle scripts/stack-guard-mappings.c "${BASH_SOURCE[0]}" \
        "$frame_oracle_script" -newer "$suite_stamp" -print -quit)" ]] \
        || return 1
    local output
    while IFS= read -r output; do
        [[ -e "$output" ]] || return 1
    done <"$suite_outputs"
}





readonly default_core_filter=0x33
readonly headerless_core_filter=0x23
readonly headers_only_core_filter=0x10
readonly memoryless_core_filter=0x0








readonly tokio_fixtures_dir="${rust_fixtures_dir}/tokio"
readonly tokio_target_dir="build/tokio-target"
readonly tokio_outputs="${output_dir}/.tokio.outputs"


readonly tokio_older_release="1.51"
readonly tokio_older_lock="${tokio_fixtures_dir}/tokio-${tokio_older_release}/Cargo.lock"



compile_tokio_variant() {
    local variant="$1"
    local profile="$2"
    local flags="$3"
    local features="$4"
    shift 4
    local -a packages=()
    local package
    for package in "$@"; do
        packages+=(--package "$package")
    done
    if [[ -n "$features" ]]; then
        packages+=(--features "$features")
    fi
    if [[ "$variant" == "${tokio_older_release}-"* ]]; then
        packages+=(--config "resolver.lockfile-path='${PWD}/${tokio_older_lock}'")
    fi
    local -a cargo=(
        env CARGO_TARGET_DIR="$tokio_target_dir/$variant"
        RUSTFLAGS="${RUSTFLAGS-} -D warnings ${flags}" NIX_HARDENING_ENABLE=
        cargo
    )
    local -a workspace=(
        --quiet --offline --locked --manifest-path "$tokio_fixtures_dir/Cargo.toml"
        --profile "$profile"
        --config "source.crates-io.replace-with='vendored'"
        --config "source.vendored.directory='${MICROSCOPE_FIXTURE_CRATES}'"
    )



    local stamp="$tokio_target_dir/$variant/.checkout"
    if [[ -f "$stamp" && "$(<"$stamp")" != "$checkout" ]]; then
        local -a fixtures=()
        for package in "$@"; do
            fixtures+=(--package "$package")
        done
        "${cargo[@]}" clean "${workspace[@]}" "${fixtures[@]}"
    fi
    printf '[cargo]  tokio fixtures (%s)\n' "$variant"
    "${cargo[@]}" build "${workspace[@]}" "${packages[@]}"
    printf '%s\n' "$checkout" >"${stamp}.tmp"
    mv "${stamp}.tmp" "$stamp"
}



copy_tokio_variant() {
    local variant="$1"
    local profile="$2"
    shift 2
    local directory="$profile"
    [[ "$profile" == dev ]] && directory=debug
    local package
    for package in "$@"; do
        local built="$tokio_target_dir/$variant/$directory/$package"
        local output="$output_dir/tokio-${package}-${variant}"
        if [[ -x "$output" ]] && ! [[ "$built" -nt "$output" ]]; then
            rebuilt_outputs["$output"]=false
            continue
        fi
        cp -p "$built" "$output"
        rebuilt_outputs["$output"]=true
    done

    [[ "$variant" == o0 || "$variant" == o3 ]] || return 0
    for package in "$@"; do
        generate_coroutine_oracle "$output_dir/tokio-${package}-${variant}"
    done
}




readonly coroutine_reducer="$output_dir/coroutine-oracle"
generate_coroutine_oracle() {
    local program="$1"
    local oracle="${program}.coroutines"
    local reducer="$coroutine_reducer"
    if [[ -s "$oracle" && "$oracle" -nt "$program" && "$oracle" -nt "$reducer" ]]; then
        printf '[cached] %s\n' "$oracle"
        rebuilt_outputs["$oracle"]=false
        return
    fi
    printf '[oracle] %s\n' "$oracle"
    rebuilt_outputs["$oracle"]=true

    spawn_job leaf "$oracle" reduce_coroutines "$program" "$oracle" "$reducer"
}


reduce_coroutines() {
    local program="$1"
    local oracle="$2"
    local reducer="$3"
    local dump="${oracle}.info"
    readelf --debug-dump=info "$program" >"$dump" 2>/dev/null
    "$reducer" "$dump" | LC_ALL=C sort -u >"${oracle}.tmp"
    rm -f "$dump"
    mv "${oracle}.tmp" "$oracle"
}




build_tokio_fixtures() {
    if [[ -z "${MICROSCOPE_FIXTURE_CRATES-}" ]]; then
        printf 'error: MICROSCOPE_FIXTURE_CRATES is unset; build inside the Nix shell\n' >&2
        exit 1
    fi
    build_go_fixture scripts/coroutine-oracle "$coroutine_reducer"


    local -a variants=()
    local -a builds=()
    variant() {
        compile_tokio_variant "$@" &
        builds+=($!)
        local name="$1" profile="$2"
        shift 4
        variants+=("$name $profile $*")
    }




    local -a fixtures=(std-async panics workers server drivers steps cancel shapes values
        runtimes blocking migrate deadlock)
    variant o0 dev "" "" "${fixtures[@]}" scale corrupt profile
    variant o3 release "" "" "${fixtures[@]}" profile


    variant "${tokio_older_release}-o0" dev "" "" "${fixtures[@]}"
    variant "${tokio_older_release}-o3" release "" "" "${fixtures[@]}"

    variant abort abort "" "" panics



    variant lines lines "" "" workers
    variant stripped stripped "" "" workers
    variant remapped dev \
        "--remap-path-prefix=${MICROSCOPE_FIXTURE_CRATES}/tokio-1.53.2=/vendor/tokio" "" workers


    variant unstable dev "--cfg tokio_unstable" "" workers


    variant traced dev "--cfg tokio_unstable" workers/tracing workers


    variant parking-lot dev "" workers/parking-lot workers

    variant wrapped dev "" workers/wrapped workers

    variant legacy dev "-Z unstable-options -C symbol-mangling-version=legacy" "" panics workers
    local build failed=0
    for build in "${builds[@]}"; do
        wait "$build" || failed=1
    done
    if (( failed )); then
        printf 'error: could not build the tokio fixtures\n' >&2
        exit 1
    fi
    local record
    for record in "${variants[@]}"; do
        # shellcheck disable=SC2086 # Each record is words.
        copy_tokio_variant $record
    done






    export TRUTH_CORE=1 MALLOC_ARENA_MAX=1
    local name
    for name in o0 o3 "${tokio_older_release}-o0" "${tokio_older_release}-o3"; do
        local program="$output_dir/tokio-workers-${name}"
        generate_core "${program}.core" 5 "$default_core_filter" "$program" "$program"
        generate_core "${program}-current.core" 5 "$default_core_filter" "$program" "$program" current
        program="$output_dir/tokio-values-${name}"
        generate_core "${program}.core" 5 "$default_core_filter" "$program" "$program"
        program="$output_dir/tokio-runtimes-${name}"
        generate_core "${program}.core" 5 "$default_core_filter" "$program" "$program"
    done
    wait_jobs
    printf '%s\n' "${!rebuilt_outputs[@]}" >"${tokio_outputs}.tmp"
    mv "${tokio_outputs}.tmp" "$tokio_outputs"
}

mkdir -p "$output_dir"


readonly stack_guard_mappings="$output_dir/stack-guard-mappings"


if (( $# > 0 )); then
    if [[ "$*" != tokio ]]; then
        printf 'usage: %s [tokio]\n' "$0" >&2
        exit 2
    fi
    build_fixture gcc scripts/stack-guard-mappings.c "$stack_guard_mappings" -O2
    wait_builds
    build_tokio_fixtures
    exit 0
fi
signature=$(suite_signature)
if suite_is_current "$signature"; then
    printf '[cached] %s\n' "$output_dir"
    exit 0
fi


printf '%s\n' "$signature" >"${suite_stamp}.tmp"
build_fixture gcc scripts/stack-guard-mappings.c "$stack_guard_mappings" -O2
wait_builds

build_tokio_fixtures &
readonly tokio_build=$!

build_fixture gcc "$c_fixtures_dir/basic.c" "$output_dir/basic" \
    -O0 -g3 -fPIE -pie


build_fixture gcc "$c_fixtures_dir/basic.c" "$output_dir/basic-relocated" \
    -O0 -g3 -fPIE -pie "-ffile-prefix-map=${PWD}=/nonexistent/microscope"


build_fixture gcc "$c_fixtures_dir/basic.c" "$output_dir/basic-split-dwarf" \
    -O0 -g3 -gsplit-dwarf -fPIE -pie

build_fixture gcc "$c_fixtures_dir/basic.c" "$output_dir/basic.o" -O0 -g3 -c
build_fixture gcc "$c_fixtures_dir/attach.c" "$output_dir/attach" \
    -O0 -g3 -fPIE -pie
build_fixture gcc "$c_fixtures_dir/signal-query-filter.c" "$output_dir/signal-query-filter" \
    -O0 -g3 -fPIE -pie
for version_node in "12.4 22" "13.6 24" "14.6 26"; do
    read -r version node <<<"$version_node"
    for compiler in g++ clang++; do
        build_cpp_fixture "$compiler" "$cpp_fixtures_dir/inspector-string-view.cpp" \
            "$output_dir/inspector-string-view-$version-$compiler" -O0 \
            -isystem "$MICROSCOPE_V8_TORQUE/$version/v8/include" \
            -isystem "$MICROSCOPE_NODE/$node/include"
    done
done
build_fixture gcc "$c_fixtures_dir/attach-threads.c" "$output_dir/attach-threads" \
    -O0 -g3 -fPIE -pie -pthread
build_fixture musl-gcc "$c_fixtures_dir/attach-threads.c" "$output_dir/attach-threads-musl" \
    -O0 -g3 -fPIE -pie -pthread
require_musl "$output_dir/attach-threads-musl"
build_fixture gcc "$c_fixtures_dir/attach-sleepers.c" "$output_dir/attach-sleepers" \
    -O0 -g3 -fPIE -pie -pthread
build_fixture gcc "$c_fixtures_dir/attach-clones.c" "$output_dir/attach-clones" \
    -O0 -g3 -fPIE -pie -pthread
build_fixture gcc "$c_fixtures_dir/attach-exited-leader.c" "$output_dir/attach-exited-leader" \
    -O0 -g3 -fPIE -pie -pthread
build_fixture gcc "$c_fixtures_dir/attach-restart.c" "$output_dir/attach-restart" \
    -O0 -g3 -fPIE -pie
build_fixture gcc "$c_fixtures_dir/attach-leader-exits.c" "$output_dir/attach-leader-exits" \
    -O0 -g3 -fPIE -pie -pthread
build_fixture gcc "$c_fixtures_dir/syscall-sweep.c" "$output_dir/syscall-sweep" -O1 -g
build_fixture gcc "$c_fixtures_dir/syscall-burst.c" "$output_dir/syscall-burst" -O2 -g
build_fixture gcc "$c_fixtures_dir/syscall-load.c" "$output_dir/syscall-load" \
    -O1 -g -fno-omit-frame-pointer -pthread
build_fixture gcc "$c_fixtures_dir/syscall-exceptions.c" "$output_dir/syscall-exceptions" \
    -O1 -g -fno-omit-frame-pointer
build_fixture gcc "$c_fixtures_dir/profile-calls.c" "$output_dir/profile-calls" \
    -O2 -g -fno-omit-frame-pointer
build_fixture gcc "$c_fixtures_dir/profile-kernel.c" "$output_dir/profile-kernel" \
    -O2 -g -fno-omit-frame-pointer
build_fixture gcc "$c_fixtures_dir/profile-bigframe.c" "$output_dir/profile-bigframe" \
    -O2 -g -fomit-frame-pointer
build_fixture gcc "$c_fixtures_dir/profile-inline.c" "$output_dir/profile-inline" \
    -O2 -g -fno-omit-frame-pointer
build_fixture gcc "$c_fixtures_dir/profile-deep.c" "$output_dir/profile-deep" \
    -O2 -g -fno-omit-frame-pointer


build_fixture clang "$c_fixtures_dir/profile-inline.c" "$output_dir/profile-inline-clang-split" \
    -O2 -g -gsplit-dwarf -fdebug-compilation-dir=. -fno-omit-frame-pointer
for variant in "gcc-o0 gcc -O0 -fno-omit-frame-pointer -fPIE -pie" \
    "clang-o2-nopie clang -O2 -fomit-frame-pointer -no-pie"; do
    read -r name compiler flags <<<"$variant"
    # shellcheck disable=SC2086
    build_fixture "$compiler" "$c_fixtures_dir/catch.c" "$output_dir/catch-${name}" \
        -g3 -gdwarf-5 -pthread $flags
done
build_fixture gcc "$c_fixtures_dir/catch.c" "$output_dir/catch-spin" -O0 -g3 -DSPIN -pthread
build_fixture gcc "$c_fixtures_dir/syscalls.c" "$output_dir/syscalls" \
    -O0 -g3 -fPIE -pie -pthread


build_fixture gcc "$c_fixtures_dir/syscalls.c" "$output_dir/syscalls-nolines" \
    -O0 -fPIE -pie -pthread
build_fixture gcc "$c_fixtures_dir/attach-fork.c" "$output_dir/attach-fork" \
    -O0 -g3 -fPIE -pie
build_fixture gcc "$c_fixtures_dir/exited-leader.c" "$output_dir/exited-leader" \
    -O0 -g3 -fPIE -pie -pthread
build_c_fixture_directory gcc "$c_fixtures_dir/pointer-memory" \
    "$output_dir/pointer-memory-gcc-o0" \
    -O0 -g3 -gdwarf-5 -fno-omit-frame-pointer -fPIE -pie
build_fixture gcc "$c_fixtures_dir/variables.c" "$output_dir/variables-gcc-o0" \
    -O0 -g3 -gdwarf-5 -fno-omit-frame-pointer -fPIE -pie
build_fixture clang "$c_fixtures_dir/variables.c" "$output_dir/variables-clang-o0" \
    -O0 -g3 -gdwarf-5 -fno-omit-frame-pointer -fPIE -pie
build_fixture gcc "$c_fixtures_dir/variables.c" "$output_dir/variables-gcc-o2" \
    -O2 -g3 -gdwarf-5 -fno-omit-frame-pointer -fPIE -pie
require_dwarf_operation "$output_dir/variables-gcc-o2" DW_OP_implicit_pointer
require_dwarf_operation "$output_dir/variables-gcc-o2" 'DW_OP_implicit_pointer:.* 4'
build_fixture clang "$c_fixtures_dir/variables.c" "$output_dir/variables-clang-o2" \
    -O2 -g3 -gdwarf-5 -fno-omit-frame-pointer -fPIE -pie
build_fixture gcc "$c_fixtures_dir/variables.c" "$output_dir/variables-gcc-nopie" \
    -O0 -g3 -gdwarf-5 -fno-omit-frame-pointer -fno-pie -no-pie
build_fixture gcc "$c_fixtures_dir/command-names.c" "$output_dir/command-names" \
    -O0 -g3 -gdwarf-5 -fno-omit-frame-pointer -fPIE -pie
build_fixture gcc "$c_fixtures_dir/records.c" "$output_dir/records-c-gcc-o0" \
    -O0 -g3 -gdwarf-5 -fno-omit-frame-pointer -fPIE -pie
build_fixture clang "$c_fixtures_dir/records.c" "$output_dir/records-c-clang-o0" \
    -O0 -g3 -gdwarf-5 -fno-omit-frame-pointer -fPIE -pie
build_fixture gcc "$c_fixtures_dir/records.c" "$output_dir/records-c-gcc-o2" \
    -O2 -g3 -gdwarf-5 -fomit-frame-pointer -fPIE -pie
build_fixture clang "$c_fixtures_dir/records.c" "$output_dir/records-c-clang-o2" \
    -O2 -g3 -gdwarf-5 -fomit-frame-pointer -fPIE -pie
build_fixture gcc "$c_fixtures_dir/enums.c" "$output_dir/enums-c-gcc-o0" \
    -O0 -g3 -gdwarf-5 -fno-omit-frame-pointer -fPIE -pie
build_fixture clang "$c_fixtures_dir/enums.c" "$output_dir/enums-c-clang-o0" \
    -O0 -g3 -gdwarf-5 -fno-omit-frame-pointer -fPIE -pie
build_fixture gcc "$c_fixtures_dir/enums.c" "$output_dir/enums-c-gcc-o2" \
    -O2 -g3 -gdwarf-5 -fomit-frame-pointer -fPIE -pie
build_fixture clang "$c_fixtures_dir/enums.c" "$output_dir/enums-c-clang-o2" \
    -O2 -g3 -gdwarf-5 -fomit-frame-pointer -fPIE -pie
for variant in "gcc -O0" "gcc -O2" "clang -O2"; do
    read -r compiler level <<<"$variant"
    suffix="${level#-}"
    build_fixture "$compiler" "$c_fixtures_dir/realigned.c" \
        "$output_dir/realigned-${compiler}-${suffix,,}" "$level" -g3 -gdwarf-5 -fPIE -pie
done
for compiler in gcc clang; do
    build_fixture "$compiler" "$c_fixtures_dir/function-types.c" \
        "$output_dir/function-types-${compiler}-o0" -O0 -g3 -gdwarf-5 -fPIE -pie
done
build_fixture gcc "$c_fixtures_dir/types.c" "$output_dir/types-c-gcc-o0" \
    -O0 -g3 -gdwarf-5 -fno-omit-frame-pointer -fPIE -pie
build_fixture clang "$c_fixtures_dir/types.c" "$output_dir/types-c-clang-o0" \
    -O0 -g3 -gdwarf-5 -fno-omit-frame-pointer -fPIE -pie
build_fixture gcc "$c_fixtures_dir/variables-parameters.c" "$output_dir/variables-parameters-gcc-o0" \
    -O0 -g3 -gdwarf-5 -fno-omit-frame-pointer -fPIE -pie
build_fixture clang "$c_fixtures_dir/variables-parameters.c" "$output_dir/variables-parameters-clang-o0" \
    -O0 -g3 -gdwarf-5 -fno-omit-frame-pointer -fPIE -pie
build_fixture gcc "$c_fixtures_dir/variables-parameters.c" "$output_dir/variables-parameters-gcc-o2" \
    -O2 -g3 -gdwarf-5 -fomit-frame-pointer -fPIE -pie
build_fixture clang "$c_fixtures_dir/variables-parameters.c" "$output_dir/variables-parameters-clang-o2" \
    -O2 -g3 -gdwarf-5 -fomit-frame-pointer -fPIE -pie
for variant in "gcc -O0" "gcc -O2" "clang -O0" "clang -O2"; do
    read -r compiler level <<<"$variant"
    suffix="${level#-}"
    build_fixture "$compiler" "$c_fixtures_dir/returns.c" \
        "$output_dir/returns-c-${compiler}-${suffix,,}" "$level" -g3 -gdwarf-5 -fPIE -pie
done
build_fixture gcc "$c_fixtures_dir/pieces.c" "$output_dir/pieces-gcc-o0" \
    -O0 -g3 -gdwarf-5 -fno-omit-frame-pointer -fPIE -pie
build_fixture gcc "$c_fixtures_dir/pieces.c" "$output_dir/pieces-gcc-o2" \
    -O2 -g3 -gdwarf-5 -fno-omit-frame-pointer -fPIE -pie
require_dwarf_operation "$output_dir/pieces-gcc-o2" 'DW_OP_implicit_value.*DW_OP_piece'
require_dwarf_operation "$output_dir/pieces-gcc-o2" 'DW_OP_piece: 8; DW_OP_piece: 8'
build_fixture clang "$c_fixtures_dir/pieces.c" "$output_dir/pieces-clang-o2" \
    -O2 -g3 -gdwarf-5 -fno-omit-frame-pointer -fPIE -pie
require_dwarf_operation "$output_dir/pieces-clang-o2" 'DW_OP_reg14 (r14); DW_OP_piece'
build_fixture gcc "$c_fixtures_dir/variables-static.c" "$output_dir/variables-static-gcc-o2" \
    -O2 -g3 -gdwarf-5 -fomit-frame-pointer -fPIE -pie
build_fixture clang "$c_fixtures_dir/variables-static.c" "$output_dir/variables-static-clang-o2" \
    -O2 -g3 -gdwarf-5 -fomit-frame-pointer -fPIE -pie
require_dwarf_operation "$output_dir/variables-static-clang-o2" DW_OP_addrx
build_fixture gcc "$c_fixtures_dir/variables-static.c" "$output_dir/variables-static-gcc-nopie" \
    -O2 -g3 -gdwarf-5 -fomit-frame-pointer -no-pie
require_dwarf_operation "$output_dir/variables-static-gcc-nopie" 'DW_OP_addr:'

for build in gcc-o0-pie gcc-o2-nopie clang-o0-nopie clang-o2-pie; do
    IFS=- read -r compiler optimization linking <<<"$build"
    if [[ "$linking" == pie ]]; then
        linking_flags=(-fPIE -pie)
    else
        linking_flags=(-fno-pie -no-pie)
    fi
    build_c_fixture_directory "$compiler" "$c_fixtures_dir/expressions" \
        "$output_dir/expressions-c-$build" "-${optimization^^}" -g3 -gdwarf-5 "${linking_flags[@]}"
done
build_c_fixture_directory gcc "$c_fixtures_dir/same-names" "$output_dir/same-names" \
    -O0 -g3 -gdwarf-5 -fPIE -pie
build_c_fixture_directory gcc "$c_fixtures_dir/globals" "$output_dir/globals-c-gcc-o0" \
    -O0 -g3 -gdwarf-5 -fno-omit-frame-pointer -fPIE -pie
build_c_fixture_directory clang "$c_fixtures_dir/globals" "$output_dir/globals-c-clang-o0" \
    -O0 -g3 -gdwarf-5 -fno-omit-frame-pointer -fPIE -pie
build_c_fixture_directory gcc "$c_fixtures_dir/globals" "$output_dir/globals-c-gcc-o2" \
    -O2 -g3 -gdwarf-5 -fomit-frame-pointer -fPIE -pie
build_c_fixture_directory clang "$c_fixtures_dir/globals" "$output_dir/globals-c-clang-o2" \
    -O2 -g3 -gdwarf-5 -fomit-frame-pointer -fPIE -pie
build_c_fixture_directory gcc "$c_fixtures_dir/globals" "$output_dir/globals-c-gcc-nopie" \
    -O0 -g3 -gdwarf-5 -fno-omit-frame-pointer -no-pie
build_shared_fixture gcc "$c_fixtures_dir/shared/library.c" "$output_dir/libglobals.so" \
    -O0 -g3 -gdwarf-5 -fno-omit-frame-pointer
build_fixture gcc "$c_fixtures_dir/shared/main.c" "$output_dir/globals-shared" \
    -O0 -g3 -gdwarf-5 -fno-omit-frame-pointer -fPIE -pie -ldl



embedded_views_dir="$c_fixtures_dir/embedded-views"
read_dash_version gcc
embedded_views_metadata="compiler=${dash_version}"$'\n'"target=x86_64-linux"$'\n'"backend=gcc"
run_cached_build "$embedded_views_dir" "$output_dir/libembedded-views.so" \
    "$embedded_views_metadata" \
    gcc -std=c17 -Wall -Wextra -Werror -shared -fPIC -O0 -g3 -gdwarf-5 -Isdk/c \
    "$embedded_views_dir/library.c" -o "$output_dir/libembedded-views.so"
run_cached_build "$embedded_views_dir" "$output_dir/embedded-views" \
    "$embedded_views_metadata" \
    gcc -std=c17 -Wall -Wextra -Werror -O0 -g3 -gdwarf-5 -fPIE -pie -Isdk/c \
    "$embedded_views_dir/main.c" -o "$output_dir/embedded-views" \
    "-L$output_dir" -lembedded-views '-Wl,-rpath,$ORIGIN'


zig build-exe -target wasm32-freestanding -O ReleaseSmall -fno-entry -rdynamic \
    --stack 16384 --dep microscope_kernel -Mroot=views/kernels/rust-btree.zig \
    -Mmicroscope_kernel=sdk/zig/microscope_kernel.zig -femit-bin="$output_dir/rust-btree.wasm"
if ! cmp -s "$output_dir/rust-btree.wasm" views/kernels/rust-btree.wasm; then
    printf 'views/kernels/rust-btree.wasm is not what its source builds: copy %s there\n' \
        "$output_dir/rust-btree.wasm" >&2
    exit 1
fi
rebuilt_outputs["$output_dir/rust-btree.wasm"]=true

zig cc --target=wasm32-freestanding -Os -nostdlib -Wl,--no-entry -Wl,-z,stack-size=16384 \
    -Isdk/c "$c_fixtures_dir/tutorial/tree.c" -o "$output_dir/tutorial-tree.wasm"
rebuilt_outputs["$output_dir/tutorial-tree.wasm"]=true
run_cached_build "$c_fixtures_dir/tutorial" "$output_dir/tutorial" \
    "$embedded_views_metadata" \
    gcc -std=c17 -Wall -Wextra -Werror -O0 -g3 -gdwarf-5 -fPIE -pie -Isdk/c "-Wa,-I$output_dir" \
    "$c_fixtures_dir/tutorial/tutorial.c" -o "$output_dir/tutorial"
build_shared_fixture gcc "$c_fixtures_dir/moved-code/library.c" "$output_dir/libmoved-code.so" \
    -O0 -g3 -gdwarf-5
build_fixture gcc "$c_fixtures_dir/moved-code/main.c" "$output_dir/moved-code" \
    -O0 -g3 -gdwarf-5 -fPIE -pie "-L$output_dir" -lmoved-code '-Wl,-rpath,$ORIGIN'
build_shared_fixture gcc "$c_fixtures_dir/tls-accessors/library.c" "$output_dir/libtls-accessors.so" \
    -O2 -g3 -gdwarf-5
build_fixture gcc "$c_fixtures_dir/tls-accessors/main.c" "$output_dir/tls-accessors" \
    -O2 -g3 -gdwarf-5 -fPIE -pie "-L$output_dir" -ltls-accessors '-Wl,-rpath,$ORIGIN'
build_fixture gcc "$c_fixtures_dir/code-copy.c" "$output_dir/code-copy" \
    -O2 -g3 -gdwarf-5 -fPIE -pie
build_shared_fixture gcc "$c_fixtures_dir/module-frames/library.c" "$output_dir/libmodule-frames.so" \
    -O0 -g3 -gdwarf-5 -fno-omit-frame-pointer
build_fixture gcc "$c_fixtures_dir/module-frames/main.c" "$output_dir/module-frames-gcc-o0" \
    -O0 -g3 -gdwarf-5 -fno-omit-frame-pointer -fPIE -pie \
    "-L$output_dir" -lmodule-frames '-Wl,-rpath,$ORIGIN'
build_fixture clang "$c_fixtures_dir/module-frames/main.c" "$output_dir/module-frames-clang-o2" \
    -O2 -g3 -gdwarf-5 -fomit-frame-pointer -fPIE -pie \
    "-L$output_dir" -lmodule-frames '-Wl,-rpath,$ORIGIN'
build_fixture gcc "$c_fixtures_dir/module-frames/main.c" "$output_dir/module-frames-gcc-nopie" \
    -O2 -g3 -gdwarf-5 -fomit-frame-pointer -no-pie \
    "-L$output_dir" -lmodule-frames '-Wl,-rpath,$ORIGIN'



mkdir -p "$output_dir/split"
build_fixture gcc "$c_fixtures_dir/basic.c" "$output_dir/split/basic-debuglink.full" \
    -O0 -g3 -gdwarf-5 -fPIE -pie
derive_split_debug "$output_dir/split/basic-debuglink.full" "$output_dir/split/basic-debuglink" \
    debuglink
build_fixture gcc "$c_fixtures_dir/basic.c" "$output_dir/split/basic-build-id.full" \
    -O0 -g3 -gdwarf-5 -fPIE -pie -Wl,--build-id
derive_split_debug "$output_dir/split/basic-build-id.full" "$output_dir/split/basic-build-id" \
    build-id "$output_dir/split/debug-root"




altlinked_script='
    set -euo pipefail
    root="$1"; output="$2"
    rm -rf "$output"
    cp -r "$root" "$output"
    debug=$(find "$output/.build-id" -name "*.debug")
    printf "../../.dwz/microscope-fixture\0\x01\x02\x03\x04" >"$output/altlink"
    objcopy --add-section ".gnu_debugaltlink=$output/altlink" "$debug"
    rm "$output/altlink"
    # The build cache counts only an executable output as built.
    touch "$output/ready"
    chmod +x "$output/ready"
'
run_cached_build "$output_dir/split/basic-build-id" "$output_dir/split/altlink-root/ready" \
    "derivation=altlinked-v2" \
    bash -c "$altlinked_script" _ "$output_dir/split/debug-root" "$output_dir/split/altlink-root"




build_shapes_fixture g++ "$output_dir/dwz/gcc-o0" -O0
build_shapes_fixture g++ "$output_dir/dwz/gcc-o2" -O2
build_shapes_fixture clang++ "$output_dir/dwz/clang-o2" -O2 -gdwarf-4
for variant in gcc-o0 gcc-o2 clang-o2; do
    derive_dwz "$output_dir/dwz/$variant" whole
    derive_dwz "$output_dir/dwz/$variant" dwarf5
done
derive_dwz "$output_dir/dwz/gcc-o2" split
build_shared_fixture gcc "$c_fixtures_dir/module-frames/library.c" \
    "$output_dir/split/libmodule-frames.so.full" -O0 -g3 -gdwarf-5 -Wl,--build-id \
    -Wl,-soname,libmodule-frames.so
derive_split_debug "$output_dir/split/libmodule-frames.so.full" \
    "$output_dir/split/libmodule-frames.so" debuglink
build_fixture gcc "$c_fixtures_dir/module-frames/main.c" "$output_dir/split/module-frames" \
    -O0 -g3 -gdwarf-5 -fPIE -pie "-L$output_dir/split" -lmodule-frames '-Wl,-rpath,$ORIGIN'
build_symbols_library gcc "$output_dir/libelf-symbols-gcc.so"
build_symbols_library clang "$output_dir/libelf-symbols-clang.so"


for library in stripped minidebug; do
    build_symbols_library gcc "$output_dir/libelf-symbols-${library}.so.full" \
        "libelf-symbols-${library}.so"
done
derive_stripped_library "$output_dir/libelf-symbols-stripped.so.full" \
    "$output_dir/libelf-symbols-stripped.so" no
derive_stripped_library "$output_dir/libelf-symbols-minidebug.so.full" \
    "$output_dir/libelf-symbols-minidebug.so" yes
require_symbols_layout "$output_dir/libelf-symbols-gcc.so" full
require_symbols_layout "$output_dir/libelf-symbols-clang.so" full
require_symbols_layout "$output_dir/libelf-symbols-stripped.so" dynamic
require_symbols_layout "$output_dir/libelf-symbols-minidebug.so" embedded
readonly symbols_variants=(
    "gcc-o0 gcc gcc -O0 -fno-omit-frame-pointer -fPIE -pie"
    "clang-o2 clang clang -O2 -fomit-frame-pointer -fPIE -pie"
    "gcc-nopie gcc gcc -O2 -fomit-frame-pointer -no-pie"
    "stripped stripped gcc -O0 -fno-omit-frame-pointer -fPIE -pie"
    "minidebug minidebug gcc -O0 -fno-omit-frame-pointer -fPIE -pie"
)
for variant in "${symbols_variants[@]}"; do
    read -r name library compiler flags <<<"$variant"
    # shellcheck disable=SC2086
    build_fixture "$compiler" "$c_fixtures_dir/elf-symbols/main.c" \
        "$output_dir/elf-symbols-${name}" -g3 -gdwarf-5 $flags \
        "-L$output_dir" "-lelf-symbols-${library}" '-Wl,-rpath,$ORIGIN'
wait_builds
    if ! readelf -dW "$output_dir/elf-symbols-${name}" \
        | grep -F "Shared library: [libelf-symbols-${library}.so]" >/dev/null; then
        printf 'error: elf-symbols-%s does not load libelf-symbols-%s.so\n' \
            "$name" "$library" >&2
        exit 1
    fi
done
build_disassembly_fixture gcc "$output_dir/disassembly-gcc-o0" \
    -O0 -fno-omit-frame-pointer -fPIE -pie
build_disassembly_fixture clang "$output_dir/disassembly-clang-o2-nopie" \
    -O2 -fomit-frame-pointer -no-pie

build_disassembly_fixture gcc "$output_dir/disassembly-gcc-mold" \
    -O0 -fno-omit-frame-pointer -fPIE -pie -fuse-ld=mold
build_fixture gcc "$c_fixtures_dir/null-call.c" "$output_dir/null-call" \
    -O0 -g3 -gdwarf-5 -fno-omit-frame-pointer -fPIE -pie
build_fixture gcc "$c_fixtures_dir/tls.c" "$output_dir/globals-tls-gcc" \
    -O0 -g3 -gdwarf-5 -fno-omit-frame-pointer -fPIE -pie -pthread
build_fixture clang "$c_fixtures_dir/tls.c" "$output_dir/globals-tls-clang" \
    -O0 -g3 -gdwarf-5 -fno-omit-frame-pointer -fPIE -pie -pthread


readonly tls_modules_dir="$c_fixtures_dir/tls-modules"
for libc in glibc musl; do
    compiler=gcc suffix=""
    if [[ $libc == musl ]]; then
        compiler=musl-gcc suffix=-musl
    fi
    build_shared_fixture "$compiler" "$tls_modules_dir/library.c" \
        "$output_dir/libtls-modules${suffix}.so" -O0 -g3 -gdwarf-5
    build_shared_fixture "$compiler" "$tls_modules_dir/plugin.c" \
        "$output_dir/libtls-plugin${suffix}.so" -O0 -g3 -gdwarf-5
done
tls_modules_linked=("-L$output_dir" '-Wl,-rpath,$ORIGIN')
build_tls_modules_fixture gcc "$output_dir/tls-modules-gcc" -O0 -fPIE -pie \
    '-DPLUGIN="libtls-plugin.so"' "$tls_modules_dir/main.c" "${tls_modules_linked[@]}" \
    -ltls-modules
build_tls_modules_fixture musl-gcc "$output_dir/tls-modules-musl-gcc-o0" -O0 -fPIE -pie \
    '-DPLUGIN="libtls-plugin-musl.so"' "$tls_modules_dir/main.c" "${tls_modules_linked[@]}" \
    -ltls-modules-musl
build_tls_modules_fixture musl-gcc "$output_dir/tls-modules-musl-gcc-static" -O0 -static \
    -DSTATIC_BUILD "$tls_modules_dir/main.c" "$tls_modules_dir/library.c"
build_tls_modules_fixture musl-clang "$output_dir/tls-modules-musl-clang-static-pie" -O2 \
    -static-pie -fPIE -DSTATIC_BUILD "$tls_modules_dir/main.c" "$tls_modules_dir/library.c"
for variant in gcc-o0 gcc-static clang-static-pie; do
    require_musl "$output_dir/tls-modules-musl-${variant}"
done

glibc_static=(-L"$GLIBC_STATIC_LIBRARIES" -DSTATIC_BUILD "$tls_modules_dir/library.c")
build_tls_modules_fixture gcc "$output_dir/tls-modules-gcc-static" -O0 -static \
    "$tls_modules_dir/main.c" "${glibc_static[@]}"
build_tls_modules_fixture clang "$output_dir/tls-modules-clang-static-pie" -O2 \
    -static-pie -fPIE "$tls_modules_dir/main.c" "${glibc_static[@]}"
build_tls_modules_fixture gcc "$output_dir/tls-modules-single-thread-gcc-static-pie" -O0 \
    -static-pie -fPIE "$tls_modules_dir/single-thread.c" "${glibc_static[@]}"
build_tls_modules_fixture clang "$output_dir/tls-modules-single-thread-clang-static" -O2 \
    -static "$tls_modules_dir/single-thread.c" "${glibc_static[@]}"
require_static_glibc "$output_dir/tls-modules-gcc-static" yes
require_static_glibc "$output_dir/tls-modules-clang-static-pie" yes
require_static_glibc "$output_dir/tls-modules-single-thread-gcc-static-pie" no
require_static_glibc "$output_dir/tls-modules-single-thread-clang-static" no



for version in 2.27 2.31 2.35; do
    suffix="-glibc${version}"
    build_ubuntu_tls_fixture library "$version" "$output_dir/libtls-modules${suffix}.so" \
        "$tls_modules_dir/library.c" --
    build_ubuntu_tls_fixture library "$version" "$output_dir/libtls-plugin${suffix}.so" \
        "$tls_modules_dir/plugin.c" --
    build_ubuntu_tls_fixture program "$version" "$output_dir/tls-modules${suffix}" \
        "-DPLUGIN=\"libtls-plugin${suffix}.so\"" "$tls_modules_dir/main.c" -- \
        "-L$output_dir" "-ltls-modules${suffix}"
done
for version in 2.31 2.35; do
    build_ubuntu_tls_fixture program "$version" \
        "$output_dir/tls-modules-single-thread-glibc${version}" \
        "$tls_modules_dir/single-thread.c" -- "-L$output_dir" "-ltls-modules-glibc${version}"
done
require_ubuntu_glibc "$output_dir/tls-modules-glibc2.27" 2.27 yes
require_ubuntu_glibc "$output_dir/tls-modules-glibc2.31" 2.31 yes
require_ubuntu_glibc "$output_dir/tls-modules-glibc2.35" 2.35 no
require_ubuntu_glibc "$output_dir/tls-modules-single-thread-glibc2.31" 2.31 no
require_ubuntu_glibc "$output_dir/tls-modules-single-thread-glibc2.35" 2.35 no
for variant in "g++ gcc -O0" "g++ gcc -O2" "clang++ clang -O2"; do
    read -r compiler name level <<<"$variant"
    suffix="${level#-}"
    build_cpp_fixture "$compiler" "$cpp_fixtures_dir/returns.cpp" \
        "$output_dir/returns-cpp-${name}-${suffix,,}" "$level" -g3 -gdwarf-5 -fPIE -pie
done
build_cpp_fixture g++ "$cpp_fixtures_dir/variables.cpp" "$output_dir/variables-cpp-gcc-o0" \
    -O0 -g3 -gdwarf-5 -fno-omit-frame-pointer -fPIE -pie
build_cpp_fixture g++ "$cpp_fixtures_dir/overloads.cpp" "$output_dir/overloads-cpp-gcc-o0" \
    -O0 -g3 -gdwarf-5 -fPIE -pie
build_cpp_fixture g++ "$cpp_fixtures_dir/overloads.cpp" "$output_dir/overloads-cpp-gcc-nodebug" \
    -O0 -fPIE -pie
build_cpp_fixture clang++ "$cpp_fixtures_dir/variables.cpp" "$output_dir/variables-cpp-clang-o0" \
    -O0 -g3 -gdwarf-5 -fno-omit-frame-pointer -fPIE -pie
build_cpp_fixture g++ "$cpp_fixtures_dir/variables.cpp" "$output_dir/variables-cpp-gcc-o2" \
    -O2 -g3 -gdwarf-5 -fomit-frame-pointer -fPIE -pie
build_cpp_fixture clang++ "$cpp_fixtures_dir/variables.cpp" "$output_dir/variables-cpp-clang-o2" \
    -O2 -g3 -gdwarf-5 -fomit-frame-pointer -fPIE -pie
build_cpp_fixture_directory g++ "$cpp_fixtures_dir/expressions" \
    "$output_dir/expressions-cpp-gcc-o0" -O0 -g3 -gdwarf-5 -fPIE -pie
build_cpp_fixture_directory clang++ "$cpp_fixtures_dir/expressions" \
    "$output_dir/expressions-cpp-clang-o2" -O2 -g3 -gdwarf-5 -fPIE -pie

build_cpp_fixture_directory g++ "$cpp_fixtures_dir/keys" \
    "$output_dir/keys-cpp-gcc-o0" -O0 -g3 -gdwarf-5 -fPIE -pie
build_cpp_fixture_directory clang++ "$cpp_fixtures_dir/keys" \
    "$output_dir/keys-cpp-clang-o2" -O2 -g3 -gdwarf-5 -fPIE -pie
build_cpp_fixture_directory clang++-libc++ "$cpp_fixtures_dir/keys" \
    "$output_dir/keys-cpp-libcxx-o0" -O0 -g3 -gdwarf-5 -fPIE -pie
build_cpp_fixture g++ "$cpp_fixtures_dir/records.cpp" "$output_dir/records-cpp-gcc-o0" \
    -O0 -g3 -gdwarf-5 -fno-omit-frame-pointer -fPIE -pie
build_cpp_fixture clang++ "$cpp_fixtures_dir/records.cpp" "$output_dir/records-cpp-clang-o0" \
    -O0 -g3 -gdwarf-5 -fno-omit-frame-pointer -fPIE -pie
build_cpp_fixture g++ "$cpp_fixtures_dir/records.cpp" "$output_dir/records-cpp-gcc-o2" \
    -O2 -g3 -gdwarf-5 -fomit-frame-pointer -fPIE -pie
build_cpp_fixture clang++ "$cpp_fixtures_dir/records.cpp" "$output_dir/records-cpp-clang-o2" \
    -O2 -g3 -gdwarf-5 -fomit-frame-pointer -fPIE -pie


build_cpp_fixture clang++ "$cpp_fixtures_dir/strings.cpp" "$output_dir/strings-cpp-clang-o0" \
    -O0 -g3 -gdwarf-5 -fstandalone-debug -fno-omit-frame-pointer -fPIE -pie
build_cpp_fixture g++ "$cpp_fixtures_dir/globals.cpp" "$output_dir/globals-cpp-gcc-o0" \
    -O0 -g3 -gdwarf-5 -fno-omit-frame-pointer -fPIE -pie
build_cpp_fixture clang++ "$cpp_fixtures_dir/globals.cpp" "$output_dir/globals-cpp-clang-o0" \
    -O0 -g3 -gdwarf-5 -fno-omit-frame-pointer -fPIE -pie
build_cpp_fixture g++ "$cpp_fixtures_dir/globals.cpp" "$output_dir/globals-cpp-gcc-o2" \
    -O2 -g3 -gdwarf-5 -fomit-frame-pointer -fPIE -pie
build_cpp_fixture clang++ "$cpp_fixtures_dir/globals.cpp" "$output_dir/globals-cpp-clang-o2" \
    -O2 -g3 -gdwarf-5 -fomit-frame-pointer -fPIE -pie
build_cpp_fixture g++ "$cpp_fixtures_dir/enums.cpp" "$output_dir/enums-cpp-gcc-o0" \
    -O0 -g3 -gdwarf-5 -fno-omit-frame-pointer -fPIE -pie
build_cpp_fixture clang++ "$cpp_fixtures_dir/enums.cpp" "$output_dir/enums-cpp-clang-o0" \
    -O0 -g3 -gdwarf-5 -fno-omit-frame-pointer -fPIE -pie
build_cpp_fixture g++ "$cpp_fixtures_dir/enums.cpp" "$output_dir/enums-cpp-gcc-o2" \
    -O2 -g3 -gdwarf-5 -fomit-frame-pointer -fPIE -pie
build_cpp_fixture clang++ "$cpp_fixtures_dir/enums.cpp" "$output_dir/enums-cpp-clang-o2" \
    -O2 -g3 -gdwarf-5 -fomit-frame-pointer -fPIE -pie
build_cpp_fixture g++ "$cpp_fixtures_dir/types.cpp" "$output_dir/types-cpp-gcc-dwarf4" \
    -O0 -g3 -gdwarf-4 -fdebug-types-section -fno-omit-frame-pointer -fPIE -pie
build_cpp_fixture g++ "$cpp_fixtures_dir/types.cpp" "$output_dir/types-cpp-gcc-dwarf5" \
    -O0 -g3 -gdwarf-5 -fdebug-types-section -fno-omit-frame-pointer -fPIE -pie
require_dwarf_operation "$output_dir/types-cpp-gcc-dwarf4" 'DW_AT_type.*signature:'
require_dwarf_operation "$output_dir/types-cpp-gcc-dwarf5" 'DW_AT_type.*signature:'
build_cpp_fixture g++ "$cpp_fixtures_dir/templates.cpp" "$output_dir/templates-cpp-gcc-o0" \
    -O0 -g3 -gdwarf-5 -fno-omit-frame-pointer -fPIE -pie
build_cpp_fixture clang++ "$cpp_fixtures_dir/templates.cpp" "$output_dir/templates-cpp-clang-o0" \
    -O0 -g3 -gdwarf-5 -fno-omit-frame-pointer -fPIE -pie

build_cpp_fixture g++ "$cpp_fixtures_dir/templates.cpp" "$output_dir/templates-cpp-gcc-dwarf4" \
    -O0 -g3 -gdwarf-4 -fdebug-types-section -fno-omit-frame-pointer -fPIE -pie
require_dwarf_operation "$output_dir/templates-cpp-gcc-dwarf4" 'DW_AT_type.*signature:'

build_cpp_fixture clang++-libc++ "$cpp_fixtures_dir/templates.cpp" "$output_dir/templates-cpp-libcxx-o0" \
    -O0 -g3 -gdwarf-5 -fno-omit-frame-pointer -fPIE -pie
require_dwarf_operation "$output_dir/templates-cpp-libcxx-o0" 'DW_AT_name.*: __1$'


for optimization in o0 o2; do
    level="-O${optimization#o}"
    build_cpp_fixture g++ "$cpp_fixtures_dir/containers.cpp" \
        "$output_dir/containers-cpp-gcc-$optimization" "$level" -std=c++23 -g3 -gdwarf-5 -fPIE -pie
    build_cpp_fixture clang++ "$cpp_fixtures_dir/containers.cpp" \
        "$output_dir/containers-cpp-clang-$optimization" "$level" -std=c++23 -g3 -gdwarf-5 -fPIE -pie
    build_cpp_fixture clang++-libc++ "$cpp_fixtures_dir/containers.cpp" \
        "$output_dir/containers-cpp-libcxx-$optimization" "$level" -std=c++23 -g3 -gdwarf-5 -fPIE -pie
done

build_cpp_fixture g++ "$cpp_fixtures_dir/containers.cpp" "$output_dir/containers-cpp-gcc-oldabi" \
    -O0 -std=c++23 -g3 -gdwarf-5 -fPIE -pie -D_GLIBCXX_USE_CXX11_ABI=0

build_cpp_fixture g++ "$cpp_fixtures_dir/containers.cpp" "$output_dir/containers-cpp-gcc-debug" \
    -O0 -std=c++23 -g3 -gdwarf-5 -fPIE -pie -D_GLIBCXX_DEBUG

build_cpp_fixture g++ "$cpp_fixtures_dir/containers.cpp" "$output_dir/containers-cpp-gcc-static" \
    -O0 -std=c++23 -g3 -gdwarf-5 -fPIE -pie -static-libstdc++


build_cpp_fixture clang++ "$cpp_fixtures_dir/containers.cpp" "$output_dir/containers-cpp-clang-simple" \
    -O0 -std=c++23 -g3 -gdwarf-5 -gsimple-template-names -fPIE -pie


build_cpp_fixture clang++-libc++ "$cpp_fixtures_dir/containers.cpp" \
    "$output_dir/containers-cpp-libcxx-standalone" -O0 -std=c++23 -g3 -gdwarf-5 -fstandalone-debug -fPIE -pie
build_rust_fixture "$rust_fixtures_dir/variables.rs" "$output_dir/variables-rust-o0" \
    -C opt-level=0 -C force-frame-pointers=yes


build_rust_fixture "$rust_fixtures_dir/syscalls.rs" "$output_dir/syscalls-rust-o0" \
    -C opt-level=0 -C force-frame-pointers=yes
build_rust_fixture "$rust_fixtures_dir/syscalls.rs" "$output_dir/syscalls-rust-release" \
    -C opt-level=3 -C debuginfo=0 -C strip=debuginfo
build_rust_fixture "$rust_fixtures_dir/variables.rs" "$output_dir/variables-rust-o2" \
    -C opt-level=2 -C force-frame-pointers=no
build_rust_fixture "$rust_fixtures_dir/records.rs" "$output_dir/records-rust-o0" \
    -C opt-level=0 -C force-frame-pointers=yes
build_rust_fixture "$rust_fixtures_dir/records.rs" "$output_dir/records-rust-o2" \
    -C opt-level=2 -C force-frame-pointers=no
build_rust_fixture "$rust_fixtures_dir/strings.rs" "$output_dir/strings-rust-o0" \
    -C opt-level=0 -C force-frame-pointers=yes
build_rust_fixture "$rust_fixtures_dir/floats.rs" "$output_dir/floats-rust" \
    -C opt-level=0 -C force-frame-pointers=yes
build_rust_fixture "$rust_fixtures_dir/enums.rs" "$output_dir/enums-rust-o0" \
    -C opt-level=0 -C force-frame-pointers=yes
build_rust_fixture "$rust_fixtures_dir/enums.rs" "$output_dir/enums-rust-o2" \
    -C opt-level=2 -C force-frame-pointers=no
build_rust_fixture "$rust_fixtures_dir/globals.rs" "$output_dir/globals-rust-o0" \
    -C opt-level=0 -C force-frame-pointers=yes
build_rust_fixture "$rust_fixtures_dir/globals.rs" "$output_dir/globals-rust-o2" \
    -C opt-level=2 -C force-frame-pointers=no
build_rust_fixture "$rust_fixtures_dir/expressions.rs" "$output_dir/expressions-rust-o0" \
    -C opt-level=0 -C force-frame-pointers=yes
build_rust_fixture "$rust_fixtures_dir/expressions.rs" "$output_dir/expressions-rust-o2" \
    -C opt-level=2 -C force-frame-pointers=no
build_rust_fixture "$rust_fixtures_dir/generics.rs" "$output_dir/generics-rust-o0" \
    -C opt-level=0 -C force-frame-pointers=yes
build_rust_fixture "$rust_fixtures_dir/generics.rs" "$output_dir/generics-rust-o2" \
    -C opt-level=2 -C force-frame-pointers=no


read_dash_version rustc
rust_sdk_metadata="compiler=${dash_version}"$'\n'"target=x86_64-linux"$'\n'"backend=rustc"
run_cached_build sdk/rust "$output_dir/libmicroscope_views.rlib" "$rust_sdk_metadata" \
    rustc --edition=2024 -D warnings --crate-type rlib --crate-name microscope_views \
    sdk/rust/src/lib.rs -o "$output_dir/libmicroscope_views.rlib"


env RUSTFLAGS= CARGO_ENCODED_RUSTFLAGS= CARGO_TARGET_DIR=build/kernels \
    cargo build --quiet --release --target wasm32-unknown-unknown \
    -Zbuild-std=core,panic_abort \
    --manifest-path "$rust_fixtures_dir/embedded-views/kernel/Cargo.toml"
cp build/kernels/wasm32-unknown-unknown/release/tree.wasm \
    "$output_dir/embedded-views-rust-tree.wasm"
rebuilt_outputs["$output_dir/embedded-views-rust-tree.wasm"]=true
run_cached_build "$rust_fixtures_dir/embedded-views" "$output_dir/embedded-views-rust" \
    "$rust_sdk_metadata" \
    env "MICROSCOPE_TREE_KERNEL=$output_dir/embedded-views-rust-tree.wasm" \
    rustc --edition=2024 -D warnings -C debuginfo=2 -C codegen-units=1 -C opt-level=0 \
    --crate-name embedded_views \
    --extern "microscope_views=$output_dir/libmicroscope_views.rlib" \
    "$rust_fixtures_dir/embedded-views/main.rs" -o "$output_dir/embedded-views-rust"

run_cached_build sdk/rust-probes "$output_dir/libmicroscope_probes.rlib" "$rust_sdk_metadata" \
    rustc --edition=2024 -D warnings -C opt-level=2 --crate-type rlib --crate-name microscope_probes \
    --cfg 'feature="enabled"' sdk/rust-probes/src/lib.rs -o "$output_dir/libmicroscope_probes.rlib"


run_cached_build sdk/rust-probes "$output_dir/libmicroscope_probes_c.so" "$rust_sdk_metadata" \
    rustc --edition=2024 -D warnings -C opt-level=2 --crate-type cdylib --crate-name microscope_probes \
    --cfg 'feature="enabled"' --cfg 'feature="c"' sdk/rust-probes/src/lib.rs \
    -o "$output_dir/libmicroscope_probes_c.so"
run_cached_build "$rust_fixtures_dir/probes" "$output_dir/probes" "$rust_sdk_metadata" \
    rustc --edition=2024 -D warnings -C debuginfo=2 -C opt-level=2 --crate-name probes \
    --extern "microscope_probes=$output_dir/libmicroscope_probes.rlib" \
    "$rust_fixtures_dir/probes/main.rs" -o "$output_dir/probes"

run_cached_build "$rust_fixtures_dir/chess" "$output_dir/chess" \
    "$rust_sdk_metadata" \
    rustc --edition=2024 -D warnings -C debuginfo=2 -C codegen-units=1 -C opt-level=0 \
    --crate-name chess \
    --extern "microscope_views=$output_dir/libmicroscope_views.rlib" \
    "$rust_fixtures_dir/chess/main.rs" -o "$output_dir/chess"

build_program rustc "$rust_fixtures_dir/containers.rs" "$output_dir/containers-rust-o0" \
    --edition=2024 -D warnings -C debuginfo=2 -C codegen-units=1 -C opt-level=0
build_program rustc "$rust_fixtures_dir/containers.rs" "$output_dir/containers-rust-o2" \
    --edition=2024 -D warnings -C debuginfo=2 -C codegen-units=1 -C opt-level=2
for level in 0 2; do
    build_program rustc "$rust_fixtures_dir/returns.rs" "$output_dir/returns-rust-o${level}" \
        --edition=2024 -D warnings -C debuginfo=2 -C codegen-units=1 -C opt-level="$level"
done

build_program rustc "$rust_fixtures_dir/containers.rs" "$output_dir/containers-rust-limited" \
    --edition=2024 -D warnings -C debuginfo=limited -C codegen-units=1 -C opt-level=0
for level in 0 2; do
    build_program rustc "$rust_fixtures_dir/keys.rs" "$output_dir/keys-rust-o${level}" \
        --edition=2024 -D warnings -C debuginfo=2 -C codegen-units=1 -C opt-level="$level"
done
build_go_fixture "$go_fixtures_dir/expressions" "$output_dir/expressions-go-o0" \
    -buildmode=pie "-gcflags=all=-N -l"
build_go_fixture "$go_fixtures_dir/expressions" "$output_dir/expressions-go-o2" \
    -buildmode=pie
build_go_fixture "$go_fixtures_dir/variables" "$output_dir/variables-go-o0" \
    -buildmode=pie "-gcflags=all=-N -l"
build_go_fixture "$go_fixtures_dir/records" "$output_dir/records-go-o0" \
    -buildmode=pie "-gcflags=all=-N -l"
build_go_fixture "$go_fixtures_dir/records" "$output_dir/records-go-o2" \
    -buildmode=pie
build_go_fixture "$go_fixtures_dir/exec" "$output_dir/exec-go-o0" \
    -buildmode=pie "-gcflags=all=-N -l"
build_go_fixture "$go_fixtures_dir/strings" "$output_dir/strings-go-o0" \
    -buildmode=pie "-gcflags=all=-N -l"
build_go_fixture "$go_fixtures_dir/globals" "$output_dir/globals-go-o0" \
    -buildmode=pie "-gcflags=all=-N -l"
build_go_fixture "$go_fixtures_dir/globals" "$output_dir/globals-go-o2" \
    -buildmode=pie
build_go_fixture "$go_fixtures_dir/enums" "$output_dir/enums-go-o0" \
    -buildmode=pie "-gcflags=all=-N -l"
build_go_fixture "$go_fixtures_dir/enums" "$output_dir/enums-go-o2" \
    -buildmode=pie
build_go_fixture "$go_fixtures_dir/generics" "$output_dir/generics-go-o0" \
    -buildmode=pie "-gcflags=all=-N -l"
build_go_fixture "$go_fixtures_dir/generics" "$output_dir/generics-go-o2" \
    -buildmode=pie
build_go_fixture "$go_fixtures_dir/containers" "$output_dir/containers-go-o0" \
    -buildmode=pie "-gcflags=all=-N -l"
build_go_fixture "$go_fixtures_dir/containers" "$output_dir/containers-go-o2" \
    -buildmode=pie
build_go_fixture "$go_fixtures_dir/names" "$output_dir/names-go-o0" \
    -buildmode=pie "-gcflags=all=-N -l"
build_go_fixture "$go_fixtures_dir/names" "$output_dir/names-go-o2" \
    -buildmode=pie
build_go_fixture "$go_fixtures_dir/stdlib" "$output_dir/stdlib-go-o0" \
    -buildmode=pie "-gcflags=all=-N -l"
build_go_fixture "$go_fixtures_dir/stdlib" "$output_dir/stdlib-go-o2" \
    -buildmode=pie
build_go_fixture "$go_fixtures_dir/values" "$output_dir/values-go-o0" \
    -buildmode=pie "-gcflags=all=-N -l"
build_go_fixture "$go_fixtures_dir/values" "$output_dir/values-go-o2" \
    -buildmode=pie
require_dwarf_operation "$output_dir/variables-go-o0" 'DW_AT_language.*Go'
require_dwarf_operation "$output_dir/variables-go-o0" main.inspectScalars
require_dwarf_operation "$output_dir/enums-go-o0" 'DW_TAG_constant'
build_zig_fixture "$zig_fixtures_dir/returns.zig" "$output_dir/returns-zig-o0" \
    -O Debug -fPIE -fno-omit-frame-pointer
build_zig_fixture "$zig_fixtures_dir/returns.zig" "$output_dir/returns-zig-o2" \
    -O ReleaseSafe -fPIE -fomit-frame-pointer
build_zig_self_hosted_fixture "$zig_fixtures_dir/returns.zig" "$output_dir/returns-zig-self-hosted" \
    -O Debug
build_zig_fixture "$zig_fixtures_dir/generics.zig" "$output_dir/generics-zig-o0" \
    -O Debug -fPIE -fno-omit-frame-pointer
build_zig_fixture "$zig_fixtures_dir/containers.zig" "$output_dir/containers-zig-o0" \
    -O Debug -fPIE -fno-omit-frame-pointer
build_zig_fixture "$zig_fixtures_dir/containers.zig" "$output_dir/containers-zig-o2" \
    -O ReleaseSafe -fPIE -fomit-frame-pointer
build_zig_self_hosted_fixture "$zig_fixtures_dir/containers.zig" "$output_dir/containers-zig-self-hosted" \
    -O Debug
build_zig_fixture "$zig_fixtures_dir/keys.zig" "$output_dir/keys-zig-o0" \
    -O Debug -fPIE -fno-omit-frame-pointer
build_zig_fixture "$zig_fixtures_dir/keys.zig" "$output_dir/keys-zig-o2" \
    -O ReleaseSafe -fPIE -fomit-frame-pointer
build_zig_fixture "$zig_fixtures_dir/expressions.zig" "$output_dir/expressions-zig-o0" \
    -O Debug -fPIE -fno-omit-frame-pointer
build_zig_fixture "$zig_fixtures_dir/expressions.zig" "$output_dir/expressions-zig-o2" \
    -O ReleaseFast -fPIE -fomit-frame-pointer
build_zig_fixture "$zig_fixtures_dir/floats.zig" "$output_dir/floats-zig" -O Debug
build_zig_fixture "$zig_fixtures_dir/variables.zig" "$output_dir/variables-zig-o0" \
    -O Debug -fPIE -fno-omit-frame-pointer
build_zig_fixture "$zig_fixtures_dir/variables.zig" "$output_dir/variables-zig-o2" \
    -O ReleaseFast -fPIE -fomit-frame-pointer
build_zig_fixture "$zig_fixtures_dir/records.zig" "$output_dir/records-zig-o0" \
    -O Debug -fPIE -fno-omit-frame-pointer
build_zig_fixture "$zig_fixtures_dir/records.zig" "$output_dir/records-zig-o2" \
    -O ReleaseFast -fPIE -fomit-frame-pointer
build_zig_fixture "$zig_fixtures_dir/enums.zig" "$output_dir/enums-zig-o0" \
    -O Debug -fPIE -fno-omit-frame-pointer
build_zig_fixture "$zig_fixtures_dir/enums.zig" "$output_dir/enums-zig-o2" \
    -O ReleaseFast -fPIE -fomit-frame-pointer
build_zig_fixture "$zig_fixtures_dir/globals.zig" "$output_dir/globals-zig-o0" \
    -O Debug -fPIE -fno-omit-frame-pointer
build_zig_fixture "$zig_fixtures_dir/globals.zig" "$output_dir/globals-zig-o2" \
    -O ReleaseFast -fPIE -fomit-frame-pointer
build_zig_fixture "$zig_fixtures_dir/globals.zig" "$output_dir/globals-zig-nopie" \
    -O Debug -fno-PIE -fno-omit-frame-pointer
require_dwarf_operation "$output_dir/variables-zig-o0" 'DW_AT_producer.*zig 0.16.0'
require_dwarf_operation "$output_dir/variables-zig-o0" variables.inspectScalars
build_rust_fixture "$rust_fixtures_dir/stepping-boundaries.rs" "$output_dir/stepping-boundaries-rust-o0" \
    -C opt-level=0 -C force-frame-pointers=yes
build_rust_fixture "$rust_fixtures_dir/stepping-boundaries.rs" "$output_dir/stepping-boundaries-rust-o2" \
    -C opt-level=2 -C force-frame-pointers=no
build_zig_fixture "$zig_fixtures_dir/stepping-boundaries.zig" \
    "$output_dir/stepping-boundaries-zig-o0" \
    -O Debug -fPIE -fno-omit-frame-pointer
build_zig_fixture "$zig_fixtures_dir/stepping-boundaries.zig" \
    "$output_dir/stepping-boundaries-zig-o2" \
    -O ReleaseFast -fPIE -fomit-frame-pointer
require_dwarf_operation "$output_dir/stepping-boundaries-zig-o0" \
    'DW_TAG_inlined_subroutine'
build_fixture gcc "$c_fixtures_dir/variables-threads.c" "$output_dir/variables-threads" \
    -O0 -g3 -gdwarf-5 -fno-omit-frame-pointer -fPIE -pie -pthread
build_zig_fixture "$zig_fixtures_dir/variables-threads.zig" \
    "$output_dir/variables-threads-zig" \
    -O Debug -fPIE -fno-omit-frame-pointer
build_fixture gcc "$c_fixtures_dir/variables-inline.c" "$output_dir/variables-inline-gcc-o0" \
    -O0 -g3 -gdwarf-5 -fno-omit-frame-pointer -fPIE -pie
build_fixture clang "$c_fixtures_dir/variables-inline.c" "$output_dir/variables-inline-clang-o0" \
    -O0 -g3 -gdwarf-5 -fno-omit-frame-pointer -fPIE -pie
build_fixture gcc "$c_fixtures_dir/variables-inline.c" "$output_dir/variables-inline-gcc-o1" \
    -O1 -g3 -gdwarf-5 -fno-omit-frame-pointer -fPIE -pie
build_fixture clang "$c_fixtures_dir/variables-inline.c" "$output_dir/variables-inline-clang-o1" \
    -O1 -g3 -gdwarf-5 -fno-omit-frame-pointer -fPIE -pie
build_fixture gcc "$c_fixtures_dir/spin.c" "$output_dir/spin" \
    -O0 -g3 -fPIE -pie
build_fixture gcc "$c_fixtures_dir/threads.c" "$output_dir/threads" \
    -O0 -g3 -fPIE -pie -pthread

build_fixture musl-gcc "$c_fixtures_dir/threads.c" "$output_dir/threads-musl" \
    -O0 -g3 -fPIE -pie -pthread
build_fixture musl-gcc "$c_fixtures_dir/threads.c" "$output_dir/threads-musl-static" \
    -O0 -g3 -static -pthread
require_musl "$output_dir/threads-musl"
require_musl "$output_dir/threads-musl-static"
build_fixture gcc "$c_fixtures_dir/process-environment.c" "$output_dir/process-environment" \
    -O0 -g3 -fPIE -pie
build_fixture gcc "$c_fixtures_dir/output-streams.c" "$output_dir/output-streams" \
    -O0 -g3 -fPIE -pie
build_fixture gcc "$c_fixtures_dir/stdin-digest.c" "$output_dir/stdin-digest" \
    -O0 -g3 -fPIE -pie
build_fixture gcc "$c_fixtures_dir/thread-burst.c" "$output_dir/thread-burst" \
    -O0 -g3 -fPIE -pie -pthread
build_c_fixture_directory gcc "$c_fixtures_dir/life" "$output_dir/life" -O0 -g3 -fPIE -pie


build_c_fixture_directory gcc "$c_fixtures_dir/metrics" "$output_dir/metrics" -O0 -g3 -fPIE -pie -lm
build_go_fixture "$go_fixtures_dir/charts" "$output_dir/charts" \
    -buildmode=pie "-gcflags=all=-N -l"
build_program rustc "$rust_fixtures_dir/profile/main.rs" "$output_dir/profile" \
    --edition=2024 -D warnings -C debuginfo=2 -C codegen-units=1 -C opt-level=0 --crate-name profile
build_zig_fixture "$zig_fixtures_dir/samples.zig" "$output_dir/samples" \
    -O Debug -fPIE -fno-omit-frame-pointer
build_cpp_fixture_directory g++ "$cpp_fixtures_dir/tree" "$output_dir/tree" -O0 -g3 -fPIE -pie
build_cpp_fixture_directory g++ "$cpp_fixtures_dir/mesh" "$output_dir/mesh" -O0 -g3 -fPIE -pie
build_fixture gcc "$c_fixtures_dir/kvstore.c" "$output_dir/kvstore" \
    -O0 -g3 -fPIE -pie -pthread
build_program rustc "$rust_fixtures_dir/kvstore.rs" "$output_dir/kvstore-rust" \
    --edition=2024 -D warnings -C debuginfo=2 -C codegen-units=1 -C opt-level=0
build_go_fixture "$go_fixtures_dir/kvstore" "$output_dir/kvstore-go" \
    -buildmode=pie "-gcflags=all=-N -l"
build_program rustc "$rust_fixtures_dir/config-error.rs" "$output_dir/config-error-rust" \
    --edition=2024 -D warnings -C debuginfo=2 -C codegen-units=1 -C opt-level=0
build_fixture gcc "$c_fixtures_dir/strings.c" "$output_dir/strings-c-gcc-o0" \
    -O0 -g3 -fPIE -pie
build_fixture gcc "$c_fixtures_dir/glibc.c" "$output_dir/glibc-c-gcc-o0" \
    -O0 -g3 -fPIE -pie
build_fixture gcc "$c_fixtures_dir/glibc.c" "$output_dir/glibc-c-gcc-o2" \
    -O2 -g3 -fPIE -pie
build_fixture clang "$c_fixtures_dir/glibc.c" "$output_dir/glibc-c-clang-o0" \
    -O0 -g3 -fPIE -pie
build_fixture gcc "$c_fixtures_dir/floats.c" "$output_dir/floats-c-gcc" \
    -O0 -g3 -fPIE -pie
build_fixture clang "$c_fixtures_dir/floats.c" "$output_dir/floats-c-clang" \
    -O0 -g3 -fPIE -pie
build_fixture gcc "$c_fixtures_dir/line-sliding.c" "$output_dir/line-sliding" \
    -O0 -g3 -fPIE -pie
build_fixture gcc "$c_fixtures_dir/terminate.c" "$output_dir/terminate" \
    -O0 -g3 -fPIE -pie
build_fixture gcc "$c_fixtures_dir/signal-policy.c" "$output_dir/signal-policy" \
    -O0 -g3 -fPIE -pie
build_fixture gcc "$c_fixtures_dir/signal-steps.c" "$output_dir/signal-steps" \
    -O0 -g3 -fno-omit-frame-pointer -fPIE -pie


build_fixture clang "$c_fixtures_dir/signal-steps.c" "$output_dir/signal-steps-clang" \
    -O0 -g3 -fno-omit-frame-pointer -fPIE -pie
build_fixture gcc "$c_fixtures_dir/signals.c" "$output_dir/signals" \
    -O0 -g3 -fPIE -pie
build_fixture gcc "$c_fixtures_dir/fatal-signal.c" "$output_dir/fatal-signal" \
    -O0 -g3 -fPIE -pie
build_fixture gcc "$c_fixtures_dir/interrupt.c" "$output_dir/interrupt" \
    -O0 -g3 -fPIE -pie
build_fixture gcc "$c_fixtures_dir/memfd-exec.c" "$output_dir/memfd-exec" \
    -O0 -g3 -fPIE -pie
build_fixture gcc "$c_fixtures_dir/fork.c" "$output_dir/fork" \
    -O0 -g3 -fPIE -pie
build_fixture gcc "$c_fixtures_dir/fork-orphans.c" "$output_dir/fork-orphans" \
    -O0 -g3 -fPIE -pie
build_fixture gcc "$c_fixtures_dir/vfork.c" "$output_dir/vfork" \
    -O0 -g3 -fPIE -pie
build_fixture gcc "$c_fixtures_dir/vfork-threads.c" "$output_dir/vfork-threads" \
    -O0 -g3 -fPIE -pie -pthread
build_fixture gcc "$c_fixtures_dir/signal-altstack.c" "$output_dir/signal-altstack" \
    -O0 -g3 -fPIE -pie
build_fixture gcc "$c_fixtures_dir/program-trap.c" "$output_dir/program-trap" \
    -O0 -g3 -fPIE -pie
build_fixture gcc "$c_fixtures_dir/job-control.c" "$output_dir/job-control" \
    -O0 -g3 -fPIE -pie
build_fixture gcc "$c_fixtures_dir/thread-exec.c" "$output_dir/thread-exec" \
    -O0 -g3 -fPIE -pie -pthread
build_fixture gcc "$c_fixtures_dir/reexec.c" "$output_dir/reexec" \
    -O0 -g3 -fPIE -pie
build_fixture gcc "$c_fixtures_dir/exec-other.c" "$output_dir/exec-other" \
    -O0 -g3 -fPIE -pie -pthread
build_fixture gcc "$c_fixtures_dir/exec-target.c" "$output_dir/exec-target" \
    -O0 -g3 -fPIE -pie
build_fixture gcc "$c_fixtures_dir/process-tree.c" "$output_dir/process-tree" \
    -O0 -g3 -fPIE -pie
build_fixture gcc "$c_fixtures_dir/group-spin.c" "$output_dir/group-spin" \
    -O0 -g3 -fPIE -pie



build_fixture gcc "$c_fixtures_dir/measure.c" "$output_dir/measure-gcc-nodebug" \
    -O0 -fPIE -pie
build_fixture clang "$c_fixtures_dir/measure.c" "$output_dir/measure-clang-nopie-lazy" \
    -O1 -fno-pie -no-pie -Wl,-z,lazy
build_fixture gcc "$c_fixtures_dir/measure.c" "$output_dir/measure-gcc-static" \
    -O0 -static -L"$GLIBC_STATIC_LIBRARIES"
build_fixture gcc "$c_fixtures_dir/thread-steps.c" "$output_dir/thread-steps-gcc-o0" \
    -O0 -g3 -fno-omit-frame-pointer -fPIE -pie -pthread
build_fixture clang "$c_fixtures_dir/thread-steps.c" "$output_dir/thread-steps-clang-o2" \
    -O2 -g3 -fPIE -pie -pthread
build_fixture gcc "$c_fixtures_dir/hot-calls.c" "$output_dir/hot-calls" \
    -O0 -g3 -fPIE -pie -pthread
build_fixture gcc "$c_fixtures_dir/thread-stress.c" "$output_dir/thread-stress" \
    -O0 -g3 -fPIE -pie -pthread
build_fixture gcc "$c_fixtures_dir/step.c" "$output_dir/step" \
    -O0 -g3 -fno-omit-frame-pointer -fPIE -pie
for variant in "gcc -O0" "clang -O0" "gcc -O2"; do
    read -r compiler level <<<"$variant"
    suffix="${level#-}"
    build_fixture "$compiler" "$c_fixtures_dir/step-targets.c" \
        "$output_dir/step-targets-${compiler}-${suffix,,}" "$level" -g3 -gdwarf-5 -fPIE -pie
done
build_fixture gcc "$c_fixtures_dir/jump.c" "$output_dir/jump" \
    -O0 -g3 -fno-omit-frame-pointer -fPIE -pie
build_fixture gcc "$c_fixtures_dir/stepping-boundaries.c" "$output_dir/stepping-boundaries-gcc-o0" \
    -O0 -g3 -gdwarf-5 -fno-omit-frame-pointer -fPIE -pie
build_fixture clang "$c_fixtures_dir/stepping-boundaries.c" "$output_dir/stepping-boundaries-clang-o0" \
    -O0 -g3 -gdwarf-5 -fno-omit-frame-pointer -fPIE -pie
build_fixture gcc "$c_fixtures_dir/stepping-boundaries.c" "$output_dir/stepping-boundaries-gcc-o2" \
    -O2 -g3 -gdwarf-5 -fomit-frame-pointer -fPIE -pie
build_fixture clang "$c_fixtures_dir/stepping-boundaries.c" "$output_dir/stepping-boundaries-clang-o2" \
    -O2 -g3 -gdwarf-5 -fomit-frame-pointer -mno-red-zone -fPIE -pie
build_fixture gcc "$c_fixtures_dir/tail-calls.c" "$output_dir/tail-calls-gcc-o2" \
    -O2 -g3 -gdwarf-5 -fomit-frame-pointer -fPIE -pie
require_tail_jump "$output_dir/tail-calls-gcc-o2" outer_tail add_one
require_tail_jump "$output_dir/tail-calls-gcc-o2" outer_chain chain_helper
require_tail_jump "$output_dir/tail-calls-gcc-o2" descend_tail mutual_tail
build_fixture clang "$c_fixtures_dir/tail-calls.c" "$output_dir/tail-calls-clang-o2" \
    -O2 -g3 -gdwarf-5 -fomit-frame-pointer -fPIE -pie
require_tail_jump "$output_dir/tail-calls-clang-o2" outer_tail add_one
require_tail_jump "$output_dir/tail-calls-clang-o2" outer_chain chain_helper
require_tail_jump "$output_dir/tail-calls-clang-o2" descend_tail mutual_tail
for variant in gcc-o2:gcc:-gdwarf-5 gcc-o2-dwarf4:gcc:-gdwarf-4 clang-o2:clang:-gdwarf-5; do
    IFS=: read -r name compiler dwarf <<<"$variant"
    build_fixture "$compiler" "$c_fixtures_dir/tail-frames.c" "$output_dir/tail-frames-$name" \
        -O2 -g3 "$dwarf" -fomit-frame-pointer -fPIE -pie
    require_tail_jump "$output_dir/tail-frames-$name" top middle
    require_tail_jump "$output_dir/tail-frames-$name" middle leaf
    require_tail_jump "$output_dir/tail-frames-$name" either leaf
done

for variant in \
    gcc-o0:gcc:-O0:-fno-omit-frame-pointer:-fPIE:-pie \
    gcc-o2-nofp:gcc:-O2:-fomit-frame-pointer:-fPIE:-pie \
    clang-o2:clang:-O2:-fno-omit-frame-pointer:-fno-pie:-no-pie \
    clang-o2-nofp:clang:-O2:-fomit-frame-pointer:-fPIE:-pie; do
    IFS=: read -r name compiler level frames code link <<<"$variant"
    build_fixture "$compiler" "$c_fixtures_dir/profile-stacks.c" "$output_dir/profile-stacks-$name" \
        "$level" -g3 "$frames" "$code" "$link" -pthread
done


build_fixture clang "$c_fixtures_dir/profile-stacks.c" "$output_dir/profile-stacks-clang-o2-nocfi" \
    -O2 -fno-omit-frame-pointer -fno-asynchronous-unwind-tables -fno-unwind-tables -fPIE -pie \
    -pthread -s

build_fixture gcc "$c_fixtures_dir/profile-attach.c" "$output_dir/profile-attach-gcc-o0" \
    -O0 -g3 -fno-omit-frame-pointer -fPIE -pie -pthread
build_fixture clang "$c_fixtures_dir/profile-attach.c" "$output_dir/profile-attach-clang-o2-nofp" \
    -O2 -g3 -fomit-frame-pointer -fPIE -pie -pthread

build_fixture gcc "$c_fixtures_dir/profile-signals.c" "$output_dir/profile-signals-gcc-o0" \
    -O0 -g3 -fno-omit-frame-pointer -fPIE -pie
build_fixture clang "$c_fixtures_dir/profile-signals.c" "$output_dir/profile-signals-clang-o2-nofp" \
    -O2 -g3 -fomit-frame-pointer -fPIE -pie
build_fixture gcc "$c_fixtures_dir/profile-vdso.c" "$output_dir/profile-vdso" \
    -O2 -g3 -fno-omit-frame-pointer -fPIE -pie

build_fixture gcc "$c_fixtures_dir/profile-moved-jit.c" "$output_dir/profile-moved-jit" \
    -O0 -g3 -fno-omit-frame-pointer -fPIE -pie

build_fixture gcc "$c_fixtures_dir/profile-phases.c" "$output_dir/profile-phases" \
    -O0 -g3 -fno-omit-frame-pointer -fPIE -pie -pthread

build_fixture gcc "$c_fixtures_dir/profile-offcpu.c" "$output_dir/profile-offcpu" \
    -O0 -g3 -fno-omit-frame-pointer -fPIE -pie -pthread

build_fixture gcc "$c_fixtures_dir/profile-switches.c" "$output_dir/profile-switches" \
    -O1 -g -fno-omit-frame-pointer -pthread

build_fixture gcc "$c_fixtures_dir/profile-recursion.c" "$output_dir/profile-recursion" \
    -O1 -g -fno-omit-frame-pointer -fno-optimize-sibling-calls

build_fixture gcc "$c_fixtures_dir/overhead.c" "$output_dir/overhead" \
    -O2 -g3 -fno-omit-frame-pointer -fPIE -pie -pthread

build_fixture musl-gcc "$c_fixtures_dir/profile-stacks.c" "$output_dir/profile-stacks-musl" \
    -O2 -g3 -fno-omit-frame-pointer -static -pthread
build_fixture musl-gcc "$c_fixtures_dir/profile-stacks.c" "$output_dir/profile-stacks-musl-nofp" \
    -O2 -g3 -fomit-frame-pointer -static -pthread

build_fixture gcc "$c_fixtures_dir/profile-stacks.c" "$output_dir/profile-stacks-gcc-mold" \
    -O2 -g3 -fno-omit-frame-pointer -fPIE -pie -fuse-ld=mold -pthread
build_cpp_fixture g++ "$cpp_fixtures_dir/profile-stacks.cpp" "$output_dir/profile-stacks-cpp-gcc-o2-nofp" \
    -O2 -g3 -fomit-frame-pointer -fPIE -pie -pthread
build_cpp_fixture clang++ "$cpp_fixtures_dir/profile-stacks.cpp" "$output_dir/profile-stacks-cpp-clang-o0" \
    -O0 -g3 -fno-omit-frame-pointer -fPIE -pie -pthread


build_cpp_fixture clang++ "$cpp_fixtures_dir/profile-names.cpp" "$output_dir/profile-names-cpp-clang-g1" \
    -O2 -g1 -fno-omit-frame-pointer -fPIE -pie
build_rust_fixture "$rust_fixtures_dir/profile-stacks.rs" "$output_dir/profile-stacks-rust-o0" \
    -C opt-level=0 -C force-frame-pointers=yes
build_rust_fixture "$rust_fixtures_dir/profile-stacks.rs" "$output_dir/profile-stacks-rust-o2-nofp" \
    -C opt-level=2 -C force-frame-pointers=no
build_fixture gcc "$c_fixtures_dir/step-over-libc.c" "$output_dir/step-over-libc" \
    -O0 -g3 -fno-omit-frame-pointer -fPIE -pie
build_fixture gcc "$c_fixtures_dir/orphan-frames.c" "$output_dir/orphan-frames" \
    -O0 -g3 -fno-omit-frame-pointer -fPIE -pie

build_fixture gcc "$c_fixtures_dir/assembly-after-code.c" "$output_dir/assembly-after-code" \
    -O0 -g3 -fno-toplevel-reorder -fno-omit-frame-pointer -fPIE -pie
build_fixture gcc "$c_fixtures_dir/undescribed-caller.c" "$output_dir/undescribed-caller" \
    -O0 -g3 -fno-toplevel-reorder -fno-omit-frame-pointer -fPIE -pie
build_fixture gcc "$c_fixtures_dir/repeated-calls.c" "$output_dir/repeated-calls-gcc-o0" \
    -O0 -g3 -fno-omit-frame-pointer -fPIE -pie
build_fixture clang "$c_fixtures_dir/repeated-calls.c" "$output_dir/repeated-calls-clang-o0" \
    -O0 -g3 -fno-omit-frame-pointer -fPIE -pie
build_fixture gcc "$c_fixtures_dir/freestanding-entry.c" "$output_dir/freestanding-entry" \
    -O0 -g3 -static -nostdlib -fno-pie -no-pie -fno-stack-protector
build_fixture gcc "$c_fixtures_dir/freestanding-entry.c" "$output_dir/freestanding-entry-bare" \
    -O0 -g3 -static -nostdlib -fno-pie -no-pie -fno-stack-protector -DBARE_ENTRY
build_fixture gcc "$c_fixtures_dir/unwind.c" "$output_dir/unwind-o0" \
    -O0 -g3 -fno-omit-frame-pointer -fPIE -pie
build_fixture gcc "$c_fixtures_dir/unwind.c" "$output_dir/unwind-o2" \
    -O2 -g3 -fomit-frame-pointer -fPIE -pie
build_fixture gcc "$c_fixtures_dir/unwind.c" "$output_dir/unwind-nopie" \
    -O2 -g3 -fomit-frame-pointer -no-pie
build_fixture clang "$c_fixtures_dir/unwind.c" "$output_dir/unwind-clang-o2" \
    -O2 -g3 -fomit-frame-pointer -fPIE -pie


build_fixture gcc "$c_fixtures_dir/frames.c" "$output_dir/frames-gcc-o0" \
    -O0 -g3 -gdwarf-5 -fno-omit-frame-pointer -fPIE -pie
build_fixture gcc "$c_fixtures_dir/frames.c" "$output_dir/frames-gcc-o2" \
    -O2 -g3 -gdwarf-5 -fomit-frame-pointer -fPIE -pie
build_fixture gcc "$c_fixtures_dir/frames.c" "$output_dir/frames-gcc-o2-nopie" \
    -O2 -g3 -gdwarf-5 -fomit-frame-pointer -no-pie
build_fixture clang "$c_fixtures_dir/frames.c" "$output_dir/frames-clang-o0" \
    -O0 -g3 -gdwarf-5 -fno-omit-frame-pointer -fPIE -pie
build_fixture clang "$c_fixtures_dir/frames.c" "$output_dir/frames-clang-o2" \
    -O2 -g3 -gdwarf-5 -fomit-frame-pointer -fPIE -pie


build_shared_fixture gcc "$c_fixtures_dir/locations/library.c" "$output_dir/liblocations.so" \
    -O2 -g3 -gdwarf-5 -fomit-frame-pointer
locations_library=("-L$output_dir" -llocations '-Wl,-rpath,$ORIGIN')
build_fixture gcc "$c_fixtures_dir/locations/main.c" "$output_dir/locations-gcc-o2" \
    -O2 -g3 -gdwarf-5 -fomit-frame-pointer -fPIE -pie "${locations_library[@]}"
require_dwarf_operation "$output_dir/locations-gcc-o2" 'DW_OP_piece'
require_dwarf_operation "$output_dir/locations-gcc-o2" 'DW_OP_entry_value'
require_dwarf_operation "$output_dir/locations-gcc-o2" 'DW_OP_GNU_parameter_ref'
require_dwarf_operation "$output_dir/locations-gcc-o2" 'DW_OP_implicit_pointer'
build_fixture gcc "$c_fixtures_dir/locations/main.c" "$output_dir/locations-gcc-o2-nopie" \
    -O2 -g3 -gdwarf-5 -fomit-frame-pointer -no-pie "${locations_library[@]}"
build_fixture clang "$c_fixtures_dir/locations/main.c" "$output_dir/locations-clang-o2" \
    -O2 -g3 -gdwarf-5 -fomit-frame-pointer -fPIE -pie "${locations_library[@]}"
require_dwarf_operation "$output_dir/locations-clang-o2" 'DW_OP_piece'
require_dwarf_operation "$output_dir/locations-clang-o2" 'DW_AT_call_tail_call'
build_shared_fixture gcc "$c_fixtures_dir/data-library/library.c" "$output_dir/libdata.so" \
    -O0 -g3 -gdwarf-5 -nostdlib
require_no_code "$output_dir/libdata.so"
build_fixture gcc "$c_fixtures_dir/data-library/main.c" "$output_dir/data-library" \
    -O0 -g3 -gdwarf-5 -fPIE -pie "-L$output_dir" -ldata '-Wl,-rpath,$ORIGIN'
build_fixture gcc "$c_fixtures_dir/inline.c" "$output_dir/inline-gcc-o1" \
    -O1 -g3 -gdwarf-5 -fno-omit-frame-pointer -fPIE -pie
build_fixture gcc "$c_fixtures_dir/inline.c" "$output_dir/inline-gcc-o2" \
    -O2 -g3 -gdwarf-5 -fomit-frame-pointer -fPIE -pie
build_fixture clang "$c_fixtures_dir/inline.c" "$output_dir/inline-clang-o1" \
    -O1 -g3 -gdwarf-5 -fno-omit-frame-pointer -fPIE -pie
build_fixture clang "$c_fixtures_dir/inline.c" "$output_dir/inline-clang-o2" \
    -O2 -g3 -gdwarf-5 -fomit-frame-pointer -fPIE -pie
build_fixture gcc "$c_fixtures_dir/inline-threads.c" "$output_dir/inline-threads-gcc-o2" \
    -O2 -g3 -gdwarf-5 -fomit-frame-pointer -fPIE -pie -pthread
build_fixture clang "$c_fixtures_dir/inline-threads.c" "$output_dir/inline-threads-clang-o2" \
    -O2 -g3 -gdwarf-5 -fomit-frame-pointer -fPIE -pie -pthread

for variant in "gcc-o0 gcc -O0 -fno-omit-frame-pointer -fPIE -pie" \
    "clang-o2 clang -O2 -fomit-frame-pointer -fPIE -pie" \
    "gcc-o2-nopie gcc -O2 -fomit-frame-pointer -no-pie"; do
    read -r name compiler flags <<<"$variant"
    # shellcheck disable=SC2086
    build_fixture "$compiler" "$c_fixtures_dir/watch.c" "$output_dir/watch-${name}" \
        -g3 -gdwarf-5 $flags
    require_instruction "$output_dir/watch-${name}" repeated_store 'rep stos'
    require_instruction "$output_dir/watch-${name}" paired_store 'movdqu'
    require_instruction "$output_dir/watch-${name}" failed_exchange 'lock cmpxchg'
done
for variant in "gcc-o0 gcc -O0 -fno-omit-frame-pointer -fPIE -pie" \
    "clang-o0 clang -O0 -fno-omit-frame-pointer -fPIE -pie" \
    "clang-o2 clang -O2 -fomit-frame-pointer -fPIE -pie" \
    "gcc-o2-nopie gcc -O2 -fomit-frame-pointer -no-pie"; do
    read -r name compiler flags <<<"$variant"
    # shellcheck disable=SC2086
    build_fixture "$compiler" "$c_fixtures_dir/hit-counts.c" "$output_dir/hit-counts-${name}" \
        -g3 -gdwarf-5 $flags
done
for fixture in hit-count-threads hit-count-spin hit-count-signals; do
    build_fixture gcc "$c_fixtures_dir/${fixture}.c" "$output_dir/${fixture}" \
        -O0 -g3 -gdwarf-5 -fno-omit-frame-pointer -fPIE -pie -pthread
done
build_fixture gcc "$c_fixtures_dir/trap-jump.c" "$output_dir/trap-jump" \
    -O0 -g3 -gdwarf-5 -fno-omit-frame-pointer -fPIE -pie -pthread
build_fixture gcc "$c_fixtures_dir/watch-threads.c" "$output_dir/watch-threads" \
    -O0 -g3 -gdwarf-5 -fno-omit-frame-pointer -fPIE -pie -pthread
build_fixture gcc "$c_fixtures_dir/watch-steady.c" "$output_dir/watch-steady" \
    -O0 -g3 -gdwarf-5 -fno-omit-frame-pointer -fPIE -pie -pthread
build_fixture gcc "$c_fixtures_dir/watch-steady.c" "$output_dir/watch-steady-spin" \
    -O0 -g3 -gdwarf-5 -fno-omit-frame-pointer -fPIE -pie -pthread -DSPIN
build_fixture gcc "$c_fixtures_dir/watch-locals.c" "$output_dir/watch-locals-gcc-o0" \
    -O0 -g3 -gdwarf-5 -fno-omit-frame-pointer -fPIE -pie -pthread
build_fixture clang "$c_fixtures_dir/watch-locals.c" "$output_dir/watch-locals-clang-o0" \
    -O0 -g3 -gdwarf-5 -fno-omit-frame-pointer -fPIE -pie -pthread
require_tail_jump "$output_dir/watch-locals-gcc-o0" tail_caller tail_callee
require_tail_jump "$output_dir/watch-locals-clang-o0" tail_caller tail_callee
build_fixture gcc "$c_fixtures_dir/watch-slot-thief.c" "$output_dir/watch-slot-thief-main" \
    -O0 -g3 -gdwarf-5 -fPIE -pie -pthread -DSTOLEN_SLOTS=3
build_fixture gcc "$c_fixtures_dir/watch-slot-thief.c" "$output_dir/watch-slot-thief-worker" \
    -O0 -g3 -gdwarf-5 -fPIE -pie -pthread -DSTOLEN_SLOTS=4 -DSTOLEN_BY_WORKER
build_fixture gcc "$c_fixtures_dir/watch-attach.c" "$output_dir/watch-attach" \
    -O0 -g3 -gdwarf-5 -fPIE -pie -pthread
build_fixture gcc "$c_fixtures_dir/watch-orphaner.c" "$output_dir/watch-orphaner" -O0 -g
build_rust_fixture "$rust_fixtures_dir/watch.rs" "$output_dir/watch-rust-o0" \
    -C opt-level=0 -C force-frame-pointers=yes
build_rust_fixture "$rust_fixtures_dir/watch.rs" "$output_dir/watch-rust-o2" \
    -C opt-level=2 -C force-frame-pointers=no
build_zig_fixture "$zig_fixtures_dir/watch.zig" "$output_dir/watch-zig-o0" \
    -O Debug -fPIE -fno-omit-frame-pointer
build_zig_fixture "$zig_fixtures_dir/watch.zig" "$output_dir/watch-zig-o2" \
    -O ReleaseFast -fPIE -fomit-frame-pointer
build_go_fixture "$go_fixtures_dir/watch" "$output_dir/watch-go-o0" \
    -buildmode=pie "-gcflags=all=-N -l"
build_go_fixture "$go_fixtures_dir/frames" "$output_dir/frames-go-o2" \
    -buildmode=pie

build_shared_fixture gcc "$c_fixtures_dir/crash/library.c" "$output_dir/libcrash.so" \
    -O0 -g3 -gdwarf-5 -fno-omit-frame-pointer -Wl,--build-id
build_shared_fixture gcc "$c_fixtures_dir/crash/library.c" "$output_dir/libcrash-rebuilt.so" \
    -O0 -g3 -gdwarf-5 -fno-omit-frame-pointer -Wl,--build-id -DCRASH_REBUILT
build_fixture gcc "$c_fixtures_dir/crash/main.c" "$output_dir/crash-gcc-o0" \
    -O0 -g3 -gdwarf-5 -fno-omit-frame-pointer -fPIE -pie -pthread -Wl,--build-id \
    "-L$output_dir" -lcrash '-Wl,-rpath,$ORIGIN'
build_fixture gcc "$c_fixtures_dir/crash/main.c" "$output_dir/crash-gcc-o0-rebuilt" \
    -O0 -g3 -gdwarf-5 -fno-omit-frame-pointer -fPIE -pie -pthread -Wl,--build-id \
    -DCRASH_REBUILT "-L$output_dir" -lcrash '-Wl,-rpath,$ORIGIN'
build_fixture clang "$c_fixtures_dir/crash/main.c" "$output_dir/crash-clang-o2" \
    -O2 -g3 -gdwarf-5 -fomit-frame-pointer -fPIE -pie -pthread \
    "-L$output_dir" -lcrash '-Wl,-rpath,$ORIGIN'
require_dwarf_operation "$output_dir/crash-clang-o2" 'DW_OP_reg17 (xmm0)'
build_fixture gcc "$c_fixtures_dir/crash/main.c" "$output_dir/crash-gcc-o2-nopie" \
    -O2 -g3 -gdwarf-5 -fomit-frame-pointer -no-pie -pthread \
    "-L$output_dir" -lcrash '-Wl,-rpath,$ORIGIN'
build_rust_fixture "$rust_fixtures_dir/crash.rs" "$output_dir/crash-rust-o0" \
    -C opt-level=0 -C force-frame-pointers=yes


build_rust_fixture "$rust_fixtures_dir/crash.rs" "$output_dir/crash-rust-nodebug" \
    -C opt-level=0 -C debuginfo=0 -C strip=debuginfo
build_go_fixture "$go_fixtures_dir/preempt" "$output_dir/preempt-go" \
    -buildmode=pie


build_go_fixture "$go_fixtures_dir/workers" "$output_dir/workers-go-o0" \
    -buildmode=pie "-gcflags=all=-N -l"
build_go_fixture "$go_fixtures_dir/workers" "$output_dir/workers-go-o2"


build_go_fixture "$go_fixtures_dir/corrupt" "$output_dir/corrupt-go"
build_go_fixture "$go_fixtures_dir/scale" "$output_dir/scale-go"

build_go_command cmd/gofmt "$output_dir/gofmt-go-o0" -buildmode=pie "-gcflags=all=-N -l"
build_go_command cmd/gofmt "$output_dir/gofmt-go-o2"
build_go_fixture "$go_fixtures_dir/stacks" "$output_dir/stacks-go-o0" \
    -buildmode=pie "-gcflags=all=-N -l"
build_go_fixture "$go_fixtures_dir/stacks" "$output_dir/stacks-go-o2"
build_go_fixture "$go_fixtures_dir/spin" "$output_dir/spin-go-o0" \
    "-gcflags=all=-N -l"
build_go_fixture "$go_fixtures_dir/spin" "$output_dir/spin-go-o2"

build_go_fixture "$go_fixtures_dir/waits" "$output_dir/profile-go-waits"

build_go_fixture "$go_fixtures_dir/profile" "$output_dir/profile-go-o2"
build_go_fixture "$go_fixtures_dir/overhead" "$output_dir/overhead-go"
build_go_fixture "$go_fixtures_dir/profile" "$output_dir/profile-go-o0" \
    -buildmode=pie "-gcflags=all=-N -l"

build_go_fixture "$go_fixtures_dir/spin" "$output_dir/spin-go-stripped" "-ldflags=-s -w"
build_go_fixture "$go_fixtures_dir/siblings" "$output_dir/siblings-go-o0" \
    -buildmode=pie "-gcflags=all=-N -l"
build_go_fixture "$go_fixtures_dir/siblings" "$output_dir/siblings-go-o2"
build_go_fixture "$go_fixtures_dir/watched" "$output_dir/watched-go-o0" \
    -buildmode=pie "-gcflags=all=-N -l"
build_go_fixture "$go_fixtures_dir/watched" "$output_dir/watched-go-o2"

GO_CGO=1 GO_CC=gcc GO_CFLAGS="-g -O0" build_go_fixture "$go_fixtures_dir/cgo" \
    "$output_dir/cgo-go-gcc" -buildmode=pie "-gcflags=all=-N -l"
GO_CGO=1 GO_CC=clang GO_CFLAGS="-g -O2" build_go_fixture "$go_fixtures_dir/cgo" \
    "$output_dir/cgo-go-clang"


build_go_fixture "$go_fixtures_dir/server" "$output_dir/server-go-o0" \
    -buildmode=pie "-gcflags=all=-N -l"
build_go_fixture "$go_fixtures_dir/server" "$output_dir/server-go-trimpath" -trimpath

build_go_fixture "$go_fixtures_dir/served" "$output_dir/served-go" -buildmode=pie

GO_CGO=1 GO_CC=gcc GO_CFLAGS="-g -O0" build_go_fixture "$go_fixtures_dir/hosted" \
    "$output_dir/libgo-hosted.so" -buildmode=c-shared "-gcflags=all=-N -l"
build_fixture gcc "$c_fixtures_dir/go-host/main.c" "$output_dir/go-host" \
    -O0 -g3 -gdwarf-5 -fPIE -pie "-L$output_dir" -lgo-hosted '-Wl,-rpath,$ORIGIN'
build_go_fixture "$go_fixtures_dir/torture" "$output_dir/torture-go-o0" \
    -buildmode=pie "-gcflags=all=-N -l"
build_go_fixture "$go_fixtures_dir/torture" "$output_dir/torture-go-o2"
build_go_fixture "$go_fixtures_dir/growing" "$output_dir/growing-go" \
    -buildmode=pie "-gcflags=all=-N -l"
build_go_fixture "$go_fixtures_dir/steps" "$output_dir/steps-go-o0" \
    -buildmode=pie "-gcflags=all=-N -l"
build_go_fixture "$go_fixtures_dir/steps" "$output_dir/steps-go-o2"
build_go_fixture "$go_fixtures_dir/defers" "$output_dir/defers-go-o0" \
    -buildmode=pie "-gcflags=all=-N -l"
build_go_fixture "$go_fixtures_dir/defers" "$output_dir/defers-go-o2"
build_go_fixture "$go_fixtures_dir/failing" "$output_dir/failing-go-o0" \
    -buildmode=pie "-gcflags=all=-N -l"
build_go_fixture "$go_fixtures_dir/failing" "$output_dir/failing-go-o2"
build_go_fixture "$go_fixtures_dir/ranges" "$output_dir/ranges-go-o0" \
    -buildmode=pie "-gcflags=all=-N -l"
build_go_fixture "$go_fixtures_dir/ranges" "$output_dir/ranges-go-o2"
build_go_fixture "$go_fixtures_dir/crash" "$output_dir/crash-go-o0" \
    -buildmode=pie "-gcflags=all=-N -l"
build_go_fixture "$go_fixtures_dir/panic" "$output_dir/panic-go-o0" \
    -buildmode=pie "-gcflags=all=-N -l"
build_go_fixture "$go_fixtures_dir/panic" "$output_dir/panic-go-o2"
build_go_fixture "$go_fixtures_dir/crash" "$output_dir/crash-go-nodwarf" \
    -buildmode=pie "-gcflags=all=-N -l" -ldflags=-w
build_zig_fixture "$zig_fixtures_dir/crash.zig" "$output_dir/crash-zig-o0" \
    -O Debug -fPIE -fno-omit-frame-pointer



build_go_fixture "$go_fixtures_dir/callers" "$output_dir/callers-go"
build_go_fixture "$go_fixtures_dir/callers" "$output_dir/callers-go-stripped" \
    -buildmode=pie "-ldflags=-s -w"
GO_CGO=1 build_go_fixture "$go_fixtures_dir/callers" "$output_dir/callers-go-external-stripped" \
    "-ldflags=-linkmode=external -s -w"
build_fixture gcc "$c_fixtures_dir/vdso.c" "$output_dir/vdso-gcc-o0" \
    -O0 -g3 -gdwarf-5 -fno-omit-frame-pointer -fPIE -pie
build_fixture gcc "$c_fixtures_dir/vdso.c" "$output_dir/vdso-gcc-o2" \
    -O2 -g3 -gdwarf-5 -fomit-frame-pointer -fPIE -pie
build_fixture clang "$c_fixtures_dir/vdso.c" "$output_dir/vdso-clang-o2-nopie" \
    -O2 -g3 -gdwarf-5 -fomit-frame-pointer -no-pie

for variant in gcc-o0 clang-o2 gcc-o2-nopie; do
    program="$output_dir/crash-${variant}"
    inputs="$program $output_dir/libcrash.so"
    generate_core "$output_dir/crash-${variant}-segv.core" 11 "$default_core_filter" \
        "$inputs" "$program" segv
    generate_core "$output_dir/crash-${variant}-abort.core" 6 "$default_core_filter" \
        "$inputs" "$program" abort
done




if [[ -z "${MICROSCOPE_CPYTHON-}" ]]; then
    printf 'error: MICROSCOPE_CPYTHON is unset; build inside the Nix shell\n' >&2
    exit 1
fi



for interpreter in 3.12 3.14 3.14t 3.15; do
    mkdir -p "$output_dir/python-extension-${interpreter}"
    build_program gcc "$fixtures_dir/python/extension.c" \
        "$output_dir/python-extension-${interpreter}/microscope_extension.so" \
        -std=c17 -Wall -Wextra -Werror -shared -fPIC -O0 -g \
        "-I$MICROSCOPE_CPYTHON/$interpreter/include"

    build_program gcc "$fixtures_dir/python/extension.c" \
        "$output_dir/python-extension-${interpreter}/microscope_bare.so" \
        -std=c17 -Wall -Wextra -Werror -shared -fPIC -O0 -g0 -s -DMODULE=microscope_bare \
        "-I$MICROSCOPE_CPYTHON/$interpreter/include"
done
for interpreter in 3.13 3.14t 3.15 debian-3.13; do
    python="$MICROSCOPE_CPYTHON/$interpreter/python"
    for fixture in stacks locals; do
        generate_core "$output_dir/python-${fixture}-${interpreter}.core" 6 "$default_core_filter" \
            "$fixtures_dir/python/${fixture}.py $fixtures_dir/python/truth.py $python" \
            "$python" -I -B "$fixtures_dir/python/${fixture}.py" --abort
    done
done




if [[ -z "${MICROSCOPE_NODE-}" ]]; then
    printf 'error: MICROSCOPE_NODE is unset; build inside the Nix shell\n' >&2
    exit 1
fi


build_program gcc "$fixtures_dir/node/addon.c" "$output_dir/node-addon.node" \
    -std=c17 -Wall -Wextra -Werror -shared -fPIC -O0 -g "-I$MICROSCOPE_NODE/26/include"


build_program gcc "$fixtures_dir/node/frameless.c" "$output_dir/node-frameless.node" \
    -std=c17 -Wall -Wextra -Werror -shared -fPIC -O2 -g -fomit-frame-pointer -mno-red-zone \
    "-I$MICROSCOPE_NODE/26/include"

build_program gcc sdk/node/microscope.c "$output_dir/node-microscope.node" \
    -std=c17 -Wall -Wextra -Werror -shared -fPIC -O2 -g "-I$MICROSCOPE_NODE/26/include" -ldl -pthread
build_program gcc "$fixtures_dir/node/inspector-lifetime.c" "$output_dir/node-inspector-lifetime.node" \
    -std=c17 -Wall -Wextra -Werror -shared -fPIC -O0 -g -pthread "-I$MICROSCOPE_NODE/26/include"




for entry_version in "26 14.6" "nixpkgs-26 14.6" "24 13.6" "nixpkgs-24 13.6" "debian-24 13.6" \
    "22 12.4" "nixpkgs-22 12.4"; do
    read -r entry version <<<"$entry_version"
    build_program g++ "$fixtures_dir/node/fast.cc" "$output_dir/node-fast-${entry}.node" \
        -std=c++20 -Wall -Wextra -Werror -shared -fPIC -O1 -g \
        -isystem "$MICROSCOPE_NODE/$entry/include" \
        -idirafter "$MICROSCOPE_V8_TORQUE/$version/v8/include"
done
for entry in 26 24 22; do
    node="$MICROSCOPE_NODE/$entry/node"
    for fixture in stacks tiers awaits; do
        flags=()
        if [[ "$fixture" == tiers ]]; then
            flags=(--allow-natives-syntax)
        fi
        generate_core "$output_dir/node-${fixture}-${entry}.core" 6 "$default_core_filter" \
            "$fixtures_dir/node/${fixture}.js $fixtures_dir/node/truth.js $node" \
            env -i "$node" "${flags[@]}" "$fixtures_dir/node/${fixture}.js" --abort
    done


    fifo="$output_dir/node-tasks.fifo"
    if [[ ! -p "$fifo" ]]; then
        rm -f "$fifo"
        mkfifo "$fifo"
    fi
    generate_core "$output_dir/node-tasks-${entry}.core" 6 "$default_core_filter" \
        "$fixtures_dir/node/tasks.js $fixtures_dir/node/truth.js $node" \
        env -i "$node" "$fixtures_dir/node/tasks.js" "$fifo" --abort
done

generate_core "$output_dir/code-copy.core" 6 "$default_core_filter" \
    "$output_dir/code-copy" "$output_dir/code-copy" abort
generate_core "$output_dir/crash-gcc-o0-headerless.core" 11 "$headerless_core_filter" \
    "$output_dir/crash-gcc-o0 $output_dir/libcrash.so" "$output_dir/crash-gcc-o0" segv
generate_core "$output_dir/crash-gcc-o0-headers-only.core" 11 "$headers_only_core_filter" \
    "$output_dir/crash-gcc-o0 $output_dir/libcrash.so" "$output_dir/crash-gcc-o0" segv
generate_core "$output_dir/crash-gcc-o0-memoryless.core" 11 "$memoryless_core_filter" \
    "$output_dir/crash-gcc-o0 $output_dir/libcrash.so" "$output_dir/crash-gcc-o0" segv
for language in rust go zig; do
    generate_core "$output_dir/crash-${language}-o0.core" 11 "$default_core_filter" \
        "$output_dir/crash-${language}-o0" "$output_dir/crash-${language}-o0"
done
for variant in gcc-o0 gcc-o2 gcc-o2-nopie clang-o0 clang-o2; do
    program="$output_dir/frames-${variant}"
    generate_core "${program}.core" 11 "$default_core_filter" "$program" "$program" crash
done
program="$output_dir/tls-modules-musl-gcc-o0"
generate_core "${program}.core" 6 "$default_core_filter" \
    "$program $output_dir/libtls-modules-musl.so $output_dir/libtls-plugin-musl.so" \
    "$program" abort
for variant in musl-clang-static-pie gcc-static single-thread-clang-static; do
    program="$output_dir/tls-modules-${variant}"
    generate_core "${program}.core" 6 "$default_core_filter" "$program" "$program" abort
done


for version in 2.27 2.31 2.35; do
    program="$output_dir/tls-modules-glibc${version}"
    generate_core "${program}.core" 6 "$default_core_filter" \
        "$program $output_dir/libtls-modules-glibc${version}.so $output_dir/libtls-plugin-glibc${version}.so" \
        "$program" abort
done
for variant in gcc-o2 gcc-o2-nopie clang-o2; do
    program="$output_dir/locations-${variant}"
    generate_core "${program}.core" 6 "$default_core_filter" \
        "$program $output_dir/liblocations.so" "$program" abort
done


generate_core_without() {
    wait_builds
    local name="$1"
    local deleted="$2"
    local directory="$output_dir/core-missing-${name}"
    local core="$directory/crash.core"
    local inputs="$output_dir/crash-gcc-o0 $output_dir/libcrash.so"
    mkdir -p "$directory"
    if ! core_is_current "$core" "$(core_signature 11 "$default_core_filter" \
        "$inputs" "$directory/crash-gcc-o0" segv)"; then
        cp "$output_dir/crash-gcc-o0" "$output_dir/libcrash.so" "$directory/"
    fi
    generate_core "$core" 11 "$default_core_filter" "$inputs" "$directory/crash-gcc-o0" segv
    wait_for "$core"
    rm -f "$directory/$deleted"
}
generate_core_without library libcrash.so
generate_core_without executable crash-gcc-o0





wait_builds
toolchain_libc=$(ldd "$output_dir/crash-gcc-o0" | awk '$1 == "libc.so.6" { print $3 }')
derive_foreign_libc "$toolchain_libc" "$output_dir/libc-foreign.so.6"
generate_foreign_core() {
    wait_builds
    local directory="$output_dir/core-foreign"
    local core="$directory/crash.core"
    local inputs="$output_dir/crash-gcc-o0 $output_dir/libcrash.so $output_dir/libc-foreign.so.6"
    mkdir -p "$directory"
    if ! core_is_current "$core" "$(core_signature 11 "$default_core_filter" \
        "$inputs" "$directory/crash-gcc-o0" segv)"; then
        cp "$output_dir/crash-gcc-o0" "$output_dir/libcrash.so" "$directory/"
        cp "$output_dir/libc-foreign.so.6" "$directory/libc.so.6"
    fi
    generate_core "$core" 11 "$default_core_filter" "$inputs" "$directory/crash-gcc-o0" segv
    wait_for "$core"
    cp "$output_dir/crash-gcc-o0-rebuilt" "$directory/crash-gcc-o0"
    cp "$output_dir/libcrash-rebuilt.so" "$directory/libcrash.so"
    rm -f "$directory/libc.so.6"
}
generate_foreign_core



generate_core "$output_dir/crash-rust-nodebug.core" 11 "$default_core_filter" \
    "$output_dir/crash-rust-nodebug" "$output_dir/crash-rust-nodebug"


for variant in o0 o2; do
    program="$output_dir/panic-go-${variant}"
    generate_core "${program}.core" 6 "$default_core_filter" "$program" \
        env GOTRACEBACK=crash "$program"
done
generate_core "$output_dir/crash-go-nodwarf.core" 11 "$headers_only_core_filter" \
    "$output_dir/crash-go-nodwarf" "$output_dir/crash-go-nodwarf"

generate_core "$output_dir/null-call.core" 11 "$default_core_filter" \
    "$output_dir/null-call" "$output_dir/null-call"


readonly vdso_variants=(gcc-o0 gcc-o2 clang-o2-nopie)
for variant in "${vdso_variants[@]}"; do
    program="$output_dir/vdso-${variant}"
    for mode in clock time; do
        generate_core "${program}-${mode}.core" 11 "$default_core_filter" "$program" \
            "$program" "$mode"
    done
done



readonly symbol_oracle_dir="$output_dir/symbol-oracles"
mkdir -p "$symbol_oracle_dir"




generate_symbol_oracle() {
    wait_builds
    local elf="$1"
    local oracle="$symbol_oracle_dir/${2:-${elf##*/}}.readelf"
    local frames="${3:-yes}"

    wait_for "$oracle"


    local header
    header="microscope-symbol-oracle-v4 $(readlink -f "$elf")"

    rebuilt_outputs["$oracle"]=false
    if [[ -s "$oracle" && "$oracle" -nt "$elf" && "$(head -n 1 "$oracle")" == "$header" ]]; then
        printf '[cached] %s\n' "$oracle"
        return
    fi
    printf '[oracle] %s\n' "$oracle"
    spawn_job leaf "$oracle" make_symbol_oracle "$elf" "$oracle" "$frames" "$header"
}

make_symbol_oracle() {
    local elf="$1"
    local oracle="$2"
    local frames="$3"
    local header="$4"
    {
        printf '%s\n' "$header"
        readelf -SW "$elf"
        readelf -sW "$elf"
        if [[ "$frames" == yes ]]; then
            readelf -wf "$elf"
            objdump -d -j .plt -j .plt.sec -j .plt.got "$elf" 2>/dev/null | grep '@plt>:$' || true
        fi
    } >"${oracle}.tmp"
    mv "${oracle}.tmp" "$oracle"
}


generate_backtrace_oracle() {
    wait_builds
    local program="$1"
    local core="$2"
    wait_for "$core"
    local oracle="${core}.gdb-backtrace"
    rebuilt_outputs["$oracle"]=false
    if [[ -s "$oracle" && "$oracle" -nt "$core" ]]; then
        printf '[cached] %s\n' "$oracle"
        return
    fi
    printf '[oracle] %s\n' "$oracle"
    spawn_job leaf "$oracle" make_backtrace_oracle "$program" "$core" "$oracle"
}

make_backtrace_oracle() {
    local program="$1"
    local core="$2"
    local oracle="$3"
    gdb -nx -batch -q \
        -iex 'set auto-load off' \
        -iex 'set debuginfod enabled off' \
        -ex 'set backtrace past-main on' \
        -ex 'set backtrace past-entry on' \
        -ex 'bt' \
        "$program" "$core" 2>&1 \
        | awk '/^#0 / { count = 0 } /^#/ { lines[count++] = $0 }
               END { for (i = 0; i < count; i++) print lines[i] }' >"${oracle}.tmp"
    mv "${oracle}.tmp" "$oracle"
}


generate_frame_oracle() {
    wait_builds
    local program="$1"
    local core="$2"
    wait_for "$core"
    local oracle="${core}.gdb-frame-variables"
    rebuilt_outputs["$oracle"]=false
    if [[ -s "$oracle" && "$oracle" -nt "$core" && "$oracle" -nt "$frame_oracle_script" ]]; then
        printf '[cached] %s\n' "$oracle"
        return
    fi
    printf '[oracle] %s\n' "$oracle"
    spawn_job leaf "$oracle" make_frame_oracle "$program" "$core" "$oracle"
}

make_frame_oracle() {
    local program="$1"
    local core="$2"
    local oracle="$3"
    local log
    if ! log=$(MICROSCOPE_FRAME_ORACLE="${oracle}.tmp" gdb -nx -batch -q \
        -iex 'set auto-load off' \
        -iex 'set debuginfod enabled off' \
        -x "$frame_oracle_script" \
        "$program" "$core" 2>&1) || [[ ! -s "${oracle}.tmp" ]]; then
        printf 'error: gdb did not record frame variables for %s:\n%s\n' "$core" "$log" >&2
        rm -f "${oracle}.tmp"
        exit 1
    fi
    mv "${oracle}.tmp" "$oracle"
}

for variant in gcc-o0 gcc-o2 gcc-o2-nopie clang-o0 clang-o2; do
    generate_frame_oracle "$output_dir/frames-${variant}" "$output_dir/frames-${variant}.core"
done
for variant in gcc-o2 gcc-o2-nopie clang-o2; do
    generate_frame_oracle "$output_dir/locations-${variant}" \
        "$output_dir/locations-${variant}.core"
done
for variant in gcc-o0 clang-o2 gcc-o2-nopie; do
    for kind in segv abort; do
        generate_frame_oracle "$output_dir/crash-${variant}" \
            "$output_dir/crash-${variant}-${kind}.core"
    done
done
for language in rust go zig; do
    generate_frame_oracle "$output_dir/crash-${language}-o0" "$output_dir/crash-${language}-o0.core"
done
for variant in "${vdso_variants[@]}"; do
    for mode in clock time; do
        program="$output_dir/vdso-${variant}"
        generate_backtrace_oracle "$program" "${program}-${mode}.core"
        generate_frame_oracle "$program" "${program}-${mode}.core"
    done
done

for library in gcc clang stripped minidebug; do
    generate_symbol_oracle "$output_dir/libelf-symbols-${library}.so"
done
generate_symbol_oracle "$output_dir/libelf-symbols-minidebug.so.embedded" \
    libelf-symbols-minidebug.so.embedded no

generate_symbol_oracle "$output_dir/libelf-symbols-stripped.so.full"
for variant in "${symbols_variants[@]}"; do
    read -r name _ <<<"$variant"
    generate_symbol_oracle "$output_dir/elf-symbols-${name}"
done

wait_builds
while read -r library; do
    generate_symbol_oracle "$library"
done < <(ldd "$output_dir/elf-symbols-gcc-o0" | awk '/=> \// { print $3 } /^\t\// { print $1 }' \
    | grep -E '/(libc\.so|ld-linux)')
for variant in "${symbols_variants[@]}"; do
    read -r name library _ <<<"$variant"
    program="$output_dir/elf-symbols-${name}"
    generate_core "${program}.core" 4 "$default_core_filter" \
        "$program $output_dir/libelf-symbols-${library}.so" "$program"
    generate_backtrace_oracle "$program" "${program}.core"
done



generate_function_type_oracle() {
    wait_builds
    local program="$1"
    local oracle="${program}.gdb-function-types"
    rebuilt_outputs["$oracle"]=false
    if [[ -s "$oracle" && "$oracle" -nt "$program" ]]; then
        printf '[cached] %s\n' "$oracle"
        return
    fi
    printf '[oracle] %s\n' "$oracle"
    local -a names=(unary unary_pointer operation 'operations[0]' no_arguments with_variadic
        chooser unprototyped handlers.on_event handlers.on_done namer constant_function
        null_function)
    local name
    for name in "${names[@]}"; do
        gdb -nx -batch -q -iex 'set auto-load off' -iex 'set debuginfod enabled off' \
            -ex "whatis $name" -ex "print $name" "$program" 2>&1 \
            | awk -v name="$name" '
                /^type = / { type = substr($0, 8) }
                /^\$1 = / { target = ""; if (match($0, /<[^>]*>$/)) {
                    target = substr($0, RSTART + 1, RLENGTH - 2) } }
                END { printf "%s\t%s\t%s\n", name, type, target }'
    done >"${oracle}.tmp"
    mv "${oracle}.tmp" "$oracle"
}
for compiler in gcc clang; do
    generate_function_type_oracle "$output_dir/function-types-${compiler}-o0"
done



readonly gosym_oracle="$output_dir/gosym-oracle"
build_go_fixture scripts/gosym-oracle "$gosym_oracle"
generate_gosym_oracle() {
    wait_builds
    local program="$1"
    local oracle="${program}.gosym"
    rebuilt_outputs["$oracle"]=false
    if [[ -s "$oracle" && "$oracle" -nt "$program" && "$oracle" -nt "$gosym_oracle" ]]; then
        printf '[cached] %s\n' "$oracle"
        return
    fi
    printf '[oracle] %s\n' "$oracle"
    "$gosym_oracle" "$program" >"${oracle}.tmp"
    mv "${oracle}.tmp" "$oracle"
}
generate_gosym_oracle "$output_dir/callers-go"
generate_gosym_oracle "$output_dir/callers-go-stripped"


build_odin_fixture "$odin_fixtures_dir/values.odin" "$output_dir/values-odin-o0" -o:none
build_odin_fixture "$odin_fixtures_dir/values.odin" "$output_dir/values-odin-o2" -o:speed
build_odin_fixture "$odin_fixtures_dir/containers.odin" "$output_dir/containers-odin-o0" -o:none
build_odin_fixture "$odin_fixtures_dir/containers.odin" "$output_dir/containers-odin-o2" -o:speed


build_fortran_fixture "$fortran_fixtures_dir/values.f90" "$output_dir/values-fortran-o0" -O0
build_fortran_fixture "$fortran_fixtures_dir/values.f90" "$output_dir/values-fortran-o2" -O2


build_d_fixture "$d_fixtures_dir/values.d" "$output_dir/values-d-o0" -O0
build_d_fixture "$d_fixtures_dir/values.d" "$output_dir/values-d-o2" -O2
build_d_fixture "$d_fixtures_dir/containers.d" "$output_dir/containers-d-o0" -O0
build_d_fixture "$d_fixtures_dir/containers.d" "$output_dir/containers-d-o2" -O2


build_nim_fixture "$nim_fixtures_dir/values.nim" "$output_dir/values-nim-gcc-o0" --cc:gcc --opt:none
build_nim_fixture "$nim_fixtures_dir/values.nim" "$output_dir/values-nim-clang-o2" --cc:clang \
    --opt:speed


build_ada_fixture "$ada_fixtures_dir/values.adb" "$output_dir/values-ada-o0" -O0
build_ada_fixture "$ada_fixtures_dir/values.adb" "$output_dir/values-ada-o2" -O2




readonly disassembly_oracle_dir="$output_dir/disassembly-oracles"
mkdir -p "$disassembly_oracle_dir"

generate_disassembly_oracle() {
    wait_builds
    local elf="$1"
    local oracle="$disassembly_oracle_dir/${elf##*/}.objdump"
    local header
    header="microscope-disassembly-oracle-v1 $(readlink -f "$elf")"
    rebuilt_outputs["$oracle"]=false
    if [[ -s "$oracle" && "$oracle" -nt "$elf" && "$(head -n 1 "$oracle")" == "$header" ]]; then
        printf '[cached] %s\n' "$oracle"
        return
    fi
    printf '[oracle] %s\n' "$oracle"
    {
        printf '%s\n' "$header"
        objdump -d -z -w "$elf"
    } >"${oracle}.tmp"
    mv "${oracle}.tmp" "$oracle"
}

for program in crash-gcc-o0 crash-gcc-o2-nopie crash-clang-o2 crash-rust-o0 crash-zig-o0 \
    libcrash.so libelf-symbols-gcc.so libelf-symbols-stripped.so; do
    generate_disassembly_oracle "$output_dir/$program"
done

wait_jobs
if ! wait "$tokio_build"; then
    printf 'error: could not make the tokio fixtures\n' >&2
    exit 1
fi
while IFS= read -r output; do
    rebuilt_outputs["$output"]=true
done <"$tokio_outputs"
printf '%s\n' "${!rebuilt_outputs[@]}" >"${suite_outputs}.tmp"
mv "${suite_outputs}.tmp" "$suite_outputs"
mv "${suite_stamp}.tmp" "$suite_stamp"
