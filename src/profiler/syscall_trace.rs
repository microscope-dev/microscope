use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use super::ladder::Rung;
use super::{Cuts, Preset, RecordOptions};
use crate::backend::syscall_engines::bpf::{self, BpfOptions, Exceptions, Shed};
use crate::backend::syscall_engines::capture::SyscallCapture;
use crate::backend::syscall_engines::stacks::SharedStacks;
use crate::backend::syscall_engines::{Sink, Target, TraceOptions, ptrace};
use crate::capture::Capture;
use crate::capture::builder::CaptureBuilder;
use crate::syscalls::captures::Limits;
use crate::syscalls::fds::{self, FdTable};
use crate::syscalls::filter::{self, Filter};
use crate::syscalls::specs::{self, Class, Ret};
use crate::syscalls::stats::Folded;
use crate::syscalls::{CallSet, Errno, InFlight, Outcome, SyscallEvent, SyscallId, TraceEvent};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum SyscallLevel {
    Off,

    Counts,

    Calls,

    Full,
}

impl SyscallLevel {
    pub const ALL: [Self; 4] = [Self::Off, Self::Counts, Self::Calls, Self::Full];

    #[must_use]
    pub const fn of(preset: Preset) -> Self {
        match preset {
            Preset::Production => Self::Counts,
            Preset::Development => Self::Calls,
            Preset::Maximum => Self::Full,
        }
    }

    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Counts => "counts",
            Self::Calls => "calls",
            Self::Full => "full",
        }
    }
}

impl std::fmt::Display for SyscallLevel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.name())
    }
}

impl std::str::FromStr for SyscallLevel {
    type Err = String;

    fn from_str(name: &str) -> Result<Self, Self::Err> {
        Self::ALL
            .into_iter()
            .find(|level| level.name() == name)
            .ok_or_else(|| format!("no level {name:?}: choose off, counts, calls, or full"))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SyscallEngine {
    #[default]
    Bpf,

    Ptrace,
}

impl SyscallEngine {
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Bpf => "bpf",
            Self::Ptrace => "ptrace",
        }
    }
}

impl std::str::FromStr for SyscallEngine {
    type Err = String;

