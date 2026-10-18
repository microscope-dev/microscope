"""A program that runs Python for a while and then replaces itself with
another interpreter running profiled.py, which the first argument names: a
profile must read the second program's frames by its own interpreter, not
by what the first one's modules said.
"""

import os
import sys
import time

deadline = time.monotonic() + 0.5
total = 0
while time.monotonic() < deadline:
    total += 1

profiled = os.path.join(os.path.dirname(os.path.abspath(__file__)), 'profiled.py')
os.execv(sys.argv[1], [sys.argv[1], '-I', '-B', profiled])
