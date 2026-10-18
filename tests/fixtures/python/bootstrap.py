"""What a program launched through microscope's bootstrap for CPython 3.12
and 3.13 sees: its own PYTHONPATH and sys.path, its own sitecustomize, none
of microscope's variables, and one thread; and what a child it starts sees.
It then counts until a breakpoint, set as it runs, stops it.
"""
import builtins
import os
import subprocess
import sys
import threading
import time

SEEN = ("import os, sys; print(os.environ.get('PYTHONPATH'), "
        "sorted(n for n in os.environ if n.startswith('MICROSCOPE_PYTHON')), "
        "any('.microscope-python-' in e for e in sys.path))")

print("SITE", getattr(builtins, "MICROSCOPE_TEST_SITE", None), flush=True)
print("SEES", os.environ.get("PYTHONPATH"),
      sorted(n for n in os.environ if n.startswith("MICROSCOPE_PYTHON")),
      any(".microscope-python-" in entry for entry in sys.path), flush=True)
print("THREADS", threading.active_count(), flush=True)
child = subprocess.run([sys.executable, "-c", SEEN], capture_output=True, text=True)
print("CHILD", child.stdout.strip(), flush=True)
print("READY", flush=True)
count = 0
while True:
    count += 1  # LOOP_LINE
    time.sleep(0.01)
