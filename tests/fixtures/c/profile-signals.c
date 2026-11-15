



#define _POSIX_C_SOURCE 199309L
#include <signal.h>
#include <stdlib.h>

volatile sig_atomic_t handled;

__attribute__((noinline)) void on_signal(int number) {
    (void)number;
    handled = handled + 1;
}

__attribute__((noinline)) void interrupted(int calls) {
    for (int call = 0; call < calls; ++call) {
        raise(SIGUSR1);
    }
}

int main(int argc, char **argv) {
    int calls = argc > 1 ? atoi(argv[1]) : 1;
    struct sigaction action = {.sa_handler = on_signal};
    sigaction(SIGUSR1, &action, NULL);
    interrupted(calls);
    return handled == calls ? 0 : 1;
}
