#![no_main]
#![no_std]

extern crate alloc;

use alloc::string::String;


struct Malloc;

unsafe extern "C" {
    fn malloc(size: usize) -> *mut u8;
    fn free(pointer: *mut u8);
}

unsafe impl core::alloc::GlobalAlloc for Malloc {
    unsafe fn alloc(&self, layout: core::alloc::Layout) -> *mut u8 {
        // SAFETY: malloc's alignment suffices for these fixtures' strings.
        unsafe { malloc(layout.size()) }
    }

    unsafe fn dealloc(&self, pointer: *mut u8, _layout: core::alloc::Layout) {
        // SAFETY: the pointer came from malloc.
        unsafe { free(pointer) }
    }
}

#[global_allocator]
static ALLOCATOR: Malloc = Malloc;

#[inline(never)]
fn strings_target(
    borrowed: &str,
    owned: &String,
    empty: &String,
    long: &String,
    letter: char,
) -> usize {
    core::hint::black_box((borrowed, owned, empty, long, letter));
    borrowed.len() + owned.len() // strings stop here
}

#[unsafe(no_mangle)]
pub extern "C" fn main() -> i32 {
    let owned = String::from("owned text");
    let empty = String::new();
    let mut long = String::new();
    for _ in 0..300 {
        long.push('z');
    }
    i32::from(strings_target(
        core::hint::black_box("héllo"),
        &owned,
        &empty,
        &long,
        'λ',
    ) != 16)
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo<'_>) -> ! {
    loop {}
}


#[unsafe(no_mangle)]
pub extern "C" fn rust_eh_personality() {}
