"""Monitoring-to-native transport. The debugger supplies whole-state probes.

A probe is (event name, report kind, number, filename, line, qualified name).
Empty selectors match any code; line zero matches any line. Conditions and
expressions are never evaluated here. Event policy belongs to the debugger.

A step is (number, kind, thread, depth, code): it steps into (0), over (1),
or out of (2) the frame with `depth` callers on the thread whose native ID is
`thread`, which runs the code whose id is `code`. It binds to that frame on
its thread, at the stop it begins from, and follows the frame object.

Exceptions are bits: RAISED (1) reports each exception raised in Python code
or reaching it; UNCAUGHT (2) each that leaves a thread's outermost Python
frame. A report carries the exception and a message formed without running
the program's code.
"""
import sys

if sys.version_info < (3, 14):
    raise RuntimeError("microscope's monitoring transport requires CPython 3.14 or later")

import ctypes
import gc
import json
import _thread
import _weakref
from os import register_at_fork as _register_at_fork
from os.path import realpath as _realpath
from types import FunctionType as _FUNCTION, ModuleType as _MODULE
from types import TracebackType as _TRACEBACK

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
# Whether two absolute paths with one final component name one file, which
# only links can make true; resolving them costs system calls.
resolved = {}
# Other threads run concurrently with this one in a free-threaded build, so
# their frames can change, and be freed, under a reader.
FREE_THREADED = 't' in sys.abiflags
_objects = gc.get_objects
# Configuration and callbacks can run concurrently in free-threaded builds.
# No native hook is called while this lock is held.
lock = _bootstrap_lock

# Only these events have callback signatures this transport understands.
LOCAL = ("LINE", "PY_START", "PY_RESUME", "PY_RETURN", "PY_YIELD")
OTHER = ("RAISE", "PY_UNWIND", "PY_THROW")
# Steps also watch a caller's instructions, which no probe names.
STEPPING = ("INSTRUCTION",)

step = None
# The generation that configured the step, which binds only in a callback or
# call that began before it, at the stop the step begins from.
step_generation = None
# The bound step: [phase, frame]. FRAME watches the stepping frame, CALLER
# the Python frame it returned to, THREAD any line its thread runs next.
stepping = None
# The code the step adds local events to, and those events.
stepped = None
INTO, OVER, OUT = 0, 1, 2
FRAME, CALLER, THREAD, DONE = 0, 1, 2, 3
# A coroutine's or async generator's frame yields as it suspends at an
# await; its step goes on to its next line rather than leaving it.
SUSPENDS = 0x80 | 0x100 | 0x200

# Which exceptions report.
raising = 0
RAISED, UNCAUGHT = 1, 2
# The longest exception text a report carries, in characters.
MAX_TEXT = 1000
# These read what C stores, so no program's code runs as they do.
_args = BaseException.__dict__['args'].__get__
_traceback = BaseException.__dict__['__traceback__'].__get__
_qualname = type.__dict__['__qualname__'].__get__
_module = type.__dict__['__module__'].__get__
_mro = type.__dict__['__mro__'].__get__
_namespace = type.__dict__['__dict__'].__get__
_BASE_STR = BaseException.__dict__['__str__']
_KEY_STR = KeyError.__dict__['__str__']
# The code of the frame that runs a threading.Thread, which catches what
# the thread raises and gives it to threading.excepthook.
_bootstrap = None
_main_ident = _thread._get_main_thread_ident


class CommandError(ValueError):
    """A malformed debugger request, distinct from an interrupted decoder."""


def matches(probe, code):
    return (id(code) not in INTERNAL
            and (not probe[3] or same_file(probe[3], code.co_filename))
            and (not probe[5] or probe[5] == code.co_qualname
                 or code.co_qualname.endswith('.' + probe[5])))


def same_file(path, filename):
    """Whether code compiled from `filename` is in the file a probe names.

    An absolute path names one file, also through links to it or to its
    directories under the same final name; a relative path names every file
    whose path ends with its components.
    """
    if filename == path:
        return True
    if path[0] != '/':
        return filename.endswith('/' + path)
    if filename[:1] != '/' or filename.rpartition('/')[2] != path.rpartition('/')[2]:
        return False
    key = (path, filename)
    same = resolved.get(key)
    if same is None:
        try:
            same = _realpath(filename) == _realpath(path)
        except (OSError, ValueError):
            same = False
        resolved[key] = same
    return same


def clear():
    global tool, probes, step, stepping, stepped, raising
    if tool is not None:
        monitoring.free_tool_id(tool)
        tool = None
    probes = ()
    step = stepping = stepped = None
    raising = 0
    discovered.clear()
    resolved.clear()


