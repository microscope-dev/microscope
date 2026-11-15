






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
