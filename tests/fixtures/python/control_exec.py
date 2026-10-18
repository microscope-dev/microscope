"""Exec into the same interpreter and source, requiring fresh installation:
the agent's, or on 3.12 and 3.13 none, since only a launch has a bootstrap."""
import os
import sys


def target():
    value = 41  # TARGET_LINE
    return value + 1


print('EXEC RESULT', target(), flush=True)
agent = sys.modules.get('_microscope_agent')
print('AGENT LIBRARY', agent._owner if agent else None, flush=True)
# 3.12's and 3.13's launch bootstrap, by the FIFO it reads.
for fd in os.listdir('/proc/self/fd'):
    try:
        held = os.readlink(f'/proc/self/fd/{fd}')
    except OSError:
        continue
    if '.microscope-python-' in held:
        print('BOOTSTRAP', os.path.dirname(held), flush=True)
if '--after-exec' not in sys.argv:
    os.execv(sys.executable, [sys.executable, '-I', '-B', __file__, *sys.argv[1:], '--after-exec'])
elif '--park' in sys.argv:
    print('PARKED', flush=True)
    sys.stdin.readline()
