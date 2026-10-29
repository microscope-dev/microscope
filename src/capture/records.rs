//! The records of a capture's tables.
//!
//! Every record is little-endian with
//! alignment one, so the rows in memory are the rows a capture file holds,
//! and records refer to each other by index, with [`NONE`] for none.

use zerocopy::little_endian::{I64, U16, U32, U64};
use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout, Unaligned};

use crate::CodeRole;

pub use crate::image::NONE;

/// What a table holds. The numbers are part of the format: a reader that
/// meets one it does not know rejects the chunk.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u32)]
pub enum CaptureTable {
    /// NUL-terminated UTF-8 strings, named by offset.
    Strings = 1,

    /// What produced the capture, as key and value.
    Settings = 2,
    Modules = 3,
    Processes = 4,
    Threads = 5,
    Mappings = 6,
    Frames = 7,
    Stacks = 8,

    /// On-CPU samples, thread by thread, each thread's in time order.
    Samples = 9,

    /// Where each thread's on-CPU samples are.
    SampleRanges = 10,

    /// Each frame's source lines, innermost inline first: empty for a
    /// frame not symbolized.
    FrameSymbols = 11,
    Lines = 12,
    Functions = 13,

    /// Counts of what was lost, throttled, or truncated.
    Accounting = 14,

    /// Intervals a thread spent off its CPU, thread by thread, each
    /// thread's in time order.
    OffCpu = 15,

    /// Where each thread's off-CPU intervals are.
    OffCpuRanges = 16,

    /// Instrumentation sites.
    Sites = 17,

    /// Instrumented zones and labels, thread by thread, depth by depth, by
    /// start.
    Zones = 18,

    /// Where each thread's zones are at each depth: how many zones of the
    /// thread hold each.
    ZoneRanges = 19,

    /// Values of plotted series, by series and time.
    Plots = 20,

    /// Instants: frames, messages, triggers, stops.
    Markers = 21,

    /// Runtime tasks: goroutines and async tasks.
    Tasks = 22,

    /// Samples of where tasks waited, thread of none, by time.
    TaskWaits = 23,

    /// Spans the recording did not sample, in time order.
    Gaps = 24,

    /// System calls, in the order they were seen.
    Syscalls = 25,

    /// The bytes calls' and signals' engines copied, which rows name by
    /// offset.
    SyscallBlobs = 26,

    /// What a system call trace saw besides calls: signals, processes and
    /// threads made and ended, programs executed, descriptors, losses.
    SyscallNotes = 27,

    /// Each process's statistics of each call it made.
    SyscallStats = 28,

    /// The errnos each row of statistics counts, row by row.
    SyscallErrnos = 29,
}

impl CaptureTable {
    pub const ALL: [Self; 29] = [
        Self::Strings,
        Self::Settings,
        Self::Modules,
        Self::Processes,
        Self::Threads,
        Self::Mappings,
        Self::Frames,
        Self::Stacks,
        Self::Samples,
        Self::SampleRanges,
        Self::FrameSymbols,
        Self::Lines,
        Self::Functions,
        Self::Accounting,
        Self::OffCpu,
        Self::OffCpuRanges,
        Self::Sites,
        Self::Zones,
        Self::ZoneRanges,
        Self::Plots,
        Self::Markers,
        Self::Tasks,
        Self::TaskWaits,
        Self::Gaps,
        Self::Syscalls,
        Self::SyscallBlobs,
        Self::SyscallNotes,
        Self::SyscallStats,
        Self::SyscallErrnos,
    ];

    pub const COUNT: usize = Self::ALL.len();

    pub const fn index(self) -> usize {
        self as usize - 1
    }

    pub fn from_number(number: u32) -> Option<Self> {
        Self::ALL.into_iter().find(|kind| *kind as u32 == number)
    }
}

/// A key and its value, both strings.
#[repr(C)]
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, FromBytes, IntoBytes, KnownLayout, Immutable, Unaligned,
)]
pub struct Setting {
    pub key: U32,
    pub value: U32,
}

