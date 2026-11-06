// Frames an unwinder must walk: recursion, a call the optimizer turns into
// a jump, calls through a table of function pointers, and frames written
// by hand: one without call-frame information, one whose return address is
// overwritten while it calls back into C, and a thread whose first frame
// begins in the middle of a function, as after a raw clone. Runs as many
// rounds as its argument says, or two.

#include "../rt/thread.h"

enum {
    STACK_SIZE = 16384,
};

typedef u64 (*step_fn)(u64);

static char orphan_stack[STACK_SIZE] __attribute__((aligned(16)));
static u64 orphan_result;
static u64 orphan_done;

__attribute__((noinline)) u64 leaf(u64 value) {
    return value * 3 + 1; // MARK: value < 64
}

__attribute__((noinline)) u64 relay(u64 value) {
    return leaf(value + 1);
}

__attribute__((noinline)) u64 descend(u64 depth) {
    if (depth == 0) {
        return relay(depth);
    }
    return descend(depth - 1) + 1; // MARK: depth > 0 && depth < 16
}

// Calls through the table keep the steps out of line, and give a
// position-independent build pointers to relocate.
static step_fn const steps[] = {leaf, relay, descend};

// bare_call(fn, value) returns fn(value) from a frame without call-frame
// information, where unwinding must stop: it realigns the stack, so its
// instructions prove no frame either.
//
// scribbled_call(fn, value) returns fn(value), overwriting its own return
// address while fn runs: unwinding past it reads a caller that is not one.
//
// orphan_spawn(fn, arg, stack, size) starts a thread that calls fn(arg)
// from the middle of orphan_spawn, on a stack whose top holds a zero return
// address, and exits when fn returns.
__asm__(".text\n"
        ".global bare_call\n"
        ".type bare_call, @function\n"
        "bare_call:\n"
        "    push %rbx\n"
        "    mov %rsp, %rbx\n"
        "    and $-16, %rsp\n"
        "    mov %rdi, %rax\n"
        "    mov %rsi, %rdi\n"
        "    call *%rax\n"
        "    mov %rbx, %rsp\n"
        "    pop %rbx\n"
        "    ret\n"
        ".size bare_call, . - bare_call\n"
        "\n"
        ".global scribbled_call\n"
        ".type scribbled_call, @function\n"
        "scribbled_call:\n"
        "    .cfi_startproc\n"
        "    push %rbx\n"
        "    .cfi_adjust_cfa_offset 8\n"
        "    .cfi_offset rbx, -16\n"
        "    mov 8(%rsp), %rbx\n"
        "    movq $1, 8(%rsp)\n"
        "    mov %rdi, %rax\n"
        "    mov %rsi, %rdi\n"
        "    call *%rax\n"
        "    mov %rbx, 8(%rsp)\n"
        "    pop %rbx\n"
        "    .cfi_adjust_cfa_offset -8\n"
        "    .cfi_restore rbx\n"
        "    ret\n"
        "    .cfi_endproc\n"
        ".size scribbled_call, . - scribbled_call\n"
        "\n"
        ".global orphan_spawn\n"
        ".type orphan_spawn, @function\n"
        "orphan_spawn:\n"
        "    .cfi_startproc\n"
        "    mov %rdi, %r8\n"
        "    mov %rsi, %r9\n"
        "    lea (%rdx,%rcx), %rsi\n"
        "    and $-16, %rsi\n"
        "    sub $16, %rsi\n"
        "    movq $0, (%rsi)\n"
        "    movq $0, 8(%rsi)\n"
        "    mov $0x50f00, %edi\n"
        "    xor %edx, %edx\n"
        "    xor %r10d, %r10d\n"
        "    mov $56, %eax\n"
        "    syscall\n"
        "    test %rax, %rax\n"
        "    jnz 1f\n"
        "    mov %r9, %rdi\n"
        "    call *%r8\n"
        "    xor %edi, %edi\n"
        "    mov $60, %eax\n"
        "    syscall\n"
        "    hlt\n"
        "1:\n"
        "    ret\n"
        "    .cfi_endproc\n"
        ".size orphan_spawn, . - orphan_spawn\n");

u64 bare_call(step_fn fn, u64 value);
u64 scribbled_call(step_fn fn, u64 value);
long orphan_spawn(void (*fn)(void *), void *arg, char *stack, u64 size);

static void orphan(void *argument) {
    u64 depth = (u64)argument;
    u64 result = scribbled_call(descend, depth) + bare_call(descend, depth);
    __atomic_store_n(&orphan_result, result, __ATOMIC_SEQ_CST);
    __atomic_store_n(&orphan_done, 1, __ATOMIC_SEQ_CST);
}

int main(int argc, char **argv) {
    u64 rounds = argc > 1 ? rt_parse_u64(argv[1]) : 2;
    orphan_spawn(orphan, (void *)rounds, orphan_stack, STACK_SIZE);
    u64 total = 0;
    for (u64 round = 0; round < rounds; round++) {
        step_fn step = steps[round % 3];
        total += step(round);
        total += bare_call(step, round);
        total += scribbled_call(step, round);
    }
    while (rt_load(&orphan_done) == 0) {
        rt_yield();
    }
    total += rt_load(&orphan_result);
    rt_print("total ");
    rt_print_u64(total);
    rt_print("\n");
    return (int)(total % 100);
}
