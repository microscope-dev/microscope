//! The system call engines: each turns what it
//! sees into the neutral events of `crate::syscalls`.

pub mod bpf;
pub mod budget;
pub mod capture;
pub mod cgroup;
pub mod continuous;
pub mod info;
pub mod launch;
pub mod ptrace;
pub mod seccomp;
pub mod stacks;
pub mod unwinder;

use crate::syscalls::stats::Folded;
use crate::syscalls::{InFlight, TraceEvent};

/// Where an engine sends what it sees, on the engine's own thread.
pub trait Sink {
    /// One event, in the order the engine saw it.
    fn event(&mut self, event: TraceEvent);

    /// A running process is now traced, every thread of it.
    fn attached(&mut self, _process: u32) {}

    /// Where the BPF engine's programs attached, and why.
    fn attached_by(&mut self, _attachment: &bpf::Attachment) {}

    /// The calls threads have been blocked in since before `since`, read
    /// every so often while the trace runs.
    fn in_flight(&mut self, _calls: &[InFlight]) {}

    /// Called every tick of the engine's reader, at `time`.
    fn tick(&mut self, _time: u64) {}

    /// Whether a segment of a continuous trace ends at `time`.
    fn segment_due(&mut self, _time: u64) -> bool {
        false
    }

    /// Ends the segment at `time`; what the kernel counted over it has
    /// just been given to [`Sink::counted`].
    fn end_segment(&mut self, _time: u64) {}

    /// What the budget says, at `time`: how it is measured, and each step
    /// that gave up detail to stay inside it.
    fn budget(&mut self, _time: u64, _note: &str) {}

    /// Nanoseconds a call must have been blocked to be reported in flight;
    /// `None` asks for no reports.
    fn in_flight_after(&self) -> Option<u64> {
        None
    }

    /// Statistics an engine counted itself, without reporting the calls,
    /// once the trace ends.
    fn counted(&mut self, _stats: Folded) {}
}

pub use ptrace::{Finished, Target, TraceOptions, monotonic};
