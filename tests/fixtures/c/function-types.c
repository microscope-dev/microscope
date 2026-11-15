


#include <stddef.h>

typedef int (*binary_op)(int, int);

struct callbacks {
  int (*on_event)(int, void *);
  void (*on_done)(void);
};

__attribute__((noinline)) int add(int left, int right) { return left + right; }

__attribute__((noinline)) static int negate(int value) { return -value; }

__attribute__((noinline)) static void nothing(void) {}

__attribute__((noinline)) static int variadic(const char *format, ...) {
  return format != NULL;
}

__attribute__((noinline)) static int handle(int event, void *data) {
  return event + (data != NULL);
}

__attribute__((noinline)) static const char *name_of(unsigned index,
                                                     double *weight) {
  return weight != NULL && index > 0 ? "many" : "one";
}

__attribute__((noinline)) static int (*choose(int which))(int) {
  return which ? negate : NULL;
}

int (*unary)(int) = negate;
int (**unary_pointer)(int) = &unary;
binary_op operation = add;
binary_op operations[2] = {add, NULL};
void (*no_arguments)(void) = nothing;
int (*with_variadic)(const char *, ...) = variadic;
int (*(*chooser)(int))(int) = choose;
int (*unprototyped)() = 0;
struct callbacks handlers = {handle, nothing};
const char *(*namer)(unsigned, double *) = name_of;
int (*const constant_function)(int) = negate;
int (*null_function)(int) = NULL;

int main(void) {
  int (*chosen)(int) = chooser(1);
  return chosen(operation(1, 2)) + 3 + handlers.on_event(0, NULL) +
         (namer(0, NULL)[0] != 'o') + (with_variadic("x") - 1) +
         ((*unary_pointer)(0));
}
