//! A sampling profiler: records where a program's threads spend their
//! time, from outside the program, into a capture that can be saved,
//! loaded, and queried.

mod annotate;
pub mod budget;
mod calls;
mod collector;
mod formats;
mod frame_pointers;
mod interpreted;
mod jit;
mod probes;
mod record;
pub(crate) use record::{OwnedLine, hex, symbolize as symbolize_lines};

mod segments;
mod session;
mod symbols;
mod syscalls;
pub use syscalls::{RecordedEvent, RecordedSyscalls, SyscallPlace};
mod tasks;
pub(crate) use interpreted::{Sampled, positions, read_record};
pub(crate) use tasks::{InterpretedWait, LiveStop};
mod zones;

pub use crate::capture::records::GapReason;

/// The task row of a sample no task snapshot named, as queries and lanes
/// give it.
pub const NO_TASK: u32 = NONE;

/// A key no path holds: a focus on it keeps nothing.
pub const NO_KEY: u32 = NONE;
pub use crate::capture::records::MarkerKind;
pub use annotate::{CodeBlock, Disassembly, InstructionCost, LineCost, LineCosts};
pub use collector::ProbeSelection;
pub use formats::Format;
pub use session::{Attachment, Recording, Save, Watch, record, record_attached, record_watched};
pub use symbols::ModuleSymbols;
pub use zones::{
    PlotValue, ProbeCounts, ProfileZone, ProgramMarker, ZONE_BEGAN_UNSEEN, ZONE_ENDED_UNSEEN,
    ZONE_MISMATCHED, ZoneSite,
};

pub use crate::analysis::{
    CallTree, Cost, Costs, DiffCost, DiffTree, Granularity, KeyKind, Keys, Lanes, Look, OffCpuLane,
    PlotBin, PlotSeries, Query, Side, Source, Transform, ZoneAt, ZoneBlock, ZoneStats, diff_costs,
    diff_trees,
};

use std::path::{Path, PathBuf};

use crate::backend::perf;
use crate::capture::records::{AccountKind, FrameKind, NONE, Truncation};
use crate::capture::{Capture, file};

/// How much a recording costs the program, and how much it sees.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Preset {
    /// For production: 49 samples a second and a small stack copy.
    Production,

    /// For development: 999 samples a second and a stack copy deep enough
    /// to unwind code without frame pointers.
    #[default]
    Development,

    /// The most the kernel allows.
    Maximum,
}

impl Preset {
    /// Samples per second of CPU time, per thread.
    #[must_use]
    pub const fn rate(self) -> u32 {
        match self {
            Self::Production => 49,
            Self::Development => 999,
            Self::Maximum => 9999,
        }
    }

    /// Whether a recording says when each thread leaves its CPU and comes
    /// back: a record of every context switch, which a program that
    /// switches often pays for.
    #[must_use]
    pub const fn off_cpu(self) -> bool {
        !matches!(self, Self::Production)
    }

    /// The CPU time microscope may spend recording: in production, one
    /// percent of a CPU; elsewhere, whatever it takes.
    #[must_use]
    pub const fn budget(self) -> Option<budget::Budget> {
        match self {
            Self::Production => Some(budget::Budget::PRODUCTION),
            Self::Development | Self::Maximum => None,
        }
    }

    /// How much stack each sample copies.
    #[must_use]
    pub const fn stack_copy(self) -> StackCopy {
        match self {
            Self::Production => StackCopy::Auto,
            Self::Development | Self::Maximum => StackCopy::Bytes(DEEP_STACK_COPY),
        }
    }
}

/// Bytes of stack a sample copies where the frame-pointer chain carries
/// stacks: enough to unwind the innermost frames exactly.
pub const SMALL_STACK_COPY: u32 = 512;

/// Bytes of stack a sample copies where call-frame information alone
/// unwinds: deep enough for most stacks.
pub const DEEP_STACK_COPY: u32 = 16 * 1024;

