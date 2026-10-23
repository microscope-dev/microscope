// Copies a function's machine code into anonymous memory, as V8 copies its
// embedded builtins, and calls the copy, which calls back into the program.
// With an argument, the callback aborts, leaving a core whose stack runs
// through the copy.
#define _GNU_SOURCE
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <unistd.h>

enum { COPIED_BYTES = 256 };

static volatile int abort_in_callback;

__attribute__((noinline)) int copy_callback(int value) {
    if (abort_in_callback) {
        abort();
    }
    return value + 1;
}

// Begins a page, as code a runtime copies whole does, and reaches the rest
// of the program only through its argument, so it runs at any address.
__attribute__((noinline, aligned(4096))) int copied_twice(int (*callback)(int), int value) {
    return callback(value) * 2;
}

int main(int argc, char **argv) {
    (void)argv;
    abort_in_callback = argc > 1;
    long page = sysconf(_SC_PAGESIZE);
    unsigned char *copy =
        mmap(NULL, (size_t)page, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (copy == MAP_FAILED) {
        return 2;
    }
    memcpy(copy, (const void *)copied_twice, COPIED_BYTES);
    if (mprotect(copy, (size_t)page, PROT_READ | PROT_EXEC) != 0) {
        return 3;
    }
    int (*copied)(int (*)(int), int) = (int (*)(int (*)(int), int))(void *)copy;
    return copied(copy_callback, 20) == 42 ? 0 : 4;
}
