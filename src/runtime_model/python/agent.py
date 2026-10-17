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


class CommandError(ValueError):
    """A malformed debugger request, distinct from an interrupted decoder."""


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


def failed(wanted_generation, reason=2):
    global generation
    generation = wanted_generation
    try:
        clear()
    except Exception:
        return (3, generation, 3)  # The previous monitoring state is unknown.
    return (3, generation, reason)


def interference():
    missed = getattr(_local, 'suppressed', None)
    _local.suppressed = None
    if missed is not None:
        number, report_owner, report_hook = missed
        if number == generation and report_owner == _owner and report_hook is hook:
            return failed(number, 5)
    # A newer configuration supersedes unsent interference from the old one.
    return None


def integer(text):
    if len(text) > 20:
        raise CommandError('microscope command integer exceeds 64-bit precision')
    return int(text)


def request(payload):
    text = payload.decode('utf-8')
    # The wire shape needs only three container levels. Scan strings with
    # escapes so hostile input cannot exhaust json's recursive decoder.
    depth, quoted, escaped = 0, False, False
    for byte in payload:
        if quoted:
            if escaped:
                escaped = False
            elif byte == 92:
                escaped = True
            elif byte == 34:
                quoted = False
        elif byte == 34:
            quoted = True
        elif byte in (91, 123):
            depth += 1
            if depth > 8:
                raise CommandError('microscope command nesting exceeds its limit')
        elif byte in (93, 125):
            depth -= 1
    return json.loads(text, parse_int=integer)


def pending():
    # The helper performs one aligned native load. A byte-wise sequence
    # copy could straddle an all-stop publication and manufacture an ID.
    wanted_generation = _sequence()
    if wanted_generation <= generation:
        return None
    _local.context = (hook, _owner, wanted_generation)
    try:
        if (bytes(_control[:8]) != CONTROL_MAGIC
                or int.from_bytes(bytes(_control[20:24]), 'little') != CONTROL_CAPACITY
                or bytes(_control[24:32]) != bytes(8)):
            raise CommandError('invalid microscope command header')
        length = int.from_bytes(bytes(_control[16:20]), 'little')
        if not 0 < length <= CONTROL_CAPACITY:
            raise CommandError('invalid microscope command length')
        decoded = request(bytes(_control[CONTROL_HEADER:CONTROL_HEADER + length]))
        if (type(decoded) is not list or len(decoded) != 2
                or type(decoded[0]) is not int or decoded[0] != wanted_generation):
            raise CommandError('invalid microscope command generation')
        return configure(_owner, wanted_generation, decoded[1])
    except (CommandError, json.JSONDecodeError, UnicodeDecodeError):
        return failed(wanted_generation)
    except BaseException:
        interrupted(wanted_generation)
        raise


def emit(report, report_hook, report_owner, report_generation):
    # Acknowledgements can accept another configuration at this hook.
    # Keep that iterative: repeated edits must not nest Python scripts.
    changed = False
    while report is not None:
        report_hook(*report)
        with lock:
            if (report_owner != _owner or report_hook is not hook
                    or report_generation != generation):
                return True
            report = pending()
            missed = interference()
            if missed is not None:
                report = missed
            if report is not None:
                report_generation = generation
                changed = True
    return changed


def interrupted(wanted_generation):
    # Capture while the configuration lock still proves this owner and
    # generation. Another thread may configure after exception unwinding.
    if getattr(_local, 'interruption', None) is None:
        notice = failed(wanted_generation, 6)
        _local.interruption = (notice, hook, _owner, wanted_generation)


def emit_interruption():
    with lock:
        captured = getattr(_local, 'interruption', None)
        context = getattr(_local, 'context', None)
        if captured is None and context is not None:
            report_hook, report_owner, number = context
            if (report_hook is hook and report_owner == _owner
                    and number == generation):
                interrupted(number)
                captured = _local.interruption
            else:
                # A superseded operation cannot mutate the new owner. Its
                # unsent reports and cleanup are conservatively unconfirmed.
                captured = ((3, number, 3), report_hook, report_owner, number)
        _local.interruption = None
    if captured is not None and captured[1] is not None:
        emit(*captured)


