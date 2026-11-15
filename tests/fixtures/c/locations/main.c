








#include <stdint.h>
#include <stdlib.h>
#include <string.h>

int locations_in_library(int value, void (*stop)(void));
int locations_interposed_target(int value, void (*stop)(void));
int locations_interposing(int value, void (*stop)(void));
int locations_interposed(int value, void (*stop)(void));

struct pair {
    long first;
    long second;
};

struct point {
    double x;
    double y;
};

struct ref {
    int *pointer;
    int count;
};

volatile long locations_sink;
static volatile int locations_aborts;

static volatile int locations_base = 100;

__attribute__((noinline)) static void observe(long value) {
    locations_sink = value;
}

__attribute__((noinline)) static void locations_stop(void) {
    if (locations_aborts) {
        abort();
    }
    observe(0);
}

__attribute__((noinline)) static int implicit_member(int seed) {
    int local = seed + 1;
    struct ref ref = {&local, seed};
    locations_stop();
    observe(*ref.pointer);
    observe(ref.count);
    return *ref.pointer + ref.count;
}

__attribute__((noinline)) static int removed_parameter(int used, int unused, int more) {

    (void)unused;
    observe(used);
    int result = implicit_member(used + more);
    observe(more);
    return result + used;
}

__attribute__((noinline)) static int chain_inner(int value) {
    observe(value);
    int base = locations_base;
    int result = removed_parameter(base, base + 101, base + 202);
    observe(result);
    return result + base;
}

__attribute__((noinline)) static int chain_outer(int value) {
    int result = chain_inner(value);
    observe(result);
    return result + 2;
}

__attribute__((noinline)) static int entry_values(int first, int second) {
    observe(first);
    observe(second);
    int result = chain_outer(first + second);
    observe(result);
    return result + 1;
}

__attribute__((noinline)) static long split_point(struct point point) {
    int base = locations_base;
    long result = entry_values(base + 1, base + 2);
    return result + base + (long)(point.x * point.y);
}

__attribute__((noinline)) static long wide_value(long seed) {
    __int128 value = ((__int128)seed << 64) | (unsigned long)(seed * 3);
    struct point point = {seed + 0.5, seed + 1.5};
    long result = split_point(point);
    return result + (long)(value >> 64) + (long)value;
}

__attribute__((noinline)) static long split_record(struct pair pair) {
    observe(pair.first);
    long result = wide_value(pair.second);
    return result + pair.first + pair.second;
}


__attribute__((noinline)) static int tail_target(int value) {
    observe(value);
    locations_stop();
    return locations_base;
}


__attribute__((noinline)) static int relay(int value) {
    observe(value);
    return tail_target(value + 1);
}

static int pong(int count);



__attribute__((noinline)) static int ping(int count) {
    observe(count);
    if (count <= 0) {
        locations_stop();
        return 0;
    }
    return pong(count - 1);
}

__attribute__((noinline)) static int pong(int count) {
    observe(count);
    return ping(count - 1);
}


__attribute__((noinline)) static int forwarded(int value) {
    observe(value);
    locations_stop();
    return locations_base + 1;
}


__attribute__((noinline)) static int forwarding(int value) {
    return forwarded(value) + 1;
}


static int (*volatile locations_forwarding)(int) = forwarding;



__attribute__((noinline)) int locations_interposed(int value, void (*stop)(void)) {
    return locations_interposed_target(value + 100, stop);
}

int main(int argc, char **argv) {
    if (argc > 3) {
        return locations_forwarding(argc) == 0;
    }
    if (argc > 1 && strcmp(argv[1], "tail") == 0) {
        return relay(argc + 40) + ping(argc + 2) == 0;
    }
    if (argc > 1 && strcmp(argv[1], "pointer") == 0) {

        int (*forward)(int) = locations_forwarding;
        return forward(argc) + forward(argc + 1) == 0;
    }
    if (argc > 1 && strcmp(argv[1], "interposed") == 0) {
        return locations_interposing(7, locations_stop) == 0;
    }
    if (argc > 1 && strcmp(argv[1], "library") == 0) {
        return locations_in_library(argc + 50, locations_stop) == 0;
    }
    locations_aborts = argc > 1 && strcmp(argv[1], "abort") == 0;
    struct pair pair = {argc + 10, argc + 20};
    return split_record(pair) == 0;
}
