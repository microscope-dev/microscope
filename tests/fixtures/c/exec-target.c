

#include <stdio.h>

__attribute__((noinline)) int target_work(int argc) {
    return argc + 40;
}

int main(int argc, char **argv) {
    int code = target_work(argc);
    puts(argv[argc - 1]);
    return code;
}
