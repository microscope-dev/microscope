use std::collections::{BTreeMap, BTreeSet};
use std::os::fd::{AsFd, OwnedFd};
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
use crate::syscalls::captures::{self, CopyKind, Limits};
use crate::syscalls::filter::CallSet;
use crate::syscalls::specs::{self, Arg, Count, MemKind, Ret, Struct};
use crate::syscalls::stats::{self, BUCKETS, Folded};
use crate::syscalls::{
    Abi, CallStack, Capture, CaptureState, Device, EventFlags, InFlight, Loss, Outcome, Pid,
    ProcessEnd, ReadAt, SyscallEvent, SyscallId, SyscallStats, Tid, TraceEvent,
};

use crate::capture::records::Truncation;
use crate::unwind::DwarfRegister;
use crate::unwind::sample::SampledState;

#[cfg(not(feature = "bpf-faults"))]
static OBJECT: &[u8] = aya::include_bytes_aligned!(concat!(env!("OUT_DIR"), "/syscalls.bpf.o"));

#[cfg(feature = "bpf-faults")]
static OBJECT: &[u8] =
    aya::include_bytes_aligned!(concat!(env!("OUT_DIR"), "/syscalls-faults.bpf.o"));

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
const CONFIG_CAP_STREAM: u32 = 256;

const CALL_EXIT_SKIPPED: u8 = 1;
const CALL_PROCESS_EXITED: u8 = 8;
const CALL_STACK_LOST: u8 = 32;
const SIGNAL_INFO_LOST: u32 = 1;

fn lost_stack() -> CallStack {
    CallStack {
        addresses: Arc::from([]),
        truncated: Some(Truncation::NoStack as u8),
        interpreted: Arc::from([]),
    }
}

const CALL_EVENT_FLAGS: u8 = EventFlags::ENTRY_UNSEEN.0
    | EventFlags::ABI_UNCERTAIN.0
    | EventFlags::KEPT_FAILED.0
    | EventFlags::KEPT_SLOW.0;

const SLOTS: u32 = 3072;

const _: () = assert!(SLOTS <= 1 << 16, "statistics rows outgrow their u16");

pub const DEFAULT_RING: u32 = 2 << 20;

pub const STACK_COPY_RING: u32 = 8 << 20;

const SMALLEST_RING: u32 = 256 << 10;

pub const CAPPED_BYTES: u32 = 16;

pub const CAPPED_ELEMENTS: u32 = 4;

pub const SAMPLED_EVERY: u32 = 16;

const COUNTED_NS: u64 = 150;
const STREAMED_NS: u64 = 220;
const STACK_NS: u64 = 560;
const COPIED_STACK_NS: u64 = 1_100;

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

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Shed(pub u8);

impl Shed {
    pub const STACKS: Self = Self(1);

    pub const STRINGS: Self = Self(2);

    pub const SAMPLED: Self = Self(4);

    pub const STREAM: Self = Self(8);

    #[must_use]
    pub const fn has(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }
}

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
    pub examples: u32,
}

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
    pub known: u8,
    pub capped: u8,
    pub pad: [u8; 7],
}

#[repr(C)]
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, FromBytes, IntoBytes, Immutable, KnownLayout,
)]
pub struct RawInFlightRecord {
    pub tid: u32,
    pub pad: u32,
    pub entry: RawEntry,
}

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

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, FromBytes, IntoBytes, Immutable, KnownLayout)]
pub struct RawSignalRecord {
    pub header: RawRecord,
    pub time: u64,
    pub signal: i32,
    pub flags: u32,
    pub info: [u8; 48],
}

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

#[must_use]
pub const fn errno_slot(call: SyscallId) -> u32 {
    let abi = match call.abi {
        Abi::X86_64 => 0,
        Abi::X86_32 => 1,
        Abi::X32 => 2,
    };

    abi << 10 | (call.number & 1023)
}

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

fn call_of(slot: u32) -> Option<SyscallId> {
    let abi = abi_of(u8::try_from(slot >> 10).ok()?)?;
    Some(SyscallId::new(abi, slot & 1023))
}

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

