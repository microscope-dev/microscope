"""A program to profile: two threads spin in Python, taking turns at the GIL
where there is one, and a third sleeps, until a line arrives on standard
input. Each line a test looks for is marked with a comment naming it, which
the test finds in this file.
"""

import ctypes
import sys
import threading
import time

stopping = threading.Event()


def work(count):
    total = 0
    for number in range(count):  # LINE work-loop
        total += number * number  # LINE work-add
    return total


def name_thread():
    if "--bad-names" in sys.argv:
        name = ("🧬" * 4).encode()
        assert ctypes.CDLL(None).prctl(15, ctypes.c_char_p(name), 0, 0, 0) == 0
        with open("/proc/thread-self/status", "rb") as source:
            name_field = source.readline()
        try:
            name_field.decode()
        except UnicodeDecodeError:
            pass
        else:
            raise AssertionError("the kernel name must end in a partial UTF-8 character")


def spin():
    name_thread()
    while not stopping.is_set():
        work(1000)  # LINE spin


def nap():
    name_thread()
    while not stopping.is_set():
        time.sleep(0.001)  # LINE nap


name_thread()

threads = [
    threading.Thread(target=spin, name="spinner-1"),
    threading.Thread(target=spin, name="spinner-2"),
    threading.Thread(target=nap, name="napper"),
]
for thread in threads:
    thread.start()

sys.stdin.readline()
stopping.set()
for thread in threads:
    thread.join()
