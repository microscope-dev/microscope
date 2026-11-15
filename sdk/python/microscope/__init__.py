




























import ctypes
import os
import sys
import threading
import time

__all__ = ['zone', 'label', 'plot', 'message', 'frame', 'trigger', 'listening', 'found',
           'trace_calls', 'problem']

_ZONE, _FRAME, _PLOT, _MESSAGE, _LABEL, _TRIGGER = 1, 2, 3, 4, 5, 6

_SUSPENDS = 0x20 | 0x80 | 0x200


problem = None




_CHECKED = os.environ.get('MICROSCOPE_SDK_CHECKS') == '1'


def _load():
    global problem
    path = os.environ.get('MICROSCOPE_PROBES_LIBRARY') or os.path.join(
        os.path.dirname(os.path.abspath(__file__)), 'libmicroscope_probes.so')
    try:


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
        'microscope_probes_found': (ctypes.c_bool, ()),
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


_RUNTIME_SITES, _RUNTIME_SITE = 4096, 48


def _site(kind, name, file, line):


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

            site = (address, ctypes.c_ubyte.from_address(address + 1)) if address else _NO_SITE
            _sites[key] = site
    assert not _CHECKED or (site[1] is None) == (not site[0]), f'site {site[0]:#x} state'
    return site


def _caller(depth=2):

    frame = sys._getframe(depth)
    return frame.f_code.co_filename, frame.f_lineno


def listening():

    return _library is not None and _library.microscope_probes_listening()


def found():


    return _library is not None and _library.microscope_probes_found()


class zone:



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

    site, state = _site(_PLOT, name, *_caller())
    if state is not None and state.value != 0:
        _library.microscope_probes_plot(site, float(value))


def message(name, text):


    site, state = _site(_MESSAGE, name, *_caller())
    if state is not None and state.value != 0:
        if callable(text):
            text = text()
        encoded = str(text).encode('utf-8', 'replace')
        _library.microscope_probes_message(site, encoded, len(encoded))


def frame(name='frame'):

    site, state = _site(_FRAME, name, *_caller())
    if state is not None and state.value != 0:
        _library.microscope_probes_event(site)


def trigger(why):


    site, state = _site(_TRIGGER, why, *_caller())
    if state is not None and state.value != 0:
        _library.microscope_probes_event(site)


_traced = None



_TRACED_RING = 18


def trace_calls(prefix):




    global _traced
    if not isinstance(prefix, str):
        raise TypeError(f'a prefix names a module, not {prefix!r}')
    monitoring = getattr(sys, 'monitoring', None)
    if monitoring is None:
        raise RuntimeError('trace_calls needs Python 3.12 or later')
    if _library is None:
        return
    if _traced is None:


        _library.microscope_probes_reserve(_TRACED_RING)
        _traced = _Tracer(monitoring)
        _traced.prefixes = (prefix,)
    else:
        _traced.add(prefix)


class _Tracer:





    def __init__(self, monitoring):
        self.monitoring = monitoring
        self.prefixes = ()



        self.sites = {}
        self.codes = []



        self.waiting = []
        self.ready = threading.Event()

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


        get, entry, waiting, states = self.sites.get, self.entry, self.waiting, self.states
        ready = self.ready.set
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
            ready()
            return disable

        def end(code, offset, value):
            index, state = get(id(code)) or entry(code)
            if index < 0:
                return disable
            if states[state]:
                end_call(index)
                return None
            waiting.append(state)
            ready()
            return disable



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

        with _defining:
            self.prefixes = (*self.prefixes, prefix)
            for key, (index, _) in list(self.sites.items()):
                if index < 0:
                    del self.sites[key]
        self.monitoring.restart_events()

    def entry(self, code):


        module = sys._getframe(2).f_globals.get('__name__')
        traced = isinstance(module, str) and module != __name__ and any(
            module == prefix or module.startswith(prefix + '.') for prefix in self.prefixes)
        site, _ = _site(_ZONE, f'{module}.{code.co_qualname}', code.co_filename,
                        code.co_firstlineno) if traced else _NO_SITE
        index = _library.microscope_probes_index(site) if site else -1
        assert not _CHECKED or index < _RUNTIME_SITES, f'site index {index} of {_RUNTIME_SITES}'
        found = (index, index * _RUNTIME_SITE + 1) if index >= 0 else (-1, 0)
        assert not _CHECKED or found[1] < len(self.states), f'state byte {found[1]}'
        self.codes.append(code)
        self.sites[id(code)] = found
        assert not _CHECKED or len(self.sites) <= len(self.codes), 'a traced site lost its code'
        return found

    def watch(self):





        waiting, states, ready = self.waiting, self.states, self.ready
        restart = self.monitoring.restart_events
        listening = _library.microscope_probes_listening

        def check():
            _name_thread()
            while True:
                if not waiting:


                    ready.wait()
                    ready.clear()
                    continue
                time.sleep(0.02 if listening() else 1.0)
                if any(map(states.__getitem__, waiting)):
                    waiting.clear()
                    restart()

        threading.Thread(target=check, name=_THREAD_NAME, daemon=True).start()




_THREAD_NAME = 'microscope-sdk'


def _name_thread():


    try:
        ctypes.CDLL(None).prctl(15, _THREAD_NAME.encode(), 0, 0, 0)
    except (AttributeError, OSError):
        pass
