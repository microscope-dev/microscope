"""Python breakpoints: lines, functions, conditions, hit counts and logs.

Each marked line is where a test sets a breakpoint. The program prints what
it computed, so stopping must change nothing; it ends by forking a child.
"""
import os
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))


def accumulate(limit):
    total = 0
    for index in range(limit):
        total += index  # LOOP_LINE
        square = index * index  # LOG_LINE
    return total


def count(limit):
    seen = 0
    for tick in range(limit):
        seen += 1  # TICK_LINE
    return seen


class Shape:
    def area(self, scale):
        return scale * 2  # AREA_LINE


def outer():
    def area(side):
        return side * side  # NESTED_LINE
    return area(4)


def forked():
    child = os.fork()
    if child == 0:
        tools = [tool for tool in range(6) if sys.monitoring.get_tool(tool)]
        print('CHILD TOOLS', tools, 'NESTED', outer(), flush=True)
        os._exit(0)
    _, status = os.waitpid(child, 0)
    print('CHILD EXIT', os.waitstatus_to_exitcode(status), flush=True)
    print('PARENT NESTED', outer(), flush=True)


def main():
    print('ACCUMULATED', accumulate(5), flush=True)
    print('COUNTED', count(6), flush=True)
    print('AREA', Shape().area(3), flush=True)
    print('NESTED', outer(), flush=True)
    import breakpoints_later
    print('LATER', breakpoints_later.later(7), flush=True)
    forked()


main()
print('BREAKPOINTS DONE', flush=True)
