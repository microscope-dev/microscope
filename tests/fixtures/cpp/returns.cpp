









#include <cstdio>

volatile long sink;

extern "C" __attribute__((noinline)) void reached(const char *checkpoint) {
    sink = reinterpret_cast<long>(checkpoint);
    __asm__ volatile("" ::: "memory");
}

static void integer(const char *checkpoint, const char *path, long value) {
    std::printf("TRUTH\t%s\t%s\tint\t%ld\n", checkpoint, path, value);
}

#define reach(checkpoint)                                                                         \
    do {                                                                                           \
        std::fflush(stdout);                                                                       \
        reached(checkpoint);                                                                       \
    } while (0)

struct Plain {
    int a;
    long b;
};

struct Base {
    int base;
};

struct Derived : Base {
    int own;
};



struct Owner {
    int value;
    ~Owner() { sink = value; }
};

struct Large {
    long a;
    long b;
    long c;
};

namespace shapes {
__attribute__((noinline)) int area(int n) {
    int value = n * 12;
    integer("returned-int", "area", value);
    reach("returned-int");
    return value;
}
}

__attribute__((noinline)) Plain r_plain(int n) {
    Plain value{n + 1, n * -1000000000000L};
    integer("returned-plain", "r_plain.a", value.a);
    integer("returned-plain", "r_plain.b", value.b);
    reach("returned-plain");
    return value;
}

__attribute__((noinline)) Derived r_derived(int n) {
    Derived value;
    value.base = n * 5;
    value.own = n * 6;
    integer("returned-derived", "r_derived.own", value.own);
    reach("returned-derived");
    return value;
}

__attribute__((noinline)) Owner r_owner(int n) {
    Owner value{n * 77};
    integer("returned-owner", "r_owner.value", value.value);
    reach("returned-owner");
    return value;
}

__attribute__((noinline)) Large r_large(int n) {
    Large value{n * 2L, n * 3L, n * 4L};
    integer("returned-large", "r_large.a", value.a);
    integer("returned-large", "r_large.c", value.c);
    reach("returned-large");
    return value;
}

int main(int argc, char **) {
    int n = argc;
    long total = shapes::area(n) + r_plain(n).b + r_derived(n).own + r_owner(n).value;
    total += r_large(n).c;
    sink = total;
    return 0;
}
