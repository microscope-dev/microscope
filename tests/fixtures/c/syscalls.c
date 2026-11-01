// System calls for the syscall tracer's tests, one workload per mode. Paths
// are relative to the working directory the test chooses.
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <sched.h>
#include <pthread.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <sys/uio.h>
#include <sys/wait.h>
#include <unistd.h>

static void files(void) {
    int fd = openat(AT_FDCWD, "present", O_CREAT | O_WRONLY | O_TRUNC | O_CLOEXEC, 0644);
    if (write(fd, "hello\n", 6) != 6) {
        _exit(10);
    }
    close(fd);
    if (openat(AT_FDCWD, "missing", O_RDONLY) != -1 || errno != ENOENT) {
        _exit(11);
    }
    fd = open("present", O_RDONLY);
    char buffer[16];
    if (read(fd, buffer, sizeof buffer) != 6) {
        _exit(12);
    }
    struct stat status;
    fstat(fd, &status);
    int pipes[2];
    pipe2(pipes, O_CLOEXEC);
    dup2(pipes[1], 9);
    close(pipes[0]);
    close(pipes[1]);
    close(9);
    close(fd);
    unlink("present");
}

static void *thread_main(void *argument) {
    (void)argument;
    syscall(SYS_getppid);
    return NULL;
}

static void family(void) {
    // SIGCHLD stays pending: delivered, it lands before or after wait4
    // returns as the child's exit races it, and two traces would differ.
    sigset_t blocked;
    sigemptyset(&blocked);
    sigaddset(&blocked, SIGCHLD);
    sigprocmask(SIG_BLOCK, &blocked, NULL);
    pid_t child = fork();
    if (child == 0) {
        syscall(SYS_write, 1, "child\n", 6);
        _exit(4);
    }
    int status;
    waitpid(child, &status, 0);
    pthread_t thread;
    pthread_create(&thread, NULL, thread_main, NULL);
    pthread_join(thread, NULL);
}

// Threads, then children, one after another, each gone at once: each one's
// first stop races the event of the call that made it.
static void churn(int count) {
    sigset_t blocked;
    sigemptyset(&blocked);
    sigaddset(&blocked, SIGCHLD);
    sigprocmask(SIG_BLOCK, &blocked, NULL);
    for (int made = 0; made < count; made++) {
        pthread_t thread;
        pthread_create(&thread, NULL, thread_main, NULL);
        pthread_join(thread, NULL);
    }
    for (int made = 0; made < count; made++) {
        pid_t child = fork();
        if (child == 0) {
            _exit(4);
        }
        int status;
        waitpid(child, &status, 0);
    }
}

static void block(void) {
    // The test sees this line, then the read blocked in flight, then
    // closes standard input.
    syscall(SYS_write, 1, "ready\n", 6);
    char byte;
    while (read(0, &byte, 1) > 0) {
    }
}

// A child alone in new user and pid namespaces, where it is pid 1, which
// starts a grandchild for each byte it reads until its input closes; each
// asks its pid and exits. The parent prints the child's pid as the host
// knows it, then waits for it.
static void namespaced(void) {
    if (unshare(CLONE_NEWUSER | CLONE_NEWPID) != 0) {
        _exit(20);
    }
    pid_t child = fork();
    if (child < 0) {
        _exit(21);
    }
    if (child == 0) {
        char byte;
        while (read(0, &byte, 1) > 0) {
            pid_t grandchild = fork();
            if (grandchild == 0) {
                syscall(SYS_getpid);
                _exit(0);
            }
            if (grandchild < 0 || waitpid(grandchild, NULL, 0) != grandchild) {
                _exit(23);
            }
        }
        _exit(0);
    }
    printf("child %d\n", child);
    fflush(stdout);
    int status;
    if (waitpid(child, &status, 0) != child || !WIFEXITED(status)) {
        _exit(22);
    }
}

