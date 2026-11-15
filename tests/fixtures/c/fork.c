


#include <sys/wait.h>
#include <unistd.h>

volatile int work_done;

__attribute__((noinline)) void shared_work(void) {
    work_done += 1;
}

int main(void) {
    pid_t child = fork();
    if (child < 0) {
        return 2;
    }
    shared_work();
    if (child == 0) {
        _exit(work_done == 1 ? 0 : 3);
    }
    int status;
    if (waitpid(child, &status, 0) != child) {
        return 4;
    }
    return WIFEXITED(status) && WEXITSTATUS(status) == 0 ? 0 : 5;
}
