





#include <any>
#include <array>
#include <atomic>
#include <bitset>
#include <chrono>
#include <cstdint>
#include <cstring>
#include <deque>
#include <expected>
#include <filesystem>
#include <flat_map>
#include <flat_set>
#include <forward_list>
#include <functional>
#include <initializer_list>
#include <list>
#include <map>
#include <memory>
#include <mutex>
#include <new>
#include <optional>
#include <queue>
#include <set>
#include <span>
#include <stack>
#include <string>
#include <string_view>
#include <thread>
#include <unordered_map>
#include <tuple>
#include <unordered_set>
#include <utility>
#include <variant>
#include <vector>

extern "C" __attribute__((noinline)) void barrier(void *fixture) {
    __asm__ volatile("" : : "r"(fixture) : "memory");
}



template <typename T> union Corrupt {
    T value;
    Corrupt() {}
    ~Corrupt() {}
};

template <typename T> static void keep(T &value) {
    __asm__ volatile("" : : "r"(&value) : "memory");
}


struct Shape {
    virtual ~Shape() = default;
    virtual int area() const = 0;
    int id = 7;
};
struct Square : Shape {
    int side = 3;
    int area() const override { return side * side; }
};
struct Named {
    virtual ~Named() = default;
    long tag = 2;
};

struct Tile : Named, Square {
    int row = 9;
};



struct Fragile {
    explicit Fragile(int) { throw 1; }
    Fragile(const Fragile &) {}
};



#ifdef _LIBCPP_VERSION
constexpr std::size_t next_link = 1;
#else
constexpr std::size_t next_link = 0;
#endif



template <typename T> static auto &plain(T &value) {
#ifdef _GLIBCXX_DEBUG
    return value._M_base();
#else
    return value;
#endif
}


template <typename T> static void **object_words(T &value) {
    return reinterpret_cast<void **>(&plain(value));
}

