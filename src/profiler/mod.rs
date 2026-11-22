mod annotate;
pub mod budget;
mod calls;
mod collector;
mod frame_pointers;
mod interpreted;
mod jit;
pub mod ladder;
mod probes;
mod record;
pub(crate) use record::{OwnedLine, hex, symbolize as symbolize_lines};
pub(crate) use symbols::locate;

mod inspect;
mod segments;
mod session;
mod syscall_trace;
pub use inspect::{InspectedCommand, InspectedModule, InspectedProcess, Inspection, TIME_BINS};
mod symbols;
mod syscalls;
pub use syscalls::{RecordedEvent, RecordedSyscalls, SyscallFrame, SyscallPlace};
mod tasks;
pub(crate) use interpreted::{Sampled, positions, read_record};
pub(crate) use tasks::{InterpretedWait, LiveStop};
mod zones;

pub use crate::capture::records::GapReason;

pub const NO_TASK: u32 = NONE;

pub const NO_KEY: u32 = NONE;
pub use crate::capture::records::MarkerKind;
pub use crate::unwind::sample::CHAIN_LIMIT;
pub use annotate::{CodeBlock, CodeLine, Disassembly, InstructionCost, LineCost, LineCosts};
pub use collector::ProbeSelection;
pub use session::{
    Attachment, Recording, Save, Watch, affordable_rate, record, record_attached,
    record_attached_many, record_watched,
};
pub use symbols::ModuleSymbols;
pub use syscall_trace::{
    BLOCKED_AT, Printer, SharedBpfEngine, SyscallEngine, SyscallLevel, SyscallOptions,
};
pub use tasks::THREAD;
pub use zones::{
    PlotValue, ProbeCounts, ProfileZone, ProgramMarker, ZONE_BEGAN_UNSEEN, ZONE_ENDED_UNSEEN,
    ZONE_LOST_WITHIN, ZONE_MISMATCHED, ZoneSite,
};

pub use crate::analysis::{
    CallTree, Cost, Costs, DiffCost, DiffTree, Granularity, KeyKind, Keys, Lanes, Look, OffCpuLane,
    PlotBin, PlotSeries, Query, Side, Source, Transform, ZoneAt, ZoneBlock, ZoneScope, ZoneStats,
    diff_costs, diff_trees,
};

use std::path::{Path, PathBuf};

use crate::backend::perf;
use crate::capture::records::{AccountKind, FrameKind, NONE, Truncation};
use crate::capture::{Capture, file};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Preset {
    Production,

    #[default]
    Development,

    Maximum,
}

impl Preset {
    #[must_use]
    pub const fn rate(self) -> u32 {
        match self {
            Self::Production => 49,
            Self::Development => 999,
            Self::Maximum => 9999,
        }
    }

    #[must_use]
    pub const fn off_cpu(self) -> bool {
        !matches!(self, Self::Production)
    }

    #[must_use]
    pub const fn budget(self) -> Option<budget::Budget> {
        match self {
            Self::Production => Some(budget::Budget::PRODUCTION),
            Self::Development | Self::Maximum => None,
        }
    }

    #[must_use]
    pub const fn stack_copy(self) -> StackCopy {
        match self {
            Self::Production => StackCopy::Auto,
            Self::Development => StackCopy::Bytes(DEEP_STACK_COPY),
            Self::Maximum => StackCopy::Bytes(MAX_RATE_STACK_COPY),
        }
    }

    #[must_use]
    pub const fn ring_pages(self) -> usize {
        match self {
            Self::Production | Self::Development => 128,
            Self::Maximum => 1024,
        }
    }
}

pub const SMALL_STACK_COPY: u32 = 512;

pub const DEEP_STACK_COPY: u32 = 32 * 1024;

pub const MAX_RATE_STACK_COPY: u32 = 16 * 1024;

