


#define _GNU_SOURCE
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>

static volatile sig_atomic_t handled;

__attribute__((noinline)) void handle_usr1(int signal_number) {
    (void)signal_number;
    handled = 1;
}

__attribute__((noinline)) void deliver(void) {
    raise(SIGUSR1);
}

int main(void) {
    stack_t alternate = {
        .ss_sp = malloc(SIGSTKSZ),
        .ss_size = SIGSTKSZ,
    };
    if (alternate.ss_sp == NULL || sigaltstack(&alternate, NULL) != 0) {
        return 1;
    }
    struct sigaction action = {.sa_handler = handle_usr1, .sa_flags = SA_ONSTACK};
    if (sigaction(SIGUSR1, &action, NULL) != 0) {
        return 2;
    }
    deliver();
    return handled == 1 ? 0 : 3;
}
