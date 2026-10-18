"""Variables a test gives new values where a breakpoint stops: the program
prints what each holds afterwards, so each value is the one the program then
uses. A second thread runs at the second stop, where nothing is given.
"""
import threading

LIMIT = 3


def alone():
    count = 1
    ratio = 0.5
    name = "before"
    flag = False
    other = [1, 2]
    held = None
    gone = "kept"
    print("ALONE", flush=True)  # ALONE_LINE
    print("COUNT", count, "RATIO", ratio, "NAME", name, "FLAG", flag,
          "HELD", held is other, "GONE", gone, "LIMIT", LIMIT, flush=True)


def beside():
    count = 1
    release = threading.Event()
    started = threading.Event()

    def wait():
        started.set()
        release.wait()  # WAIT_LINE

    thread = threading.Thread(target=wait)
    thread.start()
    started.wait()
    print("BESIDE", flush=True)  # BESIDE_LINE
    release.set()
    thread.join()
    print("BESIDE COUNT", count, flush=True)


alone()
beside()
print("ASSIGNMENTS DONE", flush=True)
