#![no_main]
#![no_std]

#[unsafe(no_mangle)]
pub static mut RUST_BOUNDARY_INPUT: i32 = -1;

#[unsafe(no_mangle)]
pub static mut RUST_BOUNDARY_SINK: i32 = 0;

#[inline(never)]
fn marked_returns(value: i32) -> i32 {
    let mut frame = [0_i32; 40];
    frame[0] = value;
    if frame[0] < 0 {
        unsafe { RUST_BOUNDARY_SINK = 11 };
        return -frame[0];
    }

    unsafe { RUST_BOUNDARY_SINK = 22 };
    frame[0] + 1
}

#[inline(never)]
fn no_prologue(value: i32) -> i32 {
    unsafe { RUST_BOUNDARY_SINK = value };
    value + 1
}

#[inline(always)]
fn inline_adjust(value: i32) -> i32 {
    let adjusted = value + 7;
    unsafe { RUST_BOUNDARY_SINK = adjusted };
    adjusted * 2
}

#[unsafe(no_mangle)]
pub extern "C" fn main() -> i32 {
    let input = unsafe { RUST_BOUNDARY_INPUT };
    let inlined = inline_adjust(input);
    let first = marked_returns(input);
    unsafe { RUST_BOUNDARY_INPUT = 4 };
    let input = unsafe { RUST_BOUNDARY_INPUT };
    let second = marked_returns(input);
    let third = no_prologue(input);
    i32::from(!(inlined == 12 && first == 1 && second == 5 && third == 5))
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo<'_>) -> ! {
    loop {}
}


#[unsafe(no_mangle)]
pub extern "C" fn rust_eh_personality() {}
