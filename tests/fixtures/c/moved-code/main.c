

#define _GNU_SOURCE
#include <stdio.h>
#include <string.h>
#include <sys/mman.h>
#include <unistd.h>

int moved_answer(int value);


static int code_mapping(unsigned long *start, unsigned long *end) {
    FILE *maps = fopen("/proc/self/maps", "r");
    if (maps == NULL) {
        return 0;
    }
    char line[512];
    char permissions[5];
    int found = 0;
    while (!found && fgets(line, sizeof line, maps) != NULL) {
        found = strstr(line, "libmoved-code.so") != NULL &&
                sscanf(line, "%lx-%lx %4s", start, end, permissions) == 3 &&
                permissions[2] == 'x';
    }
    fclose(maps);
    return found;
}

int main(void) {
    unsigned long start = 0;
    unsigned long end = 0;
    if (!code_mapping(&start, &end) || moved_answer(20) != 41) {
        return 2;
    }
    size_t size = end - start;
    char *target = mmap(NULL, size, PROT_NONE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (target == MAP_FAILED ||
        mremap((void *)start, size, size, MREMAP_MAYMOVE | MREMAP_FIXED, target) != target) {
        return 3;
    }
    int (*moved)(int) = (int (*)(int))(target + ((unsigned long)&moved_answer - start));

    _exit(moved(20) == 41 ? 0 : 4);
}
