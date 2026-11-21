use zerocopy::little_endian::{I64, U16, U32, U64};
use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout, Unaligned};

use crate::CodeRole;

pub use crate::image::NONE;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u32)]
pub enum CaptureTable {
    Strings = 1,

    Settings = 2,
    Modules = 3,
    Processes = 4,
    Threads = 5,
    Mappings = 6,
    Frames = 7,
    Stacks = 8,

    Samples = 9,

    SampleRanges = 10,

    FrameSymbols = 11,
    Lines = 12,
    Functions = 13,

    Accounting = 14,

    OffCpu = 15,

    OffCpuRanges = 16,

    Sites = 17,

    Zones = 18,

    ZoneRanges = 19,

    Plots = 20,

    Markers = 21,

    Tasks = 22,

    TaskWaits = 23,

    Gaps = 24,

    Syscalls = 25,

    SyscallBlobs = 26,

    SyscallNotes = 27,

    SyscallStats = 28,

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

    #[must_use]
    pub const fn index(self) -> usize {
        self as usize - 1
    }

    #[must_use]
    pub fn from_number(number: u32) -> Option<Self> {
        Self::ALL.into_iter().find(|kind| *kind as u32 == number)
    }
}

#[repr(C)]
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, FromBytes, IntoBytes, KnownLayout, Immutable, Unaligned,
)]
pub struct Setting {
    pub key: U32,
    pub value: U32,
}

#[repr(C)]
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, FromBytes, IntoBytes, KnownLayout, Immutable, Unaligned,
)]
pub struct Module {
    pub path: U32,

    pub build_id: U32,

    pub digest: [u8; 16],
    pub size: U64,
}

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

    pub bias: U64,
    pub time: I64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u8)]
pub enum FrameKind {
    Native = 0,

    Truncated = 1,

    TaskRoot = 2,

    Runtime = 3,

    Awaited = 4,

    Interpreted = 5,

    Kernel = 6,
}

impl FrameKind {
    #[must_use]
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

    #[must_use]
    pub const fn named(self) -> bool {
        matches!(self, Self::Awaited | Self::Interpreted | Self::Kernel)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u8)]
pub enum Truncation {
    NoUnwindInfo = 1,

    FramePointerNotKept = 2,

    StackCopyExhausted = 3,

    UnmappedAddress = 4,

    DepthLimit = 5,

    ChainEnded = 6,

    InvalidCaller = 7,

    NoStack = 8,

    StackSwitch = 9,

    AwaitChainEnded = 10,

    Unreadable = 11,

    InterpretedUnknown = 12,

    InterpretedEnded = 13,

    InterpretedUnplaced = 14,

    ChainLimit = 15,
}

impl Truncation {
    pub const ALL: [Self; 15] = [
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
        Self::ChainLimit,
    ];

    #[must_use]
    pub fn from_number(number: u64) -> Option<Self> {
        Self::ALL.into_iter().find(|kind| *kind as u64 == number)
    }

    #[must_use]
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
            Self::ChainLimit => "kernel call chain limit",
        }
    }
}

#[repr(C)]
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, FromBytes, IntoBytes, KnownLayout, Immutable, Unaligned,
)]
pub struct Frame {
    pub address: U64,
    pub module: U32,

    pub kind: u8,

    pub role: u8,
    pub reserved: U16,
}

#[repr(C)]
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, FromBytes, IntoBytes, KnownLayout, Immutable, Unaligned,
)]
pub struct Stack {
    pub parent: U32,
    pub frame: U32,
}

#[repr(C)]
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, FromBytes, IntoBytes, KnownLayout, Immutable, Unaligned,
)]
pub struct Sample {
    pub time: I64,

    pub weight: U64,
    pub stack: U32,

    pub task: U32,
}

#[repr(C)]
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, FromBytes, IntoBytes, KnownLayout, Immutable, Unaligned,
)]
pub struct OffCpu {
    pub time: I64,

    pub duration: U64,

    pub preempted: u8,

    pub flags: u8,
}

pub const OFF_CPU_BEGAN_UNSEEN: u8 = 1;

pub const OFF_CPU_ENDED_UNSEEN: u8 = 2;

#[repr(C)]
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, FromBytes, IntoBytes, KnownLayout, Immutable, Unaligned,
)]
pub struct ThreadRange {
    pub thread: U32,
    pub start: U32,
    pub count: U32,
}

