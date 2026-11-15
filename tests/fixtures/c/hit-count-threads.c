#include <pthread.h>
#include <sched.h>
#include <stdatomic.h>
#include <stdint.h>




enum { WORKERS = 4, CALLS = 100 };

static _Atomic uint64_t total;
static _Atomic int workers_ready;
static _Atomic int release_workers;

__attribute__((noinline)) void contended(uint64_t value) {
    atomic_fetch_add_explicit(&total, value, memory_order_relaxed);
}

static void *worker(void *argument) {
    (void)argument;
    atomic_fetch_add_explicit(&workers_ready, 1, memory_order_release);
    while (atomic_load_explicit(&release_workers, memory_order_acquire) == 0) {
        sched_yield();
    }
    for (uint64_t call = 1; call <= CALLS; ++call) {
        contended(call);
    }
    return NULL;
}

int main(void) {
    pthread_t workers[WORKERS];
    for (int index = 0; index < WORKERS; ++index) {
        if (pthread_create(&workers[index], NULL, worker, NULL) != 0) {
            return 2;
        }
    }
    while (atomic_load_explicit(&workers_ready, memory_order_acquire) != WORKERS) {
        sched_yield();
    }
    atomic_store_explicit(&release_workers, 1, memory_order_release);
    for (int index = 0; index < WORKERS; ++index) {
        if (pthread_join(workers[index], NULL) != 0) {
            return 3;
        }
    }
    uint64_t expected = (uint64_t)WORKERS * CALLS * (CALLS + 1) / 2;
    return atomic_load_explicit(&total, memory_order_relaxed) == expected ? 0 : 4;
}
