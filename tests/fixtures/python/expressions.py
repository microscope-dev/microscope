


















import collections
import dataclasses
import enum
import os
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import truth  # noqa: E402


class Key:


    def __init__(self, hash_):
        self.hash_ = hash_

    def __hash__(self):
        return self.hash_

    def __eq__(self, other):
        return isinstance(other, Key) and other.hash_ == self.hash_


class Point:
    def __init__(self, x, y):
        self.x = x
        self.y = y


class Slotted:


    __slots__ = ("b", "a", "unset")

    def __init__(self):
        self.a = 1
        self.b = "two"


class Both(Slotted):


    def __init__(self):
        super().__init__()
        self.c = 3.5


class Described:


    LIMIT = 10

    def __init__(self):
        self._x = 5

    @property
    def x(self):
        return self._x

    def method(self):
        return self._x


class Fallback:


    def __init__(self):
        self.real = 1

    def __getattr__(self, name):
        return 42


class Custom:


    def __getattribute__(self, name):
        return 7


class Many:


    def __init__(self):
        for index in range(40):
            setattr(self, "a%d" % index, index)


class Deleted:
    def __init__(self):
        self.a = 1
        self.b = 2
        del self.a


class Failure(ValueError):


    def __init__(self, code):
        super().__init__("bad")
        self.code = code


class Tagged(int):
    """An int, whose dict lies before it, as any object's of variable size
    does."""


class Items(list):
    """A list with attributes."""


class Counts(dict):


    def __missing__(self, key):
        return 0


class Plain(dict):
    pass


class Text(str):
    pass


class Color(enum.IntEnum):
    RED = 1


class Indexed:


    def __getitem__(self, key):
        return key

    def __len__(self):
        return 3


class Base:
    __slots__ = ("shadowed", "kept")


class Derived(Base):


    __slots__ = ("shadowed",)


Pair = collections.namedtuple("Pair", "left right")


@dataclasses.dataclass(slots=True)
class Slim:


    zed: int
    alpha: str


@dataclasses.dataclass
class Late:


    b: int
    a: int = dataclasses.field(init=False)

    def __post_init__(self):
        self.extra = 1
        self.a = 2





UNTOUCHED = {"fresh": ("x", "y")}



EXPRESSIONS = sys.argv[1:]
RESULTS = []



LIMIT = 7
count = "the global count"
max = "the global max"
many = {"k%d" % index: index for index in range(300)}
for index in range(0, 300, 3):
    del many["k%d" % index]
del index


def values():
    count = 7
    negative = -12
    ratio = 0.5
    flag = True
    name = "ann"
    wide = "日本語"
    astral = "snake \U0001f40d"
    data = b"ab"
    big = 2**100
    huge = 2**200
    numbers = [10, 20, 30]
    empty = []
    pair = (1, "two")
    nested = {"a": [1, {"b": (2, 3)}]}
    ages = {"ann": 30, "bob": 41, wide: 3, astral: 4}
    squares = {1: 1, 2: 4, 3: 9, -1: 1, 2**40: 0, 0.5: 0.25}
    labels = {None: "none", True: "yes", (1, 2): "pair", b"k": "bytes"}
    doubtful = {Key(7): "seven", 1.5: "float", "s": "str"}


    grid = {(1, 2): "a", (1, (2, 3)): "b", ("ann", 3): "c", (): "d", (1,): "e", (1.5, None): "f"}
    cell = tuple([1, 2])

    nan = float("nan")
    by_nan = {nan: 1}
    nan_pair = (1, nan)
    by_nan_pair = {nan_pair: 2}

    seven = Key(7)
    keyed = {(seven, 1): "k"}
    point = Point(3, 4)
    fresh = Point(7, 8)
    slotted = Slotted()
    both = Both()
    described = Described()
    fallback = Fallback()
    custom = Custom()
    attributes_many = Many()
    deleted = Deleted()
    materialized = Point(5, 6)
    vars(materialized)
    failure = Failure(7)
    tagged = Tagged(5)
    tagged.tag = 1
    digits = {1, 2, 3}
    frozen = frozenset({4})
    no_digits = set()
    items = Items([1, 2])
    items.label = "x"
    counts = Counts(a=1)
    plain_dict = Plain(b=2)
    text = Text("hi")
    color = Color.RED
    indexed = Indexed()
    derived = Derived()
    Base.shadowed.__set__(derived, "hidden")
    derived.kept = 5
    pair_nt = Pair(1, "b")
    ordered = collections.OrderedDict(a=1)
    slim = Slim(3, "a")
    late = Late(1)
    length = len
    append = numbers.append
    klass = Point
    captured = [1, 2]

    def reader():
        return captured

    code = reader.__code__

    frame = sys._getframe()
    RESULTS.extend(truth.evaluated(EXPRESSIONS, frame))
    for local, value in frame.f_locals.items():
        if type(value).__module__ == "__main__":
            RESULTS.append(("instance", local))
            if local in UNTOUCHED:
                found = [(name, object.__getattribute__(value, name)) for name in UNTOUCHED[local]]
            else:
                found = truth.attributes(value)
            for attribute, item in found:
                RESULTS.append(("attribute", local, attribute, truth.canonical(item)))
    del frame
    truth.checkpoint("parked", records=RESULTS)
    return reader


values()
