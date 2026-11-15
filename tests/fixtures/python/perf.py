











import resource
import sys
import threading


resource.setrlimit(resource.RLIMIT_CPU, (60, 60))


def early(unused):
    released = threading.Thread(target=sys.stdin.readline)
    released.start()
    while released.is_alive():
        pass


def leaf(unused):
    while True:
        pass


def middle():

    list(map(early, [None]))
    list(map(leaf, [None]))


middle()