/// A file the capture's code came from.
#[repr(C)]
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, FromBytes, IntoBytes, KnownLayout, Immutable, Unaligned,
)]
pub struct Module {
    /// Its path when it was mapped.
    pub path: U32,

    /// Its GNU build id in hex, or [`NONE`] for a file without one.
    pub build_id: U32,

    /// XXH3-128 of its bytes, or zero when they could not be read.
    pub digest: [u8; 16],
    pub size: U64,
}

/// A process the capture saw.
#[repr(C)]
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, FromBytes, IntoBytes, KnownLayout, Immutable, Unaligned,
)]
pub struct Process {
    pub pid: U32,
    pub name: U32,
    pub start: I64,
    pub end: I64,
}

/// A thread the capture saw.
#[repr(C)]
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, FromBytes, IntoBytes, KnownLayout, Immutable, Unaligned,
)]
pub struct Thread {
    pub process: U32,
    pub tid: U32,
    pub name: U32,
    pub reserved: U32,
    pub start: I64,
    pub end: I64,
}

/// One mapping of a module into a process.
#[repr(C)]
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, FromBytes, IntoBytes, KnownLayout, Immutable, Unaligned,
)]
pub struct Mapping {
    pub process: U32,
    pub module: U32,
    pub start: U64,
    pub end: U64,
    pub file_offset: U64,

    /// What the process adds to the module's image addresses.
    pub bias: U64,
    pub time: I64,
}

/// What a frame is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u8)]
pub enum FrameKind {
    /// An instruction of a module, by image address; or, with no module, a
    /// virtual address no module covers.
    Native = 0,

    /// Where a stack's unwinding ended without reaching its outermost
    /// frame, and why: the address is a [`Truncation`]'s number, and the
    /// module, when there is one, the module at fault.
    Truncated = 1,

    /// The task a task segment of a stack belongs to, by its root frame.
    TaskRoot = 2,

    /// The runtime's scheduler, standing for every frame below a task.
    Runtime = 3,

    /// The future a suspended task awaits, which runs no code: the address
    /// is the string that names it, and its one line says what it waits
    /// for.
    Awaited = 4,

    /// A frame an interpreter runs, whose code is in no module: the
    /// address is the string that keys its function and line, and its one
    /// line says where it is, its function declared where its code begins.
    Interpreted = 5,

    /// Kernel time attributed to a syscall: the address keys its name string.
    Kernel = 6,
}

impl FrameKind {
    pub const fn from_number(number: u8) -> Option<Self> {
        match number {
            0 => Some(Self::Native),
            1 => Some(Self::Truncated),
            2 => Some(Self::TaskRoot),
            3 => Some(Self::Runtime),
            4 => Some(Self::Awaited),
            5 => Some(Self::Interpreted),
            6 => Some(Self::Kernel),
            _ => None,
        }
    }

    /// Whether a frame of this kind is keyed by a string, not an address,
    /// and so has no address to show.
    pub const fn named(self) -> bool {
        matches!(self, Self::Awaited | Self::Interpreted | Self::Kernel)
    }
}

/// Why a stack ends without reaching its outermost frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u8)]
pub enum Truncation {
    /// No module's call-frame information covers the code.
    NoUnwindInfo = 1,

    /// The kernel's frame-pointer chain passes through a function that
    /// does not keep its frame pointer, so its callers are unknown.
    FramePointerNotKept = 2,

    /// The copy of the stack ended before a frame the kernel's chain could
    /// continue from.
    StackCopyExhausted = 3,

    /// A return address no mapping covers.
    UnmappedAddress = 4,

    /// The deepest stack recorded.
    DepthLimit = 5,

    /// The kernel's chain ended at a frame that is not a stack's outermost.
    ChainEnded = 6,

    /// Unwinding found an impossible caller.
    InvalidCaller = 7,

    /// The sample carried no stack at all.
    NoStack = 8,

