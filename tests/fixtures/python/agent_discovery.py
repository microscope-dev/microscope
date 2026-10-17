"""Deterministic monitoring transport contracts, isolated per scenario."""
import gc
import sys
import threading
import types
import weakref

agent = types.ModuleType('discovery_agent')
agent._bootstrap_lock = threading.RLock()
exec(compile(open(sys.argv[1]).read(), sys.argv[1], 'exec'), agent.__dict__)
reports = []
agent.hook = lambda *args: reports.append(args)

mode = sys.argv[2]

def target():
    return 1

line = target.__code__.co_firstlineno + 1
probe = ('LINE', 1, 42, __file__, line, '')
if mode == 'repeat':
    calls = []
    original = sys.monitoring.set_local_events
    def observe(tool, code, mask):
        if code is target.__code__:
            calls.append(mask)
        return original(tool, code, mask)
    sys.monitoring.set_local_events = observe
    try:
        agent.apply('unused', 1, (probe,))
        reports.clear()
        for _ in range(10):
            assert target() == 1
        assert len(calls) == 1, calls
        assert len([record for record in reports if record == (1, 42, 0)]) == 10, reports
    finally:
        sys.monitoring.set_local_events = original
elif mode == 'fault':
    original = sys.monitoring.set_local_events
    def refuse(tool, code, mask):
        if code is target.__code__:
            raise MemoryError('instrumentation fault')
        return original(tool, code, mask)
    sys.monitoring.set_local_events = refuse
    try:
        agent.apply('unused', 1, (probe,))
        reports.clear()
        assert target() == 1
        assert reports == [(3, 1, 2)], reports
        assert agent.tool is None and not agent.discovered
    finally:
        sys.monitoring.set_local_events = original
    agent.apply('unused', 2, (probe,))
    reports.clear()
    assert target() == 1
    assert reports == [(1, 42, 0)], reports
elif mode == 'resumed':
    def generator():
        yield 0
        value = 41
        yield value + 1
    suspended_generator = generator()
    assert next(suspended_generator) == 0
    generator_line = generator.__code__.co_firstlineno + 2
    agent.apply('unused', 1, (('LINE', 1, 42, __file__, generator_line, ''),))
    reports.clear()
    assert next(suspended_generator) == 42
    assert reports == [(1, 42, 0)], reports
    suspended_generator.close()

    def catching():
        try:
            yield 0
        except LookupError:
            value = 41
            yield value + 1
    thrown_generator = catching()
    assert next(thrown_generator) == 0
    thrown_line = catching.__code__.co_firstlineno + 4
    agent.apply('unused', 2, (('LINE', 1, 44, __file__, thrown_line, ''),))
    reports.clear()
    assert thrown_generator.throw(LookupError()) == 42
    assert reports == [(1, 44, 0)], reports
    thrown_generator.close()

    class WaitOnce:
        def __await__(self):
            yield
    async def coroutine():
        await WaitOnce()
        value = 41
        return value + 1
    suspended_coroutine = coroutine()
    assert suspended_coroutine.send(None) is None
    coroutine_line = coroutine.__code__.co_firstlineno + 2
    agent.apply('unused', 3, (('LINE', 1, 43, __file__, coroutine_line, ''),))
    reports.clear()
    try:
        suspended_coroutine.send(None)
    except StopIteration as finished:
        assert finished.value == 42
    else:
        raise AssertionError('coroutine did not finish')
    assert reports == [(1, 43, 0)], reports
elif mode == 'delegated':
    def inner():
        try:
            yield 0
        except LookupError:
            return 41
    def outer():
        value = yield from inner()
        value += 1
        yield value
    delegated = outer()
    assert next(delegated) == 0
    outer_line = outer.__code__.co_firstlineno + 2
    agent.apply('unused', 1, (('LINE', 1, 45, __file__, outer_line, ''),))
    reports.clear()
    assert delegated.throw(LookupError()) == 42
    assert reports == [(1, 45, 0)], reports
    delegated.close()

    import asyncio
    class WaitOnce:
        def __await__(self):
            yield
    async def inner_coroutine():
        try:
            await WaitOnce()
        except asyncio.CancelledError:
            return 41
    async def outer_coroutine():
        value = await inner_coroutine()
        value += 1
        return value
    delegated_coroutine = outer_coroutine()
    assert delegated_coroutine.send(None) is None
    outer_line = outer_coroutine.__code__.co_firstlineno + 2
    agent.apply('unused', 2, (('LINE', 1, 46, __file__, outer_line, ''),))
    reports.clear()
    try:
        delegated_coroutine.throw(asyncio.CancelledError())
    except StopIteration as finished:
        assert finished.value == 42
    else:
        raise AssertionError('delegating coroutine did not finish')
    assert reports == [(1, 46, 0)], reports
