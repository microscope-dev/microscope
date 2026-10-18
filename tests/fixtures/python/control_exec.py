"""Exec into the same interpreter and source, requiring fresh installation."""
import os
import sys


def target():
    value = 41  # TARGET_LINE
    return value + 1


print('EXEC RESULT', target(), flush=True)
print('AGENT LIBRARY', sys.modules['_microscope_agent']._owner, flush=True)
if '--after-exec' not in sys.argv:
    os.execv(sys.executable, [sys.executable, '-I', '-B', __file__, '--after-exec'])
