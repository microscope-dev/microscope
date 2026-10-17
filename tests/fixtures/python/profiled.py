"""A program to profile: two threads spin in Python, taking turns at the GIL
where there is one, and a third sleeps, until a line arrives on standard
input. Each line a test looks for is marked with a comment naming it, which
the test finds in this file.
"""

import sys
import threading
import time

stopping = threading.Event()


def work(count):
    total = 0
    for number in range(count):  # LINE work-loop
        total += number * number  # LINE work-add
    return total


def spin():
    while not stopping.is_set():
        work(1000)  # LINE spin


def nap():
    while not stopping.is_set():
        time.sleep(0.001)  # LINE nap


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
