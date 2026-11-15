




#define _GNU_SOURCE
#include <stdlib.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <unistd.h>

int main(int argc, char **argv) {
    for (int index = 1; index < argc; index++) {
        long number = strtol(argv[index], NULL, 10);
        pid_t child = fork();
        if (child == 0) {
            syscall(number, -1L, -1L, -1L, -1L, -1L, -1L);
            _exit(0);
        }
        int status;
        waitpid(child, &status, 0);
    }
    return 0;
}
