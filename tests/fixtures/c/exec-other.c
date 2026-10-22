/* Executes exec-target, a program other than itself, from its directory:
 * from the main thread, or from a second thread when its argument is
 * "thread". Exits with 90 or above only when the exec fails. */
#define _DEFAULT_SOURCE
#include <limits.h>
#include <pthread.h>
#include <string.h>
#include <unistd.h>

__attribute__((noinline)) int before_exec(void) {
    return 3;
}

static void execute_target(void) {
    static const char target[] = "exec-target";
    char path[PATH_MAX];
    ssize_t length = readlink("/proc/self/exe", path, sizeof path - 1);
    if (length < 0) {
        _exit(91);
    }
    path[length] = '\0';

    char *slash = strrchr(path, '/');
    if (slash == NULL || (size_t)(slash + 1 - path) + sizeof target > sizeof path) {
        _exit(92);
    }
    memcpy(slash + 1, target, sizeof target);

    char *const arguments[] = {path, "from-exec-other", NULL};
    execv(path, arguments);
    _exit(90);
}

static void *worker(void *unused) {
    (void)unused;
    execute_target();
    return NULL;
}

int main(int argc, char **argv) {
    before_exec();
    if (argc > 1 && strcmp(argv[1], "thread") == 0) {
        pthread_t thread;
        if (pthread_create(&thread, NULL, worker, NULL) != 0) {
            return 93;
        }
        pthread_join(thread, NULL);
        return 94;
    }

    execute_target();
    return 95;
}
