


#include <signal.h>

int main(void) {
    raise(SIGINT);
    return 0;
}
