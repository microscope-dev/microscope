/* A second thread executes a copy of /bin/true from a memfd once the
 * leader sleeps in pause. The new program's file, which /proc names
 * "/memfd:true (deleted)", cannot be read by any path, so a debugger
 * cannot load it. */
#define _GNU_SOURCE
#include <fcntl.h>
#include <pthread.h>
#include <sys/mman.h>
#include <stdio.h>
#include <string.h>
#include <sys/syscall.h>
#include <time.h>
#include <unistd.h>

/* Whether the leader sleeps in pause, as its /proc files say. */
static int leader_paused(pid_t leader) {
    char path[64];
    char text[512];
    FILE *file;
    char *state;
    long number = -1;

    snprintf(path, sizeof path, "/proc/self/task/%d/stat", leader);
    file = fopen(path, "r");
    if (file == NULL) {
        return 0;
    }
    if (fgets(text, sizeof text, file) == NULL) {
        text[0] = '\0';
    }
    fclose(file);
    state = strrchr(text, ')');
    if (state == NULL || state[1] != ' ' || state[2] != 'S') {
        return 0;
    }
    snprintf(path, sizeof path, "/proc/self/task/%d/syscall", leader);
    file = fopen(path, "r");
    if (file == NULL) {
        return 0;
    }
    if (fscanf(file, "%ld", &number) != 1) {
        number = -1;
    }
    fclose(file);
    return number == SYS_pause;
}

/* A memfd holding a copy of /bin/true, or -1. */
static int copy_true(void) {
    char buffer[65536];
    ssize_t length;
    int from = open("/bin/true", O_RDONLY | O_CLOEXEC);
    int to = memfd_create("true", MFD_CLOEXEC);

    if (from < 0 || to < 0) {
        return -1;
    }
    while ((length = read(from, buffer, sizeof buffer)) > 0) {
        if (write(to, buffer, (size_t)length) != length) {
            return -1;
        }
    }
    close(from);
    return length == 0 ? to : -1;
}

/* Executes the copy once the leader sleeps in pause, or after a deadline
 * without that. */
static void *replace_image(void *argument) {
    char *const arguments[] = {"true", NULL};
    char *const environment[] = {NULL};
    pid_t leader = getpid();
    struct timespec wait = {0, 1000000};
    int program = copy_true();

    (void)argument;
    if (program < 0) {
        _exit(91);
    }
    for (int tries = 0; tries < 30000 && !leader_paused(leader); tries++) {
        nanosleep(&wait, NULL);
    }
    fexecve(program, arguments, environment);
    _exit(90);
}

int main(void) {
    pthread_t thread;

    if (pthread_create(&thread, NULL, replace_image, NULL) != 0) {
        return 2;
    }
    for (;;) {
        pause();
    }
}