    /// A runtime switched stacks, and neither the kernel's chain nor the
    /// copy reaches the frames it switched from.
    StackSwitch = 9,

    /// A suspended task's chain of awaits could not be followed further.
    AwaitChainEnded = 10,

    /// Unwinding a task's stack could not read what it needed: a register,
    /// memory, or unwind information it could interpret.
    Unreadable = 11,

    /// An interpreter runs on the thread, and its frames at the sample are
    /// unknown: their record was lost, or came before the interpreter's
    /// layout or the thread's state was known.
    InterpretedUnknown = 12,

    /// The interpreter's frames end before the thread's outermost: one
    /// could not be read, or there were more than a record holds.
    InterpretedEnded = 13,

    /// The interpreter's frames, which follow the native frames, belong
    /// before activations of its loop that differ from them in number.
    InterpretedUnplaced = 14,
}

impl Truncation {
    pub const ALL: [Self; 14] = [
        Self::NoUnwindInfo,
        Self::FramePointerNotKept,
        Self::StackCopyExhausted,
        Self::UnmappedAddress,
        Self::DepthLimit,
        Self::ChainEnded,
        Self::InvalidCaller,
        Self::NoStack,
        Self::StackSwitch,
        Self::AwaitChainEnded,
        Self::Unreadable,
        Self::InterpretedUnknown,
        Self::InterpretedEnded,
        Self::InterpretedUnplaced,
    ];

    pub fn from_number(number: u64) -> Option<Self> {
        Self::ALL.into_iter().find(|kind| *kind as u64 == number)
    }

    /// How a view names the truncation.
    pub const fn describe(self) -> &'static str {
        match self {
            Self::NoUnwindInfo => "no unwind information",
            Self::FramePointerNotKept => "frame pointer not kept",
            Self::StackCopyExhausted => "stack copy exhausted",
            Self::UnmappedAddress => "unmapped return address",
            Self::DepthLimit => "depth limit",
            Self::ChainEnded => "frame chain ended",
            Self::InvalidCaller => "invalid caller",
            Self::NoStack => "no stack",
            Self::StackSwitch => "frames before a stack switch not captured",
            Self::AwaitChainEnded => "chain of awaits ended",
            Self::Unreadable => "unwinding could not read the stack",
            Self::InterpretedUnknown => "interpreted frames unknown",
            Self::InterpretedEnded => "interpreted frames ended early",
            Self::InterpretedUnplaced => "interpreted frames not placed among the native ones",
        }
    }
}

/// One distinct frame.
#[repr(C)]
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, FromBytes, IntoBytes, KnownLayout, Immutable, Unaligned,
)]
pub struct Frame {
    pub address: U64,
    pub module: U32,

    /// A [`FrameKind`].
    pub kind: u8,

    /// The function's `CodeRole`, by its number, when known.
    pub role: u8,
    pub reserved: U16,
}

/// A node of the stack tree: a frame called from its parent, or a root.
/// Parents come before their children.
#[repr(C)]
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, FromBytes, IntoBytes, KnownLayout, Immutable, Unaligned,
)]
pub struct Stack {
    pub parent: U32,
    pub frame: U32,
}

/// One sample: when, how much, and the stack it caught.
#[repr(C)]
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, FromBytes, IntoBytes, KnownLayout, Immutable, Unaligned,
)]
pub struct Sample {
    /// Nanoseconds on the capture's clock.
    pub time: I64,

    /// Nanoseconds of CPU time, or executions, the sample stands for.
    pub weight: U64,
    pub stack: U32,

    /// The task it ran, or [`NONE`].
    pub task: U32,
}

/// A span of time a thread spent off its CPU.
#[repr(C)]
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, FromBytes, IntoBytes, KnownLayout, Immutable, Unaligned,
)]
pub struct OffCpu {
    /// When it left its CPU, in nanoseconds on the capture's clock.
    pub time: I64,

    /// Nanoseconds until it ran again, or until the recording ended.
    pub duration: U64,

