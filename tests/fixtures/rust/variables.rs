#![no_main]
#![no_std]

use core::sync::atomic::{AtomicI32, Ordering};

static RUST_SINK: AtomicI32 = AtomicI32::new(0);

struct PointerPair {
    first: i32,
    second: i32,
}

struct PointerNode {
    next: *const PointerNode,
    value: i32,
}

type AliasedInt = i32;

#[inline(never)]
fn inspect_scalars(
    flag: bool,
    signed_value: i32,
    unsigned_value: u64,
    single: f32,
    double_precision: f64,
) -> bool {
    let local_flag = !flag;
    let local_signed = signed_value + 1;
    let local_unsigned = unsigned_value + 2;
    let local_single = single + 0.5;
    let local_double = double_precision - 0.25;
    RUST_SINK.store(local_signed, Ordering::Relaxed);
    !local_flag
        && local_signed == -41
        && local_unsigned == 44
        && local_single == 1.75
        && local_double == -2.75
}

#[inline(never)]
fn inspect_pointers(
    parameter: i32,
    shared_parameter: &i32,
    raw_parameter: *const i32,
) -> bool {
    let mut pointee = parameter + 2;
    let shared = &pointee;
    let raw_const = shared as *const i32;
    let raw_pointer = &raw_const as *const *const i32;
    let null_pointer = core::ptr::null::<i32>();
    let alias_pointee: AliasedInt = 42;
    let alias_pointer = &alias_pointee;
    let pair = PointerPair {
        first: 20,
        second: 22,
    };
    let structure_pointer = &pair;
    let node = PointerNode {
        next: core::ptr::null(),
        value: 42,
    };
    let recursive_pointer = &node;
    let array = [20_i32, 22_i32];
    let array_pointer = &array;
    let slice: &[i32] = &array;
    core::hint::black_box((
        shared,
        raw_const,
        raw_pointer,
        null_pointer,
        shared_parameter,
        raw_parameter,
        alias_pointer,
        structure_pointer,
        recursive_pointer,
        array_pointer,
        slice,
    ));
    let result = *shared == 42;
    pointee += 1;
    RUST_SINK.store(pointee, Ordering::Relaxed);
    result
        && *shared_parameter == 42
        && pair.first + pair.second == 42
        && node.next.is_null()
        && node.value == 42
        && array[0] + array[1] == 42
}

#[unsafe(no_mangle)]
pub extern "C" fn main() -> i32 {
    let succeeded = inspect_scalars(
        core::hint::black_box(true),
        core::hint::black_box(-42),
        core::hint::black_box(42),
        core::hint::black_box(1.25),
        core::hint::black_box(-2.5),
    );
    let pointer_parameter = core::hint::black_box(42);
    i32::from(
        !succeeded
            || !inspect_pointers(
                core::hint::black_box(40),
                &pointer_parameter,
                core::ptr::from_ref(&pointer_parameter),
            ),
    )
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo<'_>) -> ! {
    loop {}
}


#[unsafe(no_mangle)]
pub extern "C" fn rust_eh_personality() {}
