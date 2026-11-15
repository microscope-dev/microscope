#!/usr/bin/env bash












set -euo pipefail

if [[ "${MICROSCOPE_HERMETIC:-}" == 1 ]]; then
    exit 0
fi

command=$(jq -r '.tool_input.command // empty')
if [[ -z "$command" ]]; then
    exit 0
fi

host_tools=(cargo rustc rustup rustfmt clippy-driver cc c++ gcc g++ clang clang++ make)




unquoted=$(sed -z -E -e 's/"[^"]*"//g' -e "s/'[^']*'//g" <<<"$command" | tr -d '\0')


segments=$(sed -e 's/&&/\n/g' -e 's/||/\n/g' -e 's/[;|()]/\n/g' <<<"$unquoted")

while IFS= read -r segment; do

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
