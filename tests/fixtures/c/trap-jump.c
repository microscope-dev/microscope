// A worker runs `step`, where tests stop it at a breakpoint, signal it and
// move it, then waits for the main thread, which releases it only once the
// worker has run on. A worker the debugger runs alone, holding the main
// thread stopped, never returns. The program exits with `status`, which
// shows which lines of `step` ran: 111 when all of them, 100 when a jump
// skipped the first two.

#include <pthread.h>
#include <sched.h>
#include <signal.h>
#include <stdatomic.h>

volatile int status;
volatile int handled;
static _Atomic int released;

__attribute__((noinline)) void in_handler(void) {
    handled += 1;
}

static void on_signal(int signal) {
    (void)signal;
    in_handler();
}

__attribute__((noinline)) static void step(void) {
    status += 1; // trap-jump: hit
    status += 10; // trap-jump: skipped
    status += 100; // trap-jump: target
}

static void *worker(void *argument) {
    (void)argument;
    step();
    while (atomic_load(&released) == 0) {
        sched_yield();
    }
    return NULL;
}

int main(void) {
    struct sigaction action = {.sa_handler = on_signal};
    sigemptyset(&action.sa_mask);
    pthread_t thread;
    if (sigaction(SIGUSR1, &action, NULL) != 0 ||
        pthread_create(&thread, NULL, worker, NULL) != 0) {
        return 1;
    }
    while (status == 0) {
        sched_yield();
    }
    atomic_store(&released, 1);
    pthread_join(thread, NULL);
    return status;
}
