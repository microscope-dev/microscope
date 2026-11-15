using base_alias = int;
typedef base_alias alias_chain;
using const_alias = const alias_chain;
using pointer_alias = int*;
using const_pointer_alias = pointer_alias const;

template <typename T>
using pointer_template_alias = T*;

struct node {
    node const* next;
    volatile alias_chain value;
};

struct left;
struct right {
    left* peer;
};
struct left {
    right* peer;
};

__attribute__((noinline)) int inspect_types(
    const_alias value,
    pointer_alias pointer,
    const_pointer_alias const_pointer,
    pointer_template_alias<const int> templated,
    node* recursive,
    left* mutual
) {
    asm volatile("" : : "g"(&value), "g"(pointer), "g"(const_pointer),
                 "g"(templated), "g"(recursive), "g"(mutual) : "memory");
    return value + *pointer + *const_pointer + *templated + recursive->value +
           (mutual->peer != nullptr);
}

int main() {
    int value = 8;
    node recursive{nullptr, 9};
    right peer{nullptr};
    left mutual{&peer};
    peer.peer = &mutual;
    return inspect_types(value, &value, &value, &value, &recursive, &mutual) != 42;
}



struct counted {
    static int instances;
    int value;
};
int counted::instances = 1;
counted counted_global{2};


template <typename... Types>
struct packed_types {
    int count;
};
packed_types<int, char> packed_global{2};
