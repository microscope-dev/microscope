#define _GNU_SOURCE
#include <node_api.h>
#include <arpa/inet.h>
#include <assert.h>
#include <fcntl.h>
#include <pthread.h>
#include <stdatomic.h>
#include <stdint.h>
#include <stdlib.h>
#include <sys/socket.h>
#include <sys/syscall.h>
#include <unistd.h>

// Interpose the actual C++ payload allocations, not V8's managed heap.
static pthread_mutex_t lock = PTHREAD_MUTEX_INITIALIZER;
static void* tracked[16384];
static size_t outstanding;
static void* const tombstone = (void*)(uintptr_t)1;

void* _Znwm(size_t size) {
    void* pointer = malloc(size ? size : 1);
    assert(pointer);
    if (size == 37) {
        assert(pthread_mutex_lock(&lock) == 0);
        size_t at = ((uintptr_t)pointer >> 4) % 16384;
        while (tracked[at] && tracked[at] != tombstone)
            at = (at + 1) % 16384;
        tracked[at] = pointer;
        outstanding++;
        assert(pthread_mutex_unlock(&lock) == 0);
    }
    return pointer;
}

void _ZdlPv(void* pointer) {
    assert(pthread_mutex_lock(&lock) == 0);
    size_t at = ((uintptr_t)pointer >> 4) % 16384;
    size_t attempts = 0;
    while (tracked[at] && attempts++ < 16384) {
        if (tracked[at] == pointer) {
            tracked[at] = tombstone;
            outstanding--;
            break;
        }
        at = (at + 1) % 16384;
    }
    assert(pthread_mutex_unlock(&lock) == 0);
    free(pointer);
}

void _ZdlPvm(void* pointer, size_t size) {
    (void)size;
    _ZdlPv(pointer);
}

static napi_value snapshot(napi_env env, napi_callback_info info) {
    (void)info;
    assert(pthread_mutex_lock(&lock) == 0);
    double value = (double)outstanding;
    assert(pthread_mutex_unlock(&lock) == 0);
    napi_value result;
    assert(napi_create_double(env, value, &result) == napi_ok);
    return result;
}

NAPI_MODULE_INIT() {
    napi_value function;
    assert(napi_create_function(env, "outstanding", NAPI_AUTO_LENGTH,
                                snapshot, NULL, &function) == napi_ok);
    assert(napi_set_named_property(env, exports, "outstanding", function) == napi_ok);
    return exports;
}

static int listing_socket;
static void* collect_listing(void* argument) {
    const char* path = argument;
    int report = open(path, O_CREAT | O_WRONLY | O_TRUNC, 0600);
    assert(report >= 0);
    char bytes[4096];
    ssize_t length;
    while ((length = read(listing_socket, bytes, sizeof(bytes))) > 0) {
        ssize_t done = 0;
        while (done < length) {
            ssize_t written = write(report, bytes + done, (size_t)(length - done));
            assert(written > 0);
            done += written;
        }
    }
    assert(length == 0);
    assert(close(listing_socket) == 0);
    assert(close(report) == 0);
    return NULL;
}

// Queue the hostile listing before Start's GetTargetIds can initiate the
// debugger's connection. Both requests then run on the real inspector loop.
int listen(int socket, int backlog) {
    int result = (int)syscall(SYS_listen, socket, backlog);
    const char* report = getenv("MICROSCOPE_LISTING_REPORT");
    static atomic_bool queued;
    if (result == 0 && report && !atomic_exchange(&queued, 1)) {
        struct sockaddr_in address;
        socklen_t size = sizeof(address);
        assert(getsockname(socket, (struct sockaddr*)&address, &size) == 0);
        assert(address.sin_family == AF_INET);
        address.sin_addr.s_addr = htonl(INADDR_LOOPBACK);
        listing_socket = (int)syscall(SYS_socket, AF_INET, SOCK_STREAM, 0);
        assert(listing_socket >= 0);
        assert(connect(listing_socket, (struct sockaddr*)&address, size) == 0);
        static const char request[] =
            "GET /json/list HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n";
        assert(write(listing_socket, request, sizeof(request) - 1) == sizeof(request) - 1);
        pthread_t reader;
        assert(pthread_create(&reader, NULL, collect_listing, (void*)report) == 0);
        assert(pthread_detach(reader) == 0);
    }
    return result;
}
