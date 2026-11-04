// Starts and joins many short threads, one after another, then calls
// burst_done: a long burst of thread events before one stop.
#include <pthread.h>

enum { THREADS = 400 };

static void *nothing(void *argument) {
    return argument;
}

__attribute__((noinline)) void burst_done(int threads) {
    (void)threads;
}

int main(void) {
    for (int index = 0; index < THREADS; ++index) {
        pthread_t thread;
        if (pthread_create(&thread, NULL, nothing, NULL) != 0 || pthread_join(thread, NULL) != 0) {
            return 1;
        }
    }

    burst_done(THREADS);
    return 0;
}
