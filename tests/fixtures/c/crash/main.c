

#include <pthread.h>
#include <stdatomic.h>
#include <stdint.h>
#include <stdlib.h>
#include <string.h>

#define WORKERS 3

int32_t crash_library_touch(int32_t worker);

struct crash_record {
    int32_t id;
    const char *label;
    int32_t *poison;
};

const char crash_message[] = "post-mortem read-only message";
const char *const crash_label = "read-only label";
#ifdef CRASH_REBUILT

volatile int32_t crash_counter = 9;
#else
volatile int32_t crash_counter = 7;
#endif

volatile double crash_scale = 2.5;
static volatile int32_t crash_zeroed;
_Thread_local volatile int32_t crash_tls = 100;
static atomic_int ready;
static atomic_int release;

__attribute__((noinline)) static void worker_spin(int32_t worker_id) {
    volatile int32_t local_id = worker_id;
    volatile int64_t squared = (int64_t)worker_id * worker_id;
    crash_tls = 100 + worker_id;
    crash_library_touch(worker_id);
    atomic_fetch_add(&ready, 1);
    while (atomic_load(&release) == 0) {
    }
    crash_zeroed = (int32_t)(local_id + squared);
}

static void *worker_main(void *argument) {
    worker_spin((int32_t)(intptr_t)argument);
    return NULL;
}

__attribute__((noinline)) static double crash_segv(struct crash_record *record, int32_t depth,
                                                   double scale) {
    volatile int32_t local_depth = depth * 2;
    crash_zeroed = record->id + local_depth;

    *record->poison = depth;
    return scale * depth;
}

__attribute__((noinline)) static void crash_abort(struct crash_record *record) {
    crash_zeroed = record->id;
    abort();
}

int main(int argc, char **argv) {


    pthread_attr_t attributes;
    if (pthread_attr_init(&attributes) != 0 || pthread_attr_setguardsize(&attributes, 0) != 0) {
        return 1;
    }
    pthread_t threads[WORKERS];
    for (intptr_t index = 0; index < WORKERS; ++index) {
        if (pthread_create(&threads[index], &attributes, worker_main, (void *)(index + 1)) != 0) {
            return 1;
        }
    }
    crash_library_touch(0);
    while (atomic_load(&ready) != WORKERS) {
    }
    struct crash_record record = {.id = 42, .label = crash_label, .poison = NULL};
    crash_counter += 1;
    if (argc > 1 && strcmp(argv[1], "abort") == 0) {
        crash_abort(&record);
    }
    return (int)crash_segv(&record, 3, crash_scale);
}
