//! The BPF engine for a recording's system calls, which never stops the
//! traced program.
//!
//! Programs in the kernel filter by process, time each chosen call, copy
//! the bytes behind its arguments, and send one record per call through a
//! ring buffer.
//!
//! The program (`src/backend/linux/bpf/syscalls.bpf.c`) is built by
//! `build.rs` and embedded. Records and map values cross the boundary as
//! the plain structures of `bpf/records.h`, which this module mirrors
//! field for field; a test
//! compares both layouts through the object's BTF. Decoding a record is
//! pure. Everything else, from naming a call to printing it, is shared
//! with the ptrace engine.

use std::collections::{BTreeMap, BTreeSet};
use std::os::fd::AsFd as _;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use aya::maps::{
    Array, ArrayOfMaps, HashMap, MapData, PerCpuArray, PerCpuHashMap, PerCpuValues, RingBuf,
};

use aya::programs::{BtfTracePoint, FEntry, FExit, Iter, KProbe};
use aya::{Btf, Ebpf, EbpfLoader};
use aya_obj::btf::BtfKind;
use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout};

use super::super::bpf::sampler::{self, PythonLayouts, V8Layouts};
use super::super::perf;
use super::Sink;
use super::ptrace::{Finished, TraceOptions, monotonic, seed_descriptors};

use super::pairs::{Pairs, Taken};
use super::rings::Merge;
use crate::syscalls::captures::{self, CopyKind};
use crate::syscalls::specs::{self, Arg, Count, MemKind, Ret, Struct};
use crate::syscalls::stats::{self, BUCKETS, Folded};
use crate::syscalls::{
    Abi, CallStack, Capture, CaptureState, Device, EventFlags, InFlight, Loss, Outcome, Pid,
    ProcessEnd, ReadAt, SyscallEvent, SyscallId, SyscallStats, Tid, TraceEvent,
};

use crate::capture::records::Truncation;
use crate::unwind::DwarfRegister;
use crate::unwind::sample::SampledState;

/// The programs, as `build.rs` compiled them.
static OBJECT: &[u8] = aya::include_bytes_aligned!(concat!(env!("OUT_DIR"), "/syscalls.bpf.o"));

/// The tracepoints the programs attach to, by program name.
const PROGRAMS: [(&str, &str); 5] = [
    ("sys_enter", "sys_enter"),
    ("sys_exit", "sys_exit"),
    ("process_fork", "sched_process_fork"),
    ("process_exec", "sched_process_exec"),
    ("signal_deliver", "signal_deliver"),
];

const RECORD_CALL: u32 = 1;
const RECORD_ENTRY: u32 = 2;
const RECORD_SPAWN: u32 = 3;
const RECORD_EXEC: u32 = 4;
const RECORD_EXIT: u32 = 5;
const RECORD_SIGNAL: u32 = 6;
const RECORD_FILE: u32 = 7;
const RECORD_STACK: u32 = 8;
const RECORD_PY_STACK: u32 = 9;
const RECORD_V8_STACK: u32 = 10;
const RECORD_HANDOFF: u32 = 11;

const MAKES_RESULT: u8 = 1;
const MAKES_PAIR: u8 = 2;

const PIPEFS_MAGIC: u64 = 0x5049_5045;
const SOCKFS_MAGIC: u64 = 0x534f_434b;
const ANON_INODE_FS_MAGIC: u64 = 0x0904_1934;
const S_IFMT: u32 = 0o170_000;
const S_IFCHR: u32 = 0o020_000;
const S_IFBLK: u32 = 0o060_000;

const CONFIG_STREAM: u32 = 1;
const CONFIG_COUNT: u32 = 2;
const CONFIG_FOLLOW: u32 = 4;
const CONFIG_SKIP_UNSEEN: u32 = 8;
const CONFIG_STACKS: u32 = 16;
const CONFIG_STACK_COPY: u32 = 32;
const CONFIG_GENERATION: u32 = 64;
const CONFIG_EXCEPTIONS: u32 = 128;

const CALL_PROCESS_EXITED: u8 = 8;
const CALL_STACK_LOST: u8 = 32;
const SIGNAL_INFO_LOST: u32 = 1;

/// The stack of a call or signal whose own did not fit in the ring.
fn lost_stack() -> CallStack {
    CallStack {
        addresses: Arc::from([]),
        truncated: Some(Truncation::NoStack as u8),
        interpreted: Arc::from([]),
    }
}

/// The flags a call record shares with [`EventFlags`].
const CALL_EVENT_FLAGS: u8 = EventFlags::ENTRY_UNSEEN.0
    | EventFlags::ABI_UNCERTAIN.0
    | EventFlags::KEPT_FAILED.0
    | EventFlags::KEPT_SLOW.0;

/// A call's slot in the CALLS map: its ABI and number.
const SLOTS: u32 = 3072;

// Each chosen call's statistics row is a `u16` in its CALLS entry.
const _: () = assert!(SLOTS <= 1 << 16, "statistics rows outgrow their u16");

/// Each CPU's ring's default size, which a thread making 3 M calls a
/// second fills in no less than a few milliseconds.
pub const DEFAULT_RING: u32 = 2 << 20;

/// Each CPU's ring's default size when each call copies its stack.
///
/// A call's records then take about 8.6 KiB, 20 times what they take with
/// the kernel's walk alone, so that a 2 MiB ring holds some 240 calls and
/// an 8 MiB one some 970: a burst of that many on one CPU loses none, nor
/// does a thread making 10,000 calls a second while the reader is held up
/// for a tenth of a second. A memory cgroup that cannot hold the rings
/// gets smaller ones, down to [`SMALLEST_RING`].
pub const STACK_COPY_RING: u32 = 8 << 20;

/// The smallest ring a memory cgroup short of room for the default gets.
const SMALLEST_RING: u32 = 256 << 10;

/// The most bytes of a string or buffer kept once strings are shed.
pub const CAPPED_BYTES: u32 = 16;

/// The most elements of an array kept once strings are shed.
pub const CAPPED_ELEMENTS: u32 = 4;

/// One call in this many is sent once calls are sampled.
pub const SAMPLED_EVERY: u32 = 16;

/// What the programs cost a call, as `microscope-tools syscall-overhead`
/// and the 8-thread streaming loop measured them on a Ryzen 9 9950X:
/// counted, sent, sent with the kernel's walk of its stack, and with a
/// copy of its stack.
const COUNTED_NS: u64 = 150;
const STREAMED_NS: u64 = 220;
const STACK_NS: u64 = 560;
const COPIED_STACK_NS: u64 = 1_100;

/// What a call costs the programs loaded with `flags` that shed `shed`.
const fn per_call(flags: u32, shed: Shed) -> u64 {
    if flags & CONFIG_STREAM == 0 || shed.has(Shed::STREAM) {
        COUNTED_NS
    } else if shed.has(Shed::SAMPLED) {
        COUNTED_NS + STREAMED_NS / SAMPLED_EVERY as u64
    } else if shed.has(Shed::STACKS) || flags & CONFIG_STACKS == 0 {
        STREAMED_NS
    } else if flags & CONFIG_STACK_COPY != 0 {
        COPIED_STACK_NS
    } else {
        STACK_NS
    }
}

/// What a trace gives up to stay inside its recording's budget.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Shed(pub u8);

impl Shed {
    pub const STACKS: Self = Self(1);

    /// Strings and buffers past [`CAPPED_BYTES`].
    pub const STRINGS: Self = Self(2);

    /// Every call but one in [`SAMPLED_EVERY`], which are counted.
    pub const SAMPLED: Self = Self(4);

    /// Every call but those that fail or are slow, which are counted.
    pub const STREAM: Self = Self(8);

    #[must_use]
    pub const fn has(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }
}

/// How one argument is copied (`struct arg_copy`).
#[repr(C)]
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, FromBytes, IntoBytes, Immutable, KnownLayout,
)]
pub struct RawArgCopy {
    pub kind: u8,
    pub when: u8,
    pub writes: u8,
    pub count: u8,
    pub count_arg: u8,
    pub pad: [u8; 3],
    pub size: u32,
    pub limit: u32,
}

/// One call's slot in the CALLS map (`struct call_spec`).
#[repr(C)]
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, FromBytes, IntoBytes, Immutable, KnownLayout,
)]
pub struct RawCallSpec {
    pub selected: u8,
    pub moves: u8,
    pub makes: u8,
    pub makes_arg: u8,
    pub forks: u8,
    pub execs: u8,
    pub stat: u16,
    pub ends: u8,
    pub waits: u8,
    pub pad: u16,
    pub args: [RawArgCopy; 6],
}

/// `struct config`.
#[repr(C)]
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, FromBytes, IntoBytes, Immutable, KnownLayout,
)]
pub struct RawConfig {
    pub flags: u32,
    pub elements: u32,
    pub sample: u32,
    pub rows: u32,
    pub slow_ns: u64,
    pub cap: u32,
    pub burst: u32,
}

/// A thread's call in progress (`struct entry`).
#[repr(C)]
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, FromBytes, IntoBytes, Immutable, KnownLayout,
)]
pub struct RawEntry {
    pub entered: u64,
    pub args: [u64; 6],
    pub nr: u32,
    pub tgid: u32,
    pub exec_tid: u32,
    pub abi: u8,
    pub flags: u8,
    pub stacked: u8,
    pub pad: u8,
}

/// `struct in_flight_record`.
#[repr(C)]
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, FromBytes, IntoBytes, Immutable, KnownLayout,
)]
pub struct RawInFlightRecord {
    pub tid: u32,
    pub pad: u32,
    pub entry: RawEntry,
}

/// Every record's start (`struct record`).
#[repr(C)]
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, FromBytes, IntoBytes, Immutable, KnownLayout,
)]
pub struct RawRecord {
    pub kind: u32,
    pub size: u32,
    pub tid: u32,
    pub tgid: u32,
}

/// `struct call_record`, which its captures follow.
#[repr(C)]
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, FromBytes, IntoBytes, Immutable, KnownLayout,
)]
pub struct RawCallRecord {
    pub header: RawRecord,
    pub entered: u64,
    pub exited: u64,
    pub ret: i64,
    pub args: [u64; 6],
    pub nr: u32,
    pub abi: u8,
    pub flags: u8,
    pub captures: u16,
}

/// `struct capture_header`, which its bytes follow, padded to eight.
#[repr(C)]
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, FromBytes, IntoBytes, Immutable, KnownLayout,
)]
pub struct RawCaptureHeader {
    pub arg: u8,
    pub when: u8,
    pub state: u8,
    pub pad: u8,
    pub len: u32,
    pub wanted: u32,
    pub pad2: u32,
}

/// `struct spawn_record`.
#[repr(C)]
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, FromBytes, IntoBytes, Immutable, KnownLayout,
)]
pub struct RawSpawnRecord {
    pub header: RawRecord,
    pub time: u64,
    pub child: u32,
    pub child_tgid: u32,
}

/// `struct exec_record`, which the new image's path follows.
#[repr(C)]
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, FromBytes, IntoBytes, Immutable, KnownLayout,
)]
pub struct RawExecRecord {
    pub header: RawRecord,
    pub time: u64,
    pub pad: u32,
    pub len: u32,
}

/// `struct handoff_record`.
#[repr(C)]
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, FromBytes, IntoBytes, Immutable, KnownLayout,
)]
pub struct RawHandoffRecord {
    pub header: RawRecord,
    pub entered: u64,
    pub former: u32,
    pub pad: u32,
}

/// `struct exit_record`.
#[repr(C)]
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, FromBytes, IntoBytes, Immutable, KnownLayout,
)]
pub struct RawExitRecord {
    pub header: RawRecord,
    pub time: u64,
    pub code: i32,
    pub last: u32,
}

/// `struct signal_record`.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, FromBytes, IntoBytes, Immutable, KnownLayout)]
pub struct RawSignalRecord {
    pub header: RawRecord,
    pub time: u64,
    pub signal: i32,
    pub flags: u32,
    pub info: [u8; 48],
}

/// `struct file_record`, which the dentry's name follows.
#[repr(C)]
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, FromBytes, IntoBytes, Immutable, KnownLayout,
)]
pub struct RawFileRecord {
    pub header: RawRecord,
    pub fd: i32,
    pub mode: u32,
    pub ino: u64,
    pub magic: u64,
    pub rdev: u32,
    pub len: u32,
    pub cloexec: u32,
    pub pad: u32,
}

/// `struct stack_record`, which the kernel's walk and the stack's copy
/// follow.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, FromBytes, IntoBytes, Immutable, KnownLayout)]
pub struct RawStackRecord {
    pub header: RawRecord,
    pub frames: u32,
    pub copied: u32,
    pub entered: u64,
    pub interpreted: u32,
    pub pad: u32,
    pub regs: [u64; 17],
}

/// `struct stat_value`.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, FromBytes, IntoBytes, Immutable, KnownLayout)]
pub struct RawStatValue {
    pub calls: u64,
    pub errors: u64,
    pub timed: u64,
    pub total_ns: u64,
    pub min_ns: u64,
    pub max_ns: u64,
    pub bytes: u64,
    pub log2_ns: [u64; BUCKETS],
}

/// `struct stat_key`.
#[repr(C)]
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, FromBytes, IntoBytes, Immutable, KnownLayout,
)]
pub struct RawStatKey {
    pub tid: u32,
    pub tgid: u32,
    pub row: u32,
    pub reserved: u32,
}

/// `struct errno_key`.
#[repr(C)]
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, FromBytes, IntoBytes, Immutable, KnownLayout,
)]
pub struct RawErrnoKey {
    pub tid: u32,
    pub tgid: u32,
    pub slot: u32,
    pub err: u32,
}

