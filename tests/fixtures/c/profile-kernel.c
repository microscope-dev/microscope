#include <fcntl.h>
#include <stdlib.h>
#include <unistd.h>

int main(int argc, char **argv) {
    unsigned long count = argc > 1 ? strtoul(argv[1], NULL, 10) : 2000000;
    int fd = open("/dev/zero", O_RDONLY);
    if (fd < 0) {
        return 1;
    }
    char byte;
    for (unsigned long i = 0; i < count; ++i) {
        if (read(fd, &byte, 1) != 1) {
            close(fd);
            return 2;
        }
    }
    return close(fd) != 0;
}