    /// One when it left while it could still run, zero when it blocked.
    pub preempted: u8,
}

/// Where one thread's rows are in a table grouped by thread.
#[repr(C)]
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, FromBytes, IntoBytes, KnownLayout, Immutable, Unaligned,
)]
pub struct ThreadRange {
    pub thread: U32,
    pub start: U32,
    pub count: U32,
}

/// Where a frame's lines are: `count` rows from `start`.
#[repr(C)]
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, FromBytes, IntoBytes, KnownLayout, Immutable, Unaligned,
)]
pub struct Range {
    pub start: U32,
    pub count: U32,
}

/// A source line, of a function and possibly inlined into the next row's.
#[repr(C)]
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, FromBytes, IntoBytes, KnownLayout, Immutable, Unaligned,
)]
pub struct Line {
    pub function: U32,

    /// The file's path, or [`NONE`].
    pub file: U32,
    pub number: U32,
    pub column: U32,
}

/// A function frames run.
#[repr(C)]
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, FromBytes, IntoBytes, KnownLayout, Immutable, Unaligned,
)]
pub struct Function {
    pub name: U32,
    pub module: U32,

    /// The file it is declared in, or [`NONE`].
    pub file: U32,
    pub line: U32,

    /// Its `CodeRole`, by [`role_number`].
    pub role: u8,
    pub reserved: [u8; 3],
}

/// A role's number in a capture.
pub const fn role_number(role: CodeRole) -> u8 {
    match role {
        CodeRole::Ordinary => 0,
        CodeRole::Wrapper => 1,
        CodeRole::RuntimeInternal => 2,
        CodeRole::Panic => 3,
        CodeRole::StackSwitch => 4,
        CodeRole::Outermost => 5,
        CodeRole::TrapEntry => 6,
        CodeRole::SignalTrampoline => 7,
        CodeRole::Dispatch => 8,
        CodeRole::Interpreter => 9,
    }
}

/// The role a capture's number stands for.
pub const fn role_of(number: u8) -> Option<CodeRole> {
    Some(match number {
        0 => CodeRole::Ordinary,
        1 => CodeRole::Wrapper,
        2 => CodeRole::RuntimeInternal,
        3 => CodeRole::Panic,
        4 => CodeRole::StackSwitch,
        5 => CodeRole::Outermost,
        6 => CodeRole::TrapEntry,
        7 => CodeRole::SignalTrampoline,
        8 => CodeRole::Dispatch,
        9 => CodeRole::Interpreter,
        _ => return None,
    })
}

/// What a count in [`Account`] counts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u32)]
pub enum AccountKind {
    /// Samples the kernel dropped because a ring was full.
    LostSamples = 1,

    /// Other records the kernel dropped because a ring was full: of
    /// mappings, names, threads, and context switches. A recording before
    /// these were counted apart counts them among the samples.
    LostRecords = 30,

    /// Times the kernel throttled a sampling event.
    Throttles = 2,

    /// Samples whose stack ended in a truncation, by its number above
    /// 1000: 1001 is [`Truncation::NoUnwindInfo`].
    Truncated = 1000,

    /// Records microscope could not decode, and spans of a ring it skipped
    /// because the ring's head could not be trusted.
    Malformed = 3,

    /// Bytes of stack the kernel copied.
    StackBytes = 7,

    /// Records the kernel wrote after later ones had been processed, which
    /// were applied late: a mapping's samples before it end unmapped.
    LateRecords = 11,

    /// Breakpoint samples of an instruction that ran again within one
    /// call, as a Go stack check does when it grows the stack.
    Repeats = 12,

    /// What the events counted, in the weight's unit: the CPU time the
    /// program's threads ran, sampled or not, or every execution of a
    /// breakpoint's instruction, while the events were on.
    Counted = 13,

    /// Nanoseconds of CPU time microscope spent recording.
    RecorderCpu = 14,

    /// The part of it spent reading programs' debug information.
    RecorderLoading = 15,

