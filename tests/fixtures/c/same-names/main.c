


int shared_entry(int value);

__attribute__((noinline)) static int helper(int value) {
    return value * 2;
}

int main(void) {
    return helper(3) + shared_entry(4) == 13 ? 0 : 1;
}
