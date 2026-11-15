


#include <cstdlib>

namespace names {

struct Worker {
    unsigned long state = 1;
    __attribute__((noinline)) unsigned long spin(unsigned long rounds);
    unsigned long next() const { return state * 2654435761u + 1; }
};

unsigned long Worker::spin(unsigned long rounds) {
    unsigned long total = 0;
    auto step = [&](unsigned long round) {
        total += next() ^ round;
        state = total | 1;
    };
    for (unsigned long round = 0; round < rounds; ++round) {
        step(round);
    }
    return total;
}

}

int main(int argc, char **argv) {
    unsigned long rounds = argc > 1 ? std::strtoul(argv[1], nullptr, 10) : 1;
    names::Worker worker;
    volatile unsigned long sink = 0;
    for (int call = 0; call < 40; ++call) {
        sink = sink + worker.spin(rounds);
    }
    return 0;
}
