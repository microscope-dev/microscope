#include <pthread.h>
#include <setjmp.h>
#include <stdint.h>

volatile int64_t locals_sink;
static jmp_buf escape;

__attribute__((noinline)) void after_return(void) {
    locals_sink += 1;
}

__attribute__((noinline)) int leaf_local(int seed) {
    volatile int local = seed;
    local += 1;
    local += 2;
    return local;
}

__attribute__((noinline)) int recurse(int depth) {
    volatile int frame_value = depth;
    if (depth > 0) {
        frame_value += recurse(depth - 1);
    }
    frame_value += 100;
    return frame_value;
}

__attribute__((noinline)) int blocks(void) {
    int total = 0;
    {
        volatile int inner = 5;
        inner += 1;
        total += inner;
    }
    {
        volatile int reused = 7;
        reused += 1;
        total += reused;
    }
    return total;
}

__attribute__((noinline)) int tail_callee(int value) {
    volatile int scratch[8];
    for (int index = 0; index < 8; ++index) {
        scratch[index] = value + index;
    }
    return scratch[7];
}


__attribute__((noinline)) int tail_caller(int value) {
    volatile int mine = value;
    mine += 1;
    __attribute__((musttail)) return tail_callee(mine);
}

__attribute__((noinline)) void jumper(void) {
    volatile int doomed = 1;
    doomed += 1;
    longjmp(escape, 1);
}

__attribute__((noinline)) void after_longjmp(void) {
    locals_sink += 2;
}

static void *local_owner(void *argument) {
    (void)argument;
    volatile int owned = 3;
    owned += 1;
    return NULL;
}

__attribute__((noinline)) void after_owner_exit(void) {
    locals_sink += 3;
}

int main(void) {

    volatile int value = leaf_local(10);
    after_return();
    value += recurse(2);
    after_return();
    value += blocks();
    after_return();
    value += tail_caller(1);
    after_return();
    if (setjmp(escape) == 0) {
        jumper();
    }
    after_longjmp();
    pthread_t owner;
    if (pthread_create(&owner, NULL, local_owner, NULL) != 0) {
        return 2;
    }
    pthread_join(owner, NULL);
    after_owner_exit();


    return value == 13 + 303 + 14 + 9 ? 0 : 1;
}
