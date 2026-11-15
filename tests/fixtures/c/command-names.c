


struct point {
    int x;
    int y;
};

static int shadowed = 7;
static int counter = 3;

__attribute__((noinline)) static int add(int left, int right) {
    return left + right;
}

__attribute__((noinline)) static int names(int n) {
    int x = n * 2;
    int list = 5;
    int p = 4;
    int shadowed = -1;
    struct point origin = {.x = 1, .y = 2};
    struct point *where = &origin;
    int (*operation)(int, int) = add;
    volatile int sink = operation(x, list) + p + shadowed + where->y + counter;
    return sink;
}

int main(void) {
    return names(10) + shadowed == 40 ? 0 : 1;
}
