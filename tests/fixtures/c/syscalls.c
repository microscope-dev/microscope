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

struct ending_group {
    pthread_barrier_t ready;
    const char *mode;
    pid_t leader;
};

static void await_leader_exit(pid_t leader) {
    char path[80], stat[512];
    snprintf(path, sizeof path, "/proc/self/task/%d/stat", leader);
    for (;;) {
        FILE *file = fopen(path, "r");
        if (!file || !fgets(stat, sizeof stat, file)) _exit(26);
        fclose(file);
        char *name_end = strrchr(stat, ')');
        if (name_end && name_end[2] == 'Z') return;
        sched_yield();
    }
}

static void *ending_worker(void *argument) {
    struct ending_group *group = argument;
    dprintf(1, "member %d %ld\n", group->leader, syscall(SYS_gettid));
    pthread_barrier_wait(&group->ready);
    if (strcmp(group->mode, "return") == 0) return NULL;
    if (strcmp(group->mode, "leader-first") == 0) {
        await_leader_exit(group->leader);
        syscall(SYS_exit, 37);
    }
    if (strcmp(group->mode, "leader-waits") == 0) {
        await_leader_exit(group->leader);
        dprintf(1, "leader gone\n");
        char byte;
        if (read(0, &byte, 1) != 1) syscall(SYS_exit_group, 29);
        syscall(SYS_getppid);
        syscall(SYS_exit, 37);
    }
    if (strcmp(group->mode, "parallel") == 0) syscall(SYS_exit, 37);
    if (strcmp(group->mode, "worker-group") == 0) syscall(SYS_exit_group, 37);
    for (;;) pause();
}

static void leader_waits(void) {
    alarm(5);
    struct ending_group group = {.mode = "leader-waits", .leader = getpid()};
    pthread_t thread;
    if (pthread_barrier_init(&group.ready, NULL, 2) != 0) _exit(26);
    dprintf(1, "member %d %d\n", group.leader, group.leader);
    if (pthread_create(&thread, NULL, ending_worker, &group) != 0) _exit(26);
    pthread_barrier_wait(&group.ready);
    syscall(SYS_exit, 23);
}

static int group_exits(const char *mode, int workers, int rounds) {
    if (workers < 1 || workers > 64 || rounds < 1 || rounds > 1000) return 24;
    dprintf(1, "root %d\n", getpid());
    for (int round = 0; round < rounds; round++) {
        pid_t child = fork();
        if (child < 0) return 25;
        if (child == 0) {
            alarm(5);
            if (setpgid(0, 0) != 0) _exit(26);
            struct ending_group group = {.mode = mode, .leader = getpid()};
            pthread_t threads[64];
            if (pthread_barrier_init(&group.ready, NULL, workers + 1) != 0) _exit(26);
            dprintf(1, "member %d %d\n", group.leader, group.leader);
            for (int i = 0; i < workers; i++) {
                if (pthread_create(&threads[i], NULL, ending_worker, &group) != 0) _exit(26);
            }
            pthread_barrier_wait(&group.ready);
            if (strcmp(mode, "term") == 0) kill(-group.leader, SIGTERM);
            if (strcmp(mode, "kill") == 0) kill(-group.leader, SIGKILL);
            if (strcmp(mode, "group") == 0) syscall(SYS_exit_group, 37);
            if (strcmp(mode, "leader-first") == 0 || strcmp(mode, "parallel") == 0)
                syscall(SYS_exit, 23);
            if (strcmp(mode, "return") == 0) {
                for (int i = 0; i < workers; i++) pthread_join(threads[i], NULL);
                _exit(37);
            }
            for (;;) pause();
        }
        int status;
        if (waitpid(child, &status, 0) != child) return 27;
        dprintf(1, "wait %d %d\n", child, status);
        int expected = strcmp(mode, "term") == 0 ? SIGTERM :
            strcmp(mode, "kill") == 0 ? SIGKILL :
            37 << 8;
        if (status != expected && !((strcmp(mode, "parallel") == 0 ||
            strcmp(mode, "leader-first") == 0) && status == (23 << 8)))
            return 28;
    }
    return 0;
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
static void namespaced(const char *program) {
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
            if (program) {
                alarm(10);
                execl(program, program, (char *)NULL);
                _exit(24);
            }
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
    printf(program ? "READY child %d\n" : "child %d\n", child);
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
    if (strcmp(mode, "group-exits") == 0 && (argc == 5 || argc == 6)) {
        if (argc == 6) {
            alarm(5);
            dprintf(1, "READY %d\n", getpid());
            char byte;
            if (read(0, &byte, 1) != 1) return 29;
        }
        return group_exits(argv[2], atoi(argv[3]), atoi(argv[4]));
    }
    if (strcmp(mode, "leader-waits") == 0) {
        leader_waits();
        return 30;
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
        namespaced(argc > 2 ? argv[2] : NULL);
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
