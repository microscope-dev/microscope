#define _GNU_SOURCE

#include <linux/hw_breakpoint.h>
#include <linux/perf_event.h>
#include <pthread.h>
#include <sched.h>
#include <stdatomic.h>
#include <stdint.h>
#include <string.h>
#include <sys/syscall.h>
#include <unistd.h>





#ifndef STOLEN_SLOTS
#error "STOLEN_SLOTS must be defined"
#endif

enum { UNAVAILABLE = 77 };

volatile uint64_t thief_target[4];
static volatile uint64_t thief_decoys[4];

static int take_slots(int count) {
    for (int index = 0; index < count; ++index) {
        struct perf_event_attr attribute;
        memset(&attribute, 0, sizeof attribute);
        attribute.type = PERF_TYPE_BREAKPOINT;
        attribute.size = sizeof attribute;
        attribute.bp_type = HW_BREAKPOINT_W;
        attribute.bp_addr = (uint64_t)(uintptr_t)&thief_decoys[index];
        attribute.bp_len = HW_BREAKPOINT_LEN_8;
        attribute.exclude_kernel = 1;
        attribute.exclude_hv = 1;
        if (syscall(SYS_perf_event_open, &attribute, 0, -1, -1, 0) < 0) {
            return -1;
        }
    }
    return 0;
}

__attribute__((noinline)) void slots_taken(void) {
    thief_decoys[0] = 0;
}

#ifdef STOLEN_BY_WORKER
static _Atomic int worker_ready;
static _Atomic int release_worker;

static void *thief(void *argument) {
    (void)argument;
    if (take_slots(STOLEN_SLOTS) != 0) {
        _exit(UNAVAILABLE);
    }
    atomic_store(&worker_ready, 1);
    while (!atomic_load(&release_worker)) {
        sched_yield();
    }
    return NULL;
}
#endif

int main(void) {
#ifdef STOLEN_BY_WORKER
    pthread_t worker;
    if (pthread_create(&worker, NULL, thief, NULL) != 0) {
        return 2;
    }
    while (!atomic_load(&worker_ready)) {
        sched_yield();
    }
#else
    if (take_slots(STOLEN_SLOTS) != 0) {
        return UNAVAILABLE;
    }
#endif
    slots_taken();
    thief_target[0] = 1;
    thief_target[1] = 2;
#ifdef STOLEN_BY_WORKER
    atomic_store(&release_worker, 1);
    pthread_join(worker, NULL);
#endif
    return 0;
}
