#include <stdio.h>
#include <stdlib.h>
#include <string.h>

volatile unsigned long sink;

__attribute__((noinline)) void hot(unsigned long value) {
    sink += value;
}

static void call(unsigned long calls) {
    for (unsigned long i = 0; i < calls; ++i) {
        hot(i);
    }
}

int main(int argc, char **argv) {
    if (argc > 1 && strcmp(argv[1], "-") == 0) {
        unsigned long calls;
        while (scanf("%lu", &calls) == 1) {
            call(calls);
            puts("called");
            fflush(stdout);
        }
        return 0;
    }

    call(argc > 1 ? strtoul(argv[1], NULL, 10) : 2000000);
    return 0;
}
