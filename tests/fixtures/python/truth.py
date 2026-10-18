"""What a Python fixture knows about itself, printed for microscope's tests.

A fixture parks its worker threads with a Gate, then calls checkpoint(name).
The checkpoint prints TRUTH lines (tests/support/truth.rs reads them) from
CPython's own account of the process: every thread's native id, every frame's
qualified name, file, line, and instruction offset, and each function frame's
locals in canonical text. Then it prints READY and parks the calling thread
until a line arrives on its standard input, so that a test can stop the
process and compare what microscope reads with what the program said.

Every line is a tab-separated record; a field escapes a backslash, tab, or
newline as \\, \t, or \n:

    TRUTH  process     PID  VERSION  BUILD
    TRUTH  code        QUALNAME  FILE  FIRST-LINE  LINE,LINE,...
    TRUTH  checkpoint  NAME
    TRUTH  thread      NATIVE-ID  NAME  ROLE
    TRUTH  frame       NATIVE-ID  DEPTH  QUALNAME  FILE  LINE  OFFSET
    TRUTH  local       NATIVE-ID  DEPTH  NAME  VALUE
    TRUTH  evaluated   TEXT  RESULT

VERSION is major.minor.micro, BUILD is gil or free-threaded, ROLE is printer
for the thread that printed the checkpoint and parked for the others, DEPTH
counts from the innermost frame, LINE is - when CPython has none, and OFFSET
is the frame's last instruction's offset in bytes, f_lasti. VALUE is
canonical(), the text microscope must present the value as. RESULT is the
canonical() text of what CPython's eval() gave for the expression TEXT, or
! and the name of the exception's type when it raised one.

Only the standard library is used. Fixtures run with -I, so the environment
and the user's site-packages cannot change them.
"""

import os
import sys
import sysconfig
import threading
import time
import types
import _thread

# A code object's flag for a function's frame, whose locals are its own.
_CO_OPTIMIZED = 0x1

# The deepest canonical text nests, and the most items it shows of one
# container; fixtures keep their values within both.
_DEPTH = 6
_ITEMS = 64

# The flag of a class a program defines, as opposed to a type CPython or
# an extension does, and of a type an extension defines on the heap, whose
# attributes cannot be set, as no class's can.
_HEAPTYPE = 1 << 9
_IMMUTABLETYPE = 1 << 8

# The standard library's types that only C defines, which microscope reads
# itself (src/runtime_model/python/library.rs), by module and name.
_LIBRARY = {
    ("collections", "deque"),
    ("collections", "defaultdict"),
    ("collections", "OrderedDict"),
    ("datetime", "date"),
    ("datetime", "time"),
    ("datetime", "datetime"),
    ("datetime", "timedelta"),
    ("datetime", "timezone"),
    ("decimal", "Decimal"),
}

_print_lock = threading.Lock()


def _escape(text):
    return text.replace("\\", "\\\\").replace("\t", "\\t").replace("\n", "\\n")


def report(*fields):
    """Prints one TRUTH line of fields, each escaped."""
    line = "\t".join(["TRUTH", *(_escape(str(field)) for field in fields)])
    with _print_lock:
        sys.stdout.write(line + "\n")
        sys.stdout.flush()


def _string(text):
    # Python's repr, but for the characters it escapes: a backslash, the
    # quote, the C0 and C1 control characters, DEL, and lone surrogates,
    # where repr also escapes those its version's Unicode database says do
    # not print. It picks double quotes only for text that holds a single
    # quote and no double. src/runtime_model/python/text.rs is the same.
    quote = '"' if "'" in text and '"' not in text else "'"
    escaped = []
    for character in text:
        code = ord(character)
        if character in ("\\", quote):
            escaped.append("\\" + character)
        elif character == "\n":
            escaped.append("\\n")
        elif character == "\r":
            escaped.append("\\r")
        elif character == "\t":
            escaped.append("\\t")
        elif code < 0x20 or 0x7F <= code < 0xA0:
            escaped.append("\\x%02x" % code)
        elif 0xD800 <= code < 0xE000:
            escaped.append("\\u%04x" % code)
        else:
            escaped.append(character)
    return quote + "".join(escaped) + quote


