# Every recipe runs in the pinned Nix shell, entered unless it already runs
# there, so that no build or test finds the host's tools or settings. The
# few recipes that drive programs of the host's say so.
# Bash runs with --norc: entering the shell starts it at the first shell
# level, where Nix's bash reads ~/.bashrc whenever its input is a socket.
set shell := ["scripts/hermetic.sh", "bash", "--norc", "-euo", "pipefail", "-c"]
set script-interpreter := ["scripts/hermetic.sh", "bash", "--norc", "-euo", "pipefail"]
set positional-arguments := true

# Process-isolated debugger tests also create controller, waiter, and inferior
# threads. More concurrency adds contention without improving wall time.
max_test_threads := "16"

# Lints the Rust code and runs the complete test suite.
default: check

# Checks formatting, runs Clippy, and runs the complete test suite.
check: lint test miri loom web-test docs proto

# Runs everything to check before committing. The concurrent Miri tests try
# more seeds than `check` does: ordering bugs planted in the probe SDK showed
# only between seeds 48 and 61.
all: check (miri-seeds "0..64") (loom "4") loom-mutants web-e2e stress sim bench-smoke build-static

# Enters the pinned Nix development shell, or runs a command in it, with
# nothing of the host's environment but the user and their terminal.
[script("bash", "-euo", "pipefail")]
dev *ARGS="":
    exec ./scripts/dev.sh "$@"

# Installs the vscode extension as a symlink for fast local development.
[script("bash")]
install-vscode-symlink:
    ln -s "$PWD/editors/vscode" ~/.vscode/extensions/microscope.microscope-0.1.0

# Builds the native test fixtures and the simulator's golden programs
# without running Rust tests.
build-test-programs: golden
    ./scripts/build-test-programs.sh

# Builds the simulator's golden programs into build/golden, failing unless
# they match the hashes and behavior their manifests record.
golden:
    ./scripts/golden.sh build

# Builds the native test fixtures and microscope. microscope builds with the
# test profile, as the tests do, whose optimization loads debug information five
# times as fast as an unoptimized build, and which shares the tests' artifacts.
build *ARGS="": build-test-programs
    cargo build --profile test {{ARGS}}

# Builds microscope as `build` does but with its own debug information, for
# debugging microscope with microscope. The next `build` rebuilds the crate
# without it.
build-debug *ARGS="":
    cargo build --profile test --config 'profile.test.package.microscope.debug=2' {{ARGS}}

# Builds a fully static microscope, which runs on any x86-64 Linux, linked
# against the flake's musl: PROFILE is debug, release, or another Cargo profile,
# and the binary is target/x86_64-unknown-linux-musl/PROFILE/microscope. Release
# builds embed the web page. Fails unless the binary is static.
[script]
build-static PROFILE="debug" *ARGS: web
    # Unset, the musl target's own flags apply instead of the glibc ones.
    unset RUSTFLAGS
    cargo build --target x86_64-unknown-linux-musl --bin microscope \
        --profile "$([[ "$1" == debug ]] && echo dev || echo "$1")" "${@:2}"
    binary="target/x86_64-unknown-linux-musl/$1/microscope"
    if readelf -lW "$binary" | grep -q INTERP || readelf -dW "$binary" | grep -q NEEDED; then
        echo "error: $binary links dynamically" >&2
        exit 1
    fi
    echo "$binary (static, musl $MICROSCOPE_MUSL_VERSION)"

# Builds microscope and runs it with the supplied arguments.
run *ARGS: build
    ./target/debug/microscope "$@"

# Checks formatting and runs Clippy on development and release builds, which
# differ in what the flight recorder compiles. Incremental checking halves the
# release lint after an edit and leaves release builds as they are. The Rust
# probe SDK is its own crate, with its own target directory. Unsafe code stays
# in the files scripts/unsafe-sites.sh lists.
lint:
    ./scripts/unsafe-sites.sh
    cargo fmt --check
    cargo clippy --all-targets --all-features -- -D warnings
    CARGO_PROFILE_RELEASE_INCREMENTAL=true cargo clippy --release --all-targets --all-features -- -D warnings
    cargo check --quiet --manifest-path fuzz/Cargo.toml
    cargo fmt --check --manifest-path sdk/rust-probes/Cargo.toml
    CARGO_TARGET_DIR="$PWD/target/sdk-probes" cargo clippy --quiet --manifest-path sdk/rust-probes/Cargo.toml --all-targets --features c -- -D warnings
    cargo fmt --check --manifest-path loom/Cargo.toml
    CARGO_TARGET_DIR="$PWD/target/loom" cargo clippy --quiet --manifest-path loom/Cargo.toml --all-targets -- -D warnings