def would_report(event, code, location):
    if dispatch(event, code, location):
        return True
    # Reentrant entry can precede discovery of LINE events. Check the code's
    # actual line table, rather than flagging every function in its file.
    return (event in ('PY_START', 'PY_RESUME', 'PY_THROW')
            and any(matches(probe, code) and probe[0] in LOCAL
                    and (not probe[4] or any(line == probe[4]
                                             for _, _, line in code.co_lines()))
                    for probe in probes))


def callback(event):
    def report(code, location, *unused):
        if id(code) in INTERNAL:
            return None
        if getattr(_local, 'busy', False):
            with lock:
                try:
                    if tool is not None and would_report(event, code, location):
                        _local.suppressed = (generation, _owner, hook)
                except Exception:
                    _local.suppressed = (generation, _owner, hook)
            return None
        _local.busy = True
        _local.suppressed = None
        _local.interruption = None
        _local.context = None
        try:
            with lock:
                _local.context = (hook, _owner, generation)
                notices = []
                changed = pending()
                if changed is not None:
                    notices.append(changed)
                if tool is not None:
                    try:
                        if event in ('PY_START', 'PY_RESUME', 'PY_THROW'):
                            discover(code)
                        notices.extend(dispatch(event, code, location))
                    except Exception:
                        # A discovery failure supersedes an unreported ready
                        # acknowledgement produced by this same callback.
                        notices = [failed(generation)]
                    except BaseException:
                        interrupted(generation)
                        raise
                missed = interference()
                if missed is not None:
                    notices = [missed]
                report_hook, report_owner, report_generation = hook, _owner, generation
            for notice in notices:
                if emit(notice, report_hook, report_owner, report_generation):
                    break  # A consumed configuration supersedes the batch.
        except BaseException:
            emit_interruption()
            raise
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
            or bytes(control[24:32]) != bytes(8) or sequence() != 0):
        raise RuntimeError('incompatible or already-published microscope control library')
    return native, report_hook, control, sequence



def configure(library, wanted_generation, wanted_probes, replacement=None):
    global _library, hook, generation, tool, probes, _owner, _control, _sequence
    if type(wanted_generation) is not int or not 0 <= wanted_generation < 2**64:
        raise CommandError("invalid microscope generation")
    if library == _owner and wanted_generation <= generation:
        return None

    # Validate the complete request before changing the active installation.
    if (type(wanted_probes) not in (list, tuple)
            or any(type(probe) not in (list, tuple) for probe in wanted_probes)):
        raise CommandError("invalid microscope monitoring probe collection")
    wanted_probes = tuple(tuple(probe) for probe in wanted_probes)
    for probe in wanted_probes:
        if (len(probe) != 6 or probe[0] not in LOCAL + OTHER
                or type(probe[1]) is not int or probe[1] not in (1, 2)
                or type(probe[2]) is not int or not 0 < probe[2] < 2**64
                or not isinstance(probe[3], str)
                or type(probe[4]) is not int or not 0 <= probe[4] < 2**32
                or (probe[4] and probe[0] != "LINE")
                or not isinstance(probe[5], str)):
            raise CommandError("invalid microscope monitoring probe")

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
    except Exception:
        return failed(generation)
    except BaseException:
        interrupted(generation)
        raise
    return (0, generation, tool)


def apply(library, wanted_generation, wanted_probes):
    was_busy = getattr(_local, 'busy', False)
    _local.busy = True
    if not was_busy:
        _local.suppressed = None
        _local.interruption = None
        _local.context = None
    try:
        with lock:
            _local.context = (hook, _owner, generation)
            needs_transport = library != _owner
        # The loader may wait for a thread whose monitoring callback needs
        # our lock. Load outside it; configure revalidates ownership inside.
        replacement = transport(library) if needs_transport else None
        with lock:
            next_hook = hook if replacement is None else replacement[1]
            _local.context = (next_hook, library, wanted_generation)
            report = configure(library, wanted_generation, wanted_probes, replacement)
            missed = interference()
            if missed is not None:
                report = missed
            report_hook, report_owner, report_generation = hook, _owner, generation
        if report is not None:
            emit(report, report_hook, report_owner, report_generation)
    except BaseException:
        emit_interruption()
        raise
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
