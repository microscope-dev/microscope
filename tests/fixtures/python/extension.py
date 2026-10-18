"""Python objects held in a C extension's native frame: the extension's
inspect() keeps them in PyObject * locals while it calls back into Python,
which checkpoints with the frame below it. The first argument is the
directory of the extension built for the interpreter.

Besides the frames and locals, the checkpoint reports what microscope must
show for each object the native frame holds:

    TRUTH  object  NAME  VALUE
"""

import os
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
sys.path.insert(0, sys.argv[1])
import truth  # noqa: E402
import microscope_extension  # noqa: E402


class Point:
    def __init__(self, x, y):
        self.x = x
        self.y = y


def main():
    point = Point(3, 4)
    numbers = [1, 2, 3]
    mapping = {"a": 1, "b": "two"}
    held = {"point": point, "numbers": numbers, "mapping": mapping, "nothing": None}
    records = [("object", name, truth.canonical(value)) for name, value in held.items()]
    microscope_extension.inspect(
        point, numbers, mapping, None, lambda: truth.checkpoint("parked", records=records)
    )


main()
