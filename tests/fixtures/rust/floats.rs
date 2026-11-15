


#![no_main]
#![no_std]
#![feature(f16, f128)]

#[inline(never)]
fn floats_target(half: f16, quad: f128) -> bool {
    core::hint::black_box((half, quad));
    half == 1.5
}

#[unsafe(no_mangle)]
pub extern "C" fn main() -> i32 {
    let tenth = core::hint::black_box(1.0_f128) / 10.0;
    i32::from(!floats_target(core::hint::black_box(1.5), tenth))
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo<'_>) -> ! {
    loop {}
}

#[unsafe(no_mangle)]
pub extern "C" fn rust_eh_personality() {}
