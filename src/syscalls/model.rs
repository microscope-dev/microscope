use std::sync::Arc;

use serde::{Deserialize, Serialize};

use super::table;
pub use crate::model::{Pid, Tid};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Abi {
    X86_64,

    X86_32,

    X32,
}

impl Abi {
    pub const ALL: [Self; 3] = [Self::X86_64, Self::X86_32, Self::X32];

    #[must_use]
    pub const fn from_index(index: u8) -> Self {
        match index {
            1 => Self::X86_32,
            2 => Self::X32,
            _ => Self::X86_64,
        }
    }

    #[must_use]
    pub const fn suffix(self) -> &'static str {
        match self {
            Self::X86_64 => "64",
            Self::X86_32 => "32",
            Self::X32 => "x32",
        }
    }

    #[must_use]
    pub fn from_suffix(suffix: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|abi| abi.suffix() == suffix)
    }

    pub(super) const fn of<T>(self, table: &[T; 3]) -> &T {
        match self {
            Self::X86_64 => &table[0],
            Self::X86_32 => &table[1],
            Self::X32 => &table[2],
        }
    }

    pub(super) const fn of_mut<T>(self, table: &mut [T; 3]) -> &mut T {
        match self {
            Self::X86_64 => &mut table[0],
            Self::X86_32 => &mut table[1],
            Self::X32 => &mut table[2],
        }
    }

    #[must_use]
    pub const fn word_bytes(self) -> u8 {
        match self {
            Self::X86_64 => 8,
            Self::X86_32 | Self::X32 => 4,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct SyscallId {
    pub abi: Abi,
    pub number: u32,
}

impl SyscallId {
    #[must_use]
    pub const fn new(abi: Abi, number: u32) -> Self {
        Self { abi, number }
    }

    #[must_use]
    pub fn name(self) -> Option<&'static str> {
        table::name(self)
    }

    #[must_use]
    pub fn display_name(self) -> std::borrow::Cow<'static, str> {
        self.name().map_or_else(
            || format!("syscall_{:#x}", self.number).into(),
            std::borrow::Cow::Borrowed,
        )
    }

    #[must_use]
    pub fn kernel_function(self) -> Option<String> {
        table::kernel_function(self)
    }

    #[must_use]
    pub fn named(abi: Abi, name: &str) -> Option<Self> {
        table::number(abi, name).map(|number| Self::new(abi, number))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct Errno(pub u16);

pub const MAX_ERRNO: i64 = 4095;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Outcome {
    Returned(i64),

    Failed(Errno),

    Unfinished,

    ProcessExited,

    Unavailable,
}

impl Outcome {
    #[must_use]
    pub fn of_return(value: i64) -> Self {
        if (-MAX_ERRNO..0).contains(&value) {
            #[expect(
                clippy::cast_possible_truncation,
                clippy::cast_sign_loss,
                reason = "the range checked above fits"
            )]
            Self::Failed(Errno((-value) as u16))
        } else {
            Self::Returned(value)
        }
    }

    #[must_use]
    pub const fn errno(self) -> Option<Errno> {
        match self {
            Self::Failed(errno) => Some(errno),
            _ => None,
        }
    }

    #[must_use]
    pub fn raw(self) -> Option<i64> {
        match self {
            Self::Returned(value) => Some(value),
            Self::Failed(Errno(errno)) => Some(-i64::from(errno)),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ReadAt {
    Entry,

    Exit,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CaptureState {
    Complete,

    Truncated { wanted: u32 },

    Unreadable,

    NotRequested,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Capture {
    pub arg: u8,
    pub when: ReadAt,
    pub state: CaptureState,
    pub bytes: Arc<[u8]>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct EventFlags(pub u8);

impl EventFlags {
    pub const SPANNED_STOP: Self = Self(1);

    pub const ENTRY_UNSEEN: Self = Self(2);

    pub const ABI_UNCERTAIN: Self = Self(4);

    pub const KEPT_FAILED: Self = Self(64);
    pub const KEPT_SLOW: Self = Self(128);

    #[must_use]
    pub const fn kept(self) -> bool {
        self.0 & (Self::KEPT_FAILED.0 | Self::KEPT_SLOW.0) != 0
    }

    #[must_use]
    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    pub const fn insert(&mut self, other: Self) {
        self.0 |= other.0;
    }

    #[must_use]
    pub const fn with(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Loss {
    Dropped,

    Unseen,

    Capped,

    Overflowed,
}

impl Loss {
    pub const ALL: [Self; 4] = [Self::Dropped, Self::Unseen, Self::Capped, Self::Overflowed];
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Losses {
    pub dropped: u64,
    pub unseen: u64,
    pub capped: u64,
    pub overflowed: u64,
}

impl Losses {
    pub const fn add(&mut self, why: Loss, count: u64) {
        match why {
            Loss::Dropped => self.dropped += count,
            Loss::Unseen => self.unseen += count,
            Loss::Capped => self.capped += count,
            Loss::Overflowed => self.overflowed += count,
        }
    }

    #[must_use]
    pub const fn any(&self) -> bool {
        self.dropped + self.unseen + self.capped + self.overflowed > 0
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CallStack {
    pub addresses: Arc<[u64]>,

    pub truncated: Option<u8>,

    #[serde(default)]
    pub interpreted: Arc<[InterpretedCall]>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InterpretedCall {
    pub before: u32,
    pub function: Arc<str>,
    pub file: Option<Arc<str>>,

    pub line: u32,

    pub first_line: Option<u32>,
}

impl CallStack {
    pub fn frames(&self) -> impl Iterator<Item = Result<usize, &InterpretedCall>> {
        let mut interpreted = self.interpreted.iter().peekable();
        (0..=self.addresses.len()).flat_map(move |index| {
            let mut before = Vec::new();

            while let Some(call) = interpreted.next_if(|call| call.before as usize <= index) {
                before.push(Err(call));
            }

            before
                .into_iter()
                .chain((index < self.addresses.len()).then_some(Ok(index)))
        })
    }

    #[must_use]
    pub fn truncation(&self) -> Option<crate::capture::records::Truncation> {
        crate::capture::records::Truncation::from_number(self.truncated?.into())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SyscallEvent {
    pub thread: Tid,

    pub process: Pid,
    pub call: SyscallId,

    pub args: [u64; 6],

    pub captured: Vec<Capture>,
    pub outcome: Outcome,

    pub entered: Option<u64>,

    pub exited: Option<u64>,

    pub stack: Option<CallStack>,
    pub flags: EventFlags,
}

impl SyscallEvent {
    #[must_use]
    pub fn duration(&self) -> Option<u64> {
        Some(self.exited?.saturating_sub(self.entered?))
    }

    #[must_use]
    pub fn time(&self) -> u64 {
        self.exited.or(self.entered).unwrap_or(0)
    }

    #[must_use]
    pub fn capture(&self, arg: u8) -> Option<&Capture> {
        self.captured.iter().find(|capture| capture.arg == arg)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ProcessEnd {
    Exited(i32),
    Killed { signal: i32, core: bool },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum TraceEvent {
    Call(SyscallEvent),

    Signal {
        thread: Tid,
        process: Pid,
        time: u64,
        signal: i32,

        code: Option<i32>,

        sender: Option<Pid>,

        info: Option<Arc<[u8]>>,

        stack: Option<CallStack>,
    },

    Spawn {
        parent_thread: Tid,
        parent: Pid,

        child: Tid,

        thread: bool,
        time: u64,
    },

    Exec {
        thread: Tid,
        process: Pid,
        time: u64,
        path: Arc<str>,
    },

    Exit {
        process: Pid,
        time: u64,
        end: ProcessEnd,
    },

    ThreadExit {
        thread: Tid,
        process: Pid,
        time: u64,
        end: ProcessEnd,
    },

    Lost {
        time: u64,
        count: u64,
        why: Loss,
    },

    Descriptor {
        process: Pid,

        fd: i64,
        time: u64,

        text: Arc<str>,
        cloexec: bool,

        device: Option<Device>,

        made: bool,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Device {
    pub block: bool,
    pub major: u32,
    pub minor: u32,
}

impl TraceEvent {
    #[must_use]
    pub fn time(&self) -> u64 {
        match self {
            Self::Call(call) => call.time(),
            Self::Signal { time, .. }
            | Self::Spawn { time, .. }
            | Self::Exec { time, .. }
            | Self::Exit { time, .. }
            | Self::ThreadExit { time, .. }
            | Self::Lost { time, .. }
            | Self::Descriptor { time, .. } => *time,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct InFlight {
    pub thread: Tid,
    pub process: Pid,
    pub call: SyscallId,
    pub args: [u64; 6],

    pub entered: u64,
}

impl InFlight {
    #[must_use]
    pub fn unfinished(&self) -> SyscallEvent {
        SyscallEvent {
            thread: self.thread,
            process: self.process,
            call: self.call,
            args: self.args,
            captured: Vec::new(),
            outcome: Outcome::Unfinished,
            entered: Some(self.entered),
            exited: None,
            stack: None,
            flags: EventFlags::default(),
        }
    }
}