    /// Nanoseconds task snapshots held the program stopped.
    SnapshotPause = 16,

    /// Task snapshots taken.
    Snapshots = 17,

    /// Task snapshots that failed, and tasks a snapshot could not read.
    SnapshotFailures = 18,

    /// Samples task snapshots gave the task they ran.
    TaskSamples = 19,

    /// Instrumentation events read.
    ProbeEvents = 20,

    /// Instrumentation records a thread overwrote before microscope read them.
    ProbeOverwritten = 21,

    /// Instrumentation events no ring could take: the program had no ring
    /// left for a thread, or a thread recorded after it began exiting.
    ProbeNoRing = 22,

    /// Instrumentation events a signal handler raised while its thread was
    /// writing one, which it dropped.
    ProbeInterrupted = 23,

    /// Instrumentation records that broke the ABI, and rings that did.
    ProbeMalformed = 24,

    /// Waiting tasks live task snapshots left out because they changed
    /// while read.
    SnapshotChanged = 25,

    /// Task snapshots read while the program ran.
    LiveSnapshots = 26,

    /// Waiting tasks and threads live task snapshots read, kept or left
    /// out.
    LiveRead = 31,

    /// System call records the kernel's ring had no room for.
    SyscallsLost = 27,

    /// Records of interpreted frames the kernel's rings had no room for.
    InterpretedLost = 28,

    /// The part of [`AccountKind::RecorderCpu`] spent before the program
    /// was recorded: opening its events and loading BPF programs.
    RecorderSetup = 29,
}

/// A count of something the views must show.
#[repr(C)]
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, FromBytes, IntoBytes, KnownLayout, Immutable, Unaligned,
)]
pub struct Account {
    pub kind: U32,
    pub reserved: U32,
    pub value: U64,
}

/// One instrumentation site.
#[repr(C)]
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, FromBytes, IntoBytes, KnownLayout, Immutable, Unaligned,
)]
pub struct Site {
    pub name: U32,
    pub file: U32,
    pub line: U32,

    /// The module whose instrumentation defines it, or `NONE` for a label
    /// the recorder gives a thread from what its runtime says of it, as
    /// whether it holds the GIL.
    pub module: U32,

    /// The site record's kind: an `abi::SITE_` constant of
    /// `microscope-probes`.
    pub kind: u8,
    pub reserved: [u8; 3],
}

/// One instrumented zone, or one label's span.
#[repr(C)]
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, FromBytes, IntoBytes, KnownLayout, Immutable, Unaligned,
)]
pub struct Zone {
    pub start: I64,
    pub end: I64,
    pub site: U32,

    /// `ZONE_` flags.
    pub flags: U32,

    /// A label's value, as a string, or [`NONE`].
    pub value: U32,
    pub reserved: U32,
}

/// Its begin was lost, or came before the capture: it starts where its
/// thread's records were lost, or where the capture does.
pub const ZONE_BEGAN_UNSEEN: u32 = 1;

/// Its end was not seen: it ends where its thread's records were lost or
/// ended, or where a zone around it ended.
pub const ZONE_ENDED_UNSEEN: u32 = 2;

/// Its begin and end do not nest: a zone around it ended first, or no
/// begin matched its end.
pub const ZONE_MISMATCHED: u32 = 4;

pub const ZONE_FLAGS: u32 = ZONE_BEGAN_UNSEEN | ZONE_ENDED_UNSEEN | ZONE_MISMATCHED;

/// Where one thread's zones of one depth are.
#[repr(C)]
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, FromBytes, IntoBytes, KnownLayout, Immutable, Unaligned,
)]
pub struct ZoneRange {
    pub thread: U32,
    pub depth: U32,
    pub start: U32,
    pub count: U32,
}

/// One value of a plotted series.
#[repr(C)]
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, FromBytes, IntoBytes, KnownLayout, Immutable, Unaligned,
)]
pub struct Plot {
    pub time: I64,

