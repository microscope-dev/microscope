#!/usr/bin/env bash

# PreToolUse hook for Bash. Blocks compilers and build tools that would run on
# the host instead of in the pinned Nix shell (AGENTS.md: "Do not run Cargo,
# compilers, linters, or tests directly from the host").
#
# Allowed: anything inside the shell (MICROSCOPE_HERMETIC=1). `just` is not
# listed: every justfile recipe runs in the pinned shell itself (`set shell`
# and `script-interpreter` in the justfile), so `just test` from the host is
# hermetic. Blocked: a command whose first word, in any segment of a compound
# command, is one of the tools below. Exit 2 blocks the call and returns the
# message to the agent.

set -euo pipefail

if [[ "${MICROSCOPE_HERMETIC:-}" == 1 ]]; then
    exit 0
fi

command=$(jq -r '.tool_input.command // empty')
if [[ -z "$command" ]]; then
    exit 0
fi

host_tools=(cargo rustc rustup rustfmt clippy-driver cc c++ gcc g++ clang clang++ make)

# Quoted text is an argument, not a command, so drop it before splitting. The
# input is one record (-z), so a quote can span lines, as commit messages do.
# This does not handle escaped quotes; a command that has one can be misread.
unquoted=$(sed -z -E -e 's/"[^"]*"//g' -e "s/'[^']*'//g" <<<"$command" | tr -d '\0')

# Split on the operators that start a new command, one segment per line.
segments=$(sed -e 's/&&/\n/g' -e 's/||/\n/g' -e 's/[;|()]/\n/g' <<<"$unquoted")

while IFS= read -r segment; do
    # Drop leading whitespace and inline variable assignments (FOO=bar cargo ...).
    segment=$(sed -E -e 's/^[[:space:]]+//' -e 's/^([A-Za-z_][A-Za-z0-9_]*=[^[:space:]]*[[:space:]]+)+//' <<<"$segment")
    [[ -z "$segment" ]] && continue

    read -r -a words <<<"$segment"
    tool=$(basename "${words[0]}")

    for host_tool in "${host_tools[@]}"; do
        if [[ "$tool" == "$host_tool" ]]; then
            {
                echo "Blocked: \`$segment\` would run on the host."
                echo "AGENTS.md requires project tools to run in the pinned Nix shell:"
                echo "  just dev --command $segment"
            } >&2
            exit 2
        fi
    done
done <<<"$segments"

exit 0
