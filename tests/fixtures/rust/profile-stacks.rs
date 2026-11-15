


use std::hint::black_box;
use std::sync::atomic::{AtomicU64, Ordering};

static SINK: AtomicU64 = AtomicU64::new(0);

#[unsafe(no_mangle)]
#[inline(never)]
pub extern "C" fn leaf(iterations: u64) {
    for index in 0..iterations {
        SINK.fetch_add(black_box(index), Ordering::Relaxed);
    }
}

#[inline(never)]
fn middle(iterations: u64) {
    leaf(black_box(iterations));
    SINK.fetch_add(1, Ordering::Relaxed);
}

#[inline(never)]
fn outer(iterations: u64) {
    for _ in 0..3 {
        middle(black_box(iterations));
    }
    SINK.fetch_add(1, Ordering::Relaxed);
}

fn main() {
    let iterations = std::env::args()
        .nth(1)
        .and_then(|text| text.parse().ok())
        .unwrap_or(1);
    let worker = std::thread::spawn(move || outer(iterations));
    outer(iterations);
    worker.join().expect("the worker");
}