/// How much stack each sample copies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StackCopy {
    /// The least that unwinds the program: [`SMALL_STACK_COPY`] when its
    /// code keeps the frame-pointer chain where it calls, which a program
    /// linked statically shows before it runs, and [`DEEP_STACK_COPY`]
    /// otherwise. The profile says which, and why.
    Auto,

    /// This many bytes, rounded up to a multiple of eight.
    Bytes(u32),
}

/// What makes a sample.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SampleSource {
    /// CPU time, at `rate` samples a second.
    Clock { rate: u32 },

    /// Each execution of the first instruction of the function a symbol of
    /// the program names. The program runs without address randomization.
    Function { symbol: String },
}

impl SampleSource {
    /// What a sample weighs: nanoseconds of CPU time, or one execution.
    #[must_use]
    pub const fn unit(&self) -> &'static str {
        match self {
            Self::Clock { .. } => "nanoseconds",
            Self::Function { .. } => "executions",
        }
    }
}

impl std::fmt::Display for SampleSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Clock { rate } => write!(f, "CPU time at {rate} Hz"),
            Self::Function { symbol } => write!(f, "each call of {symbol}"),
        }
    }
}

/// How a recording reads where a Python program's threads are. Each is
/// chosen; a recording never switches from one to another by itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PythonEngine {
    /// A BPF program walks each sampled thread's Python frames as the
    /// sample is taken, which then holds them among its native frames: it
    /// needs `CAP_BPF` and `CAP_PERFMON`.
    Bpf,

    /// Stops the program this often, through the debugger's attach, and
    /// reads each thread's stack, its Python frames among its native ones,
    /// as a task of its own, with the labels its runtime gives it.
    Snapshot { every: std::time::Duration },

    /// Reads each thread's Python frames this often as the program runs,
    /// leaving out, and counting, each thread whose frames change as they
    /// are read.
    Live { every: std::time::Duration },
}

impl std::fmt::Display for PythonEngine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Bpf => write!(f, "with BPF at each sample"),
            Self::Snapshot { every } => {
                write!(f, "stopped every {:.3} s", every.as_secs_f64())
            }
            Self::Live { every } => {
                write!(f, "as the program runs, every {:.3} s", every.as_secs_f64())
            }
        }
    }
}

/// How to record.
#[derive(Debug, Clone, PartialEq)]
pub struct RecordOptions {
    pub source: SampleSource,
    pub stack: StackCopy,

    /// The most frames a stack keeps, at least one.
    pub max_frames: usize,

    /// Pages each CPU's ring holds: the most a burst of samples can write
    /// before they are drained. Fewer lose more samples, every one counted.
    pub ring_pages: usize,

    /// The most events an attach may open: one per thread per CPU.
    pub max_events: usize,

    /// Whether to record each interval a thread spends off its CPU.
    pub off_cpu: bool,

    /// The CPU time microscope may spend recording, which it holds to by
    /// sampling less of each period: none bounds it.
    pub budget: Option<budget::Budget>,

    /// Keeps only what happened in the last span this long, which a
    /// trigger saves: a recording's memory then holds no more.
    pub window: Option<std::time::Duration>,

    /// Cuts the recording into segments this long, without a window: each
    /// is a complete profile of its span alone, which the watch's save
    /// takes as it ends, or early when triggered. The recording's own
    /// profile is the last.
    pub segment: Option<std::time::Duration>,

    /// How often to stop the program, through the debugger's attach, to
    /// read where its tasks wait: none never does.
    pub task_snapshots: Option<std::time::Duration>,

    /// Whether task snapshots read the program as it runs, without
    /// stopping it, leaving out each task that changes while read.
    pub live_snapshots: bool,

    /// How to read where a Python program's threads are, beside its
    /// samples: none never does.
    pub python: Option<PythonEngine>,

    /// The instrumentation sites to switch on in a program built with
    /// microscope's probes SDK.
    pub probes: ProbeSelection,