def dispatch(event, code, location):
    return tuple((probe[1], probe[2], 0) for probe in probes
                 if probe[0] == event and matches(probe, code)
                 and (not probe[4] or (event == "LINE" and probe[4] == location)))


def together(hits):
    """The reports of an event's hits: every breakpoint hit in one, so the
    debugger stops once for them all, as it does at one native address.

    One number is reported whole; several by their count and the address of
    an array of them, which lives while the native hook runs.
    """
    numbers = [number for kind, number, _ in hits if kind == 1]
    others = [hit for hit in hits if hit[0] != 1]
    if len(numbers) < 2:
        return [hit for hit in hits if hit[0] == 1] + others
    array = (ctypes.c_uint64 * len(numbers))(*numbers)
    return [(1, len(numbers), ctypes.addressof(array), array)] + others


def reported(notice):
    """The hits a report carries, by kind and number."""
    if len(notice) == 4:
        return [(1, number) for number in notice[3]] if notice[0] == 1 else []
    if notice[0] in (1, 2):
        return [notice[:2]]
    return []


def local_mask(code):
    """The local events `code` takes: its probes', and the step's."""
    mask = 0
    for probe in probes:
        if probe[0] in LOCAL and matches(probe, code):
            mask |= getattr(events, probe[0])
    if stepped is not None and stepped[0] is code:
        mask |= stepped[1]
    return mask


def global_mask():
    """The events all code takes: discovery, the probes' other events, and
    what the bound step watches everywhere."""
    mask = events.PY_START | events.PY_RESUME | events.PY_THROW
    for probe in probes:
        if probe[0] in OTHER:
            mask |= getattr(events, probe[0])
    if stepping is not None and stepping[0] != DONE:
        mask |= events.PY_UNWIND
        if step[1] == INTO or stepping[0] == THREAD:
            mask |= events.LINE
    if raising & RAISED:
        mask |= events.RAISE
    if raising & UNCAUGHT:
        mask |= events.PY_UNWIND
    return mask


def watch(phase, frame):
    """Moves the bound step to `phase`, watching `frame`, with the events it
    needs, and takes away those it no longer does."""
    global stepped
    stepping[0], stepping[1] = phase, frame
    previous = stepped
    stepped = None
    if phase == FRAME:
        mask = events.PY_RETURN | events.PY_YIELD
        if step[1] != OUT:
            mask |= events.LINE
        stepped = (frame.f_code, mask)
    elif phase == CALLER:
        stepped = (frame.f_code, events.INSTRUCTION)
    if previous is not None and (stepped is None or previous[0] is not stepped[0]):
        monitoring.set_local_events(tool, previous[0], local_mask(previous[0]))
    if stepped is not None:
        monitoring.set_local_events(tool, stepped[0], local_mask(stepped[0]))
    monitoring.set_events(tool, global_mask())


def bind(frame):
    """Binds the configured step to its frame in the chain `frame` is in, on
    the step's thread; or reports that the frame is not there."""
    global stepping
    chain = []
    while frame is not None:
        chain.append(frame)
        frame = frame.f_back
    index = len(chain) - 1 - step[3]
    stepping = [DONE, None]
    if not 0 <= index < len(chain) or id(chain[index].f_code) != step[4]:
        return [(2, step[0], 1)]  # The step's frame is not where it was found.
    watch(FRAME, chain[index])
    return []


def waiting(frame, started):
    """Binds a configured step waiting for this thread, from where it is, in
    a callback or call that began at generation `started`."""
    global stepping
    if (tool is None or step is None or stepping is not None
            or step[2] != _thread.get_native_id()):
        return []
    if step_generation <= started:
        stepping = [DONE, None]
        return [(2, step[0], 2)]  # Its thread ran on before the step began.
    return bind(frame)


def finish():
    """Ends the bound step where its thread is."""
    number = step[0]
    watch(DONE, None)
    return [(2, number, 0)]


def leave(frame):
    """Follows the step out of `frame`, which returns, yields, or unwinds, to
    its Python caller; or to the next line its thread runs, if none."""
    caller = frame.f_back
    if caller is None:
        watch(THREAD, None)
    else:
        watch(CALLER, caller)
    return []


