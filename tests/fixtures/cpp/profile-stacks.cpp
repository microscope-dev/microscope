

#include <cstdlib>
#include <thread>

namespace stacks {

volatile unsigned long sink;

__attribute__((noinline)) void middle(unsigned long iterations);

__attribute__((noinline)) void outer(unsigned long iterations) {
    for (int call = 0; call < 3; ++call) {
        middle(iterations);
    }
    sink = sink + 1;
}

}

extern "C" __attribute__((noinline)) void leaf(unsigned long iterations) {
    for (unsigned long index = 0; index < iterations; ++index) {
        stacks::sink = stacks::sink + index;
    }
}

namespace stacks {

__attribute__((noinline)) void middle(unsigned long iterations) {
    leaf(iterations);
    sink = sink + 1;
}

}

int main(int argc, char **argv) {
    unsigned long iterations = argc > 1 ? std::strtoul(argv[1], nullptr, 10) : 1;
    std::thread worker([iterations] { stacks::outer(iterations); });
    stacks::outer(iterations);
    worker.join();
    return 0;
}
