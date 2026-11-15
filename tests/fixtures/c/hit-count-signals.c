#include <pthread.h>
#include <sched.h>
#include <signal.h>
#include <stdatomic.h>
#include <stdint.h>




enum { WORKERS = 3, SIGNALS = 24, CALLS_PER_SIGNAL = 20 };

static _Atomic uint64_t calls;
static _Atomic int handled;
static _Atomic int workers_ready;
static _Atomic int stop_workers;

volatile uint64_t finished_calls;

__attribute__((noinline)) void contended(void) {
    atomic_fetch_add_explicit(&calls, 1, memory_order_relaxed);
}

__attribute__((noinline)) void finished(uint64_t total_calls) {
    finished_calls = total_calls;
}

static void on_signal(int signal) {
    (void)signal;
    atomic_fetch_add_explicit(&handled, 1, memory_order_relaxed);
}

static void *worker(void *argument) {
    (void)argument;
    atomic_fetch_add_explicit(&workers_ready, 1, memory_order_release);
    while (atomic_load_explicit(&stop_workers, memory_order_acquire) == 0) {
        contended();
    }
    return NULL;
}

int main(void) {
    struct sigaction action = {.sa_handler = on_signal};
    sigemptyset(&action.sa_mask);
    if (sigaction(SIGUSR1, &action, NULL) != 0) {
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
    for (int sent = 0; sent < SIGNALS; ++sent) {


        uint64_t target = (uint64_t)(sent + 1) * CALLS_PER_SIGNAL;
        while (atomic_load_explicit(&handled, memory_order_relaxed) != sent ||
               atomic_load_explicit(&calls, memory_order_relaxed) < target) {
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
    finished(atomic_load_explicit(&calls, memory_order_relaxed));
    return 0;
}
