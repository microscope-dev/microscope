"""Deterministic monitoring transport contracts, isolated per scenario."""
import sys
import threading
import types

agent = types.ModuleType('race_agent')
agent._bootstrap_lock = threading.RLock()
exec(compile(open(sys.argv[1]).read(), sys.argv[1], 'exec'), agent.__dict__)
old_reports, new_reports = [], []
agent.hook = lambda *args: old_reports.append(args)

def target():
    value = 1
    return value

first = target.__code__.co_firstlineno + 1
mode = sys.argv[2]
old_line = first if mode == 'capture' else first + 1
old_probe = ('LINE', 1, 42, __file__, old_line, '')
new_probe = ('LINE', 1, 43, __file__, first, '')
agent.apply('unused', 1, (old_probe,))
entered, release = threading.Event(), threading.Event()
errors = []

class GateLock:
    def __init__(self):
        self.inner = agent.lock
        self.once = False
    def __enter__(self):
        self.inner.acquire()
        return self
    def __exit__(self, *args):
        parent = sys._getframe(1).f_locals
        selected = (parent.get('code') is target.__code__
                    and parent.get('event') == 'LINE'
                    and parent.get('location') == first
                    and not self.once)
        if selected:
            self.once = True
        self.inner.release()
        if selected:
            entered.set()
            assert release.wait(10), 'release the old callback'
    def _is_owned(self):
        return self.inner._is_owned()

agent.lock = agent._bootstrap_lock = GateLock()

def run():
    try:
        assert target() == 1
    except BaseException as error:
        errors.append(error)

worker = threading.Thread(target=run)
worker.start()
try:
    assert entered.wait(10), 'pause after callback lock release'
    if mode == 'capture':
        agent.hook = lambda *args: new_reports.append(args)
    agent.apply('unused', 2, (new_probe,))
finally:
    release.set()
    worker.join(10)
assert not worker.is_alive() and not errors, errors
if mode == 'capture':
    assert (1, 42, 0) in old_reports, (old_reports, new_reports)
    assert (1, 42, 0) not in new_reports, (old_reports, new_reports)
else:
    old_reports.clear()
    assert target() == 1
    assert (1, 43, 0) in old_reports, old_reports
agent.apply('unused', 3, ())
print('RACE OK', mode)
