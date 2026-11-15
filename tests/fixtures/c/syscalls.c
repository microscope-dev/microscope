

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


    syscall(SYS_write, 1, "ready\n", 6);
    char byte;
    while (read(0, &byte, 1) > 0) {
    }
}



static int wake[2];
static int sleeper_tid;
static int begun;
static int first_cpu, second_cpu;


static __attribute__((noinline)) void wait_here(void) {
    char byte;
    if (read(wake[0], &byte, 1) != 1) {
        _exit(40);
    }
}

static int pin(pid_t tid, int cpu) {
    cpu_set_t set;
    CPU_ZERO(&set);
    CPU_SET(cpu, &set);
    return sched_setaffinity(tid, sizeof set, &set);
}

static void *sleeper(void *unused) {
    (void)unused;
    __atomic_store_n(&sleeper_tid, (int)gettid(), __ATOMIC_SEQ_CST);
    for (;;) {
        if (pin(0, first_cpu) != 0) {
            _exit(41);
        }
        __atomic_fetch_add(&begun, 1, __ATOMIC_SEQ_CST);
        wait_here();
    }
    return NULL;
}



static void wait_asleep(int count) {
    while (__atomic_load_n(&begun, __ATOMIC_SEQ_CST) < count) {
        sched_yield();
    }
    char path[64];
    snprintf(path, sizeof path, "/proc/self/task/%d/stat",
             __atomic_load_n(&sleeper_tid, __ATOMIC_SEQ_CST));
    int fd = open(path, O_RDONLY | O_CLOEXEC);
    if (fd < 0) {
        _exit(42);
    }
    for (;;) {
        char stat[512];
        ssize_t got = pread(fd, stat, sizeof stat - 1, 0);
        if (got <= 0) {
            _exit(43);
        }
        stat[got] = 0;

        char *name_end = strrchr(stat, ')');
        if (name_end && name_end[1] == ' ' && name_end[2] == 'S') {
            break;
        }
        sched_yield();
    }
    close(fd);
}





static void migrate(int rounds) {
    dprintf(1, "READY\n");
    char byte;
    if (read(0, &byte, 1) != 1) {
        _exit(44);
    }
    cpu_set_t allowed;
    if (sched_getaffinity(0, sizeof allowed, &allowed) != 0 || CPU_COUNT(&allowed) < 2) {
        _exit(45);
    }
    first_cpu = -1;
    for (int cpu = 0; cpu < CPU_SETSIZE; cpu++) {
        if (CPU_ISSET(cpu, &allowed)) {
            if (first_cpu < 0) {
                first_cpu = cpu;
            } else {
                second_cpu = cpu;
                break;
            }
        }
    }
    if (pipe(wake) != 0) {
        _exit(46);
    }
    pthread_t thread;
    if (pthread_create(&thread, NULL, sleeper, NULL) != 0) {
        _exit(47);
    }
    for (int round = 0; round < rounds; round++) {
        wait_asleep(round + 1);
        pid_t sleeper_now = __atomic_load_n(&sleeper_tid, __ATOMIC_SEQ_CST);
        if (pin(sleeper_now, second_cpu) != 0 || write(wake[1], "x", 1) != 1) {
            _exit(48);
        }
    }
    wait_asleep(rounds + 1);
    dprintf(1, "blocked\n");
    for (;;) {
        pause();
    }
}




#define SKIP_ENTRY 0x5ca1ab1eL
#define SKIP_EXIT 0x5ca1ab1fL


#define INTERLEAVE 0x5ca1ab20L

static void *skipped_thread(void *unused) {
    (void)unused;
    syscall(SYS_getpgid, 0L, SKIP_ENTRY);
    syscall(SYS_getpgid, 0L, SKIP_EXIT);
    return NULL;
}




static void skipped(void) {
    syscall(SYS_getpgid, 0L, SKIP_ENTRY);
    syscall(SYS_getpgid, 0L, SKIP_EXIT);
    pthread_t thread;
    if (pthread_create(&thread, NULL, skipped_thread, NULL) != 0 ||
        pthread_join(thread, NULL) != 0) {
        _exit(50);
    }
    syscall(SYS_getpgid, 0L, 0L);
}



static void interleaved(void) {
    syscall(SYS_getpgid, 0L, INTERLEAVE);
    syscall(SYS_getpgid, 0L, 0L);
}



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


    long result;
    __asm__ volatile("int $0x80" : "=a"(result) : "a"(20) : "memory");
    if (result != getpid()) {
        _exit(14);
    }
    __asm__ volatile("syscall" : "=a"(result) : "a"(0x40000000L | 39) : "rcx", "r11", "memory");
}

static void dumps(void) {


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

static long since(const struct timespec *start) {
    struct timespec now;
    clock_gettime(CLOCK_MONOTONIC, &now);
    return (now.tv_sec - start->tv_sec) * 1000000000L + now.tv_nsec - start->tv_nsec;
}


static __attribute__((noinline)) void burn(void) {
    struct timespec start;
    clock_gettime(CLOCK_MONOTONIC, &start);
    volatile unsigned long sum = 0;
    while (since(&start) < 2000000) {
        for (int i = 0; i < 1000; i++) {
            sum += i;
        }
    }
}


static unsigned char pool[4 << 20];
static __attribute__((noinline)) void draw(void) {
    if (syscall(SYS_getrandom, pool, sizeof pool, 0) != (long)sizeof pool) {
        _exit(31);
    }
}


static __attribute__((noinline)) void rest(void) {
    struct timespec two = {0, 2000000};
    nanosleep(&two, NULL);
}



static void work(void) {
    for (int round = 0; round < 25; round++) {
        burn();
        draw();
        rest();
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
    if (strcmp(mode, "work") == 0) {
        work();
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
    if (strcmp(mode, "skipped") == 0) {
        skipped();
        return 0;
    }
    if (strcmp(mode, "interleaved") == 0) {
        interleaved();
        return 0;
    }
    if (strcmp(mode, "migrate") == 0 && argc > 2) {
        migrate(atoi(argv[2]));
    }
    if (strcmp(mode, "spin") == 0) {
        spin();
        return 0;
    }
    if (strcmp(mode, "pause") == 0) {

        pause();
        return 0;
    }
    if (strcmp(mode, "signal") == 0) {
        signals();
        return 0;
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
