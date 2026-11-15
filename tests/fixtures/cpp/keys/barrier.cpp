
extern "C" __attribute__((noinline)) void barrier(void *fixture) {
    __asm__ volatile("" : : "r"(fixture) : "memory");
}