fn reads_at_entry(call: SyscallId) -> bool {
    matches!(call.name(), Some("execve" | "execveat" | "rseq"))
}

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

fn call_program(name: &str) -> bool {
    matches!(name, "sys_enter" | "sys_exit")
        || name
            .strip_prefix("enter_")
            .or_else(|| name.strip_prefix("exit_"))
            .is_some_and(|slot| slot.parse::<u8>().is_ok())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Record {
    Call(SyscallEvent),

    Entry(SyscallEvent),
    Event(TraceEvent),

    Handoff {
        former: Tid,
        thread: Tid,
        process: Pid,
        entered: u64,
    },

    Stack(SampledStack),

    Python {
        thread: Tid,
        record: Vec<u8>,
    },
}

impl Record {
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SampledStack {
    pub thread: Tid,
    pub process: Pid,

    pub entered: Option<u64>,

    pub interpreted: bool,

    pub registers: [(DwarfRegister, u64); 17],

    pub chain: Vec<u64>,

    pub copy: Vec<u8>,
}

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

            let sender = code
                .is_some_and(|code| code <= 0 && code != -2)
                .then(|| u32::try_from(word(16)).ok().map(Pid::new))
                .flatten();

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
    let call = SyscallId::new(abi, raw.nr);

    let ends = call
        .name()
        .is_some_and(|name| matches!(name, "exit" | "exit_group" | "execve" | "execveat"));
    let exited_inside = raw.flags & CALL_PROCESS_EXITED != 0 && ends;
    let skipped = raw.flags & (CALL_EXIT_SKIPPED | CALL_PROCESS_EXITED) != 0 && !exited_inside;

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
        } else if skipped {
            Outcome::Unavailable
        } else {
            Outcome::of_return(raw.ret)
        },
        entered: (!unseen).then_some(raw.entered),
        exited: (header.kind == RECORD_CALL && !exited_inside && !skipped).then_some(raw.exited),
        stack: (raw.flags & CALL_STACK_LOST != 0).then(lost_stack),
        flags: EventFlags(raw.flags & CALL_EVENT_FLAGS),
    };

    Ok(if header.kind == RECORD_ENTRY {
        Record::Entry(event)
    } else {
        Record::Call(event)
    })
}

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

