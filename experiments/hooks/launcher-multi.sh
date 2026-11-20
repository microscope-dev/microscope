#!/usr/bin/env bash
exec /run/wrappers/bin/microscope-bpf MICROSCOPE_EXPERIMENT_HOOKS=multi MICROSCOPE_EXPERIMENT_MISSES=/tmp/hookexp/misses.$$ "$@"
