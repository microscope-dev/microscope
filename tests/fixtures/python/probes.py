"""A program instrumented with microscope's Python package: it waits until
microscope listens, then marks its work every way the package can, and ends
by timing the calls of its own module.
"""
import os
import sys
import time

sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)),
                                '..', '..', '..', 'sdk', 'python'))
import microscope  # noqa: E402

assert microscope.problem is None, microscope.problem


@microscope.zone('load')
def load(rows):
    with microscope.zone('parse'):
        return list(range(rows))


def fib(n):
    return n if n < 2 else fib(n - 1) + fib(n - 2)


def numbers():
    yield 1
    yield 2


try:
    microscope.zone('coroutine')(numbers)
except TypeError:
    pass
else:
    raise AssertionError('a zone spanned what a generator yields')

# Nothing is on until microscope reads a site it selects.
microscope.frame('started')
deadline = time.monotonic() + 30
while not microscope.listening():
    assert time.monotonic() < deadline, 'microscope never listened'
    time.sleep(0.001)

for round in range(3):
    with microscope.label('round', round):
        rows = load(round + 1)
        microscope.plot('rows', len(rows))
    microscope.message('said', lambda: f'round {round}')
microscope.trace_calls('__main__')
fib(2)
list(numbers())
print('PROBES DONE', flush=True)
