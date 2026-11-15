






#include <math.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#define ENDPOINTS 4
#define CORES 8
#define WINDOW 24
#define FB_COLUMNS 64
#define FB_ROWS 40


struct samples {
    double *data;
    size_t count;
};

struct metrics {
    int tick;
    struct samples cpu;
    struct samples latency;
    struct samples endpoints[ENDPOINTS];
    const char *endpoint_names[ENDPOINTS];
    struct samples sizes;
    struct samples times;
    double misses[CORES][WINDOW];
    const char *core_names[CORES];
    uint8_t framebuffer[FB_ROWS][FB_COLUMNS];
    uint64_t flags;
};



static double spread(size_t n) {
    double x = (double)n * 0.6180339887498949;
    return x - floor(x);
}

static struct samples allocate(size_t count) {
    struct samples run = {calloc(count, sizeof(double)), count};
    if (run.data == NULL) {
        exit(1);
    }
    return run;
}

static void fill(struct metrics *m) {
    for (size_t i = 0; i < m->cpu.count; i++) {
        m->cpu.data[i] = 40.0 + 30.0 * sin((double)(i + m->tick) / 50.0) + 5.0 * spread(i);
    }

    m->cpu.data[m->cpu.count * 3 / 5 + 1] = 250.0 + m->tick;
    m->cpu.data[m->cpu.count / 10] = NAN;

    for (size_t i = 0; i < m->latency.count; i++) {
        m->latency.data[i] = exp(2.5 + 0.5 * (spread(i) + spread(i * 7 + 3) - 1.0) * 2.0) + m->tick;
    }

    for (int e = 0; e < ENDPOINTS; e++) {
        struct samples *run = &m->endpoints[e];
        for (size_t i = 0; i < run->count; i++) {
            run->data[i] = 10.0 * (e + 1) + 8.0 * spread(i + (size_t)e * 1000) + (i % 50 == 0 ? 40.0 : 0.0) + m->tick;
        }
    }

    for (size_t i = 0; i < m->sizes.count; i++) {
        m->sizes.data[i] = 1.0 + 150.0 * spread(i);
        m->times.data[i] = 2.0 + m->sizes.data[i] * (i % 3 == 0 ? 0.09 : 0.05) + 3.0 * spread(i * 5 + 1) + 0.1 * m->tick;
    }

    for (int core = 0; core < CORES; core++) {
        for (int t = 0; t < WINDOW; t++) {
            m->misses[core][t] = round(900.0 * sin((double)(t + m->tick) / 3.5 + core));
        }
    }
    m->misses[3][8] = NAN;

    for (int row = 0; row < FB_ROWS; row++) {
        for (int column = 0; column < FB_COLUMNS; column++) {
            m->framebuffer[row][column] = (uint8_t)(((row + column + m->tick) % 16) * 16);
        }
    }

    m->flags = (0x8000000000000001ull << (m->tick % 4)) | (uint64_t)m->tick << 8;
}

int main(int argc, char **argv) {
    static struct metrics m;
    int large = argc > 1 && strcmp(argv[1], "--large") == 0;
    int ticks = 100;
    m.cpu = allocate(large ? 1000000 : 600);
    m.latency = allocate(large ? 1000000 : 5000);
    for (int e = 0; e < ENDPOINTS; e++) {
        m.endpoints[e] = allocate(300);
    }
    m.endpoint_names[0] = "search";
    m.endpoint_names[1] = "items";
    m.endpoint_names[2] = "login";
    m.endpoint_names[3] = "cart";
    m.sizes = allocate(large ? 5000 : 700);
    m.times = allocate(m.sizes.count);
    static const char *cores[CORES] = {"cpu0", "cpu1", "cpu2", "cpu3", "cpu4", "cpu5", "cpu6", "cpu7"};
    memcpy(m.core_names, cores, sizeof cores);

    for (m.tick = 1; m.tick <= ticks; m.tick++) {
        fill(&m);
        double total = 0.0;
        for (size_t i = 0; i < m.latency.count; i++) {
            total += m.latency.data[i];
        }
        printf("tick %d: mean latency %.3f\n", m.tick, total / (double)m.latency.count);
        fflush(stdout);
    }
    return 0;
}
