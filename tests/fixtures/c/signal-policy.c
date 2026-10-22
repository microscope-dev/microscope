// Raises signals whose handling the debugger's signal policy decides. The
// exit status has one bit per signal whose handler ran.
#define _GNU_SOURCE

#include <signal.h>
#include <stddef.h>
#include <sys/wait.h>
#include <unistd.h>

static volatile sig_atomic_t handled;

static void handler(int signal) {
    if (signal == SIGUSR1) {
        handled |= 1;
    } else if (signal == SIGALRM) {
        handled |= 2;
    } else if (signal == SIGURG) {
        handled |= 4;
    } else if (signal == SIGCHLD) {
        handled |= 8;
    } else if (signal == SIGWINCH) {
        handled |= 16;
    } else if (signal == SIGRTMIN + 1) {
        handled |= 32;
    }
}

int main(void) {
    int signals[] = {SIGUSR1, SIGALRM, SIGURG, SIGCHLD, SIGWINCH, SIGRTMIN + 1};
    struct sigaction action = {.sa_handler = handler};
    sigset_t unblocked;
    sigemptyset(&action.sa_mask);
    sigemptyset(&unblocked);
    for (size_t index = 0; index < sizeof signals / sizeof signals[0]; ++index) {
        if (sigaction(signals[index], &action, NULL) != 0 ||
            sigaddset(&unblocked, signals[index]) != 0) {
            return 100;
        }
    }
    // An inherited mask must not keep the fixture's signals pending.
    if (sigprocmask(SIG_UNBLOCK, &unblocked, NULL) != 0) {
        return 100;
    }
    raise(SIGUSR1);
    raise(SIGALRM);
    raise(SIGURG);
    pid_t child = fork();
    if (child == 0) {
        _exit(0);
    }
    if (child < 0 || waitpid(child, NULL, 0) != child) {
        return 101;
    }
    raise(SIGWINCH);
    raise(SIGRTMIN + 1);
    return handled;
}
