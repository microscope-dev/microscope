"""Values of the standard library's types, which microscope presents as
their own repr writes them: those only C defines, which it reads itself,
and those Python defines, through its built-in views
(views/python-std.views). Classes deriving from either are among them, as
are containers within each other, a deque of more than one block, and an
OrderedDict whose order its dict does not keep.

The test passes expressions as arguments, which CPython evaluates in the
frame before it parks. Besides each expression's result, the checkpoint
reports the children of each local of a collection type, in order: each
element's text, or each entry's key and value.

    TRUTH  child  LOCAL  KEY  VALUE

KEY is - for an element.
"""

import collections
import datetime
import decimal
import enum
import fractions
import os
import pathlib
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import truth  # noqa: E402


class Color(enum.Enum):
    RED = 1
    GREEN = "green"


class Level(enum.IntEnum):
    LOW = 1


class Queue(collections.deque):
    """A deque with attributes."""


class Ordered(collections.OrderedDict):
    pass


class Stamp(datetime.datetime):
    """A datetime its type's C writes by the class's own name."""


class Impostor:
    """A class of a library type's module and name, which C does not
    define, and so is read as any other class's objects are."""


Impostor.__module__ = "collections"
Impostor.__qualname__ = "deque"


EXPRESSIONS = sys.argv[1:]
RESULTS = []


def values():
    queue = collections.deque([1, "two", None])
    bounded = collections.deque(range(3), maxlen=5)
    empty_queue = collections.deque()
    # Two blocks' items, the first past the start of its block.
    long_queue = collections.deque(range(60))
    long_queue.rotate(-9)
    long_queue.appendleft(-1)
    nested_queue = collections.deque([[1, 2], collections.deque([3])])
    queue_sub = Queue([1, 2])
    queue_sub.label = "q"
    grouped = collections.defaultdict(list)
    grouped["a"].append(1)
    grouped["b"]
    no_factory = collections.defaultdict()
    ordered = collections.OrderedDict(a=1, b=2, c=3)
    ordered.move_to_end("a")
    del ordered["b"]
    ordered["d"] = 4
    empty_ordered = collections.OrderedDict()
    ordered_sub = Ordered(x=1)
    day = datetime.date(2024, 2, 29)
    clock = datetime.time(13, 5)
    precise_clock = datetime.time(1, 2, 3, 456789)
    zoned_clock = datetime.time(1, 2, tzinfo=datetime.timezone.utc, fold=1)
    moment = datetime.datetime(2024, 1, 2, 3, 4, 5, 6)
    est = datetime.timezone(datetime.timedelta(hours=-5), "EST")
    zoned = datetime.datetime(2024, 1, 2, 3, 4, fold=1, tzinfo=est)
    utc_moment = datetime.datetime(2000, 1, 1, tzinfo=datetime.timezone.utc)
    stamp = Stamp(2024, 1, 1, 0, 0, 30)
    span = datetime.timedelta(days=-1, seconds=5)
    no_span = datetime.timedelta(0)
    tiny_span = datetime.timedelta(microseconds=7)
    utc = datetime.timezone.utc
    india = datetime.timezone(datetime.timedelta(hours=5, minutes=30))
    named_utc = datetime.timezone(datetime.timedelta(0), "Z")
    price = decimal.Decimal("12.50")
    negative_price = decimal.Decimal("-0.001")
    small = decimal.Decimal("1E-7")
    large = decimal.Decimal("1.23E+30")
    integral = decimal.Decimal(10**25 + 1)
    negative_zero = decimal.Decimal("-0")
    infinite = decimal.Decimal("-Infinity")
    not_a_number = decimal.Decimal("NaN")
    signaling = decimal.Decimal("sNaN12")
    color = Color.RED
    green = Color.GREEN
    level = Level.LOW
    half = fractions.Fraction(1, 2)
    whole = fractions.Fraction(4, 2)
    negative_third = fractions.Fraction(-1, 3)
    path = pathlib.PurePosixPath("/tmp", "a.txt")
    str(path)
    home = pathlib.Path("/home")
    str(home)
    mixed = [day, price, ordered]
    impostor = Impostor()

    frame = sys._getframe()
    RESULTS.extend(truth.evaluated(EXPRESSIONS, frame))
    RESULTS.extend(children(frame.f_locals))
    del frame
    truth.checkpoint("parked", records=RESULTS)


def children(locals_):
    """The child records of each local of a collection type: a dict's
    entries in the order its own iteration gives."""
    records = []
    for local, value in locals_.items():
        if isinstance(value, dict):
            for key in iter(value):
                item = dict.__getitem__(value, key)
                records.append(("child", local, truth.canonical(key), truth.canonical(item)))
        elif isinstance(value, collections.deque):
            for item in collections.deque.__iter__(value):
                records.append(("child", local, "-", truth.canonical(item)))
    return records


values()
