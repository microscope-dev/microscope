



#define _GNU_SOURCE
#include <errno.h>
#include <sys/prctl.h>
#include <unistd.h>

void attach_restart_wait(void);

__asm__(".text\n"
        ".globl attach_restart_wait\n"
        ".type attach_restart_wait, @function\n"
        "attach_restart_wait:\n"
        "    movl $34, %eax\n"
        "    syscall\n"
        "attach_restart_jump:\n"
        "    jmp *%rax\n"
        ".size attach_restart_wait, . - attach_restart_wait\n");

int main(void) {
    static const char ready[] = "READY\n";
    if ((prctl(PR_SET_PTRACER, PR_SET_PTRACER_ANY) == -1 && errno != EINVAL)) {
        return 1;
    }
    if (write(STDOUT_FILENO, ready, sizeof(ready) - 1) != (ssize_t)(sizeof(ready) - 1)) {
        return 2;
    }
    attach_restart_wait();
    return 3;
}
