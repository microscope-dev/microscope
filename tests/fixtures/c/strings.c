


#define _DEFAULT_SOURCE

#include <stddef.h>
#include <string.h>
#include <sys/mman.h>
#include <uchar.h>
#include <wchar.h>

const char *global_greeting = "global text";

__attribute__((noinline)) int strings_target(const char *greeting, const char *escaped,
                                             const char *long_text, const char *edge) {
    char buffer[16] = "abc";
    char unterminated[4] = {'w', 'x', 'y', 'z'};
    const char *null_text = NULL;
    const char *invalid = (const char *)1;
    unsigned char bytes[3] = {0x41, 0xff, 0};
    const wchar_t *wide = L"wide \u00e9";
    const char16_t *sixteen = u"sixteen \u03bb \U0001F980";
    const char32_t *thirty_two = U"thirty-two \U0001F980";
    wchar_t wide_buffer[8] = L"buf";

    char16_t lone[3] = {u'a', 0xd800, 0};
    __asm__ volatile("" : : "g"(greeting), "g"(escaped), "g"(long_text), "g"(edge),
                     "g"(buffer), "g"(unterminated), "g"(null_text), "g"(invalid),
                     "g"(bytes), "g"(wide), "g"(sixteen), "g"(thirty_two), "g"(wide_buffer),
                     "g"(lone) : "memory");
    return (int)strlen(greeting) + buffer[0]; // strings stop here
}

int main(void) {
    static char long_text[600];
    memset(long_text, 'x', sizeof long_text - 1);

    char *pages = mmap(NULL, 8192, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (pages == MAP_FAILED || munmap(pages + 4096, 4096) != 0) {
        return 100;
    }
    memset(pages, 'e', 4096);
    char *edge = pages + 4096 - 5;
    return strings_target("hello, world", "tab\there \"quoted\" \\ \xc3\xa9\x80", long_text,
                          edge) == 12 + 'a'
               ? 0
               : 1;
}
