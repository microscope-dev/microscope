


#include <stdint.h>
#include <string.h>

volatile int64_t frames_sink;

volatile int64_t frames_seed = 5;
volatile int frames_crash;
int64_t *volatile frames_poison;


#ifdef __clang__
#define FRAMES_OPAQUE __attribute__((noinline))
#else
#define FRAMES_OPAQUE __attribute__((noinline, noipa))
#endif

struct frames_pair {
    int64_t left;
    int64_t right;
};

FRAMES_OPAQUE int64_t frames_leaf(int64_t token) {
    int64_t leaf_local = token * 10;



    __asm__ volatile(
        "mov $0x5ca1ab1e, %%rbx\n\t"
        "mov $0x5ca1ab1e, %%r12\n\t"
        "mov $0x5ca1ab1e, %%r13\n\t"
        "mov $0x5ca1ab1e, %%r14\n\t"
        "mov $0x5ca1ab1e, %%r15\n\t"
        "mov $0x5ca1ab1e, %%rdi\n\t"
        "mov $0x5ca1ab1e, %%rsi\n\t"
        :
        :
        : "rbx", "r12", "r13", "r14", "r15", "rdi", "rsi", "memory");
#ifdef __OPTIMIZE__
    __asm__ volatile("mov $0x5ca1ab1e, %%rbp" : : : "rbp");
#endif
    frames_sink = leaf_local;
    if (frames_crash) {
        *frames_poison = leaf_local;
    }
    return leaf_local + 1;
}



FRAMES_OPAQUE int64_t frames_relay(int64_t value) {
    int64_t result = frames_leaf(value);

    __asm__ volatile("");
    return result;
}

static inline __attribute__((always_inline)) int64_t frames_inlined(int64_t base) {
    int64_t doubled = base * 2;
    int64_t result = frames_relay(doubled);
    return result + doubled;
}

FRAMES_OPAQUE int64_t frames_keep(int64_t seed) {



    int64_t kept = seed * 3 + 1;


    int64_t *kept_pointer = &kept;
    struct frames_pair pair = {.left = seed, .right = -seed};
    int64_t result = frames_inlined(seed + 1);
    frames_sink = pair.left + pair.right;
    return result + *kept_pointer;
}

FRAMES_OPAQUE int64_t frames_recurse(int64_t depth, int64_t seed) {
    int64_t level = depth * 100 + seed;
    if (depth == 0) {
        return frames_keep(seed) + level;
    }
    int64_t below = frames_recurse(depth - 1, seed);
    return below + level;
}

int main(int argc, char **argv) {
    frames_crash = argc > 1 && strcmp(argv[1], "crash") == 0;
    int64_t total = frames_recurse(3, frames_seed);
    frames_sink = total;
    return 0;
}
