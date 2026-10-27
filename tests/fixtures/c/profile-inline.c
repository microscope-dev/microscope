#include <stdlib.h>

volatile unsigned long sink;

// Inlined into scan, where it does nearly all of the loop's work.
static inline __attribute__((always_inline)) unsigned long step(unsigned long sum,
                                                                unsigned long i) {
    return sum * 31 + i * i;
}

__attribute__((noinline)) unsigned long scan(unsigned long n) {
    unsigned long sum = 0;
    for (unsigned long i = 0; i < n; ++i) {
        sum = step(sum, i);
    }
    return sum;
}

int main(int argc, char **argv) {
    int count = argc > 1 ? atoi(argv[1]) : 40;
    for (int i = 0; i < count; ++i) {
        sink += scan(4000000 + i);
    }
    return 0;
}
