



#include <stdio.h>

int main(void) {
    puts("before");
    __asm__ volatile("int3");
    puts("after");
    return 0;
}
