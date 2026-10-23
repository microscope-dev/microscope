#!/usr/bin/env bash
# Enters the pinned Nix development shell, or runs a command in it, with
# nothing from the host's environment but what names the user and their
# terminal: every tool, library, and setting a build or test uses is the
# flake's.
set -euo pipefail

keep=(
    HOME USER LOGNAME TERM COLORTERM NO_COLOR CLICOLOR CLICOLOR_FORCE
    # contained.sh asks the user's systemd for a scope.
    XDG_RUNTIME_DIR DBUS_SESSION_BUS_ADDRESS
    CARGO_HOME CARGO_TARGET_DIR RUST_BACKTRACE RUST_LOG
    # The settings people give microscope and its recipes.
    MICROSCOPE_CACHE_DIR MICROSCOPE_CONFIG MICROSCOPE_FIXTURE_JOBS MICROSCOPE_FLIGHT_RECORDING
    MICROSCOPE_JOBS MICROSCOPE_MEMORY_MAX MICROSCOPE_UPDATE_PROTOCOL MICROSCOPE_UPDATE_SYSCALLS
    MICROSCOPE_UPDATE_V8
    MICROSCOPE_WEB_BINARY MICROSCOPE_WEB_TRANSCRIPTS
    # Says that this shell is the pinned one, for hermetic.sh.
    MICROSCOPE_HERMETIC
)
export MICROSCOPE_HERMETIC=1
arguments=()
for name in "${keep[@]}"; do
    arguments+=(--keep "$name")
done
exec nix --extra-experimental-features 'nix-command flakes' develop --ignore-env \
    "${arguments[@]}" "$@"
