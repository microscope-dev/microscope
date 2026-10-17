"""A normal program with two requested sites and repeated activations."""
import contextvars


def target():
    value = 1  # FIRST_LINE
    value += 1  # SECOND_LINE
    return value


for iteration in range(4):
    print('CONTROL ITERATION', iteration, contextvars.copy_context().run(target), flush=True)
