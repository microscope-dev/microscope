"""Values microscope's expressions read in a Python frame, and what CPython
itself gives for each expression the test passes as an argument, evaluated
in that frame before it parks: names a local, a global, or a built-in
binds, numbers at their edges, text beyond ASCII, and dicts whose keys are
found by hash among collisions, deleted entries, and a key whose own type
decides what it equals."""

import os
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import truth  # noqa: E402


class Key:
    """A key whose hash and equality are its own type's code."""

    def __init__(self, hash_):
        self.hash_ = hash_

    def __hash__(self):
        return self.hash_

    def __eq__(self, other):
        return isinstance(other, Key) and other.hash_ == self.hash_


class Point:
    def __init__(self, x, y):
        self.x = x
        self.y = y


# The expressions to evaluate and what CPython gave for each, kept out of
# the frame's locals, which the checkpoint reports.
EXPRESSIONS = sys.argv[1:]
RESULTS = []

# Globals: one a local shadows, one shadowing a built-in, and a dict of
# more entries than an inspection reads, with every third deleted.
LIMIT = 7
count = "the global count"
max = "the global max"
many = {"k%d" % index: index for index in range(300)}
for index in range(0, 300, 3):
    del many["k%d" % index]
del index


def values():
    count = 7
    negative = -12
    ratio = 0.5
    flag = True
    name = "ann"
    wide = "日本語"
    astral = "snake \U0001f40d"
    data = b"ab"
    big = 2**100
    huge = 2**200
    numbers = [10, 20, 30]
    empty = []
    pair = (1, "two")
    nested = {"a": [1, {"b": (2, 3)}]}
    ages = {"ann": 30, "bob": 41, wide: 3, astral: 4}
    squares = {1: 1, 2: 4, 3: 9, -1: 1, 2**40: 0, 0.5: 0.25}
    labels = {None: "none", True: "yes", (1, 2): "pair", b"k": "bytes"}
    doubtful = {Key(7): "seven", 1.5: "float", "s": "str"}
    point = Point(3, 4)
    RESULTS.extend(truth.evaluated(EXPRESSIONS, sys._getframe()))
    truth.checkpoint("parked", records=RESULTS)
    return (count, negative, ratio, flag, name, wide, astral, data, big, huge,
            numbers, empty, pair, nested, ages, squares, labels, doubtful, point)


values()
