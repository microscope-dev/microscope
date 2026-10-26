//! System call tracing's neutral model; see `dev/syscalls.md`.
//!
//! It holds the events both engines produce, the call tables and argument specs that drive
//! decoding, strace's selection language, and what follows from a stream
//! of events: descriptor tables, the process tree, and statistics.
//!
//! Everything here is pure. Engines live at the edge, in
//! `backend::linux::syscalls`, and formatting in the binary's
//! `present::syscalls`.

mod argnames;
pub mod blobs;
pub mod captures;
pub mod fds;
pub mod filter;
pub mod model;
pub mod procs;
pub mod sockaddr;
pub mod specs;
pub mod stats;
pub mod table;

pub use filter::{CallSet, Filter, FilterError, StatusSet};
pub use model::{
    Abi, CallStack, Capture, CaptureState, Device, Errno, EventFlags, InFlight, InterpretedCall,
    Outcome, Pid, ProcessEnd, ReadAt, SyscallEvent, SyscallId, Tid, TraceEvent,
};

pub use stats::SyscallStats;

#[cfg(test)]
mod tests;