def canonical(value, _depth=0, _seen=None):
    """The text microscope must present a value as.

    A value of a class a built-in view presents (views/python-std.views)
    is as that view presents it: _viewed() says how. A value of one of the
    standard library's types that only C defines is that type's own repr
    of it, as _library_text() gives it. A value of a built-in
    type is Python's repr of it for None, bools, ints at any size, floats
    (the shortest text that reads back as the same float), strings
    (escaping only what _string does), and bytes, and for lists, tuples,
    dicts (in insertion order), sets, and frozensets (in iteration order) of
    those; so is a value of a class deriving from one, as that type's own
    repr gives it. A class, a module, a function, a built-in function, and
    a code object show their names, and any other object its type's module
    and qualified name and its address.
    """
    seen = set() if _seen is None else _seen
    kind = type(value)
    base = _laid_out(kind)
    if value is None:
        return "None"
    viewed = _viewed(value, lambda item: canonical(item, _depth + 1, seen))
    if viewed is not None:
        return viewed
    if (base.__module__, base.__qualname__) in _LIBRARY and _defined_in_c(base):
        return _library_text(value, base, _depth, seen)
    if (base.__module__, base.__qualname__) == ("numpy", "ndarray") and _defined_in_c(base):
        return array_text(value)
    if base in (bool, int, float, bytes):
        return base.__repr__(value)
    if base is str:
        return _string(str.__str__(value))
    if isinstance(value, type):
        return "<class '%s'>" % _qualified(value)
    if base is types.ModuleType:
        name = object.__getattribute__(value, "__dict__").get("__name__")
        if type(name) is str:
            return "<module %s>" % _string(name)
        return "<module object at %#x>" % id(value)
    if kind is types.FunctionType:
        return "<function %s at %#x>" % (value.__qualname__, id(value))
    if kind in (types.BuiltinFunctionType, types.CodeType):
        return repr(value)
    if base not in (list, tuple, dict, set, frozenset):
        return "<%s object at %#x>" % (_qualified(kind), id(value))
    if _depth >= _DEPTH:
        raise ValueError("a fixture's value nests deeper than canonical() shows")
    if id(value) in seen:
        return {list: "[...]", dict: "{...}"}.get(base, "...")
    seen.add(id(value))
    try:
        inner = lambda item: canonical(item, _depth + 1, seen)  # noqa: E731
        if base is dict:
            items = list(dict.items(value))
        elif base in (list, tuple):
            items = list(base.__getitem__(value, slice(None)))
        else:
            items = list(base.__iter__(value))
        if len(items) > _ITEMS:
            raise ValueError("a fixture's container holds more than canonical() shows")
        if base is list:
            return "[" + ", ".join(map(inner, items)) + "]"
        if base is tuple:
            return "(" + ", ".join(map(inner, items)) + ("," if len(items) == 1 else "") + ")"
        if base is dict:
            return "{" + ", ".join(inner(k) + ": " + inner(v) for k, v in items) + "}"
        shown = ", ".join(map(inner, items))
        if base is set:
            return "{" + shown + "}" if items else "set()"
        return "frozenset({" + shown + "})" if items else "frozenset()"
    finally:
        seen.discard(id(value))


def _view_of(kind):
    """The built-in view that presents kind's instances, by the name of
    the class in its method resolution order whose view it is, as
    microscope chooses it: the first class a view's pattern names."""
    for klass in type.__getattribute__(kind, "__mro__"):
        module, name = klass.__module__, klass.__qualname__
        if (module, name) in (("enum", "Enum"), ("fractions", "Fraction")):
            return name
        if name == "PurePath" and (module == "pathlib" or module.startswith("pathlib.")):
            return name
    return None


