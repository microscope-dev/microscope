// Thread-local variables a library reaches by each of the x86-64 TLS access
// models, each through an exported accessor, as V8's
// v8::Isolate::GetCurrent reaches g_current_isolate_.
#include <stddef.h>

// General dynamic: a variable another module could interpose.
__thread long tls_general = 1;

// General dynamic too, of a variable no other module sees, as V8's
// g_current_isolate_ is: its GOT pair names no symbol.
__attribute__((visibility("hidden"), tls_model("global-dynamic"))) __thread long tls_hidden = 2;

// Local dynamic: two statics of one module share one block lookup.
static __thread long tls_local_first = 3;
static __thread long tls_local = 4;

// Initial exec: a GOT slot the loader fills with the offset from the
// thread pointer.
__attribute__((tls_model("initial-exec"))) __thread long tls_initial = 5;

long tls_general_get(void) { return tls_general; }
long tls_hidden_get(void) { return tls_hidden; }
long tls_local_get(void) { return tls_local_first + tls_local; }
long tls_initial_get(void) { return tls_initial; }

// Writes each variable for this thread.
void tls_set(long value) {
    tls_general = value + 1;
    tls_hidden = value + 2;
    tls_local_first = value + 3;
    tls_local = value + 4;
    tls_initial = value + 5;
}
