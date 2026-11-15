


#![no_main]
#![no_std]

use core::ffi::{CStr, c_char, c_int};

unsafe extern "C" {
    fn printf(format: *const c_char, ...) -> c_int;
    fn fflush(stream: *mut u8) -> c_int;
}

struct Inner {
    s: i16,
    ll: i64,
}

struct Fixture {
    small: i8,
    byte: u8,
    wide: u64,
    inner: Inner,
    arr: [i32; 4],
    grid: [[i32; 3]; 2],
    slice: &'static [i32],
    text: &'static str,
    pair: (i32, u8),
    flag: bool,
    real: f64,
}

static SLICE: [i32; 3] = [7, 8, 9];

fn int(expression: &CStr, value: i64) {
    // SAFETY: the format takes a C string and a long long, as passed.
    unsafe { printf(c"EXPECT\t%s\tint\t%lld\n".as_ptr(), expression.as_ptr(), value) };
}

fn unsigned(expression: &CStr, value: u64) {
    // SAFETY: the format takes a C string and an unsigned long long.
    unsafe { printf(c"EXPECT\t%s\tint\t%llu\n".as_ptr(), expression.as_ptr(), value) };
}

fn boolean(expression: &CStr, value: bool) {
    let text = if value { c"true" } else { c"false" };
    // SAFETY: the format takes two C strings.
    unsafe { printf(c"EXPECT\t%s\tbool\t%s\n".as_ptr(), expression.as_ptr(), text.as_ptr()) };
}

fn float(expression: &CStr, value: f64) {
    // SAFETY: the format takes a C string and an unsigned long long.
    unsafe { printf(c"EXPECT\t%s\tf64\t%#llx\n".as_ptr(), expression.as_ptr(), value.to_bits()) };
}

#[unsafe(no_mangle)]
#[inline(never)]
pub extern "C" fn barrier(fixture: *const u8) {
    core::hint::black_box(fixture);
}

#[unsafe(no_mangle)]
pub extern "C" fn main() -> i32 {
    let f = Fixture {
        small: -100,
        byte: 250,
        wide: 18_000_000_000_000_000_000,
        inner: Inner { s: -12, ll: 123_456_789_012 },
        arr: [10, 20, 30, 40],
        grid: [[1, 2, 3], [4, 5, 6]],
        slice: &SLICE,
        text: "hello",
        pair: (-5, 6),
        flag: true,
        real: 2.75,
    };
    int(c"f.small", f.small.into());
    unsigned(c"f.byte", f.byte.into());
    unsigned(c"f.wide", f.wide);
    int(c"f.byte + 10", i64::from(f.byte) + 10);
    int(c"f.small * f.inner.s", i64::from(f.small) * i64::from(f.inner.s));
    int(c"f.inner.ll", f.inner.ll);
    int(c"f.arr[2]", f.arr[2].into());
    int(c"f.grid[1][2]", f.grid[1][2].into());
    int(c"f.slice[1]", f.slice[1].into());
    int(c"len(f.slice)", 3);
    boolean(c"f.text == \"hello\"", f.text == "hello");
    int(c"len(f.text)", 5);
    int(c"f.pair.0", f.pair.0.into());
    boolean(c"f.flag", f.flag);
    float(c"f.real * 2", f.real * 2.0);
    // SAFETY: a null stream flushes every open output stream.
    unsafe { fflush(core::ptr::null_mut()) };
    barrier((&raw const f).cast());
    core::hint::black_box(&f);
    0
}

#[panic_handler]
fn panic(_: &core::panic::PanicInfo<'_>) -> ! {
    loop {}
}

#[unsafe(no_mangle)]
pub extern "C" fn rust_eh_personality() {}
