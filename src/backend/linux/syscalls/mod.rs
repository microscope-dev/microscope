pub mod bpf;
pub mod capture;
pub mod info;
pub mod launch;
pub mod pairs;
pub mod ptrace;
pub mod rings;
pub mod seccomp;
pub mod stacks;
pub mod unwinder;

use crate::syscalls::stats::Folded;
use crate::syscalls::{InFlight, TraceEvent};

pub trait Sink {
    fn event(&mut self, event: TraceEvent);

    fn attached(&mut self, _process: u32) {}

    fn attached_by(&mut self, _attachment: &bpf::Attachment) {}

    fn in_flight(&mut self, _calls: &[InFlight]) {}

    fn tick(&mut self, _time: u64) {}

    fn segment_due(&mut self, _time: u64) -> bool {
        false
    }

    fn end_segment(&mut self, _time: u64) {}

    fn shed(&self) -> bpf::Shed {
        bpf::Shed::default()
    }

    fn cost(&mut self, _calls: u64, _nanoseconds: u64) {}

    fn in_flight_after(&self) -> Option<u64> {
        None
    }

    fn counted(&mut self, _stats: Folded) {}
}

pub use ptrace::{Finished, Target, TraceOptions, monotonic};
