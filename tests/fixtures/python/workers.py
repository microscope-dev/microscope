



import sys
import threading

import numpy

stopping = threading.Event()


def multiply():
    matrix = numpy.ones((600, 600))
    while not stopping.is_set():
        matrix @ matrix


threading.Thread(target=multiply).start()
sys.stdin.readline()
stopping.set()
