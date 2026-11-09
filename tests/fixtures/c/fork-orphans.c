// Forks eight children and exits without waiting for them. Each child runs
// shared_work, which may hold a trap it inherited, and then creates the
// file argv[1]/<n>: a child killed by a trap it kept never does.

#include <fcntl.h>
#include <stdio.h>
#include <unistd.h>

#define CHILDREN 8

volatile int work_done;

__attribute__((noinline)) void shared_work(void) {
    work_done += 1;
}

int main(int argc, char **argv) {
    if (argc != 2) {
        return 1;
    }
    for (int n = 0; n < CHILDREN; n++) {
        pid_t child = fork();
        if (child < 0) {
            return 2;
        }
        if (child == 0) {
            shared_work();
            char path[4096];
            snprintf(path, sizeof(path), "%s/%d", argv[1], n);
            _exit(open(path, O_CREAT | O_WRONLY, 0600) < 0 ? 3 : 0);
        }
    }
    return 0;
}
