



import contextvars
import os
import sys
import threading

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import truth  # noqa: E402


def leaf(gate):
    gate.arrive()


def through_c(gate):

    contextvars.copy_context().run(leaf, gate)


def recurse(depth, gate):
    if depth == 0:
        return through_c(gate)
    return recurse(depth - 1, gate)


gate = truth.Gate(2)
workers = [
    threading.Thread(target=recurse, args=(3, gate), name="deep"),
    threading.Thread(target=leaf, args=(gate,), name="shallow"),
]
for worker in workers:
    worker.start()

truth.code_table()
truth.checkpoint("parked", gate, abort="--abort" in sys.argv)
