#!/usr/bin/env bash
# Runs the VS Code user acceptance test against the microscope on PATH, in a
# throwaway profile, and records each session's traffic in $1.
set -euo pipefail

here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/../../.." && pwd)
record=${1:?usage: run.sh RECORDING_DIRECTORY}
profile=$(mktemp -d)
trap 'rm -rf "$profile"' EXIT
# An empty folder to open, so the test can press F5 without a launch.json.
mkdir "$profile/workspace"

# Extension tests run VS Code's Electron executable, not its CLI, which
# would hand the window to another process and return at once.
if [[ -z ${VSCODE_EXECUTABLE:-} ]]; then
    cli=$(readlink -f "$(command -v code)")
    for candidate in "$(dirname "$cli")/../lib/vscode/code" /usr/share/code/code /opt/visual-studio-code/code; do
        if [[ -x $candidate ]]; then
            VSCODE_EXECUTABLE=$candidate
            break
        fi
    done
fi

# VS Code is the host's, but the programs the scenarios debug are the
# flake's, named by the pinned shell.
names='printf "export %q=%q\n" MICROSCOPE_NODE "$MICROSCOPE_NODE" MICROSCOPE_CPYTHON "$MICROSCOPE_CPYTHON" \
    MICROSCOPE_TSX "$MICROSCOPE_TSX" MICROSCOPE_ESBUILD "$MICROSCOPE_ESBUILD" \
    MICROSCOPE_UAT_NSENTER "$(command -v nsenter)"'
if [[ ${MICROSCOPE_HERMETIC:-} == 1 ]]; then
    eval "$(bash -c "$names")"
else
    eval "$("$root/scripts/dev.sh" --command bash -c "$names")"
fi

MICROSCOPE_UAT_ROOT=$root MICROSCOPE_UAT_RECORD=$(realpath -m "$record") "${VSCODE_EXECUTABLE:?set VSCODE_EXECUTABLE to the Electron executable of VS Code}" \
    --user-data-dir "$profile/data" \
    --extensions-dir "$profile/extensions" \
    --disable-extensions \
    --disable-workspace-trust \
    --skip-welcome \
    --skip-release-notes \
    --new-window \
    --extensionDevelopmentPath "$root/editors/vscode" \
    --extensionTestsPath "$here/uat.js" \
    "$profile/workspace"
