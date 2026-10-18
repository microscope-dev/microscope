"""microscope's launch bootstrap for CPython 3.12 and 3.13, which cannot be
asked to run code as 3.14's PEP 768 lets a debugger ask.

microscope puts this file's directory first on PYTHONPATH as
`sitecustomize.py`, names the FIFO beside it in MICROSCOPE_PYTHON_WAKE, and
writes the process ID it launched to `pid` beside it. In that process, the
bootstrap runs each script whose path microscope writes to the FIFO, as
PEP 768 runs a requested script: on the main thread before the program's
code, those already written, and later on one thread started through
`_thread`, which `threading` does not list and which waits on the FIFO.

Wherever it runs, it first puts PYTHONPATH, the variables, and `sys.path`
back as they were, so that neither the program nor its children see them,
and then imports the `sitecustomize` it hid, if there is one.
"""


import os as _os
import sys as _sys

# First, so that nothing below can leave the program a changed path.
_microscope_wake = _os.environ.pop('MICROSCOPE_PYTHON_WAKE', None)
# "1" and the PYTHONPATH microscope found, or "0" where there was none.
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
    try:
        with open(os.path.join(here, 'pid'), 'rb') as file:
            launched = int(file.read(32))
    except (OSError, ValueError):
        return
    if launched != os.getpid():
        return  # Another process, which the variables reached.

    # The agent loaded on 3.12 cannot say which thread is the main one.
    module = type(sys)('_microscope_bootstrap')
    module.main = _thread.get_ident()
    sys.modules['_microscope_bootstrap'] = module
    channel = os.open(wake, os.O_RDONLY | os.O_NONBLOCK | os.O_CLOEXEC)
    pending = [b'']

    def run(path):
        # Like a PEP 768 script: fresh globals, its own failures its own.
        try:
            with open(path, 'rb') as file:
                source = file.read()
            exec(compile(source, path, 'exec'), {'__name__': '__microscope__'})
        except BaseException:
            pass  # A consumed script reports its failure through its hook.

    def take(data):
        pending[0] += data
        *lines, pending[0] = pending[0].split(b'\n')
        for line in lines:
            if line:
                run(os.fsdecode(line))

    try:
        while data := os.read(channel, 4096):
            take(data)
        return  # microscope is gone, and no writer is left.
    except BlockingIOError:
        pass
    os.set_blocking(channel, True)

    def waiting():
        while True:
            try:
                data = os.read(channel, 4096)
            except InterruptedError:
                continue
            except OSError:
                return
            if not data:
                return  # microscope is gone.
            take(data)

    os.register_at_fork(after_in_child=lambda: os.close(channel))
    _thread.start_new_thread(waiting, ())


try:
    _microscope_bootstrap(_microscope_wake, _microscope_here)
except Exception:
    pass  # Requests go unacknowledged, which microscope reports.

# The program's own sitecustomize, which this one hid.
del _sys.modules['sitecustomize']
try:
    import sitecustomize  # noqa: E402, F401
except ImportError as _error:
    if _error.name != 'sitecustomize':
        raise