int main() {
    std::string text = "hello, world";            // VIEW: text => "hello, world"
    std::string empty_text;                       // VIEW: empty_text => ""
    std::string long_text(300, 'y');              // VIEW: long_text => "{y*256}"... (300 bytes)
    std::string with_nul("a\0b", 3);             // VIEW: with_nul => "a\u{0}b"
    std::string_view view = "a view";             // VIEW: view => "a view"


    std::wstring wide = L"wide \u00e9";            // VIEW: wide => "wide é"
    std::wstring long_wide(300, L'w');            // VIEW: long_wide => "{w*256}"... (1200 bytes)
    std::u16string short16 = u"ab";               // VIEW: short16 => "ab"
    std::u16string sixteen = u"\u03bb sixteen \U0001F980"; // VIEW: sixteen => "λ sixteen 🦀"
    std::u32string thirty_two = U"\U0001F980 32";  // VIEW: thirty_two => "🦀 32"
    std::u8string eight = u8"\u00e9ight";          // VIEW: eight => "éight"
    std::wstring_view wide_view = L"wide view";   // VIEW: wide_view => "wide view"
    std::vector<int> ints = {1, 2, 3};            // VIEW: ints => len=3 [1, 2, 3]
    std::vector<int> no_ints;                     // VIEW: no_ints => len=0 []
    std::vector<std::string> words = {"one", "two"}; // VIEW: words => len=2 ["one", "two"]
    std::vector<int> many(300);                   // VIEW: many => len=300 [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, …]
    for (int index = 0; index < 300; ++index) {
        many[index] = index;
    }
    std::array<int, 4> four = {4, 5, 6, 7};       // VIEW: four => len=4 [4, 5, 6, 7]
    std::array<int, 0> none{};                    // VIEW: none => len=0 []
    std::span<int> dynamic_span(ints);            // VIEW: dynamic_span => len=3 [1, 2, 3]
    std::span<int, 4> fixed_span(four);           // VIEW: fixed_span => len=4 [4, 5, 6, 7]


    int storage[16] = {10, 11, 12, 13};
    Corrupt<std::vector<int>> past_capacity;      // VIEW: past_capacity.value => problem: check
    int *past[3] = {storage, storage + 9, storage + 4};
    std::memcpy(static_cast<void *>(&plain(past_capacity.value)), past, sizeof past);

    Corrupt<std::vector<int>> dangling;           // VIEW: dangling.value => len=2 [<unavailable>, …]
    std::uintptr_t garbage[3] = {0x10, 0x18, 0x18};
    std::memcpy(static_cast<void *>(&plain(dangling.value)), garbage, sizeof garbage);

    Corrupt<std::vector<int>> ragged;             // VIEW: ragged.value => problem: whole number of elements
    char *bytes = reinterpret_cast<char *>(storage);
    char *ragged_ends[3] = {bytes, bytes + 6, bytes + 16};
    std::memcpy(static_cast<void *>(&plain(ragged.value)), ragged_ends, sizeof ragged_ends);


    std::map<int, int> ordered = {{3, 30}, {1, 10}, {2, 20}}; // VIEW: ordered => len=3 {1: 10, 2: 20, 3: 30}
    std::map<std::string, int> named = {{"two", 2}, {"one", 1}}; // VIEW: named => len=2 {"one": 1, "two": 2}
    std::map<int, int> no_entries;                // VIEW: no_entries => len=0 {}
    std::multimap<int, int> repeated_keys = {{1, 1}, {1, 2}}; // VIEW: repeated_keys => len=2 {1: 1, 1: 2}
    std::set<int> distinct = {5, 3, 4};           // VIEW: distinct => len=3 [3, 4, 5]
    std::multiset<int> repeated = {2, 1, 1};      // VIEW: repeated => len=3 [1, 1, 2]
    std::map<int, int> big_map;                   // VIEW: big_map => len=300 {0: 0, 1: 1, 2: 4, 3: 9, 4: 16, 5: 25, 6: 36, 7: 49, 8: 64, 9: 81, 10: 100, 11: 121, 12: 144, 13: 169, …}
    for (int index = 0; index < 300; ++index) {
        big_map[index] = index * index;
    }
    std::unordered_map<int, int> hashed = {{1, 10}, {2, 20}}; // VIEW: hashed => len=2 {1: 10, 2: 20} (any order)
    std::unordered_map<std::string, int> hashed_names = {{"one", 1}}; // VIEW: hashed_names => len=1 {"one": 1}
    std::unordered_multimap<int, int> hashed_repeats = {{1, 1}, {1, 2}}; // VIEW: hashed_repeats => len=2 {1: 1, 1: 2} (any order)
    std::unordered_set<int> hashed_set = {7, 8};  // VIEW: hashed_set => len=2 [7, 8] (any order)
    std::unordered_multiset<int> hashed_multiset = {9, 9}; // VIEW: hashed_multiset => len=2 [9, 9]
    std::unordered_map<int, int> no_hashed;       // VIEW: no_hashed => len=0 {}
    std::list<int> linked = {1, 2, 3};            // VIEW: linked => len=3 [1, 2, 3]
    std::list<std::string> linked_words = {"a", "b"}; // VIEW: linked_words => len=2 ["a", "b"]
    std::list<int> no_links;                      // VIEW: no_links => len=0 []
    std::forward_list<int> forward = {4, 5};      // VIEW: forward => len=2 [4, 5]
    std::forward_list<int> no_forward;            // VIEW: no_forward => len=0 []
    std::deque<int> queue;                        // VIEW: queue => len=1501 [-1, 0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, …]
    for (int index = 0; index < 1500; ++index) {
        queue.push_back(index);
    }
    queue.push_front(-1);
    std::deque<std::string> word_queue = {"x"};   // VIEW: word_queue => len=1 ["x"]
    std::deque<int> no_queue;                     // VIEW: no_queue => len=0 []

    std::unique_ptr<int> owned = std::make_unique<int>(42); // VIEW: owned => 42
    std::unique_ptr<int> no_owned;                // VIEW: no_owned => nullptr
    std::unique_ptr<int[]> owned_array(new int[3]{1, 2, 3}); // VIEW: owned_array => stored
    std::shared_ptr<int> shared = std::make_shared<int>(7); // VIEW: shared => 7
    std::shared_ptr<int> shared_too = shared;     // VIEW: shared_too => children: strong = 2, weak = 1, [raw]
    std::weak_ptr<int> weak = shared;             // VIEW: weak => 7
    std::weak_ptr<int> expired = std::make_shared<int>(1); // VIEW: expired => expired
    std::shared_ptr<int> no_shared;               // VIEW: no_shared => nullptr
    std::optional<int> some = 5;                  // VIEW: some => 5
    std::optional<int> nothing;                   // VIEW: nothing => nullopt
    std::optional<std::string> some_text = "hi";  // VIEW: some_text => "hi"
    std::variant<int, std::string> alternative = std::string("v"); // VIEW: alternative => "v"
    std::variant<int, std::string> first_alternative = 3; // VIEW: first_alternative => 3
    std::variant<int, Fragile> valueless = 1;    // VIEW: valueless => valueless
    try {
        valueless.emplace<1>(0);
    } catch (int) {
    }


    std::variant<int, std::string> bad_index = 1; // VIEW: bad_index => problem: the type has no type argument 5
    reinterpret_cast<unsigned char *>(&bad_index)[sizeof(std::string)] = 5;
    std::tuple<> no_elements;                     // VIEW: no_elements => ()
    std::tuple<int> single{1};                    // VIEW: single => (1)
    std::tuple<int, std::string> couple{1, "a"};  // VIEW: couple => (1, "a")
    std::tuple<int, char, double> triple{1, 'c', 2.5}; // VIEW: triple => (1, 99 'c', 2.5)
    std::tuple<int, int, int, int> quadruple{1, 2, 3, 4}; // VIEW: quadruple => children: 0 = 1, 1 = 2, 2 = 3, 3 = 4, [raw]
    std::tuple<int, int, int, int, int> quintuple{1, 2, 3, 4, 5}; // VIEW: quintuple => (1, 2, 3, 4, 5)
    std::tuple<int, int, int, int, int, int> sextuple{1, 2, 3, 4, 5, 6}; // VIEW: sextuple => (1, 2, 3, 4, 5, 6)
    std::tuple<int, int, int, int, int, int, int> septuple{}; // VIEW: septuple => stored
    std::unique_ptr<Shape> shape = std::make_unique<Square>(); // VIEW: shape => Square {id: 7, side: 3}
    Tile tile;
    Shape *inner_shape = &tile;                   // VIEW: *inner_shape => Tile {tag: 2, id: 7, side: 3, row: 9}
    std::pair<int, std::string> pair{1, "one"};  // VIEW: pair => {first: 1, second: "one"}


    std::vector<bool> bits = {true, false, true}; // VIEW: bits => len=3 [true, false, true]
    std::vector<bool> many_bits(130, false);      // VIEW: many_bits => count: 130
    many_bits[129] = true;
    std::vector<bool> no_bits;                    // VIEW: no_bits => len=0 []
    std::bitset<10> flags(0b1010000011);          // VIEW: flags => len=4 [0, 1, 7, 9]
    std::bitset<100> wide_flags;                  // VIEW: wide_flags => len=2 [3, 99]
    wide_flags.set(3);
    wide_flags.set(99);

    std::stack<int> stack;                        // VIEW: stack => len=2 [1, 2]
    stack.push(1);
    stack.push(2);
    std::queue<int> fifo;                         // VIEW: fifo => len=2 [3, 4]
    fifo.push(3);
    fifo.push(4);
    std::priority_queue<int> heap;                // VIEW: heap => len=3 [9, 5, 1]
    heap.push(5);
    heap.push(9);
    heap.push(1);
    std::initializer_list<int> listed = {1, 2, 3}; // VIEW: listed => len=3 [1, 2, 3]
    std::flat_map<int, int> flat{{2, 20}, {1, 10}}; // VIEW: flat => len=2 {1: 10, 2: 20}
    std::flat_set<int> flat_keys{3, 1};           // VIEW: flat_keys => len=2 [1, 3]

    std::chrono::nanoseconds nanos(42);           // VIEW: nanos => 42ns
    std::chrono::microseconds micros(1500);       // VIEW: micros => 1.5ms
    std::chrono::milliseconds millis(1500);       // VIEW: millis => 1.5s
    std::chrono::seconds secs(90);                // VIEW: secs => 1m30s
    std::chrono::minutes mins(3);                 // VIEW: mins => 3m0s
    std::chrono::hours hrs(26);                   // VIEW: hrs => 26h0m0s
    std::chrono::system_clock::time_point instant{std::chrono::seconds(1700000000)}; // VIEW: instant => 2023-11-14 22:13:20 +0000 UTC
    std::chrono::steady_clock::time_point uptime{std::chrono::seconds(5)}; // VIEW: uptime => 5s

    std::atomic<int> counter{5};                  // VIEW: counter => 5
    std::atomic<bool> ready{true};                // VIEW: ready => true
    std::atomic<long *> slot{nullptr};            // VIEW: slot => 0x0
    std::atomic<double> level{2.5};               // VIEW: level => 2.5
    int target = 3;
    std::reference_wrapper<int> reference = std::ref(target); // VIEW: reference => 3
    std::expected<int, std::string> expected = 5; // VIEW: expected => 5
    std::expected<int, std::string> unexpected = std::unexpected(std::string("bad")); // VIEW: unexpected => {unexpected: "bad"}
    std::filesystem::path where = "/tmp/microscope"; // VIEW: where => "/tmp/microscope"
    std::function<int(int)> no_callable;          // VIEW: no_callable => empty
    std::function<int(int)> callable = [&target](int x) { return x + target; };

    std::mutex unlocked_mutex;                    // VIEW: unlocked_mutex => unlocked
    std::mutex locked_mutex;                      // VIEW: locked_mutex => locked
    locked_mutex.lock();
    std::recursive_mutex reentered;               // VIEW: reentered => locked
    std::thread::id no_thread;                    // VIEW: no_thread => no thread
    std::thread::id this_thread = std::this_thread::get_id();
    std::any no_any;                              // VIEW: no_any => empty
    std::any held_any = 7;
    reentered.lock();
    reentered.lock();



    std::list<int> looped = {7, 8, 9};            // VIEW: looped => problem: cycle at element 3
    void **first = static_cast<void **>(object_words(looped)[next_link]);
    void **second = static_cast<void **>(first[next_link]);
    void **third = static_cast<void **>(second[next_link]);
    third[next_link] = second;


    std::list<int> overcounted = {1, 2};          // VIEW: overcounted => problem: the view declares 4 elements and generates 2
#if defined(_LIBCPP_VERSION) || _GLIBCXX_USE_CXX11_ABI
    object_words(looped)[2] = reinterpret_cast<void *>(5);
    object_words(overcounted)[2] = reinterpret_cast<void *>(4);
#endif

    keep(text), keep(empty_text), keep(long_text), keep(with_nul), keep(view);
    keep(wide), keep(long_wide), keep(short16), keep(sixteen), keep(thirty_two), keep(eight);
    keep(wide_view);
    keep(ints), keep(no_ints), keep(words), keep(many), keep(four), keep(none);
    keep(dynamic_span), keep(fixed_span), keep(past_capacity), keep(dangling), keep(ragged);
    keep(ordered), keep(named), keep(no_entries), keep(repeated_keys), keep(distinct);
    keep(repeated), keep(big_map), keep(hashed), keep(hashed_names), keep(hashed_repeats);
    keep(hashed_set), keep(hashed_multiset), keep(no_hashed), keep(linked), keep(linked_words);
    keep(no_links), keep(forward), keep(no_forward), keep(queue), keep(word_queue);
    keep(no_queue), keep(looped), keep(overcounted);
    keep(owned), keep(no_owned), keep(owned_array), keep(shared), keep(shared_too), keep(weak);
    keep(expired), keep(no_shared), keep(some), keep(nothing), keep(some_text), keep(alternative);
    keep(first_alternative), keep(valueless), keep(bad_index), keep(no_elements), keep(single);
    keep(couple), keep(triple), keep(quadruple), keep(quintuple), keep(sextuple), keep(septuple);
    keep(shape), keep(tile), keep(inner_shape), keep(pair);
    keep(bits), keep(many_bits), keep(no_bits), keep(flags), keep(wide_flags), keep(stack);
    keep(fifo), keep(heap), keep(listed), keep(flat), keep(flat_keys), keep(nanos), keep(micros);
    keep(millis), keep(secs), keep(mins), keep(hrs), keep(instant), keep(uptime), keep(counter);
    keep(ready), keep(slot), keep(level), keep(reference), keep(expected), keep(unexpected);
    keep(where), keep(no_callable), keep(callable), keep(unlocked_mutex), keep(locked_mutex);
    keep(reentered), keep(no_thread), keep(this_thread), keep(no_any), keep(held_any);
    barrier(&text);
    locked_mutex.unlock();
    reentered.unlock();
    reentered.unlock();

    reinterpret_cast<unsigned char *>(&bad_index)[sizeof(std::string)] = 0;

    new (&looped) std::list<int>();
    new (&overcounted) std::list<int>();
    return static_cast<int>(ints.size() + many.size()) == 303 ? 0 : 1;
}