    /// How long a program goes on writing its instrumentation once microscope
    /// stops reading it, as when microscope is killed, before it switches
    /// every site off.
    pub probe_timeout: std::time::Duration,

    /// Directories searched for separate debug files.
    pub debug_directories: Vec<PathBuf>,

    /// Whether to leave symbolizing to whoever opens the profile, which
    /// then holds only modules' build ids and addresses: for hosts that
    /// keep no debug information.
    pub symbols_later: bool,

    /// The system calls to trace beside the samples, with the BPF engine,
    /// each with its stack: a program the recording starts only.
    pub syscalls: Option<crate::syscalls::CallSet>,
}

impl RecordOptions {
    #[must_use]
    pub const fn preset(preset: Preset) -> Self {
        Self {
            source: SampleSource::Clock {
                rate: preset.rate(),
            },
            stack: preset.stack_copy(),
            max_frames: 512,
            ring_pages: 128,
            max_events: 16_384,
            off_cpu: preset.off_cpu(),
            budget: preset.budget(),
            window: None,
            segment: None,
            task_snapshots: None,
            live_snapshots: false,
            python: None,
            probes: ProbeSelection::All,
            probe_timeout: std::time::Duration::from_secs(10),
            debug_directories: Vec::new(),
            symbols_later: false,
            syscalls: None,
        }
    }
}

impl Default for RecordOptions {
    fn default() -> Self {
        Self::preset(Preset::default())
    }
}

/// Why a recording failed.
#[derive(Debug, thiserror::Error)]
pub enum RecordError {
    #[error(transparent)]
    Launch(#[from] perf::LaunchError),

    #[error(transparent)]
    Attach(#[from] perf::OpenError),

    #[error("starting the program failed: {0}")]
    Start(std::io::Error),

    #[error("tracing system calls failed: {0}")]
    Syscalls(String),

    #[error("the program has no function named '{0}'")]
    NoSuchFunction(String),

    #[error("{count} functions are named '{name}'; name one by its symbol")]
    AmbiguousFunction { name: String, count: usize },

    #[error("the calls of '{name}' cannot be counted exactly: {why}")]
    Uncountable { name: String, why: String },

    #[error("reading the program failed: {0}")]
    Program(String),

    #[error("{0}")]
    Python(String),

    #[error(
        "the program was not loaded where the breakpoint assumed (bias {expected:#x}, not {actual:#x}): its samples would be of another instruction"
    )]
    Misplaced { expected: u64, actual: u64 },
}

/// A recorded profile.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Profile {
    capture: Capture,
}

/// The query of every sample, seen as `view` shows stacks.
fn query(view: StackView) -> Query {
    Query {
        look: match view {
            StackView::Logical => Look::Logical,
            StackView::Native => Look::Native,
            StackView::Python => Look::Python,
        },
        ..Query::default()
    }
}

/// How a stack ends.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum StackEnding {
    Complete,

    /// Short of its outermost frame, for this reason.
    Truncated(String),
}

/// One sample's stack, as names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SampledStack {
    pub pid: u32,
    pub tid: u32,
    pub time: i64,
    pub weight: u64,

    /// Function names, innermost first, each inlined function a frame of
    /// its own; a frame without symbols is its module's address.
    pub functions: Vec<String>,
    pub ending: StackEnding,

    /// The number of the task it ran, when task snapshots before and after
    /// it agree on which that was.
    pub task: Option<u64>,
}

/// A task of a language runtime that task snapshots found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProfileTask {
    /// Its row, which queries select it by.
    pub row: u32,

    /// What its runtime calls a task, and its number.
    pub noun: String,
    pub number: u64,

    /// The labels the program gave it, such as Go's profiler labels.
    pub labels: Vec<(String, String)>,

    /// The weight of the samples that ran it, and the nanoseconds of
    /// waiting the snapshots found it in.
    pub weight: u64,
    pub waited: u64,
}

