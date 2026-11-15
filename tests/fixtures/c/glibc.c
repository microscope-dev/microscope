



#define _GNU_SOURCE
#include <pthread.h>

__attribute__((noinline)) void barrier(void *fixture) {
    __asm__ volatile("" : : "r"(fixture) : "memory");
}

int main(void) {
    pthread_mutex_t unlocked = PTHREAD_MUTEX_INITIALIZER; // VIEW: unlocked => unlocked
    pthread_mutex_t locked = PTHREAD_MUTEX_INITIALIZER;   // VIEW: locked => locked
    pthread_mutex_t reentered = PTHREAD_RECURSIVE_MUTEX_INITIALIZER_NP; // VIEW: reentered => locked
    pthread_mutex_t checked = PTHREAD_ERRORCHECK_MUTEX_INITIALIZER_NP; // VIEW: checked => unlocked
    pthread_mutex_t held = PTHREAD_ERRORCHECK_MUTEX_INITIALIZER_NP;    // VIEW: held => locked
    pthread_mutex_lock(&locked);
    pthread_mutex_lock(&held);
    pthread_mutex_lock(&reentered);
    pthread_mutex_lock(&reentered);
    barrier(&unlocked);
    pthread_mutex_unlock(&reentered);
    pthread_mutex_unlock(&reentered);
    pthread_mutex_unlock(&locked);
    pthread_mutex_unlock(&held);
    barrier(&checked);
    return 0;
}
