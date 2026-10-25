/* A Node-API addon that holds JavaScript values in a native frame's locals
 * while it calls back into JavaScript, so that the frame, below the
 * JavaScript frames the callback runs, shows each napi_value as the value it
 * refers to; and one that calls nothing back, which a step enters and
 * leaves. Node-API is ABI-stable, so one build serves every Node. */

#include <node_api.h>

/* inspect(object, text, callback): calls callback with no arguments while
 * its locals hold object and text, and a void * the object's handle, and
 * returns what the callback returns. */
static napi_value inspect(napi_env env, napi_callback_info info) {
    size_t count = 3; /* INSPECT_FIRST */
    napi_value args[3];
    if (napi_get_cb_info(env, info, &count, args, NULL, NULL) != napi_ok || count != 3) {
        napi_throw_type_error(env, NULL, "inspect takes three arguments");
        return NULL;
    }

    napi_value object = args[0];
    napi_value text = args[1];
    void *opaque = object;
    napi_value global;
    napi_value result = NULL;
    if (napi_get_global(env, &global) != napi_ok ||
        napi_call_function(env, global, args[2], 0, NULL, &result) != napi_ok) { /* INSPECT_CALLS */
        return NULL;
    }

    /* The locals stay in the frame, where the debugger reads them, until
     * the callback returns. */
    __asm__ volatile("" : : "g"(object), "g"(text), "g"(opaque) : "memory");
    return result;
}

/* add(a, b): the sum of two numbers, calling no JavaScript. */
static napi_value add(napi_env env, napi_callback_info info) {
    size_t count = 2; /* ADD_FIRST */
    napi_value args[2];
    double a = 0;
    double b = 0;
    napi_value sum = NULL;
    if (napi_get_cb_info(env, info, &count, args, NULL, NULL) != napi_ok || count != 2 ||
        napi_get_value_double(env, args[0], &a) != napi_ok ||
        napi_get_value_double(env, args[1], &b) != napi_ok) {
        napi_throw_type_error(env, NULL, "add takes two numbers");
        return NULL;
    }
    napi_create_double(env, a + b, &sum);
    return sum; /* ADD_RETURN */
}

NAPI_MODULE_INIT() {
    napi_value function;
    if (napi_create_function(env, "inspect", NAPI_AUTO_LENGTH, inspect, NULL, &function) !=
            napi_ok ||
        napi_set_named_property(env, exports, "inspect", function) != napi_ok ||
        napi_create_function(env, "add", NAPI_AUTO_LENGTH, add, NULL, &function) != napi_ok ||
        napi_set_named_property(env, exports, "add", function) != napi_ok) {
        return NULL;
    }
    return exports;
}
