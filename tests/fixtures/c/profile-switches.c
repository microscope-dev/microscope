// Two threads hand a byte back and forth through pipes ROUNDS times, each
// blocking until the other answers: many context switches for little CPU
// time, which fill a ring with records that are not samples. Then SPINNERS
// threads, the main one among them, spin in their own code for SPIN_MS,
// where they are sampled.
#include <pthread.h>
#include <stdlib.h>
#include <time.h>
#include <unistd.h>

volatile unsigned long sink;

static int there[2];
static int back[2];
static long rounds;
static long long until;

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

static void *spin(void *unused) {
    (void)unused;
    while (now() < until) {
        sink++;
    }
    return NULL;
}

int main(int argc, char **argv) {
    rounds = argc > 1 ? atol(argv[1]) : 20000;
    long spin_ms = argc > 2 ? atol(argv[2]) : 50;
    long spinners = argc > 3 ? atol(argv[3]) : 1;
    if (pipe(there) != 0 || pipe(back) != 0 || spinners < 1 || spinners > 64) {
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
    until = now() + spin_ms * 1000000LL;
    pthread_t others[64];
    for (long index = 1; index < spinners; ++index) {
        if (pthread_create(&others[index], NULL, spin, NULL) != 0) {
            return 2;
        }
    }
    spin(NULL);
    for (long index = 1; index < spinners; ++index) {
        pthread_join(others[index], NULL);
    }
    return failed != NULL;
}
