#define _GNU_SOURCE
#include <errno.h>
#include <pthread.h>
#include <stdatomic.h>
#include <stdint.h>
#include <sys/prctl.h>



enum { WORKERS = 3 };

_Atomic uint64_t spins;

__attribute__((noinline)) void spun(void) {
    atomic_fetch_add_explicit(&spins, 1, memory_order_relaxed);
}

static void *worker(void *argument) {
    (void)argument;
    for (;;) {
        spun();
    }
    return NULL;
}

int main(void) {
    if ((prctl(PR_SET_PTRACER, PR_SET_PTRACER_ANY) == -1 && errno != EINVAL)) {
        return 1;
    }
    pthread_t workers[WORKERS];
    for (int index = 0; index < WORKERS; ++index) {
        if (pthread_create(&workers[index], NULL, worker, NULL) != 0) {
            return 2;
        }
    }
    worker(NULL);
}
