"""Python instrumentation for microscope: zones, plots, messages, labels, frames, and triggers.

    import microscope

    @microscope.zone("handle")
    def handle(request):
        with microscope.label("route", request.route):
            with microscope.zone("query"):
                rows = query(request)
            microscope.plot("rows", len(rows))
            if not rows:
                microscope.message("empty", f"no rows for {request.id}")
            return render(rows)

Sites are defined at their first use, with a name and source location.
A recording enables sites by name; disabled sites check an in-place state byte.
Enabled sites write a few bytes to a per-thread ring in the program's memory,
which microscope reads externally. Sites disable themselves when microscope
stops reading. Without a recording, nothing is written or sent elsewhere.

The probes SDK's shared library must be beside this package or at
MICROSCOPE_PROBES_LIBRARY. Without it, instrumentation does nothing;
`problem` explains why.

`trace_calls(prefix)` times calls in that module and its submodules, with
zones named by module and qualified name. Only the program can opt in;
microscope never enables it.
"""
import ctypes
import os
import sys
import threading

__all__ = ['zone', 'label', 'plot', 'message', 'frame', 'trigger', 'listening',
           'trace_calls', 'problem']

_ZONE, _FRAME, _PLOT, _MESSAGE, _LABEL, _TRIGGER = 1, 2, 3, 4, 5, 6
# A generator's, coroutine's, or async generator's code.
_SUSPENDS = 0x20 | 0x80 | 0x200

# Why the library could not be used, or None.
problem = None


def _load():
    global problem
    path = os.environ.get('MICROSCOPE_PROBES_LIBRARY') or os.path.join(
        os.path.dirname(os.path.abspath(__file__)), 'libmicroscope_probes.so')
    try:
        library = ctypes.CDLL(path)
    except OSError as error:
        problem = f'the probes library could not be loaded: {error}'
        return None
    site = ctypes.c_void_p
    text = (ctypes.c_char_p, ctypes.c_size_t)
    try:
        abi = library.microscope_probes_abi
    except AttributeError as error:
        problem = f'the probes library at {path} is not one this package uses: {error}'
        return None
    abi.restype = ctypes.c_uint16
    abi.argtypes = ()
    if abi() < 2:
        problem = f'the probes library at {path} has no run-time sites'
        return None
    signatures = {
        'microscope_probes_listening': (ctypes.c_bool, ()),
        'microscope_probes_define': (site, (ctypes.c_uint8, *text, *text, ctypes.c_uint32)),
        'microscope_probes_on': (ctypes.c_bool, (site,)),
        'microscope_probes_zone_begin': (ctypes.c_bool, (site,)),
        'microscope_probes_zone_end': (None, (site,)),
        'microscope_probes_label_begin': (ctypes.c_bool, (site, *text)),
        'microscope_probes_label_end': (None, (site,)),
        'microscope_probes_event': (None, (site,)),
        'microscope_probes_plot': (None, (site, ctypes.c_double)),
        'microscope_probes_message': (None, (site, *text)),
    }
    try:
        for name, (result, arguments) in signatures.items():
            function = getattr(library, name)
            function.restype = result
            function.argtypes = arguments
    except AttributeError as error:
        problem = f'the probes library at {path} is not one this package uses: {error}'
        return None
    return library


_library = _load()
_sites = {}
_defining = threading.Lock()


_NO_SITE = (0, None)


def _site(kind, name, file, line):
    """Returns (site, state byte) for this kind/name/location, or (0, None) if unavailable.
    A nonzero state byte means enabled."""
    key = (kind, name, file, line)
    site = _sites.get(key)
    if site is not None or _library is None:
        return site or _NO_SITE
    with _defining:
        site = _sites.get(key)
        if site is None:
            name_bytes = str(name).encode('utf-8', 'replace')
            file_bytes = str(file).encode('utf-8', 'replace')
            address = _library.microscope_probes_define(
                kind, name_bytes, len(name_bytes), file_bytes, len(file_bytes), line)
            # The ABI puts a run-time site's state byte at its offset 1.
            site = (address, ctypes.c_ubyte.from_address(address + 1)) if address else _NO_SITE
            _sites[key] = site
    return site


def _caller(depth=2):
    """The file and line of the code `depth` frames out."""
    frame = sys._getframe(depth)
    return frame.f_code.co_filename, frame.f_lineno


def listening():
    """Whether microscope is recording the program's instrumentation."""
    return _library is not None and _library.microscope_probes_listening()


class zone:
    """A span of the thread's time named `name`: a `with` block, or each call
    of a function it decorates."""

    __slots__ = ('_site', '_state', '_began')

    def __init__(self, name):
        self._site, self._state = _site(_ZONE, name, *_caller())
        self._began = []

    def __enter__(self):
        state = self._state
        self._began.append(state is not None and state.value != 0
                           and _library.microscope_probes_zone_begin(self._site))
        return self

    def __exit__(self, *exception):
        if self._began.pop():
            _library.microscope_probes_zone_end(self._site)
        return False

    def __call__(self, function):
        flags = getattr(getattr(function, '__code__', None), 'co_flags', 0)
        if flags & _SUSPENDS:
            # Other tasks run on the thread while it waits, inside its zone.
            raise TypeError('a zone is a span of one thread, and cannot span what a '
                            'coroutine or generator awaits or yields')
        site, state = self._site, self._state
        if not site:
            return function

        def zoned(*arguments, **keywords):
            began = state.value != 0 and _library.microscope_probes_zone_begin(site)
            try:
                return function(*arguments, **keywords)
            finally:
                if began:
                    _library.microscope_probes_zone_end(site)

        for attribute in ('__module__', '__name__', '__qualname__', '__doc__'):
            try:
                setattr(zoned, attribute, getattr(function, attribute))
            except AttributeError:
                pass
        zoned.__wrapped__ = function
        return zoned