/// Where a task waited at a task snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskWaitStack {
    /// What its runtime calls a task, and its number.
    pub noun: String,
    pub task: u64,
    pub time: i64,

    /// The nanoseconds of waiting it stands for: since the snapshot before.
    pub weight: u64,

    /// The runtime's words for what it waits for, or its state.
    pub state: String,

    /// Function names, innermost first, as a sample's are.
    pub functions: Vec<String>,
    pub ending: StackEnding,
}

/// A span a thread spent off its CPU.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OffCpuInterval {
    pub pid: u32,
    pub tid: u32,

    /// When it left its CPU.
    pub time: i64,

    /// Nanoseconds until it ran again, or until the recording ended.
    pub duration: u64,

    /// Whether it left while it could still run, rather than blocking.
    pub preempted: bool,
}

/// What a function costs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FunctionCost {
    pub name: String,

    /// The weight of samples it was the innermost function of.
    pub own: u64,

    /// The weight of samples it was anywhere on the stack of.
    pub total: u64,
}

/// How a profile's stacks are shown.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum StackView {
    /// Each async task's frames as a stack of their own, rooted under the
    /// runtime's dispatch, with wrappers folded away.
    #[default]
    Logical,

    /// The frames as the thread ran them: an interpreter's frames are not
    /// among them, as its own frames that run them stand for them.
    Native,

    /// As [`Self::Logical`], with each stack from its outermost
    /// interpreted frame in, without the interpreter's own frames, which
    /// run them: a stack as a Python programmer reads it, with the native
    /// code it calls.
    Python,
}

/// A thread a profile sampled, or saw leave its CPU.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProfileThread {
    /// The thread's row, by which a [`Query`] selects it.
    pub row: u32,
    pub pid: u32,
    pub tid: u32,

    /// The name it last had, if the kernel reported one.
    pub name: Option<String>,
    pub samples: u64,
    pub weight: u64,

    /// Nanoseconds it spent off its CPU, blocked and preempted.
    pub blocked: u64,
    pub preempted: u64,
}

/// A function called from its parent's node.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallNode {
    pub parent: Option<usize>,
    pub name: String,
    pub own: u64,
    pub total: u64,
}

/// What reading a profile file found.
#[derive(Debug)]
pub struct ReadProfile {
    pub profile: Profile,

    /// Whether the file was cut short, as by a crash, and only its complete
    /// parts were read.
    pub truncated: bool,

    /// The parts read.
    pub chunks: usize,
}

impl Profile {
    /// Reads a profile file, in any format [`Profile::import`] reads, or a
    /// directory of a continuous recording's segments.
    pub fn read(path: &Path) -> Result<ReadProfile, ProfileFileError> {
        if path.is_dir() {
            return Self::read_segments(path);
        }

        Self::import(&std::fs::read(path)?)
    }

    /// Whether `bytes` begin as a profile file does.
    #[must_use]
    pub fn is_profile(bytes: &[u8]) -> bool {
        bytes.starts_with(&file::FILE_MAGIC)
    }

    /// Reads the bytes of a profile file.
    pub fn from_bytes(bytes: &[u8]) -> Result<ReadProfile, ProfileFileError> {
        let read =
            file::read(bytes).map_err(|error| ProfileFileError::Invalid(error.to_string()))?;
        Ok(ReadProfile {
            profile: Self {
                capture: read.capture,
            },
            truncated: read.truncated,
            chunks: read.chunks,
        })
    }

    /// The profile as a file holds it.
    pub fn to_bytes(&self) -> std::io::Result<Vec<u8>> {
        let mut bytes = Vec::new();
        file::write(&self.capture, &mut bytes)?;
        Ok(bytes)
    }

    /// Writes the profile to a file.
    pub fn write(&self, path: &Path) -> std::io::Result<()> {
        let out = std::io::BufWriter::new(std::fs::File::create(path)?);
        file::write(&self.capture, out)
    }