/// Map values cross as bytes, which aya can hold without unsafe code.
type Bytes<const N: usize> = [u8; N];
const SPEC_BYTES: usize = size_of::<RawCallSpec>();
const STAT_BYTES: usize = size_of::<RawStatValue>();
const STAT_KEY_BYTES: usize = size_of::<RawStatKey>();
const ERRNO_KEY_BYTES: usize = size_of::<RawErrnoKey>();
const CONFIG_BYTES: usize = size_of::<RawConfig>();

const fn abi_of(code: u8) -> Option<Abi> {
    match code {
        0 => Some(Abi::X86_64),
        1 => Some(Abi::X86_32),
        2 => Some(Abi::X32),
        _ => None,
    }
}

/// A call's slot, as the programs key it: its ABI above ten bits of
/// number. A larger number, which no table names, folds into those bits
/// as it does in the programs.
#[must_use]
pub const fn errno_slot(call: SyscallId) -> u32 {
    let abi = match call.abi {
        Abi::X86_64 => 0,
        Abi::X86_32 => 1,
        Abi::X32 => 2,
    };

    abi << 10 | (call.number & 1023)
}

/// The record the stream program writes for a call it read nothing of.
///
/// The call returned `returned`, or ended its process first, and was made
/// at the first time and returned at the second. A model of the stream
/// feeds such records to the decoder.
#[must_use]
pub fn encode_call(
    thread: u32,
    process: u32,
    call: SyscallId,
    args: [u64; 6],
    returned: Option<i64>,
    (entered, exited): (u64, u64),
) -> Vec<u8> {
    let record = RawCallRecord {
        header: RawRecord {
            kind: RECORD_CALL,
            size: u32::try_from(size_of::<RawCallRecord>()).unwrap_or(u32::MAX),
            tid: thread,
            tgid: process,
        },
        entered,
        exited,
        ret: returned.unwrap_or(0),
        args,
        nr: call.number,
        abi: u8::try_from(errno_slot(call) >> 10).unwrap_or(u8::MAX),
        flags: if returned.is_some() {
            0
        } else {
            CALL_PROCESS_EXITED
        },
        captures: 0,
    };

    record.as_bytes().to_vec()
}

/// The call a CALLS slot holds: its ABI above ten bits of number.
fn call_of(slot: u32) -> Option<SyscallId> {
    let abi = abi_of(u8::try_from(slot >> 10).ok()?)?;
    Some(SyscallId::new(abi, slot & 1023))
}

/// A call's CALLS slot value: whether it is chosen, and how to copy each
/// argument, from the rules the ptrace engine follows.
#[must_use]
pub fn call_spec(call: SyscallId, options: &TraceOptions) -> RawCallSpec {
    let mut spec = RawCallSpec {
        selected: 1,
        moves: u8::from(stats::moves(call)),
        forks: u8::from(matches!(
            call.name(),
            Some("clone" | "clone3" | "fork" | "vfork")
        )),
        execs: u8::from(matches!(call.name(), Some("execve" | "execveat"))),
        ends: u8::from(matches!(call.name(), Some("exit" | "exit_group"))),
        waits: waits(call),
        ..RawCallSpec::default()
    };

    if let Some(described) = specs::spec(call) {
        if described.ret == Ret::Fd {
            spec.makes = MAKES_RESULT;
        }

        let pair = described.args.iter().position(|arg| {
            matches!(
                arg,
                Arg::Mem {
                    kind: MemKind::Struct(Struct::Fds2),
                    ..
                }
            )
        });
        if let Some(arg) = pair.and_then(|arg| u8::try_from(arg).ok()) {
            spec.makes = MAKES_PAIR;
            spec.makes_arg = arg;
        }
    }

    for copy in captures::copies(call, options.limits, options.verbose) {
        let (count, count_arg) = match copy.count {
            Count::One => (0, 0),
            Count::Arg(n) => (1, n),
            Count::Return => (2, 0),
            Count::PointedBy(n) => (3, n),
        };

        #[expect(
            clippy::indexing_slicing,
            reason = "specs::parse rejects a seventh argument, so copies name arguments 0 to 5"
        )]
        let slot = &mut spec.args[usize::from(copy.arg)];
        *slot = RawArgCopy {
            kind: match copy.kind {
                CopyKind::Path => 1,
                CopyKind::String => 2,
                CopyKind::Strings => 3,
                CopyKind::StringsCounted => 4,
                CopyKind::Bytes => 5,
                CopyKind::Iovec => 6,
                CopyKind::Sockaddr => 7,
                CopyKind::Struct => 8,
                CopyKind::FdSet => 9,
            },
            // A tracepoint cannot fault pages in, so what a call reads
            // is copied once the call has, at its exit.
            when: u8::from(copy.when == ReadAt::Exit || !reads_at_entry(call)),
            writes: u8::from(copy.writes),
            count,
            count_arg,
            pad: [0; 3],
            size: copy.size,
            limit: copy.limit,
        };
    }

    spec
}

/// Whether a call's input must be read at its entry: `execve` leaves
/// nothing of the old image to read at its exit, and `rseq` registers an
/// area the kernel writes before the call returns.
fn reads_at_entry(call: SyscallId) -> bool {
    matches!(call.name(), Some("execve" | "execveat" | "rseq"))
}

/// Whether a call waits for another thread, a device, or a clock, as
/// `enum waits` says: such a call's stack is taken as it begins, while
/// the thread waits, rather than at its exit, once it is woken.
///
/// `futex` waits as its operation says, which the kernel reads.
fn waits(call: SyscallId) -> u8 {
    const WAITS: u8 = 1;
    const WAITS_BY_FUTEX_OP: u8 = 2;
    match call.name() {
        Some("futex" | "futex_time64") => WAITS_BY_FUTEX_OP,
        Some(
            "read"
            | "readv"
            | "pread64"
            | "preadv"
            | "preadv2"
            | "recvfrom"
            | "recvmsg"
            | "recvmmsg"
            | "recvmmsg_time64"
            | "accept"
            | "accept4"
            | "connect"
            | "poll"
            | "ppoll"
            | "ppoll_time64"
            | "select"
            | "_newselect"
            | "pselect6"
            | "pselect6_time64"
            | "epoll_wait"
            | "epoll_pwait"
            | "epoll_pwait2"
            | "nanosleep"
            | "clock_nanosleep"
            | "clock_nanosleep_time64"
            | "wait4"
            | "waitid"
            | "waitpid"
            | "pause"
            | "sigsuspend"
            | "rt_sigsuspend"
            | "rt_sigtimedwait"
            | "rt_sigtimedwait_time64"
            | "futex_wait"
            | "futex_waitv"
            | "semop"
            | "semtimedop"
            | "semtimedop_time64"
            | "msgrcv"
            | "mq_timedreceive"
            | "mq_timedreceive_time64"
            | "io_getevents"
            | "io_pgetevents"
            | "io_pgetevents_time64"
            | "flock",
        ) => WAITS,
        _ => 0,
    }
}

/// One record from the ring, decoded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Record {
    /// A call, at its exit, or as its thread ended inside it.
    Call(SyscallEvent),

    /// The arguments a call read at its entry; its exit follows.
    Entry(SyscallEvent),
    Event(TraceEvent),

    /// A captured exec entry whose thread took the leader's id.
    Handoff {
        former: Tid,
        thread: Tid,
        process: Pid,
        entered: u64,
    },

    /// The stack of the thread whose call or signal follows, or of a
    /// waiting call that began then.
    Stack(SampledStack),

    /// The Python frames of the thread whose stack follows, as the
    /// walker wrote them, to read when the stack is unwound.
    Python {
        thread: Tid,
        record: Vec<u8>,
    },
}

impl Record {
    /// When it happened, by which the rings are merged; none for a record
    /// that goes with the next one its program sent.
    fn time(&self) -> Option<u64> {
        match self {
            Self::Call(call) | Self::Entry(call) => Some(call.time()),
            Self::Stack(stack) => stack.entered,
            Self::Event(TraceEvent::Descriptor { .. })
            | Self::Handoff { .. }
            | Self::Python { .. } => None,
            Self::Event(event) => Some(event.time()),
        }
    }
}

/// A thread's stack as the kernel saw it, to unwind.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SampledStack {
    pub thread: Tid,
    pub process: Pid,

    /// When the waiting call it was taken at began; none for a stack
    /// taken at the call or signal its record goes before.
    pub entered: Option<u64>,

    /// Whether the thread's interpreted frames went just before it.
    pub interpreted: bool,

    /// The user registers by DWARF number.
    pub registers: [(DwarfRegister, u64); 17],

    /// The kernel's walk of the frame pointers, the instruction first.
    pub chain: Vec<u64>,

    /// The stack's bytes from its pointer up, if copied.
    pub copy: Vec<u8>,
}

/// Why a record could not be decoded.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DecodeError {
    #[error("a record of {0} bytes is shorter than its header says")]
    Short(usize),

    #[error("unknown record kind {0}")]
    Kind(u32),

    #[error("a call record names ABI {0}")]
    Abi(u8),

    #[error("a capture of argument {0} is malformed")]
    Capture(u8),
}

fn read<T: FromBytes + KnownLayout + Immutable>(bytes: &[u8]) -> Result<T, DecodeError> {
    T::read_from_prefix(bytes)
        .map(|(value, _)| value)
        .map_err(|_| DecodeError::Short(bytes.len()))
}

/// How a process ended, from the exit code `wait(2)` encodes.
#[must_use]
pub const fn process_end(code: i32) -> ProcessEnd {
    let signal = code & 0x7f;
    if signal == 0 {
        ProcessEnd::Exited((code >> 8) & 0xff)
    } else {
        ProcessEnd::Killed {
            signal,
            core: code & 0x80 != 0,
        }
    }
}

/// Decodes one record as the programs wrote it.
///
/// # Errors
///
/// A record that is shorter than its header says, of an unknown kind, or
/// whose captures overrun it.
pub fn decode(bytes: &[u8]) -> Result<Record, DecodeError> {
    let header: RawRecord = read(bytes)?;
    let size = usize::try_from(header.size).unwrap_or(usize::MAX);
    let bytes = bytes.get(..size).ok_or(DecodeError::Short(bytes.len()))?;
    match header.kind {
        RECORD_CALL | RECORD_ENTRY => call_record(header, bytes),
        RECORD_SPAWN => {
            let raw: RawSpawnRecord = read(bytes)?;
            Ok(Record::Event(TraceEvent::Spawn {
                parent_thread: Tid::new(header.tid),
                parent: Pid::new(header.tgid),
                child: Tid::new(raw.child),
                thread: raw.child_tgid == header.tgid,
                time: raw.time,
            }))
        }
        RECORD_EXEC => {
            let raw: RawExecRecord = read(bytes)?;
            let start = size_of::<RawExecRecord>();
            let path = bytes
                .get(start..start + raw.len as usize)
                .ok_or(DecodeError::Short(bytes.len()))?;
            Ok(Record::Event(TraceEvent::Exec {
                thread: Tid::new(header.tid),
                process: Pid::new(header.tgid),
                time: raw.time,
                path: String::from_utf8_lossy(path).into(),
            }))
        }
        RECORD_HANDOFF => {
            let raw: RawHandoffRecord = read(bytes)?;
            Ok(Record::Handoff {
                former: Tid::new(raw.former),
                thread: Tid::new(header.tid),
                process: Pid::new(header.tgid),
                entered: raw.entered,
            })
        }
        RECORD_EXIT => {
            let raw: RawExitRecord = read(bytes)?;
            Ok(Record::Event(if raw.last == 0 {
                TraceEvent::ThreadExit {
                    thread: Tid::new(header.tid),
                    process: Pid::new(header.tgid),
                    time: raw.time,
                    end: process_end(raw.code),
                }
            } else {
                TraceEvent::Exit {
                    process: Pid::new(header.tgid),
                    time: raw.time,
                    end: process_end(raw.code),
                }
            }))
        }
        RECORD_SIGNAL => {
            let raw: RawSignalRecord = read(bytes)?;
            let word = |at: usize| {
                raw.info
                    .get(at..at + 4)
                    .and_then(|bytes| bytes.try_into().ok())
                    .map_or(0, i32::from_le_bytes)
            };
            let code = (raw.flags & SIGNAL_INFO_LOST == 0).then(|| word(8));

            // Processes send with SI_USER, SI_QUEUE, SI_TKILL, and the like.
            let sender = code
                .is_some_and(|code| code <= 0 && code != -2)
                .then(|| u32::try_from(word(16)).ok().map(Pid::new))
                .flatten();

            // The rest of a siginfo holds no field of any kind of signal.
            let info = code.map(|_| {
                let mut info = [0_u8; 128];
                for (to, from) in info.iter_mut().zip(raw.info) {
                    *to = from;
                }
                Arc::from(&info[..])
            });
            Ok(Record::Event(TraceEvent::Signal {
                thread: Tid::new(header.tid),
                process: Pid::new(header.tgid),
                time: raw.time,
                signal: raw.signal,
                code,
                sender,
                info,
                stack: (raw.flags & u32::from(CALL_STACK_LOST) != 0).then(lost_stack),
            }))
        }
        RECORD_STACK => stack_record(header, bytes),
        RECORD_PY_STACK | RECORD_V8_STACK => Ok(Record::Python {
            thread: Tid::new(header.tid),
            record: bytes.to_vec(),
        }),
        RECORD_FILE => {
            let raw: RawFileRecord = read(bytes)?;
            let start = size_of::<RawFileRecord>();
            let name = bytes
                .get(start..start + raw.len as usize)
                .ok_or(DecodeError::Short(bytes.len()))?;
            Ok(Record::Event(descriptor(Pid::new(header.tgid), &raw, name)))
        }
        kind => Err(DecodeError::Kind(kind)),
    }
}

