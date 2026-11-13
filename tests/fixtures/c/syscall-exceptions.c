// Makes GOOD getppid calls, FAILED close(-1) calls, which fail with EBADF,
// and SLOW nanosleeps of 30 ms, each from its own line: the calls a
// counting recording keeps whole are the failed and the slow ones.
#define _GNU_SOURCE
#include <stdlib.h>
#include <sys/syscall.h>
#include <time.h>
#include <unistd.h>

static __attribute__((noinline)) void good(void) { syscall(SYS_getppid); }

static __attribute__((noinline)) void failed(void) { syscall(SYS_close, -1); }

static __attribute__((noinline)) void slow(void) {
    struct timespec pause = {0, 30 * 1000 * 1000};
    nanosleep(&pause, NULL);
}

int main(int argc, char **argv) {
    long goods = argc > 1 ? atol(argv[1]) : 1000;
    long fails = argc > 2 ? atol(argv[2]) : 5;
    long slows = argc > 3 ? atol(argv[3]) : 3;
    for (long call = 0; call < goods; call++) {
        good();
    }
    for (long call = 0; call < fails; call++) {
        failed();
    }
    for (long call = 0; call < slows; call++) {
        slow();
    }
    return 0;
}
