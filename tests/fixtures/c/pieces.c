








#include <complex.h>
#include <inttypes.h>
#include <stdio.h>
#include <string.h>

struct pair {
    long first;
    long second;
};

volatile long sink;

__attribute__((noinline)) void reached(const char *checkpoint) {
    sink = (long)checkpoint;
    __asm__ volatile("" ::: "memory");
}

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



__attribute__((noinline)) static long split(struct pair pair, long bias) {
    struct pair local = pair;
    local.first += bias;
    printf("TRUTH\tsplit\tpair.first\tint\t%ld\n", pair.first);
    printf("TRUTH\tsplit\tpair.second\tint\t%ld\n", pair.second);
    printf("TRUTH\tsplit\tlocal.first\tint\t%ld\n", local.first);
    printf("TRUTH\tsplit\tlocal.second\tint\t%ld\n", local.second);
    fflush(stdout);
    reached("split");
    sink = local.second;
    return local.first;
}

__attribute__((noinline)) static double complexes(float _Complex small, double _Complex large) {
    double _Complex product = large * 2.0;
    printf("TRUTH\tcomplex\tsmall\tc64\t%#" PRIx32 ":%#" PRIx32 "\n", bits32(crealf(small)),
           bits32(cimagf(small)));
    printf("TRUTH\tcomplex\tlarge\tc128\t%#" PRIx64 ":%#" PRIx64 "\n", bits64(creal(large)),
           bits64(cimag(large)));
    printf("TRUTH\tcomplex\tproduct\tc128\t%#" PRIx64 ":%#" PRIx64 "\n", bits64(creal(product)),
           bits64(cimag(product)));
    printf("TRUTH\tcomplex\tsmall.imag\tf32\t%#" PRIx32 "\n", bits32(cimagf(small)));
    printf("TRUTH\tcomplex\tlarge.real\tf64\t%#" PRIx64 "\n", bits64(creal(large)));
    printf("TRUTH\tcomplex\tsmall\tsummary\t(1.5-2i)\n");
    printf("TRUTH\tcomplex\tlarge\tsummary\t(0.25+3e300i)\n");
    fflush(stdout);
    reached("complex");
    return creal(product) - cimagf(small);
}

int main(int argc, char **argv) {
    (void)argv;
    struct pair pair = {argc * 10L, argc * 20L};
    long result = split(pair, argc);
    double value = complexes(1.5f - 2.0f * I, 0.25 + 3e300 * I);
    return (result == 11 && value > 0) ? 0 : 1;
}
