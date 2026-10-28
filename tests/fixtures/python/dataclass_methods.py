"""A program to profile: a thread makes instances of two dataclasses, whose
methods `dataclasses` compiles from strings, until a line arrives on
standard input.
"""

import dataclasses
import sys
import threading

stopping = threading.Event()


# Many fields, so that most of the time goes to each `__init__` itself.
FIELDS = [f"field_{number}" for number in range(32)]
Point = dataclasses.make_dataclass("Point", FIELDS)
Line = dataclasses.make_dataclass("Line", FIELDS)


def build():
    values = range(len(FIELDS))
    while not stopping.is_set():
        for _ in range(1000):
            Point(*values)
            Line(*values)


builder = threading.Thread(target=build, name="builder")
builder.start()
sys.stdin.readline()
stopping.set()
builder.join()
