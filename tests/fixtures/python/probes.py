"""A program instrumented with microscope's Python package: it waits until
microscope listens, then marks its work every way the package can, and ends
by timing the calls of its own module, then defines more sites than the
library holds.
"""
import os
import sys
import time
import types

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

# A recording that starts the program switches it on at once, but its marks
# are safe only once microscope has found it.
microscope.frame('started')
deadline = time.monotonic() + 30
while not microscope.found():
    assert time.monotonic() < deadline, 'microscope never found the program'
    time.sleep(0.001)

for round in range(3):
    with microscope.label('round', round):
        rows = load(round + 1)
        microscope.plot('rows', len(rows))
    microscope.message('said', lambda: f'round {round}')


def module(name):
    """A module named `name` defining `work`, whose code equals every
    other such module's: code objects compare without their files."""
    made = types.ModuleType(name)
    exec(compile('def work():\n    return 1\n', f'<{name}>', 'exec'), made.__dict__)
    return made


traced, other = module('__main__.traced'), module('elsewhere')
microscope.trace_calls('__main__')
fib(2)
list(numbers())
other.work()  # Not traced, so off where it begins.
traced.work()  # Traced, though its code equals the other's.
microscope.trace_calls('elsewhere')
other.work()  # Traced from now on.

# A name made anew for each use is a site of its own, until every site the
# library's table holds is taken.
for number in range(4100):
    microscope.label(f'request {number}', number)
print('PROBES DONE', flush=True)
