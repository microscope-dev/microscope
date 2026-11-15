#include <stdint.h>



enum { CALLS = 40, SECOND_SITE_OFFSET = 1000 };

volatile uint64_t last_call;
volatile uint64_t shared_total;

__attribute__((noinline)) void counted(uint64_t call) {
    last_call = call;
}


static inline __attribute__((always_inline)) void shared(uint64_t value) {
    shared_total += value;
}

__attribute__((noinline)) void caller(uint64_t call) {
    counted(call);
    shared(call);
    shared(call + SECOND_SITE_OFFSET);
}

int main(void) {
    for (uint64_t call = 1; call <= CALLS; ++call) {
        caller(call);
    }
    uint64_t expected = CALLS * (CALLS + 1) + CALLS * SECOND_SITE_OFFSET;
    return last_call == CALLS && shared_total == expected ? 0 : 1;
}
