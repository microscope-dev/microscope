"""A command boundary, reentrant user code, and preserved user interrupts."""
import json
import sys
import threading
import types

agent = types.ModuleType('_microscope_agent')
agent._bootstrap_lock = threading.RLock()
exec(compile(open(sys.argv[1]).read(), sys.argv[1], 'exec'), agent.__dict__)
library = sys.argv[3]
agent.apply(library, 0, ())
records = []
native_hook = agent.hook

def record(*report):
    records.append(report)
    if '--native' in sys.argv:
        native_hook(*report)

agent.hook = record


def target():
    return 42


probe = ('LINE', 1, 42, __file__, target.__code__.co_firstlineno + 1, '')
mode = sys.argv[2]


def queue(generation, probes):
    payload = json.dumps([generation, probes]).encode()
    agent._control[32:32 + len(payload)] = payload
    agent._control[16:20] = len(payload).to_bytes(4, 'little')
    agent._control[8:16] = generation.to_bytes(8, 'little')


if mode == 'drain':
    queued = False
    def record(*report):
        global queued
        records.append(report)
        if report == (1, 42, 0) and not queued:
            queued = True
            queue(2, (probe,))
    agent.hook = record
    # A step's end is reported after the breakpoint hit at its line, so the
    # configuration consumed at the hit supersedes it.
    agent.apply(library, 1, (probe, ('LINE', 2, 43, __file__, probe[4], '')))
    records.clear()
    assert target() == 42
    assert [report[:2] for report in records] == [(1, 42), (0, 2)], records
    records.clear()
    assert target() == 42
    assert records == [(1, 42, 0)], records
elif mode == 'drain_together':
    # A configuration consumed at a report of several hits adds one at the
    # same line; only the new hit is reported after it.
    import ctypes
    queued = False
    def record(*report):
        global queued
        if report[0] == 1 and report[2]:
            numbers = (ctypes.c_uint64 * report[1]).from_address(report[2])
            report = (1, tuple(numbers))
        records.append(report)
        if report == (1, (42, 43)) and not queued:
            queued = True
            queue(2, (probe, ('LINE', 1, 43, __file__, probe[4], ''),
                      ('LINE', 1, 44, __file__, probe[4], '')))
    agent.hook = record
    agent.apply(library, 1, (probe, ('LINE', 1, 43, __file__, probe[4], '')))
    records.clear()
    assert target() == 42
    assert [report[:2] for report in records] == [(1, (42, 43)), (0, 2), (1, 44)], records
elif mode == 'drain_race':
    entered, release = threading.Event(), threading.Event()
    errors = []
    def record(*report):
        records.append(report)
        if report == (1, 42, 0) and threading.current_thread() is worker:
            entered.set()
            assert release.wait(10), 'release first captured notice'
    def run():
        try:
            assert target() == 42
        except BaseException as error:
            errors.append(error)
    agent.hook = record
    # The step's end, reported after the hit, is superseded by another
    # thread's configuration while the hit's report is in the hook.
    agent.apply(library, 1, (probe, ('LINE', 2, 43, __file__, probe[4], '')))
    records.clear()
    worker = threading.Thread(target=run)
    worker.start()
    try:
        assert entered.wait(10), 'first notice reaches the hook'
        agent.apply(library, 2, (probe,))
    finally:
        release.set()
        worker.join(10)
    assert not worker.is_alive() and not errors, errors
    assert (1, 42, 0) in records, records
    assert not any(report == (2, 43, 0) for report in records), records
