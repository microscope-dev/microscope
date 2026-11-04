// Reads stdin to its end and prints how many bytes came and a digest of
// them, so a test can tell that its input arrived whole, once, and in order.
#include <stdio.h>

int main(void) {
    unsigned long count = 0;
    unsigned long digest = 14695981039346656037ul;
    int byte;
    while ((byte = getchar()) != EOF) {
        ++count;
        digest = (digest ^ (unsigned char)byte) * 1099511628211ul;
    }
    printf("stdin: %lu bytes, digest %016lx\n", count, digest);
    return 0;
}
