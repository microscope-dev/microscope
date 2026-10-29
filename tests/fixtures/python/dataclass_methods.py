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


# Many fields, so that most of the time goes to each `__init__` itself.
FIELDS = [f"field_{number}" for number in range(32)]
Point = dataclasses.make_dataclass("Point", FIELDS)
Line = dataclasses.make_dataclass("Line", FIELDS)


def build():
    values = range(len(FIELDS))
    started = time.monotonic()
    while not stopping.is_set() or time.monotonic() - started < MINIMUM:
        for _ in range(1000):
            Point(*values)
            Line(*values)


builder = threading.Thread(target=build, name="builder")
builder.start()
sys.stdin.readline()
stopping.set()
builder.join()
