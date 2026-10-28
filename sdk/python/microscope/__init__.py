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
microscope never enables it. A traced function whose site is off costs
nothing once it has run; recorded, a call costs well under a microsecond.
"""
import ctypes
import os
import sys
import threading
import time

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
        # Every call is short and never blocks, so it keeps the GIL, which
        # halves what a call costs.
        library = ctypes.PyDLL(path)
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
        'microscope_probes_reserve': (None, (ctypes.c_uint32,)),
        'microscope_probes_index': (ctypes.c_int32, (site,)),
        'microscope_probes_sites': (site, ()),
        # Called with a small int, which converts fastest.
        'microscope_probes_call_begin': (None, None),
        'microscope_probes_call_end': (None, None),
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

# The ABI's run-time sites: how many, and the size of each one's record.
_RUNTIME_SITES, _RUNTIME_SITE = 4096, 48


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

# The least size of a ring, as a power of two of records, once calls are
# traced.
_TRACED_RING = 18


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
        # A thread that traces its calls writes millions of records a
        # second, which a ring of 2^18 holds for about 40 ms.
        _library.microscope_probes_reserve(_TRACED_RING)
        _traced = _Tracer(monitoring)
        _traced.prefixes = (prefix,)
    else:
        _traced.add(prefix)


class _Tracer:
    """Assigns zone sites to traced code objects. Untraced code's events,
    and a traced site's while it is off, are disabled where they fire, so
    they cost nothing until a thread that checks every so often finds a
    site switched on and restarts them."""

    def __init__(self, monitoring):
        self.monitoring = monitoring
        self.prefixes = ()
        # By the code's id, as equal code objects may belong to different
        # files: (site, state), or (None, None) for untraced code. `codes`
        # keeps each code object, so that its id stays its.
        self.sites = {}
        self.codes = []
        # Where in `states` lie the state bytes of traced sites that were off
        # where their events were disabled.
        self.waiting = []
        # Every run-time site's record, whose state bytes the callbacks read.
        records = ctypes.c_ubyte * (_RUNTIME_SITES * _RUNTIME_SITE)
        self.states = memoryview(records.from_address(_library.microscope_probes_sites())).cast('B')
        tool = monitoring.PROFILER_ID
        if monitoring.get_tool(tool) is not None:
            raise RuntimeError(f'the profiler tool is taken by {monitoring.get_tool(tool)}')
        monitoring.use_tool_id(tool, 'microscope')
        events = monitoring.events
        begin, end, thrown, unwind = self.callbacks()
        for event in (events.PY_START, events.PY_RESUME):
            monitoring.register_callback(tool, event, begin)
        for event in (events.PY_RETURN, events.PY_YIELD):
            monitoring.register_callback(tool, event, end)
        monitoring.register_callback(tool, events.PY_THROW, thrown)
        monitoring.register_callback(tool, events.PY_UNWIND, unwind)
        monitoring.set_events(tool, events.PY_START | events.PY_RESUME | events.PY_RETURN
                              | events.PY_YIELD | events.PY_THROW | events.PY_UNWIND)
        self.watch()
        os.register_at_fork(after_in_child=self.watch)

    def callbacks(self):
        """The callbacks, which run at every call and return of traced code,
        and so read only what they close over."""
        get, entry, waiting, states = self.sites.get, self.entry, self.waiting, self.states
        disable = self.monitoring.DISABLE
        begin_call = _library.microscope_probes_call_begin
        end_call = _library.microscope_probes_call_end

        def begin(code, offset):
            index, state = get(id(code)) or entry(code)
            if index < 0:
                return disable
            if states[state]:
                begin_call(index)
                return None
            waiting.append(state)
            return disable

        def end(code, offset, value):
            index, state = get(id(code)) or entry(code)
            if index < 0:
                return disable
            if states[state]:
                end_call(index)
                return None
            waiting.append(state)
            return disable

        # A generator thrown into resumes, and a frame an exception leaves
        # returns; neither event can be disabled.
        def thrown(code, offset, exception):
            index, state = get(id(code)) or entry(code)
            if index >= 0 and states[state]:
                begin_call(index)

        def unwind(code, offset, exception):
            index, state = get(id(code)) or entry(code)
            if index >= 0 and states[state]:
                end_call(index)

        return begin, end, thrown, unwind

    def add(self, prefix):
        """Adds `prefix`, reenabling events for previously seen, untraced code."""
        with _defining:
            self.prefixes = (*self.prefixes, prefix)
            for key, (index, _) in list(self.sites.items()):
                if index < 0:
                    del self.sites[key]
        self.monitoring.restart_events()

    def entry(self, code):
        """The code's site's index and where its state byte lies in
        `states`, which the callbacks hold to; -1 for untraced code."""
        module = sys._getframe(2).f_globals.get('__name__')
        traced = isinstance(module, str) and module != __name__ and any(
            module == prefix or module.startswith(prefix + '.') for prefix in self.prefixes)
        site, _ = _site(_ZONE, f'{module}.{code.co_qualname}', code.co_filename,
                        code.co_firstlineno) if traced else _NO_SITE
        index = _library.microscope_probes_index(site) if site else -1
        found = (index, index * _RUNTIME_SITE + 1) if index >= 0 else (-1, 0)
        self.codes.append(code)
        self.sites[id(code)] = found
        return found

    def watch(self):
        """Checks every 20 ms whether a site whose events were disabled
        while it was off is on, and restarts them if so."""
        waiting, states = self.waiting, self.states
        restart = self.monitoring.restart_events

        def check():
            while True:
                time.sleep(0.02)
                if waiting and any(map(states.__getitem__, waiting)):
                    waiting.clear()
                    restart()

        threading.Thread(target=check, name='microscope trace_calls', daemon=True).start()
