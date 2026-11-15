




#include <cstdint>
#include <cstdio>
#include <cstring>
#include <map>
#include <string>
#include <vector>

extern "C" void barrier(void *fixture);

namespace shapes {
struct Inner {
    short s;
    long long ll;
};
}



struct Base {
    int base_value;
};

struct Top {
    int top_value;
};

struct Left : virtual Top {
    int left_value;
};

struct Right : virtual Top {
    int right_value;
};

struct Diamond : Left, Right {
    int own_value;
};

struct Fixture : Base {
    int count;
    unsigned char byte;
    shapes::Inner inner;
    int arr[4];
    int *ptr;
    const char *text;
    bool flag;
    double real;
    union {
        int as_int;
        unsigned as_unsigned;
    };
};

#define EXPECT_INT(expression, native) \
    std::printf("EXPECT\t%s\tint\t%lld\n", expression, static_cast<long long>(native))
#define EXPECT_BOOL(expression, native) \
    std::printf("EXPECT\t%s\tbool\t%s\n", expression, (native) ? "true" : "false")
#define EXPECT_F64(expression, native)                                               \
    do {                                                                             \
        double value_ = (native);                                                    \
        std::uint64_t bits_;                                                         \
        std::memcpy(&bits_, &value_, sizeof bits_);                                  \
        std::printf("EXPECT\t%s\tf64\t%#llx\n", expression,                         \
                    static_cast<unsigned long long>(bits_));                         \
    } while (0)

#define EXPECT_TEXT(expression, native) \
    std::printf("EXPECT\t%s\ttext\t%s\n", expression, std::string(native).c_str())
#define EXPECT_ERROR(expression, kind) std::printf("EXPECT\t%s\terror\t%s\n", expression, kind)



struct Library {
    std::map<int, int> squares;
    std::map<std::string, int> ages;
    std::vector<int> room;
    std::string name;
};

int main() {
    Fixture f{{-5}, -70000, 250, {-12, 123456789012LL}, {10, 20, 30, 40}, nullptr, "hello", true, 2.75, {-9}};
    f.ptr = &f.arr[2];
    Diamond diamond;
    diamond.top_value = 1;
    diamond.left_value = 2;
    diamond.right_value = 3;
    diamond.own_value = 4;

    __asm__ volatile("" : : "r"(&diamond) : "memory");
    int &ref = f.arr[1];
    Library library{{{1, 1}, {2, 4}, {3, 9}}, {{"ann", 30}, {"bob", 41}}, {}, "hello"};
    library.room.reserve(10);
    library.room.push_back(1);

    EXPECT_INT("f.count", f.count);
    EXPECT_INT("f.byte + 10", f.byte + 10);
    EXPECT_INT("f.inner.ll", f.inner.ll);
    EXPECT_INT("f.arr[3]", f.arr[3]);
    EXPECT_INT("*f.ptr", *f.ptr);
    EXPECT_INT("f.ptr - f.arr", f.ptr - f.arr);
    EXPECT_INT("ref", ref);
    EXPECT_INT("ref * 2", ref * 2);
    EXPECT_BOOL("&ref == &f.arr[1]", &ref == &f.arr[1]);
    EXPECT_BOOL("f.text == \"hello\"", std::strcmp(f.text, "hello") == 0);
    EXPECT_BOOL("f.flag", f.flag);
    EXPECT_F64("f.real * 2", f.real * 2);
    EXPECT_INT("sizeof(f.inner)", sizeof f.inner);
    EXPECT_INT("(short)f.count", static_cast<short>(f.count));
    EXPECT_INT("f.base_value", f.base_value);
    EXPECT_INT("f.base_value + f.count", f.base_value + f.count);
    EXPECT_INT("f.as_int", f.as_int);
    EXPECT_INT("f.as_unsigned", f.as_unsigned);
    EXPECT_INT("diamond.top_value", diamond.top_value);
    EXPECT_INT("diamond.left_value + diamond.right_value", diamond.left_value + diamond.right_value);
    EXPECT_INT("diamond.own_value", diamond.own_value);
    EXPECT_BOOL("f.ptr != nil", f.ptr != nullptr);
    std::printf("EXPECT\tf.arr[1:3]\trange\t%d,%d\n", f.arr[1], f.arr[2]);
    EXPECT_INT("cap(f.arr)", sizeof f.arr / sizeof f.arr[0]);
    EXPECT_TEXT("f.text[1:3]", std::string(f.text).substr(1, 2));
    EXPECT_INT("len(f.text[2:])", std::strlen(f.text) - 2);
    EXPECT_INT("library.squares[3]", library.squares.at(3));
    EXPECT_INT("library.ages[\"bob\"]", library.ages.at("bob"));
    if (library.ages.count("zed") == 0) {
        EXPECT_ERROR("library.ages[\"zed\"]", "missing-key");
    }
    EXPECT_INT("cap(library.room)", library.room.capacity());
    EXPECT_INT("len(library.room)", library.room.size());
    EXPECT_TEXT("library.name[1:4]", library.name.substr(1, 3));
    std::fflush(stdout);
    __asm__ volatile("" : : "r"(&library) : "memory");
    barrier(&f);
    return ref == 0;
}
