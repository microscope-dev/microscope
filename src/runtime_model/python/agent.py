

























import sys

if sys.version_info < (3, 12):
    raise RuntimeError("microscope's monitoring transport requires CPython 3.12 or later")

import builtins as _builtins
import ctypes
import gc
import json
import _thread
import _weakref
from os import getpid as _getpid, register_at_fork as _register_at_fork
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
_configured_in = None
_control = None
_sequence = None
_local = _thread._local()
CONTROL_HEADER = 32
CONTROL_CAPACITY = 65536
CONTROL_MAGIC = b'MICPYC02'
discovered = {}

resolved = {}

FREE_THREADED = 't' in sys.abiflags
_objects = gc.get_objects


lock = _bootstrap_lock


LOCAL = ("LINE", "PY_START", "PY_RESUME", "PY_RETURN", "PY_YIELD")
OTHER = ("RAISE", "PY_UNWIND", "PY_THROW")


STEPPING = ("INSTRUCTION", "CALL")

step = None


step_generation = None


stepping = None

stepped = None
INTO, OVER, OUT, BACK = 0, 1, 2, 3
FRAME, CALLER, THREAD, DONE = 0, 1, 2, 3


SUSPENDS = 0x80 | 0x100 | 0x200



assignment = None
assigned = 0
_kept = []

ASSIGNED = ("int", "float", "bool", "str", "none", "object")
_SINGLETONS = (None, True, False)
_MISSING = object()

raising = 0
RAISED, UNCAUGHT = 1, 2

MAX_TEXT = 1000

_args = BaseException.__dict__['args'].__get__
_traceback = BaseException.__dict__['__traceback__'].__get__
_qualname = type.__dict__['__qualname__'].__get__
_module = type.__dict__['__module__'].__get__
_mro = type.__dict__['__mro__'].__get__
_namespace = type.__dict__['__dict__'].__get__
_BASE_STR = BaseException.__dict__['__str__']
_KEY_STR = KeyError.__dict__['__str__']

_BUILTIN = type(len)
_METHOD = type(list.append)
_bound_to = _BUILTIN.__dict__['__self__'].__get__
_objclass = _METHOD.__dict__['__objclass__'].__get__


_bootstrap = None
_main_ident = getattr(_thread, '_get_main_thread_ident', None)
if _main_ident is None:


    _MAIN = getattr(sys.modules.get('_microscope_bootstrap'), 'main', None)
    if type(_MAIN) is not int:
        raise RuntimeError("microscope's agent needs its launch bootstrap on CPython 3.12")

    def _main_ident():
        return _MAIN


class CommandError(ValueError):
    """A malformed debugger request, distinct from an interrupted decoder."""


def matches(probe, code):
    return (id(code) not in INTERNAL
            and (not probe[3] or same_file(probe[3], code.co_filename))
            and (not probe[5] or probe[5] == code.co_qualname
                 or code.co_qualname.endswith('.' + probe[5])))


def same_file(path, filename):


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
        if not hasattr(monitoring, 'clear_tool_id'):

            monitoring.set_events(tool, 0)
            for event in LOCAL + OTHER + STEPPING:
                monitoring.register_callback(tool, getattr(events, event), None)
            for reference in tuple(discovered.values()):
                code = reference()
                if code is not None and monitoring.get_local_events(tool, code):
                    monitoring.set_local_events(tool, code, 0)
            if stepped is not None:
                monitoring.set_local_events(tool, stepped[0], 0)
        monitoring.free_tool_id(tool)
        tool = None
    probes = ()
    step = stepping = stepped = None
    raising = 0
    discovered.clear()
    resolved.clear()
    assert tool is None, f"tool {tool} is still held after clearing"


def dispatch(event, code, location):
    return tuple((probe[1], probe[2], 0) for probe in probes
                 if probe[0] == event and matches(probe, code)
                 and (not probe[4] or (event == "LINE" and probe[4] == location)))


def together(hits):


    assert all(hit[0] in (1, 2) for hit in hits), f"hit kinds {[hit[0] for hit in hits]}"
    numbers = [number for kind, number, _ in hits if kind == 1]
    others = [hit for hit in hits if hit[0] != 1]
    if len(numbers) < 2:
        return [hit for hit in hits if hit[0] == 1] + others
    array = (ctypes.c_uint64 * len(numbers))(*numbers)
    return [(1, len(numbers), ctypes.addressof(array), array)] + others


