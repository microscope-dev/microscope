



#define _GNU_SOURCE
#include <stdio.h>
#include <stdlib.h>
#include <sys/syscall.h>
#include <time.h>
#include <unistd.h>

static double now(void) {
    struct timespec time;
    clock_gettime(CLOCK_MONOTONIC, &time);
    return (double)time.tv_sec * 1e9 + (double)time.tv_nsec;
}

int main(int argc, char **argv) {
    long count = argc > 1 ? atol(argv[1]) : 1000000;
    double rate = argc > 2 ? atof(argv[2]) : 0;
    double start = now();
    for (long call = 0; call < count; call++) {
        syscall(SYS_getppid);

        if (rate > 0 && call % 64 == 0) {
            double due = start + (double)call * 1e9 / rate;
            while (now() < due) {
            }
        }
    }
    double elapsed = now() - start;
    printf("%ld %.1f\n", count, elapsed / (double)count);
    return 0;
}
