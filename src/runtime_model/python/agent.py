"""Monitoring-to-native transport. The debugger supplies whole-state probes.

A probe is (event name, report kind, number, filename, line, qualified name).
Empty selectors match any code; line zero matches any line. Conditions and
expressions are never evaluated here. Event policy belongs to the debugger.
"""
import sys

if sys.version_info < (3, 14):
    raise RuntimeError("microscope's monitoring transport requires CPython 3.14 or later")

import ctypes
import json
import _thread
import _weakref

monitoring = sys.monitoring
events = monitoring.events
tool = None
generation = -1
probes = ()
hook = None
_library = None
_owner = None
_control = None
_sequence = None
_local = _thread._local()
CONTROL_HEADER = 32
CONTROL_CAPACITY = 65536
CONTROL_MAGIC = b'MICPYC02'
discovered = {}
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
    discovered.clear()


def dispatch(event, code, location):
    return tuple((probe[1], probe[2], 0) for probe in probes
                 if probe[0] == event and matches(probe, code)
                 and (not probe[4] or (event == "LINE" and probe[4] == location)))


def discover(code):
    identity = id(code)
    previous = discovered.get(identity)
    if previous is not None and previous() is code:
        return
    mask = 0
    for probe in probes:
        if probe[0] in LOCAL and matches(probe, code):
            mask |= getattr(events, probe[0])
    if mask:
        monitoring.set_local_events(tool, code, mask)

    # Code equality can ignore its filename. Cache by identity and retain
    # only a weak reference, so dynamic code neither aliases nor leaks.
    def expired(reference):
        with lock:
            if discovered.get(identity) is reference:
                discovered.pop(identity, None)
    discovered[identity] = _weakref.ref(code, expired)


def failed(wanted_generation):
    global generation
    generation = wanted_generation
    try:
        clear()
    except BaseException:
        return (3, generation, 3)  # The previous monitoring state is unknown.
    return (3, generation, 2)


def pending():
    # The helper performs one aligned native load. A byte-wise sequence
    # copy could straddle an all-stop publication and manufacture an ID.
    wanted_generation = _sequence()
    if wanted_generation <= generation:
        return None
    try:
        if (bytes(_control[:8]) != CONTROL_MAGIC
                or int.from_bytes(bytes(_control[20:24]), 'little') != CONTROL_CAPACITY):
            raise ValueError('invalid microscope command header')
        length = int.from_bytes(bytes(_control[16:20]), 'little')
        if not 0 < length <= CONTROL_CAPACITY:
            raise ValueError('invalid microscope command length')
        request = json.loads(bytes(_control[CONTROL_HEADER:CONTROL_HEADER + length]))
        if (type(request) is not list or len(request) != 2
                or type(request[0]) is not int or request[0] != wanted_generation):
            raise ValueError('invalid microscope command generation')
        return configure(_owner, wanted_generation, request[1])
    except BaseException:
        return failed(wanted_generation)


def emit(report, report_hook, report_owner):
    # Acknowledgements can accept another configuration at this hook.
    # Keep that iterative: repeated edits must not nest Python scripts.
    while report is not None:
        report_hook(*report)
        with lock:
            if report_owner != _owner or report_hook is not hook:
                return
            report = pending()


def callback(event):
    def report(code, location, *unused):
        if id(code) in INTERNAL or getattr(_local, 'busy', False):
            return None
        _local.busy = True
        try:
            with lock:
                notices = []
                changed = pending()
                if changed is not None:
                    notices.append(changed)
                if tool is not None:
                    try:
                        if event in ('PY_START', 'PY_RESUME', 'PY_THROW'):
                            discover(code)
                        notices.extend(dispatch(event, code, location))
                    except BaseException:
                        notices.append(failed(generation))
                report_hook, report_owner = hook, _owner
            for notice in notices:
                emit(notice, report_hook, report_owner)
        finally:
            _local.busy = False
    return report


def transport(library):
    native = ctypes.PyDLL(library)
    report_hook = native.microscope_python_hook
    report_hook.argtypes = (ctypes.c_uint64, ctypes.c_uint64, ctypes.c_uint64)
    report_hook.restype = None
    control = (ctypes.c_ubyte * (CONTROL_HEADER + CONTROL_CAPACITY)).in_dll(
        native, 'microscope_python_control')
    sequence = native.microscope_python_sequence
    sequence.argtypes = ()
    sequence.restype = ctypes.c_uint64
    if (bytes(control[:8]) != CONTROL_MAGIC
            or int.from_bytes(bytes(control[20:24]), 'little') != CONTROL_CAPACITY
            or sequence() != 0):
        raise RuntimeError('incompatible or already-published microscope control library')
    return native, report_hook, control, sequence



def configure(library, wanted_generation, wanted_probes, replacement=None):
    global _library, hook, generation, tool, probes, _owner, _control, _sequence
    if type(wanted_generation) is not int or not 0 <= wanted_generation < 2**64:
        raise ValueError("invalid microscope generation")
    if library == _owner and wanted_generation <= generation:
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

    if library != _owner and replacement is None:
        raise RuntimeError("microscope transport owner changed during preparation")
    clear()
    if library != _owner:
        _library, hook, _control, _sequence = replacement
        _owner = library
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
        mask = events.PY_START | events.PY_RESUME | events.PY_THROW
        for probe in probes:
            if probe[0] in OTHER:
                mask |= getattr(events, probe[0])
        monitoring.set_events(tool, mask)
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
    was_busy = getattr(_local, 'busy', False)
    _local.busy = True
    try:
        with lock:
            needs_transport = library != _owner
        # The loader may wait for a thread whose monitoring callback needs
        # our lock. Load outside it; configure revalidates ownership inside.
        replacement = transport(library) if needs_transport else None
        with lock:
            report = configure(library, wanted_generation, wanted_probes, replacement)
            report_hook, report_owner = hook, _owner
        if report is not None:
            emit(report, report_hook, report_owner)
    finally:
        _local.busy = was_busy


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
_microscope_loaded = 2
