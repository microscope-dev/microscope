// A program with a thread-local variable of its own, reached by the local
// exec model, and a library's of every other model, read through each
// accessor.
#include <stdio.h>

long tls_general_get(void);
long tls_hidden_get(void);
long tls_local_get(void);
long tls_initial_get(void);
void tls_set(long value);

__thread long tls_program = 6;

__attribute__((noinline)) long tls_program_get(void) {
    return tls_program;
}

int main(void) {
    tls_set(10);
    tls_program = 16;
    long sum = tls_general_get() + tls_hidden_get() + tls_local_get() + tls_initial_get() +
               tls_program_get();
    printf("%ld\n", sum);
    return sum == 11 + 12 + 13 + 14 + 15 + 16 ? 0 : 1;
}