elif mode == 'malformed':
    agent.apply(library, 1, (probe,))
    generation = 2
    for defect in ('reserved', 'magic', 'capacity', 'zero', 'oversize',
                   'boolean', 'mismatch', 'object', 'probe', 'deep', 'integer',
                   'utf16', 'utf32', 'utf16deep'):
        records.clear()
        request = [generation, [probe]]
        if defect == 'boolean':
            request[0] = True
        elif defect == 'mismatch':
            request[0] += 1
        elif defect == 'object':
            request = {'generation': generation}
        elif defect == 'probe':
            request[1] = [['LINE', 1, 0, '', 0, '']]
        payload = json.dumps(request).encode()
        if defect == 'deep':
            payload = b'[' * 2000 + b'0' + b']' * 2000
        elif defect == 'integer':
            payload = b'[' + b'9' * 5000 + b', []]'
        elif defect == 'utf16':
            payload = json.dumps(request).encode('utf-16-le')
        elif defect == 'utf32':
            payload = json.dumps(request).encode('utf-32-be')
        elif defect == 'utf16deep':
            payload = ('["\u2200",' + '[' * 16000 + ']' * 16000 + ']').encode('utf-16-le')
        agent._control[32:32 + len(payload)] = payload
        length = 0 if defect == 'zero' else (65537 if defect == 'oversize' else len(payload))
        agent._control[16:20] = length.to_bytes(4, 'little')
        if defect == 'reserved':
            agent._control[24] = 1
        elif defect == 'magic':
            agent._control[0] = 33
        elif defect == 'capacity':
            agent._control[20:24] = (1).to_bytes(4, 'little')
        agent._control[8:16] = generation.to_bytes(8, 'little')
        assert target() == 42
        assert records == [(3, generation, 2)], (defect, records)
        assert agent.tool is None
        agent._control[:8] = agent.CONTROL_MAGIC
        agent._control[20:24] = agent.CONTROL_CAPACITY.to_bytes(4, 'little')
        agent._control[24:32] = bytes(8)
        generation += 1
        agent.apply(library, generation, (probe,))
        generation += 1
    agent.apply(library, generation, ())
elif mode == 'arming_fault':
    report = agent.callback('PY_START')
    agent.apply(library, 1, (('PY_START', 1, 99, '', 0, 'not_present'),))
    original = agent.discover
    def discover(code):
        if code is target.__code__:
            raise MemoryError('post-configuration discovery failed')
        return original(code)
    agent.discover = discover
    records.clear()
    queue(2, (probe,))
    try:
        report(target.__code__, 0)
    finally:
        agent.discover = original
    assert records == [(3, 2, 2)], records
    assert agent.tool is None
elif mode == 'reentry':
    agent.apply(library, 1, (probe,))
    original = sys.monitoring.set_events
    def instrument(tool, mask):
        original(tool, mask)
        if mask:
            assert target() == 42
    sys.monitoring.set_events = instrument
    records.clear()
    try:
        agent.apply(library, 2, (probe,))
    finally:
        sys.monitoring.set_events = original
    assert records == [(3, 2, 5)], records
    assert agent.tool is None
elif mode == 'hook_interrupt':
    def record(*report):
        records.append(report)
        if report[:2] == (0, 2):
            raise KeyboardInterrupt('after hook')
    agent.hook = record
    caught = False
    try:
        agent.apply(library, 2, (probe,))
    except KeyboardInterrupt as error:
        caught = str(error) == 'after hook'
    assert caught
    assert records[-1] == (3, 2, 6), records
    assert agent.tool is None
elif mode == 'reentry_race':
    agent.apply(library, 1, (probe,))
    entered, release = threading.Event(), threading.Event()
    errors = []
    def record(*report):
        records.append(report)
        if report[:2] == (0, 2):
            assert target() == 42
            entered.set()
            assert release.wait(10), 'release old acknowledgement'
    def run():
        try:
            agent.apply(library, 2, (probe,))
        except BaseException as error:
            errors.append(error)
    agent.hook = record
    records.clear()
    worker = threading.Thread(target=run)
    worker.start()
    try:
        assert entered.wait(10), 'old acknowledgement is outside the lock'
        agent.apply(library, 3, (probe,))
    finally:
        release.set()
        worker.join(10)
    assert not worker.is_alive() and not errors, errors
    assert not any(report[:2] == (3, 3) for report in records), records
    assert agent.generation == 3 and agent.tool is not None
    records.clear()
    assert target() == 42
    assert records == [(1, 42, 0)], records
