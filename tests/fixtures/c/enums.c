#include <stdint.h>

enum Signed {
    SIGNED_NEGATIVE = -3,
    SIGNED_ZERO = 0,
    SIGNED_ZERO_ALIAS = 0,
};

enum Flags {
    FLAG_READ = 1U,
    FLAG_WRITE = 2U,
};


enum Sequence {
    SEQUENCE_FIRST,
    SEQUENCE_SECOND,
    SEQUENCE_THIRD,
};

enum __attribute__((packed)) Byte {
    BYTE_ZERO = 0,
    BYTE_MAX = UINT8_MAX,
};

union Raw {
    int32_t integer;
    float floating;
};

__attribute__((noinline))
static int inspect_enums(
    const enum Signed *signed_value,
    const enum Signed *zero_alias,
    const enum Flags *flags,
    const enum Byte *byte_value,
    const union Raw *raw,
    const enum Sequence *sequence
) {
    __asm__ volatile(
        ""
        :
        : "r"(signed_value), "r"(zero_alias), "r"(flags), "r"(byte_value), "r"(raw),
          "r"(sequence)
        : "memory"
    );
    return *signed_value == SIGNED_NEGATIVE
        && *zero_alias == SIGNED_ZERO
        && *flags == (enum Flags)(FLAG_READ | FLAG_WRITE)
        && *byte_value == BYTE_MAX
        && raw->integer == 42
        && *sequence == (enum Sequence)3;
}

int main(void) {
    enum Signed signed_value = SIGNED_NEGATIVE;
    enum Signed zero_alias = SIGNED_ZERO;
    enum Flags flags = (enum Flags)(FLAG_READ | FLAG_WRITE);
    enum Byte byte_value = BYTE_MAX;
    union Raw raw = {.integer = 42};
    enum Sequence sequence = (enum Sequence)3;
    return !inspect_enums(&signed_value, &zero_alias, &flags, &byte_value, &raw, &sequence);
}
