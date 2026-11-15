


#define _GNU_SOURCE
#include <sys/mman.h>
#include <unistd.h>

__attribute__((noinline)) int after_mapping(void) {
    return 7;
}

int main(void) {
    int fd = memfd_create("jit", 0);
    if (fd < 0 || ftruncate(fd, 4096) != 0) {
        return 2;
    }
    void *code = mmap(NULL, 4096, PROT_READ | PROT_EXEC, MAP_PRIVATE, fd, 0);
    if (code == MAP_FAILED) {
        return 3;
    }
    return after_mapping() == 7 ? 0 : 4;
}
