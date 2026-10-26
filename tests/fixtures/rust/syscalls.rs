// Opens a file that is not there through the standard library, from a
// function of the program's own, so that a traced call's site is the
// program's function, past std's frames and the C library's.

use std::hint::black_box;

#[inline(never)]
fn open_settings() -> bool {
    std::fs::File::open(black_box("missing-settings")).is_ok()
}

fn main() {
    std::process::exit(i32::from(black_box(open_settings())) + 3);
}
