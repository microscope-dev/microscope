"""Attach, detach, and attach again without an application agent import."""
import contextvars
import os
import sys
import threading
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import truth


def target():
    value = 41  # TARGET_LINE
    return value + 1


if '--namespace' in sys.argv:
    assert os.getpid() == 1
    status = open('/proc/self/status', 'rb').read()
    host = int(next(line.split()[1] for line in status.splitlines() if line.startswith(b'NSpid:')))
    truth.report('host_pid', host)
finished = threading.Event()
worker = threading.Thread(target=finished.wait, name="application-worker")
worker.start()
truth.checkpoint("initial")
print("ATTACH RESULT", contextvars.copy_context().run(target), flush=True)
print("DETACHED TOOLS", [sys.monitoring.get_tool(i) for i in range(6)], flush=True)
truth.checkpoint("detached")
print("REATTACH RESULT", contextvars.copy_context().run(target), flush=True)

finished.set()
worker.join()
