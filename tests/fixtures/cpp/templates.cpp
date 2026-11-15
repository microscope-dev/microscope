


#include <array>
#include <map>
#include <string>
#include <vector>




#if __has_include(<debug/vector>)
#include <debug/vector>
#define CHECKED_VECTOR 1
#endif

namespace outer {
inline namespace v1 {
struct Thing {
    int value;
};
}
}

template <typename... Ts> struct Pack {
    int count;
};

template <int N, typename T> struct Fixed {
    T items[N];
};

template <typename T> struct Node {
    T value;
    Node *next;
};

__attribute__((noinline)) int templates_target(const std::vector<int> &numbers,
                                               const std::string &text,
                                               const std::map<int, std::string> &names) {
    asm volatile("" : : "g"(&numbers), "g"(&text), "g"(&names) : "memory");
    return static_cast<int>(numbers.size() + text.size() + names.size()); // templates stop here
}

int main() {
    struct Local {
        int x;
    };
    std::vector<int> numbers = {1, 2, 3};
    std::string text = "templates";
    std::map<int, std::string> names = {{1, "one"}};
    std::array<int, 4> fixed_numbers = {1, 2, 3, 4};
    Pack<int, char, double> pack{3};
    Fixed<3, short> shorts{{1, 2, 3}};
    outer::Thing thing{7};
    Local local{1};
    Node<long> node{5, nullptr};
    asm volatile("" : : "g"(&fixed_numbers), "g"(&pack), "g"(&shorts), "g"(&thing), "g"(&local),
                 "g"(&node)
                 : "memory");
#ifdef CHECKED_VECTOR
    __gnu_debug::vector<int> checked = {4, 5};
    asm volatile("" : : "g"(&checked) : "memory");
#endif
    return templates_target(numbers, text, names) == 13 ? 0 : 1;
}
