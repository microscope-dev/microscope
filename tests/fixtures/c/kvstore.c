



#include <pthread.h>
#include <stdio.h>
#include <string.h>
#include <time.h>

enum op { OP_GET, OP_PUT, OP_DEL };

struct entry {
    char key[16];
    char value[16];
    size_t len;
    struct entry *next;
};

struct table {
    struct entry *buckets[8];
    struct entry slots[64];
    size_t count;
};

struct stats {
    unsigned long gets;
    unsigned long puts;
    unsigned long misses;
};

struct server {
    struct table table;
    struct stats stats;
    pthread_mutex_t lock;
};

struct request {
    enum op op;
    char key[16];
    const char *value;
    size_t len;
};

struct queue {
    struct request items[16];
    size_t head;
    size_t tail;
    int closed;
    pthread_mutex_t lock;
    pthread_cond_t changed;
};

static struct server server = {.lock = PTHREAD_MUTEX_INITIALIZER};
static struct queue queue = {.lock = PTHREAD_MUTEX_INITIALIZER, .changed = PTHREAD_COND_INITIALIZER};
static const char *names[] = {"alice", "bob", "carol", "dave"};

static unsigned hash(const char *key) {
    unsigned value = 5381;
    while (*key) {
        value = value * 33 + (unsigned char)*key++;
    }
    return value;
}

static struct entry *table_find(struct table *table, const char *key) {
    for (struct entry *e = table->buckets[hash(key) % 8]; e != NULL; e = e->next) {
        if (strcmp(e->key, key) == 0) {
            return e;
        }
    }
    return NULL;
}

static struct entry *table_insert(struct table *table, const char *key) {
    struct entry *e = &table->slots[table->count++ % 64];
    snprintf(e->key, sizeof e->key, "%s", key);
    unsigned bucket = hash(key) % 8;
    e->next = table->buckets[bucket];
    table->buckets[bucket] = e;
    return e;
}

static void entry_set(struct entry *e, const char *value, size_t len) {
    snprintf(e->value, sizeof e->value, "%.*s", (int)len, value);
    e->len = len;
}


static int handle_request(struct server *s, const struct request *req) {
    pthread_mutex_lock(&s->lock);
    struct entry *e = table_find(&s->table, req->key);
    int status = 0;
    switch (req->op) {
    case OP_GET:
        s->stats.gets++;
        if (e == NULL) {
            s->stats.misses++;
            status = -1;
        }
        break;
    case OP_PUT:
        if (e == NULL) {
            e = table_insert(&s->table, req->key);
        }
        s->stats.puts++;
        entry_set(e, req->value, req->len);
        break;
    case OP_DEL:
        status = e == NULL ? -1 : 0;
        break;
    }
    pthread_mutex_unlock(&s->lock);
    return status;
}

static int next_request(struct request *req) {
    pthread_mutex_lock(&queue.lock);
    while (queue.head == queue.tail && !queue.closed) {
        pthread_cond_wait(&queue.changed, &queue.lock);
    }
    int got = queue.head != queue.tail;
    if (got) {
        *req = queue.items[queue.head++ % 16];
        pthread_cond_broadcast(&queue.changed);
    }
    pthread_mutex_unlock(&queue.lock);
    return got;
}

static void *worker(void *argument) {
    long id = (long)argument;
    struct request req;
    while (next_request(&req)) {
        int status = handle_request(&server, &req);
        printf("worker %ld: %s %s -> %d\n", id, req.op == OP_PUT ? "put" : "get", req.key, status);
        fflush(stdout);
    }
    return NULL;
}

static void submit(const struct request *req) {
    pthread_mutex_lock(&queue.lock);
    while (queue.tail - queue.head == 16) {
        pthread_cond_wait(&queue.changed, &queue.lock);
    }
    queue.items[queue.tail++ % 16] = *req;
    pthread_cond_broadcast(&queue.changed);
    pthread_mutex_unlock(&queue.lock);
}

static volatile int input_ended;

static void *read_input(void *argument) {
    (void)argument;
    char line[128];
    while (fgets(line, sizeof line, stdin) != NULL) {
        printf("input: %s", line);
        fflush(stdout);
    }
    input_ended = 1;
    return NULL;
}

int main(void) {
    pthread_t workers[2];
    pthread_t input;
    for (long id = 0; id < 2; ++id) {
        pthread_create(&workers[id], NULL, worker, (void *)(id + 1));
    }
    pthread_create(&input, NULL, read_input, NULL);

    const struct timespec pause = {.tv_sec = 0, .tv_nsec = 20 * 1000 * 1000};
    for (unsigned long round = 0; !input_ended; ++round) {
        struct request req = {.op = round % 3 == 2 ? OP_GET : OP_PUT};
        snprintf(req.key, sizeof req.key, "user:%lu", 1000 + round % 24);
        req.value = names[round % 4];
        req.len = strlen(req.value);
        submit(&req);
        nanosleep(&pause, NULL);
    }
    pthread_mutex_lock(&queue.lock);
    queue.closed = 1;
    pthread_cond_broadcast(&queue.changed);
    pthread_mutex_unlock(&queue.lock);
    for (int id = 0; id < 2; ++id) {
        pthread_join(workers[id], NULL);
    }
    pthread_join(input, NULL);
    printf("served %lu puts, %lu gets, %lu misses\n", server.stats.puts, server.stats.gets,
           server.stats.misses);
    return 0;
}
