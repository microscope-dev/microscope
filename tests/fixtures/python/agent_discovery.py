"""Deterministic monitoring transport contracts, isolated per scenario."""
import gc
import sys
import threading
import types
import weakref

agent = types.ModuleType('discovery_agent')
agent._bootstrap_lock = threading.RLock()
exec(compile(open(sys.argv[1]).read(), sys.argv[1], 'exec'), agent.__dict__)
library = sys.argv[3]
agent.apply(library, 0, ())
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
        agent.apply(library, 1, (probe,))
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
        reports.clear()
        agent.apply(library, 1, (probe,))
        assert target() == 1
        # A free-threaded build instruments matching code as it configures,
        # so the configuration itself reports the fault.
        acknowledged = [] if 't' in sys.abiflags else [(0, 1)]
        assert [r[:2] for r in reports[:-1]] == acknowledged, reports
        assert reports[-1] == (3, 1, 2), reports
        assert agent.tool is None and not agent.discovered
    finally:
        sys.monitoring.set_local_events = original
    agent.apply(library, 2, (probe,))
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
    agent.apply(library, 1, (('LINE', 1, 42, __file__, generator_line, ''),))
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
    agent.apply(library, 2, (('LINE', 1, 44, __file__, thrown_line, ''),))
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
    agent.apply(library, 3, (('LINE', 1, 43, __file__, coroutine_line, ''),))
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
    agent.apply(library, 1, (('LINE', 1, 45, __file__, outer_line, ''),))
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
    agent.apply(library, 2, (('LINE', 1, 46, __file__, outer_line, ''),))
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
    agent.apply(library, 1, (probe,))
    reports.clear()
    assert other() == 1
    assert not reports
    assert target() == 1
    assert reports == [(1, 42, 0)], reports
elif mode == 'gc':
    agent.apply(library, 1, (probe,))
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
    agent.apply(library, 1, (probe,))
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
    agent.apply(library, 1, (wanted,))
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
    agent.apply(library, 2, ())
    assert unrelated() == 1 and not disabled
    sys.monitoring.free_tool_id(0)
elif mode == 'running':
    # Code another thread is running at installation is instrumented. A
    # free-threaded build never reads that thread's frames to find it: the
    # thread runs concurrently, and its frames can change under the reader.
    started, spinning, results = threading.Event(), [True], []
    def worker():
        started.set()
        while spinning[0]:
            pass
        value = 41
        results.append(value + 1)
    worker_line = worker.__code__.co_firstlineno + 4
    thread = threading.Thread(target=worker)
    thread.start()
    assert started.wait(10)
    original = sys._current_frames
    if 't' in sys.abiflags:
        def forbidden():
            raise AssertionError("another thread's frames were read")
        sys._current_frames = forbidden
    try:
        agent.apply(library, 1, (('LINE', 1, 47, __file__, worker_line, ''),))
    finally:
        sys._current_frames = original
    reports.clear()
    spinning[0] = False
    thread.join(10)
    assert not thread.is_alive() and results == [42], results
    assert reports == [(1, 47, 0)], reports
elif mode == 'together':
    # One event's breakpoint hits are one report, so they stop once; a
    # single hit is reported whole.
    import ctypes
    def record(event, number, argument):
        if event == 1 and argument:
            numbers = (ctypes.c_uint64 * number).from_address(argument)
            reports.append((1, tuple(numbers)))
        else:
            reports.append((event, number, argument))
    agent.hook = record
    same_line = ('LINE', 1, 43, __file__, line, '')
    other_line = ('LINE', 1, 44, __file__, line + 99, '')
    entry = ('PY_START', 1, 45, '', 0, 'target')
    other_entry = ('PY_START', 1, 46, '', 0, 'arget')
    agent.apply(library, 1, (probe, same_line, other_line, entry, other_entry))
    reports.clear()
    assert target() == 1
    assert reports == [(1, 45, 0), (1, (42, 43))], reports
elif mode == 'matching':
    import os
    import tempfile
    same = agent.same_file
    here = os.path.abspath(__file__)
    name = os.path.basename(here)
    directory = os.path.basename(os.path.dirname(here))
    # An absolute path names one file; a relative one ends whole paths.
    assert same(here, here)
    assert same(name, here) and same(directory + '/' + name, here)
    assert same(name, name) and same(name, directory + '/' + name)
    assert not same(name[1:], here), 'only the end of a name'
    assert not same('/elsewhere/' + name, here)
    assert not same(here, name), 'a relative filename is not resolved'
    assert not same(here + 'x', here)
    # A link names the same file, from either side.
    with tempfile.TemporaryDirectory() as linked:
        link = os.path.join(linked, name)
        os.symlink(here, link)
        assert same(link, here) and same(here, link)
        renamed = os.path.join(linked, 'renamed.py')
        os.symlink(here, renamed)
        assert not same(renamed, here), 'a link under another name is not resolved'
        assert not same(os.path.join(linked, 'missing', name), here)

    class Shape:
        def area(self):
            pass

    def entry(qualname):
        return ('PY_START', 1, 1, '', 0, qualname)
    code = Shape.area.__code__
    assert code.co_qualname == 'Shape.area', code.co_qualname
    assert agent.matches(entry('Shape.area'), code)
    assert agent.matches(entry('area'), code)
    assert agent.matches(entry(''), code)
    for qualname in ('rea', 'hape.area', 'Shape', 'area.Shape', 'Shape.area.x'):
        assert not agent.matches(entry(qualname), code), qualname
else:
    raise AssertionError(mode)
agent.apply(library, 4, ())
print('DISCOVERY OK', mode)
