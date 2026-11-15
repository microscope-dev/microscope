







import _thread
import asyncio
import json
import sys
import threading
import types



sys.modules.setdefault('_microscope_bootstrap', types.SimpleNamespace(main=threading.get_ident()))
agent = types.ModuleType('_microscope_agent')
agent._bootstrap_lock = threading.RLock()
exec(compile(open(sys.argv[1]).read(), sys.argv[1], 'exec'), agent.__dict__)
library = sys.argv[3]
agent.apply(library, 0, ())
mode = sys.argv[2]
INTO, OVER, OUT = agent.INTO, agent.OVER, agent.OUT
SOURCE = open(__file__).read().splitlines()


def line(marker):

    return next(number for number, text in enumerate(SOURCE, 1)
                if text.endswith('# ' + marker))


def program_frame():


    frame = sys._getframe(1)
    while id(frame.f_code) not in agent.INTERNAL:
        frame = frame.f_back
    while id(frame.f_code) in agent.INTERNAL:
        frame = frame.f_back
    return frame


def depth(frame):
    callers = 0
    while frame.f_back is not None:
        frame = frame.f_back
        callers += 1
    return callers


class Debugger:



    def __init__(self, probes, plan, thread=None):
        self.probes = probes
        self.plan = list(plan)
        self.stops = []
        self.generation = agent.generation
        self.number = 0
        self.thread = thread
        agent.hook = self.report

    def queue(self, step, assignment=None):
        self.generation += 1
        command = [self.generation, self.probes, step]
        if assignment is not None:
            command += [0, assignment]
        payload = json.dumps(command).encode()
        agent._control[32:32 + len(payload)] = payload
        agent._control[16:20] = len(payload).to_bytes(4, 'little')
        agent._control[8:16] = self.generation.to_bytes(8, 'little')

    def report(self, event, number, argument):
        if event not in (1, 2):
            return
        frame = program_frame()
        self.stops.append(((event, number, argument), frame.f_code.co_qualname,
                           frame.f_lineno))
        if not self.plan:
            self.queue(None)
            return
        kind, up = self.plan.pop(0)
        for _ in range(up):
            frame = frame.f_back
        self.number += 1
        thread = threading.get_native_id() if self.thread is None else self.thread
        self.queue([self.number, kind, thread, depth(frame), id(frame.f_code)])

    def start(self):
        agent.apply(library, self.generation + 1, self.probes)
        self.generation = agent.generation

    def places(self):
        return [(name, at) for _, name, at in self.stops]

    def kinds(self):
        return [report for report, _, _ in self.stops]


def breakpoint_at(marker):
    return ('LINE', 1, 900, __file__, line(marker), '')


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
    return value  # OUTER_RETURN


def numbers():
    yield 1  # GEN_FIRST
    yield 2  # GEN_SECOND


def consume():
    produced = numbers()
    first = next(produced)  # CONSUME_FIRST
    second = next(produced)  # CONSUME_SECOND
    return first + second


async def leaf():
    await asyncio.sleep(0)  # LEAF_SLEEP
    return 1  # LEAF_RETURN


async def task():
    begun = 1  # TASK_BEGIN
    await leaf()  # TASK_AWAIT
    ended = begun + 1  # TASK_END
    return ended


def raises():
    raise ValueError('step')  # RAISES


def handles():
    try:
        raises()  # HANDLES_CALL
    except ValueError:  # HANDLES_EXCEPT
        caught = True  # HANDLES_CATCH
    return caught


def recursive(count):
    if count == 0:  # RECURSE_TEST
        return 0
    inner = recursive(count - 1)  # RECURSE_CALL
    return inner + 1  # RECURSE_RETURN


def straight():
    a = 1  # STRAIGHT_FIRST
    b = 2  # STRAIGHT_SECOND
    return a + b  # STRAIGHT_THIRD


class Key:


    def __hash__(self):
        return 1  # KEY_HASH

    def __eq__(self, other):
        return self is other  # KEY_EQ


def table():
    built = {Key(): 1, Key(): 2}  # TABLE_BUILD
    return len(built)  # TABLE_LEN


here = __file__

if mode == 'over':


    debugger = Debugger([breakpoint_at('CALLER_START')], [(OVER, 0)] * 4)
    debugger.start()
    assert outer() == 4
    assert debugger.places() == [
        ('caller', line('CALLER_START')),
        ('caller', line('CALLER_CALL')),
        ('caller', line('CALLER_AFTER')),
        ('caller', line('CALLER_RETURN')),
        ('outer', line('OUTER_CALL')),
    ], debugger.places()
    assert debugger.kinds()[1:] == [(2, number, 0) for number in range(1, 5)]
elif mode == 'into':


    debugger = Debugger([breakpoint_at('CALLER_START')], [(INTO, 0)] * 5)
    debugger.start()
    assert outer() == 4
    assert debugger.places() == [
        ('caller', line('CALLER_START')),
        ('caller', line('CALLER_CALL')),
        ('callee', line('CALLEE_FIRST')),
        ('callee', line('CALLEE_LAST')),
        ('caller', line('CALLER_CALL')),
        ('caller', line('CALLER_AFTER')),
    ], debugger.places()
