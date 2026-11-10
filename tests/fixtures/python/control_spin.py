"""Released, the main thread only runs Python: its loop calls a Python
function directly, so no C code enters the interpreter again, and a pause
almost always finds the thread among the loop's instructions.

With --release, a second line on standard input ends the loop, and the
program says which monitoring tools are registered before parking."""
import os
import sys
import threading
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import truth

released = False


def release():
    global released
    sys.stdin.readline()
    released = True


def tick(count):
    return count + 1  # TARGET_LINE


def spin():
    count = 0
    while not released:
        count = tick(count)


truth.checkpoint("initial")
if '--release' in sys.argv:
    threading.Thread(target=release, name="releaser", daemon=True).start()
print("SPINNING", flush=True)
spin()
print("RELEASED TOOLS", [sys.monitoring.get_tool(i) for i in range(6)], flush=True)
truth.checkpoint("released")