/// A stack record and the walk and copy that follow it.
fn stack_record(header: RawRecord, bytes: &[u8]) -> Result<Record, DecodeError> {
    let raw: RawStackRecord = read(bytes)?;
    let start = size_of::<RawStackRecord>();
    let chain_end = start + raw.frames as usize * 8;
    let chain = bytes
        .get(start..chain_end)
        .ok_or(DecodeError::Short(bytes.len()))?;
    let copy = bytes
        .get(chain_end..chain_end + raw.copied as usize)
        .ok_or(DecodeError::Short(bytes.len()))?;
    let mut number = 0;
    let registers = raw.regs.map(|value| {
        let register = (DwarfRegister::new(number), value);
        number += 1;
        register
    });
    Ok(Record::Stack(SampledStack {
        thread: Tid::new(header.tid),
        process: Pid::new(header.tgid),
        entered: (raw.entered != 0).then_some(raw.entered),
        interpreted: raw.interpreted != 0,
        registers,
        chain: chain
            .as_chunks::<8>()
            .0
            .iter()
            .map(|word| u64::from_le_bytes(*word))
            .collect(),
        copy: copy.to_vec(),
    }))
}

/// A call record and the captures that follow it.
fn call_record(header: RawRecord, bytes: &[u8]) -> Result<Record, DecodeError> {
    let raw: RawCallRecord = read(bytes)?;
    let abi = abi_of(raw.abi).ok_or(DecodeError::Abi(raw.abi))?;
    let mut captured = Vec::with_capacity(usize::from(raw.captures));
    let mut at = size_of::<RawCallRecord>();
    for _ in 0..raw.captures {
        let capture: RawCaptureHeader =
            read(bytes.get(at..).ok_or(DecodeError::Short(bytes.len()))?)?;
        let start = at + size_of::<RawCaptureHeader>();
        let len = capture.len as usize;
        let data = bytes
            .get(start..start + len)
            .ok_or(DecodeError::Capture(capture.arg))?;
        captured.push(Capture {
            arg: capture.arg,
            when: if capture.when == 0 {
                ReadAt::Entry
            } else {
                ReadAt::Exit
            },
            state: match capture.state {
                0 => CaptureState::Complete,
                1 => CaptureState::Truncated {
                    wanted: capture.wanted,
                },
                2 => CaptureState::Unreadable,
                _ => return Err(DecodeError::Capture(capture.arg)),
            },
            bytes: Arc::from(data),
        });
        at = start + len.next_multiple_of(8);
    }

    let unseen = raw.flags & EventFlags::ENTRY_UNSEEN.0 != 0;
    let exited_inside = raw.flags & CALL_PROCESS_EXITED != 0;
    let call = SyscallId::new(abi, raw.nr);

    // Only these end a thread inside them. Any other call returns first,
    // so its return went unseen: a function's exit programs are skipped
    // for calls in progress whenever another tracer attaches to it.
    let ends = call
        .name()
        .is_some_and(|name| matches!(name, "exit" | "exit_group" | "execve" | "execveat"));
    if exited_inside && !ends {
        record!("syscalls bpf: {call:?} on {} never returned", header.tid);
        return Ok(Record::Event(TraceEvent::Lost {
            time: monotonic(),
            count: 1,
            why: Loss::Unseen,
        }));
    }

    let event = SyscallEvent {
        thread: Tid::new(header.tid),
        process: Pid::new(header.tgid),
        call,
        args: raw.args,
        captured,
        outcome: if header.kind == RECORD_ENTRY {
            Outcome::Unfinished
        } else if exited_inside {
            Outcome::ProcessExited
        } else {
            Outcome::of_return(raw.ret)
        },
        entered: (!unseen).then_some(raw.entered),
        exited: (header.kind == RECORD_CALL && !exited_inside).then_some(raw.exited),
        stack: (raw.flags & CALL_STACK_LOST != 0).then(lost_stack),
        flags: EventFlags(raw.flags & CALL_EVENT_FLAGS),
    };

    Ok(if header.kind == RECORD_ENTRY {
        Record::Entry(event)
    } else {
        Record::Call(event)
    })
}

/// What a descriptor a call made refers to, as `/proc/PID/fd/N` would
/// name it. The kernel gives no path, so a file's text is left empty and
/// the descriptor table keeps the path the call named.
fn descriptor(process: Pid, raw: &RawFileRecord, name: &[u8]) -> TraceEvent {
    let name = String::from_utf8_lossy(name);
    let text = match raw.magic {
        PIPEFS_MAGIC => format!("pipe:[{}]", raw.ino),
        SOCKFS_MAGIC => format!("socket:[{}]", raw.ino),
        ANON_INODE_FS_MAGIC => format!("anon_inode:{name}"),
        _ => String::new(),
    };

    let kind = raw.mode & S_IFMT;
    let device = (kind == S_IFCHR || kind == S_IFBLK).then_some(Device {
        block: kind == S_IFBLK,
        major: raw.rdev >> 20,
        minor: raw.rdev & 0xf_ffff,
    });
    TraceEvent::Descriptor {
        process,
        fd: i64::from(raw.fd),
        time: 0,
        text: text.into(),
        cloexec: raw.cloexec != 0,
        device,
        made: true,
    }
}

/// Folds the kernel's statistics, each a thread's by its tgid and tid,
/// and with both zero, a CPU's of the threads it had no room for, which
/// `Folded` names by `process`.
#[must_use]
pub fn fold(
    process: Pid,
    stats: impl IntoIterator<Item = (u32, u32, SyscallId, RawStatValue)>,
    errnos: impl IntoIterator<Item = (RawErrnoKey, u64)>,
) -> Folded {
    let key = |tgid: u32, tid: u32, call| {
        if tid == 0 {
            (process, Tid::new(0), call)
        } else {
            (Pid::new(tgid), Tid::new(tid), call)
        }
    };

    let mut folded = Folded::default();
    for (tgid, tid, call, value) in stats {
        folded
            .by_call
            .entry(key(tgid, tid, call))
            .or_default()
            .merge(&SyscallStats {
                calls: value.calls,
                errors: value.errors,
                errors_by_errno: BTreeMap::new(),
                timed: value.timed,
                total_ns: value.total_ns,
                min_ns: value.min_ns,
                max_ns: value.max_ns,
                log2_ns: value.log2_ns.to_vec(),
                bytes: value.bytes,
            });
    }

    for (which, count) in errnos {
        let (Some(call), Ok(errno)) = (call_of(which.slot), u16::try_from(which.err)) else {
            continue;
        };

        if count > 0 {
            *folded
                .by_call
                .entry(key(which.tgid, which.tid, call))
                .or_default()
                .errors_by_errno
                .entry(errno)
                .or_default() += count;
        }
    }

    folded
}

/// Why the BPF engine cannot trace here, each with what to do about it.
#[derive(Debug, thiserror::Error)]
pub enum BpfError {
    #[error("{0}")]
    Unavailable(String),

    #[error("process {0} does not exist")]
    NoProcess(u32),

    #[error("loading the BPF programs failed: {0}")]
    Load(String),

    #[error("tracing failed: {0}")]
    System(#[from] nix::errno::Errno),

    #[error("following what the program maps, for its stacks, failed: {0}")]
    Mappings(super::super::perf::OpenError),
}

/// What to do if the engine is missing a privilege.
const REMEDY: &str =
    "  microscope uses eBPF so it never stops or slows the traced program, and it will
  not fall back to ptrace on its own. To continue, one of:
    - run it from a unit with AmbientCapabilities=CAP_BPF CAP_PERFMON
    - on NixOS or system-manager, import microscope's bpf module and run through
      /run/wrappers/bin/microscope-bpf (docs/profiling.md#give-microscope-the-bpf-privileges)
    - record a program you launch with --syscall-engine ptrace, which stops the
      program at each chosen call";

/// The one thing the engine lacks to run here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Missing {
    /// A kernel older than 5.15, by its release.
    Kernel(String),
    Btf,

    /// `CAP_BPF`, `CAP_PERFMON`, or both, as the end of a sentence.
    Privileges(&'static str),
    UserNamespace,
    Lockdown,
}

impl std::fmt::Display for Missing {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Kernel(release) => {
                write!(f, "Linux 5.15 or newer is needed, and this is {release}")
            }
            Self::Btf => write!(f, "the kernel's BTF, /sys/kernel/btf/vmlinux, is missing"),
            Self::Privileges(missing) => write!(f, "this process {missing}"),
            Self::UserNamespace => write!(
                f,
                "this process runs in a user namespace, whose capabilities do not reach the \
                 kernel's BPF"
            ),
            Self::Lockdown => write!(f, "the kernel is locked down for confidentiality"),
        }
    }
}

/// Where a BPF filesystem mounted with `delegate_cmds` is, as a container
/// manager mounts one to hand out tokens.
fn delegating_bpffs() -> Option<std::path::PathBuf> {
    let mounts = std::fs::read_to_string("/proc/self/mountinfo").ok()?;
    mounts.lines().find_map(|line| {
        // The mount point is the fifth field; after " - " come the
        // filesystem type, the source, and the superblock's options.
        let (mount, rest) = line.split_once(" - ")?;
        let mut kind = rest.split(' ');
        let delegates = kind.next() == Some("bpf")
            && kind
                .nth(1)
                .is_some_and(|options| options.contains("delegate_cmds"));

        delegates.then(|| mount.split(' ').nth(4).map(std::path::PathBuf::from))?
    })
}

const CAP_SYS_ADMIN: u32 = 21;
const CAP_PERFMON: u32 = 38;
const CAP_BPF: u32 = 39;

/// What the running kernel and this process offer the engine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Probe {
    /// The kernel's release, as `uname -r` prints it.
    pub release: String,

    /// Its major and minor version.
    pub version: Option<(u32, u32)>,
    pub btf: bool,

    /// The effective capability set.
    pub capabilities: u64,

    /// The lockdown mode in force, when the kernel has one.
    pub lockdown: Option<String>,

    /// A BPF filesystem that delegates commands, which offers a token.
    pub token: Option<std::path::PathBuf>,

    /// Whether the process runs in a user namespace other than the
    /// host's, where its capabilities do not reach the kernel's BPF.
    pub user_namespace: bool,
}

impl Probe {
    /// Reads what the system offers.
    #[must_use]
    pub fn read() -> Self {
        let release = nix::sys::utsname::uname()
            .map(|name| name.release().to_string_lossy().into_owned())
            .unwrap_or_default();
        let capabilities = super::super::proc_status::read_path("/proc/self/status")
            .ok()
            .and_then(|status| super::super::proc_status::hex_field(&status, b"CapEff:"))
            .unwrap_or(0);
        let lockdown = std::fs::read_to_string("/sys/kernel/security/lockdown")
            .ok()
            .and_then(|modes| {
                let start = modes.find('[')?;
                let end = modes[start..].find(']')?;
                Some(modes[start + 1..start + end].to_owned())
            });

        Self {
            version: version(&release),
            release,
            btf: std::path::Path::new("/sys/kernel/btf/vmlinux").exists(),
            capabilities,
            lockdown,
            token: delegating_bpffs(),
            user_namespace: std::fs::read_to_string("/proc/self/uid_map").is_ok_and(|map| {
                map.split_whitespace().collect::<Vec<_>>() != ["0", "0", "4294967295"]
            }),
        }
    }

    const fn has(&self, capability: u32) -> bool {
        self.capabilities & (1 << capability) != 0
    }

    /// Which of `CAP_BPF` and `CAP_PERFMON`, which every BPF engine needs,
    /// this process lacks, as the end of a sentence about it: `None` when
    /// it has both.
    #[must_use]
    pub const fn lacks_privileges(&self) -> Option<&'static str> {
        let bpf = self.has(CAP_BPF) || self.has(CAP_SYS_ADMIN);
        let perfmon = self.has(CAP_PERFMON) || self.has(CAP_SYS_ADMIN);
        match (bpf, perfmon) {
            (true, true) => None,
            (false, false) => Some("has neither CAP_BPF nor CAP_PERFMON"),
            (false, true) => Some("lacks CAP_BPF"),
            (true, false) => Some("lacks CAP_PERFMON"),
        }
    }

    /// The first thing the engine lacks, in the order a reader fixes
    /// them, if any.
    #[must_use]
    pub fn missing(&self) -> Option<Missing> {
        if self.version.is_none_or(|version| version < (5, 15)) {
            return Some(Missing::Kernel(self.release.clone()));
        }

        if !self.btf {
            return Some(Missing::Btf);
        }

        if let Some(missing) = self.lacks_privileges() {
            return Some(Missing::Privileges(missing));
        }

        if self.user_namespace {
            return Some(Missing::UserNamespace);
        }

        (self.lockdown.as_deref() == Some("confidentiality")).then_some(Missing::Lockdown)
    }

    /// Whether the engine can run, or the one specific reason it cannot,
    /// with the fix.
    ///
    /// # Errors
    ///
    /// The first missing piece, in the order a reader fixes them.
    pub fn check(&self) -> Result<(), BpfError> {
        let Some(missing) = self.missing() else {
            return Ok(());
        };

        let fix = match &missing {
            Missing::Kernel(_) => {
                "\n  Record a program you launch with --syscall-engine ptrace instead.".to_owned()
            }
            Missing::Btf => {
                "\n  Build the kernel with CONFIG_DEBUG_INFO_BTF=y, or record a program \
                             you launch with --syscall-engine ptrace."
                    .to_owned()
            }
            Missing::Privileges(_) | Missing::UserNamespace => {
                let token = self.token.as_ref().map_or_else(String::new, |path| {
                    format!(
                        "\n  {} offers a BPF token, which microscope cannot use: aya, which loads \
                         its programs, has no way to pass one to the kernel.",
                        path.display()
                    )
                });
                format!("\n{REMEDY}{token}")
            }
            Missing::Lockdown => "\n  BPF programs may not read kernel memory. Boot with \
                                  lockdown=integrity or none, or record a program you launch \
                                  with --syscall-engine ptrace."
                .to_owned(),
        };

        Err(BpfError::Unavailable(format!(
            "system calls need the BPF engine, and {missing}.{fix}"
        )))
    }
}

