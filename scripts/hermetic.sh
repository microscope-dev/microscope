#!/usr/bin/env bash



set -euo pipefail

if [[ "${MICROSCOPE_HERMETIC:-}" == 1 ]]; then
    exec "$@"
fi
exec "$(dirname "${BASH_SOURCE[0]}")/dev.sh" --command "$@"
