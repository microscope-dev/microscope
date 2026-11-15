





#include <pthread.h>
#include <stdlib.h>

enum { CALLS = 3, DEPTH = 4 };

volatile unsigned long sink;

__attribute__((noinline)) void leaf(unsigned long iterations) {
    for (unsigned long index = 0; index < iterations; ++index) {
        sink += index;
    }
}

__attribute__((noinline)) void middle(unsigned long iterations) {
    leaf(iterations);
    sink++;
}

__attribute__((noinline)) void outer(unsigned long iterations) {
    for (int call = 0; call < CALLS; ++call) {
        middle(iterations);
    }
    sink++;
}

__attribute__((noinline)) void descend(int depth, unsigned long iterations) {
    volatile char frame[256];
    for (unsigned index = 0; index < sizeof frame; ++index) {
        frame[index] = (char)depth;
    }
    if (depth > 0) {
        descend(depth - 1, iterations);
    } else {
        outer(iterations);
    }
    sink += (unsigned long)frame[depth];
}

static void *worker(void *argument) {
    outer((unsigned long)argument);
    return NULL;
}

int main(int argc, char **argv) {
    unsigned long iterations = argc > 1 ? strtoul(argv[1], NULL, 10) : 1;
    pthread_t thread;
    if (pthread_create(&thread, NULL, worker, (void *)iterations) != 0) {
        return 2;
    }
    descend(DEPTH, iterations);
    pthread_join(thread, NULL);
    return 0;
}
