




#include <stdbool.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

void barrier(void *fixture);

enum color { RED, GREEN = 5, BLUE = 7 };
enum sign { NEGATIVE = -2, POSITIVE = 3 };
typedef long counter_t;

struct inner {
    short s;
    long long ll;
};

struct bits {
    unsigned a : 3;
    signed b : 5;
    unsigned c : 9;
};

union both {
    int32_t i;
    float f;
};

struct node {
    int value;
    struct node *next;
};



struct tail {
    int count;
    int data[];
};

struct fixture {
    int8_t i8;
    uint8_t u8;
    int16_t i16;
    uint16_t u16;
    int32_t i32;
    uint32_t u32;
    int64_t i64;
    uint64_t u64;
    char c;
    signed char sc;
    unsigned char uc;
    bool flag;
    float f32;
    double f64;
    long double ld;
    struct inner inner;
    struct bits bits;
    union both both;
    int arr[5];
    int grid[2][3];
    struct node nodes[3];
    struct node *head;
    struct tail *tail;
    int *ip;
    int **ipp;
    const char *text;
    char buf[8];
    enum color color;
    enum sign sign;
    counter_t counter;

    union {
        int32_t anonymous_int;
        uint32_t anonymous_bits;
    };
    struct {
        int16_t outer_half;
        struct {
            int16_t deep_half;
        };
    };
};

int global_counter = 42;
static unsigned short static_value = 65535;

#define EXPECT_INT(expression, native) \
    printf("EXPECT\t%s\tint\t%lld\n", expression, (long long)(native))
#define EXPECT_UINT(expression, native) \
    printf("EXPECT\t%s\tint\t%llu\n", expression, (unsigned long long)(native))
#define EXPECT_BOOL(expression, native) \
    printf("EXPECT\t%s\tbool\t%s\n", expression, (native) ? "true" : "false")
#define EXPECT_ADDRESS(expression, native) \
    printf("EXPECT\t%s\taddress\t%#llx\n", expression, (unsigned long long)(uintptr_t)(native))
#define EXPECT_F32(expression, native)                                  \
    do {                                                                \
        float value_ = (native);                                        \
        uint32_t bits_;                                                 \
        memcpy(&bits_, &value_, sizeof bits_);                          \
        printf("EXPECT\t%s\tf32\t%#x\n", expression, (unsigned)bits_);  \
    } while (0)
#define EXPECT_F64(expression, native)                                         \
    do {                                                                       \
        double value_ = (native);                                              \
        uint64_t bits_;                                                        \
        memcpy(&bits_, &value_, sizeof bits_);                                 \
        printf("EXPECT\t%s\tf64\t%#llx\n", expression, (unsigned long long)bits_); \
    } while (0)
#define EXPECT_F80(expression, native)                                            \
    do {                                                                          \
        long double value_ = (native);                                            \
        uint64_t significand_;                                                    \
        uint16_t exponent_;                                                       \
        memcpy(&significand_, &value_, sizeof significand_);                      \
        memcpy(&exponent_, (char *)&value_ + 8, sizeof exponent_);                \
        printf("EXPECT\t%s\tf80\t%#x:%#llx\n", expression, (unsigned)exponent_,  \
               (unsigned long long)significand_);                                 \
    } while (0)

