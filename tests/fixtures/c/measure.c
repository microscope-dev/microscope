


#include <stdio.h>
#include <string.h>

__attribute__((noinline)) size_t measure(const char *text) {
    return strlen(text);
}

int main(int argc, char **argv) {
    printf("%zu\n", measure(argc > 1 ? argv[1] : "symbols"));
    return 0;
}
