


#include <stdio.h>

int disasm_marked_data(void);
int disasm_hidden_data(void);
int disasm_indirect(int selector);

int disasm_counter = 40;

__attribute__((noinline)) static int disasm_helper(int value) {
    return value + disasm_counter;
}

int main(void) {
    disasm_counter += disasm_marked_data();
    disasm_counter += disasm_hidden_data();
    if (disasm_indirect(1) != 0x3f) {
        return 1;
    }
    printf("%d\n", disasm_helper(0));
    return 0;
}
