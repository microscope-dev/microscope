"""A program whose threads that spin run no Python: numpy multiplies
matrices on OpenMP's worker threads, which no interpreter knows, until a
line arrives on standard input.
"""
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
