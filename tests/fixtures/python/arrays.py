"""numpy arrays, which keep their elements as the machine does: of each
element type microscope reads, of one axis and of several, laid out next
to each other, with strides, backwards, and transposed, an array of no
axes, an empty one, one past a summary's bounds, a class deriving from
numpy's, arrays of elements microscope does not read, and a list, whose
numbers are objects.

The test passes the directory numpy is in, then expressions, which CPython
evaluates in the frame before it parks. Besides each expression's result,
the checkpoint reports the children of each local array that has any, in
order, as truth.canonical() gives each:

    TRUTH  child  LOCAL  VALUE
"""

import os
import sys

sys.path.insert(0, sys.argv[1])
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import numpy  # noqa: E402
import truth  # noqa: E402

EXPRESSIONS = sys.argv[2:]
RESULTS = []


class Tagged(numpy.ndarray):
    """An array of a class deriving from numpy's."""


def values():
    floats = numpy.array([1.5, -2.0, 0.1, 1e16])
    singles = numpy.array([0.1, 16777216.0, 1e-5], dtype=numpy.float32)
    grid = numpy.arange(12, dtype=numpy.int32).reshape(3, 4)
    image = numpy.zeros((2, 3, 3), dtype=numpy.uint8)
    image[1, 2] = [255, 128, 7]
    flags = numpy.array([True, False, True])
    small = numpy.array([-128, 127], dtype=numpy.int8)
    shorts = numpy.array([-32768, 32767], dtype=numpy.int16)
    huge = numpy.array([2**64 - 1, 0], dtype=numpy.uint64)
    strided = floats[::2]
    backwards = floats[::-1]
    transposed = grid.T
    scalar = numpy.array(5)
    empty = numpy.zeros((0, 3))
    long = numpy.arange(100.0)
    tagged = numpy.arange(3.0).view(Tagged)
    complex_numbers = numpy.array([1 + 2j])
    big_endian = numpy.array([1, 2], dtype=">i4")
    listed = [1.0, 2.0]

    frame = sys._getframe()
    RESULTS.extend(truth.evaluated(EXPRESSIONS, frame))
    RESULTS.extend(children(frame.f_locals))
    del frame
    truth.checkpoint("parked", records=RESULTS)


def children(locals_):
    """The child records of each local array of at most 64 elements on its
    first axis: its numbers, or its arrays of the rest."""
    records = []
    for local, value in locals_.items():
        if not isinstance(value, numpy.ndarray) or value.ndim == 0 or len(value) > 64:
            continue
        if value.dtype.num > 12 or value.dtype.byteorder == ">":
            continue
        for item in numpy.ndarray.view(value, numpy.ndarray):
            text = truth.array_text(item) if item.ndim else truth._number_text(item)
            records.append(("child", local, text))
    return records


values()