def reported(notice):

    assert len(notice) in (3, 4), f"a notice of kind {notice[0]} has {len(notice)} fields"
    if len(notice) == 4:
        return [(1, number) for number in notice[3]] if notice[0] == 1 else []
    if notice[0] in (1, 2):
        return [notice[:2]]
    return []


def local_mask(code):

    assert tool is not None, "local events asked for without a tool"
    mask = 0
    for probe in probes:
        if probe[0] in LOCAL and matches(probe, code):
            mask |= getattr(events, probe[0])
    if stepped is not None and stepped[0] is code:
        mask |= stepped[1]
    return mask


def global_mask():


    assert stepping is None or step is not None, f"no step is configured, bound in {stepping[0]}"
    mask = events.PY_START | events.PY_RESUME | events.PY_THROW
    for probe in probes:
        if probe[0] in OTHER:
            mask |= getattr(events, probe[0])
    if stepping is not None and stepping[0] != DONE:
        mask |= events.PY_UNWIND
        if step[1] == INTO or stepping[0] == THREAD:
            mask |= events.LINE
    elif step is not None and step[1] == BACK and stepping is None:
        mask |= events.LINE
    if raising & RAISED:
        mask |= events.RAISE
    if raising & UNCAUGHT:
        mask |= events.PY_UNWIND
    return mask


def watch(phase, frame):

    global stepped
    assert (frame is None) == (phase in (THREAD, DONE)), f"phase {phase}, framed {frame is not None}"
    stepping[0], stepping[1] = phase, frame
    previous = stepped
    stepped = None
    if phase == FRAME:
        mask = events.PY_RETURN | events.PY_YIELD
        if step[1] != OUT:
            mask |= events.LINE
        if step[1] == INTO:
            mask |= events.CALL
        stepped = (frame.f_code, mask)
    elif phase == CALLER:
        stepped = (frame.f_code, events.INSTRUCTION)
    if previous is not None and (stepped is None or previous[0] is not stepped[0]):
        monitoring.set_local_events(tool, previous[0], local_mask(previous[0]))
    if stepped is not None:
        monitoring.set_local_events(tool, stepped[0], local_mask(stepped[0]))
    monitoring.set_events(tool, global_mask())


def bind(frame):


    global stepping
    assert stepping is None, f"binding step {step[0]} while it is bound in phase {stepping[0]}"
    assert step[1] != BACK, f"binding step back {step[0]} to a frame"
    chain = []
    while frame is not None:
        chain.append(frame)
        frame = frame.f_back
    index = len(chain) - 1 - step[3]
    stepping = [DONE, None]
    if not 0 <= index < len(chain) or id(chain[index].f_code) != step[4]:
        return [(2, step[0], 1)]
    watch(FRAME, chain[index])
    return []


def waiting(frame, started, event=None):


    return assigning(frame) + step_waiting(frame, started, event)


def assigning(frame):

    global assigned
    if (assignment is None or assignment[0] <= assigned
            or assignment[1] != _thread.get_native_id()):
        return []
    number, _, depth, code, name, value = assignment
    assigned = number
    chain = []
    while frame is not None:
        chain.append(frame)
        frame = frame.f_back
    index = len(chain) - 1 - depth
    if not 0 <= index < len(chain) or id(chain[index].f_code) != code:
        return [(6, number, 1)]
    target = chain[index]
    given = assigned_value(target, value)
    if given is _MISSING:
        return [(6, number, 2)]
    if sys.version_info < (3, 13):
        return [(6, number, 3)]
    try:
        variables = target.f_locals
        old = variables.get(name, _MISSING)
        variables[name] = given
    except Exception:
        return [(6, number, 3)]
    if old is not _MISSING:
        _kept.append(old)
    return [(6, number, 0)]


