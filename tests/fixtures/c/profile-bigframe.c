#include <stdlib.h>
#include <string.h>

volatile unsigned long sink;

__attribute__((noinline)) unsigned long leaf(unsigned long n) {
    unsigned long sum = 0;
    for (unsigned long i = 0; i < n; ++i) {
        sum += i * i;
    }
    return sum;
}



int main(int argc, char **argv) {
    volatile char frame[24 * 1024];
    memset((char *)frame, 1, sizeof frame);
    int count = argc > 1 ? atoi(argv[1]) : 40;
    for (int i = 0; i < count; ++i) {
        sink += leaf(4000000 + i) + frame[i % sizeof frame];
    }
    return 0;
}
