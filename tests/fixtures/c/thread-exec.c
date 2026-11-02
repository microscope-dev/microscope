/* A second thread executes a copy of /bin/true from a memfd once the
 * leader sleeps in pause. The new program's file, which /proc names
 * "/memfd:true (deleted)", cannot be read by any path, so a debugger
 * cannot load it. */
#define _GNU_SOURCE
#include <elf.h>
#include <fcntl.h>
#include <pthread.h>
#include <sys/mman.h>
#include <stdio.h>
#include <string.h>
#include <sys/syscall.h>
#include <time.h>
#include <unistd.h>

/* Whether the leader sleeps in pause, as its /proc files say. */
static int leader_paused(void) {
    char text[512];
    FILE *file;
    char *state;
    long number = -1;

    file = fopen("/proc/self/stat", "r");
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
    file = fopen("/proc/self/syscall", "r");
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

/* A valid interpreter header without loadable segments fails only after
 * begin_new_exec has changed the worker's id. */
static int break_interpreter(int program) {
    Elf64_Ehdr header = {
        .e_ident = {ELFMAG0, ELFMAG1, ELFMAG2, ELFMAG3,
                    ELFCLASS64, ELFDATA2LSB, EV_CURRENT},
        .e_type = ET_DYN,
        .e_machine = EM_X86_64,
        .e_version = EV_CURRENT,
        .e_ehsize = sizeof(Elf64_Ehdr),
        .e_phoff = sizeof(Elf64_Ehdr),
        .e_phentsize = sizeof(Elf64_Phdr),
        .e_phnum = 1,
    };
    Elf64_Phdr segment = {.p_type = PT_NULL};
    int interpreter = memfd_create("empty-interpreter", MFD_CLOEXEC);
    char path[64];
    if (interpreter < 0 ||
        write(interpreter, &header, sizeof header) != sizeof header ||
        write(interpreter, &segment, sizeof segment) != sizeof segment ||
        pread(program, &header, sizeof header, 0) != sizeof header) {
        return -1;
    }
    snprintf(path, sizeof path, "/proc/self/fd/%d", interpreter);
    for (int i = 0; i < header.e_phnum; i++) {
        off_t offset = header.e_phoff + i * sizeof segment;
        if (pread(program, &segment, sizeof segment, offset) != sizeof segment) {
            return -1;
        }
        if (segment.p_type != PT_INTERP) {
            continue;
        }
        segment.p_offset = lseek(program, 0, SEEK_END);
        segment.p_filesz = strlen(path) + 1;
        if (write(program, path, segment.p_filesz) != (ssize_t)segment.p_filesz ||
            pwrite(program, &segment, sizeof segment, offset) != sizeof segment) {
            return -1;
        }
        return 0;
    }
    return -1;
}

/* Executes the copy once the leader sleeps in pause, or after a deadline
 * without that. */
static void *replace_image(void *argument) {
    char *const arguments[] = {"true", NULL};
    char *const environment[] = {NULL};
    struct timespec wait = {0, 1000000};
    int program = copy_true();

    if (program < 0 || (argument != NULL && break_interpreter(program) != 0)) {
        _exit(91);
    }
    for (int tries = 0; tries < 30000 && !leader_paused(); tries++) {
        nanosleep(&wait, NULL);
    }
    fexecve(program, arguments, environment);
    _exit(90);
}

int main(int argc, char **argv) {
    pthread_t thread;

    void *mode = argc == 2 && strcmp(argv[1], "late-failure") == 0 ? argv[1] : NULL;
    if (pthread_create(&thread, NULL, replace_image, mode) != 0) {
        return 2;
    }
    for (;;) {
        pause();
    }
}
