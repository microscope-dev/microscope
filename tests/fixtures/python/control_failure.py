"""A real interpreter whose monitoring slots all belong to the application."""
import contextvars
import os
import sys
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import truth


def target():
    value = 41  # TARGET_LINE
    return value + 1


for number in range(6):
    sys.monitoring.use_tool_id(number, 'application-' + str(number))
truth.checkpoint('initial')
print('FAILURE RESULT', contextvars.copy_context().run(target), flush=True)
print('APPLICATION TOOLS', [sys.monitoring.get_tool(i) for i in range(6)], flush=True)
