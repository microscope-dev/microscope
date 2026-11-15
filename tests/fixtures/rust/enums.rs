#![no_main]
#![no_std]

#[derive(Clone, Copy)]
enum Payload {
    Unit,
    Integer(u32),
}

#[derive(Clone, Copy)]
enum Fieldless {
    Negative = -3,
    Zero = 0,
    Positive = 7,
}



impl Fieldless {
    #[inline(never)]
    fn sign(self) -> i32 {
        (self as i32).signum()
    }
}

#[repr(u128)]
#[derive(Clone, Copy)]
enum Wide {
    Huge = (1_u128 << 100) + 9,
}

#[inline(never)]
fn inspect_enum(
    value: &Payload,
    fieldless: &Fieldless,
    wide: &Wide,
    optional: &Option<&u32>,
    empty: &Option<&u32>,
    done: &Result<(), u32>,
    failed: &Result<(), u32>,
) -> bool {
    core::hint::black_box((value, fieldless, wide, optional, empty, done, failed));
    matches!(value, Payload::Integer(42))
        && matches!(fieldless, Fieldless::Negative)
        && matches!(wide, Wide::Huge)
        && matches!(optional, Some(43))
        && empty.is_none()
        && done.is_ok()
        && matches!(failed, Err(5))
}

#[unsafe(no_mangle)]
pub extern "C" fn main() -> i32 {
    let value = Payload::Integer(42);
    let fieldless = Fieldless::Negative;
    let wide = Wide::Huge;
    let optional_value = 43;
    let optional = Some(&optional_value);
    let empty = None;
    let done: Result<(), u32> = Ok(());
    let failed: Result<(), u32> = Err(5);
    core::hint::black_box(Payload::Unit);
    core::hint::black_box((Fieldless::Zero, Fieldless::Positive));
    i32::from(
        !inspect_enum(
            &value, &fieldless, &wide, &optional, &empty, &done, &failed,
        ) || core::hint::black_box(fieldless).sign() != -1,
    )
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo<'_>) -> ! {
    loop {}
}

#[unsafe(no_mangle)]
pub extern "C" fn rust_eh_personality() {}
