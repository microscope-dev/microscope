"""Locals of every built-in type the debugger presents, each frame parked
on a thread of its own: scalars at their edges, containers of each layout,
and the locals that hold no value of their own at the checkpoint, unbound
ones and those a closure shares. With --abort, the main thread aborts at
the checkpoint for a core dump."""

import os
import sys
import threading

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import truth  # noqa: E402


def scalars(gate):
    nothing = None
    yes = True
    no = False
    zero = 0
    small = 7
    negative = -12345
    one_digit = 2**30 - 1
    two_digits = 2**30
    big = 2**100 + 12345
    huge_negative = -(10**40) - 1
    half = 0.5
    tenth = 0.1
    large = 1e16
    precise = 1234567890123456.0
    tiny = 5e-324
    small_fraction = 0.0001
    smaller_fraction = 0.00001
    infinity = float("inf")
    negative_infinity = float("-inf")
    negative_zero = -0.0
    not_a_number = float("nan")
    text = "hello"
    empty = ""
    latin = "café"
    wide = "日本語"
    astral = "snake \U0001f40d"
    escaped = "tab\tnew\nquote's \"both\" back\\slash \x7f"
    single_quoted = "it's"
    controls = "next line \x85 and \x9f"
    surrogate = "lone \udc80"
    unprinted = "no\xa0break \u200b\u2028 \u202eright \ue000 \U000e0001"
    data = b"\x00\x01binary\xff'"
    no_data = b""
    gate.arrive()
    return (nothing, yes, no, zero, small, negative, one_digit, two_digits, big,
            huge_negative, half, tenth, large, precise, tiny, small_fraction,
            smaller_fraction, infinity, negative_infinity, negative_zero,
            not_a_number, text, empty, latin, wide, astral, escaped,
            single_quoted, controls, surrogate, unprinted, data, no_data)


def containers(gate):
    numbers = [1, 2, 3]
    empty_list = []
    nested = [[1, [2]], (3,), {"k": [4]}]
    pair = (1, "two")
    single = (1,)
    empty_tuple = ()
    by_name = {"a": 1, "b": "two", "c": None}
    mixed = {1: "one", "two": 2, (3,): [3], None: b"n"}
    empty_dict = {}
    # Deleted entries leave holes the dict's entries keep, in order.
    thinned = {}
    for index in range(20):
        thinned["k%d" % index] = index
    for index in range(0, 20, 2):
        del thinned["k%d" % index]
    gate.arrive()
    return numbers, empty_list, nested, pair, single, empty_tuple, by_name, mixed, empty_dict


def unbound_and_shared(gate):
    bound = 1
    shared = [2]

    def reader():
        return shared

    gate.arrive()
    later = 3
    return bound, reader, later


def calls_with_arguments(gate, positional, *rest, keyword=None, **options):
    gate.arrive()
    return positional, rest, keyword, options


gate = truth.Gate(4)
workers = [
    threading.Thread(target=scalars, args=(gate,), name="scalars"),
    threading.Thread(target=containers, args=(gate,), name="containers"),
    threading.Thread(target=unbound_and_shared, args=(gate,), name="unbound"),
    threading.Thread(
        target=calls_with_arguments,
        args=(gate, "first", 2, 3),
        kwargs={"keyword": 4.5, "extra": "yes"},
        name="arguments",
    ),
]
for worker in workers:
    worker.start()

truth.code_table()
truth.checkpoint("parked", gate, abort="--abort" in sys.argv)
