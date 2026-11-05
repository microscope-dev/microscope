// A program to attach to whose threads all sleep until a byte on its
// standard input releases them, then each calls attach_breakpoint. A
// debugger that releases it while it runs must stop every thread before it
// can lift a trap, so a release cut short leaves the trap for some thread
// to hit. Exits 23 once every thread has called attach_breakpoint.
#define _GNU_SOURCE
#include <pthread.h>
#include <stdint.h>
#include <sys/prctl.h>
#include <unistd.h>

enum { SLEEPERS = 64 };

static pthread_mutex_t lock = PTHREAD_MUTEX_INITIALIZER;
static pthread_cond_t changed = PTHREAD_COND_INITIALIZER;
static int sleeping;
static int released;

__attribute__((noinline)) static int attach_breakpoint(void) {
    return 23;
}

static void *sleeper(void *unused) {
    (void)unused;
    pthread_mutex_lock(&lock);
    sleeping += 1;
    pthread_cond_broadcast(&changed);
    while (!released) {
        pthread_cond_wait(&changed, &lock);
    }
    pthread_mutex_unlock(&lock);
    return (void *)(intptr_t)attach_breakpoint();
}

int main(void) {
    static const char ready[] = "READY\n";
    pthread_t threads[SLEEPERS];
    char release;

    if (prctl(PR_SET_PTRACER, PR_SET_PTRACER_ANY) == -1) {
        return 1;
    }
    for (int index = 0; index < SLEEPERS; ++index) {
        if (pthread_create(&threads[index], NULL, sleeper, NULL) != 0) {
            return 2;
        }
    }

    pthread_mutex_lock(&lock);
    while (sleeping != SLEEPERS) {
        pthread_cond_wait(&changed, &lock);
    }
    pthread_mutex_unlock(&lock);

    if (write(STDOUT_FILENO, ready, sizeof(ready) - 1) != (ssize_t)(sizeof(ready) - 1)) {
        return 3;
    }
    if (read(STDIN_FILENO, &release, 1) != 1) {
        return 4;
    }

    pthread_mutex_lock(&lock);
    released = 1;
    pthread_cond_broadcast(&changed);
    pthread_mutex_unlock(&lock);

    for (int index = 0; index < SLEEPERS; ++index) {
        void *result;
        if (pthread_join(threads[index], &result) != 0 || result != (void *)23) {
            return 5;
        }
    }
    return attach_breakpoint();
}
