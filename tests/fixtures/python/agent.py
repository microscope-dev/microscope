"""Agent contract tests, run under every supported injection interpreter."""
import sys
import threading

source, library = sys.argv[1:3]
native = "--native" in sys.argv[3:]
source = open(source).read()


def load(generation, probes):
    # Each PEP 768 script runs with fresh globals; only sys.modules persists.
    namespace = dict(__microscope_library=library,
                     __microscope_generation=generation,
                     __microscope_probes=probes)
    exec(compile(source, "<microscope-bootstrap>", "exec"), namespace)
    return sys.modules["_microscope_agent"]


if sys.version_info < (3, 14):
    try:
        load(1, ())
    except RuntimeError as error:
        assert "requires CPython 3.14" in str(error)
    else:
        raise AssertionError("an unsupported interpreter loaded the agent")
    print("AGENT OK (unsupported version refused)")
    sys.exit(0)

if "--failed-bootstrap" in sys.argv:
    incompatible = type(sys)("_microscope_agent")
    incompatible._bootstrap_lock = threading.RLock()
    incompatible._microscope_loaded = 999
    sys.modules["_microscope_agent"] = incompatible
    try:
        load(1, ())
    except RuntimeError as error:
        assert "incompatible microscope agent transport" in str(error)
    else:
        raise AssertionError("incompatible agent replaced silently")
    assert incompatible._microscope_loaded == 999
    print("FAILED BOOTSTRAP OK")
    sys.exit(0)


def bootstrap_race():
    # Pause the first source execution before any module body runs. A second
    # fresh script must find the same published, guarded module, not create
    # another independent monitoring installation.
    entered = threading.Event()
    second = threading.Event()
    release = threading.Event()
    counts = [0, 0]
    count_lock = threading.Lock()
    active = True
    errors = []
    modules = []

    def audit(event, args):
        if not active or event != "exec":
            return
        code = args[0]
        if code.co_filename == "<microscope-bootstrap>":
            with count_lock:
                counts[0] += 1
                if counts[0] == 2:
                    second.set()
        elif "configure" in code.co_names and "monitoring" in code.co_names:
            with count_lock:
                counts[1] += 1
                first = counts[1] == 1
            if first:
                entered.set()
                assert release.wait(10), "release agent initialization"

    def run():
        try:
            modules.append(load(1, ()))
        except BaseException as error:
            errors.append(error)

    sys.addaudithook(audit)
    first = threading.Thread(target=run)
    other = threading.Thread(target=run)
    first.start()
    try:
        assert entered.wait(10), "agent source begins initialization"
        assert "_microscope_agent" in sys.modules, "concurrent bootstrap can create a second agent"
        other.start()
        assert second.wait(10), "second script enters its fresh namespace"
    finally:
        release.set()
        first.join(10)
        if other.ident is not None:
            other.join(10)
        active = False
    assert not first.is_alive() and not other.is_alive()
    assert not errors, errors
    assert counts == [2, 1], counts
    assert len(modules) == 2 and modules[0] is modules[1]


if not native:
    bootstrap_race()

gil = sys._is_gil_enabled()
agent = load(1, ())
assert sys._is_gil_enabled() == gil, "loading the agent changed the GIL mode"
assert agent.tool is None
initial = [sys.monitoring.get_tool(i) for i in range(6)]
# The actual native hook must be callable without a debugger.
agent.hook(0, 0, 0)
records = []
native_hook = agent.hook
def record(*args):
    assert not agent.lock._is_owned(), "native hook called with the agent lock held"
    assert agent.lock is agent._bootstrap_lock, "initialization and configuration share one lock"
    records.append(args)


if not native:
    agent.hook = record


def target():
    value = 1  # TARGET
    return value


if native:
    load(2, (("PY_START", 1, 42, __file__, 0, "target"),))
    target()
    agent.hook(99, 42, 0)  # Corrupt reports must surface as RuntimeEvent.
    load(3, ())
    print("NATIVE AGENT OK")
    sys.exit(0)

line = target.__code__.co_firstlineno + 1
probe = ("LINE", 1, 42, __file__, line, "")
load(2, (probe,))
assert records.pop() == (0, 2, agent.tool)
target()
assert records == [(1, 42, 0)], records
records.clear()
# Identical and stale scripts cannot overwrite the desired state.
load(2, ())
load(1, ())
target()
assert records == [(1, 42, 0)], records
records.clear()
# A new generation replaces the local event mask on already seen code.
probe2 = ("PY_START", 1, 43, __file__, 0, "target")
load(3, (probe2,))
records.clear()
target()
assert records == [(1, 43, 0)], records
load(4, ())
assert agent.tool is None
assert [sys.monitoring.get_tool(i) for i in range(6)] == initial

