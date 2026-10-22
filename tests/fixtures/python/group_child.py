"""A program that starts a child interpreter, which runs child_work.

`python group_child.py spawn` starts it as subprocess does, by vfork and
exec; `fork` forks it with os.fork. The parent exits with the child's
status, which is 0 when child_work returned what it should.
"""

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
    if how == 'spawn':
        return subprocess.run([sys.executable, __file__, 'child']).returncode
    process = os.fork()
    if process == 0:
        os._exit(child())
    return os.waitstatus_to_exitcode(os.waitpid(process, 0)[1])


if __name__ == '__main__':
    sys.exit(main(sys.argv[1]))
