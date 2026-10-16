"""Spins in Python code entered from C, for as many seconds as its argument
says. Under -X perf, CPython enters each such code object through a
trampoline of its own, in anonymous memory it names in /tmp/perf-PID.map."""

import sys
import time


def leaf(deadline):
    while time.monotonic() < deadline:
        pass


def middle(deadline):
    # map() calls leaf from C, through leaf's trampoline.
    list(map(leaf, [deadline]))


middle(time.monotonic() + float(sys.argv[1]))