def step_event(event, code, frame):
    """What the bound step does at `event` in `frame`, running `code`."""
    global step
    phase, target = stepping
    kind = step[1]
    if frame is target:
        if phase == FRAME:
            if event == 'LINE' and kind != OUT:
                return finish()
            if event == 'PY_YIELD' and code.co_flags & SUSPENDS:
                if kind == INTO:
                    # Into the task's next line, not the loop that runs it.
                    step = (step[0], OVER) + step[2:]
                    watch(FRAME, target)
                return []
            if event in ('PY_RETURN', 'PY_YIELD', 'PY_UNWIND'):
                return leave(target)
        elif phase == CALLER:
            # Some instructions have no line, as a handler's first does.
            if event == 'INSTRUCTION' and frame.f_lineno is not None:
                return finish()
            if event == 'PY_UNWIND':
                return leave(target)
        return []
    if (event == 'LINE' and (kind == INTO or phase == THREAD)
            and _thread.get_native_id() == step[2]):
        return finish()
    return []


def discover(code):
    identity = id(code)
    previous = discovered.get(identity)
    if previous is not None and previous() is code:
        return
    mask = local_mask(code)
    if mask:
        # A freed tool's events linger on code until it next starts or
        # resumes. Setting the same events again would change nothing,
        # leaving locations that tool disabled disabled.
        if monitoring.get_local_events(tool, code):
            monitoring.set_local_events(tool, code, 0)
        monitoring.set_local_events(tool, code, mask)

    # Code equality can ignore its filename. Cache by identity and retain
    # only a weak reference, so dynamic code neither aliases nor leaks.
    def expired(reference):
        with lock:
            if discovered.get(identity) is reference:
                discovered.pop(identity, None)
    discovered[identity] = _weakref.ref(code, expired)


def running_code():
    """Code that may be running now, including on other threads."""
    if FREE_THREADED:
        # Never read another thread's frames. The collector tracks code in
        # this build and lists it while every thread is at a safe point;
        # code is immutable. Uncalled matching code is instrumented early.
        return [code for code in _objects() if type(code) is CODE
                and any(probe[0] in LOCAL and matches(probe, code)
                        for probe in probes)]
    # The GIL keeps another thread's frame objects consistent here.
    codes = []
    for frame in sys._current_frames().values():
        while frame is not None:
            codes.append(frame.f_code)
            frame = frame.f_back
    return codes


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
        if (type(decoded) is not list or len(decoded) not in (2, 3, 4)
                or type(decoded[0]) is not int or decoded[0] != wanted_generation):
            raise CommandError('invalid microscope command generation')
        wanted_step = decoded[2] if len(decoded) >= 3 else None
        wanted_exceptions = decoded[3] if len(decoded) >= 4 else 0
        return configure(_owner, wanted_generation, decoded[1], wanted_step=wanted_step,
                         wanted_exceptions=wanted_exceptions)
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
        report_hook(report[0], report[1], report[2])
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


def outermost(frame):
    """Whether an exception that leaves `frame` leaves its thread's Python
    code for Python's own report of it: the main module's code; another
    thread's first frame, whose exception `_thread` reports; or the frame
    threading runs a thread from, which gives what the thread raises to
    threading.excepthook. Any other frame C calls on the main thread leaves
    what it raises to C, which may handle it, as the import system does as
    the interpreter starts."""
    global _bootstrap
    caller = frame.f_back
    if caller is None and _thread.get_ident() != _main_ident():
        return True
    if main_module(frame, caller):
        return True
    if caller is None:
        return False
    if _bootstrap is None:
        threading = sys.modules.get('threading')
        if type(threading) is not _MODULE:
            return False
        thread = threading.__dict__.get('Thread')
        if type(thread) is not type:
            return False
        inner = _namespace(thread).get('_bootstrap_inner')
        if type(inner) is not _FUNCTION:
            return False
        _bootstrap = inner.__code__
    return caller.f_code is _bootstrap


def main_module(frame, caller):
    """Whether `frame` runs the main module's code on the main thread, as the
    interpreter runs it: called from C, or through runpy alone, for -m."""
    if frame.f_code.co_name != '<module>' or _thread.get_ident() != _main_ident():
        return False
    names = frame.f_globals
    name = dict.get(names, '__name__') if isinstance(names, dict) else None
    if type(name) is not str or name != '__main__':
        return False
    runpy = sys.modules.get('runpy')
    runner = runpy.__dict__ if type(runpy) is _MODULE else None
    for _ in range(64):
        if caller is None:
            return True
        if runner is None or caller.f_globals is not runner:
            return False
        caller = caller.f_back
    return False


