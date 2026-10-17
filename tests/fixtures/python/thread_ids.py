"""A real PID-namespace and non-UTF-8 proc-status identity oracle."""
import os
import sys
import threading

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import truth

assert os.getpid() == 1, "this fixture must run as a namespace init"
status = open("/proc/self/status", "rb").read()
host = int(next(line.split()[1] for line in status.splitlines()
                if line.startswith(b"NSpid:")))
assert host != os.getpid(), "the proc mount must retain the outer namespace"
truth.report("host_pid", host)


def worker(gate):
    # 16 UTF-8 bytes, truncated by Linux to 15 bytes in the OS comm field.
    threading.current_thread().name = "🧬" * 4
    status = open("/proc/thread-self/status", "rb").read()
    name = next(line for line in status.splitlines() if line.startswith(b"Name:"))
    try:
        name.decode("utf-8")
    except UnicodeDecodeError:
        pass
    else:
        raise AssertionError("the proc name must exercise non-UTF-8 decoding")
    gate.arrive()


gate = truth.Gate(1)
thread = threading.Thread(target=worker, args=(gate,), name="worker")
thread.start()
truth.checkpoint("parked", gate)