pub const MOST_STACK_COPY: u32 = 65528;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StackCopy {
    Auto,

    Bytes(u32),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SampleSource {
    Clock { rate: u32 },

    Function { symbol: String },
}

impl SampleSource {
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PythonEngine {
    Bpf,

    Snapshot { every: std::time::Duration },

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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cuts {
    Whole,

    Window(std::time::Duration),

    Segments(std::time::Duration),
}

#[derive(Debug, Clone, PartialEq)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "independent recording features"
)]
pub struct RecordOptions {
    pub source: SampleSource,
    pub stack: StackCopy,

    pub function_period: u64,

    pub kernel_stacks: bool,

    pub max_frames: usize,

    pub ring_pages: usize,

    pub max_events: usize,

    pub off_cpu: bool,

    pub budget: Option<budget::Budget>,

    pub cuts: Cuts,

    pub task_snapshots: Option<std::time::Duration>,

    pub live_snapshots: bool,

    pub python: Option<PythonEngine>,

    pub probes: ProbeSelection,

    pub probe_timeout: std::time::Duration,

    pub debug_directories: Vec<PathBuf>,

    pub symbols_later: bool,

    pub syscalls: SyscallOptions,
}

impl RecordOptions {
    #[must_use]
    pub const fn preset(preset: Preset) -> Self {
        Self {
            source: SampleSource::Clock {
                rate: preset.rate(),
            },
            stack: preset.stack_copy(),
            function_period: 1024,
            kernel_stacks: false,
            max_frames: 512,
            ring_pages: preset.ring_pages(),
            max_events: 65_536,
            off_cpu: preset.off_cpu(),
            budget: preset.budget(),
            cuts: Cuts::Whole,
            task_snapshots: None,
            live_snapshots: false,
            python: None,
            probes: ProbeSelection::All,
            probe_timeout: std::time::Duration::from_secs(10),
            debug_directories: Vec::new(),
            symbols_later: false,
            syscalls: SyscallOptions::preset(preset),
        }
    }
}

impl Default for RecordOptions {
    fn default() -> Self {
        Self::preset(Preset::default())
    }
}

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

    #[error("{count} functions are named '{name}'; pick one by its symbol name")]
    AmbiguousFunction { name: String, count: usize },

    #[error("the calls of '{name}' cannot be counted exactly: {why}")]
    Uncountable { name: String, why: String },

    #[error(
        "'{name}' has no code of its own: every call of it was inlined, {copies} {}, into {}; count the calls of one of those instead",
        if *copies == 1 { "copy" } else { "copies" },
        into.join(", ")
    )]
    Inlined {
        name: String,
        copies: usize,
        into: Vec<String>,
    },

    #[error("reading the program failed: {0}")]
    Program(String),

    #[error("{0}")]
    Python(String),

    #[error(
        "the program was not loaded where the breakpoint assumed (bias {expected:#x}, not {actual:#x}): its samples would be of another instruction"
    )]
    Misplaced { expected: u64, actual: u64 },
}

pub type SourceFrame<'a> = (&'a str, &'a str, u32);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Profile {
    capture: Capture,
}

impl Profile {
    pub(crate) const fn capture(&self) -> &Capture {
        &self.capture
    }

