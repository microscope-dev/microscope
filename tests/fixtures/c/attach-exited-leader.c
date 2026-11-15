#include <pthread.h>
#include <stdlib.h>
#include <unistd.h>

static __thread int worker_value;

static void *worker(void *unused) {
    static const char ready_message[] = "READY\n";
    char release;

    (void)unused;
    worker_value = 31;
    if (write(STDOUT_FILENO, ready_message, sizeof(ready_message) - 1) !=
        (ssize_t)(sizeof(ready_message) - 1)) {
        exit(3);
    }
    if (read(STDIN_FILENO, &release, 1) != 1) {
        exit(4);
    }
    exit(worker_value);
}



int main(void) {
    pthread_t thread;

    if (pthread_create(&thread, NULL, worker, NULL) != 0) {
        return 2;
    }
    pthread_exit(NULL);
}
