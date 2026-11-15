

volatile int sliding_sink;

__attribute__((noinline)) int first_function(int value) {
    int doubled = value * 2;
    // a comment inside the function

    sliding_sink = doubled; // the next code after the comment
    return doubled;
}
// a comment between the functions

__attribute__((noinline)) int second_function(int value) {
    return value + 1;
}

int main(void) {
    return first_function(2) + second_function(3) == 8 ? 0 : 1;
}