fn version(release: &str) -> Option<(u32, u32)> {
    let mut parts = release.split(|c: char| !c.is_ascii_digit());
    Some((parts.next()?.parse().ok()?, parts.next()?.parse().ok()?))
}

/// How the BPF engine runs, beyond what to trace.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct BpfOptions {
    /// Count in the kernel and report no calls, only their statistics.
    pub count_only: bool,

    /// With stacks, copy each stack's top to unwind by call-frame
    /// information, rather than trust the kernel's walk alone.
    pub stack_copy: bool,

    /// While counting, the failed and slow calls to send whole.
    pub exceptions: Option<Exceptions>,
}

/// The calls counting sends whole: each that fails or takes `slow`, up to
/// `per_second` a second over every CPU.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Exceptions {
    pub slow: std::time::Duration,
    pub per_second: u32,
}

impl RawConfig {
    /// The programs' settings for a trace of `options` run as `engine`
    /// says, of `rows` calls' statistics.
    fn new(options: &TraceOptions, engine: BpfOptions, launch: bool, rows: u32) -> Self {
        // Counting is always on: a call a full ring has no room for is
        // counted instead.
        let mut flags = if engine.count_only {
            CONFIG_COUNT
        } else {
            CONFIG_COUNT | CONFIG_STREAM
        };

        if options.follow {
            flags |= CONFIG_FOLLOW;
        }

        if launch {
            flags |= CONFIG_SKIP_UNSEEN;
        }

        if options.stacks.is_some() {
            flags |= CONFIG_STACKS;
            if engine.stack_copy {
                flags |= CONFIG_STACK_COPY;
            }
        }

        let (slow_ns, cap, burst) = engine.exceptions.map_or((0, 0, 0), |exceptions| {
            flags |= CONFIG_EXCEPTIONS;
            let cpus = u32::try_from(aya::util::nr_cpus().unwrap_or(1)).unwrap_or(1);
            let cap = (exceptions.per_second / cpus).max(1);
            (
                u64::try_from(exceptions.slow.as_nanos()).unwrap_or(u64::MAX),
                cap,
                cap,
            )
        });

        Self {
            flags,
            elements: u32::try_from(options.limits.elements).unwrap_or(u32::MAX),
            sample: 0,
            rows,
            slow_ns,
            cap,
            burst,
        }
    }
}

/// The most kernel functions the programs attach to: their slots.
const FUNCTION_SLOTS: usize = 32;

/// Where the programs attached, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Attachment {
    /// The chosen calls' kernel functions; none for the tracepoints.
    pub functions: Vec<String>,
    pub reason: String,
}

impl Attachment {
    /// The program type, as the kernel names it.
    #[must_use]
    pub const fn name(&self) -> &'static str {
        if self.functions.is_empty() {
            "tp_btf"
        } else {
            "fentry"
        }
    }
}

/// The kernel function a call enters through, when `has` finds it.
fn function_of(call: SyscallId, has: &impl Fn(&str) -> bool) -> Option<String> {
    call.kernel_function().filter(|function| has(function))
}

/// Where the programs attach to see `chosen`, every call when `all`: the
/// calls' functions when they can, else the tracepoints every call on the
/// host passes. `has` says which kernel functions BTF describes. Without
/// `x32` in the kernel, its calls fail before any function is entered;
/// it is asked only when that matters.
fn attachment(
    all: bool,
    chosen: &[SyscallId],
    x32: impl FnOnce() -> bool,
    has: &impl Fn(&str) -> bool,
) -> Attachment {
    let tracepoints = |reason: String| Attachment {
        functions: Vec::new(),
        reason,
    };

    let why = if all {
        "every call is traced".to_owned()
    } else {
        let x32 = x32();
        let mut functions: Vec<String> = Vec::new();
        let mut missing = None;
        for &call in chosen.iter().filter(|call| x32 || call.abi != Abi::X32) {
            match function_of(call, has) {
                Some(function) if !functions.contains(&function) => functions.push(function),
                Some(_) => {}
                None => {
                    missing.get_or_insert(call);
                }
            }
        }

        match missing {
            Some(call) => format!(
                "{} has no kernel function that BTF describes",
                call.display_name()
            ),
            None if functions.len() > FUNCTION_SLOTS => format!(
                "the calls enter through {} kernel functions, more than {FUNCTION_SLOTS}",
                functions.len()
            ),
            None => {
                return Attachment {
                    reason: format!(
                        "{} calls enter through {} kernel functions",
                        chosen.len(),
                        functions.len()
                    ),
                    functions,
                };
            }
        }
    };

    tracepoints(why)
}

/// Whether the kernel runs x32 calls, by the dispatcher it then has: a
/// function since 6.9, which BTF names, or a table before, which only
/// `/proc/kallsyms` does, and which takes over 100 ms to read.
fn has_x32(has: &impl Fn(&str) -> bool) -> bool {
    if has("x64_sys_call") {
        return has("x32_sys_call");
    }
    std::fs::read_to_string("/proc/kallsyms").is_ok_and(|symbols| {
        symbols.lines().any(|line| {
            line.rsplit_once(' ')
                .is_some_and(|(_, name)| name == "x32_sys_call" || name == "x32_sys_call_table")
        })
    })
}

fn load_error(error: impl std::fmt::Display) -> BpfError {
    BpfError::Load(error.to_string())
}

/// Loads the programs, with `rows` rows of statistics.
fn load_object(btf: &Btf, rows: u32) -> Result<Ebpf, BpfError> {
    let cpus = u32::try_from(aya::util::nr_cpus().map_err(|(_, error)| load_error(error))?)
        .unwrap_or(u32::MAX);
    EbpfLoader::new()
        .btf(Some(btf))
        .map_max_entries("EVENTS", cpus)
        // Two halves, one counted into while the other is read.
        .map_max_entries("STATS", rows * 2)
        .override_global(
            "loop_helper",
            &crate::backend::linux::bpf::loop_helper(),
            true,
        )
        // aya cannot open task storage itself, which only the
        // programs use.
        .allow_unsupported_maps()
        .load(OBJECT)
        .map_err(load_error)
}

/// A ring for each CPU, set in EVENTS for the programs: [`DEFAULT_RING`]
/// bytes each, or [`STACK_COPY_RING`] with stack copies, halved while
/// microscope's memory cgroup has no room for them.
fn rings(ebpf: &mut Ebpf, engine: BpfOptions) -> Result<Vec<RingBuf<MapData>>, BpfError> {
    let mut events: ArrayOfMaps<&mut MapData, MapData> =
        ArrayOfMaps::try_from(ebpf.map_mut("EVENTS").ok_or_else(|| missing("EVENTS"))?)
            .map_err(load_error)?;
    let cpus = aya::util::nr_cpus().map_err(|(_, error)| load_error(error))?;
    let mut bytes = if engine.stack_copy {
        STACK_COPY_RING
    } else {
        DEFAULT_RING
    };

    'sizes: loop {
        let mut rings = Vec::with_capacity(cpus);
        for cpu in (0_u32..).take(cpus) {
            let ring = aya_obj::Map::new_from_params(
                aya_obj::generated::bpf_map_type::BPF_MAP_TYPE_RINGBUF as u32,
                0,
                0,
                bytes,
                0,
            );
            let ring = match MapData::create(ring, &format!("EVENTS_{cpu}"), None) {
                Ok(ring) => ring,
                Err(aya::maps::MapError::CreateError { io_error, .. })
                    if bytes > SMALLEST_RING
                        && io_error.raw_os_error() == Some(nix::libc::ENOMEM) =>
                {
                    record!("syscalls bpf: no memory for rings of {bytes} bytes");
                    bytes /= 2;
                    continue 'sizes;
                }
                Err(error) => return Err(load_error(error)),
            };

            events.set(cpu, &ring, 0).map_err(load_error)?;
            rings.push(RingBuf::try_from(aya::maps::Map::RingBuf(ring)).map_err(load_error)?);
        }

        return Ok(rings);
    }
}

/// The loaded and attached programs, and their maps.
struct Loaded {
    ebpf: Ebpf,
    targets: HashMap<MapData, u32, u8>,

    /// Each CPU's ring of records, by the CPU's number.
    rings: Vec<RingBuf<MapData>>,

    /// The call each row of statistics counts.
    rows: Vec<SyscallId>,

    /// What waits out the programs running, and what it is set to.
    fence: (
        ArrayOfMaps<MapData, Array<MapData, u32>>,
        Array<MapData, u32>,
    ),

    /// The half of the statistics being counted into.
    generation: u32,
    attachment: Attachment,

    /// The programs' settings and the chosen calls' specs as loaded, and
    /// what has been shed of them since.
    base: RawConfig,
    specs: Vec<(u32, RawCallSpec)>,
    shedding: Shed,

    /// The calls seen return as last counted, and what they and those
    /// before them cost.
    runs: u64,
    estimated: u64,
}

impl Loaded {
    fn load(options: &TraceOptions, engine: BpfOptions, launch: bool) -> Result<Self, BpfError> {
        let _span = crate::span!("syscalls bpf load");
        // Every slot when every call is chosen, numbers no table names
        // among them. Each chosen call has a row of statistics.
        let all = options.calls.is_all();
        let chosen: Vec<(u32, SyscallId)> = (0..SLOTS)
            .filter_map(|slot| Some((slot, call_of(slot)?)))
            .filter(|&(_, call)| all || options.calls.contains(call))
            .collect();
        let rows = u32::try_from(chosen.len().max(1)).unwrap_or(u32::MAX);
        let btf = Btf::from_sys_fs().map_err(load_error)?;
        let calls: Vec<SyscallId> = chosen.iter().map(|&(_, call)| call).collect();
        let has = |name: &str| btf.id_by_type_name_kind(name, BtfKind::Func).is_ok();
        if !has("do_notify_parent") {
            return Err(BpfError::Unavailable(
                "the kernel's BTF has no do_notify_parent hook for complete process exits".into(),
            ));
        }
        let attachment = attachment(all, &calls, || has_x32(&has), &has);
        let terminal: BTreeSet<_> = calls
            .iter()
            .filter(|call| matches!(call.name(), Some("exit" | "exit_group")))
            .filter_map(|&call| call.kernel_function())
            .filter(|function| !function.starts_with("__ia32_") || has(function))
            .collect();
        record!(
            "syscalls bpf: attach {} ({}): {:?}",
            attachment.name(),
            attachment.reason,
            attachment.functions
        );

        let mut ebpf = load_object(&btf, rows)?;
        let rings = rings(&mut ebpf, engine)?;
        let mut specs = Vec::with_capacity(chosen.len());
        let base;
        {
            let mut calls: Array<_, Bytes<SPEC_BYTES>> =
                Array::try_from(ebpf.map_mut("CALLS").ok_or_else(|| missing("CALLS"))?)
                    .map_err(load_error)?;
            for (row, &(slot, call)) in chosen.iter().enumerate() {
                let mut spec = call_spec(call, options);
                spec.stat = u16::try_from(row).unwrap_or(u16::MAX);
                specs.push((slot, spec));
                let mut value = [0; SPEC_BYTES];
                value.copy_from_slice(spec.as_bytes());
                calls.set(slot, value, 0).map_err(load_error)?;
            }
        }
        {
            let mut config: Array<_, Bytes<CONFIG_BYTES>> =
                Array::try_from(ebpf.map_mut("CONFIG").ok_or_else(|| missing("CONFIG"))?)
                    .map_err(load_error)?;
            let raw = RawConfig::new(options, engine, launch, rows);
            base = raw;
            let mut value = [0; CONFIG_BYTES];
            value.copy_from_slice(raw.as_bytes());
            config.set(0, value, 0).map_err(load_error)?;
        }

        {
            let _span = crate::span!("syscalls bpf attach");
            attach(&mut ebpf, &btf, &attachment, &terminal)?;
        }

        let targets =
            HashMap::try_from(ebpf.take_map("TARGETS").ok_or_else(|| missing("TARGETS"))?)
                .map_err(load_error)?;
        let fence = (
            ArrayOfMaps::try_from(ebpf.take_map("FENCE").ok_or_else(|| missing("FENCE"))?)
                .map_err(load_error)?,
            Array::try_from(
                ebpf.take_map("FENCE_TOKEN")
                    .ok_or_else(|| missing("FENCE_TOKEN"))?,
            )
            .map_err(load_error)?,
        );
        record!("syscalls bpf: loaded and attached");

        Ok(Self {
            ebpf,
            targets,
            rings,
            fence,
            generation: 0,
            rows: calls,

            attachment,
            base,
            specs,
            shedding: Shed::default(),
            runs: 0,
            estimated: 0,
        })
    }

    /// Reads the programs' settings, changes them, and writes them back.
    fn configure(&mut self, change: impl FnOnce(&mut RawConfig)) -> Result<(), BpfError> {
        let mut config: Array<_, Bytes<CONFIG_BYTES>> = Array::try_from(
            self.ebpf
                .map_mut("CONFIG")
                .ok_or_else(|| missing("CONFIG"))?,
        )
        .map_err(load_error)?;
        let mut raw = RawConfig::read_from_bytes(&config.get(&0, 0).map_err(load_error)?)
            .map_err(|_| BpfError::Load("the CONFIG map's value has another size".into()))?;
        change(&mut raw);
        let mut value = [0; CONFIG_BYTES];
        value.copy_from_slice(raw.as_bytes());
        config.set(0, value, 0).map_err(load_error)
    }