def _viewed(value, inner):
    """The text a built-in view presents value as, or None where none
    presents it. inner gives the text of a value the view shows."""
    view = _view_of(type(value))
    held = lambda name: object.__getattribute__(value, name)  # noqa: E731
    if view == "Enum":
        return "{name: %s, value: %s}" % (inner(held("_name_")), inner(held("_value_")))
    if view == "Fraction":
        numerator, denominator = held("_numerator"), held("_denominator")
        if denominator == 1:
            return inner(numerator)
        return "%s/%s" % (inner(numerator), inner(denominator))
    if view == "PurePath":
        return inner(held("_str"))
    return None


def _library_text(value, base, depth, seen):
    """The text of value, of the library type base or of a class deriving
    from it, as base's own repr writes it, with the names of value's type
    its C uses: the name alone for a collection, and for a datetime type
    its tp_name, which a static type qualifies by its module. A Decimal's
    exponent is written E, as the default context writes it."""
    kind = type(value)
    short = kind.__name__
    full = short if kind.__flags__ & _HEAPTYPE else "%s.%s" % (kind.__module__, short)
    name = base.__qualname__
    get = lambda field: getattr(base, field).__get__(value, base)  # noqa: E731
    inner = lambda item: canonical(item, depth + 1, seen)  # noqa: E731
    if name in ("deque", "defaultdict", "OrderedDict"):
        if depth >= _DEPTH:
            raise ValueError("a fixture's value nests deeper than canonical() shows")
        if id(value) in seen:
            if name == "deque":
                return "[...]"
            if name == "defaultdict":
                factory = get("default_factory")
                return "%s(%s, {...})" % (short, "None" if factory is None else inner(factory))
            return "..."
        seen.add(id(value))
        try:
            return _collection_text(value, base, short, inner, get)
        finally:
            seen.discard(id(value))
    if name == "date":
        return "%s(%d, %d, %d)" % (full, get("year"), get("month"), get("day"))
    if name in ("time", "datetime"):
        clock = [get("hour"), get("minute"), get("second"), get("microsecond")]
        while len(clock) > 2 and not clock[-1]:
            clock.pop()
        arguments = ", ".join(map(str, clock))
        if name == "datetime":
            arguments = "%d, %d, %d, %s" % (get("year"), get("month"), get("day"), arguments)
        tzinfo = "" if get("tzinfo") is None else ", tzinfo=" + inner(get("tzinfo"))
        fold = ", fold=%d" % get("fold") if get("fold") else ""
        if name == "time":
            return "%s(%s%s%s)" % (full, arguments, tzinfo, fold)
        return "%s(%s%s%s)" % (full, arguments, fold, tzinfo)
    if name == "timedelta":
        parts = [(field, get(field)) for field in ("days", "seconds", "microseconds")]
        shown = ", ".join("%s=%d" % (field, part) for field, part in parts if part)
        return "%s(%s)" % (full, shown or "0")
    if name == "timezone":
        if value is base.utc:
            return full + ".utc"
        return "%s(%s)" % (full, ", ".join(map(inner, value.__getinitargs__())))
    decimal = sys.modules[base.__module__]
    with decimal.localcontext() as context:
        context.capitals = 1
        return "Decimal('%s')" % base.__str__(value)


# numpy's type numbers of the elements microscope reads, and their names,
# and the names of those it does not.
_ARRAY_ELEMENTS = {
    0: "bool", 1: "int8", 2: "uint8", 3: "int16", 4: "uint16", 5: "int32", 6: "uint32",
    7: "int64", 8: "uint64", 9: "int64", 10: "uint64", 11: "float32", 12: "float64",
}
_ARRAY_UNREAD = {
    13: "longdouble", 14: "complex64", 15: "complex128", 16: "clongdouble", 17: "object",
    18: "bytes", 19: "str", 20: "void", 21: "datetime64", 22: "timedelta64", 23: "float16",
}

