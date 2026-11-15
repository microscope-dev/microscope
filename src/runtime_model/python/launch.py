

















import os as _os
import sys as _sys


if _os.environ.get('MICROSCOPE_PYTHON_FOLLOW') == '1':
    _microscope_wake = _os.environ.get('MICROSCOPE_PYTHON_WAKE')
else:
    _microscope_wake = _os.environ.pop('MICROSCOPE_PYTHON_WAKE', None)

    _microscope_path = _os.environ.pop('MICROSCOPE_PYTHON_PATH', None)
    if _microscope_path == '0':
        _os.environ.pop('PYTHONPATH', None)
    elif _microscope_path is not None and _microscope_path.startswith('1'):
        _os.environ['PYTHONPATH'] = _microscope_path[1:]
_microscope_here = _os.path.dirname(_os.path.abspath(__file__))
_sys.path[:] = [_entry for _entry in _sys.path
                if _os.path.abspath(_entry or _os.curdir) != _microscope_here]


def _microscope_bootstrap(wake, here):
    import os
    import sys
    import _thread

    if wake is None or os.path.dirname(wake) != here or sys.version_info[:2] not in (
            (3, 12), (3, 13)):
        return

    def own_channel():


        try:
            with open(os.path.join(here, 'pid'), 'rb') as file:
                launched = int(file.read(32))
        except (OSError, ValueError):
            launched = None
        path = wake if launched == os.getpid() else os.path.join(here, f'wake-{os.getpid()}')
        try:
            return os.open(path, os.O_RDONLY | os.O_NONBLOCK | os.O_CLOEXEC)
        except OSError:
            return None


    module = type(sys)('_microscope_bootstrap')
    sys.modules['_microscope_bootstrap'] = module

    state = {'channel': None, 'pending': b''}

    def run(path):

        try:
            with open(path, 'rb') as file:
                source = file.read()
            exec(compile(source, path, 'exec'), {'__name__': '__microscope__'})
        except BaseException:
            pass

    def take(data):
        state['pending'] += data
        *lines, state['pending'] = state['pending'].split(b'\n')
        for line in lines:
            if line:
                run(os.fsdecode(line))

    def serve(channel):

        module.main = _thread.get_ident()
        state['channel'], state['pending'] = channel, b''
        try:
            while data := os.read(channel, 4096):
                take(data)
            return
        except BlockingIOError:
            pass
        os.set_blocking(channel, True)
        _thread.start_new_thread(waiting, (channel,))

    def waiting(channel):
        try:

            with open(f'/proc/self/task/{_thread.get_native_id()}/comm', 'w') as file:
                file.write('microscope')
        except OSError:
            pass
        while True:
            try:
                data = os.read(channel, 4096)
            except InterruptedError:
                continue
            except OSError:
                return
            if not data:
                return
            take(data)

    def forked():


        if state['channel'] is not None:
            os.close(state['channel'])
            state['channel'] = None
        channel = own_channel()
        if channel is not None:
            serve(channel)

    os.register_at_fork(after_in_child=forked)
    channel = own_channel()
    if channel is not None:
        serve(channel)


try:
    _microscope_bootstrap(_microscope_wake, _microscope_here)
except Exception:
    pass


del _sys.modules['sitecustomize']
try:
    import sitecustomize  # noqa: E402, F401
except ImportError as _error:
    if _error.name != 'sitecustomize':
        raise
