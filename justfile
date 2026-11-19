set shell := ["scripts/hermetic.sh", "bash", "--norc", "-euo", "pipefail", "-c"]
set script-interpreter := ["scripts/hermetic.sh", "bash", "--norc", "-euo", "pipefail"]
set positional-arguments := true

max_test_threads := "16"

test_features := "tools,ring-lease,bpf-faults"

decoding_fuzz_targets := "disassembly dispatch elf-symbols dwarf-load jit-code runtime-go runtime-tokio runtime-python runtime-node"

js_files := "web/src web/test web/e2e web/*.ts web/*.json editors/vscode views/visualizers"

default: check

check: lint test miri loom web-test docs

all: check props fuzz-replay (miri-seeds "0..64") (loom "4") loom-mutants web-props web-e2e stress sim bench-smoke (audit-bin "debug") test-bpf-if-installed

[script("bash", "-euo", "pipefail")]
dev *ARGS="":
    exec ./scripts/dev.sh "$@"

[script("bash")]
install-vscode-symlink:
    ln -s "$PWD/editors/vscode" ~/.vscode/extensions/microscope.microscope-0.1.0

build-test-programs: golden
    ./scripts/build-test-programs.sh

golden:
    ./scripts/golden.sh build

build *ARGS="": build-test-programs
    cargo build --profile test {{ARGS}}

build-debug *ARGS="":
    cargo build --profile test --config 'profile.test.package.microscope.debug=2' {{ARGS}}

[script]
build-static PROFILE="debug" *ARGS: web-build
    unset RUSTFLAGS
    cargo auditable build --target x86_64-unknown-linux-musl --bin microscope \
        --profile "$([[ "$1" == debug ]] && echo dev || echo "$1")" "${@:2}"
    binary="target/x86_64-unknown-linux-musl/$1/microscope"
    if readelf -lW "$binary" | grep -q INTERP || readelf -dW "$binary" | grep -q NEEDED; then
        echo "error: $binary links dynamically" >&2
        exit 1
    fi
    echo "$binary (static, musl $MICROSCOPE_MUSL_VERSION)"

audit-bin PROFILE="release": (build-static PROFILE)
    cargo audit --db "$(./scripts/advisory-db.sh)" --no-fetch --deny warnings bin "target/x86_64-unknown-linux-musl/$1/microscope"

run *ARGS: build
    ./target/debug/microscope "$@"

lint: deny vet
    ./scripts/unsafe-sites.sh
    ./scripts/test-sleeps.sh
    cargo fmt --check
    cargo clippy --all-targets --all-features -- -D warnings
    CARGO_PROFILE_RELEASE_INCREMENTAL=true cargo clippy --release --all-targets --all-features -- -D warnings
    cargo check --quiet --manifest-path fuzz/Cargo.toml
    cargo fmt --check --manifest-path sdk/rust-probes/Cargo.toml
    CARGO_TARGET_DIR="$PWD/target/sdk-probes" cargo clippy --quiet --manifest-path sdk/rust-probes/Cargo.toml --all-targets --features c -- -D warnings
    cargo fmt --check --manifest-path loom/Cargo.toml
    CARGO_TARGET_DIR="$PWD/target/loom" cargo clippy --quiet --manifest-path loom/Cargo.toml --all-targets -- -D warnings

[script]
deny *ARGS:
    ./scripts/advisory-db.sh >/dev/null
    cargo deny --locked check --config deny.toml --disable-fetch -D warnings "$@"
    for manifest in fuzz/Cargo.toml loom/Cargo.toml sdk/rust/Cargo.toml sdk/rust-probes/Cargo.toml; do
        cargo deny --locked --manifest-path "$manifest" check --config deny.toml --disable-fetch -D warnings \
            --allow advisory-not-detected --allow unmatched-skip --allow unnecessary-skip "$@"
    done

vet:
    cargo vet --locked

