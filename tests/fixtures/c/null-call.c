

#include <stddef.h>

typedef void (*callback_fn)(void);

volatile callback_fn null_callback = NULL;

int main(void) {
    null_callback();
    return 0;
}
