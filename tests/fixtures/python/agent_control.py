"""Published native commands, acknowledgement, bounded stack and recovery."""
import json
import os
import sys
import threading
import types

# As the launch bootstrap records on 3.12, which cannot say which thread
# is the main one.
sys.modules.setdefault('_microscope_bootstrap', types.SimpleNamespace(main=threading.get_ident()))
agent = types.ModuleType('control_agent')
agent._bootstrap_lock = threading.RLock()
exec(compile(open(sys.argv[1]).read(), sys.argv[1], 'exec'), agent.__dict__)
library = os.path.abspath(sys.argv[3])
original_transport = agent.transport

def checked_transport(path):
    # Loading a DSO can wait on the loader lock. A monitoring callback on
    # its owner must never wait on our configuration lock in the opposite order.
    assert not agent.lock._is_owned(), 'transport loaded under agent lock'
    return original_transport(path)

agent.transport = checked_transport
agent.apply(library, 0, ())

def target():
    value = 41
    return value + 1

line = target.__code__.co_firstlineno + 1
probe = ('LINE', 1, 42, __file__, line, '')
records = []
depths = []
limit = 1001 if sys.argv[2] == "chain" else 1

def queue(generation, probes):
    payload = json.dumps([generation, probes]).encode()
    assert len(payload) <= agent.CONTROL_CAPACITY
    agent._control[32:32 + len(payload)] = payload
    agent._control[16:20] = len(payload).to_bytes(4, 'little')
    agent._control[8:16] = generation.to_bytes(8, 'little')

def record(*args):
    assert not agent.lock._is_owned()
    depth, frame = 0, sys._getframe()
    while frame is not None:
        depth += 1
        frame = frame.f_back
    depths.append(depth)
    records.append(args)
    if args[0] == 0 and args[1] < limit:
        queue(args[1] + 1, (probe,))

agent.hook = record
agent.apply(library, 1, (probe,))
assert agent.generation == limit
assert [r[1] for r in records if r[0] == 0] == list(range(1, limit + 1))
assert max(depths) - min(depths) == 0, (min(depths), max(depths))
records.clear()
assert target() == 42
assert records == [(1, 42, 0)], records
# A malformed published request is contained; the target result is unchanged.
records.clear()
agent._control[32:33] = b'!'
agent._control[16:20] = (1).to_bytes(4, 'little')
agent._control[8:16] = (limit + 1).to_bytes(8, 'little')
assert target() == 42
assert records == [(3, limit + 1, 2)], records
assert agent.tool is None
# Cooperative idle activation uses the same owner but a later generation.
agent.apply(library, limit + 2, (probe,))
records.clear()
assert target() == 42
assert records == [(1, 42, 0)], records
agent.apply(library, limit + 3, ())
assert agent.tool is None
if sys.argv[2] == 'batch':
    # The probed line's callback consumes the configuration published by
    # the line before it and hits the site. Reporting that acknowledgement
    # accepts another configuration that keeps the site armed, so the hit
    # is still reported, once.
    def batch_target():
        queue(agent.generation + 1, (batch_probe,))
        value = 41
        return value + 1

    batch_probe = ('LINE', 1, 44, __file__, batch_target.__code__.co_firstlineno + 2, '')
    base = agent.generation + 1

    def batch_record(*args):
        records.append(args)
        if args[0] == 0 and args[1] == base + 1:
            queue(base + 2, (batch_probe,))

    agent.hook = batch_record
    agent.apply(library, base, (batch_probe,))
    records.clear()
    assert batch_target() == 42
    assert [r[1] for r in records if r[0] == 0] == [base + 1, base + 2], records
    assert records.count((1, 44, 0)) == 1, records
    agent.hook = record
    agent.apply(library, base + 3, ())
    assert agent.tool is None
print('CONTROL AGENT OK', sys.argv[2])
