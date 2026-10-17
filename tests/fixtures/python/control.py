"""Ordinary application code: the debugger installs its own transport."""
import sys


def target():
    value = 41  # TARGET_LINE
    return value + 1


agent = sys.modules.get('_microscope_agent')
if agent is not None:
    print('AGENT LIBRARY', agent._owner, flush=True)
print('CONTROL RESULT', target(), flush=True)