def described(exc):
    """What Python's traceback would print last for `exc`, the exception's
    type and text, where that runs none of the program's code; otherwise
    its type alone."""
    cls = type(exc)
    name = _qualname(cls)
    module = _module(cls)
    if type(module) is str and module not in ('builtins', '__main__'):
        name = module + '.' + name
    method = None
    for klass in _mro(cls):
        method = _namespace(klass).get('__str__')
        if method is not None:
            break
    args = _args(exc)
    text = None
    if type(args) is tuple:
        if method is _BASE_STR:
            if not args:
                text = ''
            elif len(args) == 1 and type(args[0]) is str:
                text = args[0]
        elif method is _KEY_STR and len(args) == 1 and type(args[0]) is str:
            text = repr(args[0])
    if not text:
        return name
    if len(text) > MAX_TEXT:
        text = text[:MAX_TEXT] + '…'
    return name + ': ' + text


def origin(exc):
    """Where `exc` was raised, by its traceback's innermost entry."""
    entry = _traceback(exc)
    innermost = None
    for _ in range(4096):
        if type(entry) is not _TRACEBACK:
            break
        innermost, entry = entry, entry.tb_next
    if innermost is None:
        return ''
    code = innermost.tb_frame.f_code
    line = innermost.tb_lineno
    place = code.co_filename if type(line) is not int else f'{code.co_filename}:{line}'
    return f', raised in {code.co_qualname} at {place}'


def exception_event(event, frame, exc):
    """The report of an exception at `event` in `frame`, if one stops: one
    raised there, or one leaving its thread's Python code there."""
    if not issubclass(type(exc), BaseException):
        return []
    if event == 'RAISE':
        if not raising & RAISED:
            return []
        # RAISE fires again in each frame an exception unwinds into, whose
        # traceback then goes on into the frame it called: that is no raise.
        entry = _traceback(exc)
        following = entry.tb_next if type(entry) is _TRACEBACK else None
        if type(following) is _TRACEBACK and following.tb_frame.f_back is frame:
            return []
        bit, message = RAISED, described(exc)
    else:
        # Python exits on SystemExit, and prints no traceback.
        if (not raising & UNCAUGHT or not outermost(frame)
                or issubclass(type(exc), SystemExit)):
            return []
        bit, message = UNCAUGHT, described(exc) + origin(exc)
    encoded = message.encode('utf-8', 'backslashreplace')
    text = (ctypes.c_char * len(encoded)).from_buffer_copy(encoded)
    record = (ctypes.c_uint64 * 3)(id(exc), ctypes.addressof(text), len(encoded))
    # The objects live as long as the report, while the native hook runs.
    return [(4, bit, ctypes.addressof(record), (record, text, exc))]


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
                started = generation
                notices = []
                changed = pending()
                if changed is not None:
                    notices.append(changed)
                if tool is not None:
                    try:
                        if event in ('PY_START', 'PY_RESUME', 'PY_THROW'):
                            discover(code)
                        notices.extend(together(dispatch(event, code, location)))
                        if stepping is not None and stepping[0] != DONE:
                            notices.extend(step_event(event, code, sys._getframe(1)))
                        if raising and event in ('RAISE', 'PY_UNWIND'):
                            notices.extend(exception_event(event, sys._getframe(1), unused[0]))
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
            emitted = set()
            while notices:
                notice = notices.pop(0)
                superseded = emit(notice, report_hook, report_owner, report_generation)
                emitted.update(reported(notice))
                if superseded:
                    # A consumed configuration supersedes the batch. Sites
                    # keep their numbers, so report what this event hits
                    # under it that has not been reported already.
                    with lock:
                        notices = [] if tool is None else together([
                            hit for hit in dispatch(event, code, location)
                            if hit[:2] not in emitted])
                        report_hook, report_owner, report_generation = (
                            hook, _owner, generation)
            # A step configured at this event's stop begins here, before its
            # thread runs on: either this thread consumed it at the stop, or
            # another did while this one waited for the lock.
            with lock:
                notices = waiting(sys._getframe(1), started)
                report_hook, report_owner, report_generation = hook, _owner, generation
            for notice in notices:
                emit(notice, report_hook, report_owner, report_generation)
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