    /// Gives up what `shed` names, and takes back what it does not, from
    /// the next call on.
    fn shed(&mut self, shed: Shed) -> Result<(), BpfError> {
        record!("syscalls bpf: shed {shed:?}");
        let base = self.base;
        self.configure(|config| {
            let mut flags = base.flags | (config.flags & CONFIG_GENERATION);
            if shed.has(Shed::STACKS) {
                flags &= !(CONFIG_STACKS | CONFIG_STACK_COPY);
            }

            if shed.has(Shed::STREAM) {
                flags &= !CONFIG_STREAM;
            }

            *config = RawConfig {
                flags,
                sample: if shed.has(Shed::SAMPLED) {
                    SAMPLED_EVERY
                } else {
                    0
                },
                elements: if shed.has(Shed::STRINGS) {
                    base.elements.min(CAPPED_ELEMENTS)
                } else {
                    base.elements
                },
                ..base
            };
        })?;

        if shed.has(Shed::STRINGS) != self.shedding.has(Shed::STRINGS) {
            let mut calls: Array<_, Bytes<SPEC_BYTES>> =
                Array::try_from(self.ebpf.map_mut("CALLS").ok_or_else(|| missing("CALLS"))?)
                    .map_err(load_error)?;
            for &(slot, base) in &self.specs {
                let mut spec = base;
                if shed.has(Shed::STRINGS) {
                    // Paths stay whole for the descriptors they name, and
                    // structures for their fields: only String, Strings,
                    // Bytes, and Iovec, as `call_spec` numbers them.
                    for arg in &mut spec.args {
                        if matches!(arg.kind, 2 | 3 | 5 | 6) {
                            arg.limit = arg.limit.min(CAPPED_BYTES);
                        }
                    }
                }

                let mut value = [0; SPEC_BYTES];
                value.copy_from_slice(spec.as_bytes());
                calls.set(slot, value, 0).map_err(load_error)?;
            }
        }

        self.shedding = shed;
        Ok(())
    }

    /// The calls the programs have seen return, over every CPU.
    fn runs(&self) -> u64 {
        self.ebpf
            .map("RUNS")
            .and_then(|map| PerCpuArray::<_, u64>::try_from(map).ok())
            .and_then(|runs| runs.get(&0, 0).ok())
            .map_or(0, |values| values.iter().sum())
    }

    /// Nanoseconds the programs have cost the traced threads: as the
    /// kernel times them, or else the calls they saw return since the last
    /// estimate, `runs` in all, each at what one costs as they now run.
    fn kernel_cost(&mut self, runs: u64) -> u64 {
        if let Some(time) = self.run_time() {
            return time;
        }

        self.estimated += runs.saturating_sub(self.runs) * per_call(self.base.flags, self.shedding);
        self.runs = runs;
        self.estimated
    }

    /// Nanoseconds the kernel has spent running the programs, when it
    /// times them (`kernel.bpf_stats_enabled`).
    fn run_time(&self) -> Option<u64> {
        let enabled = std::fs::read_to_string("/proc/sys/kernel/bpf_stats_enabled")
            .is_ok_and(|text| text.trim() == "1");
        enabled.then(|| self.fdinfo_sum("run_time_ns:"))
    }

    fn target(&mut self, process: u32) -> Result<(), BpfError> {
        record!("syscalls bpf: trace {process}");
        self.targets.insert(process, 1, 0).map_err(load_error)
    }

    /// Calls threads have been in for at least `after` nanoseconds, which
    /// the `in_flight` iterator lists from each thread's own storage.
    fn in_flight(&mut self, after: u64) -> Vec<InFlight> {
        let listed = match self.list_in_flight() {
            Ok(listed) => listed,
            // Only the flight recorder, which release builds leave out,
            // reads the error.
            #[cfg_attr(not(debug_assertions), expect(unused_variables))]
            Err(error) => {
                record!("syscalls bpf: the in-flight scan failed: {error}");
                Vec::new()
            }
        };

        let now = monotonic();
        let mut calls: Vec<InFlight> = listed
            .as_chunks::<{ size_of::<RawInFlightRecord>() }>()
            .0
            .iter()
            .filter_map(|bytes| {
                let record = RawInFlightRecord::read_from_bytes(bytes).ok()?;
                let entry = record.entry;
                (now.saturating_sub(entry.entered) >= after).then_some(InFlight {
                    thread: Tid::new(record.tid),
                    process: Pid::new(entry.tgid),
                    call: SyscallId::new(abi_of(entry.abi)?, entry.nr),
                    args: entry.args,
                    entered: entry.entered,
                })
            })
            .collect();
        calls.sort_by_key(|call| (call.entered, call.thread));

        calls
    }

    /// Runs the `in_flight` iterator once: the records it wrote.
    fn list_in_flight(&mut self) -> Result<Vec<u8>, BpfError> {
        use std::io::Read as _;
        let program: &mut Iter = self
            .ebpf
            .program_mut("in_flight")
            .ok_or_else(|| missing("in_flight"))?
            .try_into()
            .map_err(load_error)?;
        let link = program.attach().map_err(load_error)?;
        let link = program.take_link(link).map_err(load_error)?;
        let mut file = link.into_file().map_err(load_error)?;
        let mut listed = Vec::new();
        file.read_to_end(&mut listed).map_err(load_error)?;

        Ok(listed)
    }

    /// Calls in progress as the trace ends, which never returned while it
    /// ran.
    fn unfinished(&mut self) -> Vec<SyscallEvent> {
        self.in_flight(0)
            .into_iter()
            .map(|call| SyscallEvent {
                thread: call.thread,
                process: call.process,
                call: call.call,
                args: call.args,
                captured: Vec::new(),
                outcome: Outcome::Unfinished,
                entered: Some(call.entered),
                exited: None,
                stack: None,
                flags: EventFlags::default(),
            })
            .collect()
    }

    /// Records the programs dropped, and the times the kernel skipped a
    /// program because it was already running on that CPU: a bound on
    /// what the trace is missing, since a skip may have been of a thread
    /// it does not trace.
    fn lost(&self) -> [u64; 4] {
        let mut lost = [0; 4];
        if let Some(map) = self.ebpf.map("LOST")
            && let Ok(counts) = PerCpuArray::<_, u64>::try_from(map)
        {
            for (index, count) in (0..).zip(&mut lost) {
                *count = counts
                    .get(&index, 0)
                    .map_or(0, |values| values.iter().sum());
            }
        }

        lost[0] += self.skipped();
        lost
    }

    /// The kernel's count of skipped runs of every loaded program, which
    /// it prints among a program descriptor's details.
    fn skipped(&self) -> u64 {
        self.fdinfo_sum("recursion_misses:")
    }

    /// The sum over every loaded program of a count the kernel prints
    /// among its descriptor's details.
    fn fdinfo_sum(&self, field: &str) -> u64 {
        use std::os::fd::{AsFd as _, AsRawFd as _};
        self.ebpf
            .programs()
            .filter_map(|(_, program)| program.fd().ok())
            .filter_map(|fd| {
                let raw = fd.as_fd().as_raw_fd();
                std::fs::read_to_string(format!("/proc/self/fdinfo/{raw}")).ok()
            })
            .filter_map(|details| {
                details
                    .lines()
                    .find_map(|line| line.strip_prefix(field))
                    .and_then(|count| count.trim().parse::<u64>().ok())
            })
            .sum()
    }

    /// The statistics counted so far into the half being counted into.
    fn counted(&self, process: Pid) -> Folded {
        self.half(self.generation, process)
    }

    /// The statistics counted since the last call, or since tracing began:
    /// the kernel switches halves, every program that might still count
    /// into the old one finishes, and the old one is read and cleared.
    fn take_counted(&mut self, process: Pid) -> Result<Folded, BpfError> {
        let old = self.generation;
        self.configure(|config| config.flags ^= CONFIG_GENERATION)?;
        self.generation ^= 1;
        let (fence, token) = &mut self.fence;
        fence.set(0, token, 0).map_err(load_error)?;
        let folded = self.half(old, process);
        let rows = u32::try_from(self.rows.len().max(1)).unwrap_or(u32::MAX);
        if let Some(map) = self.ebpf.map_mut("STATS") {
            let mut stats =
                PerCpuArray::<_, Bytes<STAT_BYTES>>::try_from(map).map_err(load_error)?;
            let cpus = aya::util::nr_cpus().map_err(|(_, error)| load_error(error))?;
            for row in 0..rows {
                let zeros =
                    PerCpuValues::try_from(vec![[0; STAT_BYTES]; cpus]).map_err(load_error)?;
                stats.set(old * rows + row, zeros, 0).map_err(load_error)?;
            }
        }

        if let Some(map) = self.ebpf.map_mut("THREAD_STATS") {
            let mut threads = HashMap::<_, Bytes<STAT_KEY_BYTES>, Bytes<STAT_BYTES>>::try_from(map)
                .map_err(load_error)?;
            let keys: Vec<_> = threads
                .keys()
                .flatten()
                .filter(|key| {
                    RawStatKey::read_from_bytes(key).is_ok_and(|key| key.row / rows == old)
                })
                .collect();
            for key in keys {
                threads.remove(&key).map_err(load_error)?;
            }
        }

        if let Some(map) = self.ebpf.map_mut("ERRNOS") {
            let mut errnos = PerCpuHashMap::<_, Bytes<ERRNO_KEY_BYTES>, u64>::try_from(map)
                .map_err(load_error)?;
            let keys: Vec<_> = errnos
                .keys()
                .flatten()
                .filter(|key| {
                    RawErrnoKey::read_from_bytes(key).is_ok_and(|key| key.slot >> 31 == old)
                })
                .collect();
            for key in keys {
                errnos.remove(&key).map_err(load_error)?;
            }
        }

        Ok(folded)
    }

