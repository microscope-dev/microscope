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

# A code object's flag for a function's frame, whose locals are its own.
_CO_OPTIMIZED = 0x1

# The deepest canonical text nests, and the most items it shows of one
# container; fixtures keep their values within both.
_DEPTH = 6
_ITEMS = 64

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

    It is Python's repr for None, bools, ints at any size, floats (the
    shortest text that reads back as the same float), strings (escaping
    only what _string does), and bytes,
    and for lists, tuples, and dicts (in insertion order) of those. Sets are
    sorted by their items' text, so that every interpreter agrees. Any other
    object is its type's module and qualified name and, for a type the
    fixture defines, its attributes in the order they were set, then those
    its slots hold.
    """
    seen = set() if _seen is None else _seen
    if value is None or isinstance(value, (bool, int, float, bytes)):
        return repr(value)
    if isinstance(value, str):
        return _string(value)
    if _depth >= _DEPTH:
        raise ValueError("a fixture's value nests deeper than canonical() shows")
    if id(value) in seen:
        return {list: "[...]", dict: "{...}"}.get(type(value), "...")
    seen.add(id(value))
    try:
        inner = lambda item: canonical(item, _depth + 1, seen)  # noqa: E731
        if isinstance(value, (list, tuple, dict, set, frozenset)) and len(value) > _ITEMS:
            raise ValueError("a fixture's container holds more than canonical() shows")
        if type(value) is list:
            return "[" + ", ".join(map(inner, value)) + "]"
        if type(value) is tuple:
            items = list(map(inner, value))
            return "(" + ", ".join(items) + ("," if len(items) == 1 else "") + ")"
        if type(value) is dict:
            return "{" + ", ".join(inner(k) + ": " + inner(v) for k, v in value.items()) + "}"
        if type(value) in (set, frozenset):
            items = sorted(map(inner, value))
            if type(value) is set:
                return "{" + ", ".join(items) + "}" if items else "set()"
            return "frozenset({" + ", ".join(items) + "})" if items else "frozenset()"
        kind = type(value)
        if kind.__module__ != "__main__":
            return "<%s.%s>" % (kind.__module__, kind.__qualname__)
        attributes = list(getattr(value, "__dict__", {}).items())
        for klass in kind.__mro__:
            slots = klass.__dict__.get("__slots__", ())
            for slot in (slots,) if isinstance(slots, str) else slots:
                if slot not in ("__dict__", "__weakref__") and hasattr(value, slot):
                    attributes.append((slot, getattr(value, slot)))
        shown = "".join(" %s=%s" % (name, inner(item)) for name, item in attributes)
        return "<%s.%s%s>" % (kind.__module__, kind.__qualname__, shown)
    finally:
        seen.discard(id(value))


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

    Each worker calls arrive() at its marker; the thread that checks the
    process calls wait() until every worker has, so that every stack is still
    while it is printed and while microscope reads it.
    """

    def __init__(self, count):
        self._count = count
        self._arrived = 0
        self._condition = threading.Condition()
        self._release = threading.Event()

    def arrive(self):
        with self._condition:
            self._arrived += 1
            self._condition.notify_all()
        self._release.wait()

    def wait(self):
        with self._condition:
            while self._arrived < self._count:
                self._condition.wait()

    def release(self):
        self._release.set()


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
