"""What the VS Code acceptance test debugs: a line to break at and step
from, and an exception the program raises and catches itself."""


def area(side):
    result = side * side  # AREA_LINE
    return result


def fail():
    raise ValueError("the shape is wrong")  # RAISE_LINE


print("AREA", area(4), flush=True)
try:
    fail()
except ValueError:
    pass
print("DONE", flush=True)
