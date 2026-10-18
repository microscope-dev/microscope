"""What the page's tests debug: a line far enough into its file that the
page must scroll to show it, run by a caller in another file."""

from contextlib import contextmanager
from dataclasses import dataclass, field


@dataclass
class Item:
    name: str
    price: float
    quantity: int
    tags: list[str] = field(default_factory=list)

    def value(self):
        return self.price * self.quantity


class Shelf:
    def __init__(self, name):
        self.name = name
        self.items = {}

    def add(self, item):
        self.items[item.name] = item

    def count(self):
        return sum(item.quantity for item in self.items.values())


@contextmanager
def stocked():
    shelf = Shelf("north")
    shelf.add(Item("bolt", 0.25, 120, ["metal", "small"]))  # STOCK_LINE
    shelf.add(Item("nut", 0.10, 40))
    shelf.add(Item("gear", 4.50, 7, ["metal"]))
    yield shelf


def worth(shelf):
    total = 0.0
    for item in shelf.items.values():
        total += item.value()
    return total


def report(shelf):
    total = worth(shelf)
    return f"{shelf.name}: {shelf.count()} items worth {total:.2f}"


with stocked() as shelf:
    print(report(shelf), flush=True)
