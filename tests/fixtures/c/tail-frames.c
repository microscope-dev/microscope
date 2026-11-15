


#include <stdio.h>




#if defined(__clang__)
#define OPAQUE __attribute__((noinline))
#else
#define OPAQUE __attribute__((noipa))
#endif

volatile int sink;

OPAQUE void reached(void) {
    __asm__ volatile("" ::: "memory");
}

OPAQUE int leaf(int value) {
    sink = value;
    reached(); // frames: leaf
    return sink + 1;
}


OPAQUE int middle(int value) {
    sink = value;
    return leaf(value * 2); // frames: middle
}

OPAQUE int top(int value) {
    sink = value;
    return middle(value + 1); // frames: top
}


OPAQUE int either(int value) {
    sink = value;
    if (value & 1) {
        return middle(value); // frames: either middle
    }
    return leaf(value + 3); // frames: either leaf
}

int main(void) {
    int total = top(5); // frames: call top
    total += either(8); // frames: call either
    printf("%d\n", total);
    return 0;
}
