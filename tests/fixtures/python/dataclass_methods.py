"""A program to profile: a thread makes instances of two dataclasses, whose
methods `dataclasses` compiles from strings, until a line arrives on
standard input, and for at least `MINIMUM` seconds: a recording released
by time may take most of that time to start.
"""

import dataclasses
import sys
import threading
import time

stopping = threading.Event()
MINIMUM = 0.3


def trace(line):
    for _ in range(10_000):
        pass


# `Line.__init__` calls `trace`, where nearly all the time goes, so that
# nearly every snapshot finds it; one under `just stress` can take seconds.
Line = dataclasses.make_dataclass("Line", ["end"], namespace={"__post_init__": trace})


def draw(point):
    point.line = Line(0)


# `Point.__init__` makes a `Line`, so that one stack, as one snapshot takes,
# names both methods.
Point = dataclasses.make_dataclass("Point", ["origin"], namespace={"__post_init__": draw})


def build():
    started = time.monotonic()
    while not stopping.is_set() or time.monotonic() - started < MINIMUM:
        for _ in range(100):
            Point(0)


builder = threading.Thread(target=build, name="builder")
builder.start()
sys.stdin.readline()
stopping.set()
builder.join()
