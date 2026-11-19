#include <sys/wait.h>
#include <unistd.h>

enum { CALLS = 3 };

volatile unsigned long sink;

__attribute__((noinline)) void leaf(void) { sink++; }

__attribute__((noinline)) void outer(void) {
    for (int call = 0; call < CALLS; ++call) {
        leaf();
    }
    sink++;
}

int main(void) {
    static const char ready[] = "READY\n";
    char release;
    int go[2];
    if (pipe(go) != 0) {
        return 2;
    }

    pid_t child = fork();
    if (child < 0) {
        return 3;
    }
    if (child == 0) {
        close(go[1]);
        if (read(go[0], &release, 1) != 1) {
            _exit(4);
        }
        outer();
        _exit(0);
    }
    close(go[0]);

    if (write(STDOUT_FILENO, ready, sizeof ready - 1) != (ssize_t)(sizeof ready - 1)) {
        return 5;
    }
    if (read(STDIN_FILENO, &release, 1) != 1 || write(go[1], &release, 1) != 1) {
        return 6;
    }
    outer();

    int status;
    if (waitpid(child, &status, 0) != child) {
        return 7;
    }
    return WIFEXITED(status) ? WEXITSTATUS(status) : 8;
}
