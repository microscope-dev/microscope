"""Spins in Python code entered from C until the recorder ends the process.
Under -X perf, CPython enters each such code object through a trampoline
of its own, in anonymous memory it names in /tmp/perf-PID.map.

It spins in early first, until a line arrives on its standard input, which
the test writes once the recorder has named early through the map: so the
recorder read the map before leaf's trampoline exists. CPython makes a code
object's trampoline when the code first runs, so leaf's line is written
after a read, into memory the recorder already named.
"""


import resource
import sys
import threading

# Also bound accidental manual runs; the recorder stops normal runs much earlier.
resource.setrlimit(resource.RLIMIT_CPU, (60, 60))


def early(unused):
    released = threading.Thread(target=sys.stdin.readline)
    released.start()
    while released.is_alive():
        pass


def leaf(unused):
    while True:
        pass


def middle():
    # map() calls each function from C, through its trampoline.
    list(map(early, [None]))
    list(map(leaf, [None]))


middle()
