#define _GNU_SOURCE

#include <stddef.h>
#include <stdint.h>
#include <stdlib.h>
#include <sys/ptrace.h>
#include <sys/user.h>
#include <sys/wait.h>





int main(int argc, char **argv) {
    if (argc != 3) {
        return 2;
    }
    pid_t pid = (pid_t)strtol(argv[1], NULL, 10);
    uintptr_t address = (uintptr_t)strtoull(argv[2], NULL, 16);
    int status;

    if (ptrace(PTRACE_SEIZE, pid, NULL, NULL) != 0 ||
        ptrace(PTRACE_INTERRUPT, pid, NULL, NULL) != 0 || waitpid(pid, &status, __WALL) != pid) {
        return 3;
    }
    uint64_t control = 1 | 1u << 16 | 2u << 18;
    if (ptrace(PTRACE_POKEUSER, pid, (void *)offsetof(struct user, u_debugreg[0]),
               (void *)address) != 0 ||
        ptrace(PTRACE_POKEUSER, pid, (void *)offsetof(struct user, u_debugreg[7]),
               (void *)(uintptr_t)control) != 0) {
        return 4;
    }
    return 0;
}
