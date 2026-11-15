



#include <stdint.h>

__attribute__((noinline)) int leaf(volatile int *value, volatile int *more) {
    return *value + *more + 1;
}

__attribute__((noinline)) int realigned(int a, int b, int c, int d, int e, int f, int g, int h) {
    _Alignas(64) volatile int buffer[16];
    volatile int sized[a + 1];
    sized[0] = 0;
    buffer[0] = a + b + c + d + e + f + g + h;
    return leaf(&buffer[0], &sized[0]) + (int)((uintptr_t)buffer & 63);
}

int main(void) {
    volatile int local = 41;
    return realigned(local, 0, 0, 0, 0, 0, 0, 0) - local - 1;
}