# Arguments go to nextest, e.g. `just test print_` or `just test --test cli`.
# Doc tests only run with the full suite. Tests run inside a memory cap, and
# each test process also caps its own heap (tests/support/memory_cap.rs). `nix develop` turns address
# randomization off, which setarch turns back on, so tests see what they would
# in any shell. A user's own view files are not the tests', so the user
# configuration is an empty directory. Tests read images the cache-warm recipe
# wrote; `just stress` loads them afresh. Programs the tests kill dump no core,
# which took the machine's core handler most of a second each.
[doc("Builds the native test fixtures and runs the Rust test suite.")]
test *ARGS: cache-warm
    ulimit -c 0; test_threads="$(nproc)"; if (( test_threads > {{max_test_threads}} )); then test_threads={{max_test_threads}}; fi; MICROSCOPE_CACHE_DIR="$PWD/target/image-cache" XDG_CONFIG_HOME="$PWD/target/test-config" ./scripts/contained.sh setarch "$(uname -m)" cargo nextest run --features tools --test-threads "$test_threads" "$@"
    if (( $# == 0 )); then cargo test --doc; CARGO_TARGET_DIR="$PWD/target/sdk-probes" cargo test --quiet --manifest-path sdk/rust-probes/Cargo.toml --features c; fi

# Runs the BPF engine's tests, which need CAP_BPF and CAP_PERFMON, starting
# microscope through LAUNCHER: the microscope-bpf wrapper
# nixosModules.microscope-bpf installs (dev/syscalls.md). Nothing else in the
# suite runs with privilege. Arguments go to nextest.
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
    MICROSCOPE_BPF_LAUNCHER="$1" MICROSCOPE_CACHE_DIR="$PWD/target/image-cache" XDG_CONFIG_HOME="$PWD/target/test-config" ./scripts/contained.sh setarch "$(uname -m)" cargo nextest run --features tools --run-ignored only --test syscalls --test python --test node -E 'test(/(^|::)bpf::/)' "${@:2}"

# How many seeds `just miri` tries its concurrent tests under: about as many
# as run at once. Try more with `just miri_seeds=0..256 miri`, or
# `just miri-seeds 0..256`.
miri_seeds := "0..8"

# Runs tests/miri.rs and tests/miri_global_allocator.rs, the tests of
# microscope's unsafe code, and the Rust probe SDK's tests under Miri, which stops at undefined behavior: an access out of
# bounds, misaligned, or after a free, a block freed with the wrong layout, a
# data race, a pointer made from an integer. Tests named `concurrent` then
# run under many seeds (miri-seeds). Its sysroot builds under target/.
# Arguments go to the test harnesses.
[script]
miri *ARGS: && (miri-seeds miri_seeds ARGS)
    export MIRI_SYSROOT="$PWD/target/miri/sysroot"
    cargo miri setup >/dev/null
    MIRIFLAGS="-Zmiri-strict-provenance" cargo miri test --quiet --test miri --test miri_global_allocator -- --skip concurrent "$@"
    export CARGO_TARGET_DIR="$PWD/target/sdk-probes"
    MIRIFLAGS="-Zmiri-strict-provenance" cargo miri test --quiet --manifest-path sdk/rust-probes/Cargo.toml --features c --lib -- --skip concurrent "$@"

# Runs the Miri tests named `concurrent` under SEEDS, a range of seeds, each a
# different schedule and weak-memory outcome. Seeds run at once, so only a
# failure's panic and seed, on stderr, are shown.
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

# Runs the loom models in loom/, which check concurrent protocols under every
# schedule of a few threads with at most BOUND preemptions each. Each
# preemption allowed multiplies the schedules about tenfold: 3 takes about a
# second, 4 a few, and 5 half a minute. The models build the protocols' own
# source files against loom's types in a crate and target directory of their
# own, so microscope is not rebuilt. Arguments go to the test harness.
loom BOUND="3" *ARGS:
    CARGO_TARGET_DIR="$PWD/target/loom" LOOM_MAX_PREEMPTIONS="$1" cargo test --quiet --manifest-path loom/Cargo.toml --lib -- "${@:2}"

# Breaks the protocols the loom models check and runs the models at BOUND
# against each break, which they must fail: first the bugs in loom/mutants/,
# which the models' mutex and condvar exist to catch, then each break
# cargo-mutants makes. A break the models pass is a case they do not check,
# unless it changes nothing; those are listed in loom/mutants.toml with the
# reason. cargo-mutants mutates only files inside the crate it tests, so this
# works on a copy of loom/ with each protocol's file moved in, under
# target/loom-mutants: the source tree never changes. It takes under a
# minute. Arguments go to cargo-mutants, e.g. `just loom-mutants 3 --list`.
[script]
loom-mutants BOUND="3" *ARGS:
    out="$PWD/target/loom-mutants"
    crate="$out/crate"
    rm -rf "$crate" "$out/clean"
    mkdir -p "$crate/.cargo"
    cp -r loom/Cargo.toml loom/Cargo.lock loom/build.rs loom/src "$crate/"
    cp loom/mutants.toml "$crate/.cargo/mutants.toml"
    # Each `#[path = "..."]` line and the `mod NAME;` after it.
    files=()
    while read -r path name; do
        cp "loom/src/$path" "$crate/src/$name.rs"
        # Its own modules, such as its std tests, which cfg(loom) leaves out.
        if [[ -d "loom/src/${path%.rs}" ]]; then cp -r "loom/src/${path%.rs}" "$crate/src/$name"; fi
        files+=(--file "src/$name.rs")
    done < <(awk '/^#\[path = "/ { split($0, part, "\""); path = part[2]; next } path && /^mod / { sub(/^mod /, ""); sub(/;$/, ""); print path, $0; path = "" }' loom/src/lib.rs)
    sed -i '/^#\[path = "/d' "$crate/src/lib.rs"
    cd "$crate"
    export CARGO_TARGET_DIR="$out/target" LOOM_MAX_PREEMPTIONS="$1"
    # A break counts as caught only if the models pass without it and it
    # builds.
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

# Fills the image cache tests read with every fixture's image, built by this
# checkout's loader: entries name the sources that built them, so an edit
# leaves the old ones to eviction.
cache-warm: build-test-programs
    cargo build --quiet --profile test --features tools --bin microscope-tools
    ./scripts/contained.sh ./target/debug/microscope-tools --cache target/image-cache warm build/test-programs build/golden

# Builds the tokio fixtures where their sources changed and runs the tokio
# suite: the quick loop for work on tokio support. Arguments go to nextest,
# e.g. `just tokio workers::`.
tokio *ARGS:
    ./scripts/build-test-programs.sh tokio
    ulimit -c 0; test_threads="$(nproc)"; if (( test_threads > {{max_test_threads}} )); then test_threads={{max_test_threads}}; fi; MICROSCOPE_CACHE_DIR="$PWD/target/image-cache" XDG_CONFIG_HOME="$PWD/target/test-config" ./scripts/contained.sh setarch "$(uname -m)" cargo nextest run --features tools --test-threads "$test_threads" --test tokio "$@"

# Runs the Node suite alone, after the fixtures and Node cores it reads are
# built: the quick loop for work on Node support. Arguments go to nextest,
# e.g. `just node stacks::`.
node *ARGS: cache-warm
    ulimit -c 0; test_threads="$(nproc)"; if (( test_threads > {{max_test_threads}} )); then test_threads={{max_test_threads}}; fi; MICROSCOPE_CACHE_DIR="$PWD/target/image-cache" XDG_CONFIG_HOME="$PWD/target/test-config" ./scripts/contained.sh setarch "$(uname -m)" cargo nextest run --features tools --test-threads "$test_threads" --test node "$@"

# Races in process control fail far more often when the debugger competes for
# the CPUs, so this oversubscribes the test threads and keeps busy loops
# running beside them. It stops at the first failure so that the failing
# test's flight recording is kept; a later pass of the same test would remove
# it. It loads debug information afresh, without the image cache, so that the
# suite also runs against the loader itself. Arguments go to nextest, e.g.
# `just stress 100 -E 'binary(dap)'`.
#
# With SOAK_MINUTES set, the tokio soak then runs under the same load for that
# many minutes, e.g. `SOAK_MINUTES=30 just stress 1`. It attaches to a tokio
# server under client load, inspecting and re-attaching it over and over, with
# every invariant checked at every stop. Without it, only the suite runs.
[doc("Runs the test suite COUNT times under CPU load, then the soak if SOAK_MINUTES is set.")]
[script]
stress COUNT="3" *ARGS: build-test-programs
    # Build before the busy loops start so they slow only the tests.
    cargo nextest run --features tools --no-run
    # Physical cores, not logical CPUs: two hyperthreads of a core run little
    # more than one does, and the tests' deadlines are in seconds. Counting
    # each of them oversubscribed a 12-core, 24-thread machine past the
    # deadlines of tests that only load debug information.
    cpus="$(lscpu -p=CORE,SOCKET | grep -v '^#' | sort -u | wc -l)"
    burners=()
    trap 'kill "${burners[@]}" 2>/dev/null || true' EXIT
    for (( i = 0; i < cpus / 2; i++ )); do (while :; do :; done) & burners+=($!); done
    ulimit -c 0
    XDG_CONFIG_HOME="$PWD/target/test-config" ./scripts/contained.sh setarch "$(uname -m)" cargo nextest run --features tools --test-threads "$(( cpus * 2 ))" --stress-count "$1" "${@:2}"
    if (( ${SOAK_MINUTES:-0} > 0 )); then
        MICROSCOPE_SOAK_SECONDS="$(( SOAK_MINUTES * 60 ))" XDG_CONFIG_HOME="$PWD/target/test-config" ./scripts/contained.sh setarch "$(uname -m)" cargo nextest run --features tools --profile soak --no-capture --run-ignored only --test tokio -E 'test(=soak::soak)'
    fi

# Installs the web page's locked dependencies.
web-deps:
    cd web && pnpm install --frozen-lockfile --silent

# Installs the documentation site's locked dependencies.
# Lints the agent protocol's schema, and checks that it can still talk to
# agents and services built from main's, once main has one.
proto:
    buf lint
    if git cat-file -e main:proto/microscope/agent/v1/agent.proto 2>/dev/null; then \
        buf breaking --against '.git#branch=main'; \
    fi

docs-deps:
    cd docs && pnpm install --frozen-lockfile --silent

# Builds the public documentation and fails on broken internal links.
docs: docs-deps
    cd docs && ./node_modules/.bin/vitepress build .

# Serves the public documentation with live reload.
docs-dev: docs-deps
    cd docs && ./node_modules/.bin/vitepress dev .

# Builds the web page into build/web, where `microscope web` serves it from.
# Development builds of microscope read it from disk, so rebuilding the page
# needs no Rust rebuild.
web: web-deps
    cd web && ./node_modules/.bin/vite build --logLevel warn

# Type-checks and lints the page, checks renderers against sdk/web's
# declarations, and runs the page's tests outside a browser and
# its component tests in headless Chromium. Arguments go to Vitest.
web-test *ARGS: web-deps
    cd web && ./node_modules/.bin/tsc --noEmit && ./node_modules/.bin/tsc -p ../sdk/web && biome check src test e2e && ./node_modules/.bin/vitest run "$@"

# Drives the built page and real `microscope web` servers in Chromium and
# Firefox. Arguments go to Playwright, e.g. `just web-e2e --project=chromium`.
web-e2e *ARGS: web build-test-programs
    cargo build --quiet --profile test
    cd web && MICROSCOPE_CACHE_DIR="$PWD/../target/image-cache" ./node_modules/.bin/playwright test "$@"

# Serves PROGRAM on port 7342 with the page from Vite, which reloads on every
# save: open the join link microscope prints, with 5173 in place of 7342.
[script]
web-dev *ARGS: web-deps
    cargo build --quiet --profile test
    ./target/debug/microscope web --port 7342 --allow-origin http://127.0.0.1:5173 "$@" &
    trap 'kill %1 2>/dev/null || true' EXIT
    cd web && ./node_modules/.bin/vite

# Rerecords the server traffic the page's replay tests read
# (web/test/transcripts) from the Rust web tests.
web-transcripts: build-test-programs
    MICROSCOPE_WEB_TRANSCRIPTS="$PWD/web/test/transcripts" cargo nextest run --features tools --test web

# Rebuilds one golden program and rewrites its manifest, after a deliberate
# change to its sources or the toolchain. Commit a new manifest on its own.
golden-record NAME:
    ./scripts/golden.sh record "$1"

# Simulates random sessions on every core for SECONDS, inside a memory cap,
# and reports each kind of failure with its smallest seed.
sim SECONDS="30": golden
    cargo build --profile sim --features sim --bin microscope-sim
    MICROSCOPE_CACHE_DIR="$PWD/target/image-cache" ./scripts/contained.sh ./target/sim/microscope-sim sweep --seconds "$1"

# Replays one simulated session and prints its trace. Pass `--at STEP` to
# stop there and print the state, or the `--fingerprint` a report gave.
sim-seed SEED *ARGS: golden
    cargo build --profile sim --features sim --bin microscope-sim
    MICROSCOPE_CACHE_DIR="$PWD/target/image-cache" ./target/sim/microscope-sim replay "$1" "${@:2}"

# Summarizes a `--timings` report, or what changed between two:
# `just timings BASE NEW`. Pass --threads for each thread's work, and --all
# for every phase.
timings *ARGS:
    cargo run --quiet --profile test --features tools --bin microscope-tools -- timings "$@"

# Prints every answer PROGRAM's debug information gives, in a canonical form
# that compares with `diff`. Pass --sections to dump only some.
dump PROGRAM *ARGS:
    cargo run --quiet --profile test --features tools --bin microscope-tools -- dump "$@"

# Loads every program of the pinned corpus in processes of their own and
# reports what each load costs. Pass --out FILE to save the report and
# --compare BASE to compare it with a saved one; `just bench-large` builds
# the large program first.
bench *ARGS: build-test-programs
    cargo build --quiet --release --features tools --bin microscope-tools
    ./scripts/contained.sh ./target/release/microscope-tools bench "$@"

# Runs reference workloads bare and under each preset, interleaved, and
# prints what recording cost each, in minutes. Compare with the recorded
# baseline with `--compare bench/overhead.json`, record one with `--out`,
# and fail unless prod costs every workload under 1% with `--check 1`.
profile-overhead *ARGS: build-test-programs
    cargo build --quiet --release --features tools --bin microscope-tools
    ./scripts/contained.sh ./target/release/microscope-tools overhead "$@"

# Times a getppid loop bare, under each syscall-tracing engine, and untraced
# beside a BPF trace of another program, and prints what each adds a call.
# The BPF setups start microscope through LAUNCHER. Compare with the recorded
# baseline with `--compare bench/syscalls-overhead.json`, record one with
# `--out`.
syscall-overhead LAUNCHER="/run/wrappers/bin/microscope-bpf" *ARGS: build-test-programs
    cargo build --quiet --release --bin microscope
    cargo build --quiet --release --features tools --bin microscope-tools
    ./scripts/contained.sh ./target/release/microscope-tools syscall-overhead --launcher "$1" "${@:2}"

# Checks cold, single-worker loads of the pinned Nix corpus against reviewed
# allocation-count and allocated-byte ceilings. No test-fixture build needed.
bench-smoke *ARGS:
    cargo build --quiet --profile test --features tools --bin microscope-tools
    ./scripts/contained.sh ./target/debug/microscope-tools bench-smoke --corpus "$MICROSCOPE_BENCH_CORPUS" --budgets bench/smoke.json "$@"

# Builds the full benchmark's large program: microscope's own development build
# at a pinned commit, in a worktree, with its paths remapped so that its
# bytes do not depend on the checkout. Fails unless they match the digest.
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

# Prints where loading PROGRAM spends its instructions: Callgrind's
# inclusive costs, trimmed to microscope's functions.
[script]
profile-instructions PROGRAM:
    cargo build --quiet --profile profiling --features tools --bin microscope-tools
    out="$(mktemp)"
    trap 'rm -f "$out"' EXIT
    valgrind --tool=callgrind --callgrind-out-file="$out" ./target/profiling/microscope-tools load "$1" >/dev/null 2>&1
    callgrind_annotate --inclusive=yes "$out" | grep -E 'PROGRAM TOTALS|microscope' | head -80

# Prints which microscope functions allocate when loading PROGRAM, by blocks
# and by bytes live at the heap's peak, under DHAT with the C library's
# allocator, which Valgrind can see.
[script]
profile-heap PROGRAM:
    cargo build --quiet --profile profiling --features tools,system-alloc --bin microscope-tools
    out="$(mktemp)"
    trap 'rm -f "$out"' EXIT
    valgrind --tool=dhat --dhat-out-file="$out" ./target/profiling/microscope-tools load "$1" >/dev/null 2>&1
    ./target/profiling/microscope-tools heap "$out"

# Counts user-mode instructions, cycles, and cache and branch misses while
# loading PROGRAM.
profile-counters PROGRAM:
    cargo build --quiet --profile profiling --features tools --bin microscope-tools
    perf stat -e instructions:u,cycles:u,cache-references:u,cache-misses:u,branch-misses:u ./target/profiling/microscope-tools load "$1" >/dev/null

# Times one instrumented zone in a release build of microscope: with its site
# off, as when nothing records, and on, as microscope reads it.
probe-cost COUNT="100000000": build-test-programs
    cargo build --quiet --release --bin microscope
    ./build/test-programs/probes cost {{COUNT}}
    ./target/release/microscope record -o /dev/null -- ./build/test-programs/probes cost-on {{COUNT}} | grep 'a zone'

# Runs one fuzz target: expression-parse, dwarf-expression, core-dump, dispatch,
# elf-symbols, gopclntab, disassembly, debug-register-plan, dap-transport,
# dap-request, views, image, perf-records, capture, profile-import,
# syscall-record, syscall-filter, glibc-tls, js-source-map, or cdp-message.
# Arguments go to libFuzzer.
# iced-x86 builds its formatter tables once and never frees them, which
# LeakSanitizer would report as a failure when the disassembly target exits.
fuzz TARGET *ARGS="":
    if [[ "$1" == disassembly ]]; then set -- "$@" -detect_leaks=0; fi; cargo fuzz run "$1" -- "${@:2}"

# Drives the DAP adapter from a real VS Code window, as a user would, and
# records each session's traffic in DIR. Needs a display. Recording into
# tests/dap/traffic refreshes the traffic the DAP tests replay. VS Code is
# the host's.
[script("bash", "-euo", "pipefail")]
uat-vscode DIR="target/uat": build
    PATH="$PWD/target/debug:$PATH" editors/vscode/test/run.sh "$1"
    sed -i "s#$PWD#\${root}#g" "$1"/vscode-*.log

# Drives the DAP adapter from nvim-dap in a headless Neovim and records each
# session's traffic in DIR. NVIM_DAP is an nvim-dap checkout. Neovim is the
# host's.
[script("bash", "-euo", "pipefail")]
uat-nvim NVIM_DAP DIR="target/uat": build
    rm -f "$2"/nvim-*.log
    PATH="$PWD/target/debug:$PATH" nvim --headless --clean -l editors/nvim/uat.lua "$1" "$PWD" "$2"
    sed -i "s#$PWD#\${root}#g" "$2"/nvim-*.log

# Screenshots every screen in light and dark at three widths into
# target/web-shots, with PROGRAM loaded (the kvstore fixture by default).
web-shot *PROGRAM: web build-test-programs
    cargo build --quiet --profile test
    cd web && node e2e/shots.ts "$@"

# Times the metrics fixture's drawings with a million samples in headless
# Chromium, from a stop to its picture, over ROUNDS stops each, with a release
# build. Not part of the gate: the times depend on the machine.
web-bench ROUNDS="5": web build-test-programs
    cargo build --quiet --release
    cd web && MICROSCOPE_WEB_BINARY="$PWD/../target/release/microscope" MICROSCOPE_CACHE_DIR="$PWD/../target/image-cache" node e2e/bench.ts "{{ROUNDS}}"

# Drives PROGRAM through STEPS in headless Chromium, saving a screenshot after
# each and printing the page's console: `just web-probe build/test-programs/basic
# key:F9 key:F5 wait:Stopped`. See web/e2e/probe.ts for the steps.
web-probe PROGRAM *STEPS: web
    cargo build --quiet --profile test
    cd web && MICROSCOPE_CACHE_DIR="$PWD/../target/image-cache" node e2e/probe.ts "$(realpath "../$1")" "${@:2}"
