volatile int cpp_sink;
int pointer_parameter_value = 42;

namespace fixture {

using aliased_int = int;

struct PointerPair {
    int first;
    int second;
};

struct PointerNode {
    PointerNode* next;
    int value;
};

__attribute__((noinline)) bool inspect_scalars(bool flag, int signed_value,
                                               unsigned long unsigned_value,
                                               float single,
                                               double double_precision) {
    bool local_flag = !flag;
    int local_signed = signed_value + 1;
    unsigned long local_unsigned = unsigned_value + 2;
    float local_single = single + 0.5F;
    double local_double = double_precision - 0.25;
    cpp_sink = local_signed;
    return !local_flag && local_signed == -41 && local_unsigned == 44UL &&
           local_single == 1.75F && local_double == -2.75;
}

__attribute__((noinline)) bool inspect_pointers(int parameter, int* pointer_parameter,
                                                int& reference_parameter) {
    int pointee = parameter + 2;
    int* pointer = &pointee;
    int** pointer_pointer = &pointer;
    int& lvalue_reference = pointee;
    const int& const_reference = pointee;
    int&& rvalue_reference = static_cast<int&&>(pointee);
    int*& reference_to_pointer = pointer;
    int* null_pointer = nullptr;
    aliased_int alias_pointee = 42;
    aliased_int* alias_pointer = &alias_pointee;
    PointerPair pair{20, 22};
    PointerPair* structure_pointer = &pair; PointerPair& structure_reference = pair;
    PointerNode node{nullptr, 42};
    PointerNode* recursive_pointer = &node;
    int array[2]{20, 22};
    int (*array_pointer)[2] = &array;
    asm volatile("" : : "g"(pointer), "g"(pointer_pointer), "g"(&lvalue_reference),
                 "g"(&const_reference), "g"(&rvalue_reference),
                 "g"(&reference_to_pointer), "g"(null_pointer), "g"(pointer_parameter),
                 "g"(&reference_parameter), "g"(alias_pointer), "g"(structure_pointer),
                 "g"(&structure_reference), "g"(recursive_pointer), "g"(array_pointer) : "memory");
    cpp_sink = **pointer_pointer;
    return lvalue_reference == 42 && const_reference == 42 &&
           rvalue_reference == 42 && *reference_to_pointer == 42;
}

}

volatile bool input_flag = true;
volatile int input_signed = -42;
volatile unsigned long input_unsigned = 42UL;
volatile float input_single = 1.25F;
volatile double input_double = -2.5;

int main() {
    return fixture::inspect_scalars(input_flag, input_signed, input_unsigned,
                                    input_single, input_double) &&
                   fixture::inspect_pointers(40, &pointer_parameter_value,
                                             pointer_parameter_value)
               ? 0
               : 1;
}
