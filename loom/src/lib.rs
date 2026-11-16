#![cfg_attr(not(test), allow(dead_code))]

#[path = "../../src/debug_info/dwarf/variables/in_order.rs"]
mod in_order;

#[path = "../../src/backend/linux/alarm.rs"]
mod alarm;

#[path = "../../src/cache/one_at_a_time.rs"]
mod one_at_a_time;

mod mutex;

mod sync {
    pub use crate::mutex::{Condvar, Mutex};
    pub use loom::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod alarm_tests;

#[cfg(test)]
mod one_at_a_time_tests;
