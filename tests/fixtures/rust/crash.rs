#![no_main]
#![no_std]

use core::sync::atomic::{AtomicI32, Ordering};

static CRASH_SINK: AtomicI32 = AtomicI32::new(0);

struct Record {
    id: i32,
    scale: f64,
}

#[inline(never)]
fn crash_now(record: &Record, depth: i32) -> f64 {
    let doubled = depth * 2;
    CRASH_SINK.store(record.id + doubled, Ordering::Relaxed);

    let target = core::ptr::without_provenance_mut::<i32>(8);
    unsafe { core::ptr::write_volatile(target, doubled) };
    record.scale * f64::from(doubled)
}

#[unsafe(no_mangle)]
pub extern "C" fn main() -> i32 {
    let record = Record { id: 42, scale: 2.5 };
    crash_now(&record, 3) as i32
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo<'_>) -> ! {
    loop {}
}

#[unsafe(no_mangle)]
pub extern "C" fn rust_eh_personality() {}
