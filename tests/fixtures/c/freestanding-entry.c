



#ifdef BARE_ENTRY
#define ENTRY_CFI(directive) ""
#else
#define ENTRY_CFI(directive) directive
#endif

__asm__(".globl _start\n"
        "_start:\n" ENTRY_CFI(".cfi_startproc\n") ENTRY_CFI(".cfi_undefined rip\n")
        "\txor %ebp, %ebp\n"
        "\tcall main\n"
        "\tmov %eax, %edi\n"
        "\tmov $231, %eax\n"
        "\tsyscall\n" ENTRY_CFI(".cfi_endproc\n"));

static volatile long total;

int main(void) {
    total = 1; // FIRST_STATEMENT
    total += 2;
    return (int)total;
}
