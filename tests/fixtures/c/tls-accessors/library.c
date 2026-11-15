


#include <stddef.h>


__thread long tls_general = 1;



__attribute__((visibility("hidden"), tls_model("global-dynamic"))) __thread long tls_hidden = 2;


static __thread long tls_local_first = 3;
static __thread long tls_local = 4;



__attribute__((tls_model("initial-exec"))) __thread long tls_initial = 5;

long tls_general_get(void) { return tls_general; }
long tls_hidden_get(void) { return tls_hidden; }
long tls_local_get(void) { return tls_local_first + tls_local; }
long tls_initial_get(void) { return tls_initial; }


void tls_set(long value) {
    tls_general = value + 1;
    tls_hidden = value + 2;
    tls_local_first = value + 3;
    tls_local = value + 4;
    tls_initial = value + 5;
}
