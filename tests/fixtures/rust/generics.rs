#![no_main]
#![no_std]





extern crate alloc;

use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec::Vec;


struct Malloc;

unsafe extern "C" {
    fn malloc(size: usize) -> *mut u8;
    fn free(pointer: *mut u8);
}

unsafe impl core::alloc::GlobalAlloc for Malloc {
    unsafe fn alloc(&self, layout: core::alloc::Layout) -> *mut u8 {
        // SAFETY: malloc's alignment suffices for these fixtures' values.
        unsafe { malloc(layout.size()) }
    }

    unsafe fn dealloc(&self, pointer: *mut u8, _layout: core::alloc::Layout) {
        // SAFETY: the pointer came from malloc.
        unsafe { free(pointer) }
    }
}

#[global_allocator]
static ALLOCATOR: Malloc = Malloc;

pub struct Wrapper<T, const N: usize> {
    pub items: [T; N],
}


pub struct Tail {
    pub head: u32,
    pub tail: [u16],
}

#[inline(never)]
#[expect(clippy::too_many_arguments, reason = "one stop shows every shape")]
fn generics_target(
    numbers: &Vec<i32>,
    slice: &[i32],
    bytes: &mut [u8],
    text: &str,
    boxed: &Box<[u16]>,
    boxed_text: &Box<str>,
    wrapper: &Wrapper<u8, 3>,
    raw: *const [i32],
    tail: &Tail,
    owned: &String,
) -> usize {
    core::hint::black_box((
        numbers, &slice, &bytes, &text, boxed, boxed_text, wrapper, &raw, &tail, owned,
    ));
    numbers.len() + slice.len() + bytes.len() + text.len() + boxed.len() + tail.tail.len() // generics stop here
}

#[unsafe(no_mangle)]
pub extern "C" fn main() -> i32 {
    let numbers: Vec<i32> = alloc::vec![1, 2, 3];
    let mut bytes = [7_u8, 8];
    let boxed: Box<[u16]> = alloc::vec![4_u16, 5, 6, 7].into_boxed_slice();
    let boxed_text: Box<str> = Box::from("boxed");
    let wrapper = Wrapper::<u8, 3> { items: [1, 2, 3] };
    let halves = [10_u32, 11, 12];
    // SAFETY: the tail's two u16s lie within `halves`, whose u32 alignment
    // suffices for `Tail`.
    let tail = unsafe {
        &*(core::ptr::slice_from_raw_parts(halves.as_ptr().cast::<u16>(), 2) as *const Tail)
    };
    let owned = String::from("owned");
    let total = generics_target(
        core::hint::black_box(&numbers),
        core::hint::black_box(&numbers[1..]),
        core::hint::black_box(&mut bytes),
        core::hint::black_box("héllo"),
        &boxed,
        &boxed_text,
        &wrapper,
        core::hint::black_box(&numbers[..2]) as *const [i32],
        tail,
        &owned,
    );
    i32::from(total != 19)
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo<'_>) -> ! {
    loop {}
}


#[unsafe(no_mangle)]
pub extern "C" fn rust_eh_personality() {}