    pub(crate) fn into_capture(self) -> Capture {
        self.capture
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandIdentity<'a> {
    Argv(&'a str),

    Comm(&'a str),
}

impl<'a> CommandIdentity<'a> {
    #[must_use]
    pub const fn text(self) -> &'a str {
        match self {
            Self::Argv(text) | Self::Comm(text) => text,
        }
    }
}

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

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum StackEnding {
    Complete,

    Truncated(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SampledStack {
    pub pid: u32,
    pub tid: u32,
    pub time: i64,
    pub weight: u64,

    pub functions: Vec<String>,
    pub ending: StackEnding,

    pub task: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProfileTask {
    pub row: u32,

    pub noun: String,
    pub number: u64,

    pub labels: Vec<(String, String)>,

    pub weight: u64,
    pub waited: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskWaitStack {
    pub noun: String,
    pub task: u64,
    pub time: i64,

    pub weight: u64,

    pub state: String,

    pub functions: Vec<String>,
    pub ending: StackEnding,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OffCpuInterval {
    pub pid: u32,
    pub tid: u32,

    pub time: i64,

    pub duration: u64,

    pub preempted: bool,

    pub began_unseen: bool,

    pub ended_unseen: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FunctionCost {
    pub name: String,

    pub own: u64,

    pub total: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum StackView {
    #[default]
    Logical,

    Native,

    Python,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProfileThread {
    pub row: u32,

    pub process: u32,
    pub pid: u32,
    pub tid: u32,

    pub name: Option<String>,
    pub samples: u64,
    pub weight: u64,

    pub zones: u64,

    pub blocked: u64,
    pub preempted: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallNode {
    pub parent: Option<usize>,
    pub name: String,
    pub own: u64,
    pub total: u64,
}

#[derive(Debug)]
pub struct ReadProfile {
    pub profile: Profile,

    pub truncated: bool,

    pub chunks: usize,
}

impl Profile {
    pub fn read(path: &Path) -> Result<ReadProfile, ProfileFileError> {
        if path.is_dir() {
            return Self::read_segments(path);
        }

        Self::from_bytes(&std::fs::read(path)?)
    }

    #[doc(hidden)]
    #[must_use]
    pub fn of_stacks(unit: &str, stacks: &[(&[SourceFrame<'_>], u64)]) -> Self {
        use crate::capture::builder::{CaptureBuilder, LineInput};

        let mut builder = CaptureBuilder::new();
        builder.setting("weight", unit);
        let thread = builder.thread(crate::model::Pid::new(1), crate::model::Tid::new(1), 0);
        let mut frames = std::collections::BTreeMap::new();
        for (path, weight) in stacks {
            let stack: Vec<u32> = path
                .iter()
                .rev()
                .map(|&(function, file, line)| {
                    let next = frames.len() as u64;
                    *frames.entry((function, file, line)).or_insert_with(|| {
                        let frame = builder.frame(FrameKind::Native, NONE, next, 0);
                        let line = LineInput {
                            function,
                            file: Some(file),
                            line,
                            column: 0,
                            declared: Some((file, 1)),
                            role: 0,
                        };
                        builder.symbolize(frame, NONE, &[line]);
                        frame
                    })
                })
                .collect();
            let stack = builder.stack(&stack);
            builder.sample(thread, 0, *weight, stack, NONE);
        }

        Self {
            capture: builder.finish(),
        }
    }

    #[must_use]
    pub fn is_profile(bytes: &[u8]) -> bool {
        bytes.starts_with(&file::FILE_MAGIC)
    }

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

    pub fn to_bytes(&self) -> std::io::Result<Vec<u8>> {
        let mut bytes = Vec::new();
        file::write(&self.capture, &mut bytes)?;
        Ok(bytes)
    }

    pub fn write(&self, path: &Path) -> std::io::Result<()> {
        let out = std::io::BufWriter::new(std::fs::File::create(path)?);
        file::write(&self.capture, out)
    }

    #[must_use]
    pub const fn samples(&self) -> usize {
        self.capture.samples.len()
    }

    #[must_use]
    pub fn setting(&self, key: &str) -> Option<&str> {
        self.capture.setting(key)
    }

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

    #[must_use]
    pub fn span(&self) -> (i64, i64) {
        self.capture.span()
    }

    #[must_use]
    pub fn total_weight(&self) -> u64 {
        self.capture
            .samples
            .iter()
            .map(|sample| sample.weight.get())
            .sum()
    }

    #[must_use]
    pub fn keys(&self, granularity: Granularity) -> Keys<'_> {
        Keys::new(&self.capture, granularity)
    }

    #[must_use]
    pub fn threads(&self) -> Vec<ProfileThread> {
        let capture = &self.capture;
        let mut threads = std::collections::BTreeMap::<u32, ProfileThread>::new();
        let new = |row: u32| {
            let thread = capture.thread(row);
            ProfileThread {
                row,
                process: thread.process.get(),
                pid: capture.process(thread.process.get()).pid.get(),
                tid: thread.tid.get(),
                name: Some(capture.string(thread.name.get()))
                    .filter(|name| !name.is_empty())
                    .map(str::to_owned),
                samples: 0,
                weight: 0,
                zones: 0,
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
            threads.entry(row).or_insert_with(|| new(row)).zones += u64::from(range.count.get());
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

    #[must_use]
    pub fn command(&self, process: u32) -> Option<&str> {
        self.capture
            .setting(&command_setting(process))
            .filter(|command| !command.is_empty())
    }

    #[must_use]
    pub fn command_identity(&self, process: u32) -> Option<CommandIdentity<'_>> {
        if let Some(command) = self.command(process) {
            return Some(CommandIdentity::Argv(command));
        }

        let record = self.capture.processes.get(process as usize)?;
        let name = self.capture.string(record.name.get());
        if !name.is_empty() {
            return Some(CommandIdentity::Comm(name));
        }

        self.capture
            .threads
            .iter()
            .find(|thread| thread.process.get() == process && thread.tid.get() == record.pid.get())
            .map(|thread| self.capture.string(thread.name.get()))
            .filter(|name| !name.is_empty())
            .map(CommandIdentity::Comm)
    }

    #[must_use]
    pub fn throttles(&self) -> u64 {
        self.capture
            .account(crate::capture::records::AccountKind::Throttles as u32)
    }

    #[must_use]
    pub fn top(&self, view: StackView) -> Vec<FunctionCost> {
        self.top_of(view, Source::Samples, Granularity::Function)
    }

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

    #[must_use]
    pub fn call_tree(&self, view: StackView) -> Vec<CallNode> {
        self.call_tree_of(view, Source::Samples, Granularity::Function, false)
    }

    #[must_use]
    pub fn call_tree_of(
        &self,
        view: StackView,
        source: Source,
        granularity: Granularity,
        invert: bool,
    ) -> Vec<CallNode> {
        let keys = self.keys(granularity);
        let tree = keys.aggregate(&Query {
            source,
            invert,
            ..query(view)
        });

        tree.parent
            .iter()
            .zip(&tree.key)
            .zip(tree.own.iter().zip(&tree.total))
            .map(|((&parent, &key), (&own, &total))| CallNode {
                parent: (parent != NONE).then_some(parent as usize),
                name: keys.name(key),
                own,
                total,
            })
            .collect()
    }

    #[must_use]
    pub fn lost_samples(&self) -> u64 {
        self.capture
            .account(crate::capture::records::AccountKind::LostSamples as u32)
    }

    #[must_use]
    #[expect(
        clippy::cast_precision_loss,
        reason = "a percentage needs no more precision than f64's"
    )]
    pub fn dropped_samples(&self) -> Option<String> {
        let lost = self.lost_samples();
        if lost == 0 {
            return None;
        }

        let total = (self.samples() as u64).saturating_add(lost);
        let unit = if self.setting("weight") == Some("executions") {
            "calls"
        } else {
            "samples"
        };
        Some(format!(
            "the kernel dropped {lost} of {total} {unit} ({:.1}%)",
            lost as f64 * 100.0 / total as f64
        ))
    }

    #[must_use]
    pub fn shortened_stacks(&self) -> u64 {
        self.capture
            .account(crate::capture::records::AccountKind::ShortenedStacks as u32)
    }

    #[must_use]
    pub fn off_cpu_lost(&self) -> u64 {
        self.capture
            .account(crate::capture::records::AccountKind::OffCpuLost as u32)
    }

    #[must_use]
    pub fn lost_records(&self) -> u64 {
        self.capture
            .account(crate::capture::records::AccountKind::LostRecords as u32)
    }

    #[must_use]
    pub fn kept_share(&self) -> Option<(u64, u64)> {
        use crate::capture::records::AccountKind;
        let raised = self.capture.account(AccountKind::RaisedSamples as u32);
        let kept = self.capture.account(AccountKind::KeptSamples as u32);
        (kept < raised).then_some((raised, kept))
    }

    #[must_use]
    #[expect(
        clippy::cast_precision_loss,
        reason = "a margin needs no more precision than f64's"
    )]
    pub fn share_margin(&self) -> f64 {
        let (sum, squares) = self
            .capture
            .samples
            .iter()
            .map(|sample| sample.weight.get() as f64)
            .fold((0.0, 0.0), |(sum, squares), weight| {
                (sum + weight, weight.mul_add(weight, squares))
            });
        if sum <= 0.0 {
            return 1.0;
        }

        let effective = sum * sum / squares;
        (1.96 * 0.5 / effective.sqrt()).min(1.0)
    }

    #[must_use]
    pub fn sampled_cpu(&self) -> u64 {
        match self
            .capture
            .account(crate::capture::records::AccountKind::SampledCpu as u32)
        {
            0 => self.total_weight(),
            sampled => sampled,
        }
    }

    #[must_use]
    pub fn counted(&self) -> u64 {
        self.capture
            .account(crate::capture::records::AccountKind::Counted as u32)
    }

    #[must_use]
    pub fn repeats(&self) -> u64 {
        self.capture
            .account(crate::capture::records::AccountKind::Repeats as u32)
    }

    #[must_use]
    pub fn truncations(&self) -> Vec<(String, u64)> {
        record::truncation_counts(&self.capture)
            .into_iter()
            .map(|(reason, count)| (reason.describe().to_owned(), count))
            .collect()
    }

    #[must_use]
    pub fn stack_copy_exhausted(&self) -> u64 {
        self.capture
            .account(AccountKind::Truncated as u32 + Truncation::StackCopyExhausted as u32)
    }

    #[must_use]
    pub fn off_cpu_lanes(&self, query: &Query, buckets: usize) -> Vec<OffCpuLane> {
        crate::analysis::off_cpu(&self.capture, query, buckets)
    }

    #[must_use]
    pub fn kernel_lanes(
        &self,
        keys: &Keys<'_>,
        query: &Query,
        buckets: usize,
    ) -> Vec<crate::analysis::KernelLane> {
        crate::analysis::kernel_lanes(&self.capture, keys, query, buckets)
    }

    #[must_use]
    pub fn gaps(&self) -> Vec<(i64, i64)> {
        self.capture
            .gaps
            .iter()
            .map(|gap| (gap.start.get(), gap.end.get()))
            .collect()
    }

    #[must_use]
    pub fn paused(&self, reason: GapReason) -> Vec<(i64, i64)> {
        self.capture
            .gaps
            .iter()
            .filter(|gap| gap.reason == reason as u8)
            .map(|gap| (gap.start.get(), gap.end.get()))
            .collect()
    }

    #[must_use]
    pub fn budget_changes(&self) -> Vec<(i64, String)> {
        self.markers(MarkerKind::Budget)
    }

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

    #[must_use]
    pub fn recorder_cpu(&self) -> (u64, u64) {
        (
            self.capture.account(AccountKind::RecorderCpu as u32),
            self.capture.account(AccountKind::RecorderLoading as u32),
        )
    }

    #[must_use]
    pub fn recorder_setup(&self) -> u64 {
        self.capture.account(AccountKind::RecorderSetup as u32)
    }

    #[must_use]
    pub fn off_cpu(&self) -> Vec<OffCpuInterval> {
        let capture = &self.capture;
        let mut intervals = Vec::with_capacity(capture.off_cpu.len());

        for range in &capture.off_cpu_ranges {
            let (pid, tid) = ids_of(capture, range.thread.get());
            intervals.extend(
                capture
                    .thread_off_cpu(range)
                    .iter()
                    .map(|interval| OffCpuInterval {
                        pid,
                        tid,
                        time: interval.time.get(),
                        duration: interval.duration.get(),
                        preempted: interval.preempted != 0,
                        began_unseen: interval.flags
                            & crate::capture::records::OFF_CPU_BEGAN_UNSEEN
                            != 0,
                        ended_unseen: interval.flags
                            & crate::capture::records::OFF_CPU_ENDED_UNSEEN
                            != 0,
                    }),
            );
        }

        intervals
    }

    #[must_use]
    pub fn stacks(&self) -> Vec<SampledStack> {
        let capture = &self.capture;
        let mut stacks = Vec::with_capacity(capture.samples.len());

        for range in &capture.sample_ranges {
            let (pid, tid) = ids_of(capture, range.thread.get());
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

    #[must_use]
    pub fn task_waits(&self) -> Vec<TaskWaitStack> {
        let capture = &self.capture;
        capture
            .task_waits
            .iter()
            .map(|wait| {
                let task = capture.task(wait.task.get());
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

    #[must_use]
    pub fn task_samples(&self) -> u64 {
        self.capture.account(AccountKind::TaskSamples as u32)
    }

    #[must_use]
    pub fn task_snapshots(&self) -> (u64, u64, u64) {
        let account = |kind: AccountKind| self.capture.account(kind as u32);
        (
            account(AccountKind::Snapshots),
            account(AccountKind::SnapshotPause),
            account(AccountKind::SnapshotFailures),
        )
    }

    #[must_use]
    pub fn interpreted(&self) -> bool {
        self.capture
            .frames
            .iter()
            .any(|frame| frame.kind == FrameKind::Interpreted as u8)
    }

    #[must_use]
    pub fn live_snapshots(&self) -> (u64, u64, u64) {
        let account = |kind: AccountKind| self.capture.account(kind as u32);
        (
            account(AccountKind::LiveSnapshots),
            account(AccountKind::LiveRead),
            account(AccountKind::SnapshotChanged),
        )
    }

    fn names(&self, stack: u32) -> (Vec<String>, StackEnding) {
        let capture = &self.capture;
        let mut functions = Vec::new();
        let mut ending = StackEnding::Complete;
        for frame in capture.stack_frames(stack) {
            let record = capture.frame(frame);
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
                functions.push(function_name(capture, line.function.get()).to_owned());
            }
        }

        (functions, ending)
    }

    fn address_name(&self, frame: u32) -> String {
        let record = self.capture.frame(frame);
        let module = record.module.get();
        if module == NONE {
            return format!("{:#x}", record.address.get());
        }

        let path = self.capture.string(self.capture.module(module).path.get());
        let name = Path::new(path)
            .file_name()
            .map_or_else(|| path.into(), |name| name.to_string_lossy());
        format!("{name}+{:#x}", record.address.get())
    }

    fn check_bias(&self, program: &Path, expected: crate::LoadBias) -> Result<(), RecordError> {
        let program =
            session::which(program).and_then(|program| std::fs::canonicalize(program).ok());
        let capture = &self.capture;

        for mapping in &capture.mappings {
            let Some(module) = capture.modules.get(mapping.module.get() as usize) else {
                continue;
            };
            let path = capture.string(module.path.get());
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

#[expect(
    clippy::indexing_slicing,
    reason = "Capture::validate checks every line's function, and line keys come from lines"
)]
fn function_name(capture: &Capture, function: u32) -> &str {
    debug_assert!(
        (function as usize) < capture.functions.len(),
        "function {function} of {}",
        capture.functions.len(),
    );
    capture.string(capture.functions[function as usize].name.get())
}

fn ids_of(capture: &Capture, thread: u32) -> (u32, u32) {
    let row = capture.thread(thread);
    (capture.process(row.process.get()).pid.get(), row.tid.get())
}

#[must_use]
pub fn command_setting(process: u32) -> String {
    format!("command {process}")
}

pub const ARGUMENTS: &str = "arguments";

#[must_use]
pub fn arguments_setting(process: u32) -> String {
    format!("arguments {process}")
}

#[must_use]
pub fn encode_arguments<A: AsRef<std::ffi::OsStr>>(
    arguments: impl IntoIterator<Item = A>,
) -> String {
    let arguments: Vec<_> = arguments
        .into_iter()
        .map(|argument| argument.as_ref().to_string_lossy().into_owned())
        .collect();
    serde_json::to_string(&arguments).unwrap_or_default()
}

#[must_use]
pub fn encode_cmdline(cmdline: &[u8]) -> String {
    use std::os::unix::ffi::OsStrExt as _;

    let Some(last) = cmdline.iter().rposition(|byte| *byte != 0) else {
        return "[]".to_owned();
    };
    encode_arguments(
        cmdline
            .get(..=last)
            .unwrap_or_default()
            .split(|byte| *byte == 0)
            .map(std::ffi::OsStr::from_bytes),
    )
}

#[derive(Debug, thiserror::Error)]
pub enum ProfileFileError {
    #[error(transparent)]
    Io(#[from] std::io::Error),

    #[error("{0}")]
    Invalid(String),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capture::builder::CaptureBuilder;
    use crate::capture::records::{AccountKind, FrameKind, NONE};
    use crate::model::{Pid, Tid};

    fn lossy(weight: &str, kept: u64, lost: u64) -> Profile {
        let mut builder = CaptureBuilder::new();
        builder.setting("weight", weight);
        let frame = builder.frame(FrameKind::Native, NONE, 0x1000, 0);
        let stack = builder.stack(&[frame]);
        let thread = builder.thread(Pid::new(1), Tid::new(1), 0);
        for time in 0..kept {
            builder.sample(thread, time.cast_signed(), 1, stack, NONE);
        }
        builder.count(AccountKind::LostSamples as u32, lost);
        Profile::from_capture(builder.finish())
    }

    #[test]
    fn dropped_samples_are_said_in_the_recordings_unit() {
        assert_eq!(
            lossy("executions", 4, 2).dropped_samples().as_deref(),
            Some("the kernel dropped 2 of 6 calls (33.3%)")
        );
        assert_eq!(
            lossy("nanoseconds", 9, 1).dropped_samples().as_deref(),
            Some("the kernel dropped 1 of 10 samples (10.0%)")
        );
        assert_eq!(lossy("executions", 6, 0).dropped_samples(), None);
    }
}
