



#include <pthread.h>
#include <stdlib.h>
#include <unistd.h>

enum { MOST_THREADS = 64, UNIT = 4000000 };

volatile unsigned long sink;

__attribute__((noinline)) unsigned long leaf(unsigned long rounds) {
    unsigned long total = 0;
    for (unsigned long index = 0; index < rounds; ++index) {
        total += index ^ (total >> 3);
    }
    return total;
}

__attribute__((noinline)) unsigned long middle(unsigned long rounds) {
    return leaf(rounds / 2) + leaf(rounds / 2);
}

__attribute__((noinline)) void unit(void) {
    sink += middle(UNIT);
}

static void *work(void *unused) {
    (void)unused;

    do {
        unit();
    } while (write(STDOUT_FILENO, "u\n", 2) == 2);
    return NULL;
}

int main(int argc, char **argv) {
    int threads = argc > 1 ? atoi(argv[1]) : 4;
    if (threads < 1 || threads > MOST_THREADS) {
        return 2;
    }
    pthread_t workers[MOST_THREADS];
    for (int index = 1; index < threads; ++index) {
        if (pthread_create(&workers[index], NULL, work, NULL) != 0) {
            return 1;
        }
    }
    work(NULL);
    return 0;
}
