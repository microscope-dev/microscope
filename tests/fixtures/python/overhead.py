



import sys


def leaf(n):
    total = 0
    for i in range(n):
        total += i * i
    return total


def middle(n, depth):
    if depth == 0:
        return leaf(n)
    return middle(n, depth - 1) + 1


def unit():
    return sum(middle(50, 8) for _ in range(20))


def main():
    out = sys.stdout
    while True:
        unit()
        out.write("done\n")
        out.flush()


main()
