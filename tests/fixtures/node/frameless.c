/* A Node-API addon whose functions run with rbp holding no frame pointer,
 * as code built without frame pointers may use it, so that a walk of the
 * frame-pointer chain from them reads nothing; their call-frame
 * information still unwinds them. Built with -fomit-frame-pointer and
 * -mno-red-zone, so that the call-frame information is by the stack
 * pointer and the pushes below clobber nothing.
 *
 * open(path): opens path read-only with the openat system call, made while
 * rbp holds 8, and returns its descriptor or -errno.
 * spin(rounds): counts rounds down to 0 while rbp holds 8. */

#include <node_api.h>

/* Opens path while rbp holds 8. */
static long frameless_open(const char *path) {
    long result;
    __asm__ volatile("push %%rbp\n\t"
                     ".cfi_adjust_cfa_offset 8\n\t"
                     ".cfi_rel_offset %%rbp, 0\n\t"
                     "mov $8, %%rbp\n\t"
                     "syscall\n\t"
                     "pop %%rbp\n\t"
                     ".cfi_adjust_cfa_offset -8\n\t"
                     ".cfi_restore %%rbp\n\t"
                     : "=a"(result)
                     : "0"(257L), "D"(-100L), "S"(path), "d"(0L)
                     : "rcx", "r11", "memory");
    return result;
}

/* Counts rounds down to 0 while rbp holds 8. */
static void frameless_spin(unsigned long rounds) {
    __asm__ volatile("push %%rbp\n\t"
                     ".cfi_adjust_cfa_offset 8\n\t"
                     ".cfi_rel_offset %%rbp, 0\n\t"
                     "mov $8, %%rbp\n\t"
                     "1:\n\t"
                     "sub $1, %0\n\t"
                     "jnz 1b\n\t"
                     "pop %%rbp\n\t"
                     ".cfi_adjust_cfa_offset -8\n\t"
                     ".cfi_restore %%rbp\n\t"
                     : "+r"(rounds)
                     :
                     : "cc");
}

static napi_value open_path(napi_env env, napi_callback_info info) {
    size_t count = 1;
    napi_value arg;
    char path[256];
    size_t length = 0;
    napi_value result;
    if (napi_get_cb_info(env, info, &count, &arg, NULL, NULL) != napi_ok || count != 1 ||
        napi_get_value_string_utf8(env, arg, path, sizeof(path), &length) != napi_ok ||
        napi_create_int64(env, frameless_open(path), &result) != napi_ok) {
        napi_throw_type_error(env, NULL, "open takes a path");
        return NULL;
    }
    return result;
}

static napi_value spin(napi_env env, napi_callback_info info) {
    size_t count = 1;
    napi_value arg;
    int64_t rounds = 0;
    if (napi_get_cb_info(env, info, &count, &arg, NULL, NULL) != napi_ok || count != 1 ||
        napi_get_value_int64(env, arg, &rounds) != napi_ok || rounds < 1) {
        napi_throw_type_error(env, NULL, "spin takes a positive count");
        return NULL;
    }
    frameless_spin((unsigned long)rounds);
    return NULL;
}

NAPI_MODULE_INIT() {
    napi_value function;
    if (napi_create_function(env, "open", NAPI_AUTO_LENGTH, open_path, NULL, &function) !=
            napi_ok ||
        napi_set_named_property(env, exports, "open", function) != napi_ok ||
        napi_create_function(env, "spin", NAPI_AUTO_LENGTH, spin, NULL, &function) != napi_ok ||
        napi_set_named_property(env, exports, "spin", function) != napi_ok) {
        return NULL;
    }
    return exports;
}
