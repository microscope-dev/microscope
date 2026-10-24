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
    MICROSCOPE_UAT_NSENTER "$(command -v nsenter)" MICROSCOPE_UAT_XVFB "$(command -v Xvfb)"'
if [[ ${MICROSCOPE_HERMETIC:-} == 1 ]]; then
    eval "$(bash -c "$names")"
else
    eval "$("$root/scripts/dev.sh" --command bash -c "$names")"
fi

# VS Code runs on a display of its own, where its window is the only one
# and has focus, which commands such as Run to Cursor require. A desktop's
# compositor may refuse focus to a window that is not in front, and the
# test then fails only when the user is looking elsewhere.
"$MICROSCOPE_UAT_XVFB" -displayfd 3 -nolisten tcp -screen 0 1920x1080x24 3>"$profile/display" 2>/dev/null &
xvfb=$!
trap 'kill "$xvfb" 2>/dev/null; rm -rf "$profile"' EXIT
for _ in $(seq 100); do
    [[ -s $profile/display ]] && break
    sleep 0.1
done
read -r display <"$profile/display" || { echo "Xvfb did not start" >&2; exit 1; }
export DISPLAY=:$display
unset WAYLAND_DISPLAY XDG_SESSION_TYPE NIXOS_OZONE_WL ELECTRON_OZONE_PLATFORM_HINT

MICROSCOPE_UAT_ROOT=$root MICROSCOPE_UAT_RECORD=$(realpath -m "$record") "${VSCODE_EXECUTABLE:?set VSCODE_EXECUTABLE to the Electron executable of VS Code}" \
    --ozone-platform=x11 \
    --disable-gpu \
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