#[repr(C)]
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, FromBytes, IntoBytes, KnownLayout, Immutable, Unaligned,
)]
pub struct Range {
    pub start: U32,
    pub count: U32,
}

#[repr(C)]
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, FromBytes, IntoBytes, KnownLayout, Immutable, Unaligned,
)]
pub struct Line {
    pub function: U32,

    pub file: U32,
    pub number: U32,
    pub column: U32,
}

#[repr(C)]
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, FromBytes, IntoBytes, KnownLayout, Immutable, Unaligned,
)]
pub struct Function {
    pub name: U32,
    pub module: U32,

    pub file: U32,
    pub line: U32,

    pub role: u8,
    pub reserved: [u8; 3],
}

#[must_use]
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

#[must_use]
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u32)]
pub enum AccountKind {
    LostSamples = 1,

    LostRecords = 30,

    Throttles = 2,

    Truncated = 1000,

    Malformed = 3,

    StackBytes = 7,

    LateRecords = 11,

    Repeats = 12,

    Counted = 13,

    RecorderCpu = 14,

    RecorderLoading = 15,

    SnapshotPause = 16,

    Snapshots = 17,

    SnapshotFailures = 18,

    TaskSamples = 19,

    ProbeEvents = 20,

    ProbeOverwritten = 21,

    ProbeNoRing = 22,

    ProbeInterrupted = 23,

    ProbeMalformed = 24,

    SnapshotChanged = 25,

    LiveSnapshots = 26,

    LiveRead = 31,

    SyscallsLost = 27,

    SyscallsUnseen = 36,

    SyscallsCapped = 37,

    SyscallsOverflowed = 38,

    InterpretedLost = 28,

    RecorderSetup = 29,

    ShortenedStacks = 32,

    ProbesUnread = 33,

    ProbesRead = 34,

    OffCpuLost = 35,

    RaisedSamples = 43,

    KeptSamples = 44,

    SampledCpu = 45,

    SyscallsSkipped = 46,
}

#[repr(C)]
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, FromBytes, IntoBytes, KnownLayout, Immutable, Unaligned,
)]
pub struct Account {
    pub kind: U32,
    pub reserved: U32,
    pub value: U64,
}

#[repr(C)]
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, FromBytes, IntoBytes, KnownLayout, Immutable, Unaligned,
)]
pub struct Site {
    pub name: U32,
    pub file: U32,
    pub line: U32,

    pub module: U32,

    pub kind: u8,
    pub reserved: [u8; 3],
}

#[repr(C)]
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, FromBytes, IntoBytes, KnownLayout, Immutable, Unaligned,
)]
pub struct Zone {
    pub start: I64,
    pub end: I64,
    pub site: U32,

    pub flags: U32,

    pub value: U32,
    pub reserved: U32,
}

pub const ZONE_BEGAN_UNSEEN: u32 = 1;

pub const ZONE_ENDED_UNSEEN: u32 = 2;

pub const ZONE_MISMATCHED: u32 = 4;

pub const ZONE_LOST_WITHIN: u32 = 8;

pub const ZONE_FLAGS: u32 =
    ZONE_BEGAN_UNSEEN | ZONE_ENDED_UNSEEN | ZONE_MISMATCHED | ZONE_LOST_WITHIN;

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

#[repr(C)]
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, FromBytes, IntoBytes, KnownLayout, Immutable, Unaligned,
)]
pub struct Plot {
    pub time: I64,

    pub value: U64,
    pub site: U32,
    pub thread: U32,
}

#[repr(C)]
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, FromBytes, IntoBytes, KnownLayout, Immutable, Unaligned,
)]
pub struct Marker {
    pub time: I64,
    pub thread: U32,
    pub site: U32,

    pub text: U32,
    pub kind: u8,
    pub reserved: [u8; 3],
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum MarkerKind {
    Budget = 1,

    TaskSnapshot = 2,

    Message = 3,

    Frame = 4,

    Trigger = 5,

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

#[repr(C)]
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, FromBytes, IntoBytes, KnownLayout, Immutable, Unaligned,
)]
pub struct Gap {
    pub start: I64,
    pub end: I64,

    pub reason: u8,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum GapReason {
    TaskSnapshot = 2,

    Behind = 3,
}

impl GapReason {
    #[must_use]
    pub const fn from_number(number: u8) -> Option<Self> {
        match number {
            2 => Some(Self::TaskSnapshot),
            3 => Some(Self::Behind),
            _ => None,
        }
    }
}

#[repr(C)]
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, FromBytes, IntoBytes, KnownLayout, Immutable, Unaligned,
)]
pub struct Task {
    pub runtime: U32,
    pub number: U64,

