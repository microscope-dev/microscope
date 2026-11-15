



#include <stdint.h>

typedef void (*symbols_callback_fn)(void);

int32_t lib_exported_entry(symbols_callback_fn callback);
void lib_fault(void);
void asm_sized(symbols_callback_fn callback);
void asm_unsized(symbols_callback_fn callback);
void asm_nested_outer(symbols_callback_fn callback);
void asm_alias_a_weak(symbols_callback_fn callback);
void *asm_resolver_impl(symbols_callback_fn callback);
void asm_gap_entry(symbols_callback_fn callback);
void asm_noreturn_caller(symbols_callback_fn callback);

volatile int32_t symbols_sink;

__attribute__((noinline)) static void chain_fault(void) {
    lib_fault();
    symbols_sink += 1;
}

__attribute__((noinline)) static void chain_unsized(void) {
    asm_unsized(chain_fault);
    symbols_sink += 1;
}

__attribute__((noinline)) static void chain_sized(void) {
    asm_sized(chain_unsized);
    symbols_sink += 1;
}

__attribute__((noinline)) static void chain_resolver(void) {
    asm_resolver_impl(chain_sized);
    symbols_sink += 1;
}

__attribute__((noinline)) static void chain_alias(void) {
    asm_alias_a_weak(chain_resolver);
    symbols_sink += 1;
}



static int32_t nested_calls;

__attribute__((noinline)) static void chain_nested_site(void) {
    nested_calls += 1;
    if (nested_calls == 3) {
        chain_alias();
    }
    symbols_sink += 1;
}

__attribute__((noinline)) static void chain_nested(void) {
    asm_nested_outer(chain_nested_site);
    symbols_sink += 1;
}

__attribute__((noinline)) static void chain_noreturn(void) {
    asm_noreturn_caller(chain_nested);
    symbols_sink += 1;
}

__attribute__((noinline)) static void chain_gap(void) {
    asm_gap_entry(chain_noreturn);
    symbols_sink += 1;
}

int main(void) {
    lib_exported_entry(chain_gap);
    return 0;
}
