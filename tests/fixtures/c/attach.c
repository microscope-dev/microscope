#define _GNU_SOURCE
#include <errno.h>
#include <stdint.h>
#include <sys/prctl.h>
#include <unistd.h>

volatile uint64_t attach_value = UINT64_C(0x1122334455667788);

__attribute__((noinline)) static int attach_breakpoint(void) {
    return attach_value == UINT64_C(0x1122334455667788) ? 23 : 24;
}

int main(void) {
    static const char ready[] = "READY\n";
    char release;

    if ((prctl(PR_SET_PTRACER, PR_SET_PTRACER_ANY) == -1 && errno != EINVAL)) {
        return 1;
    }
    if (write(STDOUT_FILENO, ready, sizeof(ready) - 1) != (ssize_t)(sizeof(ready) - 1)) {
        return 2;
    }
    if (read(STDIN_FILENO, &release, 1) != 1) {
        return 3;
    }
    return attach_breakpoint();
}
