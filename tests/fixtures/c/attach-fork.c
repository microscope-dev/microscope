



#define _GNU_SOURCE
#include <signal.h>
#include <sys/prctl.h>
#include <sys/wait.h>
#include <unistd.h>

volatile int child_ran;
volatile sig_atomic_t continued;

static void on_continue(int signal) {
    (void)signal;
    continued = 1;
}

__attribute__((noinline)) int child_work(void) {
    child_ran += 1;
    return child_ran == 1 ? 7 : 8;
}

int main(void) {
    static const char ready[] = "READY\n";
    char release;
    struct sigaction action = {.sa_handler = on_continue};

    if (prctl(PR_SET_PTRACER, PR_SET_PTRACER_ANY) == -1 || sigaction(SIGCONT, &action, NULL) == -1) {
        return 1;
    }
    if (write(STDOUT_FILENO, ready, sizeof(ready) - 1) != (ssize_t)(sizeof(ready) - 1)) {
        return 2;
    }
    if (read(STDIN_FILENO, &release, 1) != 1) {
        return 3;
    }
    pid_t child = fork();
    if (child < 0) {
        return 4;
    }
    if (child == 0) {
        _exit(child_work() + (continued ? 10 : 0));
    }
    int status;
    if (waitpid(child, &status, 0) != child || read(STDIN_FILENO, &release, 1) != 1) {
        return 5;
    }
    return WIFEXITED(status) ? WEXITSTATUS(status) : 6;
}
