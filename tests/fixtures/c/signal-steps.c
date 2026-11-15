


#define _GNU_SOURCE

#include <signal.h>
#include <sys/syscall.h>
#include <unistd.h>

volatile sig_atomic_t handled;
static pid_t process;
static pid_t thread;

static void handler(int signal) {
    (void)signal;
    handled += 1;
}

__attribute__((noinline)) long sum_to(long count) {
    long total = 0;
    for (long index = 0; index < count; ++index) {
        total += index;
        if (index == 2) {
            long result;
            __asm__ volatile("syscall"
                             : "=a"(result)
                             : "a"((long)SYS_tgkill), "D"((long)process), "S"((long)thread),
                               "d"((long)SIGUSR2)
                             : "rcx", "r11", "memory");
            (void)result;
        }
    }
    return total;
}

__attribute__((noinline)) long twice(long value) {
    return value * 2; // the returning line
}




__attribute__((noinline)) long twice_then_signal(long value) {



    value = twice(value); __asm__ volatile("syscall" : : "a"((long)SYS_tgkill), "D"((long)process), "S"((long)thread), "d"((long)SIGUSR2) : "rcx", "r11", "memory"); // the calling line

    value += 1; // the line after the signal
    return value;
}

int main(void) {
    struct sigaction action = {.sa_handler = handler};
    sigemptyset(&action.sa_mask);
    if (sigaction(SIGUSR2, &action, NULL) != 0) {
        return 100;
    }
    process = getpid();
    thread = gettid();
    long first = sum_to(5); // the stepped call
    long second = sum_to(5);
    long third = sum_to(5);
    long fourth = twice_then_signal(5);
    return first == 10 && second == 10 && third == 10 && fourth == 11 && handled == 4 ? 0 : 1;
}
