






#define _GNU_SOURCE
#include <elf.h>
#include <stdio.h>
#include <string.h>
#include <sys/auxv.h>
#include <sys/mman.h>
#include <time.h>


#define UNMAPPED ((void *)16)




long vdso_depth = 42;
struct timespec vdso_clock_value;
time_t vdso_time_value;

__attribute__((noinline)) long vdso_clock(struct timespec *destination) {
    long depth = vdso_depth;
    int status = clock_gettime(CLOCK_MONOTONIC_COARSE, destination);
    return status + depth;
}

__attribute__((noinline)) long vdso_time(time_t *destination) {
    long depth = vdso_depth;
    long now = (long)time(destination);
    return now + depth;
}


static int vdso_mapping(unsigned long *start, unsigned long *end) {
    FILE *maps = fopen("/proc/self/maps", "r");
    if (maps == NULL) {
        return 0;
    }
    char line[512];
    int found = 0;
    while (!found && fgets(line, sizeof line, maps) != NULL) {
        found = strstr(line, "[vdso]") != NULL && sscanf(line, "%lx-%lx", start, end) == 2;
    }
    fclose(maps);
    return found;
}



static unsigned long vdso_offset(const char *name) {
    const unsigned char *image = (const unsigned char *)getauxval(AT_SYSINFO_EHDR);
    const Elf64_Ehdr *header = (const Elf64_Ehdr *)image;
    const Elf64_Shdr *sections = (const Elf64_Shdr *)(image + header->e_shoff);
    for (int index = 0; index < header->e_shnum; index++) {
        if (sections[index].sh_type != SHT_DYNSYM) {
            continue;
        }
        const Elf64_Sym *symbols = (const Elf64_Sym *)(image + sections[index].sh_offset);
        const char *names = (const char *)(image + sections[sections[index].sh_link].sh_offset);
        size_t count = sections[index].sh_size / sizeof *symbols;
        for (size_t symbol = 0; symbol < count; symbol++) {
            if (strcmp(names + symbols[symbol].st_name, name) == 0) {
                return symbols[symbol].st_value;
            }
        }
    }
    return 0;
}

typedef long (*getcpu_function)(unsigned *cpu, unsigned *node, void *cache);



__attribute__((noinline)) long vdso_getcpu(unsigned long base, unsigned long offset) {
    unsigned cpu = 0;
    return ((getcpu_function)(base + offset))(&cpu, NULL, NULL);
}

__attribute__((noinline)) void vdso_moved(void *address) {
    __asm__ volatile("" : : "r"(address) : "memory");
}

__attribute__((noinline)) void vdso_unmapped(void) {
    __asm__ volatile("" : : : "memory");
}



static int move_vdso(void) {
    unsigned long start = 0;
    unsigned long end = 0;
    unsigned long getcpu = vdso_offset("__vdso_getcpu");
    if (!vdso_mapping(&start, &end) || getcpu == 0) {
        return 2;
    }
    if (vdso_getcpu(start, getcpu) != 0) {
        return 3;
    }
    size_t size = end - start;
    void *target = mmap(NULL, size, PROT_NONE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (target == MAP_FAILED ||
        mremap((void *)start, size, size, MREMAP_MAYMOVE | MREMAP_FIXED, target) != target) {
        return 4;
    }
    vdso_moved(target);
    if (vdso_getcpu((unsigned long)target, getcpu) != 0) {
        return 5;
    }
    if (munmap(target, size) != 0) {
        return 6;
    }
    vdso_unmapped();
    return 0;
}

__attribute__((noinline)) void vdso_replaced(void *address) {
    __asm__ volatile("" : : "r"(address) : "memory");
}



static int replace_vdso(void) {
    unsigned long start = 0;
    unsigned long end = 0;
    unsigned long getcpu = vdso_offset("__vdso_getcpu");
    if (!vdso_mapping(&start, &end) || getcpu == 0) {
        return 2;
    }
    if (vdso_getcpu(start, getcpu) != 0) {
        return 3;
    }
    size_t size = end - start;
    unsigned char *fresh = mmap((void *)start, size, PROT_READ | PROT_WRITE,
                                MAP_PRIVATE | MAP_ANONYMOUS | MAP_FIXED, -1, 0);
    if (fresh != (void *)start) {
        return 4;
    }
    memset(fresh, 0x90, size);
    vdso_replaced(fresh);
    for (size_t index = 0; index < size; index++) {
        if (fresh[index] != 0x90) {
            return 5;
        }
    }
    return 0;
}

int main(int argc, char **argv) {
    const char *mode = argc > 1 ? argv[1] : "clock";
    if (strcmp(mode, "time") == 0) {
        return vdso_time(UNMAPPED) == vdso_depth ? 0 : 1;
    }
    if (strcmp(mode, "calls") == 0) {
        long now = vdso_time(&vdso_time_value);
        long clock = vdso_clock(&vdso_clock_value);
        return now > vdso_depth && clock == vdso_depth ? 0 : 1;
    }
    if (strcmp(mode, "move") == 0) {
        return move_vdso();
    }
    if (strcmp(mode, "replace") == 0) {
        return replace_vdso();
    }
    return vdso_clock(UNMAPPED) == vdso_depth ? 0 : 1;
}
