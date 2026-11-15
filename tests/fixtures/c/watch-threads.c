#define _GNU_SOURCE

#include <pthread.h>
#include <stdint.h>

enum { WORKERS = 2, ITERATIONS = 40, CHURN = 16 };


volatile uint64_t locked_counter;

volatile uint64_t racing_counter;
_Thread_local volatile int64_t tls_value;
volatile int64_t *volatile first_worker_tls;

static pthread_mutex_t lock = PTHREAD_MUTEX_INITIALIZER;
static pthread_barrier_t tls_published;
static pthread_barrier_t tls_written;

__attribute__((noinline)) void before_threads(void) {
    racing_counter = 0;
}

__attribute__((noinline)) void tls_ready(void) {
    tls_value = 2;
}

__attribute__((noinline)) void after_join(void) {
    locked_counter += 0;
}

static void *counting_worker(void *argument) {
    (void)argument;
    for (int iteration = 0; iteration < ITERATIONS; ++iteration) {
        pthread_mutex_lock(&lock);
        locked_counter += 1;
        pthread_mutex_unlock(&lock);
        racing_counter += 1;
    }
    return NULL;
}

static void *churn_worker(void *argument) {
    (void)argument;
    pthread_mutex_lock(&lock);
    locked_counter += 1;
    pthread_mutex_unlock(&lock);
    return NULL;
}

static void *tls_worker(void *argument) {
    int first = argument != NULL;
    tls_value = first ? 1 : 10;
    if (first) {
        first_worker_tls = &tls_value;
        tls_ready();
    }
    pthread_barrier_wait(&tls_published);
    tls_value = first ? 3 : 30;
    pthread_barrier_wait(&tls_written);
    return NULL;
}

int main(void) {
    before_threads();
    pthread_t workers[WORKERS];
    for (int index = 0; index < WORKERS; ++index) {
        if (pthread_create(&workers[index], NULL, counting_worker, NULL) != 0) {
            return 2;
        }
    }
    for (int index = 0; index < WORKERS; ++index) {
        pthread_join(workers[index], NULL);
    }
    for (int index = 0; index < CHURN; ++index) {
        pthread_t thread;
        if (pthread_create(&thread, NULL, churn_worker, NULL) != 0) {
            return 3;
        }
        pthread_join(thread, NULL);
    }

    if (pthread_barrier_init(&tls_published, NULL, 3) != 0 ||
        pthread_barrier_init(&tls_written, NULL, 3) != 0) {
        return 4;
    }
    pthread_t first;
    pthread_t second;
    if (pthread_create(&first, NULL, tls_worker, (void *)1) != 0 ||
        pthread_create(&second, NULL, tls_worker, NULL) != 0) {
        return 5;
    }
    pthread_barrier_wait(&tls_published);
    *first_worker_tls = 5;
    pthread_barrier_wait(&tls_written);
    pthread_join(first, NULL);
    pthread_join(second, NULL);
    after_join();
    return locked_counter == WORKERS * ITERATIONS + CHURN ? 0 : 1;
}
