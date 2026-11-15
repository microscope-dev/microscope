




#include <stdint.h>
#include <stdlib.h>
#include <string.h>

struct tls_addresses {
    volatile int32_t *main;
    volatile int64_t *zero;
    volatile int32_t *library;
};

_Thread_local volatile int32_t main_tls = 100;
_Thread_local volatile int64_t main_zero_tls;
extern _Thread_local volatile int32_t library_tls;

struct tls_addresses tls_addresses[1];

static int aborting;

__attribute__((noinline)) static void tls_stop(void) {
    if (aborting) {
        abort();
    }
}

int main(int argc, char **argv) {
    aborting = argc > 1 && strcmp(argv[1], "abort") == 0;
    main_zero_tls = 200;
    tls_addresses[0].main = &main_tls;
    tls_addresses[0].zero = &main_zero_tls;
    tls_addresses[0].library = &library_tls;
    tls_stop();
    return 0;
}
