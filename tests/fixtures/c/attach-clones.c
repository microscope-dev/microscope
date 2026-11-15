#define _GNU_SOURCE
#include <errno.h>
#include <pthread.h>
#include <semaphore.h>
#include <stdlib.h>
#include <sys/prctl.h>





enum { LIVE_WORKERS = 128 };

struct worker {
    sem_t release;
};

static void *worker(void *argument) {
    struct worker *self = argument;
    while (sem_wait(&self->release) != 0 && errno == EINTR) {
    }
    sem_destroy(&self->release);
    free(self);
    return NULL;
}

int main(void) {
    if (prctl(PR_SET_PTRACER, PR_SET_PTRACER_ANY) == -1) {
        return 1;
    }
    pthread_attr_t attributes;
    if (pthread_attr_init(&attributes) != 0 ||
        pthread_attr_setdetachstate(&attributes, PTHREAD_CREATE_DETACHED) != 0 ||
        pthread_attr_setstacksize(&attributes, 64 * 1024) != 0) {
        return 2;
    }
    struct worker *workers[LIVE_WORKERS] = {0};
    for (int slot = 0;; slot = (slot + 1) % LIVE_WORKERS) {
        if (workers[slot] != NULL && sem_post(&workers[slot]->release) != 0) {
            return 3;
        }
        workers[slot] = malloc(sizeof(*workers[slot]));
        if (workers[slot] == NULL || sem_init(&workers[slot]->release, 0, 0) != 0) {
            return 4;
        }
        pthread_t thread;
        if (pthread_create(&thread, &attributes, worker, workers[slot]) != 0) {
            return 5;
        }
    }
}