    #[must_use]
    pub const fn samples(&self) -> usize {
        self.capture.samples.len()
    }

    /// A setting the recording recorded, such as `program`.
    #[must_use]
    pub fn setting(&self, key: &str) -> Option<&str> {
        self.capture.setting(key)
    }

    /// Every setting the recording recorded, by key.
    #[must_use]
    pub fn settings(&self) -> Vec<(&str, &str)> {
        let capture = &self.capture;
        capture
            .settings
            .iter()
            .map(|setting| {
                (
                    capture.string(setting.key.get()),
                    capture.string(setting.value.get()),
                )
            })
            .collect()
    }

    /// The first and last times the recording saw, in nanoseconds of
    /// `CLOCK_MONOTONIC`.
    #[must_use]
    pub fn span(&self) -> (i64, i64) {
        self.capture.span()
    }

    /// The weight of every sample: nanoseconds of CPU time for a clock
    /// source, and executions for a function source.
    #[must_use]
    pub fn total_weight(&self) -> u64 {
        self.capture
            .samples
            .iter()
            .map(|sample| sample.weight.get())
            .sum()
    }

    /// The keys frames stand for at `granularity`, through which every
    /// query of the profile runs.
    #[must_use]
    pub fn keys(&self, granularity: Granularity) -> Keys<'_> {
        Keys::new(&self.capture, granularity)
    }

    /// Each thread that was sampled, left its CPU, or recorded zones, by its row, which
    /// queries name.
    #[must_use]
    pub fn threads(&self) -> Vec<ProfileThread> {
        let capture = &self.capture;
        let mut threads = std::collections::BTreeMap::<u32, ProfileThread>::new();
        let new = |row: u32| {
            let thread = &capture.threads[row as usize];
            ProfileThread {
                row,
                pid: capture.processes[thread.process.get() as usize].pid.get(),
                tid: thread.tid.get(),
                name: Some(capture.string(thread.name.get()))
                    .filter(|name| !name.is_empty())
                    .map(str::to_owned),
                samples: 0,
                weight: 0,
                blocked: 0,
                preempted: 0,
            }
        };

        for range in &capture.sample_ranges {
            let samples = capture.thread_samples(range);
            let row = range.thread.get();
            let thread = threads.entry(row).or_insert_with(|| new(row));
            thread.samples = samples.len() as u64;
            thread.weight = samples.iter().map(|sample| sample.weight.get()).sum();
        }

        for range in &capture.zone_ranges {
            let row = range.thread.get();
            threads.entry(row).or_insert_with(|| new(row));
        }

        for range in &capture.off_cpu_ranges {
            let row = range.thread.get();
            let thread = threads.entry(row).or_insert_with(|| new(row));
            for interval in capture.thread_off_cpu(range) {
                let total = if interval.preempted == 0 {
                    &mut thread.blocked
                } else {
                    &mut thread.preempted
                };
                *total += interval.duration.get();
            }
        }

        threads.into_values().collect()
    }

    /// How many times the kernel throttled sampling.
    #[must_use]
    pub fn throttles(&self) -> u64 {
        self.capture
            .account(crate::capture::records::AccountKind::Throttles as u32)
    }

    /// What each function costs, most expensive first.
    #[must_use]
    pub fn top(&self, view: StackView) -> Vec<FunctionCost> {
        self.top_of(view, Source::Samples, Granularity::Function)
    }

    /// What each function, or what `granularity` tells apart, costs of
    /// `source`, most expensive first.
    #[must_use]
    pub fn top_of(
        &self,
        view: StackView,
        source: Source,
        granularity: Granularity,
    ) -> Vec<FunctionCost> {
        let keys = self.keys(granularity);
        keys.functions(&Query {
            source,
            ..query(view)
        })
        .rows
        .into_iter()
        .map(|cost| FunctionCost {
            name: keys.name(cost.key),
            own: cost.own,
            total: cost.total,
        })
        .collect()
    }

    /// The call tree, from the outermost callers in; parents come before
    /// their children.
    #[must_use]
    pub fn call_tree(&self, view: StackView) -> Vec<CallNode> {
        self.call_tree_of(view, Source::Samples, Granularity::Function)
    }

    /// The call tree of `source`, its frames told apart as `granularity`
    /// says, from the outermost callers in; parents come before their
    /// children.
    #[must_use]
    pub fn call_tree_of(
        &self,
        view: StackView,
        source: Source,
        granularity: Granularity,
    ) -> Vec<CallNode> {
        let keys = self.keys(granularity);
        let tree = keys.aggregate(&Query {
            source,
            ..query(view)
        });

        (0..tree.len())
            .map(|node| CallNode {
                parent: (tree.parent[node] != NONE).then_some(tree.parent[node] as usize),
                name: keys.name(tree.key[node]),
                own: tree.own[node],
                total: tree.total[node],
            })
            .collect()
    }

    /// Samples the kernel dropped.
    #[must_use]
    pub fn lost_samples(&self) -> u64 {
        self.capture
            .account(crate::capture::records::AccountKind::LostSamples as u32)
    }

    /// What the events counted, in the weight's unit, sampled or not: the
    /// CPU time the program ran, or each execution of a breakpoint's
    /// instruction. A thread's time on each CPU is sampled only once a whole
    /// period of it has passed there, so a short recording at a low rate
    /// samples less of it.
    #[must_use]
    pub fn counted(&self) -> u64 {
        self.capture
            .account(crate::capture::records::AccountKind::Counted as u32)
    }

    /// Breakpoint samples set aside as repeats within one call.
    #[must_use]
    pub fn repeats(&self) -> u64 {
        self.capture
            .account(crate::capture::records::AccountKind::Repeats as u32)
    }

    /// How many stacks ended short of their outermost frame, by reason.
    #[must_use]
    pub fn truncations(&self) -> Vec<(String, u64)> {
        record::truncation_counts(&self.capture)
            .into_iter()
            .map(|(reason, count)| (reason.describe().to_owned(), count))
            .collect()
    }

    /// How long each thread `query` selects was off its CPU in each of
    /// `buckets` slices of its range.
    #[must_use]
    pub fn off_cpu_lanes(&self, query: &Query, buckets: usize) -> Vec<OffCpuLane> {
        crate::analysis::off_cpu(&self.capture, query, buckets)
    }

    /// Each span the recording did not sample, as its start and end, in
    /// time order.
    #[must_use]
    pub fn gaps(&self) -> Vec<(i64, i64)> {
        self.capture
            .gaps
            .iter()
            .map(|gap| (gap.start.get(), gap.end.get()))
            .collect()
    }

    /// Each span sampling paused for `reason`, in time order.
    #[must_use]
    pub fn paused(&self, reason: GapReason) -> Vec<(i64, i64)> {
        self.capture
            .gaps
            .iter()
            .filter(|gap| gap.reason == reason as u8)
            .map(|gap| (gap.start.get(), gap.end.get()))
            .collect()
    }

    /// Each time the budget changed how much was sampled, and what it says.
    #[must_use]
    pub fn budget_changes(&self) -> Vec<(i64, String)> {
        self.markers(MarkerKind::Budget)
    }

    /// Each time a task snapshot failed or missed tasks, and why.
    #[must_use]
    pub fn task_snapshot_problems(&self) -> Vec<(i64, String)> {
        self.markers(MarkerKind::TaskSnapshot)
    }

    fn markers(&self, kind: MarkerKind) -> Vec<(i64, String)> {
        let capture = &self.capture;
        capture
            .markers
            .iter()
            .filter(|marker| marker.kind == kind as u8)
            .map(|marker| {
                (
                    marker.time.get(),
                    capture.string(marker.text.get()).to_owned(),
                )
            })
            .collect()
    }

    /// Nanoseconds of CPU time microscope spent recording, and the part of it
    /// spent reading programs' debug information.
    #[must_use]
    pub fn recorder_cpu(&self) -> (u64, u64) {
        (
            self.capture.account(AccountKind::RecorderCpu as u32),
            self.capture.account(AccountKind::RecorderLoading as u32),
        )
    }

    /// Every interval a thread spent off its CPU, by thread and then time.
    #[must_use]
    pub fn off_cpu(&self) -> Vec<OffCpuInterval> {
        let capture = &self.capture;
        let mut intervals = Vec::with_capacity(capture.off_cpu.len());

        for range in &capture.off_cpu_ranges {
            let thread = &capture.threads[range.thread.get() as usize];
            let pid = capture.processes[thread.process.get() as usize].pid.get();
            intervals.extend(
                capture
                    .thread_off_cpu(range)
                    .iter()
                    .map(|interval| OffCpuInterval {
                        pid,
                        tid: thread.tid.get(),
                        time: interval.time.get(),
                        duration: interval.duration.get(),
                        preempted: interval.preempted != 0,
                    }),
            );
        }

        intervals
    }

    /// Every sample's stack, by thread and then time.
    #[must_use]
    pub fn stacks(&self) -> Vec<SampledStack> {
        let capture = &self.capture;
        let mut stacks = Vec::with_capacity(capture.samples.len());

        for range in &capture.sample_ranges {
            let thread = &capture.threads[range.thread.get() as usize];
            let tid = thread.tid.get();
            let pid = capture.processes[thread.process.get() as usize].pid.get();
            for sample in capture.thread_samples(range) {
                let (functions, ending) = self.names(sample.stack.get());
                let task = capture
                    .tasks
                    .get(sample.task.get() as usize)
                    .map(|task| task.number.get());
                stacks.push(SampledStack {
                    task,
                    pid,
                    tid,
                    time: sample.time.get(),
                    weight: sample.weight.get(),
                    functions,
                    ending,
                });
            }
        }

        stacks
    }

    /// Every wait of a task snapshot, in time order.
    #[must_use]
    pub fn task_waits(&self) -> Vec<TaskWaitStack> {
        let capture = &self.capture;
        capture
            .task_waits
            .iter()
            .map(|wait| {
                let task = &capture.tasks[wait.task.get() as usize];
                let (functions, ending) = self.names(wait.stack.get());
                TaskWaitStack {
                    noun: capture.string(task.runtime.get()).to_owned(),
                    task: task.number.get(),
                    time: wait.time.get(),
                    weight: wait.weight.get(),
                    state: capture.string(wait.state.get()).to_owned(),
                    functions,
                    ending,
                }
            })
            .collect()
    }

    /// Every task task snapshots found, with its labels and weights.
    #[must_use]
    pub fn tasks(&self) -> Vec<ProfileTask> {
        let capture = &self.capture;
        let mut weights = vec![(0_u64, 0_u64); capture.tasks.len()];

        for sample in &capture.samples {
            if let Some(weight) = weights.get_mut(sample.task.get() as usize) {
                weight.0 += sample.weight.get();
            }
        }

        for wait in &capture.task_waits {
            if let Some(weight) = weights.get_mut(wait.task.get() as usize) {
                weight.1 += wait.weight.get();
            }
        }

        capture
            .tasks
            .iter()
            .zip(weights)
            .zip(0_u32..)
            .map(|((task, (weight, waited)), row)| {
                let labels = match task.labels.get() {
                    NONE => Vec::new(),
                    labels => capture
                        .string(labels)
                        .lines()
                        .filter_map(|line| line.split_once('='))
                        .map(|(key, value)| (key.to_owned(), value.to_owned()))
                        .collect(),
                };

                ProfileTask {
                    row,
                    noun: capture.string(task.runtime.get()).to_owned(),
                    number: task.number.get(),
                    labels,
                    weight,
                    waited,
                }
            })
            .collect()
    }

    /// How many samples task snapshots gave the task they ran.
    #[must_use]
    pub fn task_samples(&self) -> u64 {
        self.capture.account(AccountKind::TaskSamples as u32)
    }

    /// How many task snapshots were taken, how many nanoseconds they held
    /// the program stopped, and how many failed, or failed to read a task.
    #[must_use]
    pub fn task_snapshots(&self) -> (u64, u64, u64) {
        let account = |kind: AccountKind| self.capture.account(kind as u32);
        (
            account(AccountKind::Snapshots),
            account(AccountKind::SnapshotPause),
            account(AccountKind::SnapshotFailures),
        )
    }

    /// Whether any stack holds a frame an interpreter runs, which the
    /// [`StackView::Python`] look shows as its code sees it.
    #[must_use]
    pub fn interpreted(&self) -> bool {
        self.capture
            .frames
            .iter()
            .any(|frame| frame.kind == FrameKind::Interpreted as u8)
    }

    /// How many task snapshots read the program as it ran, and how many
    /// waiting tasks they left out because they changed while read.
    #[must_use]
    pub fn live_snapshots(&self) -> (u64, u64) {
        let account = |kind: AccountKind| self.capture.account(kind as u32);
        (
            account(AccountKind::LiveSnapshots),
            account(AccountKind::SnapshotChanged),
        )
    }

    /// A stack's function names, innermost first, and how it ends.
    fn names(&self, stack: u32) -> (Vec<String>, StackEnding) {
        let capture = &self.capture;
        let mut functions = Vec::new();
        let mut ending = StackEnding::Complete;
        for frame in capture.stack_frames(stack) {
            let record = &capture.frames[frame as usize];
            if record.kind == FrameKind::Truncated as u8 {
                let reason = Truncation::from_number(record.address.get())
                    .map_or("unknown", Truncation::describe);
                ending = StackEnding::Truncated(reason.to_owned());
                continue;
            }

            let lines = capture.frame_lines(frame);
            if lines.is_empty() {
                functions.push(self.address_name(frame));
            }

            for line in lines {
                let function = &capture.functions[line.function.get() as usize];
                functions.push(capture.string(function.name.get()).to_owned());
            }
        }

        (functions, ending)
    }

    /// A frame without symbols, as `module+0xoffset`.
    fn address_name(&self, frame: u32) -> String {
        let record = &self.capture.frames[frame as usize];
        let module = record.module.get();
        if module == NONE {
            return format!("{:#x}", record.address.get());
        }

        let path = self
            .capture
            .string(self.capture.modules[module as usize].path.get());
        let name = Path::new(path)
            .file_name()
            .map_or_else(|| path.into(), |name| name.to_string_lossy());
        format!("{name}+{:#x}", record.address.get())
    }

    /// Whether every mapping of `program` has the bias a breakpoint
    /// source assumed.
    fn check_bias(&self, program: &Path, expected: crate::LoadBias) -> Result<(), RecordError> {
        let program =
            session::which(program).and_then(|program| std::fs::canonicalize(program).ok());
        let capture = &self.capture;

        for mapping in &capture.mappings {
            let path = capture.string(capture.modules[mapping.module.get() as usize].path.get());
            if program.as_deref() == Some(Path::new(path)) && mapping.bias.get() != expected.get() {
                return Err(RecordError::Misplaced {
                    expected: expected.get(),
                    actual: mapping.bias.get(),
                });
            }
        }

        Ok(())
    }
}

/// Why a profile file could not be read.
#[derive(Debug, thiserror::Error)]
pub enum ProfileFileError {
    #[error(transparent)]
    Io(#[from] std::io::Error),

    /// The file is not a profile, or is damaged.
    #[error("{0}")]
    Invalid(String),
}
