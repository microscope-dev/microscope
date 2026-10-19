// Code written at run time, as a just-in-time compiler writes it, that a
// profiler test samples: main calls a function it wrote to anonymous
// memory, which calls spin, which spins until the recorder ends the
// program. The function is written, then moved with mremap, which no perf
// event reports: a recording sees the memory mapped only where it was.
// The program's perf map names the function where it runs.
#define _GNU_SOURCE
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/resource.h>
#include <unistd.h>

__attribute__((noinline)) void spin(void) {
    for (;;) {
        __asm__ volatile("" ::: "memory");
    }
}

int main(void) {
    // Bounds a run by hand; the recorder ends the program much earlier.
    struct rlimit cpu = {.rlim_cur = 60, .rlim_max = 60};
    setrlimit(RLIMIT_CPU, &cpu);

    // push rbp; mov rbp, rsp; movabs rax, spin; call rax; pop rbp; ret
    unsigned char code[] = {
        0x55, 0x48, 0x89, 0xe5, 0x48, 0xb8, 0, 0, 0, 0, 0, 0, 0, 0, 0xff, 0xd0, 0x5d, 0xc3,
    };
    uint64_t target = (uint64_t)(uintptr_t)spin;
    memcpy(&code[6], &target, sizeof target);

    long page = sysconf(_SC_PAGESIZE);
    unsigned char *written = mmap(NULL, page, PROT_READ | PROT_WRITE | PROT_EXEC,
                                  MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    // Memory nothing can run, which no perf event reports either, holds
    // the place the code moves to.
    void *place = mmap(NULL, page, PROT_NONE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (written == MAP_FAILED || place == MAP_FAILED) {
        perror("mmap");
        return 1;
    }

    memcpy(written, code, sizeof code);
    void *moved = mremap(written, page, page, MREMAP_MAYMOVE | MREMAP_FIXED, place);
    if (moved == MAP_FAILED) {
        perror("mremap");
        return 1;
    }

    char path[64];
    snprintf(path, sizeof path, "/tmp/perf-%d.map", getpid());
    FILE *map = fopen(path, "w");
    if (map == NULL) {
        perror(path);
        return 1;
    }

    fprintf(map, "%lx %zx jitted_call\n", (unsigned long)(uintptr_t)moved, sizeof code);
    fclose(map);

    void (*jitted)(void) = (void (*)(void))moved;
    jitted();
    return 0;
}