// Asks for its parent's pid until a byte arrives on its input, then prints
// how many times it asked.
static void spin(void) {
    if (fcntl(0, F_SETFL, O_NONBLOCK) != 0) {
        _exit(30);
    }
    unsigned long asked = 0;
    char byte;
    while (read(0, &byte, 1) < 0 && errno == EAGAIN) {
        syscall(SYS_getppid);
        asked++;
    }
    printf("asked %lu\n", asked);
}

static volatile sig_atomic_t delivered;

static void handler(int signal) {
    (void)signal;
    delivered = 1;
}

static void signals(void) {
    struct sigaction action = {.sa_handler = handler};
    sigaction(SIGUSR1, &action, NULL);
    kill(getpid(), SIGUSR1);
    if (!delivered) {
        _exit(13);
    }
}

static void compat(void) {
    // getpid through the i386 ABI, then through x32's numbering, which
    // kernels without x32 support fail with ENOSYS.
    long result;
    __asm__ volatile("int $0x80" : "=a"(result) : "a"(20) : "memory");
    if (result != getpid()) {
        _exit(14);
    }
    __asm__ volatile("syscall" : "=a"(result) : "a"(0x40000000L | 39) : "rcx", "r11", "memory");
}

static void dumps(void) {
    // Data through a pipe, written whole and in parts, read whole and in
    // parts, for -e read= and -e write= to dump.
    int pipes[2];
    if (pipe(pipes) != 0) {
        _exit(15);
    }
    static const char text[] = "a dump of\tforty-odd bytes\x01\x02 over three lines\n";
    if (write(pipes[1], text, sizeof text - 1) != sizeof text - 1) {
        _exit(16);
    }
    char buffer[64];
    if (read(pipes[0], buffer, sizeof buffer) != sizeof text - 1) {
        _exit(17);
    }
    struct iovec parts[2] = {{(void *)"two ", 4}, {(void *)"parts\n", 6}};
    if (writev(pipes[1], parts, 2) != 10) {
        _exit(18);
    }
    struct iovec into[2] = {{buffer, 3}, {buffer + 3, 61}};
    if (readv(pipes[0], into, 2) != 10) {
        _exit(19);
    }
}

int main(int argc, char **argv) {
    const char *mode = argc > 1 ? argv[1] : "files";
    if (strcmp(mode, "files") == 0) {
        files();
        return 3;
    }
    if (strcmp(mode, "family") == 0) {
        family();
        return 0;
    }
    if (strcmp(mode, "churn") == 0 && argc > 2) {
        churn(atoi(argv[2]));
        return 0;
    }
    if (strcmp(mode, "block") == 0) {
        block();
        return 0;
    }
    if (strcmp(mode, "fails") == 0) {
        // A write, then a close that fails as nothing before it does.
        syscall(SYS_write, 1, "a\n", 2);
        return close(-1) == -1 && errno == EBADF ? 0 : 1;
    }
    if (strcmp(mode, "spin") == 0) {
        spin();
        return 0;
    }
    if (strcmp(mode, "namespaced") == 0) {
        namespaced();
        return 0;
    }
    if (strcmp(mode, "pause") == 0) {
        // Blocked until killed, for a live trace to show in flight.
        pause();
        return 0;
    }
    if (strcmp(mode, "signal") == 0) {
        signals();
        return 0;
    }
    if (strcmp(mode, "signal-term") == 0) {
        kill(getpid(), SIGTERM);
        return 15;
    }
    if (strcmp(mode, "signal-kill") == 0) {
        kill(getpid(), SIGKILL);
        return 15;
    }
    if (strcmp(mode, "signal-ignore") == 0) {
        struct sigaction action = {.sa_handler = SIG_IGN};
        if (sigaction(SIGUSR1, &action, NULL) != 0) return 24;
        return kill(getpid(), SIGUSR1) == 0 ? 0 : 25;
    }
    if (strcmp(mode, "compat") == 0) {
        compat();
        return 0;
    }
    if (strcmp(mode, "dumps") == 0) {
        dumps();
        return 0;
    }
    return 2;
}
