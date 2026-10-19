"""Steps between Python and the C functions it calls: into an extension's
function its debug information describes, out of it and over its last
line back to the Python that called it, and through what nothing
describes, a builtin and a stripped extension's function, to the next
line; out of the extension's function from a breakpoint there, where
the agent watches no line; and over the lines of an extension's function,
one of which calls back into Python. The first argument is the directory of the
extensions built for the interpreter.
"""

import sys

sys.path.insert(0, sys.argv[1])
import microscope_bare  # noqa: E402
import microscope_extension  # noqa: E402


def unused():
    pass  # UNUSED


def identity(value):
    return value


def main():
    first = microscope_extension.reached(1)  # FIRST_CALL
    second = microscope_extension.reached(2)  # SECOND_CALL
    counted = len([first, second])  # COUNTED
    microscope_bare.reached(counted)  # BARE_CALL
    microscope_extension.reached(identity(3))  # THIRD_CALL
    print("NATIVE", counted)  # NATIVE_END
    microscope_extension.inspect(first, second, counted, None, lambda: identity(4))  # INSPECT


main()