    /// The value's bits, as an `f64`.
    pub value: U64,
    pub site: U32,
    pub thread: U32,
}

/// An instant on a thread's timeline.
#[repr(C)]
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, FromBytes, IntoBytes, KnownLayout, Immutable, Unaligned,
)]
pub struct Marker {
    pub time: I64,
    pub thread: U32,
    pub site: U32,

    /// A string, or [`NONE`].
    pub text: U32,
    pub kind: u8,
    pub reserved: [u8; 3],
}

/// What a [`Marker`] marks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum MarkerKind {
    /// The budget changed how much of each period is sampled; the text
    /// says to what, and why.
    Budget = 1,

    /// A task snapshot failed, or could not read some tasks; the text
    /// says why.
    TaskSnapshot = 2,

    /// A program's message, on its thread.
    Message = 3,

    /// The end of a program's frame.
    Frame = 4,

    /// A program asked for the window to be saved.
    Trigger = 5,

    /// microscope could not read a module's instrumentation; the text says
    /// why.
    Probes = 6,
}

impl MarkerKind {
    #[must_use]
    pub const fn from_number(number: u8) -> Option<Self> {
        match number {
            1 => Some(Self::Budget),
            2 => Some(Self::TaskSnapshot),
            3 => Some(Self::Message),
            4 => Some(Self::Frame),
            5 => Some(Self::Trigger),
            6 => Some(Self::Probes),
            _ => None,
        }
    }
}

/// A span of time the recording did not sample: no event counted or
/// sampled, though mappings and threads are still recorded.
#[repr(C)]
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, FromBytes, IntoBytes, KnownLayout, Immutable, Unaligned,
)]
pub struct Gap {
    /// From the moment sampling had stopped to the moment before it began
    /// again, on the capture's clock.
    pub start: I64,
    pub end: I64,

    /// A [`GapReason`].
    pub reason: u8,
}

/// Why a recording paused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum GapReason {
    /// To hold microscope's own CPU time to its budget.
    Budget = 1,

    /// A task snapshot held the program stopped.
    TaskSnapshot = 2,

    /// Microscope fell behind processing samples, and sampling was off
    /// until it caught up.
    Behind = 3,
}

impl GapReason {
    #[must_use]
    pub const fn from_number(number: u8) -> Option<Self> {
        match number {
            1 => Some(Self::Budget),
            2 => Some(Self::TaskSnapshot),
            3 => Some(Self::Behind),
            _ => None,
        }
    }
}

/// A task of a language runtime.
#[repr(C)]
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, FromBytes, IntoBytes, KnownLayout, Immutable, Unaligned,
)]
pub struct Task {
    /// The runtime's name, as a string: "goroutine", "task".
    pub runtime: U32,
    pub number: U64,

    /// The task's root frame, or [`NONE`].
    pub root: U32,

    /// Its labels, as a string `key=value` per line, or [`NONE`].
    pub labels: U32,
    pub first_seen: I64,
    pub last_seen: I64,
}

/// One task snapshot entry: a task waiting at a stack.
#[repr(C)]
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, FromBytes, IntoBytes, KnownLayout, Immutable, Unaligned,
)]
pub struct TaskWait {
    pub time: I64,

    /// The time it stands for.
    pub weight: U64,
    pub task: U32,
    pub stack: U32,

    /// The runtime's state, as a string.
    pub state: U32,
    pub reserved: U32,
}

/// The time of a call's end or entry that was not seen.
pub const UNSEEN: i64 = i64::MIN;

/// One system call.
#[repr(C)]
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, FromBytes, IntoBytes, KnownLayout, Immutable, Unaligned,
)]
pub struct Syscall {
    /// When it entered and returned, [`UNSEEN`] when either was not seen.
    pub entered: I64,
    pub exited: I64,

    /// What it returned: a negative errno when it failed.
    pub ret: I64,
    pub args: [U64; 6],

    /// Its thread's row.
    pub thread: U32,
    pub number: U32,