int main(int argc, char **argv) {
    (void)argv;
    struct fixture f = {
        .i8 = -100,
        .u8 = 250,
        .i16 = -30000,
        .u16 = 65000,
        .i32 = -70000,
        .u32 = 4000000000u,
        .i64 = -5000000000000LL,
        .u64 = 18000000000000000000ULL,
        .c = 'x',
        .sc = -7,
        .uc = 200,
        .flag = true,
        .f32 = 1.5f,
        .f64 = 2.75,
        .ld = 1.25L,
        .inner = {.s = -12, .ll = 123456789012LL},
        .bits = {.a = 5, .b = -9, .c = 300},
        .both = {.i = 0x3fc00000},
        .arr = {10, 20, 30, 40, 50},
        .grid = {{1, 2, 3}, {4, 5, 6}},
        .text = "hello",
        .buf = "abc",
        .color = BLUE,
        .sign = NEGATIVE,
        .counter = 77,
        .anonymous_int = -42,
        .outer_half = 300,
        .deep_half = -301,
    };
    f.nodes[0] = (struct node){.value = 1, .next = &f.nodes[1]};
    f.nodes[1] = (struct node){.value = 2, .next = &f.nodes[2]};
    f.nodes[2] = (struct node){.value = 3, .next = NULL};
    f.head = &f.nodes[0];
    f.ip = &f.arr[2];
    f.ipp = &f.ip;
    f.tail = malloc(sizeof *f.tail + 4 * sizeof f.tail->data[0]);
    f.tail->count = 4;
    for (int i = 0; i < f.tail->count; i++) {
        f.tail->data[i] = 100 + i;
    }

    int count = argc + 3;
    int vla[count];
    int vla_grid[2][count];
    for (int i = 0; i < count; i++) {
        vla[i] = i * i;
        vla_grid[0][i] = i;
        vla_grid[1][i] = 10 + i;
    }

    EXPECT_INT("f.i8", f.i8);
    EXPECT_UINT("f.u8", f.u8);
    EXPECT_INT("f.i16", f.i16);
    EXPECT_UINT("f.u16", f.u16);
    EXPECT_INT("f.i32", f.i32);
    EXPECT_UINT("f.u32", f.u32);
    EXPECT_INT("f.i64", f.i64);
    EXPECT_UINT("f.u64", f.u64);
    EXPECT_INT("f.c", f.c);
    EXPECT_INT("f.sc", f.sc);
    EXPECT_UINT("f.uc", f.uc);
    EXPECT_BOOL("f.flag", f.flag);
    EXPECT_BOOL("!f.flag", !f.flag);
    EXPECT_INT("f.u8 + 10", f.u8 + 10);
    EXPECT_INT("f.u32 + f.u32", (int64_t)f.u32 + f.u32);
    EXPECT_INT("f.i8 * f.i16", f.i8 * f.i16);
    EXPECT_INT("f.i64 / 7", f.i64 / 7);
    EXPECT_INT("f.i64 % 7", f.i64 % 7);
    EXPECT_INT("(u8)(f.u8 + 10)", (uint8_t)(f.u8 + 10));
    EXPECT_INT("f.u8 & 0x0f", f.u8 & 0x0f);
    EXPECT_INT("~f.u8", (uint8_t)~f.u8);
    EXPECT_INT("f.i32 >> 3", f.i32 >> 3);
    EXPECT_INT("f.u16 << 4", (uint16_t)(f.u16 << 4));
    EXPECT_BOOL("f.u8 > f.i8", f.u8 > f.i8);
    EXPECT_BOOL("f.i8 < f.u64", f.i8 < 0 || (uint64_t)f.i8 < f.u64);
    EXPECT_INT("f.inner.s", f.inner.s);
    EXPECT_INT("f.inner.ll", f.inner.ll);
    EXPECT_UINT("f.bits.a", f.bits.a);
    EXPECT_INT("f.bits.b", f.bits.b);
    EXPECT_UINT("f.bits.c", f.bits.c);
    EXPECT_INT("f.both.i", f.both.i);
    EXPECT_F32("f.both.f", f.both.f);
    EXPECT_INT("f.arr[3]", f.arr[3]);
    EXPECT_INT("f.grid[1][2]", f.grid[1][2]);
    EXPECT_INT("*f.arr", *f.arr);
    EXPECT_INT("&f.arr[4] - &f.arr[1]", &f.arr[4] - &f.arr[1]);
    EXPECT_BOOL("f.arr + 2 == f.ip", f.arr + 2 == f.ip);
    EXPECT_INT("f.head->value", f.head->value);
    EXPECT_INT("f.head->next->next->value", f.head->next->next->value);
    EXPECT_BOOL("f.nodes[1].next == &f.nodes[2]", f.nodes[1].next == &f.nodes[2]);
    EXPECT_BOOL("f.nodes[2].next == null", f.nodes[2].next == NULL);
    EXPECT_INT("*f.ip", *f.ip);
    EXPECT_INT("**f.ipp", **f.ipp);
    EXPECT_INT("f.ip[1]", f.ip[1]);
    EXPECT_INT("*(f.ip - 2)", *(f.ip - 2));
    EXPECT_INT("f.ip - f.arr", f.ip - f.arr);
    EXPECT_ADDRESS("&f", &f);
    EXPECT_ADDRESS("&f.inner.ll", &f.inner.ll);
    EXPECT_ADDRESS("f.head->next", f.head->next);
    EXPECT_ADDRESS("&global_counter", &global_counter);
    EXPECT_BOOL("f.text == \"hello\"", strcmp(f.text, "hello") == 0);
    EXPECT_BOOL("f.text == \"help\"", strcmp(f.text, "help") == 0);
    EXPECT_INT("len(f.text)", strlen(f.text));
    EXPECT_BOOL("f.buf == \"abc\"", strcmp(f.buf, "abc") == 0);
    EXPECT_INT("len(f.buf)", sizeof f.buf);
    EXPECT_INT("len(f.arr)", sizeof f.arr / sizeof f.arr[0]);
    EXPECT_INT("f.color", f.color);
    EXPECT_BOOL("f.color == BLUE", f.color == BLUE);
    EXPECT_BOOL("f.sign < POSITIVE", f.sign < POSITIVE);
    EXPECT_INT("f.color + 1", f.color + 1);
    EXPECT_INT("(enum color)5", (enum color)5);
    EXPECT_INT("f.counter * 2", f.counter * 2);
    EXPECT_F32("f.f32 + 1", f.f32 + 1);
    EXPECT_F64("f.f32 * 2.0", f.f32 * 2.0);
    EXPECT_F64("f.f64 * 2", f.f64 * 2);
    EXPECT_F64("f.f64 / 3", f.f64 / 3);
    EXPECT_F80("f.ld * 2", f.ld * 2);
    EXPECT_F80("f.ld + f.f64", f.ld + f.f64);
    EXPECT_INT("(int)f.f64", (int)f.f64);
    EXPECT_INT("f.f64 as i32", (int32_t)f.f64);
    EXPECT_INT("(short)f.i32", (short)f.i32);
    EXPECT_INT("(unsigned char)f.i16", (unsigned char)f.i16);
    EXPECT_INT("(long)f.u32", (long)f.u32);
    EXPECT_INT("*(long long*)&f.inner.ll", *(long long *)&f.inner.ll);
    EXPECT_INT("sizeof(f)", sizeof f);
    EXPECT_INT("sizeof(struct inner)", sizeof(struct inner));
    EXPECT_INT("sizeof(f.arr)", sizeof f.arr);
    EXPECT_INT("sizeof(int)", sizeof(int));
    EXPECT_INT("sizeof(long double)", sizeof(long double));
    EXPECT_INT("sizeof(counter_t)", sizeof(counter_t));
    EXPECT_INT("global_counter + 1", global_counter + 1);
    EXPECT_UINT("static_value", static_value);
    EXPECT_INT("f.flag ? f.arr[0] : f.arr[1]", f.flag ? f.arr[0] : f.arr[1]);
    EXPECT_BOOL("f.head != null && f.head->value == 1", f.head != NULL && f.head->value == 1);
    EXPECT_INT("f.anonymous_int", f.anonymous_int);
    EXPECT_UINT("f.anonymous_bits", f.anonymous_bits);
    EXPECT_INT("f.outer_half + f.deep_half", f.outer_half + f.deep_half);
    EXPECT_ADDRESS("&f.deep_half", &f.deep_half);
    EXPECT_INT("f.tail->data[2]", f.tail->data[2]);
    EXPECT_INT("f.tail->data[f.tail->count - 1]", f.tail->data[f.tail->count - 1]);
    EXPECT_INT("vla[3]", vla[3]);
    EXPECT_INT("len(vla)", count);
    EXPECT_INT("vla_grid[1][2]", vla_grid[1][2]);
    fflush(stdout);
    barrier(&f);
    __asm__ volatile("" : : "r"(vla), "r"(vla_grid) : "memory");
    return f.i32 == 0;
}
