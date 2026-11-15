




#define _GNU_SOURCE
#include <pthread.h>
#include <sys/wait.h>
#include <unistd.h>

static int release[2];

__attribute__((noinline)) int child_work(int code) {
    return code;
}

__attribute__((noinline)) int thread_work(int value) {
    return value + 2;
}

static void *worker(void *argument) {
    char byte;
    (void)argument;
    if (read(release[0], &byte, 1) == 1) {
        thread_work(40);
    }
    return NULL;
}

int main(void) {
    pthread_t thread;
    if (pipe(release) != 0 || pthread_create(&thread, NULL, worker, NULL) != 0) {
        return 1;
    }
    int status = 0;
    pid_t child = vfork();
    if (child == 0) {
        _exit(child_work(7));
    }
    if (child < 0 || waitpid(child, &status, 0) != child) {
        return 2;
    }
    if (write(release[1], "g", 1) != 1) {
        return 3;
    }
    pthread_join(thread, NULL);
    return WIFEXITED(status) && WEXITSTATUS(status) == 7 ? 0 : 4;
}
