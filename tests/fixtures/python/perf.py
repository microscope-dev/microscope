"""Spins in Python code entered from C until the recorder ends the process.
Under -X perf, CPython enters each such code object through a trampoline
of its own, in anonymous memory it names in /tmp/perf-PID.map.
"""


import resource

# Also bound accidental manual runs; the recorder stops normal runs much earlier.
resource.setrlimit(resource.RLIMIT_CPU, (60, 60))


def leaf(unused):
    while True:
        pass


def middle():
    # map() calls leaf from C, through leaf's trampoline.
    list(map(leaf, [None]))


middle()
