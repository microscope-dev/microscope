



#include "microscope_views.h"

MICROSCOPE_VIEWS_FILE("tests/fixtures/c/embedded-views/main.views");

typedef struct {
    int *data;
    unsigned long n;
    unsigned long cap;
} intvec;

struct point {
    int x;
    int y;
};

extern struct point library_origin;
int library_touch(void);

__attribute__((noinline)) void barrier(void *fixture) {
    __asm__ volatile("" : : "r"(fixture) : "memory");
}

int main(void) {
    int storage[4] = {1, 2, 3, 0};
    intvec numbers = {storage, 3, 4};
    struct point here = {1, 2};
    barrier(&numbers);
    barrier(&here);
    return library_touch() + numbers.data[0] + here.x - 2;
}
