









#include <complex.h>
#include <inttypes.h>
#include <stdbool.h>
#include <stdio.h>
#include <string.h>

volatile long sink;

__attribute__((noinline)) void reached(const char *checkpoint) {
    sink = (long)checkpoint;
    __asm__ volatile("" ::: "memory");
}

static void truth(const char *checkpoint, const char *path, const char *kind, const char *value) {
    printf("TRUTH\t%s\t%s\t%s\t%s\n", checkpoint, path, kind, value);
}

static void integer(const char *checkpoint, const char *path, long long value) {
    char text[32];
    snprintf(text, sizeof text, "%lld", value);
    truth(checkpoint, path, "int", text);
}


#define reach(checkpoint)                                                                         \
    do {                                                                                           \
        fflush(stdout);                                                                            \
        reached(checkpoint);                                                                       \
    } while (0)

static uint32_t bits32(float value) {
    uint32_t bits;
    memcpy(&bits, &value, sizeof bits);
    return bits;
}

static uint64_t bits64(double value) {
    uint64_t bits;
    memcpy(&bits, &value, sizeof bits);
    return bits;
}

static void f32(const char *checkpoint, const char *path, float value) {
    char text[32];
    snprintf(text, sizeof text, "%#" PRIx32, bits32(value));
    truth(checkpoint, path, "f32", text);
}

static void f64(const char *checkpoint, const char *path, double value) {
    char text[32];
    snprintf(text, sizeof text, "%#" PRIx64, bits64(value));
    truth(checkpoint, path, "f64", text);
}

enum color { RED, GREEN, BLUE };

struct ints {
    int a;
    int b;
};

struct pair {
    long first;
    long second;
};

struct mixed {
    double d;
    int i;
};

struct flipped {
    int i;
    double d;
};

struct floats {
    float x;
    float y;
    float z;
};

struct doubles {
    double x;
    double y;
};

struct vector {
    float v[2];
    double w;
};

struct big {
    long a;
    long b;
    long c;
};

struct text {
    char bytes[12];
};

struct bits {
    unsigned low : 3;
    unsigned high : 5;
    int rest;
};

union number {
    double d;
    long l;
};

__attribute__((noinline)) int r_int(int n) {
    int value = -7 * n;
    integer("returned-int", "r_int", value);
    reach("returned-int");
    return value;
}

__attribute__((noinline)) unsigned char r_char(int n) {
    unsigned char value = (unsigned char)('A' + n);
    integer("returned-char", "r_char", value);
    reach("returned-char");
    return value;
}

__attribute__((noinline)) bool r_bool(int n) {
    bool value = n > 0;
    truth("returned-bool", "r_bool", "summary", value ? "true" : "false");
    reach("returned-bool");
    return value;
}

__attribute__((noinline)) __int128 r_int128(int n) {
    __int128 value = ((__int128)n << 70) + 5;

    truth("returned-int128", "r_int128", "int", n == 1 ? "1180591620717411303429" : "?");
    reach("returned-int128");
    return value;
}

__attribute__((noinline)) enum color r_enum(int n) {
    enum color value = (enum color)(n % 3);
    truth("returned-enum", "r_enum", "symbol", value == GREEN ? "GREEN" : "?");
    reach("returned-enum");
    return value;
}

__attribute__((noinline)) float r_float(int n) {
    float value = 1.25f * (float)n;
    f32("returned-float", "r_float", value);
    reach("returned-float");
    return value;
}

__attribute__((noinline)) double r_double(int n) {
    double value = -0.375 * n;
    f64("returned-double", "r_double", value);
    reach("returned-double");
    return value;
}

__attribute__((noinline)) long double r_long_double(int n) {
    long double value = 2.5L * n;
    truth("returned-long-double", "r_long_double", "summary", n == 1 ? "2.5" : "?");
    reach("returned-long-double");
    return value;
}

__attribute__((noinline)) float _Complex r_complex_float(int n) {
    float _Complex value = 1.5f * (float)n - 2.0f * I;
    char text[64];
    snprintf(text, sizeof text, "%#" PRIx32 ":%#" PRIx32, bits32(crealf(value)),
             bits32(cimagf(value)));
    truth("returned-complex-float", "r_complex_float", "c64", text);
    reach("returned-complex-float");
    return value;
}

__attribute__((noinline)) double _Complex r_complex_double(int n) {
    double _Complex value = 0.25 * n + 3.0 * I;
    char text[64];
    snprintf(text, sizeof text, "%#" PRIx64 ":%#" PRIx64, bits64(creal(value)),
             bits64(cimag(value)));
    truth("returned-complex-double", "r_complex_double", "c128", text);
    reach("returned-complex-double");
    return value;
}

