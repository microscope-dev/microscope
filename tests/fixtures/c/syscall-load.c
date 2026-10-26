// Makes ROUNDS rounds of openat, read, and close of /dev/zero on each of
// THREADS threads, paced to RATE calls a second on each thread (as fast as
// it can when RATE is 0), from a few frames deep, then prints the calls it
// made and the nanoseconds they took: a steady load or a burst for the BPF
// engine's ring.
#define _GNU_SOURCE
#include <fcntl.h>
#include <pthread.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
#include <unistd.h>

static long rounds = 10000;
static double rate = 0;

static double now(void) {
    struct timespec time;
    clock_gettime(CLOCK_MONOTONIC, &time);
    return (double)time.tv_sec * 1e9 + (double)time.tv_nsec;
}

static __attribute__((noinline)) int one_round(void) {
    char buffer[64];
    int fd = openat(AT_FDCWD, "/dev/zero", O_RDONLY | O_CLOEXEC);
    if (fd < 0) {
        return 1;
    }
    int failed = read(fd, buffer, sizeof buffer) != sizeof buffer;
    close(fd);
    return failed;
}

static __attribute__((noinline)) int nested(int depth) {
    if (depth > 0) {
        int failed = nested(depth - 1);
        __asm__ volatile("" ::: "memory");
        return failed;
    }
    return one_round();
}

static void *work(void *argument) {
    (void)argument;
    // Stack above every call, written so that its pages are there to
    // read, so that each call's copy of its stack (-k=dwarf) is whole, as
    // a deeper program's is.
    char above[16384];
    memset(above, 1, sizeof above);
    __asm__ volatile("" : : "r"(above) : "memory");
    double start = now();
    for (long round = 0; round < rounds; round++) {
        if (nested(4)) {
            _exit(2);
        }
        // Pacing reads the vDSO clock, which makes no system call.
        if (rate > 0) {
            double due = start + (double)(round + 1) * 3e9 / rate;
            while (now() < due) {
            }
        }
    }
    return NULL;
}

int main(int argc, char **argv) {
    int threads = argc > 1 ? atoi(argv[1]) : 1;
    rounds = argc > 2 ? atol(argv[2]) : rounds;
    rate = argc > 3 ? atof(argv[3]) : 0;
    if (threads < 1 || threads > 64 || rounds < 0) {
        fprintf(stderr, "usage: syscall-load [THREADS [ROUNDS [RATE]]]\n");
        return 1;
    }
    pthread_t ids[64];
    double start = now();
    for (int thread = 1; thread < threads; thread++) {
        if (pthread_create(&ids[thread], NULL, work, NULL)) {
            return 1;
        }
    }
    work(NULL);
    for (int thread = 1; thread < threads; thread++) {
        pthread_join(ids[thread], NULL);
    }
    printf("%ld %.0f\n", (long)threads * rounds * 3, now() - start);
    return 0;
}
