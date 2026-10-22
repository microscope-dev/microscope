/* The program exec-other executes: prints its last argument and exits
 * with its argument count plus 40. */
#include <stdio.h>

__attribute__((noinline)) int target_work(int argc) {
    return argc + 40;
}

int main(int argc, char **argv) {
    int code = target_work(argc);
    puts(argv[argc - 1]);
    return code;
}
