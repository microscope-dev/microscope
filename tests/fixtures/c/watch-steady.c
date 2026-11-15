#define _GNU_SOURCE
#include <pthread.h>
#include <sched.h>
#include <signal.h>
#include <stdatomic.h>
#include <stdint.h>
#include <sys/prctl.h>






enum { WORKERS = 3, SIGNALS = 12, STORES_PER_SIGNAL = 40 };

volatile uint64_t steady = 7;
_Atomic uint64_t stores;
volatile uint64_t finished_stores;

static _Atomic int handled;
static _Atomic int workers_ready;
static _Atomic int stop_workers;

__attribute__((noinline)) void store_steady(void) {
    steady = 7;
    atomic_fetch_add_explicit(&stores, 1, memory_order_relaxed);
}

static void *worker(void *argument) {
    (void)argument;
    atomic_fetch_add_explicit(&workers_ready, 1, memory_order_release);
    while (atomic_load_explicit(&stop_workers, memory_order_acquire) == 0) {
        store_steady();
    }
    return NULL;
}

__attribute__((noinline)) void finished(uint64_t total_stores) {
    finished_stores = total_stores;
    steady = 8;
}

static void on_signal(int signal) {
    (void)signal;
    atomic_fetch_add_explicit(&handled, 1, memory_order_relaxed);
}

int main(void) {
    struct sigaction action = {.sa_handler = on_signal};
    sigemptyset(&action.sa_mask);
    if (prctl(PR_SET_PTRACER, PR_SET_PTRACER_ANY) == -1 ||
        sigaction(SIGUSR1, &action, NULL) != 0) {
        return 2;
    }
    pthread_t workers[WORKERS];
    for (int index = 0; index < WORKERS; ++index) {
        if (pthread_create(&workers[index], NULL, worker, NULL) != 0) {
            return 3;
        }
    }
    while (atomic_load_explicit(&workers_ready, memory_order_acquire) != WORKERS) {
        sched_yield();
    }
#ifdef SPIN
    worker(NULL);
#endif
    for (int sent = 0; sent < SIGNALS; ++sent) {


        uint64_t target = (uint64_t)(sent + 1) * STORES_PER_SIGNAL;
        while (atomic_load_explicit(&handled, memory_order_relaxed) != sent ||
               atomic_load_explicit(&stores, memory_order_relaxed) < target) {
            sched_yield();
        }
        if (pthread_kill(workers[sent % WORKERS], SIGUSR1) != 0) {
            return 4;
        }
    }
    while (atomic_load_explicit(&handled, memory_order_relaxed) != SIGNALS) {
        sched_yield();
    }
    atomic_store_explicit(&stop_workers, 1, memory_order_release);
    for (int index = 0; index < WORKERS; ++index) {
        if (pthread_join(workers[index], NULL) != 0) {
            return 5;
        }
    }
    finished(atomic_load_explicit(&stores, memory_order_relaxed));
    return steady == 8 ? 0 : 6;
}
