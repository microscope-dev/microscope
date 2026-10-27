#include <stdlib.h>

volatile unsigned long sink;

__attribute__((noinline)) void hot(unsigned long value) {
    sink += value;
}

int main(int argc, char **argv) {
    unsigned long calls = argc > 1 ? strtoul(argv[1], NULL, 10) : 2000000;
    for (unsigned long i = 0; i < calls; ++i) {
        hot(i);
    }
    return 0;
}