    /// The thread's stack, or [`NONE`].
    pub stack: U32,

    /// The bytes behind its arguments, as the engines' captures: each an
    /// argument, when it was read, its state, a pad byte, the bytes wanted
    /// and the bytes kept as `u32`s, then the bytes kept.
    pub captures: U32,
    pub captures_len: U32,

    /// A [`crate::syscalls::Abi`] by its index.
    pub abi: u8,

    /// A [`CallOutcome`].
    pub outcome: u8,

    /// The event's [`crate::syscalls::EventFlags`].
    pub flags: u8,
    pub reserved: u8,
}

/// How a call ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum CallOutcome {
    Returned = 0,
    Failed = 1,

    /// Still running when the trace ended.
    Unfinished = 2,

    /// Its process ended while it ran.
    ProcessExited = 3,

    /// Its result could not be read.
    Unavailable = 4,
}

impl CallOutcome {
    pub const fn from_number(number: u8) -> Option<Self> {
        Some(match number {
            0 => Self::Returned,
            1 => Self::Failed,
            2 => Self::Unfinished,
            3 => Self::ProcessExited,
            4 => Self::Unavailable,
            _ => return None,
        })
    }
}

/// Something a system call trace saw besides a call.
#[repr(C)]
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, FromBytes, IntoBytes, KnownLayout, Immutable, Unaligned,
)]
pub struct SyscallNote {
    pub time: I64,

    /// A signal's number, a child's pid, an exit's code as `wait(2)`
    /// encodes it, a descriptor, or a count lost.
    pub value: I64,

    /// A signal's code above its sender, or a device's numbers: block in
    /// the top bit, its major above its minor.
    pub extra: U64,

    /// Its thread's row, or [`NONE`].
    pub thread: U32,

    /// Its process's row, or [`NONE`].
    pub process: U32,

    /// A program's path, or what a descriptor refers to.
    pub text: U32,

    /// A signal's stack.
    pub stack: U32,

    /// A signal's `siginfo_t`, in the blobs.
    pub blob: U32,
    pub blob_len: U32,

    /// How many calls came before it, which places it among them: an
    /// engine may stamp a note later than the call after it.
    pub calls: U32,

    /// A [`NoteKind`].
    pub kind: u8,

    /// [`NoteKind`]'s flags.
    pub flags: u8,
    pub reserved: [u8; 10],
}

/// The bytes behind a call's arguments, decoded from its captures'
/// encoding, or `None` when it is malformed.
#[must_use]
pub fn captures(mut bytes: &[u8]) -> Option<Vec<crate::syscalls::Capture>> {
    use crate::syscalls::{Capture, CaptureState, ReadAt};
    let mut captures = Vec::new();

    while !bytes.is_empty() {
        let (head, rest) = bytes.split_first_chunk::<12>()?;
        let word =
            |at: usize| u32::from_le_bytes([head[at], head[at + 1], head[at + 2], head[at + 3]]);
        let len = word(8) as usize;
        if rest.len() < len {
            return None;
        }

        let state = match head[2] {
            0 => CaptureState::Complete,
            1 => CaptureState::Truncated { wanted: word(4) },
            2 => CaptureState::Unreadable,
            3 => CaptureState::NotRequested,
            _ => return None,
        };

        let when = match head[1] {
            0 => ReadAt::Entry,
            1 => ReadAt::Exit,
            _ => return None,
        };

        captures.push(Capture {
            arg: head[0],
            when,
            state,
            bytes: rest[..len].into(),
        });
        bytes = &rest[len..];
    }

    Some(captures)
}

/// A [`SyscallNote`]'s flags: a signal's code and sender, a spawn's
/// thread, and a descriptor's facts.
pub mod note_flags {
    pub const SIGNAL_CODE: u8 = 1;

    pub const SIGNAL_SENDER: u8 = 2;

    pub const SPAWN_THREAD: u8 = 1;

    pub const FD_CLOEXEC: u8 = 1;