class label:
    """Labels the thread's time `name=value` for a `with` block: samples and
    zones inside it carry the label. The value is cut to 256 bytes."""

    __slots__ = ('_site', '_state', '_value', '_began')

    def __init__(self, name, value):
        self._site, self._state = _site(_LABEL, name, *_caller())
        self._value = value
        self._began = False

    def __enter__(self):
        state = self._state
        if state is not None and state.value != 0:
            text = str(self._value).encode('utf-8', 'replace')
            self._began = _library.microscope_probes_label_begin(self._site, text, len(text))
        return self

    def __exit__(self, *exception):
        if self._began:
            self._began = False
            _library.microscope_probes_label_end(self._site)
        return False


def plot(name, value):
    """A value of the series `name`."""
    site, state = _site(_PLOT, name, *_caller())
    if state is not None and state.value != 0:
        _library.microscope_probes_plot(site, float(value))


def message(name, text):
    """Records a thread-timeline message at `name`, truncated to 256 bytes.
    Pass a callable to defer text creation until the site is enabled."""
    site, state = _site(_MESSAGE, name, *_caller())
    if state is not None and state.value != 0:
        if callable(text):
            text = text()
        encoded = str(text).encode('utf-8', 'replace')
        _library.microscope_probes_message(site, encoded, len(encoded))


def frame(name='frame'):
    """The end of a frame, for a program that works in frames."""
    site, state = _site(_FRAME, name, *_caller())
    if state is not None and state.value != 0:
        _library.microscope_probes_event(site)


def trigger(why):
    """Asks microscope to save the recording's window around this moment,
    when it keeps one."""
    site, state = _site(_TRIGGER, why, *_caller())
    if state is not None and state.value != 0:
        _library.microscope_probes_event(site)


_traced = None


def trace_calls(prefix):
    """Times future calls in `prefix` and its submodules as module/qualified-name zones.
    Generator and coroutine zones end at suspension and restart at resumption.
    Requires Python 3.12+ monitoring and claims its profiler tool; repeated calls add prefixes.
    Without the probes library it does nothing."""
    global _traced
    if not isinstance(prefix, str):
        raise TypeError(f'a prefix names a module, not {prefix!r}')
    monitoring = getattr(sys, 'monitoring', None)
    if monitoring is None:
        raise RuntimeError('trace_calls needs Python 3.12 or later')
    if _library is None:
        return
    if _traced is None:
        _traced = _Tracer(monitoring)
        _traced.prefixes = (prefix,)
    else:
        _traced.add(prefix)


class _Tracer:
    """Assigns zone sites to traced code objects; disables untraced code's entry events."""

    def __init__(self, monitoring):
        self.monitoring = monitoring
        self.prefixes = ()
        # Key by identity: equal code objects may belong to different files.
        # Retain (code, site, state); site 0 means untraced.
        self.sites = {}
        self.stacks = threading.local()
        tool = monitoring.PROFILER_ID
        if monitoring.get_tool(tool) is not None:
            raise RuntimeError(f'the profiler tool is taken by {monitoring.get_tool(tool)}')
        monitoring.use_tool_id(tool, 'microscope')
        events = monitoring.events
        self.disable = monitoring.DISABLE
        for event in (events.PY_START, events.PY_RESUME):
            monitoring.register_callback(tool, event, self.begin)
        for event in (events.PY_RETURN, events.PY_YIELD):
            monitoring.register_callback(tool, event, self.end)
        monitoring.register_callback(tool, events.PY_THROW, self.thrown)
        monitoring.register_callback(tool, events.PY_UNWIND, self.unwind)
        monitoring.set_events(tool, events.PY_START | events.PY_RESUME | events.PY_RETURN
                              | events.PY_YIELD | events.PY_THROW | events.PY_UNWIND)

    def add(self, prefix):
        """Adds `prefix`, reenabling events for previously seen, untraced code."""
        with _defining:
            self.prefixes = (*self.prefixes, prefix)
            self.sites = {key: entry for key, entry in self.sites.items() if entry[1]}
        self.monitoring.restart_events()

    def entry(self, code):
        """The code's (code, site, state)."""
        entry = self.sites.get(id(code))
        if entry is not None:
            return entry
        module = sys._getframe(2).f_globals.get('__name__')
        traced = isinstance(module, str) and module != __name__ and any(
            module == prefix or module.startswith(prefix + '.') for prefix in self.prefixes)
        site, state = _site(_ZONE, f'{module}.{code.co_qualname}', code.co_filename,
                            code.co_firstlineno) if traced else _NO_SITE
        entry = self.sites[id(code)] = (code, site, state)
        return entry

    def stack(self):
        stack = getattr(self.stacks, 'calls', None)
        if stack is None:
            stack = self.stacks.calls = []
        return stack

    def begin(self, code, offset):
        _, site, state = self.entry(code)
        if not site:
            return self.disable
        self.stack().append(
            (code, state.value != 0 and _library.microscope_probes_zone_begin(site)))

    def thrown(self, code, offset, exception):
        # A generator thrown into resumes; nothing can be disabled here.
        if self.entry(code)[1]:
            self.begin(code, offset)

    def end(self, code, offset, value):
        if not self.entry(code)[1]:
            return self.disable
        self.finish(code)

    def unwind(self, code, offset, exception):
        if self.entry(code)[1]:
            self.finish(code)

    def finish(self, code):
        # A call that began before tracing did has no zone to end.
        stack = self.stack()
        if stack and stack[-1][0] is code:
            _, began = stack.pop()
            if began:
                _library.microscope_probes_zone_end(self.sites[id(code)][1])
