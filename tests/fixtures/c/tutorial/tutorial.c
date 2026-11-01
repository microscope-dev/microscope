// Embedded views and a kernel built with the C SDK.
// Each `VIEW:` marker says what its expression shows.

#include <stddef.h>

#include "microscope_views.h"

MICROSCOPE_VIEWS_FILE("tests/fixtures/c/tutorial/tutorial.views");

// The kernel its tree's view calls, built from tree.c into a directory the
// assembler is given with -Wa,-I.
MICROSCOPE_KERNEL("tree", "tests/fixtures/c/tutorial/tree.c", "tutorial-tree.wasm");

// A vector of integers: `n` of them at `data`, with room for `cap`.
typedef struct {
    int *data;
    size_t n;
    size_t cap;
} intvec;

// A value of one of three kinds.
enum kind { VAL_INT, VAL_STR, VAL_NIL };

typedef struct {
    enum kind kind;
    union {
        long i;
        struct {
            const char *ptr;
            size_t len;
        } s;
    } u;
} value;

// Tasks on a run queue, linked through the node each embeds.
struct list_head {
    struct list_head *next, *prev;
};

struct task {
    int pid;
    struct list_head run_node;
};

struct run_queue {
    struct list_head tasks;
    size_t nr;
};

// A tree whose nodes keep their children in a list.
typedef struct node {
    int value;
    struct node *child;
    struct node *sibling;
} node;

struct tree {
    node *root;
    size_t count;
};

__attribute__((noinline)) void barrier(void *fixture) {
    __asm__ volatile("" : : "r"(fixture) : "memory");
}

static void enqueue(struct run_queue *queue, struct task *task) {
    struct list_head *last = queue->tasks.prev;
    task->run_node.prev = last;
    task->run_node.next = &queue->tasks;
    last->next = &task->run_node;
    queue->tasks.prev = &task->run_node;
    queue->nr++;
}

int main(void) {
    int storage[] = {10, 20, 30, 0};
    intvec numbers = {storage, 3, 4}; // VIEW: numbers => len=3 [10, 20, 30]
    intvec broken = {storage, 9, 4};  // VIEW: broken => problem: check `n <= cap` failed
    value count = {VAL_INT, {.i = 42}};                // VIEW: count => 42
    value name = {VAL_STR, {.s = {"microscope", 10}}}; // VIEW: name => "microscope"
    value nothing = {VAL_NIL, {.i = 0}};               // VIEW: nothing => nil
    struct run_queue queue = {{&queue.tasks, &queue.tasks}, 0}; // VIEW: queue => len=2 [7, 8]
    struct task first = {7, {0, 0}};
    struct task second = {8, {0, 0}};
    enqueue(&queue, &first);
    enqueue(&queue, &second);
    node leaves[] = {{3, 0, &leaves[1]}, {4, 0, 0}};
    node branches[] = {{2, &leaves[0], &branches[1]}, {5, 0, 0}};
    node top = {1, &branches[0], 0};
    struct tree family = {&top, 5}; // VIEW: family => len=5 [1, 2, 3, 4, 5]
    barrier(&numbers);
    barrier(&broken);
    barrier(&count);
    barrier(&name);
    barrier(&nothing);
    barrier(&queue);
    barrier(&family);
    return 0;
}
