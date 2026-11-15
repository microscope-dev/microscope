


#define _POSIX_C_SOURCE 200809L

#include <signal.h>
#include <stddef.h>
#include <string.h>

static volatile sig_atomic_t terminate_requested;
volatile int terminate_loops;

static void request_termination(int signal) {
    (void)signal;
    terminate_requested = 1;
}

__attribute__((noinline)) void terminate_tick(void) {
    terminate_loops += 1;
}

int main(int argc, char **argv) {
    struct sigaction action = {.sa_handler = request_termination};
    sigemptyset(&action.sa_mask);
    if (sigaction(SIGTERM, &action, NULL) != 0) {
        return 100;
    }
    while (!terminate_requested) {
        terminate_tick();
    }
    if (argc > 1 && strcmp(argv[1], "reraise") == 0) {
        signal(SIGTERM, SIG_DFL);
        raise(SIGTERM);
        return 101;
    }
    return 7;
}