# The most objects one summary reads, past which it shows `...`.
_OBJECTS = 512


def array_text(value):
    """The text of a numpy array: array(ELEMENTS, dtype=NAME), its
    elements as nested lists, each number as Python writes it, a float32
    with its own shortest digits; at most _ITEMS of each axis and _OBJECTS
    numbers, past which it shows `...`. An array of elements microscope
    does not read shows its shape and dtype alone."""
    numpy = sys.modules["numpy"]
    array = numpy.ndarray.view(value, numpy.ndarray)
    number = array.dtype.num
    if number not in _ARRAY_ELEMENTS or array.dtype.byteorder == ">":
        name = _ARRAY_ELEMENTS.get(number) or _ARRAY_UNREAD.get(number, "dtype %d" % number)
        if array.dtype.byteorder == ">":
            name += " (big-endian)"
        return "<numpy.ndarray of shape %r, dtype %s>" % (tuple(array.shape), name)
    counted = [1]

    def nested(part, depth):
        if part.ndim == 0:
            counted[0] += 1
            return _number_text(part[()])
        if depth >= _DEPTH:
            return "..."
        texts = []
        for index in range(min(len(part), _ITEMS)):
            if counted[0] >= _OBJECTS:
                texts.append("...")
                return "[" + ", ".join(texts) + "]"
            texts.append(nested(part[index], depth + 1))
        if len(part) > _ITEMS:
            texts.append("...")
        return "[" + ", ".join(texts) + "]"

    return "array(%s, dtype=%s)" % (nested(array, 0), _ARRAY_ELEMENTS[number])


def _number_text(number):
    """A numpy scalar as microscope writes an array's element."""
    numpy = sys.modules["numpy"]
    if isinstance(number, numpy.bool_):
        return "True" if number else "False"
    if isinstance(number, numpy.integer):
        return str(int(number))
    if isinstance(number, numpy.float32):
        return _float32_text(number)
    return repr(float(number))


def _float32_text(number):
    """A float32 as Python's repr writes a float, with the fewest digits
    that read back as the same float32."""
    numpy = sys.modules["numpy"]
    value = float(number)
    if value != value or value in (float("inf"), float("-inf")) or value == 0:
        return repr(value)
    for precision in range(1, 10):
        written = "%.*e" % (precision - 1, abs(value))
        with numpy.errstate(over="ignore"):
            if numpy.float32(float(written)) == abs(number):
                break
    mantissa, exponent = written.split("e")
    digits = mantissa.replace(".", "").rstrip("0") or "0"
    point = int(exponent) + 1
    sign = "-" if value < 0 else ""
    if point <= -4 or point > 16:
        fraction = "." + digits[1:] if len(digits) > 1 else ""
        return "%s%s%se%s%02d" % (sign, digits[0], fraction, "-" if point - 1 < 0 else "+",
                                  abs(point - 1))
    if point <= 0:
        return "%s0.%s%s" % (sign, "0" * -point, digits)
    if point < len(digits):
        return "%s%s.%s" % (sign, digits[:point], digits[point:])
    return "%s%s%s.0" % (sign, digits, "0" * (point - len(digits)))


def _collection_text(value, base, short, inner, get):
    """The text of a deque, defaultdict, or OrderedDict, value, of the type
    base or a class deriving from it: an OrderedDict's items in the order
    its own iteration gives, which is its list's, not its dict's."""
    name = base.__qualname__
    if name == "deque":
        items = list(base.__iter__(value))
    elif name == "OrderedDict":
        items = [(key, dict.__getitem__(value, key)) for key in base.__iter__(value)]
    else:
        items = list(dict.items(value))
    if len(items) > _ITEMS:
        raise ValueError("a fixture's container holds more than canonical() shows")
    if name == "deque":
        listed = "[" + ", ".join(map(inner, items)) + "]"
        maxlen = get("maxlen")
        if maxlen is None:
            return "%s(%s)" % (short, listed)
        return "%s(%s, maxlen=%d)" % (short, listed, maxlen)
    pairs = "{" + ", ".join(inner(key) + ": " + inner(item) for key, item in items) + "}"
    if name == "defaultdict":
        factory = get("default_factory")
        return "%s(%s, %s)" % (short, "None" if factory is None else inner(factory), pairs)
    return "%s(%s)" % (short, pairs) if items else short + "()"


