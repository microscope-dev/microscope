"""System calls made from marked lines: on the main thread, and on a worker
through a call into C that calls back into Python, so that the worker's
stack has two activations of the interpreter's loop. Each opens a path that
names the call.

With --hold, for the BPF engine, which reads the interpreter a moment after
the program starts and reads each frame's code as it drains its records:
until standard input says "go", the program opens /dev/random from `probe`,
so the test sees when the engine names its Python frames; then it makes its
calls, and stays alive until standard input ends, so the test has read
their frames before the program is gone."""

import contextvars
import os
import select
import sys
import threading


def opens(path):
    os.close(os.open(path, os.O_RDONLY))  # OPENS


def through_c(path):
    # Context.run is C, which calls opens: a second evaluation loop.
    contextvars.copy_context().run(opens, path)  # THROUGH_C


def worker():
    through_c('/dev/null')  # WORKER


def probe():
    os.close(os.open('/dev/random', os.O_RDONLY))


hold = '--hold' in sys.argv
if hold:
    while not select.select([sys.stdin], [], [], 0.01)[0]:
        probe()
    sys.stdin.readline()

thread = threading.Thread(target=worker)
thread.start()
thread.join()
opens('/proc/self/stat')  # MAIN

if hold:
    sys.stdin.read()
