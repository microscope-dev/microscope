// The keys test stops here and evaluates its caller's expressions.
extern "C" __attribute__((noinline)) void barrier(void *fixture) {
    __asm__ volatile("" : : "r"(fixture) : "memory");
}
