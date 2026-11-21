#!/usr/bin/env bash

set -euo pipefail

build_args=()
direct=0
skip_build=0
cargo_verbose=0
i=1

while (( i <= $# )); do
    arg="${!i}"
    case "$arg" in
        --)
            break
            ;;
        -h|--help|--no-run|--unit-graph)
            direct=1
            ;;
        --archive-file|--binaries-metadata)
            skip_build=1
            i=$((i + 1))
            ;;
        --archive-file=*|--binaries-metadata=*)
            skip_build=1
            ;;
        --cargo-verbose)
            cargo_verbose=1
            build_args+=("$arg")
            ;;
        --workspace|--all|--lib|--bins|--examples|--tests|--benches|--all-targets|--all-features|--no-default-features|-r|--release|--frozen|--locked|--offline|--cargo-quiet|--ignore-rust-version|--future-incompat-report|--timings|-v|--verbose)
            build_args+=("$arg")
            ;;
        --color=*|--package=*|--exclude=*|--bin=*|--example=*|--test=*|--bench=*|--features=*|--build-jobs=*|--cargo-profile=*|--target=*|--target-dir=*|--timings=*|--manifest-path=*|--cargo-message-format=*|--config=*|-p?*|-F?*|-Z?*)
            build_args+=("$arg")
            ;;
        --color|-p|--package|--exclude|--bin|--example|--test|--bench|-F|--features|--build-jobs|--cargo-profile|--target|--target-dir|--manifest-path|--cargo-message-format|--config|-Z)
            build_args+=("$arg")
            if (( i < $# )); then
                i=$((i + 1))
                build_args+=("${!i}")
            fi
            ;;
        -R|--rerun|--run-ignored|--partition|--platform-filter|-E|--filterset|-j|--test-threads|--jobs|--retries|--flaky-result|--max-fail|--debugger|--tracer|--no-tests|--stress-count|--stress-duration|--failure-output|--success-output|--status-level|--final-status-level|--show-progress|--max-progress-running|--message-format|--message-format-version|--archive-format|--extract-to|--cargo-metadata|--workspace-remap|--target-dir-remap|--build-dir-remap|--config-file|--user-config-file|--tool-config-file|-P|--profile)
            i=$((i + 1))
            ;;
    esac
    i=$((i + 1))
done

if (( direct )); then
    exec cargo nextest run "$@"
fi

run_args=()
if (( ! skip_build )); then
    cargo nextest run --no-run "${build_args[@]}"
    if (( ! cargo_verbose )); then
        run_args+=(--cargo-quiet)
    fi
fi

exec "$(dirname "${BASH_SOURCE[0]}")/contained.sh" setarch "$(uname -m)" cargo nextest run "${run_args[@]}" "$@"