elif mode == 'out':


    debugger = Debugger([breakpoint_at('CALLEE_FIRST')], [(OUT, 0), (OUT, 0)])
    debugger.start()
    assert outer() == 4
    assert debugger.places() == [
        ('callee', line('CALLEE_FIRST')),
        ('caller', line('CALLER_CALL')),
        ('outer', line('OUTER_CALL')),
    ], debugger.places()
    debugger = Debugger([breakpoint_at('CALLEE_FIRST')], [(OUT, 1)])
    debugger.start()
    assert outer() == 4
    assert debugger.places() == [
        ('callee', line('CALLEE_FIRST')),
        ('outer', line('OUTER_CALL')),
    ], debugger.places()
elif mode == 'recursion':


    debugger = Debugger([breakpoint_at('RECURSE_CALL')], [(OVER, 0)])
    counts = []
    original = debugger.report

    def noting(event, number, argument):
        if event in (1, 2):
            counts.append(program_frame().f_locals['count'])
        original(event, number, argument)
    agent.hook = noting
    debugger.start()
    debugger.probes = []  # The step is armed without the breakpoint.
    assert recursive(2) == 2
    assert debugger.places() == [
        ('recursive', line('RECURSE_CALL')),
        ('recursive', line('RECURSE_RETURN')),
    ], debugger.places()
    assert counts == [2, 2], counts
elif mode == 'generator':

    debugger = Debugger([breakpoint_at('GEN_FIRST')], [(OVER, 0)])
    debugger.start()
    assert consume() == 3
    assert debugger.places() == [
        ('numbers', line('GEN_FIRST')),
        ('consume', line('CONSUME_FIRST')),
    ], debugger.places()
elif mode == 'coroutine':


    debugger = Debugger([breakpoint_at('TASK_AWAIT')], [(OVER, 0)])
    debugger.start()
    assert asyncio.run(task()) == 2
    assert debugger.places() == [
        ('task', line('TASK_AWAIT')),
        ('task', line('TASK_END')),
    ], debugger.places()
    debugger = Debugger([breakpoint_at('TASK_AWAIT')], [(INTO, 0), (OVER, 0), (OVER, 0)])
    debugger.start()
    assert asyncio.run(task()) == 2
    assert debugger.places()[:3] == [
        ('task', line('TASK_AWAIT')),
        ('leaf', line('LEAF_SLEEP')),
        ('leaf', line('LEAF_RETURN')),
    ], debugger.places()

    debugger = Debugger([breakpoint_at('LEAF_SLEEP')], [(INTO, 0)])
    debugger.start()
    assert asyncio.run(task()) == 2
    stop = debugger.places()[1]
    assert stop[0] in ('sleep', 'leaf'), debugger.places()
elif mode == 'exception':


    debugger = Debugger([breakpoint_at('HANDLES_CALL')], [(OVER, 0), (OVER, 0)])
    debugger.start()
    assert handles() is True
    assert debugger.places() == [
        ('handles', line('HANDLES_CALL')),
        ('handles', line('HANDLES_EXCEPT')),
        ('handles', line('HANDLES_CATCH')),
    ], debugger.places()
    debugger = Debugger([breakpoint_at('RAISES')], [(OUT, 0)])
    debugger.start()
    assert handles() is True


    assert debugger.places() == [
        ('raises', line('RAISES')),
        ('handles', line('HANDLES_EXCEPT')),
    ], debugger.places()
elif mode == 'outermost':



    results = []

    def body():
        results.append(straight())  # BODY_CALL
    debugger = Debugger([breakpoint_at('STRAIGHT_THIRD')], [(OUT, 0), (OVER, 0)])
    debugger.start()
    worker = threading.Thread(target=body)
    worker.start()
    worker.join(10)
    assert not worker.is_alive() and results == [3]
    assert debugger.places()[:2] == [
        ('straight', line('STRAIGHT_THIRD')),
        ('body', line('BODY_CALL')),
    ], debugger.places()
elif mode == 'mismatch':


    debugger = Debugger([breakpoint_at('STRAIGHT_FIRST')], [])
    reports = []

    def wrong(event, number, argument):
        if event == 0:
            return
        reports.append((event, number, argument))
        if event == 1:
            frame = program_frame()
            debugger.number += 1
            debugger.queue([debugger.number, OVER, threading.get_native_id(), depth(frame),
                            id(caller.__code__)])
    agent.hook = wrong
    debugger.start()
    debugger.probes = []
    assert straight() == 3
    assert reports == [(1, 900, 0), (2, 1, 1)], reports

    reports.clear()

    def too_deep(event, number, argument):
        if event == 0:
            return
        reports.append((event, number, argument))
        if event == 1:
            frame = program_frame()
            debugger.number += 1
            debugger.queue([debugger.number, OVER, threading.get_native_id(), 10_000,
                            id(frame.f_code)])
    agent.hook = too_deep
    debugger.probes = [breakpoint_at('STRAIGHT_FIRST')]
    debugger.start()
    debugger.probes = []
    assert straight() == 3
    assert reports == [(1, 900, 0), (2, 2, 1)], reports



    def assigned(code, at):
        reports.clear()

        def assign(event, number, argument):
            if event == 0:
                return
            reports.append((event, number, argument))
            if event == 1:
                frame = program_frame()
                where = depth(frame) if at is None else at
                debugger.queue(None, [agent.assigned + 1, threading.get_native_id(), where,
                                      id(frame.f_code if code is None else code),
                                      'a', ['int', '40']])
        agent.hook = assign
        debugger.probes = [breakpoint_at('STRAIGHT_SECOND')]
        debugger.start()
        debugger.probes = []
        return straight()

    given, status = (42, 0) if sys.version_info >= (3, 13) else (3, 3)
    assert assigned(None, None) == given
    assert reports == [(1, 900, 0), (6, agent.assigned, status)], reports
    assert assigned(caller.__code__, None) == 3
    assert reports == [(1, 900, 0), (6, agent.assigned, 1)], reports
    assert assigned(None, 10_000) == 3
    assert reports == [(1, 900, 0), (6, agent.assigned, 1)], reports
