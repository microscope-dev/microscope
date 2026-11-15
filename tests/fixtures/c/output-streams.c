

#include <stdio.h>
#include <string.h>

int main(void) {
    for (int line = 1; line <= 3; ++line) {
        printf("out %d\n", line);
        fflush(stdout);
        fprintf(stderr, "err %d\n", line);
        fflush(stderr);
    }
    fputs("bad \xff\xfe bytes, then \xe2\x9c\x93\n", stdout);
    static char burst[1024 * 1024];
    memset(burst, 'x', sizeof burst);
    fwrite(burst, 1, sizeof burst, stdout);
    printf("\nburst done\n");
    int input = getchar();
    printf("stdin: %s\n", input == EOF ? "eof" : "data");
    return 0;
}
