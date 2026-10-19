"""Python stepping: over, into, and out of lines, calls, awaits, and raises;
then exceptions, raised and uncaught.

Each marked line is where a test breaks or expects a step to stop. The
program prints what it computed, so stopping must change nothing. It parks,
blocked in C, for a step from a stop its agent did not report, unless run
`--editor`; run `--attach`, it first says READY and waits for a byte on its
standard input, blocked in C.
"""
import asyncio
import os
import sys
import threading

if '--attach' in sys.argv:
    print('READY', flush=True)
    sys.stdin.read(1)
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import truth  # noqa: E402


def callee(value):
    doubled = value * 2  # CALLEE_FIRST
    return doubled + 1  # CALLEE_LAST


def caller():
    start = 1  # CALLER_START
    result = callee(start)  # CALLER_CALL
    after = result + 1  # CALLER_AFTER
    return after  # CALLER_RETURN


def outer():
    value = caller()  # OUTER_CALL
    return value + 1  # OUTER_RETURN


async def leaf():
    await asyncio.sleep(0)
    return 1


async def task():
    begun = 1
    await leaf()  # TASK_AWAIT
    ended = begun + 1  # TASK_END
    return ended


def raises():
    raise ValueError('stepped')


def handles():
    try:
        raises()  # HANDLES_CALL
    except ValueError:  # HANDLES_EXCEPT
        caught = True
    return caught


def lookup(table):
    return table['missing']  # LOOKUP


def caught():
    try:
        lookup({})
    except KeyError:
        return 'caught'


def worker():
    raise RuntimeError('worker failed')  # WORKER_RAISE


def deep():
    return 1  # DEEP


def shallow():
    value = deep()  # SHALLOW_CALL
    return value + 1  # SHALLOW_AFTER


print('OUTER', outer(), flush=True)
print('TASK', asyncio.run(task()), flush=True)
print('HANDLED', handles(), flush=True)
print('SHALLOW', shallow(), flush=True)
if '--editor' not in sys.argv:
    truth.checkpoint('parked')
print('CAUGHT', caught(), flush=True)
thread = threading.Thread(target=worker)
thread.start()
thread.join()
print('STEPPING DONE', flush=True)
