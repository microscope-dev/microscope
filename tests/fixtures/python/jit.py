"""A hot loop that calls the extension's C function on every pass.
The first argument is the extension's directory. With --cold-stop, wait
for input and make an interpreted call before the loop warms up.
"""

import sys

sys.path.insert(0, sys.argv[1])
import microscope_extension  # noqa: E402


def spin():
    count = 0
    while True:
        microscope_extension.reached(count)  # SPIN_CALL
        count += 1
        if count == 200000:
            print("WARM", flush=True)


print("JIT", sys._jit.is_enabled(), flush=True)
debugging = "--cold-stop" in sys.argv
if debugging:
    print("READY", flush=True)
    input()
    microscope_extension.reached(-1)
spin()