__attribute__((noinline)) struct ints r_ints(int n) {
    struct ints value = {n * 3, -n * 4};
    integer("returned-ints", "r_ints.a", value.a);
    integer("returned-ints", "r_ints.b", value.b);
    reach("returned-ints");
    return value;
}

__attribute__((noinline)) struct pair r_pair(int n) {
    struct pair value = {n * 100000000000L, -n * 7L};
    integer("returned-pair", "r_pair.first", value.first);
    integer("returned-pair", "r_pair.second", value.second);
    reach("returned-pair");
    return value;
}

__attribute__((noinline)) struct mixed r_mixed(int n) {
    struct mixed value = {0.5 * n, n + 41};
    f64("returned-mixed", "r_mixed.d", value.d);
    integer("returned-mixed", "r_mixed.i", value.i);
    reach("returned-mixed");
    return value;
}

__attribute__((noinline)) struct flipped r_flipped(int n) {
    struct flipped value = {n + 9, 1.75 * n};
    integer("returned-flipped", "r_flipped.i", value.i);
    f64("returned-flipped", "r_flipped.d", value.d);
    reach("returned-flipped");
    return value;
}

__attribute__((noinline)) struct floats r_floats(int n) {
    struct floats value = {1.0f * (float)n, 2.0f * (float)n, -3.0f * (float)n};
    f32("returned-floats", "r_floats.x", value.x);
    f32("returned-floats", "r_floats.y", value.y);
    f32("returned-floats", "r_floats.z", value.z);
    reach("returned-floats");
    return value;
}

__attribute__((noinline)) struct doubles r_doubles(int n) {
    struct doubles value = {0.125 * n, -8.0 * n};
    f64("returned-doubles", "r_doubles.x", value.x);
    f64("returned-doubles", "r_doubles.y", value.y);
    reach("returned-doubles");
    return value;
}

__attribute__((noinline)) struct vector r_vector(int n) {
    struct vector value = {{0.5f * (float)n, 4.0f}, -1.5 * n};
    f32("returned-vector", "r_vector.v.0", value.v[0]);
    f32("returned-vector", "r_vector.v.1", value.v[1]);
    f64("returned-vector", "r_vector.w", value.w);
    reach("returned-vector");
    return value;
}



__attribute__((noinline)) struct big r_big(int n) {
    struct big value = {n * 11L, n * 22L, n * 33L};
    integer("returned-big", "r_big.a", value.a);
    integer("returned-big", "r_big.c", value.c);
    reach("returned-big");
    return value;
}

__attribute__((noinline)) struct text r_text(int n) {
    struct text value;
    memcpy(value.bytes, "hello world", 12);
    value.bytes[0] = (char)(value.bytes[0] + n - 1);
    integer("returned-text", "r_text.bytes.0", value.bytes[0]);
    integer("returned-text", "r_text.bytes.10", value.bytes[10]);
    reach("returned-text");
    return value;
}

__attribute__((noinline)) struct bits r_bits(int n) {
    struct bits value = {(unsigned)n + 4, (unsigned)n * 9, -n};
    integer("returned-bits", "r_bits.low", value.low);
    integer("returned-bits", "r_bits.high", value.high);
    integer("returned-bits", "r_bits.rest", value.rest);
    reach("returned-bits");
    return value;
}

__attribute__((noinline)) union number r_union(int n) {
    union number value;
    value.l = n * 123456789L;
    integer("returned-union", "r_union.l", value.l);
    reach("returned-union");
    return value;
}

__attribute__((noinline)) void r_void(int n) {
    sink = n;
    truth("returned-void", "r_void", "absent", "");
    reach("returned-void");

    sink = n + 1;
}

int main(int argc, char **argv) {
    (void)argv;
    int n = argc;
    long total = r_int(n) + r_char(n) + r_bool(n) + (long)(r_int128(n) >> 70) + r_enum(n);
    total += (long)(r_float(n) + r_double(n) + r_long_double(n));
    total += (long)(crealf(r_complex_float(n)) + creal(r_complex_double(n)));
    total += r_ints(n).a + r_pair(n).second + r_mixed(n).i + r_flipped(n).i;
    total += (long)(r_floats(n).z + r_doubles(n).y + r_vector(n).w);
    total += r_big(n).c + r_text(n).bytes[4] + r_bits(n).high + r_union(n).l;
    r_void(n);
    sink = total;
    return 0;
}
