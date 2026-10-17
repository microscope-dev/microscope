"""Python's values, where this program says they are. Before each checkpoint
the program prints its own truth, one tab-separated line per value,

    TRUTH  CHECKPOINT  PATH  summary  VALUE

and then calls the extension's reached(CHECKPOINT), a C function; the tests
stop there and read the values in its caller. A path is an expression in
the caller's frame, written as both Python and microscope read it: a
variable, or an item, a key, an attribute, or a length of one. VALUE is truth.canonical() of what CPython's eval() of the
path gives, the text microscope must show. The first checkpoint, skipped,
is one a breakpoint's condition over its Python argument must pass by.

The first argument is the directory of the extension built for the
interpreter.
"""

import collections
import dataclasses
import datetime
import decimal
import enum
import fractions
import os
import pathlib
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
sys.path.insert(0, sys.argv[1])
import microscope_extension  # noqa: E402
import truth  # noqa: E402

reached = microscope_extension.reached


def say(checkpoint, frame_locals, paths):
    """Prints the truth of each path in the caller's frame."""
    for path in paths:
        value = eval(path, globals(), frame_locals)  # noqa: S307
        truth.report(checkpoint, path, "summary", truth.canonical(value))


class Point:
    def __init__(self, x, y):
        self.x = x
        self.y = y


class Slotted:
    __slots__ = ("left", "right")

    def __init__(self, left, right):
        self.left = left
        self.right = right


@dataclasses.dataclass
class Order:
    item: str
    count: int = 1


Pair = collections.namedtuple("Pair", "first second")


class Color(enum.Enum):
    RED = 1
    GREEN = 2


class Level(enum.IntEnum):
    LOW = 10


def skipped():
    nothing = None
    say("skipped", locals(), ["nothing"])
    reached("skipped")


def scalars():
    nothing = None
    yes = True
    no = False
    small = 7
    negative = -42
    huge = 2**100
    tiny = -(2**64)
    ratio = 0.1
    large = 1e300
    infinite = float("inf")
    negative_zero = -0.0
    not_a_number = float("nan")
    say(
        "scalars",
        locals(),
        [
            "nothing", "yes", "no", "small", "negative", "huge", "tiny", "ratio", "large",
            "infinite", "negative_zero", "not_a_number", "small + negative", "huge - small",
            "-small", "small * 3 == 21", "ratio * 2",
        ],
    )
    reached("scalars")


def text():
    ascii_text = "hello"
    latin = "café"
    wide = "日本語"
    astral = "🐍 snake"
    escaped = "tab\there\nquote'\"\\"
    lone = "\ud800"
    empty = ""
    data = b"\x00bytes\xff"
    say(
        "text",
        locals(),
        [
            "ascii_text", "latin", "wide", "astral", "escaped", "lone", "empty", "data",
            "len(wide)", "len(astral)", "len(data)",
            'ascii_text == "hello"',
        ],
    )
    reached("text")


def containers():
    numbers = [1, 2, 3]
    pair = (numbers, "two")
    single = (1,)
    mapping = {"a": 1, "b": "two", 3: None}
    nested = {"rows": [[1, 2], [3, 4]], "empty": {}}
    unique = {5}
    frozen = frozenset()
    looping = [1]
    looping.append(looping)
    say(
        "containers",
        locals(),
        [
            "numbers", "pair", "single", "mapping", "nested", "unique", "frozen", "looping",
            "numbers[0]", "numbers[-1]", "pair[0][1]", 'mapping["b"]', "mapping[3]",
            'nested["rows"][1][0]', "len(mapping)", 'len(nested["rows"])', "looping[1][0]",
        ],
    )
    reached("containers")


def objects():
    point = Point(3, 4)
    slotted = Slotted("l", "r")
    order = Order("tea", 2)
    pair = Pair(1, "b")
    color = Color.GREEN
    level = Level.LOW
    kind = Point
    function = say
    builtin = len
    module = collections
    say(
        "objects",
        locals(),
        [
            "point", "slotted", "order", "pair", "color", "level", "kind", "function",
            "builtin", "module", "point.x", "point.x + point.y", "slotted.right", "order.item",
            "order.count", "pair[1]", "len(pair)",
        ],
    )
    reached("objects")


def library():
    queue = collections.deque([1, 2, 3], maxlen=5)
    defaults = collections.defaultdict(list, {"k": [1]})
    ordered = collections.OrderedDict([("z", 1), ("a", 2)])
    ordered.move_to_end("z")
    counts = collections.Counter("abca")
    day = datetime.date(2024, 2, 29)
    moment = datetime.datetime(2024, 2, 29, 13, 5, 9, 250, tzinfo=datetime.timezone.utc)
    clock = datetime.time(7, 30)
    span = datetime.timedelta(days=-1, seconds=5)
    zone = datetime.timezone(datetime.timedelta(hours=-5), "EST")
    price = decimal.Decimal("-12.50")
    tiny = decimal.Decimal("1E-30")
    half = fractions.Fraction(1, 2)
    home = pathlib.PurePosixPath("/home/user")
    str(home)
    say(
        "library",
        locals(),
        [
            "queue", "defaults", "ordered", "counts", "day", "moment", "clock", "span", "zone",
            "price", "tiny", "half", "home", "queue[0]", "queue[-1]", "len(queue)",
            'defaults["k"]', 'ordered["a"]', 'counts["a"]',
        ],
    )
    reached("library")


skipped()
scalars()
text()
containers()
objects()
library()
