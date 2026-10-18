"""Released, the main thread only runs Python: its loop calls a Python
function directly, so no C code enters the interpreter again, and a pause
almost always finds the thread among the loop's instructions."""
import os
import sys
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import truth


def tick(count):
    return count + 1  # TARGET_LINE


def spin():
    count = 0
    while True:
        count = tick(count)


truth.checkpoint("initial")
print("SPINNING", flush=True)
spin()