elif mode == 'interrupt_race':
    agent.apply(library, 1, (probe,))
    entered, release = threading.Event(), threading.Event()
    main = threading.get_ident()
    original_loads = agent.json.loads
    original_lock = agent.lock
    errors = []
    class GateLock:
        def __enter__(self):
            original_lock.acquire()
            return self
        def __exit__(self, kind, error, traceback):
            selected = kind is KeyboardInterrupt and threading.get_ident() != main
            original_lock.release()
            if selected:
                entered.set()
                assert release.wait(10), 'release interrupted callback'
    def interrupted(*args, **kwargs):
        raise KeyboardInterrupt('old generation')
    def run():
        try:
            queue(2, (probe,))
            target()
        except KeyboardInterrupt as error:
            errors.append(str(error))
    agent.lock = GateLock()
    agent.json.loads = interrupted
    records.clear()
    worker = threading.Thread(target=run)
    worker.start()
    try:
        assert entered.wait(10), 'interruption releases the agent lock'
        agent.apply(library, 3, (probe,))
    finally:
        release.set()
        worker.join(10)
        agent.lock = original_lock
        agent.json.loads = original_loads
    assert not worker.is_alive() and errors == ['old generation'], errors
    assert (3, 2, 6) in records, records
    assert not any(report[:2] == (3, 3) for report in records), records
    assert agent.generation == 3 and agent.tool is not None
    records.clear()
    assert target() == 42
    assert records == [(1, 42, 0)], records
elif mode in ('keyboard', 'exit', 'custom', 'cancelled', 'timeout'):
    agent.apply(library, 1, (probe,))
    original = agent.json.loads
    previous_handler = None
    if mode == 'keyboard':
        wanted = KeyboardInterrupt
    elif mode == 'exit':
        wanted = SystemExit
    elif mode == 'cancelled':
        import asyncio
        wanted = asyncio.CancelledError
    elif mode == 'custom':
        class UserInterruption(BaseException):
            pass
        wanted = UserInterruption
    else:
        import signal
        wanted = TimeoutError
        def timeout(signum, frame):
            raise TimeoutError('user interruption')
        previous_handler = signal.signal(signal.SIGALRM, timeout)
    def interrupted(*args, **kwargs):
        if mode == 'timeout':
            signal.raise_signal(signal.SIGALRM)
            raise AssertionError('SIGALRM handler did not run')
        raise wanted('user interruption')
    agent.json.loads = interrupted
    records.clear()
    caught = False
    try:
        queue(2, (probe,))
        target()
    except wanted as error:
        caught = str(error) == 'user interruption'
    finally:
        agent.json.loads = original
        if mode == 'timeout':
            signal.signal(signal.SIGALRM, previous_handler)
    assert caught, (mode, records)
    assert records == [(3, 2, 6)], records
    assert agent.tool is None
elif mode == 'fork':
    # A process forks while another thread holds the agent's lock in a
    # callback: the fork waits for it, so the child, whose fork handlers
    # run monitored before the agent's, never waits on a lock no thread of
    # its own holds. The child is disarmed; the parent keeps its agent.
    import _thread
    import os
    import select
    import time
    # Only `target` takes line events, so the module's own lines, which
    # fork, run as uninstrumented code does, never waiting for the lock.
    agent.apply(library, 1, (probe[:5] + ('target',),), None, agent.RAISED)
    # Native locks, whose waits call no Python function the agent would
    # take its lock to discover while the holder keeps it.
    go, held = _thread.allocate_lock(), _thread.allocate_lock()
    go.acquire()
    held.acquire()

    def holder():
        go.acquire()
        with agent.lock:
            held.release()
            time.sleep(0.2)

    thread = threading.Thread(target=holder)
    thread.start()
    go.release()
    held.acquire()
    reading, writing = os.pipe()
    child = os.fork()
    if child == 0:
        os.close(reading)
        try:
            records.clear()
            target()
            try:
                raise ValueError('child')
            except ValueError:
                pass
            with agent.lock:
                pass
            tools = [tool for tool in range(6) if sys.monitoring.get_tool(tool)]
            disarmed = (agent.tool is None and not tools and not records
                        and agent.probes == () and agent.raising == 0)
            os.write(writing, b'disarmed' if disarmed else repr((tools, records)).encode())
        finally:
            os._exit(0)
    os.close(writing)
    thread.join()
    ready, _, _ = select.select([reading], [], [], 10)
    if not ready:
        os.kill(child, 9)
        os.waitpid(child, 0)
        raise AssertionError('the forked child hung')
    reported = os.read(reading, 1024)
    _, status = os.waitpid(child, 0)
    assert os.waitstatus_to_exitcode(status) == 0 and reported == b'disarmed', reported
    records.clear()
    target()
    assert records == [(1, 42, 0)] and agent.tool is not None, records
else:
    raise AssertionError(mode)
agent.apply(library, agent.generation + 1, ())
print('SAFETY AGENT OK', mode)