def assigned_value(frame, value):

    kind = value[0]
    if kind == "int":
        return int(value[1])
    if kind == "float":
        return ctypes.c_double.from_buffer_copy(value[1].to_bytes(8, 'little')).value
    if kind == "bool":
        return value[1] == 1
    if kind == "str":
        return value[1]
    if kind == "none":
        return None
    assert kind == "object", f"assignment value kind {kind}"
    for candidate in _SINGLETONS:
        if id(candidate) == value[1]:
            return candidate
    for names in (frame.f_locals, frame.f_globals, frame.f_builtins):
        for candidate in names.values():
            if id(candidate) == value[1]:
                return candidate
    return _MISSING


def release():

    with lock:
        kept = _kept[:]
        _kept.clear()
    del kept


def step_waiting(frame, started, event):

    global stepping
    if (tool is None or step is None or stepping is not None
            or step[2] != _thread.get_native_id()):
        return []
    if step[1] == BACK:
        return returned(frame, event)
    assert step_generation is not None, f"step {step[0]} has no generation"
    if step_generation <= started:
        stepping = [DONE, None]
        return [(2, step[0], 2)]
    return bind(frame)


def returned(frame, event):


    global stepping
    assert step[1] == BACK, f"step {step[0]} of kind {step[1]} ends as a step back"
    assert stepping is None, f"step back {step[0]} is already bound"
    if event != 'LINE':
        return []
    depth = 0
    caller = frame.f_back
    while caller is not None:
        depth += 1
        caller = caller.f_back
    if depth > step[3] or (depth == step[3] and id(frame.f_code) != step[4]):
        return []
    stepping = [DONE, None]
    return finish()


def native(callable):



    kind = type(callable)
    if kind is _BUILTIN:
        bound = _bound_to(callable)
        if type(bound) is _MODULE:
            return bound is not _builtins
        return not builtin_type(type(bound))
    if kind is _METHOD:
        return not builtin_type(_objclass(callable))
    return False


def builtin_type(cls):
    module = _module(cls)
    return type(module) is str and module == 'builtins'


def finish():

    number = step[0]
    watch(DONE, None)
    return [(2, number, 0)]


def leave(frame):


    assert stepping[0] in (FRAME, CALLER), f"step {step[0]} leaves a frame in phase {stepping[0]}"
    caller = frame.f_back
    if caller is None:
        watch(THREAD, None)
    else:
        watch(CALLER, caller)
    return []


def step_event(event, code, frame, arguments):


    global step
    phase, target = stepping
    assert phase != DONE, f"step {step[0]} takes an event after it ended"
    kind = step[1]
    if frame is target:
        if phase == FRAME:
            if event == 'CALL':


                if kind == INTO and native(arguments[0]):
                    return [(5, step[0], id(arguments[0]))]
                return []
            if event == 'LINE' and kind != OUT:
                return finish()
            if event == 'PY_YIELD' and code.co_flags & SUSPENDS:
                if kind == INTO:

                    step = (step[0], OVER) + step[2:]
                    watch(FRAME, target)
                return []
            if event in ('PY_RETURN', 'PY_YIELD', 'PY_UNWIND'):
                return leave(target)
        elif phase == CALLER:

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
    assert tool is not None, "discovering code without a tool"
    identity = id(code)
    previous = discovered.get(identity)
    if previous is not None and previous() is code:
        return
    mask = local_mask(code)
    if mask:


        if monitoring.get_local_events(tool, code):
            monitoring.set_local_events(tool, code, 0)
        monitoring.set_local_events(tool, code, mask)



    def expired(reference):
        with lock:
            if discovered.get(identity) is reference:
                discovered.pop(identity, None)
    discovered[identity] = _weakref.ref(code, expired)
    assert discovered[identity]() is code, "discovered code is not the code it names"


def running_code():

    if FREE_THREADED:



        return [code for code in _objects() if type(code) is CODE
                and any(probe[0] in LOCAL and matches(probe, code)
                        for probe in probes)]

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
        return (3, generation, 3)
    return (3, generation, reason)


def interference():
    missed = getattr(_local, 'suppressed', None)
    _local.suppressed = None
    if missed is not None:
        number, report_owner, report_hook = missed
        if number == generation and report_owner == _owner and report_hook is hook:
            return failed(number, 5)

    return None


def integer(text):
    if len(text) > 20:
        raise CommandError('microscope command integer exceeds 64-bit precision')
    return int(text)


