









import multiprocessing
import os
import subprocess
import sys


def child_work(value):
    doubled = value * 2  # child_work
    return doubled


def child():
    return 0 if child_work(21) == 42 else 3


def main(how):
    if how == 'child':
        return child()
    if how == 'pool':
        with multiprocessing.Pool(2) as pool:
            return 0 if pool.map(child_work, [21, 21]) == [42, 42] else 3
    if how == 'spawn':
        return subprocess.run([sys.executable, __file__, 'child']).returncode
    process = os.fork()
    if process == 0:
        os._exit(child())
    return os.waitstatus_to_exitcode(os.waitpid(process, 0)[1])


if __name__ == '__main__':
    sys.exit(main(sys.argv[1]))