elif mode == 'late':



    reports = []
    agent.hook = lambda *report: reports.append(report) if report[0] != 0 else None
    probes = [breakpoint_at('STRAIGHT_SECOND')]
    agent.apply(library, agent.generation + 1, probes)
    target_thread = threading.get_native_id()
    frame_depth = depth(sys._getframe())




    done = _thread.allocate_lock()
    done.acquire()

    def configure():
        try:
            agent.apply(library, agent.generation + 1, probes,
                        [7, OVER, target_thread, frame_depth + 1, id(straight.__code__)])
        finally:
            done.release()
    waiter = {}
    exec(compile('def wait(start, configure, done):\n'
                 '    start(configure, ())\n'
                 '    return done.acquire(timeout=10)\n', '<late wait>', 'exec'), waiter)
    assert waiter['wait'](_thread.start_new_thread, configure, done), \
        'the configuration never ended'
    assert straight() == 3
    assert (1, 900, 0) in reports and (2, 7, 2) in reports, reports
    assert not any(report[0] == 2 and report[2] == 0 for report in reports), reports
elif mode == 'threads':


    gate, release = threading.Event(), threading.Event()

    def spin():
        gate.set()
        while not release.is_set():
            straight()
    other = threading.Thread(target=spin)
    other.start()
    assert gate.wait(10)
    try:
        for kind in (OVER, INTO):
            debugger = Debugger([breakpoint_at('CALLER_START')], [(kind, 0)])

            debugger.start()
            assert caller() == 4
            assert debugger.places() == [
                ('caller', line('CALLER_START')),
                ('caller', line('CALLER_CALL')),
            ], (kind, debugger.places())
    finally:
        release.set()
        other.join(10)
elif mode == 'reconfigured':




    compared = ('LINE', 1, 901, here, line('KEY_EQ'), '')
    debugger = Debugger([breakpoint_at('KEY_HASH')], [(OUT, 0)])
    phases = []
    original = debugger.report

    def republish(event, number, argument):
        if (event, number) == (1, 901):
            debugger.stops.append(((event, number, argument), 'republished', 0))
            phases.append(agent.stepping[0])
            debugger.generation += 1
            payload = json.dumps([debugger.generation, [compared], list(agent.step)])
            payload = payload.encode()
            agent._control[32:32 + len(payload)] = payload
            agent._control[16:20] = len(payload).to_bytes(4, 'little')
            agent._control[8:16] = debugger.generation.to_bytes(8, 'little')
            return
        original(event, number, argument)
    agent.hook = republish
    debugger.start()
    debugger.probes = [compared]
    assert table() == 2
    assert phases == [agent.CALLER], phases
    assert debugger.places() == [
        ('Key.__hash__', line('KEY_HASH')),
        ('republished', 0),
        ('table', line('TABLE_BUILD')),
    ], debugger.places()
    assert debugger.kinds()[2] == (2, 1, 0), debugger.kinds()
elif mode == 'malformed':

    good = [1, OVER, 1, 0, 1]
    for bad in ([0, OVER, 1, 0, 1], [1, 4, 1, 0, 1], [1, OVER, 0, 0, 1],
                [1, OVER, 1, -1, 1], [1, OVER, 1, 0, 0], [1, OVER, 1, 0],
                [1, OVER, 1, 0, 1, 2], [1, OVER, 1, 0, True], {'step': 1},
                [1, OVER, 1, 0, 2**64], [1.0, OVER, 1, 0, 1]):
        before = agent.generation
        try:
            agent.configure(agent._owner, before + 1, [], wanted_step=bad)
        except agent.CommandError:
            pass
        else:
            raise AssertionError(bad)
        assert agent.generation == before, bad
    agent.configure(agent._owner, agent.generation + 1, [], wanted_step=good)
    assert agent.step == tuple(good) and agent.tool is not None
else:
    raise AssertionError(mode)
agent.hook = lambda *report: None
agent.apply(library, agent.generation + 1, ())
assert agent.tool is None and agent.step is None
print('STEPPING AGENT OK', mode)
