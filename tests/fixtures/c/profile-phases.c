



#include <pthread.h>
#include <stdatomic.h>
#include <time.h>
#include <unistd.h>

enum { SECOND_MS = 300 };

volatile unsigned long sink;
static _Atomic int released;

static long long now(void) {
    struct timespec time;
    clock_gettime(CLOCK_MONOTONIC, &time);
    return time.tv_sec * 1000000000LL + time.tv_nsec;
}

static void *wait_for_release(void *unused) {
    (void)unused;
    char byte;
    if (read(STDIN_FILENO, &byte, 1) != 1) {
        return (void *)1;
    }
    atomic_store_explicit(&released, 1, memory_order_release);
    return NULL;
}

__attribute__((noinline)) void first(void) {
    while (atomic_load_explicit(&released, memory_order_acquire) == 0) {
        sink++;
    }
}

__attribute__((noinline)) void second(void) {
    long long until = now() + SECOND_MS * 1000000LL;
    while (now() < until) {
        sink++;
    }
}

int main(void) {
    pthread_t waiter;
    if (pthread_create(&waiter, NULL, wait_for_release, NULL) != 0) {
        return 2;
    }
    first();
    void *result;
    if (pthread_join(waiter, &result) != 0 || result != NULL) {
        return 3;
    }
    second();
    return 0;
}
