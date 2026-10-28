// Two threads hand a byte back and forth through pipes ROUNDS times, each
// blocking until the other answers: many context switches for little CPU
// time, which fill a ring with records that are not samples. Then the main
// thread spins in its own code for SPIN_MS, where it is sampled.
#include <pthread.h>
#include <stdlib.h>
#include <time.h>
#include <unistd.h>

volatile unsigned long sink;

static int there[2];
static int back[2];
static long rounds;

static void *answer(void *unused) {
    (void)unused;
    char byte;
    for (long round = 0; round < rounds; ++round) {
        if (read(there[0], &byte, 1) != 1 || write(back[1], &byte, 1) != 1) {
            return (void *)1;
        }
    }
    return NULL;
}

static long long now(void) {
    struct timespec time;
    clock_gettime(CLOCK_MONOTONIC, &time);
    return time.tv_sec * 1000000000LL + time.tv_nsec;
}

int main(int argc, char **argv) {
    rounds = argc > 1 ? atol(argv[1]) : 20000;
    long spin_ms = argc > 2 ? atol(argv[2]) : 50;
    if (pipe(there) != 0 || pipe(back) != 0) {
        return 2;
    }
    pthread_t thread;
    if (pthread_create(&thread, NULL, answer, NULL) != 0) {
        return 2;
    }
    char byte = 0;
    for (long round = 0; round < rounds; ++round) {
        if (write(there[1], &byte, 1) != 1 || read(back[0], &byte, 1) != 1) {
            return 1;
        }
    }
    void *failed;
    pthread_join(thread, &failed);
    long long until = now() + spin_ms * 1000000LL;
    while (now() < until) {
        sink++;
    }
    return failed != NULL;
}