const REMEDY: &str =
    "  microscope uses eBPF so it never stops or slows the traced program, and it will
  not fall back to ptrace on its own. To continue, one of:
    - run it from a unit with AmbientCapabilities=CAP_BPF CAP_PERFMON
    - on NixOS or system-manager, import microscope's bpf module and run through
      /run/wrappers/bin/microscope-bpf (docs/profiling.md#give-microscope-the-bpf-privileges)
    - record a program you launch with --syscall-engine ptrace, which stops the
      program at each chosen call";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Missing {
    Kernel(String),
    Btf,

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

fn delegating_bpffs() -> Option<std::path::PathBuf> {
    let mounts = std::fs::read_to_string("/proc/self/mountinfo").ok()?;
    mounts.lines().find_map(|line| {
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Probe {
    pub release: String,

    pub version: Option<(u32, u32)>,
    pub btf: bool,

    pub capabilities: u64,

    pub lockdown: Option<String>,

    pub token: Option<std::path::PathBuf>,

    pub user_namespace: bool,
}

impl Probe {
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct BpfOptions {
    pub count_only: bool,

    pub stack_copy: bool,

    pub exceptions: Option<Exceptions>,

    pub streamed_per_second: Option<u32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Exceptions {
    pub slow: std::time::Duration,
    pub per_second: u32,
    pub examples_per_minute: Option<u32>,
}

impl RawConfig {
    fn new(options: &TraceOptions, engine: BpfOptions, launch: bool, rows: u32) -> Self {
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

        let (slow_ns, mut cap, examples) = engine.exceptions.map_or((0, 0, 0), |exceptions| {
            flags |= CONFIG_EXCEPTIONS;
            (
                u64::try_from(exceptions.slow.as_nanos()).unwrap_or(u64::MAX),
                exceptions.per_second.max(1),
                exceptions.examples_per_minute.map_or(0, |most| most.max(1)),
            )
        });

        if let Some(most) = engine.streamed_per_second.filter(|_| !engine.count_only) {
            flags |= CONFIG_CAP_STREAM;
            cap = most.max(1);
        }

        Self {
            flags,
            elements: u32::try_from(options.limits.elements).unwrap_or(u32::MAX),
            sample: 0,
            rows,
            slow_ns,
            cap,
            examples,
        }
    }
}

const FUNCTION_SLOTS: usize = 32;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Attachment {
    pub functions: Vec<String>,
    pub reason: String,
}

impl Attachment {
    #[must_use]
    pub const fn name(&self) -> &'static str {
        if self.functions.is_empty() {
            "tp_btf"
        } else {
            "fentry"
        }
    }
}

fn function_of(call: SyscallId, has: &impl Fn(&str) -> bool) -> Option<String> {
    call.kernel_function().filter(|function| has(function))
}

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

fn load_object(btf: &Btf, rows: u32) -> Result<Ebpf, BpfError> {
    let cpus = u32::try_from(aya::util::nr_cpus().map_err(|(_, error)| load_error(error))?)
        .unwrap_or(u32::MAX);
    EbpfLoader::new()
        .btf(Some(btf))
        .map_max_entries("EVENTS", cpus)
        .map_max_entries("STATS", rows * 2)
        .override_global(
            "loop_helper",
            &crate::backend::linux::bpf::loop_helper(),
            true,
        )
        .allow_unsupported_maps()
        .load(OBJECT)
        .map_err(load_error)
}

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

const PID_LIMIT: u32 = 1 << 22;

struct Targets(Array<MapData, u64>);

impl Targets {
    fn word(&self, process: u32) -> u64 {
        assert!(process < PID_LIMIT, "the kernel gave out pid {process}");
        self.0.get(&(process / 64), 0).unwrap_or(0)
    }

    fn holds(&self, process: u32) -> bool {
        self.word(process) >> (process % 64) & 1 == 1
    }

    fn first(&mut self, process: u32) -> Result<(), BpfError> {
        let word = self.word(process);
        assert_eq!(word, 0, "a process was traced before the first");
        self.0
            .set(process / 64, 1 << (process % 64), 0)
            .map_err(load_error)
    }

    fn add(&mut self, process: u32) -> Result<(), BpfError> {
        let word = self.word(process);
        self.0
            .set(process / 64, word | 1 << (process % 64), 0)
            .map_err(load_error)
    }
}

struct Loaded {
    ebpf: Ebpf,
    targets: Targets,

    rings: Vec<RingBuf<MapData>>,

    rows: Vec<SyscallId>,

    fence: (
        ArrayOfMaps<MapData, Array<MapData, u32>>,
        Array<MapData, u32>,
    ),

    unexeced: Array<MapData, u32>,

    generation: u32,
    attachment: Attachment,

    base: RawConfig,
    specs: Vec<(u32, RawCallSpec)>,
    shedding: Shed,

    runs: u64,
    estimated: u64,
}

impl Loaded {
    fn load(options: &TraceOptions, engine: BpfOptions, launch: bool) -> Result<Self, BpfError> {
        let _span = crate::span!("syscalls bpf load");
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

        let targets = Targets(
            Array::try_from(ebpf.take_map("TARGETS").ok_or_else(|| missing("TARGETS"))?)
                .map_err(load_error)?,
        );
        let fence = (
            ArrayOfMaps::try_from(ebpf.take_map("FENCE").ok_or_else(|| missing("FENCE"))?)
                .map_err(load_error)?,
            Array::try_from(
                ebpf.take_map("FENCE_TOKEN")
                    .ok_or_else(|| missing("FENCE_TOKEN"))?,
            )
            .map_err(load_error)?,
        );
        let unexeced = Array::try_from(
            ebpf.take_map("UNEXECED")
                .ok_or_else(|| missing("UNEXECED"))?,
        )
        .map_err(load_error)?;
        record!("syscalls bpf: loaded and attached");

        Ok(Self {
            ebpf,
            targets,
            rings,
            fence,
            unexeced,
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

    fn runs(&self) -> u64 {
        self.ebpf
            .map("RUNS")
            .and_then(|map| PerCpuArray::<_, u64>::try_from(map).ok())
            .and_then(|runs| runs.get(&0, 0).ok())
            .map_or(0, |values| values.iter().sum())
    }

    const fn kernel_cost(&mut self, runs: u64) -> u64 {
        self.estimated += runs.saturating_sub(self.runs) * per_call(self.base.flags, self.shedding);
        self.runs = runs;
        self.estimated
    }

    fn target(&mut self, process: u32) -> Result<(), BpfError> {
        record!("syscalls bpf: trace {process}");
        self.targets.first(process)
    }

    fn target_from_exec(&mut self, process: u32) -> Result<(), BpfError> {
        self.unexeced.set(0, process, 0).map_err(load_error)?;
        self.target(process)
    }

    fn also_target(&mut self, process: u32) -> Result<(), BpfError> {
        record!("syscalls bpf: also trace {process}");
        self.targets.add(process)
    }

    fn in_flight(&mut self, after: u64) -> Vec<InFlight> {
        self.in_flight_stacked(after)
            .into_iter()
            .map(|(call, _)| call)
            .collect()
    }

    // Each call in flight, and whether its entry took a stack.
    fn in_flight_stacked(&mut self, after: u64) -> Vec<(InFlight, bool)> {
        let listed = match self.list_in_flight() {
            Ok(listed) => listed,
            #[cfg_attr(not(debug_assertions), expect(unused_variables))]
            Err(error) => {
                record!("syscalls bpf: the in-flight scan failed: {error}");
                Vec::new()
            }
        };

        let now = monotonic();
        let mut calls: Vec<(InFlight, bool)> = listed
            .as_chunks::<{ size_of::<RawInFlightRecord>() }>()
            .0
            .iter()
            .filter_map(|bytes| {
                let record = RawInFlightRecord::read_from_bytes(bytes).ok()?;
                let entry = record.entry;
                let call = InFlight {
                    thread: Tid::new(record.tid),
                    process: Pid::new(entry.tgid),
                    call: SyscallId::new(abi_of(entry.abi)?, entry.nr),
                    args: entry.args,
                    entered: entry.entered,
                };
                (now.saturating_sub(entry.entered) >= after).then_some((call, entry.stacked != 0))
            })
            .collect();
        calls.sort_by_key(|(call, _)| (call.entered, call.thread));

        calls
    }

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

    fn unfinished(&mut self) -> Vec<(SyscallEvent, bool)> {
        self.in_flight_stacked(0)
            .into_iter()
            .map(|(call, stacked)| {
                (
                    SyscallEvent {
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
                    },
                    stacked,
                )
            })
            .collect()
    }

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

    fn skipped(&self) -> u64 {
        self.fdinfo_sum("recursion_misses:", |name| !call_program(name))
    }

    fn fdinfo_sum(&self, field: &str, chosen: impl Fn(&str) -> bool) -> u64 {
        self.ebpf
            .programs()
            .filter(|(name, _)| chosen(name))
            .filter_map(|(_, program)| fdinfo(program.fd().ok()?.as_fd(), field))
            .sum()
    }

    fn verified(&self) -> Vec<(String, u64)> {
        self.ebpf
            .programs()
            .filter_map(|(name, program)| {
                let verified = fdinfo(program.fd().ok()?.as_fd(), "verified_insns:")?;
                Some((name.to_owned(), verified))
            })
            .collect()
    }

    fn counted(&self, process: Pid) -> Folded {
        self.half(self.generation, process)
    }

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

#[must_use]
pub fn fdinfo(fd: std::os::fd::BorrowedFd<'_>, field: &str) -> Option<u64> {
    use std::os::fd::AsRawFd as _;
    std::fs::read_to_string(format!("/proc/self/fdinfo/{}", fd.as_raw_fd()))
        .ok()?
        .lines()
        .find_map(|line| line.strip_prefix(field))
        .and_then(|count| count.trim().parse().ok())
}

pub fn verified_instructions(calls: CallSet) -> Result<Vec<(String, u64)>, BpfError> {
    Probe::read().check()?;
    let options = TraceOptions {
        calls,
        limits: Limits::default(),
        verbose: false,
        follow: true,
        stacks: None,
    };
    let loaded = Loaded::load(&options, BpfOptions::default(), false)?;
    Ok(loaded.verified())
}

fn missing(name: &str) -> BpfError {
    BpfError::Load(format!("the BPF object has no {name}"))
}

fn wait(
    rings: &[impl AsFd],
    live: &mut BTreeMap<Pid, Option<OwnedFd>>,
    tick: u16,
) -> Result<(), BpfError> {
    let waiting: Vec<Pid> = live
        .iter()
        .filter(|(_, exits)| exits.is_some())
        .map(|(&pid, _)| pid)
        .collect();
    let mut fds: Vec<_> = rings
        .iter()
        .map(AsFd::as_fd)
        .chain(live.values().flatten().map(AsFd::as_fd))
        .map(|fd| nix::poll::PollFd::new(fd, nix::poll::PollFlags::POLLIN))
        .collect();
    debug_assert_eq!(fds.len(), rings.len() + waiting.len());
    match nix::poll::poll(&mut fds, nix::poll::PollTimeout::from(tick)) {
        Ok(_) | Err(nix::errno::Errno::EINTR) => {}
        Err(error) => return Err(BpfError::System(error)),
    }

    let exited: Vec<Pid> = fds
        .iter()
        .skip(rings.len())
        .zip(waiting)
        .filter(|(fd, _)| fd.any().unwrap_or(true))
        .map(|(_, pid)| pid)
        .collect();
    drop(fds);
    for pid in exited {
        live.insert(pid, None);
    }
    Ok(())
}

fn exits(process: u32) -> Option<OwnedFd> {
    let pid = rustix::process::Pid::from_raw(i32::try_from(process).ok()?)?;
    rustix::process::pidfd_open(pid, rustix::process::PidfdFlags::empty()).ok()
}

fn ended(process: u32) -> bool {
    std::fs::read_to_string(format!("/proc/{process}/stat")).map_or(true, |stat| {
        stat.rsplit_once(") ").is_some_and(|(_, rest)| {
            (rest.starts_with('Z') || rest.starts_with('X'))
                && rest.split_whitespace().nth(17) == Some("1")
        })
    })
}

pub fn trace_running(
    pid: u32,
    options: &TraceOptions,
    engine: BpfOptions,
    stop: &AtomicBool,
    sink: &mut dyn Sink,
) -> Result<Finished, BpfError> {
    run(Start::Attach(pid), options, engine, stop, sink)
}

pub fn trace_held(
    pid: u32,
    options: &TraceOptions,
    engine: BpfOptions,
    stop: &AtomicBool,
    sink: &mut dyn Sink,
) -> Result<Finished, BpfError> {
    run(Start::Held(pid), options, engine, stop, sink)
}

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

struct Begun {
    first: Pid,
    live: BTreeMap<Pid, Option<OwnedFd>>,
    mappings: Vec<perf::Events>,
}

fn begin(
    start: Start,
    loaded: &mut Loaded,
    options: &TraceOptions,
    sink: &mut dyn Sink,
) -> Result<Begun, BpfError> {
    let mut live = BTreeMap::new();
    let mut mappings = Vec::new();
    let first = match start {
        Start::Held(pid) => {
            loaded.target_from_exec(pid)?;
            if options.stacks.is_some() {
                mappings.push(perf::Events::open(pid, &MAPPINGS).map_err(BpfError::Mappings)?);
            }

            seed_descriptors(pid, sink);
            live.insert(Pid::new(pid), exits(pid));
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

            let mut children = BTreeSet::new();
            loop {
                let found: Vec<u32> = perf::descendants(pid)
                    .into_iter()
                    .filter(|child| !children.contains(child))
                    .collect();
                if found.is_empty() {
                    break;
                }
                for child in found {
                    loaded.also_target(child)?;
                    children.insert(child);
                }
            }

            for process in std::iter::once(pid).chain(children) {
                live.insert(Pid::new(process), exits(process));
                sink.attached(process);
                if let Some(stacks) = &options.stacks {
                    stacks
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .read_maps(Pid::new(process));
                }
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

const MAPPINGS: perf::EventOptions = perf::EventOptions {
    source: perf::Source::Mappings,
    kernel: false,
    stack_bytes: 0,
    ring_pages: 64,
    switches: false,
};

const MAPPING_EVENTS: usize = 65_536;

struct Reader<'a> {
    sink: &'a mut dyn Sink,
    follow: bool,

    live: BTreeMap<Pid, Option<OwnedFd>>,
    first: Pid,
    end: Option<ProcessEnd>,

    entries: BTreeMap<Tid, SyscallEvent>,

    stacks: Option<super::stacks::SharedStacks>,

    pairs: Pairs,

    layouts: Option<PythonLayouts>,
    v8_layouts: Option<V8Layouts>,
    laid_out: BTreeSet<Pid>,

    mappings: Vec<perf::Events>,
    batch: Vec<Record>,
    perf: Vec<u8>,
    lost: [u64; 4],

    untrusted: u64,

    unwinder: Option<super::unwinder::Unwinder>,

    pending: std::collections::VecDeque<Pending>,

    merge: Merge<Record>,

    arrived: std::collections::VecDeque<super::unwinder::Done>,
    next_job: u64,
}

struct Pending {
    event: TraceEvent,

    waiting: Option<(u64, Option<Vec<u8>>)>,
}

#[derive(Clone, Copy)]
enum Until {
    Waiting,

    Fewer(usize),

    None,
}

enum Unwound {
    Now(Option<CallStack>),
    Later(u64, Option<Vec<u8>>),
}

const TICK_MS: u16 = 100;

const WAITING_TICK_MS: u16 = 10;

const STATS_NS: u64 = 1_000_000_000;

const MOST_DRAINED: usize = 4096;

const SETTLED_NS: u64 = 5_000_000;

const STACK_SETTLE_NS: u64 = 100_000_000;

const MOST_PENDING: usize = 2048;

impl Reader<'_> {
    fn run(&mut self, loaded: &mut Loaded, stop: &AtomicBool) -> Result<(), BpfError> {
        let mut rings = std::mem::take(&mut loaded.rings);
        self.merge = Merge::new(rings.len());

        if self.stacks.is_some() && sampler::check_namespace().is_ok() {
            self.layouts = Some(PythonLayouts::take(&mut loaded.ebpf).map_err(load_error)?);
            self.v8_layouts = Some(V8Layouts::take(&mut loaded.ebpf).map_err(load_error)?);
        }
        let mut stopping = false;
        let mut last_tick = monotonic();
        let mut last_stats = last_tick;
        loop {
            if stop.load(Ordering::Relaxed) && !stopping {
                stopping = true;
                record!("syscalls bpf: stop requested");
                self.live.clear();
            }

            let gone: Vec<Pid> = self
                .live
                .iter()
                .filter(|(_, exits)| exits.is_none())
                .map(|(&pid, _)| pid)
                .filter(|&pid| {
                    ended(pid.get())
                        && (!loaded.targets.holds(pid.get())
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

                self.report_lost(loaded, monotonic());
                break;
            }

            let now = monotonic();
            if now - last_tick >= u64::from(TICK_MS) * 1_000_000 {
                last_tick = now;
                self.release(Until::None);
                if now - last_stats >= STATS_NS {
                    last_stats = now;
                    self.report_lost(loaded, now);
                }
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

            let tick = if self.pending.is_empty() && !self.merge.holds() {
                TICK_MS
            } else {
                WAITING_TICK_MS
            };
            wait(&rings, &mut self.live, tick)?;
        }

        self.release(Until::None);
        self.report_lost(loaded, monotonic());

        // on_enter keeps a call's entry before it sends the entry's stack, so a
        // call that began as the trace stopped can be in flight with its stack
        // still on the way. Drain until each such stack is read, or a while.
        let unfinished = loaded.unfinished();
        let until = monotonic().saturating_add(STACK_SETTLE_NS);
        loop {
            self.drain(&mut rings, true);
            let arrived = unfinished.iter().all(|(call, stacked)| {
                !stacked || self.pairs.has_waiting(call.thread, call.entered)
            });
            if arrived || monotonic() >= until {
                break;
            }
            std::thread::yield_now();
        }
        for (call, _) in unfinished {
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

    fn map(&mut self) {
        let Some(stacks) = self.stacks.clone() else {
            return;
        };

        self.perf.clear();
        for events in &mut self.mappings {
            events.drain(&mut self.perf);
        }

        let untrusted = self.mappings.iter().map(perf::Events::untrusted).sum();
        if untrusted > self.untrusted {
            self.emit(TraceEvent::Lost {
                time: monotonic(),
                count: untrusted - self.untrusted,
                why: Loss::Dropped,
            });
            self.untrusted = untrusted;
        }

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
        if !records.is_empty() {
            self.release(Until::None);
        }
        let mut stacks = stacks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
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
                #[cfg_attr(not(debug_assertions), expect(unused_variables))]
                Err(why) => record!("syscalls bpf: runtime of {process}: {why}"),
            }
        }

        drop(stacks);
    }

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
            self.unwinder = None;
            Unwound::Now(Some(lost_stack()))
        }
    }

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
                        self.emit(TraceEvent::Call(entry));
                    }
                }

                self.emit_unwound(TraceEvent::Call(call), unwound);
            }
            Record::Event(mut event) => {
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
                        let process = Pid::led_by(*child);
                        self.live.insert(process, exits(process.get()));
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
                    Unwound::Now(None) => self.emit(event),
                    unwound => self.emit_unwound(event, unwound),
                }
            }
        }
    }
}

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

    #[test]
    fn a_traced_process_that_exits_wakes_the_reader_to_check_its_end() {
        let mut running = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .expect("start sleep");
        let mut done = std::process::Command::new("true")
            .spawn()
            .expect("start true");
        let (running_pid, done_pid) = (Pid::new(running.id()), Pid::new(done.id()));
        let mut live = BTreeMap::from([
            (running_pid, exits(running.id())),
            (done_pid, exits(done.id())),
        ]);
        assert!(live.values().all(Option::is_some));

        let began = std::time::Instant::now();
        wait(&[] as &[OwnedFd], &mut live, 10_000).expect("poll");
        let waited = began.elapsed();

        assert!(waited < std::time::Duration::from_secs(5), "{waited:?}");
        assert!(live[&done_pid].is_none() && ended(done.id()));
        assert!(live[&running_pid].is_some() && !ended(running.id()));
        let _ = running.kill();
        let _ = running.wait();
        let _ = done.wait();
    }

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
                [flags, elements, sample, rows, slow_ns, cap, examples]
            ),
            (
                RawEntry,
                "entry",
                [
                    entered, args, nr, tgid, exec_tid, abi, flags, stacked, known, capped, pad
                ]
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

        for flags in [CALL_PROCESS_EXITED, CALL_EXIT_SKIPPED] {
            let Ok(Record::Call(call)) = decode(&call_bytes(RECORD_CALL, flags, &[])) else {
                panic!("a call");
            };
            assert_eq!(
                (
                    call.outcome,
                    call.entered.is_some(),
                    call.exited,
                    call.flags
                ),
                (Outcome::Unavailable, true, None, EventFlags::default()),
                "{flags}"
            );
        }

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
