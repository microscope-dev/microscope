





import dataclasses
import sys
import threading
import time

stopping = threading.Event()
MINIMUM = 0.3


def trace(line):
    for _ in range(10_000):
        pass




Line = dataclasses.make_dataclass("Line", ["end"], namespace={"__post_init__": trace})


def draw(point):
    point.line = Line(0)




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