def _defined_in_c(kind):
    """Whether C defines kind, as it does every type but a class."""
    return not kind.__flags__ & _HEAPTYPE or bool(kind.__flags__ & _IMMUTABLETYPE)


def _laid_out(kind):
    """The type kind's instances are laid out as: the first its bases lead
    to that C defines, which is the built-in or library type whose value
    they hold, if they hold one."""
    while not _defined_in_c(kind):
        kind = kind.__base__
    return kind


def _qualified(kind):
    """A type's name as Python's repr gives it: qualified by its module,
    unless that is builtins."""
    if kind.__module__ == "builtins":
        return kind.__qualname__
    return "%s.%s" % (kind.__module__, kind.__qualname__)


def _mangle(klass, name):
    # The name CPython keeps a private slot under, as it mangles a private
    # name in a class body.
    owner = klass.__name__.lstrip("_")
    if name.startswith("__") and not name.endswith("__") and owner:
        return "_%s%s" % (owner, name)
    return name


def _dataclass_fields(kind):
    """The names of the fields of the dataclass kind is, in the order it
    defines them, or None where it is none."""
    for klass in kind.__mro__:
        fields = type.__getattribute__(klass, "__dict__").get("__dataclass_fields__")
        if fields is not None:
            return [name for name in fields if type(name) is str] if type(fields) is dict else None
    return None


def attributes(value):
    """An object's attributes, each a name and its value, as microscope
    shows them: those its dict holds, in insertion order, then those its
    slots hold, each class's from the most derived on, in the sorted order
    CPython keeps a class's slots in; but a dataclass's fields first, in
    the order it defines them. A slot is read by its own class's
    descriptor, and one whose name a more derived class's slot takes is
    left out, whatever either holds."""
    try:
        found = list(object.__getattribute__(value, "__dict__").items())
    except AttributeError:
        found = []
    taken = set()
    for klass in type(value).__mro__:
        if not klass.__flags__ & _HEAPTYPE:
            continue
        slots = klass.__dict__.get("__slots__", ())
        names = (slots,) if isinstance(slots, str) else slots
        for slot in sorted(_mangle(klass, name) for name in names):
            if slot in ("__dict__", "__weakref__") or slot in taken:
                continue
            taken.add(slot)
            descriptor = klass.__dict__.get(slot)
            if type(descriptor) is not types.MemberDescriptorType:
                continue
            try:
                found.append((slot, descriptor.__get__(value, klass)))
            except AttributeError:
                pass
    fields = _dataclass_fields(type(value))
    if fields is not None:
        found.sort(key=lambda item: fields.index(item[0]) if item[0] in fields else len(fields))
    return found


def _codes(code):
    yield code
    for constant in code.co_consts:
        if hasattr(constant, "co_code"):
            yield from _codes(constant)


def code_table():
    """Prints the lines of every code object compiled from the main script
    and from this module, whose frames park the fixture's threads.

    Every line microscope reports for a code object must be one CPython
    gives it. Compiling a file again gives the same code objects without
    racing the threads that run them.
    """
    for path in (sys.modules["__main__"].__file__, __file__):
        with open(path, encoding="utf-8") as source:
            compiled = compile(source.read(), path, "exec")
        for code in _codes(compiled):
            lines = sorted({line for _, _, line in code.co_lines() if line is not None})
            report("code", code.co_qualname, code.co_filename, code.co_firstlineno,
                   ",".join(map(str, lines)))