elif mode == 'identity':
    # Code equality ignores filename. A WeakSet/WeakKeyDictionary is not
    # an identity cache and would conflate these distinct source places.
    other_code = target.__code__.replace(co_filename='/unmatched.py')
    assert other_code == target.__code__ and other_code is not target.__code__
    other = types.FunctionType(other_code, globals())
    agent.apply('unused', 1, (probe,))
    reports.clear()
    assert other() == 1
    assert not reports
    assert target() == 1
    assert reports == [(1, 42, 0)], reports
elif mode == 'gc':
    agent.apply('unused', 1, (probe,))
    references = []
    identities = []
    for i in range(100):
        code = target.__code__.replace(co_filename=f'/unused-{i}.py')
        function = types.FunctionType(code, globals())
        references.append(weakref.ref(code))
        identities.append(id(code))
        assert function() == 1
        del function, code
    gc.collect()
    assert all(ref() is None for ref in references)
    assert all(identity not in agent.discovered for identity in identities)
elif mode == 'reused':
    tool = next(i for i in range(6) if sys.monitoring.get_tool(i) is None)
    sys.monitoring.use_tool_id(tool, 'previous debugger')
    disabled = []
    def disable(code, location):
        disabled.append((code, location))
        return sys.monitoring.DISABLE
    sys.monitoring.register_callback(tool, sys.monitoring.events.LINE, disable)
    sys.monitoring.set_local_events(tool, target.__code__, sys.monitoring.events.LINE)
    assert target() == 1
    assert disabled
    sys.monitoring.free_tool_id(tool)
    agent.apply('unused', 1, (probe,))
    assert agent.tool == tool
    reports.clear()
    assert target() == 1
    assert reports == [(1, 42, 0)], reports
elif mode in ('coexist', 'coexist_matching'):
    unrelated_code = target.__code__.replace(co_filename='/unrelated-code.py')
    unrelated = types.FunctionType(unrelated_code, globals())
    sys.monitoring.use_tool_id(0, 'other debugger')
    disabled = []
    def disable(code, location):
        disabled.append((code, location))
        return sys.monitoring.DISABLE
    sys.monitoring.register_callback(0, sys.monitoring.events.LINE, disable)
    sys.monitoring.set_local_events(0, target.__code__, sys.monitoring.events.LINE)
    sys.monitoring.set_local_events(0, unrelated_code, sys.monitoring.events.LINE)
    assert unrelated() == 1
    assert target() == 1 and disabled
    disabled.clear()
    assert target() == 1 and not disabled
    wanted = probe if mode == 'coexist_matching' else ('LINE', 1, 42, '/unmatched.py', 1, '')
    agent.apply('unused', 1, (wanted,))
    assert agent.tool != 0
    reports.clear()
    assert target() == 1
    if mode == 'coexist_matching':
        assert reports == [(1, 42, 0)], reports
        # CPython can reactivate the other tool's disabled locations when
        # this code changes from one LINE tool to several. Registrations
        # remain intact; unrelated code must stay disabled.
        assert all(code is target.__code__ and location == line
                   for code, location in disabled)
        print('SHARED CODE REACTIVATED', bool(disabled))
    else:
        assert not disabled, 'configuration reactivated unrelated code'
    disabled.clear()
    assert unrelated() == 1 and not disabled
    assert sys.monitoring.get_tool(0) == 'other debugger'
    assert sys.monitoring.get_local_events(0, target.__code__) == sys.monitoring.events.LINE
    agent.apply('unused', 2, ())
    assert unrelated() == 1 and not disabled
    sys.monitoring.free_tool_id(0)
else:
    raise AssertionError(mode)
agent.apply('unused', 4, ())
print('DISCOVERY OK', mode)
