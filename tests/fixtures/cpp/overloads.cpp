

namespace shapes {
struct Widget {
    int size;
    __attribute__((noinline)) int pick() const { return size; }
};
}

__attribute__((noinline)) int pick(int value) { return value + 1; }
__attribute__((noinline)) double pick(double value) { return value * 2; }

int main() {
    shapes::Widget widget{5};
    int total = pick(1) + static_cast<int>(pick(1.5)) + widget.pick();
    return total == 10 ? 0 : 1;
}
