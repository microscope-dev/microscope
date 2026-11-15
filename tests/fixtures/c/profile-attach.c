




#include <pthread.h>
#include <sched.h>
#include <stdatomic.h>
#include <unistd.h>

enum { CALLS = 3 };

static _Atomic int released;
static _Atomic int waiting;
volatile unsigned long sink;

__attribute__((noinline)) void leaf(void) { sink++; }

__attribute__((noinline)) void middle(void) {
    leaf();
    sink++;
}

__attribute__((noinline)) void outer(void) {
    for (int call = 0; call < CALLS; ++call) {
        middle();
    }
    sink++;
}

static void *worker(void *unused) {
    (void)unused;
    atomic_fetch_add_explicit(&waiting, 1, memory_order_release);
    while (atomic_load_explicit(&released, memory_order_acquire) == 0) {
        sched_yield();
    }
    outer();
    return NULL;
}

int main(void) {
    pthread_t early;
    pthread_t late;
    char release;
    static const char ready[] = "READY\n";
    if (pthread_create(&early, NULL, worker, NULL) != 0) {
        return 2;
    }
    while (atomic_load_explicit(&waiting, memory_order_acquire) != 1) {
        sched_yield();
    }
    if (write(STDOUT_FILENO, ready, sizeof ready - 1) != (ssize_t)(sizeof ready - 1)) {
        return 3;
    }
    if (read(STDIN_FILENO, &release, 1) != 1) {
        return 4;
    }
    atomic_store_explicit(&released, 1, memory_order_release);
    if (pthread_create(&late, NULL, worker, NULL) != 0) {
        return 5;
    }
    outer();
    if (pthread_join(early, NULL) != 0 || pthread_join(late, NULL) != 0) {
        return 6;
    }
    return 0;
}
