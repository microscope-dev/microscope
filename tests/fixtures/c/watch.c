#define _GNU_SOURCE

#include <signal.h>
#include <stddef.h>
#include <stdint.h>
#include <sys/types.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>




struct __attribute__((packed, aligned(8))) packed_record {
    uint8_t lead;
    volatile uint32_t field;
    uint8_t tail[3];
};

struct pair_record {
    volatile uint64_t first;
    volatile uint64_t second;
};

struct wide_record {
    volatile uint64_t words[4];
};

struct oversized_record {
    volatile uint8_t bytes[33];
};

volatile uint8_t watch_u8;
volatile uint16_t watch_u16;
volatile int32_t watch_i32;
volatile uint64_t watch_u64 = 0x1111111111111111;
struct packed_record watch_packed;
_Alignas(16) struct pair_record watch_pair;
_Alignas(32) struct wide_record watch_wide;
struct oversized_record watch_oversized;
volatile int32_t watch_array[8];
volatile int32_t pointee_first;
volatile int32_t pointee_second;
volatile int32_t *volatile watch_pointer = &pointee_first;
struct timespec watch_time;
volatile int64_t watch_sink;
volatile int32_t watch_steady = 7;
volatile uint64_t watch_kernel;
static volatile sig_atomic_t handled;

__attribute__((noinline)) void scalar_stores(void) {
    watch_i32 = 1;
    watch_i32 = 2;
    watch_i32 = 2;
    watch_i32 += 40;
}

__attribute__((noinline)) void size_stores(void) {
    watch_u8 = 0x11;
    watch_u16 = 0x2222;
    watch_u64 = 0x4444444444444444;
    watch_packed.field = 0x55555555;
    watch_array[3] = 33;
    watch_oversized.bytes[32] = 1;
}


__attribute__((noinline)) void paired_store(void) {
    __asm__ volatile("pcmpeqb %%xmm0, %%xmm0\n\t"
                     "movdqu %%xmm0, (%0)"
                     :
                     : "r"(&watch_pair)
                     : "xmm0", "memory");
}


__attribute__((noinline)) void failed_exchange(void) {
    uint64_t expected = ~watch_u64;
    __asm__ volatile("lock cmpxchgq %2, (%1)"
                     : "+a"(expected)
                     : "r"(&watch_u64), "r"((uint64_t)1)
                     : "memory", "cc");
    watch_sink = (int64_t)expected;
}


__attribute__((noinline)) void repeated_store(void) {
    void *destination = (void *)&watch_wide;
    size_t count = 24;
    __asm__ volatile("cld\n\t"
                     "rep stosb"
                     : "+D"(destination), "+c"(count)
                     : "a"(0x41)
                     : "memory");
}

__attribute__((noinline)) void read_access(void) {
    watch_sink = watch_i32;
    watch_sink = -watch_sink;
}

__attribute__((noinline)) void retarget_pointer(void) {
    *watch_pointer = 1;
    watch_pointer = &pointee_second;
    *watch_pointer = 2;
    pointee_first = 3;
}



__attribute__((noinline)) void kernel_write(void) {
    int pipes[2];
    uint64_t value = 0x1234;
    if (pipe(pipes) != 0 || write(pipes[1], &value, sizeof value) != (ssize_t)sizeof value ||
        read(pipes[0], (void *)&watch_u64, sizeof value) != (ssize_t)sizeof value) {
        _exit(70);
    }
    close(pipes[0]);
    close(pipes[1]);
    watch_u64 += 1;
}

static void write_in_handler(int signal) {
    (void)signal;
    watch_i32 = 99;
    handled = 1;
}

__attribute__((noinline)) void handler_write(void) {
    struct sigaction action = {.sa_handler = write_in_handler};
    sigemptyset(&action.sa_mask);
    if (sigaction(SIGUSR1, &action, NULL) != 0) {
        _exit(71);
    }

    sigset_t unblocked;
    sigemptyset(&unblocked);
    if (sigaddset(&unblocked, SIGUSR1) != 0 ||
        sigprocmask(SIG_UNBLOCK, &unblocked, NULL) != 0) {
        _exit(71);
    }
    raise(SIGUSR1);
    if (!handled) {
        _exit(72);
    }
}



__attribute__((noinline)) void await_signal(void) {
    watch_sink = 1;
}



__attribute__((noinline)) void fork_write(void) {
    pid_t child = fork();
    if (child == 0) {
        watch_i32 = 1234;
        _exit(0);
    }
    int status = 0;
    if (child < 0 || waitpid(child, &status, 0) != child || !WIFEXITED(status) ||
        WEXITSTATUS(status) != 0) {
        _exit(73);
    }
    watch_i32 = 4321;
}


__attribute__((noinline)) void vdso_write(void) {
    if (clock_gettime(CLOCK_MONOTONIC_COARSE, &watch_time) != 0) {
        _exit(74);
    }
}

__attribute__((noinline)) void store_then_breakpoint(void) {
    __asm__ volatile("movw $7, watch_u16(%%rip)\n\t"
                     ".globl after_watched_store\n"
                     "after_watched_store:\n\t"
                     "nop" ::: "memory");
}

__attribute__((noinline)) void breakpoint_on_store(void) {
    __asm__ volatile(".globl watched_store_site\n"
                     "watched_store_site:\n\t"
                     "movl $5, watch_i32(%%rip)" ::: "memory");
}

__attribute__((noinline)) void nested_writer(void) {
    watch_i32 = 77;
}

__attribute__((noinline)) void step_over_writer(void) {
    nested_writer();
    watch_sink = 2;
}

__attribute__((noinline)) int static_local_counter(void) {
    static volatile int calls;
    calls += 1;
    return calls;
}

__attribute__((noinline)) void steady_store(void) {
    watch_steady = 7;
}



__attribute__((noinline)) void steady_stores(void) {
    watch_steady = 7;
    steady_store();
    for (int round = 0; round < 3; ++round) {
        steady_store();
        watch_steady = 7;
    }
    watch_steady = 7;
}

__attribute__((noinline)) void steady_change(void) {
    watch_steady = 8;
    watch_steady = 8;
}




__attribute__((noinline)) void kernel_then_same_store(void) {
    int pipes[2];
    uint64_t value = 0x99;
    if (pipe(pipes) != 0 || write(pipes[1], &value, sizeof value) != (ssize_t)sizeof value ||
        read(pipes[0], (void *)&watch_kernel, sizeof value) != (ssize_t)sizeof value) {
        _exit(75);
    }
    close(pipes[0]);
    close(pipes[1]);
    watch_kernel = 0x99;
}

int main(void) {
    scalar_stores();
    size_stores();
    paired_store();
    failed_exchange();
    repeated_store();
    read_access();
    retarget_pointer();
    kernel_write();
    handler_write();
    await_signal();
    fork_write();
    vdso_write();
    store_then_breakpoint();
    breakpoint_on_store();
    step_over_writer();
    steady_stores();
    steady_change();
    kernel_then_same_store();
    int calls = static_local_counter() + static_local_counter();
    return watch_i32 == 77 && calls == 3 && pointee_second == 2 && watch_steady == 8 ? 0 : 1;
}
