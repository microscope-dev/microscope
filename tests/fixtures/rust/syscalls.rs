// Opens a file that is not there through the standard library, from a
// function of the program's own, so that a traced call's site is the
// program's function, past std's frames and the C library's.

use std::hint::black_box;

#[inline(never)]
fn open_settings() -> bool {
    std::fs::File::open(black_box("missing-settings")).is_ok()
}

#[inline(never)]
fn parent() -> u64 {
    let result;
    // SAFETY: getppid has no pointer arguments and leaves user memory alone.
    unsafe {
        std::arch::asm!(
            "syscall",
            inlateout("rax") 110_u64 => result,
            lateout("rcx") _,
            lateout("r11") _,
            options(nostack),
        );
    }
    result
}

#[inline(never)]
fn deep_parent() {
    let mut bytes = [0_u8; 16384];
    black_box(&mut bytes);
    black_box(parent());
    black_box(&bytes);
}

fn main() {
    match std::env::args().nth(1).as_deref() {
        Some("stacks") => {
            for _ in 0..32 {
                black_box(parent());
            }
            return;
        }
        Some("deep-stack") => {
            deep_parent();
            return;
        }
        Some("thread-stacks") => {
            let threads: Vec<_> = (0..2)
                .map(|_| {
                    std::thread::spawn(|| {
                        for _ in 0..32 {
                            black_box(parent());
                        }
                    })
                })
                .collect();
            for thread in threads {
                thread.join().expect("worker");
            }
            return;
        }
        _ => {}
    }
    std::process::exit(i32::from(black_box(open_settings())) + 3);
}