def configure(library, wanted_generation, wanted_probes, replacement=None,
              wanted_step=None, wanted_exceptions=0):
    global _library, hook, generation, tool, probes, _owner, _control, _sequence
    global step, stepping, step_generation, raising
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
    if wanted_step is not None:
        if (type(wanted_step) not in (list, tuple) or len(wanted_step) != 5
                or any(type(field) is not int for field in wanted_step)
                or not 0 < wanted_step[0] < 2**64 or wanted_step[1] not in (INTO, OVER, OUT)
                or not 0 < wanted_step[2] < 2**64 or not 0 <= wanted_step[3] < 2**32
                or not 0 < wanted_step[4] < 2**64):
            raise CommandError("invalid microscope step")
        wanted_step = tuple(wanted_step)
    if type(wanted_exceptions) is not int or not 0 <= wanted_exceptions <= RAISED | UNCAUGHT:
        raise CommandError("invalid microscope exceptions")

    if library != _owner and replacement is None:
        raise RuntimeError("microscope transport owner changed during preparation")
    # A step already bound goes on from where it is: binding it again later
    # would begin it where its thread has run to.
    kept = (step, stepping, step_generation) if (
        wanted_step is not None and step is not None and step[0] == wanted_step[0]) else None
    clear()
    if library != _owner:
        _library, hook, _control, _sequence = replacement
        _owner = library
    generation = wanted_generation
    if not wanted_probes and wanted_step is None and not wanted_exceptions:
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
        raising = wanted_exceptions
        for event in LOCAL + OTHER + STEPPING:
            monitoring.register_callback(tool, getattr(events, event), callback(event))
        monitoring.set_events(tool, global_mask())
        # Enable discovery before scanning: another thread can enter code
        # while this one configures a free-threaded interpreter.
        if any(probe[0] in LOCAL for probe in probes):
            for code in running_code():
                discover(code)
        if kept is not None:
            step, stepping, step_generation = kept
            if stepping is not None and stepping[0] != DONE:
                watch(stepping[0], stepping[1])
        else:
            step, step_generation = wanted_step, generation
    except Exception:
        return failed(generation)
    except BaseException:
        interrupted(generation)
        raise
    return (0, generation, tool)


def apply(library, wanted_generation, wanted_probes, wanted_step=None, wanted_exceptions=0):
    was_busy = getattr(_local, 'busy', False)
    _local.busy = True
    if not was_busy:
        _local.suppressed = None
        _local.interruption = None
        _local.context = None
    try:
        with lock:
            _local.context = (hook, _owner, generation)
            started = generation
            needs_transport = library != _owner
        # The loader may wait for a thread whose monitoring callback needs
        # our lock. Load outside it; configure revalidates ownership inside.
        replacement = transport(library) if needs_transport else None
        with lock:
            next_hook = hook if replacement is None else replacement[1]
            _local.context = (next_hook, library, wanted_generation)
            report = configure(library, wanted_generation, wanted_probes, replacement,
                               wanted_step, wanted_exceptions)
            missed = interference()
            if missed is not None:
                report = missed
            report_hook, report_owner, report_generation = hook, _owner, generation
        if report is not None:
            emit(report, report_hook, report_owner, report_generation)
        with lock:
            notices = waiting(sys._getframe(1), started)
            report_hook, report_owner, report_generation = hook, _owner, generation
        for notice in notices:
            emit(notice, report_hook, report_owner, report_generation)
    except BaseException:
        emit_interruption()
        raise
    finally:
        _local.busy = was_busy


def forking():
    """Takes the lock as the process forks, so that no other thread holds
    it in the child, where that thread would not exist to release it. The
    child's earlier fork handlers run with the agent still armed, and their
    callbacks take the lock again on this thread."""
    _local.forking = False
    lock.acquire()
    _local.forking = True


def forked_parent():
    if getattr(_local, 'forking', False):
        _local.forking = False
        lock.release()


def forked():
    """Disarms the agent in a forked child, which no debugger traces, so its
    monitoring would only slow the child and report to no one; a debugger
    that adopts the child installs it anew."""
    lock._at_fork_reinit()
    _local.forking = _local.busy = False
    try:
        clear()
    except Exception:
        pass  # Nothing in the child could hear of it.


_register_at_fork(before=forking, after_in_parent=forked_parent, after_in_child=forked)

# Suppress only our own code objects, never functions guessed from their names,
# nor those imported from the standard library, as os.path.realpath.
_pending = [value.__code__ for value in tuple(globals().values())
            if type(value) is type(matches) and value.__globals__ is globals()]
_internal = set()
while _pending:
    _code = _pending.pop()
    _internal.add(id(_code))
    _pending.extend(value for value in _code.co_consts if type(value) is type(_code))
INTERNAL = frozenset(_internal)
del _pending, _internal, _code
CODE = type(matches.__code__)
if FREE_THREADED and not gc.is_tracked(matches.__code__):
    raise RuntimeError("microscope cannot find running code safely: this "
                       "free-threaded build's collector does not track code")

# Published only after initialization succeeds; the bootstrap serializes it.
_microscope_loaded = 2