# A competing debugger retains its callback and events.
sys.monitoring.use_tool_id(0, "other debugger")
other = []
callback = lambda *args: other.append(args)
sys.monitoring.register_callback(0, sys.monitoring.events.PY_START, callback)
sys.monitoring.set_events(0, sys.monitoring.events.PY_START)
load(5, (probe,))
assert agent.tool != 0
records.clear()
target()
assert records == [(1, 42, 0)], records
load(6, ())
assert sys.monitoring.get_tool(0) == "other debugger"
assert sys.monitoring.get_events(0) == sys.monitoring.events.PY_START
assert sys.monitoring.register_callback(0, sys.monitoring.events.PY_START, callback) is callback
assert other
sys.monitoring.free_tool_id(0)

# All IDs occupied: an explicit failed handshake, no stolen ID.
for i in range(6):
    sys.monitoring.use_tool_id(i, str(i))
records.clear()
load(7, (probe,))
assert records == [(3, 7, 1)], records
assert agent.tool is None
for i in range(6):
    assert sys.monitoring.get_tool(i) == str(i)
    sys.monitoring.free_tool_id(i)
load(8, (probe,))
assert agent.tool is not None

# A callback failure disarms monitoring and reports it, never changing the
# target's return value or letting an exception escape into user code.
records.clear()
real_dispatch = agent.dispatch
agent.dispatch = lambda *args: 1 / 0
assert target() == 1
assert records == [(3, 8, 2)], records
assert agent.tool is None
agent.dispatch = real_dispatch
load(9, ())

# Validate before mutation: a malformed request keeps the old generation.
load(10, (probe,))
for bad in (("LINE", 1, 0, "", 0, ""), ("RAISE", 1, 1, "", 1, ""),
            ("BOGUS", 1, 1, "", 0, "")):
    try:
        load(11, (bad,))
    except ValueError:
        pass
    else:
        raise AssertionError("invalid probe accepted")
    assert agent.generation == 10
records.clear()
target()
assert records == [(1, 42, 0)], records
load(11, ())

# All callback signatures, including non-disableable events and generators
# resumed, thrown into, and unwound. Other code must keep running normally.
def event_target(fail=False):
    yield 1
    if fail:
        raise ValueError("fixture")
    yield 2


names = agent.LOCAL + agent.OTHER
load(12, tuple((name, 1, i + 100, __file__, 0, "event_target")
               for i, name in enumerate(names)))
records.clear()
gen = event_target()
assert list(gen) == [1, 2]
gen = event_target(True)
assert next(gen) == 1
try:
    next(gen)
except ValueError:
    pass
gen = event_target()
assert next(gen) == 1
try:
    gen.throw(LookupError())
except LookupError:
    pass
assert {number for kind, number, arg in records} == set(range(100, 108)), records
load(13, ())

# Force an old callback to stay in its hook while another thread applies
# a new generation. The hook must hold no lock required by configuration.
entered = threading.Event()
release = threading.Event()


def held_hook(*args):
    if args == (1, 42, 0):
        entered.set()
        assert release.wait(10), "release the old report"
    record(*args)


agent.hook = held_hook
load(14, (probe,))
worker_errors = []


def run_worker():
    try:
        target()
    except BaseException as error:
        worker_errors.append(error)


worker = threading.Thread(target=run_worker)
worker.start()
assert entered.wait(10), "worker reaches its hook"
load(15, (probe2,))
release.set()
worker.join(10)
assert not worker.is_alive()
assert not worker_errors, worker_errors
records.clear()
target()
assert records == [(1, 43, 0)], records
agent.hook = record
load(16, ())

# Force a weakref callback under the real bootstrap's reentrant lock.
# The only strong references form a cycle, so collection triggers expiry.
import gc
import weakref
load(17, (probe,))
was_enabled = gc.isenabled()
gc.disable()
try:
    namespace = {}
    exec(compile("def cyclic(): return cyclic\n", "<cycle>", "exec"), namespace)
    cyclic = namespace["cyclic"]
    assert cyclic() is cyclic
    identity = id(cyclic.__code__)
    reference = weakref.ref(cyclic.__code__)
    assert identity in agent.discovered
    del cyclic, namespace
    with agent.lock:
        assert agent.lock is agent._bootstrap_lock
        gc.collect()
        assert reference() is None
        assert identity not in agent.discovered
finally:
    if was_enabled:
        gc.enable()
load(18, ())

# A generated whole-state script needs no prepopulated namespace, including
# on repeated execution. Its string literals are escaped by the Rust builder.
standalone = open(sys.argv[3]).read()
exec(compile(standalone, "<whole-state>", "exec"), {})
exec(compile(standalone, "<whole-state>", "exec"), {})
assert agent.generation == 100 and agent.tool is None
assert [sys.monitoring.get_tool(i) for i in range(6)] == initial
agent.hook = native_hook
print("AGENT OK")
