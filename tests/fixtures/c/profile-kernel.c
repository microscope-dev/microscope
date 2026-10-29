#include <fcntl.h>
#include <stdlib.h>
#include <unistd.h>

int main(int argc, char **argv) {
    unsigned long count = argc > 1 ? strtoul(argv[1], NULL, 10) : 2000000;
    /* Large reads spend their CPU time in the kernel, not libc's syscall
     * wrapper. Keep one-byte reads as the short CLI workload's default. */
    static char bytes[65536];
    size_t size = argc > 2 ? strtoul(argv[2], NULL, 10) : 1;
    if (size == 0 || size > sizeof(bytes)) {
        return 1;
    }
    int fd = open("/dev/zero", O_RDONLY);
    if (fd < 0) {
        return 1;
    }
    for (unsigned long i = 0; i < count; ++i) {
        if (read(fd, bytes, size) != (ssize_t)size) {
            close(fd);
            return 2;
        }
    }
    return close(fd) != 0;
}