[doc("Builds the native test fixtures and runs the Rust test suite.")]
test *ARGS: cache-warm
    ulimit -c 0; test_threads="$(nproc)"; if (( test_threads > {{max_test_threads}} )); then test_threads={{max_test_threads}}; fi; MIMALLOC_ALLOW_THP=0 MICROSCOPE_CACHE_DIR="$PWD/target/image-cache" XDG_CONFIG_HOME="$PWD/target/test-config" ./scripts/contained.sh setarch "$(uname -m)" cargo nextest run --features {{test_features}} --test-threads "$test_threads" "$@"
    if (( $# == 0 )); then CARGO_TARGET_DIR="$PWD/target/sdk-probes" cargo test --quiet --manifest-path sdk/rust-probes/Cargo.toml --features c; fi

[doc("Runs the unit tests with CASES cases for each property test.")]
props CASES="10000" *ARGS: cache-warm
    ulimit -c 0; test_threads="$(nproc)"; if (( test_threads > {{max_test_threads}} )); then test_threads={{max_test_threads}}; fi; PROPTEST_CASES="$1" MICROSCOPE_CACHE_DIR="$PWD/target/image-cache" XDG_CONFIG_HOME="$PWD/target/test-config" ./scripts/contained.sh setarch "$(uname -m)" cargo nextest run --features {{test_features}} --profile props --test-threads "$test_threads" --lib --bins "${@:2}"

[doc("Runs the BPF engine's tests through the microscope-bpf wrapper.")]
[script]
test-bpf LAUNCHER="/run/wrappers/bin/microscope-bpf" *ARGS: build-test-programs
    if [[ ! -x "$1" ]]; then
        echo "error: $1 is missing. The BPF engine's tests need CAP_BPF and CAP_PERFMON:" >&2
        echo "  import microscope's nixosModules.microscope-bpf, rebuild, and run this again;" >&2
        echo "  or pass a launcher that grants them: just test-bpf LAUNCHER" >&2
        exit 1
    fi
    ulimit -c 0
    MICROSCOPE_BPF_LAUNCHER="$1" MICROSCOPE_CACHE_DIR="$PWD/target/image-cache" XDG_CONFIG_HOME="$PWD/target/test-config" ./scripts/contained.sh setarch "$(uname -m)" cargo nextest run --features {{test_features}} --run-ignored only --test syscalls --test python --test node --test profiler_kernel -E 'test(/(^|::)bpf::/) | binary(profiler_kernel)' "${@:2}"

[script]
test-bpf-if-installed LAUNCHER="/run/wrappers/bin/microscope-bpf":
    if [[ ! -x "$1" ]]; then
        printf '\n%s\n%s\n%s\n\n' \
            "!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!" \
            "WARNING: the BPF engine's tests were SKIPPED: $1 is missing. Import nixosModules.microscope-bpf to run them." \
            "!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!" >&2
        exit 0
    fi
    {{just_executable()}} test-bpf "$1"

miri_seeds := "0..8"

[script]
miri *ARGS: && (miri-seeds miri_seeds ARGS)
    export MIRI_SYSROOT="$PWD/target/miri/sysroot"
    cargo miri setup >/dev/null
    MIRIFLAGS="-Zmiri-strict-provenance" cargo miri test --quiet --test miri --test miri_global_allocator -- --skip concurrent "$@"
    export CARGO_TARGET_DIR="$PWD/target/sdk-probes"
    MIRIFLAGS="-Zmiri-strict-provenance" cargo miri test --quiet --manifest-path sdk/rust-probes/Cargo.toml --features c --lib -- --skip concurrent "$@"

[script]
miri-seeds SEEDS *ARGS:
    export MIRI_SYSROOT="$PWD/target/miri/sysroot"
    cargo miri setup >/dev/null
    export MIRIFLAGS="-Zmiri-strict-provenance -Zmiri-many-seeds=$1"
    filters=()
    for filter in "${@:2}"; do [[ -n "$filter" ]] && filters+=("$filter"); done
    seeds() { grep -v '^Trying seed: ' || true; }
    cargo miri test --quiet --test miri --test miri_global_allocator -- concurrent "${filters[@]}" 2>&1 >/dev/null | seeds
    export CARGO_TARGET_DIR="$PWD/target/sdk-probes"
    cargo miri test --quiet --manifest-path sdk/rust-probes/Cargo.toml --features c --lib -- concurrent "${filters[@]}" 2>&1 >/dev/null | seeds

loom BOUND="3" *ARGS:
    CARGO_TARGET_DIR="$PWD/target/loom" LOOM_MAX_PREEMPTIONS="$1" cargo test --quiet --manifest-path loom/Cargo.toml --lib -- "${@:2}"

[script]
loom-mutants BOUND="3" *ARGS:
    out="$PWD/target/loom-mutants"
    crate="$out/crate"
    rm -rf "$crate" "$out/clean"
    mkdir -p "$crate/.cargo"
    cp -r loom/Cargo.toml loom/Cargo.lock loom/build.rs loom/src "$crate/"
    cp loom/mutants.toml "$crate/.cargo/mutants.toml"
    files=()
    while read -r path name; do
        cp "loom/src/$path" "$crate/src/$name.rs"
        if [[ -d "loom/src/${path%.rs}" ]]; then cp -r "loom/src/${path%.rs}" "$crate/src/$name"; fi
        files+=(--file "src/$name.rs")
    done < <(awk '/^#\[path = "/ { split($0, part, "\""); path = part[2]; next } path && /^mod / { sub(/^mod /, ""); sub(/;$/, ""); print path, $0; path = "" }' loom/src/lib.rs)
    sed -i '/^#\[path = "/d' "$crate/src/lib.rs"
    cd "$crate"
    export CARGO_TARGET_DIR="$out/target" LOOM_MAX_PREEMPTIONS="$1"
    cargo test --quiet --lib >/dev/null
    missed=0
    for planted in "$OLDPWD"/loom/mutants/*.patch; do
        name="$(basename "$planted" .patch)"
        cp -r src "$out/clean"
        if ! patch --quiet --batch -p1 < "$planted"; then
            echo "error: $name no longer applies; make it again against the source" >&2
            exit 1
        fi
        if ! cargo test --quiet --lib --no-run 2>/dev/null; then
            echo "error: $name does not build" >&2
            exit 1
        fi
        if timeout 300 cargo test --quiet --lib >/dev/null 2>&1; then
            echo "MISSED  $name: $(head -1 "$planted")"
            missed=1
        else
            echo "caught  $name"
        fi
        rm -rf src
        mv "$out/clean" src
    done
    if (( missed )); then exit 1; fi
    unset CARGO_TARGET_DIR
    LOOM_MAX_PREEMPTIONS="$1" cargo mutants --output "$out" --jobs 4 --cargo-test-arg=--lib "${files[@]}" "${@:2}"

cache-warm: build-test-programs
    cargo build --quiet --profile test --features {{test_features}} --bin microscope-tools
    ./scripts/contained.sh ./target/debug/microscope-tools --cache target/image-cache warm build/test-programs build/golden

tokio *ARGS:
    ./scripts/build-test-programs.sh tokio
    ulimit -c 0; test_threads="$(nproc)"; if (( test_threads > {{max_test_threads}} )); then test_threads={{max_test_threads}}; fi; MICROSCOPE_CACHE_DIR="$PWD/target/image-cache" XDG_CONFIG_HOME="$PWD/target/test-config" ./scripts/contained.sh setarch "$(uname -m)" cargo nextest run --features {{test_features}} --test-threads "$test_threads" --test tokio "$@"

node *ARGS: cache-warm
    ulimit -c 0; test_threads="$(nproc)"; if (( test_threads > {{max_test_threads}} )); then test_threads={{max_test_threads}}; fi; MICROSCOPE_CACHE_DIR="$PWD/target/image-cache" XDG_CONFIG_HOME="$PWD/target/test-config" ./scripts/contained.sh setarch "$(uname -m)" cargo nextest run --features {{test_features}} --test-threads "$test_threads" --test node "$@"

[doc("Runs the test suite COUNT times under CPU load, then the soak if SOAK_MINUTES is set.")]
[script]
stress COUNT="3" *ARGS: build-test-programs
    cargo nextest run --features {{test_features}} --no-run
    cpus="$(lscpu -p=CORE,SOCKET | grep -v '^#' | sort -u | wc -l)"
    burners=()
    trap 'kill "${burners[@]}" 2>/dev/null || true' EXIT
    for (( i = 0; i < cpus / 2; i++ )); do (while :; do :; done) & burners+=($!); done
    ulimit -c 0
    XDG_CONFIG_HOME="$PWD/target/test-config" ./scripts/contained.sh setarch "$(uname -m)" cargo nextest run --features {{test_features}} --test-threads "$(( cpus * 2 ))" --stress-count "$1" "${@:2}"
    if (( ${SOAK_MINUTES:-0} > 0 )); then
        MICROSCOPE_SOAK_SECONDS="$(( SOAK_MINUTES * 60 ))" XDG_CONFIG_HOME="$PWD/target/test-config" ./scripts/contained.sh setarch "$(uname -m)" cargo nextest run --features {{test_features}} --profile soak --no-capture --run-ignored only --test tokio -E 'test(=soak::soak)'
    fi

web-deps:
    cd web && pnpm install --frozen-lockfile --silent

docs-deps:
    cd docs && pnpm install --frozen-lockfile --silent

docs: docs-deps
    cd docs && ./node_modules/.bin/vitepress build .

docs-dev: docs-deps
    cd docs && ./node_modules/.bin/vitepress dev .

web HOST="0.0.0.0": web-deps
    cd web && ./node_modules/.bin/vite --host "$1"

dev_enroll_key := "mse_nvuwg4tponrw64dffvsgk5rnmvxhe33mnrwwk3tufvvwk6jb2nwhedoh"

[doc("Enrolls an agent with the key services' `just up` seeds and runs it, with its state under target/.")]
[script]
agent-dev SERVER="http://127.0.0.1:3000":
    cargo build --quiet --profile test --bin microscope
    export MICROSCOPE_ENROLL="${1%%://*}://{{dev_enroll_key}}@${1#*://}"
    export MICROSCOPE_STATE_DIR="$PWD/target/agent-dev" MICROSCOPE_CLOUD_METADATA=false
    exec ./target/debug/microscope agent run --config /dev/null

web-build: web-deps
    cd web && ./node_modules/.bin/vite build --logLevel warn

web-test *ARGS: web-lint
    cd web && ./node_modules/.bin/tsc && ./node_modules/.bin/tsc -p ../sdk/web && ./node_modules/.bin/vitest run "$@"

web-lint: web-deps
    web/node_modules/.bin/oxlint {{ js_files }}
    web/node_modules/.bin/oxfmt --check {{ js_files }}

web-fmt: web-deps
    web/node_modules/.bin/oxfmt {{ js_files }}

web-watch: web-deps
    cd web && ./node_modules/.bin/vitest --project unit --project browser

web-props CASES="10000": web-deps
    cd web && PROPTEST_CASES="$1" ./node_modules/.bin/vitest run --project unit
    cd web && PROPTEST_CASES="$(( $1 / 5 ))" ./node_modules/.bin/vitest run --project browser test/docking.test.tsx

web-e2e *ARGS: web-build build-test-programs
    cargo build --quiet --profile test --features {{test_features}}
    cd web && MICROSCOPE_CACHE_DIR="$PWD/../target/image-cache" ./node_modules/.bin/playwright test "$@"

web-transcripts: build-test-programs
    MICROSCOPE_WEB_TRANSCRIPTS="$PWD/web/test/transcripts" cargo nextest run --features {{test_features}} --test web

golden-record NAME:
    ./scripts/golden.sh record "$1"

sim SECONDS="30": golden
    cargo build --profile sim --features sim --bin microscope-sim
    MICROSCOPE_CACHE_DIR="$PWD/target/image-cache" ./scripts/contained.sh ./target/sim/microscope-sim sweep --seconds "$1"

sim-seed SEED *ARGS: golden
    cargo build --profile sim --features sim --bin microscope-sim
    MICROSCOPE_CACHE_DIR="$PWD/target/image-cache" ./target/sim/microscope-sim replay "$1" "${@:2}"

sim-shrink SEED *ARGS: golden
    cargo build --profile sim --features sim --bin microscope-sim
    MICROSCOPE_CACHE_DIR="$PWD/target/image-cache" ./scripts/contained.sh ./target/sim/microscope-sim shrink "$1" "${@:2}"

timings *ARGS:
    cargo run --quiet --profile test --features {{test_features}} --bin microscope-tools -- timings "$@"

unwind-oracle PROGRAM *ARGS:
    cargo build --quiet --release --features tools --bin microscope-tools
    ./target/release/microscope-tools unwind-oracle "$@"

dump PROGRAM *ARGS:
    cargo run --quiet --profile test --features {{test_features}} --bin microscope-tools -- dump "$@"

bench *ARGS: build-test-programs
    cargo build --quiet --release --features tools --bin microscope-tools
    ./scripts/contained.sh ./target/release/microscope-tools bench "$@"

profile-overhead *ARGS: build-test-programs
    cargo build --quiet --release --features tools --bin microscope-tools
    ./scripts/contained.sh ./target/release/microscope-tools overhead "$@"

bench-snapshots *ARGS:
    cargo build --quiet --release --features tools --bin microscope-tools
    ./scripts/contained.sh ./target/release/microscope-tools snapshot-pause "$@"

syscall-overhead LAUNCHER="/run/wrappers/bin/microscope-bpf" *ARGS: build-test-programs
    cargo build --quiet --release --bin microscope
    cargo build --quiet --release --features tools --bin microscope-tools
    ./scripts/contained.sh ./target/release/microscope-tools syscall-overhead --launcher "$1" "${@:2}"

bench-smoke *ARGS:
    cargo build --quiet --profile test --features {{test_features}} --bin microscope-tools
    ./scripts/contained.sh ./target/debug/microscope-tools bench-smoke --corpus "$MICROSCOPE_BENCH_CORPUS" --budgets bench/smoke.json "$@"

large_commit := "d089093868eba621d618c059b9bb7374dd61c9fe"
large_digest := "db849566d2cc0547bdb9c209da44075a9b6dd1b14b45b1814cadfce8ae44e480"
[script]
bench-large:
    dir="$PWD/target/bench/large"
    [[ -d "$dir/src" ]] || git worktree add --quiet --detach "$dir/src" {{large_commit}}
    git -C "$dir/src" checkout --quiet --detach {{large_commit}}
    (cd "$dir/src" && RUSTFLAGS="$RUSTFLAGS --remap-path-prefix=$dir/src=/microscope" CARGO_TARGET_DIR="$dir/target" cargo build --quiet --bin microscope)
    cp "$dir/target/debug/microscope" "$dir/microscope"
    digest="$(sha256sum "$dir/microscope" | cut -d' ' -f1)"
    if [[ -n "{{large_digest}}" && "$digest" != "{{large_digest}}" ]]; then
        echo "error: the large program's digest is $digest, not {{large_digest}}" >&2
        exit 1
    fi
    echo "$dir/microscope $digest"

[script]
profile-instructions PROGRAM:
    cargo build --quiet --profile profiling --features tools --bin microscope-tools
    out="$(mktemp)"
    trap 'rm -f "$out"' EXIT
    valgrind --tool=callgrind --callgrind-out-file="$out" ./target/profiling/microscope-tools load "$1" >/dev/null 2>&1
    callgrind_annotate --inclusive=yes "$out" | grep -E 'PROGRAM TOTALS|microscope' | head -80

[script]
profile-heap PROGRAM:
    cargo build --quiet --profile profiling --features tools,system-alloc --bin microscope-tools
    out="$(mktemp)"
    trap 'rm -f "$out"' EXIT
    valgrind --tool=dhat --dhat-out-file="$out" ./target/profiling/microscope-tools load "$1" >/dev/null 2>&1
    ./target/profiling/microscope-tools heap "$out"

profile-counters PROGRAM:
    cargo build --quiet --profile profiling --features tools --bin microscope-tools
    perf stat -e instructions:u,cycles:u,cache-references:u,cache-misses:u,branch-misses:u ./target/profiling/microscope-tools load "$1" >/dev/null

probe-cost COUNT="100000000": build-test-programs
    cargo build --quiet --release --bin microscope
    ./build/test-programs/probes cost {{COUNT}}
    ./target/release/microscope record -o /dev/null -- ./build/test-programs/probes cost-on {{COUNT}} | grep 'a zone'

fuzz TARGET *ARGS="": build-test-programs
    leaks=1; if [[ " {{decoding_fuzz_targets}} " == *" $1 "* ]]; then leaks=0; fi; ASAN_OPTIONS="detect_leaks=$leaks" cargo fuzz run "$1" -- -detect_leaks="$leaks" "${@:2}"

[script]
fuzz-replay: build-test-programs
    cargo fuzz build
    for target in $(cargo fuzz list); do
        leaks=1
        if [[ " {{decoding_fuzz_targets}} " == *" $target "* ]]; then leaks=0; fi
        mkdir -p "fuzz/corpus/$target"
        ASAN_OPTIONS="detect_leaks=$leaks" ./scripts/contained.sh cargo fuzz run "$target" -- -runs=0 -detect_leaks="$leaks"
    done

[script("bash", "-euo", "pipefail")]
uat-vscode DIR="target/uat": build
    PATH="$PWD/target/debug:$PATH" editors/vscode/test/run.sh "$1"
    sed -i "s#$PWD#\${root}#g" "$1"/vscode-*.log

[script("bash", "-euo", "pipefail")]
uat-nvim NVIM_DAP DIR="target/uat": build
    rm -f "$2"/nvim-*.log
    PATH="$PWD/target/debug:$PATH" nvim --headless --clean -l editors/nvim/uat.lua "$1" "$PWD" "$2"
    sed -i "s#$PWD#\${root}#g" "$2"/nvim-*.log

web-shot *PROGRAM: web-build build-test-programs
    cargo build --quiet --profile test
    cd web && node e2e/shots.ts "$@"

web-bench ROUNDS="5": web-build build-test-programs
    cargo build --quiet --release
    cd web && MICROSCOPE_WEB_BINARY="$PWD/../target/release/microscope" MICROSCOPE_CACHE_DIR="$PWD/../target/image-cache" node e2e/bench.ts "{{ROUNDS}}"

web-probe PROGRAM *STEPS: web-build
    cargo build --quiet --profile test
    cd web && MICROSCOPE_CACHE_DIR="$PWD/../target/image-cache" node e2e/probe.ts "$(realpath "../$1")" "${@:2}"