def request(payload):
    text = payload.decode('utf-8')


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


    assert _sequence is not None, "a command is read before a transport is loaded"
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
        if (type(decoded) is not list or len(decoded) not in (2, 3, 4, 5)
                or type(decoded[0]) is not int or decoded[0] != wanted_generation):
            raise CommandError('invalid microscope command generation')
        wanted_step = decoded[2] if len(decoded) >= 3 else None
        wanted_exceptions = decoded[3] if len(decoded) >= 4 else 0
        wanted_assignment = decoded[4] if len(decoded) == 5 else None
        return configure(_owner, wanted_generation, decoded[1], wanted_step=wanted_step,
                         wanted_exceptions=wanted_exceptions,
                         wanted_assignment=wanted_assignment)
    except (CommandError, json.JSONDecodeError, UnicodeDecodeError):
        return failed(wanted_generation)
    except BaseException:
        interrupted(wanted_generation)
        raise


def emit(report, report_hook, report_owner, report_generation):


    changed = False
    while report is not None:
        assert len(report) in (3, 4), f"a report of kind {report[0]} has {len(report)} fields"
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


                captured = ((3, number, 3), report_hook, report_owner, number)
        _local.interruption = None
    assert captured is None or len(captured) == 4, f"an interruption has {len(captured)} fields"
    if captured is not None and captured[1] is not None:
        emit(*captured)


def would_report(event, code, location):
    if dispatch(event, code, location):
        return True


    return (event in ('PY_START', 'PY_RESUME', 'PY_THROW')
            and any(matches(probe, code) and probe[0] in LOCAL
                    and (not probe[4] or any(line == probe[4]
                                             for _, _, line in code.co_lines()))
                    for probe in probes))


def outermost(frame):



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


    assert event in ('RAISE', 'PY_UNWIND'), f"exception report at {event}"
    if not issubclass(type(exc), BaseException):
        return []
    if event == 'RAISE':
        if not raising & RAISED:
            return []


        entry = _traceback(exc)
        following = entry.tb_next if type(entry) is _TRACEBACK else None
        if type(following) is _TRACEBACK and following.tb_frame.f_back is frame:
            return []
        bit, message = RAISED, described(exc)
    else:

        if (not raising & UNCAUGHT or not outermost(frame)
                or issubclass(type(exc), SystemExit)):
            return []
        bit, message = UNCAUGHT, described(exc) + origin(exc)
    encoded = message.encode('utf-8', 'backslashreplace')
    text = (ctypes.c_char * len(encoded)).from_buffer_copy(encoded)
    record = (ctypes.c_uint64 * 3)(id(exc), ctypes.addressof(text), len(encoded))

    return [(4, bit, ctypes.addressof(record), (record, text, exc))]


