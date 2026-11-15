











import contextvars
import os
import select
import sys
import threading


def opens(path):
    os.close(os.open(path, os.O_RDONLY))  # OPENS


def through_c(path):

    contextvars.copy_context().run(opens, path)  # THROUGH_C


def worker():
    through_c('/dev/null')  # WORKER


def probe():
    os.close(os.open('/dev/random', os.O_RDONLY))


hold = '--hold' in sys.argv
if hold:
    while not select.select([sys.stdin], [], [], 0.01)[0]:
        probe()
    sys.stdin.readline()

thread = threading.Thread(target=worker)
thread.start()
thread.join()
opens('/proc/self/stat')  # MAIN

if hold:
    sys.stdin.read()
