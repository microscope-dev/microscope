"""Fault injection: remove polling after the first accepted configuration."""
import contextvars
import os
import sys
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import truth


def target():
    value = 41  # FIRST_LINE
    return value + 1


truth.checkpoint('initial')
contextvars.copy_context().run(target)
sys.modules['_microscope_agent'].clear()
truth.checkpoint('faulted')
while True:
    value = 42  # SECOND_LINE