    fn from_str(name: &str) -> Result<Self, Self::Err> {
        match name {
            "bpf" => Ok(Self::Bpf),
            "ptrace" => Ok(Self::Ptrace),
            _ => Err(format!("no engine {name:?}: choose bpf or ptrace")),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyscallOptions {
    pub level: SyscallLevel,

    pub asked: bool,
    pub engine: SyscallEngine,

    pub filter: Option<(String, Filter)>,

    pub slow: Duration,

    pub kept_per_second: Option<u32>,

    pub save_on: SaveOn,

    pub print: Option<Printer>,

    pub unavailable: Option<String>,
}

impl SyscallOptions {
    #[must_use]
    pub const fn preset(preset: Preset) -> Self {
        Self {
            level: SyscallLevel::of(preset),
            asked: false,
            engine: SyscallEngine::Bpf,
            filter: None,
            slow: Duration::from_millis(10),
            kept_per_second: None,
            save_on: SaveOn {
                slow: None,
                errnos: Vec::new(),
            },
            print: None,
            unavailable: None,
        }
    }

    pub fn check(&self) -> Result<(), String> {
        if self.engine == SyscallEngine::Ptrace && self.level == SyscallLevel::Counts {
            return Err(
                "ptrace cannot count calls without stopping each one; record counts with the \
                 BPF engine, or calls with ptrace"
                    .to_owned(),
            );
        }

        if self.kept_per_second.is_some() {
            if self.engine == SyscallEngine::Ptrace {
                return Err(
                    "ptrace cannot cap the calls it keeps; cap them with the BPF engine".to_owned(),
                );
            }

            if self.level < SyscallLevel::Calls {
                return Err(format!(
                    "--syscalls {} keeps only failed and slow calls, already capped at \
                     {KEPT_PER_SECOND} a second; cap calls or full",
                    self.level
                ));
            }
        }

        Ok(())
    }

    pub fn resolve(&mut self) -> Result<Option<String>, String> {
        self.check()?;
        if self.level == SyscallLevel::Off || self.engine == SyscallEngine::Ptrace {
            return Ok(None);
        }

        let probe = bpf::Probe::read();
        let Some(missing) = probe.missing() else {
            return Ok(None);
        };

        if self.asked {
            return Err(probe
                .check()
                .err()
                .map_or_else(|| missing.to_string(), |error| error.to_string()));
        }

        let why = format!("{missing}");
        self.level = SyscallLevel::Off;
        self.unavailable = Some(why.clone());
        Ok(Some(format!(
            "system calls not recorded: {why}; record them with --syscall-engine ptrace, or \
             silence this with --syscalls off"
        )))
    }

    pub(super) fn traced(&self) -> CallSet {
        let Some((_, filter)) = &self.filter else {
            return CallSet::all();
        };

        let mut calls = filter.calls.clone();
        if !filter.paths.is_empty() || filter.fds.is_some() || calls.iter().any(names_files) {
            for call in fds::followed() {
                calls.insert(call);
            }
        }

        calls
    }

    pub(super) fn settings(&self) -> Vec<(&'static str, String)> {
        let mut settings = vec![("syscalls", self.level.name().to_owned())];
        if let Some(why) = &self.unavailable {
            settings.push(("syscalls not recorded", why.clone()));
        }

        if self.level != SyscallLevel::Off {
            settings.push(("syscall engine", self.engine.name().to_owned()));
            if let Some((text, _)) = &self.filter {
                settings.push(("syscall filter", text.clone()));
            }

            if self.level == SyscallLevel::Counts {
                settings.push(("syscall slow", format!("{:?}", self.slow)));
                settings.push(("syscall cap", format!("{KEPT_PER_SECOND}/s")));
                settings.push(("syscall examples", format!("{EXAMPLES_PER_MINUTE}/min")));
            } else if let Some(most) = self.kept_per_second {
                settings.push(("syscall cap", format!("{most}/s")));
            }
        }

        settings
    }
}

pub(super) struct SyscallTrace {
    stop: Arc<AtomicBool>,
    thread: std::thread::JoinHandle<Result<(), String>>,
    shared: Arc<Mutex<Shared>>,
    stacks: Option<SharedStacks>,
    level: SyscallLevel,
    engine: SyscallEngine,
    budget: Arc<Budgeted>,
}

#[derive(Default)]
struct Budgeted {
    shed: AtomicU8,
    calls: AtomicU64,
    kernel: AtomicU64,
}

struct Shared {
    events: VecDeque<TraceEvent>,

    counted: VecDeque<(u64, Folded)>,

    settings: Vec<(String, String)>,

    window: Option<u64>,

    flushing: bool,
    last_flush: u64,

    trigger: Option<String>,
    saving: Option<String>,
    quiet_until: u64,

    blocked: Vec<InFlight>,
    blocked_at: u64,
}

#[derive(Clone)]
pub struct Printer(pub Arc<dyn Fn(&TraceEvent) + Send + Sync>);

impl std::fmt::Debug for Printer {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("Printer")
    }
}

impl PartialEq for Printer {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

impl Eq for Printer {}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SaveOn {
    pub slow: Option<Duration>,

    pub errnos: Vec<Errno>,
}

impl SaveOn {
    #[must_use]
    pub const fn any(&self) -> bool {
        self.slow.is_some() || !self.errnos.is_empty()
    }

    fn reason(&self, call: &SyscallEvent) -> Option<String> {
        let name = call.call.display_name();
        if let (Some(slow), Some(took)) = (self.slow, call.duration())
            && Duration::from_nanos(took) >= slow
        {
            #[expect(clippy::cast_precision_loss, reason = "milliseconds to show")]
            return Some(format!("{name} took {:.1} ms", took as f64 / 1e6));
        }

        match call.outcome {
            Outcome::Failed(errno) if self.errnos.contains(&errno) => {
                Some(format!("{name} failed with errno {}", errno.0))
            }
            _ => None,
        }
    }
}

struct Held {
    shared: Arc<Mutex<Shared>>,
    budget: Arc<Budgeted>,
    ready: Option<std::sync::mpsc::Sender<()>>,
    filter: Option<Filter>,
    fds: FdTable,

    save_on: SaveOn,
    quiet: u64,
    print: Option<Printer>,

    watched: bool,
}

fn lock(shared: &Mutex<Shared>) -> MutexGuard<'_, Shared> {
    shared.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Held {
    fn keeps(&mut self, event: &TraceEvent) -> bool {
        let fds = &self.fds;
        let kept = self.filter.as_ref().is_none_or(|filter| match event {
            TraceEvent::Call(call) => {
                let paths = filter::paths(call);
                let paths: Vec<&str> = paths.iter().map(String::as_str).collect();
                filter.shows(call, &paths, &|fd| {
                    fds.describe(call.process, fd)
                        .map(|description| description.text.to_string())
                })
            }
            TraceEvent::Signal { signal, .. } => filter.signals.contains(signal),
            _ => true,
        });

        self.fds.apply(event);
        kept
    }
}

const KEPT_PER_SECOND: u32 = 2000;

const EXAMPLES_PER_MINUTE: u32 = 10;

const BLOCKED_AFTER: u64 = 1_000_000;

pub const BLOCKED_AT: &str = "syscalls blocked at";

const FLUSH_EVERY: u64 = 1_000_000_000;

impl Sink for Held {
    fn event(&mut self, event: TraceEvent) {
        if !self.keeps(&event) {
            return;
        }

        if let Some(print) = &self.print {
            (print.0)(&event);
        }

        let reason = match &event {
            TraceEvent::Call(call) => self.save_on.reason(call),
            _ => None,
        };
        let mut shared = lock(&self.shared);
        let time = event.time();
        if let Some(reason) = reason
            && shared.trigger.is_none()
            && time >= shared.quiet_until
        {
            shared.trigger = Some(reason);
            shared.quiet_until = time.saturating_add(self.quiet);
        }

        shared.events.push_back(event);
    }

    fn attached_by(&mut self, attachment: &bpf::Attachment) {
        let mut shared = lock(&self.shared);
        shared
            .settings
            .push(("syscall hook".into(), attachment.name().into()));
        shared
            .settings
            .push(("syscall hook reason".into(), attachment.reason.clone()));
    }

    fn attached(&mut self, _process: u32) {
        if let Some(ready) = self.ready.take() {
            let _ = ready.send(());
        }
    }

    fn in_flight(&mut self, calls: &[InFlight]) {
        let kept = calls.iter().filter(|call| {
            self.filter
                .as_ref()
                .is_none_or(|filter| filter.calls.contains(call.call))
        });
        let mut shared = lock(&self.shared);
        shared.blocked.clear();
        shared.blocked.extend(kept.copied());
        shared.blocked_at = super::session::monotonic();
    }

    fn in_flight_after(&self) -> Option<u64> {
        self.watched.then_some(BLOCKED_AFTER)
    }

    fn segment_due(&mut self, time: u64) -> bool {
        let mut shared = lock(&self.shared);
        let due = shared.flushing && time.saturating_sub(shared.last_flush) >= FLUSH_EVERY;
        if due {
            shared.last_flush = time;
        }

        due
    }

    fn counted(&mut self, stats: Folded) {
        let time = super::session::monotonic();
        lock(&self.shared).counted.push_back((time, stats));
    }

    fn shed(&self) -> Shed {
        Shed(self.budget.shed.load(Ordering::Relaxed))
    }

    fn cost(&mut self, calls: u64, nanoseconds: u64) {
        self.budget.calls.store(calls, Ordering::Relaxed);
        self.budget.kernel.store(nanoseconds, Ordering::Relaxed);
    }
}

#[derive(Debug, Clone, Copy)]
pub(super) enum Traced {
    Held(u32),

    Running(u32),
}

impl SyscallTrace {
    pub(super) fn beside(
        traced: Traced,
        recording: &RecordOptions,
        stack_bytes: u32,
        watched: bool,
    ) -> Result<Option<Self>, String> {
        let _span = crate::span!("record syscalls start");
        let options = &recording.syscalls;
        let level = options.level;
        if level == SyscallLevel::Off {
            return Ok(None);
        }

        let stack_copy = stack_bytes > super::SMALL_STACK_COPY;
        let limits = match level {
            SyscallLevel::Full => Limits {
                string: 4096,
                dump: 4096,
                elements: 64,
            },
            SyscallLevel::Off | SyscallLevel::Counts | SyscallLevel::Calls => Limits::default(),
        };

        let streams = level >= SyscallLevel::Calls;
        let trace = TraceOptions {
            calls: options.traced(),
            limits,
            verbose: level == SyscallLevel::Full,
            follow: true,
            stacks: Some(SharedStacks::default()),
        };
        let engine = BpfOptions {
            count_only: !streams,
            stack_copy,
            exceptions: Some(Exceptions {
                slow: options
                    .save_on
                    .slow
                    .map_or(options.slow, |slow| slow.min(options.slow)),
                per_second: KEPT_PER_SECOND,
                examples_per_minute: (!streams).then_some(EXAMPLES_PER_MINUTE),
            }),
            streamed_per_second: options.kept_per_second,
        };

        let stacks = trace.stacks.clone();
        let shared = Arc::new(Mutex::new(Shared {
            events: VecDeque::new(),
            counted: VecDeque::new(),
            settings: Vec::new(),
            window: match recording.cuts {
                Cuts::Window(span) => Some(u64::try_from(span.as_nanos()).unwrap_or(u64::MAX)),
                Cuts::Whole | Cuts::Segments(_) => None,
            },
            flushing: recording.cuts != Cuts::Whole,
            last_flush: 0,
            trigger: None,
            saving: None,
            quiet_until: 0,
            blocked: Vec::new(),
            blocked_at: 0,
        }));
        let stop = Arc::new(AtomicBool::new(false));
        let budget = Arc::new(Budgeted::default());
        let (ready, seen) = std::sync::mpsc::channel();
        let stopped = Arc::clone(&stop);
        let mut sink = Held {
            shared: Arc::clone(&shared),
            budget: Arc::clone(&budget),
            ready: Some(ready),
            filter: options.filter.as_ref().map(|(_, filter)| filter.clone()),
            fds: FdTable::default(),
            save_on: options.save_on.clone(),
            print: options.print.clone(),
            quiet: match recording.cuts {
                Cuts::Window(span) | Cuts::Segments(span) => {
                    u64::try_from(span.as_nanos()).unwrap_or(u64::MAX)
                }
                Cuts::Whole => 0,
            },
            watched,
        };

        let engine = (options.engine == SyscallEngine::Bpf).then_some(engine);
        let thread = std::thread::Builder::new()
            .name("microscope-record-syscalls".into())
            .spawn(move || run(traced, engine, &trace, &stopped, &mut sink))
            .map_err(|error| error.to_string())?;
        if seen.recv().is_ok() {
            return Ok(Some(Self {
                stop,
                thread,
                shared,
                stacks,
                level,
                engine: options.engine,
                budget,
            }));
        }

        Err(match thread.join() {
            Ok(Err(why)) => why,
            _ => "the trace ended before it began".to_owned(),
        })
    }

    pub(super) fn rungs(&self) -> Vec<Rung> {
        let mut rungs = Vec::new();
        if self.engine == SyscallEngine::Ptrace {
            return rungs;
        }

        if self.level >= SyscallLevel::Calls {
            rungs.extend([Rung::Stacks, Rung::Sampled, Rung::ExceptionsOnly]);
        }

        if self.level == SyscallLevel::Full {
            rungs.push(Rung::Strings);
        }

        rungs
    }

    pub(super) fn shed(&self, shed: &[Rung]) {
        let bits = shed
            .iter()
            .map(|rung| match rung {
                Rung::Stacks => Shed::STACKS.0,
                Rung::Strings => Shed::STRINGS.0,
                Rung::Sampled => Shed::SAMPLED.0,
                Rung::ExceptionsOnly => Shed::STREAM.0,
                Rung::Samples => 0,
            })
            .fold(0, |bits, bit| bits | bit);
        self.budget.shed.store(bits, Ordering::Relaxed);
    }

    pub(super) fn cost(&self) -> (u64, Duration) {
        (
            self.budget.calls.load(Ordering::Relaxed),
            Duration::from_nanos(self.budget.kernel.load(Ordering::Relaxed)),
        )
    }

    pub(super) fn take_trigger(&self) -> bool {
        let mut shared = lock(&self.shared);
        let Some(reason) = shared.trigger.take() else {
            return false;
        };

        shared.saving = Some(reason);
        true
    }

    pub(super) fn cut(&self, samples: &Capture) -> Capture {
        cut(&self.shared, self.stacks.as_ref(), samples, Taken::Span)
    }

    pub(super) fn live(&self, samples: &Capture) -> Capture {
        cut(&self.shared, self.stacks.as_ref(), samples, Taken::Live)
    }

    pub(super) fn merge(self, samples: &Capture) -> Result<Capture, String> {
        let _span = crate::span!("record syscalls merge");
        self.stop.store(true, Ordering::Relaxed);
        self.thread
            .join()
            .map_err(|_| "the system call trace panicked".to_owned())??;
        Ok(cut(
            &self.shared,
            self.stacks.as_ref(),
            samples,
            Taken::Rest,
        ))
    }
}

fn names_files(call: SyscallId) -> bool {
    specs::spec(call).is_some_and(|spec| {
        spec.ret == Ret::Fd
            || spec.classes.contains(Class::Net)
            || spec.classes.contains(Class::Desc)
            || spec.classes.contains(Class::File)
    })
}

fn run(
    traced: Traced,
    bpf: Option<BpfOptions>,
    trace: &TraceOptions,
    stop: &AtomicBool,
    sink: &mut Held,
) -> Result<(), String> {
    let ended = match (traced, bpf) {
        (Traced::Held(pid), Some(engine)) => {
            bpf::trace_held(pid, trace, engine, stop, sink).map_err(|error| error.to_string())
        }
        (Traced::Running(pid), Some(engine)) => {
            bpf::trace_running(pid, trace, engine, stop, sink).map_err(|error| error.to_string())
        }
        (Traced::Held(pid), None) => {
            ptrace::trace_held(pid, trace, stop, sink).map_err(|error| error.to_string())
        }
        (Traced::Running(pid), None) => {
            let tree = std::iter::once(pid)
                .chain(crate::backend::perf::descendants(pid))
                .collect();
            ptrace::trace(Target::Attach(tree), trace, stop, sink)
                .map_err(|error| error.to_string())
        }
    };

    ended.map(|_| ())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Taken {
    Span,

    Live,

    Rest,
}

fn cut(
    shared: &Mutex<Shared>,
    stacks: Option<&SharedStacks>,
    samples: &Capture,
    taken: Taken,
) -> Capture {
    let (start, stop) = samples.span();
    let (start, stop) = (start.cast_unsigned(), stop.cast_unsigned());
    let mut capture = SyscallCapture::new(stacks.cloned());
    {
        let mut shared = lock(shared);
        for (key, value) in &shared.settings {
            capture.setting(key, value);
        }

        if taken != Taken::Live
            && let Some(reason) = shared.saving.take()
        {
            capture.setting("trigger", &reason);
        }

        if let Some(length) = shared.window {
            let passed = start.min(stop.saturating_sub(length));
            while shared
                .events
                .front()
                .is_some_and(|event| event.time() < passed)
            {
                shared.events.pop_front();
            }

            while shared
                .counted
                .front()
                .is_some_and(|(time, _)| *time <= passed)
            {
                shared.counted.pop_front();
            }
        }

        let all = taken != Taken::Span;
        let mut counted = Folded::default();
        if shared.window.is_some() || taken == Taken::Live {
            for event in shared
                .events
                .iter()
                .filter(|event| all || event.time() < stop)
            {
                capture.event(event);
            }

            for (_, stats) in shared
                .counted
                .iter()
                .filter(|(time, _)| all || *time <= stop)
            {
                counted.merge(stats);
            }
        } else {
            while let Some(event) = shared
                .events
                .pop_front_if(|event| all || event.time() < stop)
            {
                capture.event(&event);
            }

            while let Some((_, stats)) = shared
                .counted
                .pop_front_if(|(time, _)| all || *time <= stop)
            {
                counted.merge(&stats);
            }
        }

        if !counted.by_call.is_empty() {
            capture.counted(&counted);
        }

        if taken == Taken::Live {
            capture.setting(BLOCKED_AT, &shared.blocked_at.to_string());
            for call in &shared.blocked {
                capture.event(&TraceEvent::Call(call.unfinished()));
            }
        }
    }

    let mut builder = CaptureBuilder::new();
    builder.absorb(samples);
    builder.absorb(&capture.finish().capture);
    builder.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_preset_records_its_level_and_ptrace_cannot_count() {
        let levels: Vec<SyscallLevel> = [Preset::Production, Preset::Development, Preset::Maximum]
            .into_iter()
            .map(|preset| SyscallOptions::preset(preset).level)
            .collect();
        assert_eq!(
            levels,
            [
                SyscallLevel::Counts,
                SyscallLevel::Calls,
                SyscallLevel::Full
            ]
        );

        let counting = SyscallOptions {
            engine: SyscallEngine::Ptrace,
            ..SyscallOptions::preset(Preset::Production)
        };
        assert!(counting.check().is_err());
        assert!(
            SyscallOptions {
                level: SyscallLevel::Calls,
                ..counting
            }
            .check()
            .is_ok()
        );
    }

    #[test]
    fn a_trace_that_kept_no_calls_leaves_the_samples_span() {
        let shared = Mutex::new(Shared {
            events: VecDeque::new(),
            counted: VecDeque::new(),
            settings: Vec::new(),
            window: None,
            flushing: false,
            last_flush: 0,
            trigger: None,
            saving: None,
            quiet_until: 0,
            blocked: Vec::new(),
            blocked_at: 0,
        });
        let mut samples = CaptureBuilder::new();
        samples.setting("start", "5000000000");
        samples.setting("end", "9000000000");

        let capture = cut(&shared, None, &samples.finish(), Taken::Rest);
        assert_eq!(capture.span(), (5_000_000_000, 9_000_000_000));
    }

    #[test]
    fn only_calls_that_name_files_follow_descriptors() {
        let traced = |expression: &str| {
            let filter = Filter::parse(&[expression.to_owned()], &|_| None).expect("a filter");
            SyscallOptions {
                filter: Some((expression.to_owned(), filter)),
                ..SyscallOptions::preset(Preset::Development)
            }
            .traced()
        };
        let named = |name| SyscallId::named(crate::syscalls::Abi::X86_64, name).expect("a call");

        let exits = traced("trace=rt_sigprocmask,clone,clone3,wait4,exit,exit_group,kill");
        assert!(exits.contains(named("kill")));
        assert!(!exits.contains(named("openat")));

        for expression in ["trace=read", "trace=connect", "trace=unlink", "trace=pipe2"] {
            assert!(traced(expression).contains(named("dup2")), "{expression}");
        }
    }

    #[test]
    fn levels_read_back_by_name() {
        for level in SyscallLevel::ALL {
            assert_eq!(level.name().parse::<SyscallLevel>(), Ok(level));
        }

        assert!("all".parse::<SyscallLevel>().is_err());
    }
}
