"""System calls made from marked lines: on the main thread, and on a worker
through a call into C that calls back into Python, so that the worker's
stack has two activations of the interpreter's loop. Each opens a path that
names the call."""

import contextvars
import os
import threading


def opens(path):
    os.close(os.open(path, os.O_RDONLY))  # OPENS


def through_c(path):
    # Context.run is C, which calls opens: a second evaluation loop.
    contextvars.copy_context().run(opens, path)  # THROUGH_C


def worker():
    through_c('/dev/null')  # WORKER


thread = threading.Thread(target=worker)
thread.start()
thread.join()
opens('/proc/self/stat')  # MAIN