    /// The statistics in half `half` of the maps: each thread's, then
    /// those of threads they had no room for.
    fn half(&self, half: u32, process: Pid) -> Folded {
        let rows = u32::try_from(self.rows.len().max(1)).unwrap_or(u32::MAX);
        let mut stats = Vec::new();
        if let Some(map) = self.ebpf.map("THREAD_STATS")
            && let Ok(map) = HashMap::<_, Bytes<STAT_KEY_BYTES>, Bytes<STAT_BYTES>>::try_from(map)
        {
            for (key, value) in map.iter().flatten() {
                let (Ok(key), Ok(value)) = (
                    RawStatKey::read_from_bytes(&key),
                    RawStatValue::read_from_bytes(&value),
                ) else {
                    continue;
                };

                let call = key
                    .row
                    .checked_sub(half * rows)
                    .filter(|&row| row < rows)
                    .and_then(|row| self.rows.get(row as usize));
                if let Some(&call) = call {
                    stats.push((key.tgid, key.tid, call, value));
                }
            }
        }

        if let Some(map) = self.ebpf.map("STATS")
            && let Ok(map) = PerCpuArray::<_, Bytes<STAT_BYTES>>::try_from(map)
        {
            for (row, &call) in (0_u32..).zip(&self.rows) {
                let Ok(values) = map.get(&(half * rows + row), 0) else {
                    continue;
                };

                for value in values.iter() {
                    if let Ok(value) = RawStatValue::read_from_bytes(value)
                        && value.calls > 0
                    {
                        stats.push((0, 0, call, value));
                    }
                }
            }
        }

        let errnos = self
            .ebpf
            .map("ERRNOS")
            .and_then(|map| PerCpuHashMap::<_, Bytes<ERRNO_KEY_BYTES>, u64>::try_from(map).ok())
            .map(|map| {
                map.iter()
                    .flatten()
                    .filter_map(|(key, values)| {
                        let mut key = RawErrnoKey::read_from_bytes(&key).ok()?;
                        (key.slot >> 31 == half).then_some(())?;
                        key.slot &= !(1 << 31);
                        Some((key, values.iter().sum::<u64>()))
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        fold(process, stats, errnos)
    }
}

/// Loads and attaches the programs where `attachment` says, and loads
/// the in-flight iterator, which runs only when asked.
fn attach(
    ebpf: &mut Ebpf,
    btf: &Btf,
    attachment: &Attachment,
    terminal: &BTreeSet<String>,
) -> Result<(), BpfError> {
    for (name, tracepoint) in PROGRAMS {
        if !attachment.functions.is_empty() && matches!(name, "sys_enter" | "sys_exit") {
            continue;
        }

        let program: &mut BtfTracePoint = ebpf
            .program_mut(name)
            .ok_or_else(|| missing(name))?
            .try_into()
            .map_err(load_error)?;
        program.load(tracepoint, btf).map_err(load_error)?;
        program.attach().map_err(load_error)?;
    }

    let exit: &mut KProbe = ebpf
        .program_mut("process_exit")
        .ok_or_else(|| missing("process_exit"))?
        .try_into()
        .map_err(load_error)?;
    exit.load().map_err(load_error)?;
    exit.attach("exit_notify", 0).map_err(load_error)?;

    if !terminal.is_empty() {
        let enter: &mut KProbe = ebpf
            .program_mut("terminal_enter")
            .ok_or_else(|| missing("terminal_enter"))?
            .try_into()
            .map_err(load_error)?;
        enter.load().map_err(load_error)?;
        for function in terminal {
            enter.attach(function, 0).map_err(load_error)?;
        }
    }

    let end: &mut FEntry = ebpf
        .program_mut("process_end")
        .ok_or_else(|| missing("process_end"))?
        .try_into()
        .map_err(load_error)?;
    end.load("do_notify_parent", btf).map_err(load_error)?;
    end.attach().map_err(load_error)?;

    for (slot, function) in attachment.functions.iter().enumerate() {
        if terminal.contains(function) {
            continue;
        }
        let enter = format!("enter_{slot}");
        let program: &mut FEntry = ebpf
            .program_mut(&enter)
            .ok_or_else(|| missing(&enter))?
            .try_into()
            .map_err(load_error)?;
        program.load(function, btf).map_err(load_error)?;
        program.attach().map_err(load_error)?;
        let exit = format!("exit_{slot}");
        let program: &mut FExit = ebpf
            .program_mut(&exit)
            .ok_or_else(|| missing(&exit))?
            .try_into()
            .map_err(load_error)?;
        program.load(function, btf).map_err(load_error)?;
        program.attach().map_err(load_error)?;
    }

    let iterator: &mut Iter = ebpf
        .program_mut("in_flight")
        .ok_or_else(|| missing("in_flight"))?
        .try_into()
        .map_err(load_error)?;
    iterator.load("task", btf).map_err(load_error)?;

    Ok(())
}

fn missing(name: &str) -> BpfError {
    BpfError::Load(format!("the BPF object has no {name}"))
}

/// A zombie leader can still have running workers.
fn ended(process: u32) -> bool {
    std::fs::read_to_string(format!("/proc/{process}/stat")).map_or(true, |stat| {
        stat.rsplit_once(") ").is_some_and(|(_, rest)| {
            (rest.starts_with('Z') || rest.starts_with('X'))
                && rest.split_whitespace().nth(17) == Some("1")
        })
    })
}

/// Traces running process `pid` until it ends or `stop` is set, which
/// leaves it running untraced. Each event goes to `sink` as it arrives.
///
/// # Errors
///
/// What the probe found missing, a failure to load or attach the
/// programs, or a process that has ended.
pub fn trace_running(
    pid: u32,
    options: &TraceOptions,
    engine: BpfOptions,
    stop: &AtomicBool,
    sink: &mut dyn Sink,
) -> Result<Finished, BpfError> {
    run(Start::Attach(pid), options, engine, stop, sink)
}

/// Traces process `pid`, held at another launcher's gate, from its first
/// call.
///
/// The sink hears [`Sink::attached`] once the gate may open.
///
/// # Errors
///
/// As [`trace_running`]'s.
pub fn trace_held(
    pid: u32,
    options: &TraceOptions,
    engine: BpfOptions,
    stop: &AtomicBool,
    sink: &mut dyn Sink,
) -> Result<Finished, BpfError> {
    run(Start::Held(pid), options, engine, stop, sink)
}

/// How a trace finds its first process.
#[derive(Clone, Copy)]
enum Start {
    Attach(u32),
    Held(u32),
}

fn run(
    start: Start,
    options: &TraceOptions,
    engine: BpfOptions,
    stop: &AtomicBool,
    sink: &mut dyn Sink,
) -> Result<Finished, BpfError> {
    Probe::read().check()?;
    let mut loaded = Loaded::load(options, engine, matches!(start, Start::Held(_)))?;
    sink.attached_by(&loaded.attachment);
    let Begun {
        first,
        live,
        mappings,
    } = begin(start, &mut loaded, options, sink)?;
    let mut reader = Reader {
        sink,
        follow: options.follow,
        live,
        first,
        end: None,
        entries: BTreeMap::new(),
        stacks: options.stacks.clone(),
        pairs: Pairs::default(),
        layouts: None,
        v8_layouts: None,
        laid_out: BTreeSet::new(),
        mappings,
        batch: Vec::new(),
        perf: Vec::new(),
        lost: [0; 4],
        untrusted: 0,
        unwinder: options
            .stacks
            .as_ref()
            .and_then(|_| super::unwinder::Unwinder::spawn().ok()),
        pending: std::collections::VecDeque::new(),
        merge: Merge::new(0),
        arrived: std::collections::VecDeque::new(),
        next_job: 0,
    };

    let result = reader.run(&mut loaded, stop);
    let end = reader.end;
    let Reader { sink, .. } = reader;
    sink.counted(loaded.counted(first));
    result?;
    Ok(Finished { end })
}

/// The traced processes as tracing begins.
struct Begun {
    first: Pid,
    live: BTreeSet<Pid>,
    mappings: Vec<perf::Events>,
}

/// Takes the target on, once the programs are attached.
fn begin(
    start: Start,
    loaded: &mut Loaded,
    options: &TraceOptions,
    sink: &mut dyn Sink,
) -> Result<Begun, BpfError> {
    let mut live = BTreeSet::new();
    let mut mappings = Vec::new();
    let first = match start {
        Start::Held(pid) => {
            loaded.target(pid)?;
            if options.stacks.is_some() {
                mappings.push(perf::Events::open(pid, &MAPPINGS).map_err(BpfError::Mappings)?);
            }

            seed_descriptors(pid, sink);
            live.insert(Pid::new(pid));
            sink.attached(pid);
            Pid::new(pid)
        }
        Start::Attach(pid) => {
            if ended(pid) {
                return Err(BpfError::NoProcess(pid));
            }

            loaded.target(pid)?;
            if options.stacks.is_some() {
                mappings.push(
                    perf::Events::attach(pid, &MAPPINGS, MAPPING_EVENTS)
                        .map_err(BpfError::Mappings)?,
                );
            }

            live.insert(Pid::new(pid));
            // The recording samples from here: reading the modules can take
            // seconds, and no stack unwinds before the reader starts.
            sink.attached(pid);
            if let Some(stacks) = &options.stacks {
                // What it mapped before, while it is there to read.
                stacks
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .read_maps(Pid::new(pid));
            }

            Pid::new(pid)
        }
    };

    Ok(Begun {
        first,
        live,
        mappings,
    })
}

/// Events that sample nothing and say what the traced processes map.
const MAPPINGS: perf::EventOptions = perf::EventOptions {
    source: perf::Source::Mappings,
    kernel: false,
    stack_bytes: 0,
    ring_pages: 64,
    switches: false,
};

/// The most such events an attach opens: one per thread and CPU.
const MAPPING_EVENTS: usize = 65_536;

/// Reads the ring and turns records into events.
struct Reader<'a> {
    sink: &'a mut dyn Sink,
    follow: bool,

    /// The traced processes still running.
    live: BTreeSet<Pid>,
    first: Pid,
    end: Option<ProcessEnd>,

    /// Arguments read at a call's entry, by thread, until its exit.
    entries: BTreeMap<Tid, SyscallEvent>,

    /// Unwinds each stack, when stacks were asked for.
    stacks: Option<super::stacks::SharedStacks>,

    /// Each stack and its interpreted frames, until the call or signal
    /// it belongs to.
    pairs: Pairs,

    /// The walker's layouts, when stacks were asked for and the kernel's
    /// process ids are microscope's, and the processes given one or found
    /// to have none.
    layouts: Option<PythonLayouts>,
    v8_layouts: Option<V8Layouts>,
    laid_out: BTreeSet<Pid>,

    /// What the traced processes map, as they map it, for their stacks.
    mappings: Vec<perf::Events>,
    batch: Vec<Record>,
    perf: Vec<u8>,
    lost: [u64; 4],

    /// The mapping rings' heads that could not be trusted, as last counted.
    untrusted: u64,

    /// The thread that unwinds the stacks, when stacks were asked for.
    unwinder: Option<super::unwinder::Unwinder>,

    /// The events not yet given to the sink, in the order of their
    /// records, each waiting for its stack, if it has one, or for those
    /// before it.
    pending: std::collections::VecDeque<Pending>,

    /// Records read from the rings that may yet have earlier ones behind
    /// them in another.
    merge: Merge<Record>,

    /// The stacks unwound that the events at the front have not reached,
    /// in the order the events wait for them.
    arrived: std::collections::VecDeque<super::unwinder::Done>,
    next_job: u64,
}

/// An event waiting to be given to the sink.
struct Pending {
    event: TraceEvent,

    /// The number of the stack it waits for, and the walker's record of
    /// the thread's interpreted frames, read as the stack is finished.
    waiting: Option<(u64, Option<Vec<u8>>)>,
}

/// How long [`Reader::release`] waits.
#[derive(Clone, Copy)]
enum Until {
    /// For nothing: it gives the sink the events whose stacks are here.
    Waiting,

    /// Until fewer than so many events wait.
    Fewer(usize),

    /// Until none waits.
    None,
}

/// The stack of a call or signal, or the number of the job unwinding it.
enum Unwound {
    Now(Option<CallStack>),
    Later(u64, Option<Vec<u8>>),
}

/// How long the reader waits for the ring before it looks at the world.
const TICK_MS: u16 = 100;

/// How long the reader waits for the ring while events wait for their
/// stacks, which it then gives the sink.
const WAITING_TICK_MS: u16 = 10;

/// The most records a read takes from each ring, so that a reader behind
/// its rings still ticks: reports losses, sheds what the budget says, and
/// ends segments.
const MOST_DRAINED: usize = 4096;

/// How long after its time a record is surely in its ring: a program
/// stamps its record before it writes it.
const SETTLED_NS: u64 = 5_000_000;

/// The most events that wait for their stacks before the reader waits for
/// them rather than drain more: with `-k=dwarf`, each holds 8 KiB of stack.
const MOST_PENDING: usize = 2048;

impl Reader<'_> {
    fn run(&mut self, loaded: &mut Loaded, stop: &AtomicBool) -> Result<(), BpfError> {
        let mut rings = std::mem::take(&mut loaded.rings);
        self.merge = Merge::new(rings.len());

        // The walker finds a process's layout by the kernel's process id,
        // which is the one microscope sees only in the initial namespace;
        // elsewhere, Python frames stay unknown, and the stacks say so.
        if self.stacks.is_some() && sampler::check_namespace().is_ok() {
            self.layouts = Some(PythonLayouts::take(&mut loaded.ebpf).map_err(load_error)?);
            self.v8_layouts = Some(V8Layouts::take(&mut loaded.ebpf).map_err(load_error)?);
        }
        let mut stopping = false;
        let mut last_tick = monotonic();
        loop {
            if stop.load(Ordering::Relaxed) && !stopping {
                stopping = true;
                record!("syscalls bpf: stop requested");
                // The traced processes run on untraced.
                self.live.clear();
            }

            // Target removal follows the final record, even if the ring was full.
            // A zombie can appear before that notification has run.
            let gone: Vec<Pid> = self
                .live
                .iter()
                .copied()
                .filter(|&pid| {
                    ended(pid.get())
                        && (loaded.targets.get(&pid.get(), 0).is_err()
                            || !std::path::Path::new(&format!("/proc/{}", pid.get())).exists())
                })
                .collect();
            self.drain(&mut rings, false);
            self.release(Until::Waiting);
            for pid in gone {
                self.live.remove(&pid);
            }

            if self.live.is_empty() {
                self.drain(&mut rings, true);
                self.release(Until::None);

                // Whatever was lost since the last tick.
                self.report_lost(loaded, monotonic());
                break;
            }

            let now = monotonic();
            if now - last_tick >= u64::from(TICK_MS) * 1_000_000 {
                last_tick = now;
                // What the sink is told at a tick follows every event
                // before it.
                self.release(Until::None);
                self.report_lost(loaded, now);
                self.follow_budget(loaded)?;
                self.sink.tick(now);
                if self.sink.segment_due(now) {
                    let counted = loaded.take_counted(self.first)?;
                    self.sink.counted(counted);

                    self.sink.end_segment(now);
                }

                if let Some(after) = self.sink.in_flight_after() {
                    self.sink.in_flight(&loaded.in_flight(after));
                }
            }

            let mut fds: Vec<_> = rings
                .iter()
                .map(|ring| nix::poll::PollFd::new(ring.as_fd(), nix::poll::PollFlags::POLLIN))
                .collect();
            let tick = if self.pending.is_empty() && !self.merge.holds() {
                TICK_MS
            } else {
                WAITING_TICK_MS
            };
            match nix::poll::poll(&mut fds, nix::poll::PollTimeout::from(tick)) {
                Ok(_) | Err(nix::errno::Errno::EINTR) => {}
                Err(error) => return Err(BpfError::System(error)),
            }
        }

        self.release(Until::None);
        self.report_lost(loaded, monotonic());
        for call in loaded.unfinished() {
            // A waiting call took its stack as it began.
            let taken = self.pairs.waiting(call.thread, call.entered);
            let unwound = self.unwind(taken, None);
            self.emit_unwound(TraceEvent::Call(call), unwound);
        }
        self.release(Until::None);

        for (_, entry) in std::mem::take(&mut self.entries) {
            self.sink.event(TraceEvent::Call(entry));
        }

        Ok(())
    }

    /// Gives up what the sink says to shed, and tells it what tracing
    /// has cost the traced threads beyond counting their calls, which no
    /// budget sheds.
    fn follow_budget(&mut self, loaded: &mut Loaded) -> Result<(), BpfError> {
        let shed = self.sink.shed();
        if shed != loaded.shedding {
            loaded.shed(shed)?;
        }

        let runs = loaded.runs();
        let counting = runs.saturating_mul(COUNTED_NS);
        self.sink
            .cost(runs, loaded.kernel_cost(runs).saturating_sub(counting));
        Ok(())
    }

    fn report_lost(&mut self, loaded: &Loaded, time: u64) {
        let lost = loaded.lost();
        for ((why, now), before) in Loss::ALL.into_iter().zip(lost).zip(self.lost) {
            if now > before {
                record!("syscalls bpf: {} {why:?}", now - before);
                self.emit(TraceEvent::Lost {
                    time,
                    count: now - before,
                    why,
                });
            }
        }
        self.lost = lost;
    }

    /// Reads every ring, and handles the records old enough that no ring
    /// can still be writing one before them, or with `every`, all of them.
    fn drain(&mut self, rings: &mut [RingBuf<MapData>], every: bool) {
        let cut = (!every).then(|| monotonic().saturating_sub(SETTLED_NS));
        for (index, ring) in rings.iter_mut().enumerate() {
            let mut taken = 0;
            while taken < MOST_DRAINED
                && let Some(item) = ring.next()
            {
                taken += 1;
                match decode(&item) {
                    Ok(record) => self.merge.push(index, record),
                    // Only the flight recorder, which release builds leave
                    // out, reads the error.
                    #[cfg_attr(not(debug_assertions), expect(unused_variables))]
                    Err(error) => {
                        record!("syscalls bpf: undecodable record: {error}");
                        self.emit(TraceEvent::Lost {
                            time: monotonic(),
                            count: 1,
                            why: Loss::Dropped,
                        });
                    }
                }
            }
        }

        // A stack's code was mapped before the stack was sent, so the
        // mappings read after the records name all it needs.
        let mut batch = std::mem::take(&mut self.batch);
        self.merge.release(cut, Record::time, &mut batch);
        self.map();
        self.lay_out();
        for record in std::mem::take(&mut batch) {
            self.handle(record);
            if self.pending.len() >= MOST_PENDING {
                self.release(Until::Fewer(MOST_PENDING / 2));
            }
        }

        self.batch = batch;
    }

    /// Takes in what the traced processes mapped since last asked, in the
    /// order they mapped it.
    fn map(&mut self) {
        let Some(stacks) = self.stacks.clone() else {
            return;
        };

        self.perf.clear();
        for events in &mut self.mappings {
            events.drain(&mut self.perf);
        }

        // A ring that skipped a span whose head it could not trust lost
        // the mappings in it.
        let untrusted = self.mappings.iter().map(perf::Events::untrusted).sum();
        if untrusted > self.untrusted {
            self.emit(TraceEvent::Lost {
                time: monotonic(),
                count: untrusted - self.untrusted,
                why: Loss::Dropped,
            });
            self.untrusted = untrusted;
        }

        // The records that change a process's modules.
        let mut records = Vec::new();
        let mut offset = 0;
        while let Some(Ok((record, length))) = self.perf.get(offset..).and_then(perf::decode::next)
        {
            let changes = match record {
                perf::decode::Record::Mmap(mmap) => mmap.executable,
                perf::decode::Record::Comm { exec, .. } => exec,
                perf::decode::Record::Fork { pid, ppid, .. } => pid != ppid,
                _ => false,
            };
            if changes {
                records.push((record.time().unwrap_or(0), offset));
            }
            offset += length;
        }

        records.sort_unstable();
        // A stack is finished through its process's modules as they were
        // before these changed them.
        if !records.is_empty() {
            self.release(Until::None);
        }
        let mut stacks = stacks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // Processes that began or replaced their images, whose layouts are
        // read again.
        let mut fresh = Vec::new();
        for (_, offset) in records {
            match self.perf.get(offset..).and_then(perf::decode::next) {
                Some(Ok((perf::decode::Record::Mmap(mmap), _))) if mmap.executable => {
                    stacks.mapped(&mmap);
                }

                Some(Ok((
                    perf::decode::Record::Comm {
                        pid, exec: true, ..
                    },
                    _,
                ))) => {
                    stacks.forget(pid);
                    fresh.push(pid);
                }
                Some(Ok((perf::decode::Record::Fork { pid, ppid, .. }, _))) if pid != ppid => {
                    stacks.forked(ppid, pid);
                    fresh.push(pid);
                }
                _ => {}
            }
        }

        drop(stacks);
        for process in fresh {
            self.forget_layout(process);
            self.laid_out.remove(&process);
        }
    }

    /// Gives the walkers the layout of each process that runs Python or V8
    /// and has none yet: it maps its runtime before any of its threads
    /// runs code of the runtime's.
    fn lay_out(&mut self) {
        let Some(stacks) = &self.stacks else {
            return;
        };

        let mut stacks = stacks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for process in stacks.processes() {
            if self.laid_out.contains(&process) {
                continue;
            }

            let set = if let Some(layouts) = &mut self.layouts
                && let Some(layout) = stacks.python_layout(process)
            {
                layout.and_then(|layout| {
                    layouts
                        .set(process.get(), &layout)
                        .map_err(|error| error.to_string())
                })
            } else if let Some(layouts) = &mut self.v8_layouts
                && let Some(layout) = stacks.v8_layout(process)
            {
                layout.and_then(|layout| {
                    layouts
                        .set(process.get(), &layout)
                        .map_err(|error| error.to_string())
                })
            } else {
                continue;
            };

            self.laid_out.insert(process);
            match set {
                Ok(()) => {}
                // Only the flight recorder, which release builds leave
                // out, reads why.
                #[cfg_attr(not(debug_assertions), expect(unused_variables))]
                Err(why) => record!("syscalls bpf: runtime of {process}: {why}"),
            }
        }

        drop(stacks);
    }

    /// Forgets the layout of `process`, which exited or replaced its image.
    fn forget_layout(&mut self, process: Pid) {
        if self.laid_out.contains(&process) {
            if let Some(layouts) = &mut self.layouts {
                layouts.forget(process.get());
            }
            if let Some(layouts) = &mut self.v8_layouts {
                layouts.forget(process.get());
            }
        }
    }

    /// Unwinds the stack paired with a call or signal, unless the record
    /// says its stack did not fit in the ring, and that whatever stack it
    /// was paired with is another record's.
    ///
    /// A stack is unwound on the thread of [`Self::unwinder`], through its
    /// process's modules as they are now, and finished, with its
    /// interpreted frames, as its event is given to the sink.
    fn unwind(&mut self, taken: Option<Taken>, lost: Option<CallStack>) -> Unwound {
        if lost.is_some() {
            return Unwound::Now(lost);
        }

        let (
            Some(Taken {
                stack: sampled,
                python,
            }),
            Some(stacks),
        ) = (taken, self.stacks.as_ref())
        else {
            return Unwound::Now(None);
        };
        let mut stacks = stacks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(unwinder) = &self.unwinder else {
            return Unwound::Now(Some(stacks.unwind_sampled(
                sampled.process,
                SampledState {
                    registers: &sampled.registers,
                    stack: &sampled.copy,
                    chain: &sampled.chain,
                },
                python.as_deref(),
            )));
        };

        let unwinding = stacks.unwinding(sampled.process, sampled.chain.first().copied());
        drop(stacks);
        let id = self.next_job;
        self.next_job += 1;
        let job = super::unwinder::Job {
            id,
            unwinding,
            sampled,
        };
        if unwinder.send(job) {
            Unwound::Later(id, python)
        } else {
            // The thread no longer runs.
            self.unwinder = None;
            Unwound::Now(Some(lost_stack()))
        }
    }

    /// Gives `event`, whose stack is set, to the sink after those before it.
    fn emit(&mut self, event: TraceEvent) {
        if self.pending.is_empty() {
            self.sink.event(event);
        } else {
            self.pending.push_back(Pending {
                event,
                waiting: None,
            });
        }
    }

    /// Gives `event` to the sink once its stack is unwound, or now.
    fn emit_unwound(&mut self, mut event: TraceEvent, unwound: Unwound) {
        match unwound {
            Unwound::Now(stack) => {
                set_stack(&mut event, stack);
                self.emit(event);
            }
            Unwound::Later(id, python) => self.pending.push_back(Pending {
                event,
                waiting: Some((id, python)),
            }),
        }
    }

    /// Gives the sink each waiting event whose stack, and those before it,
    /// are unwound, waiting for them as `until` says.
    fn release(&mut self, until: Until) {
        loop {
            if let Some(unwinder) = &self.unwinder {
                while let Some(done) = unwinder.try_done() {
                    self.arrived.push_back(done);
                }
            }

            let Some(front) = self.pending.front() else {
                return;
            };
            // The stacks come back in the order the events wait for them.
            if front.waiting.is_some() && self.arrived.is_empty() {
                let wait = match until {
                    Until::Waiting => false,
                    Until::Fewer(most) => self.pending.len() >= most,
                    Until::None => true,
                };
                if !wait {
                    return;
                }
                if let Some(done) = self
                    .unwinder
                    .as_ref()
                    .and_then(super::unwinder::Unwinder::wait_done)
                {
                    self.arrived.push_back(done);
                } else {
                    // The thread no longer runs: the stacks it had not
                    // unwound are lost.
                    self.unwinder = None;
                    for pending in &mut self.pending {
                        if pending.waiting.take().is_some() {
                            set_stack(&mut pending.event, Some(lost_stack()));
                        }
                    }
                }
                continue;
            }

            let Some(Pending { mut event, waiting }) = self.pending.pop_front() else {
                return;
            };
            if let Some((id, python)) = waiting
                && let Some(done) = self.arrived.pop_front()
                && let Some(stacks) = &self.stacks
            {
                debug_assert_eq!(done.id, id, "a stack came back out of turn");
                let unwound = match done.unwound {
                    Ok(unwound) => unwound,
                    Err(panic) => std::panic::resume_unwind(panic),
                };
                let mut stacks = stacks
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                let stack = stacks.finish_sampled(
                    done.sampled.process,
                    SampledState {
                        registers: &done.sampled.registers,
                        stack: &done.sampled.copy,
                        chain: &done.sampled.chain,
                    },
                    (&done.unwinding, unwound),
                    python.as_deref(),
                );
                drop(stacks);
                set_stack(&mut event, Some(stack));
            }
            self.sink.event(event);
        }
    }

    fn handle(&mut self, record: Record) {
        match record {
            Record::Stack(stack) => self.pairs.stack(stack),
            Record::Python { thread, record } => self.pairs.python(thread, record),
            Record::Entry(entry) => {
                self.entries.insert(entry.thread, entry);
            }
            Record::Handoff {
                former,
                thread,
                process,
                entered,
            } => {
                if former != thread
                    && self.entries.get(&former).is_some_and(|entry| {
                        entry.process == process && entry.entered == Some(entered)
                    })
                    && let Some(mut entry) = self.entries.remove(&former)
                {
                    entry.thread = thread;
                    if let Some(displaced) = self.entries.insert(thread, entry) {
                        self.emit(TraceEvent::Call(displaced));
                    }
                }
            }
            Record::Call(mut call) => {
                let taken = self.pairs.call(call.thread, call.entered);
                let unwound = self.unwind(taken, call.stack.take());
                if let Some(entry) = self.entries.remove(&call.thread) {
                    if Some(entry.entered.unwrap_or(0)) == call.entered.or(Some(0)) {
                        let mut captured = entry.captured;
                        captured.append(&mut call.captured);
                        captured.sort_by_key(|capture| capture.arg);
                        call.captured = captured;
                    } else {
                        // The entry's exit was never seen.
                        self.emit(TraceEvent::Call(entry));
                    }
                }

                self.emit_unwound(TraceEvent::Call(call), unwound);
            }
            Record::Event(mut event) => {
                // A file record precedes its call, whose exit it shares.
                if let TraceEvent::Descriptor { time, .. } = &mut event {
                    *time = monotonic();
                }

                let unwound = match &mut event {
                    TraceEvent::Signal { thread, stack, .. } => {
                        let (taken, stack) = (self.pairs.signal(*thread), stack.take());
                        self.unwind(taken, stack)
                    }
                    _ => Unwound::Now(None),
                };

                match &event {
                    TraceEvent::Spawn { child, thread, .. } if !*thread && self.follow => {
                        self.live.insert(Pid::led_by(*child));
                    }

                    TraceEvent::Exit { process, end, .. } => {
                        if *process == self.first {
                            self.end = Some(*end);
                        }

                        self.live.remove(process);
                        self.forget_layout(*process);
                    }
                    _ => {}
                }

                match unwound {
                    // An event other than a signal has no stack to set.
                    Unwound::Now(None) => self.emit(event),
                    unwound => self.emit_unwound(event, unwound),
                }
            }
        }
    }
}

/// Sets the stack of `event`, a call or a signal.
fn set_stack(event: &mut TraceEvent, unwound: Option<CallStack>) {
    match event {
        TraceEvent::Call(call) => call.stack = unwound,
        TraceEvent::Signal { stack, .. } => *stack = unwound,
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A short list attaches to its calls' functions, each once; anything
    /// that cannot falls back to the tracepoints and says why.
    #[test]
    fn short_lists_attach_to_their_functions_and_the_rest_say_why_not() {
        let has = |name: &str| !name.contains("getppid");
        let named = |abi, name| SyscallId::named(abi, name).expect(name);
        let open = [
            named(Abi::X86_64, "openat"),
            named(Abi::X32, "openat"),
            named(Abi::X86_32, "openat"),
        ];
        let chosen = attachment(false, &open, || true, &has);
        assert_eq!(
            chosen.functions,
            ["__x64_sys_openat", "__ia32_compat_sys_openat"]
        );
        assert_eq!(chosen.name(), "fentry");

        let without = [named(Abi::X86_64, "getppid")];
        let fallen = attachment(false, &without, || true, &has);
        assert_eq!(
            (fallen.name(), fallen.reason.as_str()),
            (
                "tp_btf",
                "getppid has no kernel function that BTF describes"
            )
        );
        let all = attachment(true, &[], || unreachable!("every call needs no x32"), &has);
        assert_eq!(all.reason, "every call is traced");

        let many: Vec<SyscallId> = (0..=40)
            .map(|number| SyscallId::new(Abi::X86_64, number))
            .filter(|call| call.name().is_some())
            .collect();
        let wide = attachment(false, &many, || true, &|_: &str| true);
        assert_eq!(wide.name(), "tp_btf", "{wide:?}");

        // A kernel without x32 has none of its compat functions, and no x32
        // call ever reaches one.
        let no_x32 = |name: &str| !name.starts_with("__x64_compat_");
        let sigaction = [
            named(Abi::X86_64, "rt_sigaction"),
            named(Abi::X32, "rt_sigaction"),
        ];
        let chosen = attachment(false, &sigaction, || false, &no_x32);
        assert_eq!(chosen.functions, ["__x64_sys_rt_sigaction"]);
        let fallen = attachment(false, &sigaction, || true, &no_x32);
        assert_eq!(fallen.name(), "tp_btf");
    }

    use crate::syscalls::captures::MOST_BYTES;

    #[test]
    fn records_h_and_the_decoders_agree_field_for_field() {
        crate::backend::linux::bpf::assert_mirrors!(
            OBJECT,
            (
                RawArgCopy,
                "arg_copy",
                [kind, when, writes, count, count_arg, pad, size, limit]
            ),
            (
                RawCallSpec,
                "call_spec",
                [
                    selected, moves, makes, makes_arg, forks, execs, stat, ends, waits, pad, args
                ]
            ),
            (
                RawFileRecord,
                "file_record",
                [header, fd, mode, ino, magic, rdev, len, cloexec, pad]
            ),
            (
                RawConfig,
                "config",
                [flags, elements, sample, rows, slow_ns, cap, burst]
            ),
            (
                RawEntry,
                "entry",
                [entered, args, nr, tgid, exec_tid, abi, flags, stacked, pad]
            ),
            (RawRecord, "record", [kind, size, tid, tgid]),
            (RawInFlightRecord, "in_flight_record", [tid, pad, entry]),
            (
                RawCallRecord,
                "call_record",
                [header, entered, exited, ret, args, nr, abi, flags, captures]
            ),
            (
                RawCaptureHeader,
                "capture_header",
                [arg, when, state, pad, len, wanted, pad2]
            ),
            (
                RawSpawnRecord,
                "spawn_record",
                [header, time, child, child_tgid]
            ),
            (RawExecRecord, "exec_record", [header, time, pad, len]),
            (
                RawHandoffRecord,
                "handoff_record",
                [header, entered, former, pad]
            ),
            (RawExitRecord, "exit_record", [header, time, code, last]),
            (
                RawSignalRecord,
                "signal_record",
                [header, time, signal, flags, info]
            ),
            (
                RawStatValue,
                "stat_value",
                [
                    calls, errors, timed, total_ns, min_ns, max_ns, bytes, log2_ns
                ]
            ),
            (RawStatKey, "stat_key", [tid, tgid, row, reserved]),
            (RawErrnoKey, "errno_key", [tid, tgid, slot, err]),
            (
                RawStackRecord,
                "stack_record",
                [header, frames, copied, entered, interpreted, pad, regs]
            ),
        );
    }

    fn call_bytes(kind: u32, flags: u8, captures: &[(u8, u8, &[u8])]) -> Vec<u8> {
        let mut tail = Vec::new();
        for &(arg, state, data) in captures {
            let header = RawCaptureHeader {
                arg,
                when: 1,
                state,
                len: u32::try_from(data.len()).expect("small"),
                wanted: if state == 1 { u32::MAX } else { 0 },
                ..RawCaptureHeader::default()
            };

            tail.extend_from_slice(header.as_bytes());
            tail.extend_from_slice(data);
            tail.resize(tail.len().next_multiple_of(8), 0);
        }

        let record = RawCallRecord {
            header: RawRecord {
                kind,
                size: u32::try_from(size_of::<RawCallRecord>() + tail.len()).expect("small"),
                tid: 11,
                tgid: 10,
            },
            entered: 100,
            exited: 250,
            ret: -2,
            args: [1, 2, 3, 4, 5, 6],
            nr: 257,
            abi: 0,
            flags,
            captures: u16::try_from(captures.len()).expect("small"),
        };

        let mut bytes = record.as_bytes().to_vec();
        bytes.extend_from_slice(&tail);
        bytes
    }

    #[test]
    fn unreadable_signal_info_stays_unknown() {
        let raw = RawSignalRecord {
            header: RawRecord {
                kind: RECORD_SIGNAL,
                size: u32::try_from(size_of::<RawSignalRecord>()).expect("small"),
                tid: 11,
                tgid: 10,
            },
            time: 250,
            signal: 10,
            flags: SIGNAL_INFO_LOST | u32::from(CALL_STACK_LOST),
            info: [0; 48],
        };
        assert_eq!(
            decode(raw.as_bytes()),
            Ok(Record::Event(TraceEvent::Signal {
                thread: Tid::new(11),
                process: Pid::new(10),
                time: 250,
                signal: 10,
                code: None,
                sender: None,
                info: None,
                stack: Some(lost_stack()),
            }))
        );
    }

    /// A record of Python frames decodes whole, its bytes kept to read when
    /// its thread's stack is unwound, and one cut short is refused.
    #[test]
    fn a_record_of_python_frames_decodes_whole() {
        let header = RawRecord {
            kind: RECORD_PY_STACK,
            size: 24,
            tid: 7,
            tgid: 5,
        };
        let mut bytes = header.as_bytes().to_vec();
        bytes.extend([1; 8]);
        bytes.extend([2; 4]);
        assert_eq!(
            decode(&bytes),
            Ok(Record::Python {
                thread: Tid::new(7),
                record: bytes[..24].to_vec(),
            })
        );
        assert_eq!(decode(&bytes[..20]), Err(DecodeError::Short(20)));
    }

    /// A stack taken as a waiting call began is merged by that time, since
    /// its call can return on another CPU; one taken at its call's exit or
    /// signal goes with that record.
    #[test]
    fn a_stack_taken_as_its_call_began_is_merged_by_that_time() {
        for (entered, interpreted, time) in [(0, 1, None), (5_000, 0, Some(5_000))] {
            let raw = RawStackRecord {
                header: RawRecord {
                    kind: RECORD_STACK,
                    size: u32::try_from(size_of::<RawStackRecord>() + 8).expect("small"),
                    tid: 11,
                    tgid: 10,
                },
                frames: 1,
                copied: 0,
                entered,
                interpreted,
                pad: 0,
                regs: [0; 17],
            };
            let mut bytes = raw.as_bytes().to_vec();
            bytes.extend(0x40_1000_u64.to_le_bytes());
            let record = decode(&bytes).expect("a stack");
            let merged = record.time();
            let Record::Stack(stack) = record else {
                panic!("a stack: {record:?}");
            };
            assert_eq!(
                (stack.chain, stack.interpreted, merged),
                (vec![0x40_1000], interpreted == 1, time)
            );
        }
    }

    #[test]
    fn call_records_decode_with_their_captures_and_reject_overruns() {
        let bytes = call_bytes(RECORD_CALL, 0, &[(1, 0, b"/etc/hosts"), (2, 1, b"abc")]);
        let Ok(Record::Call(call)) = decode(&bytes) else {
            panic!("a call");
        };

        assert_eq!(
            (call.thread, call.process, call.call.name(), call.outcome),
            (
                Tid::new(11),
                Pid::new(10),
                Some("openat"),
                Outcome::Failed(crate::syscalls::Errno(2))
            )
        );
        assert_eq!(call.duration(), Some(150));
        assert_eq!(&*call.captured[0].bytes, b"/etc/hosts");
        assert_eq!(
            call.captured[1].state,
            CaptureState::Truncated { wanted: u32::MAX }
        );

        // A thread that ended inside exit_group, whose entry was not seen.
        let mut bytes = call_bytes(RECORD_CALL, 2 | CALL_PROCESS_EXITED, &[]);
        bytes[std::mem::offset_of!(RawCallRecord, nr)..][..4]
            .copy_from_slice(&231_u32.to_ne_bytes());
        let Ok(Record::Call(call)) = decode(&bytes) else {
            panic!("a call");
        };

        assert_eq!(
            (call.outcome, call.entered, call.exited),
            (Outcome::ProcessExited, None, None)
        );
        assert!(call.flags.contains(EventFlags::ENTRY_UNSEEN));

        // Every other call returns before its thread ends, so one held
        // then had its return go unseen.
        let bytes = call_bytes(RECORD_CALL, CALL_PROCESS_EXITED, &[]);
        assert!(matches!(
            decode(&bytes),
            Ok(Record::Event(TraceEvent::Lost { count: 1, .. }))
        ));

        // A call whose stack did not fit in the ring says so, rather than
        // take the thread's last stack.
        let Ok(Record::Call(call)) = decode(&call_bytes(RECORD_CALL, CALL_STACK_LOST, &[])) else {
            panic!("a call");
        };
        let stack = call.stack.expect("a stack that says it was lost");
        assert_eq!(
            (stack.addresses.len(), stack.truncation()),
            (0, Some(Truncation::NoStack))
        );

        let mut bytes = call_bytes(RECORD_CALL, 0, &[(1, 0, b"/etc/hosts")]);
        let header = size_of::<RawCallRecord>();
        bytes[header + 4] = 200;
        assert_eq!(decode(&bytes), Err(DecodeError::Capture(1)));
        assert!(decode(&bytes[..10]).is_err());
    }

    #[test]
    fn copy_rules_reach_the_kernel_as_the_ptrace_engine_follows_them() {
        let options = TraceOptions {
            calls: crate::syscalls::CallSet::none(),
            limits: captures::Limits::default(),
            verbose: false,
            follow: true,
            stacks: None,
        };

        let execve = SyscallId::named(Abi::X86_64, "execve").expect("execve");
        let spec = call_spec(execve, &options);

        // The path and argv at entry; the environment only counted.
        assert_eq!(
            spec.args[..3]
                .iter()
                .map(|arg| (arg.kind, arg.when))
                .collect::<Vec<_>>(),
            [(1, 0), (3, 0), (4, 0)]
        );
        let read = SyscallId::named(Abi::X86_64, "read").expect("read");
        let spec = call_spec(read, &options);
        assert_eq!(spec.moves, 1);
        assert_eq!(
            (spec.args[1].kind, spec.args[1].when, spec.args[1].count),
            (5, 1, 2)
        );
        assert!(spec.args.iter().all(|arg| arg.limit as usize <= MOST_BYTES));
    }

    #[test]
    fn statistics_fold_over_cpus_with_their_errnos() {
        let openat = SyscallId::named(Abi::X86_64, "openat").expect("openat");
        let mut one = RawStatValue {
            calls: 3,
            errors: 1,
            timed: 3,
            total_ns: 300,
            min_ns: 50,
            max_ns: 200,
            bytes: 0,
            log2_ns: [0; BUCKETS],
        };

        one.log2_ns[7] = 3;
        let mut two = one;
        two.min_ns = 20;
        // x86-64 calls fill the first slots.
        let errno = |tgid, tid| RawErrnoKey {
            tid,
            tgid,
            slot: openat.number,
            err: 2,
        };
        let folded = fold(
            Pid::new(7),
            [
                (0, 0, openat, one),
                (0, 0, openat, two),
                (9, 10, openat, one),
            ],
            [(errno(0, 0), 2), (errno(9, 10), 1)],
        );

        // Threads the kernel had no room for are the trace's process's.
        let spilled = &folded.by_call[&(Pid::new(7), Tid::new(0), openat)];
        assert_eq!(
            (
                spilled.calls,
                spilled.min_ns,
                spilled.max_ns,
                spilled.log2_ns[7]
            ),
            (6, 20, 200, 6)
        );
        assert_eq!(spilled.errors_by_errno[&2], 2);

        let thread = &folded.by_call[&(Pid::new(9), Tid::new(10), openat)];
        assert_eq!((thread.calls, thread.errors_by_errno[&2]), (3, 1));
    }

    /// Where the kernel does not time the programs, a call costs what its
    /// cheapest remaining work does: shedding never estimates it dearer.
    #[test]
    fn a_call_costs_what_the_programs_still_do_with_it() {
        let copied = CONFIG_STREAM | CONFIG_STACKS | CONFIG_STACK_COPY;
        let walked = CONFIG_STREAM | CONFIG_STACKS;
        let shed = |bits: &[Shed]| Shed(bits.iter().fold(0, |all, bit| all | bit.0));
        assert_eq!(per_call(copied, Shed::default()), COPIED_STACK_NS);
        assert_eq!(per_call(walked, Shed::default()), STACK_NS);
        assert_eq!(per_call(copied, Shed::STACKS), STREAMED_NS);
        assert_eq!(per_call(CONFIG_STREAM, Shed::default()), STREAMED_NS);
        assert_eq!(
            per_call(copied, shed(&[Shed::STACKS, Shed::SAMPLED])),
            COUNTED_NS + STREAMED_NS / 16
        );
        assert_eq!(
            per_call(copied, shed(&[Shed::STACKS, Shed::SAMPLED, Shed::STREAM])),
            COUNTED_NS
        );
        assert_eq!(
            per_call(CONFIG_COUNT | CONFIG_STACKS, Shed::default()),
            COUNTED_NS
        );

        // Each rung costs less than the one above it.
        let mut shedding = Shed::default();
        let mut cost = per_call(copied, shedding);
        for rung in [Shed::STACKS, Shed::STRINGS, Shed::SAMPLED, Shed::STREAM] {
            shedding.0 |= rung.0;
            let next = per_call(copied, shedding);
            assert!(next <= cost, "{shedding:?}");
            cost = next;
        }
    }

    #[test]
    fn the_probe_names_what_is_missing() {
        let probe = Probe {
            release: "6.8.0".into(),
            version: Some((6, 8)),
            btf: true,
            capabilities: 0,
            lockdown: None,
            token: None,
            user_namespace: false,
        };

        let message = probe.check().expect_err("unprivileged").to_string();
        assert!(message.contains("has neither"), "{message}");
        assert!(message.contains("--syscall-engine ptrace"), "{message}");

        // A token a container offers is named, and why it goes unused.
        let offered = Probe {
            token: Some("/sys/fs/bpf".into()),
            ..probe.clone()
        };

        let message = offered.check().expect_err("unprivileged").to_string();
        assert!(
            message.contains("/sys/fs/bpf offers a BPF token, which microscope cannot use"),
            "{message}"
        );
        let able = Probe {
            capabilities: (1 << CAP_BPF) | (1 << CAP_PERFMON),
            ..probe
        };

        assert!(able.check().is_ok());
        let contained = Probe {
            user_namespace: true,
            ..able.clone()
        };
        assert_eq!(contained.missing(), Some(Missing::UserNamespace));
        let old = Probe {
            version: version("5.10.0-23-amd64"),
            ..able
        };

        assert!(old.check().expect_err("old").to_string().contains("5.15"));
        assert_eq!(version("7.2.5-200.fc44.x86_64"), Some((7, 2)));
    }
}
