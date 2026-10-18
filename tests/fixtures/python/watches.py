"""A Python variable watched from a checkpoint in its frame: each store
that changes it stops, with its values before and after, and the watch
ends with the frame, where the next call of its function puts a frame in
its place: `total` changes, and `spare` is assigned only in the next
call's frame.
"""

import os
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import truth  # noqa: E402


def count(limit, park):
    total = 0
    if park:
        truth.checkpoint("counting")
    else:
        spare = limit  # noqa: F841
    for step in range(limit):
        total += step  # COUNT_ADD
    return total


def main():
    first = count(4, True)
    second = count(2, False)
    print("COUNTED", first, second)


main()
