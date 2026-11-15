


#include <stdio.h>
#include <string.h>
#include <sys/syscall.h>

volatile int status;

static __attribute__((always_inline)) inline void bump(void) {
    status += 1000; // jump: inlined
}

__attribute__((noinline)) static int checked(int value) {
    status = value; // jump: start
    status += 10; // jump: skipped
    status += 100; // jump: target
    if (status > 100000) {
        bump();
        bump();
    }
    return status; // jump: return
}

__attribute__((noinline)) static int elsewhere(void) {
    return 7; // jump: elsewhere
}



__attribute__((noinline)) static int wait_in_pause(void) {
    puts("waiting");
    fflush(stdout);
    long result;
    __asm__ volatile("syscall" : "=a"(result) : "a"((long)SYS_pause) : "rcx", "r11", "memory");
    if (result != 0) {
        return 4; // jump: interrupted
    }
    return 5; // jump: resumed
}

int main(int argc, char **argv) {
    if (argc > 1 && strcmp(argv[1], "wait") == 0) {
        return wait_in_pause();
    }
    int got = elsewhere(); // jump: call
    if (got != 7) {
        return got;
    }
    return checked(argc);
}
