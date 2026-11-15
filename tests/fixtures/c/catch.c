





#define _GNU_SOURCE
#include <fcntl.h>
#include <pthread.h>
#include <string.h>
#include <sys/syscall.h>
#include <unistd.h>

#ifndef SPIN
enum { THREADS = 4, CALLS = 1000 };

static long raw_getppid(void) {
    long result;
    __asm__ volatile(".globl raw_syscall\nraw_syscall:\n\tsyscall"
                     : "=a"(result)
                     : "a"((long)SYS_getppid)
                     : "rcx", "r11", "memory");
    return result;
}

static pthread_barrier_t start;

static void *calls(void *unused) {
    (void)unused;

    pthread_barrier_wait(&start);
    for (int call = 0; call < CALLS; call++) {
        syscall(SYS_getppid);
    }
    return NULL;
}
#endif

int main(int argc, char **argv) {
#ifdef SPIN
    (void)argc;
    (void)argv;
    for (;;) {
        syscall(SYS_getppid);
    }
#else
    if (argc > 1 && strcmp(argv[1], "raw") == 0) {
        long raw = raw_getppid();
        return raw == getppid() ? 0 : 3;
    }
    if (argc > 1 && strcmp(argv[1], "threads") == 0) {
        pthread_t threads[THREADS];
        pthread_barrier_init(&start, NULL, THREADS);
        for (int thread = 0; thread < THREADS; thread++) {
            pthread_create(&threads[thread], NULL, calls, NULL);
        }
        for (int thread = 0; thread < THREADS; thread++) {
            pthread_join(threads[thread], NULL);
        }
        return 0;
    }
    if (openat(AT_FDCWD, "/nonexistent/microscope-catch", O_RDONLY) != -1) {
        return 1;
    }
    if (write(2, "to stderr\n", 10) != 10 || write(1, "to stdout\n", 10) != 10) {
        return 2;
    }
    return 0;
#endif
}