def _process():
    version = "%d.%d.%d" % sys.version_info[:3]
    build = "free-threaded" if sysconfig.get_config_var("Py_GIL_DISABLED") else "gil"
    report("process", os.getpid(), version, build)


class Gate:
    """Where worker threads park until the process ends.

    Each worker calls arrive() at its marker and parks, blocked in C; the
    thread that checks the process calls wait() until every worker's stack is
    still, so that it stays so while it is printed and while microscope reads
    it. A worker that has only said it arrived is still running: in a
    free-threaded build, its stack can change as it is printed.
    """

    def __init__(self, count):
        self._count = count
        self._parked = []
        self._released = False
        self._condition = threading.Condition()

    def arrive(self):
        lock = _thread.allocate_lock()
        lock.acquire()
        with self._condition:
            if self._released:
                return
            self._parked.append((threading.get_ident(), lock))
            self._condition.notify_all()
        _park(lock)

    def wait(self):
        with self._condition:
            while len(self._parked) < self._count:
                self._condition.wait()
            workers = [ident for ident, _ in self._parked]
        # A worker in _park at its one line shows that line whether or not
        # it has blocked yet, and keeps showing it until it is released.
        deadline = time.monotonic() + 30
        while not all(_parked(frame) for ident, frame in sys._current_frames().items()
                      if ident in workers):
            if time.monotonic() > deadline:
                raise RuntimeError("a gate's worker did not park")
            time.sleep(0.001)

    def release(self):
        with self._condition:
            self._released = True
            for _, lock in self._parked:
                lock.release()


def _park(lock):
    lock.acquire()


def _parked(frame):
    return (frame.f_code is _park.__code__
            and frame.f_lineno == _park.__code__.co_firstlineno + 1)


def _frames(frame):
    depth = 0
    while frame is not None:
        yield depth, frame
        frame = frame.f_back
        depth += 1


def _print_thread(native_id, name, role, frame):
    report("thread", native_id, name, role)
    for depth, current in _frames(frame):
        code = current.f_code
        line = current.f_lineno
        report("frame", native_id, depth, code.co_qualname, code.co_filename,
               "-" if line is None else line, current.f_lasti)
        if code.co_flags & _CO_OPTIMIZED:
            for local, value in current.f_locals.items():
                report("local", native_id, depth, local, canonical(value))


def evaluated(expressions, frame):
    """Each expression and what CPython's eval() gives for it in frame, as
    checkpoint() reports it."""
    results = []
    for text in expressions:
        try:
            result = canonical(eval(text, frame.f_globals, frame.f_locals))
        except Exception as error:  # noqa: BLE001
            result = "!" + type(error).__name__
        results.append(("evaluated", text, result))
    return results


def checkpoint(name, gate=None, abort=False, records=()):
    """Prints every thread's frames and locals, then records, each a
    tuple of a line's fields; then parks until a line arrives on standard
    input, or parks for good at its end; or, to leave a core dump of the
    process as it reported it, aborts."""
    if gate is not None:
        gate.wait()
    printer = threading.current_thread()
    report("checkpoint", name)
    frames = sys._current_frames()
    # The caller's frame, not this function's: the printer's own frames,
    # which park below, are not compared.
    _print_thread(printer.native_id, printer.name, "printer", sys._getframe(1))
    for thread in sorted(threading.enumerate(), key=lambda thread: thread.native_id or 0):
        if thread is printer or thread.ident not in frames:
            continue
        _print_thread(thread.native_id, thread.name, "parked", frames[thread.ident])
    del frames
    for record in records:
        report(*record)
    with _print_lock:
        sys.stdout.write("READY\n")
        sys.stdout.flush()
    if abort:
        os.abort()
    park()


def park():
    """Waits for a line on standard input, or forever when it ends."""
    if not sys.stdin.readline():
        threading.Event().wait()


_process()
