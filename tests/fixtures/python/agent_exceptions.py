"""The agent's exception reports, read as the controller reads them.

A fake hook stands in for the debugger: at each report of an exception it
reads the record the report points to, the exception's address and its
message, and notes where the program is.
"""
import ctypes
import json
import os
import sys
import threading
import _thread
import types

# As the launch bootstrap records on 3.12, which cannot say which thread
# is the main one.
sys.modules.setdefault('_microscope_bootstrap', types.SimpleNamespace(main=threading.get_ident()))
agent = types.ModuleType('_microscope_agent')
agent._bootstrap_lock = threading.RLock()
exec(compile(open(sys.argv[1]).read(), sys.argv[1], 'exec'), agent.__dict__)
library = sys.argv[3]
agent.apply(library, 0, ())
mode = sys.argv[2]
RAISED, UNCAUGHT = agent.RAISED, agent.UNCAUGHT
SOURCE = open(__file__).read().splitlines()


def line(marker):
    """The line that carries `# marker`."""
    return next(number for number, text in enumerate(SOURCE, 1)
                if text.endswith('# ' + marker))


def program_frame():
    """The program's frame the agent reported from: below the fake hook's
    frames and the agent's own."""
    frame = sys._getframe(1)
    while id(frame.f_code) not in agent.INTERNAL:
        frame = frame.f_back
    while id(frame.f_code) in agent.INTERNAL:
        frame = frame.f_back
    return frame


class Debugger:
    """The controller's side: every report, and each exception's."""

    def __init__(self):
        self.reports = []
        self.exceptions = []
        self.reported = threading.Event()
        agent.hook = self.report

    def report(self, event, number, argument):
        self.reports.append((event, number, argument))
        if event != 4:
            return
        address, text, length = (ctypes.c_uint64 * 3).from_address(argument)
        frame = program_frame()
        self.exceptions.append((number, address, ctypes.string_at(text, length).decode(),
                                frame.f_code.co_qualname, frame.f_lineno))
        self.reported.set()

    def configure(self, exceptions, probes=()):
        agent.apply(library, agent.generation + 1, probes, None, exceptions)
        acknowledged = self.reports[-1]
        assert acknowledged[:2] == (0, agent.generation), self.reports
        return acknowledged[2]

    def failures(self):
        return [report for report in self.reports if report[0] == 3]


def check(condition, *detail):
    if not condition:
        print('MISMATCH', mode, *detail, flush=True)
        sys.exit(1)


def raises(text):
    raise ValueError(text)  # RAISES


def calls_raises():
    raises('inner')  # CALLS_RAISES


class Loud(Exception):
    """An exception whose text runs the program's own code."""
    ran = False

    def __str__(self):
        Loud.ran = True
        return 'loud'


def lookup():
    return {}['missing']  # LOOKUP


debugger = Debugger()

if mode == 'raised':
    # Each raise reports once, where it is raised, with what Python would
    # print: not again as it unwinds, nor as a handler re-raises it.
    debugger.configure(RAISED)
    try:
        calls_raises()
    except ValueError as caught:
        inner = caught
    try:
        try:
            raises('again')
        except ValueError:
            raise
    except ValueError as caught:
        again = caught
    try:
        lookup()
    except KeyError as caught:
        missing = caught
    try:
        raise Loud('quiet')  # LOUD
    except Loud as caught:
        loud = caught
    try:
        raises('x' * 5000)
    except ValueError as caught:
        long = caught
    try:
        int('seven')  # PARSES
    except ValueError as caught:
        parsed = caught
    debugger.configure(0)
    got = debugger.exceptions
    check([report[0] for report in got] == [RAISED] * 6, got)
    check(got[0][1:] == (id(inner), 'ValueError: inner', 'raises', line('RAISES')), got[0])
    check(got[1][1:] == (id(again), 'ValueError: again', 'raises', line('RAISES')), got[1])
    check(got[2][1:] == (id(missing), "KeyError: 'missing'", 'lookup', line('LOOKUP')),
          got[2])
    check(got[3][1:] == (id(loud), 'Loud', '<module>', line('LOUD')) and not Loud.ran,
          got[3], Loud.ran)
    check(got[4][1] == id(long) and got[4][2] == 'ValueError: ' + 'x' * 1000 + '…', got[4][2:])
    check(got[5][1:] == (id(parsed), "ValueError: invalid literal for int() with base 10: "
                         "'seven'", '<module>', line('PARSES')), got[5])
    check(not debugger.failures(), debugger.reports)

