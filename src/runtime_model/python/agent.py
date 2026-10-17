"""Monitoring-to-native transport. The debugger supplies whole-state probes.

A probe is (event name, report kind, number, filename, line, qualified name).
Empty selectors match any code; line zero matches any line. Conditions and
expressions are never evaluated here. Event policy belongs to the debugger.
"""
import sys

if sys.version_info < (3, 14):
    raise RuntimeError("microscope's monitoring transport requires CPython 3.14 or later")

import ctypes

monitoring = sys.monitoring
events = monitoring.events
tool = None
generation = -1
probes = ()
hook = None
_library = None
# Configuration and callbacks can run concurrently in free-threaded builds.
# No native hook is called while this lock is held.
lock = _bootstrap_lock

# Only these events have callback signatures this transport understands.
LOCAL = ("LINE", "PY_START", "PY_RESUME", "PY_RETURN", "PY_YIELD")
OTHER = ("RAISE", "PY_UNWIND", "PY_THROW")


def matches(probe, code):
    return (id(code) not in INTERNAL
            and (not probe[3] or probe[3] == code.co_filename)
            and (not probe[5] or probe[5] == code.co_qualname))


def clear():
    global tool, probes
    if tool is not None:
        monitoring.free_tool_id(tool)
        tool = None
    probes = ()


def dispatch(event, code, location):
    return tuple((probe[1], probe[2], 0) for probe in probes
                 if probe[0] == event and matches(probe, code)
                 and (not probe[4] or (event == "LINE" and probe[4] == location)))


def discover(code):
    mask = 0
    for probe in probes:
        if probe[0] in LOCAL and matches(probe, code):
            mask |= getattr(events, probe[0])
    monitoring.set_local_events(tool, code, mask)


def callback(event):
    def report(code, location, *unused):
        if id(code) in INTERNAL:
            return monitoring.DISABLE if event in LOCAL else None
        with lock:
            if tool is None:
                return
            try:
                if event == "PY_START":
                    discover(code)
                reports = dispatch(event, code, location)
                disable = not reports and event in LOCAL
            except BaseException:
                # A monitoring exception must never escape into user code.
                clear()
                reports = ((3, generation, 2),)
                disable = False
        for arguments in reports:
            hook(*arguments)
        if disable:
            return monitoring.DISABLE
    return report


def configure(library, wanted_generation, wanted_probes):
    global _library, hook, generation, tool, probes
    if type(wanted_generation) is not int or not 0 <= wanted_generation < 2**64:
        raise ValueError("invalid microscope generation")
    if wanted_generation <= generation:
        return None

    # Validate the complete request before changing the active installation.
    wanted_probes = tuple(tuple(probe) for probe in wanted_probes)
    for probe in wanted_probes:
        if (len(probe) != 6 or probe[0] not in LOCAL + OTHER
                or type(probe[1]) is not int or probe[1] not in (1, 2)
                or type(probe[2]) is not int or not 0 < probe[2] < 2**64
                or not isinstance(probe[3], str)
                or type(probe[4]) is not int or not 0 <= probe[4] < 2**32
                or (probe[4] and probe[0] != "LINE")
                or not isinstance(probe[5], str)):
            raise ValueError("invalid microscope monitoring probe")

    if hook is None:
        # PyDLL preserves the GIL in conventional builds. Arguments belong to
        # the calling thread in both GIL modes; there is no shared mailbox.
        _library = ctypes.PyDLL(library)
        hook = _library.microscope_python_hook
        hook.argtypes = (ctypes.c_uint64, ctypes.c_uint64, ctypes.c_uint64)
        hook.restype = None

    clear()
    generation = wanted_generation
    if not wanted_probes:
        return (0, generation, 6)  # 6 means no tool is registered.

    for candidate in range(6):
        try:
            monitoring.use_tool_id(candidate, "microscope")
        except ValueError:
            continue
        tool = candidate
        break
    if tool is None:
        return (3, generation, 1)

    try:
        probes = wanted_probes
        for event in LOCAL + OTHER:
            monitoring.register_callback(tool, getattr(events, event), callback(event))
        mask = events.PY_START
        for probe in probes:
            if probe[0] in OTHER:
                mask |= getattr(events, probe[0])
        monitoring.set_events(tool, mask)
        # DISABLE survives tool-ID reuse. CPython offers only a global restart;
        # other tools keep their events/callbacks but can get another callback
        # at a location they disabled.
        monitoring.restart_events()
        # Enable discovery before scanning frames: another thread can enter
        # code while this one configures a free-threaded interpreter.
        for frame in sys._current_frames().values():
            while frame is not None:
                discover(frame.f_code)
                frame = frame.f_back
    except BaseException:
        clear()
        return (3, generation, 2)
    return (0, generation, tool)


def apply(library, wanted_generation, wanted_probes):
    with lock:
        report = configure(library, wanted_generation, wanted_probes)
    if report is not None:
        hook(*report)


# Suppress only our own code objects, never functions guessed from their names.
_pending = [value.__code__ for value in tuple(globals().values())
            if type(value) is type(matches)]
_internal = set()
while _pending:
    _code = _pending.pop()
    _internal.add(id(_code))
    _pending.extend(value for value in _code.co_consts if type(value) is type(_code))
INTERNAL = frozenset(_internal)
del _pending, _internal, _code

# Published only after initialization succeeds; the bootstrap serializes it.
_microscope_loaded = 1
