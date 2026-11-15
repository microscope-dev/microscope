#!/usr/bin/env bash





set -euo pipefail

[[ $# -gt 0 ]] || { echo "usage: contained.sh COMMAND [ARGUMENT...]" >&2; exit 2; }

memory_max="${MICROSCOPE_MEMORY_MAX:-50%}"
echo 1000 >/proc/self/oom_score_adj
exec systemd-run --user --scope --quiet \
    -p MemoryMax="$memory_max" -p MemorySwapMax=0 -p OOMPolicy=continue \
    -- "$@"
