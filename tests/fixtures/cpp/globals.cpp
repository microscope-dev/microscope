#include <cstdint>

volatile std::int32_t global_sink;

namespace fixture {
inline volatile std::int32_t inline_value = 111;
static volatile std::int32_t namespace_static = 112;

namespace alpha {
volatile std::int32_t duplicate = 121;
}

namespace beta {
volatile std::int32_t duplicate = 122;
}

namespace {
volatile std::int32_t anonymous_value = 123;
}

struct Holder {
    static volatile std::int32_t member;
    inline static volatile std::int32_t inline_member = 132;
    static constexpr std::int32_t constexpr_member = 133;
    static constexpr std::int32_t negative_constexpr_member = -123;
};

volatile std::int32_t Holder::member = 131;
}

__attribute__((noinline)) void inspect_globals() {
    global_sink = fixture::inline_value + fixture::namespace_static +
                  fixture::alpha::duplicate + fixture::beta::duplicate +
                  fixture::anonymous_value + fixture::Holder::member +
                  fixture::Holder::inline_member +
                  fixture::Holder::constexpr_member +
                  fixture::Holder::negative_constexpr_member;
}

int main() {
    inspect_globals();
    return global_sink == 862 ? 0 : 1;
}
