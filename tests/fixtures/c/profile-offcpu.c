



#define _GNU_SOURCE
#include <pthread.h>
#include <sched.h>
#include <stdatomic.h>
#include <time.h>

enum { ROUNDS = 5, NAP_MS = 20, SPIN_MS = 100 };

volatile unsigned long sink;
static cpu_set_t one_cpu;
static _Atomic int arrived;

static long long now(void) {
    struct timespec time;
    clock_gettime(CLOCK_MONOTONIC, &time);
    return time.tv_sec * 1000000000LL + time.tv_nsec;
}

static void spin(long long nanoseconds) {
    long long until = now() + nanoseconds;
    while (now() < until) {
        sink++;
    }
}

static void *sleeper(void *unused) {
    (void)unused;
    pthread_setname_np(pthread_self(), "sleeper");
    struct timespec nap = {0, NAP_MS * 1000000L};
    for (int round = 0; round < ROUNDS; ++round) {
        spin(1000000);
        while (nanosleep(&nap, &nap) != 0) {
        }
        nap.tv_nsec = NAP_MS * 1000000L;
    }
    return NULL;
}

static void *spinner(void *unused) {
    (void)unused;
    pthread_setname_np(pthread_self(), "spinner");
    if (sched_setaffinity(0, sizeof one_cpu, &one_cpu) != 0) {
        return (void *)1;
    }
    atomic_fetch_add_explicit(&arrived, 1, memory_order_acq_rel);
    while (atomic_load_explicit(&arrived, memory_order_acquire) != 2) {
        sched_yield();
    }
    spin(SPIN_MS * 1000000LL);
    return NULL;
}

int main(void) {
    cpu_set_t allowed;
    if (sched_getaffinity(0, sizeof allowed, &allowed) != 0) {
        return 2;
    }
    CPU_ZERO(&one_cpu);
    for (int cpu = 0; cpu < CPU_SETSIZE; ++cpu) {
        if (CPU_ISSET(cpu, &allowed)) {
            CPU_SET(cpu, &one_cpu);
            break;
        }
    }
    pthread_t threads[3];
    void *(*starts[3])(void *) = {sleeper, spinner, spinner};
    for (int index = 0; index < 3; ++index) {
        if (pthread_create(&threads[index], NULL, starts[index], NULL) != 0) {
            return 3;
        }
    }
    for (int index = 0; index < 3; ++index) {
        void *result;
        if (pthread_join(threads[index], &result) != 0 || result != NULL) {
            return 4;
        }
    }
    return 0;
}
