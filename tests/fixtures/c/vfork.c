




#define _GNU_SOURCE
#include <stdlib.h>
#include <sys/wait.h>
#include <unistd.h>

__attribute__((noinline)) int child_work(int code) {
    return code;
}

int main(void) {
    int status = 0;
    pid_t child = vfork();
    if (child == 0) {
        _exit(child_work(7));
    }
    if (child < 0 || waitpid(child, &status, 0) != child) {
        return 1;
    }
    int exit_code = WIFEXITED(status) ? WEXITSTATUS(status) : -1;

    child_work(exit_code);

    int spawned = system("true");
    return WIFEXITED(spawned) && WEXITSTATUS(spawned) == 0 ? 0 : 2;
}
