#!/usr/bin/env bash
exec /run/wrappers/bin/microscope-bpf MICROSCOPE_EXPERIMENT_HOOKS=backstop MICROSCOPE_EXPERIMENT_MISSES=/tmp/hookexp/misses.$$ "$@"
