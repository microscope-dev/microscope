



#include <stdlib.h>

enum { DEPTH = 200 };

volatile unsigned long sink;

__attribute__((noinline)) void leaf(unsigned long iterations) {
    for (unsigned long index = 0; index < iterations; ++index) {
        sink += index;
    }
}

__attribute__((noinline)) void deep(int depth, unsigned long iterations) {
    volatile char frame[64];
    frame[0] = (char)depth;
    if (depth > 0) {
        deep(depth - 1, iterations);
    } else {
        leaf(iterations);
    }
    sink += (unsigned long)frame[0];
}

int main(int argc, char **argv) {
    unsigned long iterations = argc > 1 ? strtoul(argv[1], NULL, 10) : 1;
    deep(DEPTH, iterations);
    return 0;
}
