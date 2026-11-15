










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
