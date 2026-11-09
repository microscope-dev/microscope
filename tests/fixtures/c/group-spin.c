// Forks CHILDREN children, and then every process spins without end, so a
// group has several members running at once to stop and resume together.
#include <unistd.h>

enum { CHILDREN = 4 };

volatile unsigned long spins;

__attribute__((noinline)) void spin(void) {
    for (;;) {
        spins += 1;
    }
}

int main(void) {
    for (int child = 0; child < CHILDREN; ++child) {
        if (fork() == 0) {
            break;
        }
    }
    spin();
}
