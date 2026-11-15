





import os
import sys
import threading

released = threading.Thread(target=sys.stdin.readline)
released.start()
total = 0
while released.is_alive():
    total += 1

profiled = os.path.join(os.path.dirname(os.path.abspath(__file__)), 'profiled.py')
os.execv(sys.argv[1], [sys.argv[1], '-I', '-B', profiled])
