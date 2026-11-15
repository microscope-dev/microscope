



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
