





import os
import sys
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



microscope.frame('started')
sys.stdin.readline()

for round in range(3):
    with microscope.label('round', round):
        rows = load(round + 1)
        microscope.plot('rows', len(rows))
    microscope.message('said', lambda: f'round {round}')


def module(name):


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



for number in range(4100):
    microscope.label(f'request {number}', number)
print('PROBES DONE', flush=True)