elif mode == 'uncaught':
    # Only an exception that leaves its thread's Python code reports: in a
    # threading.Thread, as threading takes it to its excepthook, in a
    # thread nothing in Python called, and in the main module; not one
    # caught, not SystemExit, and not one C may handle on the main thread.
    debugger.configure(UNCAUGHT)
    try:
        calls_raises()
    except ValueError:
        pass
    try:
        # Code named as the main module is not it, when Python runs it.
        exec(compile("raises('nested')", '<nested>', 'exec'),
             {'__name__': '__main__', 'raises': raises})
    except ValueError:
        pass

    def worker():
        raises('worker')

    def exits():
        raise SystemExit(3)

    quiet = threading.excepthook
    threading.excepthook = lambda args: None
    for target in (exits, worker):
        thread = threading.Thread(target=target)
        thread.start()
        thread.join()
    threading.excepthook = quiet
    check(len(debugger.exceptions) == 1, debugger.exceptions)
    bit, _, message, function, _ = debugger.exceptions[0]
    check((bit, function) == (UNCAUGHT, 'Thread.run'), debugger.exceptions)
    check(message == f'ValueError: worker, raised in raises at {__file__}:{line("RAISES")}',
          message)

    def bare():
        raises('bare')

    debugger.reported.clear()
    sys.unraisablehook = lambda unraisable: None
    _thread.start_new_thread(bare, ())
    check(debugger.reported.wait(10), 'no report from a bare thread')
    bit, _, message, function, at = debugger.exceptions[1]
    check((bit, function) == (UNCAUGHT, 'bare'), debugger.exceptions)
    check(message.startswith('ValueError: bare, raised in raises at '), message)

    # Python deletes the main module's __file__ as its code finishes.
    def finished(source=__file__):
        # An atexit callback's exception, which C reports as ignored, is
        # not uncaught; the main module's is, where it leaves the module.
        sys.unraisablehook = sys.__unraisablehook__
        check(len(debugger.exceptions) == 3, debugger.exceptions)
        bit, _, message, function, at = debugger.exceptions[2]
        check((bit, function, at) == (UNCAUGHT, '<module>', line('LEAVES')),
              debugger.exceptions)
        check(message == f'ValueError: leaves, raised in raises at {source}:'
              f'{line("RAISES")}', message)
        debugger.configure(0)
        check(not debugger.failures(), debugger.reports)
        print('EXCEPTIONS AGENT OK', mode, flush=True)
        # The exception it left with would make its status 1.
        os._exit(0)

    import atexit
    atexit.register(finished)
    atexit.register(raises, 'at exit')
    sys.excepthook = lambda *exception: None
    raises('leaves')  # LEAVES

elif mode == 'off':
    # With no exception chosen, an armed agent takes no exception events,
    # and the choice changes with each configuration.
    probe = ('PY_START', 1, 7, __file__, 0, 'calls_raises')
    check(debugger.configure(0, [probe]) != 6, debugger.reports)
    check(not agent.monitoring.get_events(agent.tool) & agent.events.RAISE, 'RAISE armed')
    try:
        calls_raises()
    except ValueError:
        pass
    check(not debugger.exceptions, debugger.exceptions)
    check(debugger.configure(RAISED | UNCAUGHT) != 6, 'no tool for exceptions alone')
    check(agent.monitoring.get_events(agent.tool)
          & (agent.events.RAISE | agent.events.PY_UNWIND), 'exceptions unarmed')
    check(debugger.configure(0) == 6, 'a tool with nothing to report')
    try:
        calls_raises()
    except ValueError:
        pass
    check(not debugger.exceptions, debugger.exceptions)

elif mode == 'internal':
    # What the agent raises and catches itself while it configures, as
    # resolving a path that does not exist, is not the program's: it
    # reports nothing and costs no coverage. The library function that
    # raises it is the program's to break in, not the agent's own.
    import os.path
    check(id(os.path.realpath.__code__) not in agent.INTERNAL, 'realpath is internal')
    check(id(agent.matches.__code__) in agent.INTERNAL, 'the agent is not internal')
    elsewhere = '/microscope/no/such/dir/' + __file__.rpartition('/')[2]
    debugger.configure(RAISED, [('LINE', 1, 9, elsewhere, 1, '')])
    debugger.configure(RAISED, [('LINE', 1, 9, elsewhere, 2, '')])
    check(not debugger.exceptions, debugger.exceptions)
    check(not debugger.failures(), debugger.reports)
    debugger.configure(0)

elif mode == 'malformed':
    # A malformed choice of exceptions is refused whole, keeping nothing of
    # it; through the command slot, it disarms and says so.
    for wanted in (4, -1, '1', 1.0, None, True):
        before = agent.generation
        try:
            agent.configure(agent._owner, before + 1, [], wanted_exceptions=wanted)
        except agent.CommandError:
            pass
        else:
            check(False, 'accepted', wanted)
        check(agent.generation == before, wanted)
    debugger.configure(RAISED)
    wanted = agent.generation + 1
    payload = json.dumps([wanted, [], None, 8]).encode()
    agent._control[32:32 + len(payload)] = payload
    agent._control[16:20] = len(payload).to_bytes(4, 'little')
    agent._control[8:16] = wanted.to_bytes(8, 'little')
    # The next function the program enters takes the command.
    line('LOUD')
    check(debugger.reports[-1] == (3, wanted, 2) and agent.tool is None, debugger.reports)

print('EXCEPTIONS AGENT OK', mode, flush=True)