    pub const FD_MADE: u8 = 2;

    pub const FD_DEVICE: u8 = 4;
}

/// What a [`SyscallNote`] says.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum NoteKind {
    /// A signal delivered; flags say whether it has a code and a sender.
    Signal = 1,

    /// A process or thread made; flag 1 when a thread.
    Spawn = 2,
    Exec = 3,
    Exit = 4,
    ThreadExit = 5,

    /// What a descriptor refers to; flags: close-on-exec, made by the next
    /// call, a device.
    Descriptor = 6,
    Lost = 7,
}

impl NoteKind {
    pub const fn from_number(number: u8) -> Option<Self> {
        Some(match number {
            1 => Self::Signal,
            2 => Self::Spawn,
            3 => Self::Exec,
            4 => Self::Exit,
            5 => Self::ThreadExit,
            6 => Self::Descriptor,
            7 => Self::Lost,
            _ => return None,
        })
    }
}

/// One process's statistics of one call.
#[repr(C)]
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, FromBytes, IntoBytes, KnownLayout, Immutable, Unaligned,
)]
pub struct SyscallStat {
    pub calls: U64,
    pub errors: U64,
    pub timed: U64,
    pub total_ns: U64,
    pub min_ns: U64,
    pub max_ns: U64,
    pub bytes: U64,
    pub log2_ns: [U64; crate::syscalls::stats::BUCKETS],

    /// The process's row, or [`NONE`] for every process.
    pub process: U32,
    pub number: U32,

    /// Its rows of errnos.
    pub errnos: U32,
    pub errno_count: U32,
    pub abi: u8,
    pub reserved: [u8; 7],
}

/// How many of a call's failures had one errno.
#[repr(C)]
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, FromBytes, IntoBytes, KnownLayout, Immutable, Unaligned,
)]
pub struct SyscallErrno {
    pub count: U64,
    pub errno: U16,
    pub reserved: [u8; 6],
}

macro_rules! records {
    ($($record:ty => $table:ident),* $(,)?) => {
        /// Each table's record size, for the directory.
        pub const fn stride(table: CaptureTable) -> usize {
            match table {
                CaptureTable::Strings | CaptureTable::SyscallBlobs => 1,
                CaptureTable::SampleRanges => size_of::<ThreadRange>(),
                CaptureTable::OffCpuRanges => size_of::<ThreadRange>(),
                $(CaptureTable::$table => size_of::<$record>(),)*
            }
        }

        /// The schema as text, which fingerprints every record's layout.
        pub fn schema_text() -> String {
            let mut text = String::from("capture\n");
            $(text.push_str(&format!(
                "{} {} {}\n",
                CaptureTable::$table as u32,
                stringify!($record),
                size_of::<$record>()
            ));)*
            text
        }
    };
}

records! {
    Setting => Settings,
    Module => Modules,
    Process => Processes,
    Thread => Threads,
    Mapping => Mappings,
    Frame => Frames,
    Stack => Stacks,
    Sample => Samples,
    OffCpu => OffCpu,
    Range => FrameSymbols,
    Line => Lines,
    Function => Functions,
    Account => Accounting,
    Site => Sites,
    Zone => Zones,
    ZoneRange => ZoneRanges,
    Plot => Plots,
    Marker => Markers,
    Task => Tasks,
    TaskWait => TaskWaits,
    Gap => Gaps,
    Syscall => Syscalls,
    SyscallNote => SyscallNotes,
    SyscallStat => SyscallStats,
    SyscallErrno => SyscallErrnos,
}

const _: () = assert!(size_of::<Syscall>() == 96 && size_of::<SyscallNote>() == 64);

const _: () = assert!(size_of::<Frame>() == 16);
const _: () = assert!(size_of::<Stack>() == 8);
const _: () = assert!(size_of::<Sample>() == 24);
const _: () = assert!(size_of::<OffCpu>() == 17);
const _: () = assert!(size_of::<Zone>() == 32);
