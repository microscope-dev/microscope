
#include <stdint.h>

typedef void (*symbols_callback)(void);



#if defined(__clang__)
#define NO_CLONE __attribute__((noinline))
#else
#define NO_CLONE __attribute__((noipa))
#endif


NO_CLONE static int32_t lib_static_helper(symbols_callback callback, volatile int32_t *sink) {
    *sink += 1;
    callback();
    return *sink + 2;
}

__attribute__((visibility("default"), noinline)) int32_t lib_exported_entry(
    symbols_callback callback) {
    volatile int32_t sink = 0;
    int32_t result = lib_static_helper(callback, &sink);
    return result + sink;
}

__attribute__((visibility("default"), noinline)) void lib_fault(void) {
    __builtin_trap();
}
