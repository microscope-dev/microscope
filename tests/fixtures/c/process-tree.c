/* Starts COUNT children and waits for them, as a test runner or a watcher
 * does: `process-tree COUNT HOW [serial]`. Each child, by HOW:
 *   direct  runs child_work after its fork, and exits 0;
 *   shell   executes /bin/sh -c "exec-target child", which exits 42;
 *   spawn   is started by posix_spawn, a vfork and an exec, as exec-target;
 *   hang    executes /bin/sh -c "sleep 100; true", and is never waited for.
 * Children run concurrently unless "serial" follows, when each is waited
 * for before the next starts, as a watcher restarts its child. A direct
 * child exits 3 if it ever received SIGCONT. The parent exits with the
 * number of children that did not exit as they should. */
#define _GNU_SOURCE
#include <limits.h>
#include <signal.h>
#include <spawn.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/wait.h>
#include <unistd.h>

extern char **environ;

static volatile sig_atomic_t continued;

static void on_continue(int signal) {
    (void)signal;
    continued = 1;
}

__attribute__((noinline)) int child_work(int index) {
    return index;
}

__attribute__((noinline)) void children_started(int count) {
    (void)count;
}

/* The path of exec-target, beside this program. */
static void target_path(char *path, size_t size) {
    static const char target[] = "exec-target";
    ssize_t length = readlink("/proc/self/exe", path, size - 1);
    if (length < 0) {
        _exit(91);
    }
    path[length] = '\0';

    char *slash = strrchr(path, '/');
    if (slash == NULL || (size_t)(slash + 1 - path) + sizeof target > size) {
        _exit(92);
    }
    memcpy(slash + 1, target, sizeof target);
}

static pid_t start(const char *how, int index) {
    char target[PATH_MAX];
    target_path(target, sizeof target);

    if (strcmp(how, "spawn") == 0) {
        char *const arguments[] = {target, "child", NULL};
        pid_t child;
        return posix_spawn(&child, target, NULL, NULL, arguments, environ) == 0 ? child : -1;
    }

    pid_t child = fork();
    if (child != 0) {
        return child;
    }

    if (strcmp(how, "direct") == 0) {
        child_work(index);
        _exit(continued ? 3 : 0);
    }

    char command[PATH_MAX + 16];
    if (strcmp(how, "shell") == 0) {
        snprintf(command, sizeof command, "%s child", target);
    } else {
        snprintf(command, sizeof command, "sleep 100; true");
    }
    execl("/bin/sh", "sh", "-c", command, (char *)NULL);
    _exit(90);
}

static int expected(const char *how) {
    return strcmp(how, "direct") == 0 ? 0 : 42;
}

static int failed(pid_t child, const char *how) {
    int status;
    if (waitpid(child, &status, 0) != child) {
        return 1;
    }
    return !(WIFEXITED(status) && WEXITSTATUS(status) == expected(how));
}

int main(int argc, char **argv) {
    if (argc < 3) {
        return 100;
    }

    signal(SIGCONT, on_continue);
    int count = atoi(argv[1]);
    const char *how = argv[2];
    int serial = argc > 3 && strcmp(argv[3], "serial") == 0;
    pid_t children[16];
    if (count < 1 || count > 16) {
        return 101;
    }

    int failures = 0;
    for (int index = 0; index < count; index++) {
        children[index] = start(how, index);
        if (children[index] < 0) {
            failures++;
        } else if (serial) {
            failures += failed(children[index], how);
        }
    }
    children_started(count);

    if (strcmp(how, "hang") == 0) {
        pause();
        return 102;
    }

    if (!serial) {
        for (int index = 0; index < count; index++) {
            if (children[index] > 0) {
                failures += failed(children[index], how);
            }
        }
    }

    return failures;
}
