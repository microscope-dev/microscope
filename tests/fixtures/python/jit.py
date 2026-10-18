"""A loop hot enough for CPython's JIT to compile, which calls the
extension's C function on every pass: a native breakpoint there, set once
the loop has run long enough, stops in C that the JIT's code called. The
first argument is the directory of the extension built for the
interpreter.
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
spin()
