#include <v8-inspector.h>
#include <uv.h>
#include <cassert>
#include <cstddef>
#include <cstdint>
#include <cstring>
#include <string>
#include <vector>

template <typename T>
static void check(const T* text, bool is8) {
    v8_inspector::StringView view(text, 123);
    static_assert(sizeof(view) == 24);
    const auto* bytes = reinterpret_cast<const unsigned char*>(&view);
    size_t length;
    const void* pointer;
    std::memcpy(&length, bytes + 8, sizeof(length));
    std::memcpy(&pointer, bytes + 16, sizeof(pointer));
    assert(bytes[0] == static_cast<unsigned char>(is8));
    assert(length == 123);
    assert(pointer == text);
}

int main() {
    static_assert(offsetof(uv_async_t, data) == 0);
    static_assert(offsetof(uv_async_t, pending) == 120);
    static_assert(sizeof(uv_async_t) == 128);
    static_assert(sizeof(std::vector<std::string>) == 24);
    static_assert(sizeof(std::string) == 32);
    std::vector<std::string> empty;
    uintptr_t pointers[3];
    std::memcpy(pointers, &empty, sizeof(pointers));
    assert(pointers[0] == 0 && pointers[1] == 0 && pointers[2] == 0);
    uint8_t narrow[] = {'x'};
    uint16_t wide[] = {'x'};
    check(narrow, true);
    check(wide, false);
}
