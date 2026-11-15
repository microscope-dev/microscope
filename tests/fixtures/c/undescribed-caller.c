



__attribute__((noinline)) int callback(int value) {
    return value + 1; // CALLBACK
}

__asm__(".text\n"
        ".global trampoline\n"
        ".type trampoline, @function\n"
        "trampoline:\n"
        "    push %rbx\n"
        "    call callback\n"
        "    pop %rbx\n"
        "    ret\n"
        ".size trampoline, . - trampoline\n");

int trampoline(int value);

int main(void) {
    int total = trampoline(1);
    total += callback(2);
    return total == 5 ? 0 : 1;
}