    pub root: U32,

    pub labels: U32,
    pub first_seen: I64,
    pub last_seen: I64,
}

#[repr(C)]
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, FromBytes, IntoBytes, KnownLayout, Immutable, Unaligned,
)]
pub struct TaskWait {
    pub time: I64,

    pub weight: U64,
    pub task: U32,
    pub stack: U32,

    pub state: U32,
    pub reserved: U32,
}

pub const UNSEEN: i64 = i64::MIN;

#[repr(C)]
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, FromBytes, IntoBytes, KnownLayout, Immutable, Unaligned,
)]
pub struct Syscall {
    pub entered: I64,
    pub exited: I64,

    pub ret: I64,
    pub args: [U64; 6],

    pub thread: U32,
    pub number: U32,

    pub stack: U32,

    pub captures: U32,
    pub captures_len: U32,

    pub abi: u8,

    pub outcome: u8,

    pub flags: u8,
    pub reserved: u8,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum CallOutcome {
    Returned = 0,
    Failed = 1,

    Unfinished = 2,

    ProcessExited = 3,

    Unavailable = 4,
}

impl CallOutcome {
    #[must_use]
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

#[repr(C)]
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, FromBytes, IntoBytes, KnownLayout, Immutable, Unaligned,
)]
pub struct SyscallNote {
    pub time: I64,

    pub value: I64,

    pub extra: U64,

    pub thread: U32,

    pub process: U32,

    pub text: U32,

    pub stack: U32,

    pub blob: U32,
    pub blob_len: U32,

    pub calls: U32,

    pub kind: u8,

    pub flags: u8,
    pub reserved: [u8; 10],
}

#[must_use]
pub fn captures(mut bytes: &[u8]) -> Option<Vec<crate::syscalls::Capture>> {
    use crate::syscalls::{Capture, CaptureState, ReadAt};
    let mut captures = Vec::new();

    while !bytes.is_empty() {
        let (fields, rest) = bytes.split_first_chunk::<4>()?;
        let (wanted, rest) = rest.split_first_chunk::<4>()?;
        let (len, rest) = rest.split_first_chunk::<4>()?;
        let [arg, when, state, _] = *fields;
        let len = u32::from_le_bytes(*len) as usize;
        let (held, rest) = rest.split_at_checked(len)?;

        let state = match state {
            0 => CaptureState::Complete,
            1 => CaptureState::Truncated {
                wanted: u32::from_le_bytes(*wanted),
            },
            2 => CaptureState::Unreadable,
            3 => CaptureState::NotRequested,
            _ => return None,
        };

        let when = match when {
            0 => ReadAt::Entry,
            1 => ReadAt::Exit,
            _ => return None,
        };

        captures.push(Capture {
            arg,
            when,
            state,
            bytes: held.into(),
        });
        bytes = rest;
    }

    Some(captures)
}

pub mod note_flags {
    pub const SIGNAL_CODE: u8 = 1;

    pub const SIGNAL_SENDER: u8 = 2;

    pub const SPAWN_THREAD: u8 = 1;

    pub const FD_CLOEXEC: u8 = 1;

    pub const FD_MADE: u8 = 2;

    pub const FD_DEVICE: u8 = 4;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum NoteKind {
    Signal = 1,

    Spawn = 2,
    Exec = 3,
    Exit = 4,
    ThreadExit = 5,

    Descriptor = 6,
    Lost = 7,
}

impl NoteKind {
    #[must_use]
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

    pub process: U32,

    pub thread: U32,
    pub number: U32,

    pub errnos: U32,
    pub errno_count: U32,
    pub abi: u8,
    pub reserved: [u8; 3],
}

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
        pub const fn stride(table: CaptureTable) -> usize {
            match table {
                CaptureTable::Strings | CaptureTable::SyscallBlobs => 1,
                CaptureTable::SampleRanges => size_of::<ThreadRange>(),
                CaptureTable::OffCpuRanges => size_of::<ThreadRange>(),
                $(CaptureTable::$table => size_of::<$record>(),)*
            }
        }

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
const _: () = assert!(size_of::<OffCpu>() == 18);
const _: () = assert!(size_of::<Zone>() == 32);
