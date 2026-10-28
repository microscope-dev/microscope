// Recursion of two shapes, each RUNS times, 26 calls deep: self calls
// itself, and visit and children call each other. Each call does a little
// work of its own.
#include <stdlib.h>
volatile unsigned long sink;
__attribute__((noinline)) void work(void) { for (int i = 0; i < 20000; i++) sink += i; }
__attribute__((noinline)) void self(int d) { work(); if (d) self(d - 1); sink++; }
__attribute__((noinline)) void visit(int d);
__attribute__((noinline)) void children(int d) { work(); if (d) visit(d - 1); sink++; }
__attribute__((noinline)) void visit(int d) { children(d); sink++; }
int main(int c, char **v) {
    int n = c > 1 ? atoi(v[1]) : 4000;
    for (int i = 0; i < n; i++) { self(25); visit(25); }
    return 0;
}