def callback(event):
    assert event in LOCAL + OTHER + STEPPING, f"a callback for {event}"

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
        if _kept:
            release()
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
                            notices.extend(step_event(event, code, sys._getframe(1), unused))
                        if raising and event in ('RAISE', 'PY_UNWIND'):
                            notices.extend(exception_event(event, sys._getframe(1), unused[0]))
                    except Exception:


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


                    with lock:
                        notices = [] if tool is None else together([
                            hit for hit in dispatch(event, code, location)
                            if hit[:2] not in emitted])
                        report_hook, report_owner, report_generation = (
                            hook, _owner, generation)



            with lock:
                notices = waiting(sys._getframe(1), started, event)
                report_hook, report_owner, report_generation = hook, _owner, generation
            while notices:
                notice = notices.pop(0)
                if emit(notice, report_hook, report_owner, report_generation):


                    with lock:
                        notices = waiting(sys._getframe(1), started, event)
                        report_hook, report_owner, report_generation = (
                            hook, _owner, generation)
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
              wanted_step=None, wanted_exceptions=0, wanted_assignment=None):
    global _library, hook, generation, tool, probes, _owner, _control, _sequence, _configured_in
    global step, stepping, step_generation, raising, assignment
    if type(wanted_generation) is not int or not 0 <= wanted_generation < 2**64:
        raise CommandError("invalid microscope generation")
    if library == _owner and wanted_generation <= generation:
        return None


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
                or not 0 < wanted_step[0] < 2**64 or wanted_step[1] not in (INTO, OVER, OUT, BACK)
                or not 0 < wanted_step[2] < 2**64 or not 0 <= wanted_step[3] < 2**32
                or not 0 < wanted_step[4] < 2**64):
            raise CommandError("invalid microscope step")
        wanted_step = tuple(wanted_step)
    if type(wanted_exceptions) is not int or not 0 <= wanted_exceptions <= RAISED | UNCAUGHT:
        raise CommandError("invalid microscope exceptions")
    if wanted_assignment is not None:
        wanted_assignment = valid_assignment(wanted_assignment)

    if library != _owner and replacement is None:
        raise RuntimeError("microscope transport owner changed during preparation")


    kept = (step, stepping, step_generation) if (
        wanted_step is not None and step is not None and step[0] == wanted_step[0]) else None
    clear()
    if library != _owner:
        _library, hook, _control, _sequence = replacement
        _owner = library
    _configured_in = _getpid()
    generation = wanted_generation
    assignment = wanted_assignment
    if not wanted_probes and wanted_step is None and not wanted_exceptions:
        return (0, generation, 6)

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


        if any(probe[0] in LOCAL for probe in probes):
            for code in running_code():
                discover(code)
        if kept is not None:
            step, stepping, step_generation = kept
            if stepping is not None and stepping[0] != DONE:
                watch(stepping[0], stepping[1])
        else:
            step, step_generation = wanted_step, generation

        monitoring.set_events(tool, global_mask())
    except Exception:
        return failed(generation)
    except BaseException:
        interrupted(generation)
        raise
    return (0, generation, tool)


def valid_assignment(wanted):

    if (type(wanted) not in (list, tuple) or len(wanted) != 6
            or any(type(field) is not int for field in wanted[:4])
            or not 0 < wanted[0] < 2**64 or not 0 < wanted[1] < 2**64
            or not 0 <= wanted[2] < 2**32 or not 0 < wanted[3] < 2**64
            or type(wanted[4]) is not str or type(wanted[5]) not in (list, tuple)
            or not wanted[5] or wanted[5][0] not in ASSIGNED):
        raise CommandError("invalid microscope assignment")
    value = tuple(wanted[5])
    kind = value[0]
    valid = (
        (kind == "none" and len(value) == 1)
        or (len(value) == 2 and (
            (kind == "int" and type(value[1]) is str and 0 < len(value[1]) <= 40
             and value[1].removeprefix('-').isdigit() and value[1].isascii())
            or (kind in ("float", "object") and type(value[1]) is int
                and 0 <= value[1] < 2**64)
            or (kind == "bool" and value[1] in (0, 1) and type(value[1]) is int)
            or (kind == "str" and type(value[1]) is str))))
    if not valid:
        raise CommandError("invalid microscope assignment value")
    return tuple(wanted[:5]) + (value,)


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


        replacement = transport(library) if needs_transport else None
        with lock:
            next_hook = hook if replacement is None else replacement[1]
            _local.context = (next_hook, library, wanted_generation)
            report = configure(library, wanted_generation, wanted_probes, replacement,
                               wanted_step, wanted_exceptions)
            assert _owner == library, "the transport's owner is not the library that configured it"
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



    if getattr(_local, 'forking', False):
        return
    lock.acquire()
    _local.forking = True


def forked_parent():
    if getattr(_local, 'forking', False):
        _local.forking = False
        lock.release()


def forked():


    lock._at_fork_reinit()
    _local.forking = _local.busy = False
    if _configured_in == _getpid():
        return
    try:
        clear()
    except Exception:
        pass


_register_at_fork(before=forking, after_in_parent=forked_parent, after_in_child=forked)



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
if FREE_THREADED and sys.version_info < (3, 14):
    raise RuntimeError("microscope cannot find running code safely: 3.13's free-threaded "
                       "collector stops listing code once a second thread starts")
if FREE_THREADED and not gc.is_tracked(matches.__code__):
    raise RuntimeError("microscope cannot find running code safely: this "
                       "free-threaded build's collector does not track code")


_microscope_loaded = 2
