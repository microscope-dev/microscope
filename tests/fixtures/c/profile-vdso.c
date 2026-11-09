// Time a profiler test samples in the vDSO, the kernel's shared object that
// no file holds: main reads the clock in a loop through it for the number
// of reads its first argument names, or, given "random" as its second,
// draws that many pages of random bytes, which the vDSO's hand-written
// ChaCha20 code makes without call-frame information.
#define _GNU_SOURCE
#include <stdlib.h>
#include <string.h>
#include <sys/random.h>
#include <time.h>

volatile long sink;

__attribute__((noinline)) void read_clock(long reads) {
    struct timespec now;
    for (long read = 0; read < reads; ++read) {
        clock_gettime(CLOCK_MONOTONIC, &now);
        sink += now.tv_nsec;
    }
}

__attribute__((noinline)) void draw_random(long pages) {
    static unsigned char page[4096];
    for (long drawn = 0; drawn < pages; ++drawn) {
        if (getrandom(page, sizeof(page), 0) != (ssize_t)sizeof(page)) {
            exit(1);
        }
        sink += page[0];
    }
}

int main(int argc, char **argv) {
    long count = argc > 1 ? atol(argv[1]) : 1;
    if (argc > 2 && strcmp(argv[2], "random") == 0) {
        draw_random(count);
    } else {
        read_clock(count);
    }
    return 0;
}
