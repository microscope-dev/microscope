//! System calls in a recording: what to trace, and the trace that runs
//! beside the samples, on their clock, cut with them.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use super::{Cuts, Preset, RecordOptions};
use crate::backend::syscall_engines::bpf::{self, BpfOptions};
use crate::backend::syscall_engines::capture::SyscallCapture;
use crate::backend::syscall_engines::stacks::SharedStacks;
use crate::backend::syscall_engines::{Sink, TraceOptions};
use crate::capture::Capture;
use crate::capture::builder::CaptureBuilder;
use crate::syscalls::captures::Limits;
use crate::syscalls::fds::{self, FdTable};
use crate::syscalls::filter::{self, Filter};
use crate::syscalls::stats::Folded;
use crate::syscalls::{CallSet, TraceEvent};

/// How much of each system call a recording keeps.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum SyscallLevel {
    Off,

    /// Every call counted in the kernel, by call and thread.
    Counts,

    /// Every call with its arguments, result, short strings, and stack.
    Calls,

    /// As `Calls`, with whole strings and the bytes behind pointers.
    Full,
}

impl SyscallLevel {
    pub const ALL: [Self; 4] = [Self::Off, Self::Counts, Self::Calls, Self::Full];

    /// The level a preset records at.
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

/// What sees the calls.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SyscallEngine {
    /// Programs in the kernel; the traced program never stops.
    #[default]
    Bpf,

    /// The debugger's interface, which stops each traced call.
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

/// Everything a recording's system calls depend on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyscallOptions {
    pub level: SyscallLevel,

    /// Whether the level was asked for. The preset's own steps down to
    /// `Off` where its engine cannot run; one asked for refuses.
    pub asked: bool,
    pub engine: SyscallEngine,

    /// The calls kept, and the filter's text; none keeps every call.
    pub filter: Option<(String, Filter)>,

    /// How long a call takes before `Counts` keeps it as slow.
    pub slow: Duration,

    /// Why a level stepped down to `Off`, once it has.
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
            unavailable: None,
        }
    }

    /// Whether the engine can record the level, refusing pairs that
    /// cannot be.
    ///
    /// # Errors
    ///
    /// Counting with ptrace, which stops every call.
    pub fn check(&self) -> Result<(), String> {
        if self.engine == SyscallEngine::Ptrace && self.level == SyscallLevel::Counts {
            return Err(
                "ptrace cannot count calls without stopping each one; record counts with the \
                 BPF engine, or calls with ptrace"
                    .to_owned(),
            );
        }

        Ok(())
    }

    /// Steps a level the preset chose down to `Off` where its engine
    /// cannot run here, keeping why; returns that line to tell the user.
    ///
    /// # Errors
    ///
    /// Why a level that was asked for cannot be recorded.
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

    /// The calls the kernel must see: those kept, and every call that
    /// changes a descriptor, so that the paths of those kept are right.
    fn traced(&self) -> CallSet {
        let Some((_, filter)) = &self.filter else {
            return CallSet::all();
        };

        let mut calls = filter.calls.clone();
        for call in fds::followed() {
            calls.insert(call);
        }

        calls
    }

    /// The settings a capture records of them.
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
        }

        settings
    }
}

/// The system call trace a recording runs beside its samples, on the
/// same clock, with the BPF engine.
pub(super) struct SyscallTrace {
    stop: Arc<AtomicBool>,
    thread: std::thread::JoinHandle<Result<(), String>>,
    shared: Arc<Mutex<Shared>>,
    stacks: Option<SharedStacks>,
}

/// What a recording keeps of its trace until the samples are cut.
struct Shared {
    /// Every event since the last cut, in time order.
    events: VecDeque<TraceEvent>,

    /// What the kernel counted, by when it was read.
    counted: VecDeque<(u64, Folded)>,

    /// Each setting the engine reported.
    settings: Vec<(String, String)>,

    /// Whether to drop events before a cut's start: a window's.
    windowed: bool,

    /// Whether the engine reads its counts every second, for a recording
    /// cut while it runs, or only at its end.
    flushing: bool,
    last_flush: u64,
}

/// The trace's sink, which says when the trace sees its process.
struct Held {
    shared: Arc<Mutex<Shared>>,
    ready: Option<std::sync::mpsc::Sender<()>>,
    filter: Option<Filter>,
    fds: FdTable,
}

fn lock(shared: &Mutex<Shared>) -> MutexGuard<'_, Shared> {
    shared.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Held {
    /// Whether the filter keeps `event`. Every event updates the
    /// descriptors, so that later calls' paths are right.
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

/// How often a cut recording reads the kernel's counts.
const FLUSH_EVERY: u64 = 1_000_000_000;

impl Sink for Held {
    fn event(&mut self, event: TraceEvent) {
        if self.keeps(&event) {
            lock(&self.shared).events.push_back(event);
        }
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
}

impl SyscallTrace {
    /// Traces the calls `recording` asks for of process `pid`, held at its
    /// gate, and of every process it starts, if any; returns once the
    /// trace sees the process. Each call's stack is copied when the
    /// samples copy `stack_bytes`, more than the frame-pointer chain needs.
    pub(super) fn beside(
        pid: u32,
        recording: &RecordOptions,
        stack_bytes: u32,
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
            stacks: streams.then(SharedStacks::default),
        };
        let engine = BpfOptions {
            count_only: !streams,
            stack_copy,
            ..BpfOptions::default()
        };

        let stacks = trace.stacks.clone();
        let shared = Arc::new(Mutex::new(Shared {
            events: VecDeque::new(),
            counted: VecDeque::new(),
            settings: Vec::new(),
            windowed: matches!(recording.cuts, Cuts::Window(_)),
            flushing: recording.cuts != Cuts::Whole,
            last_flush: 0,
        }));
        let stop = Arc::new(AtomicBool::new(false));
        let (ready, seen) = std::sync::mpsc::channel();
        let stopped = Arc::clone(&stop);
        let mut sink = Held {
            shared: Arc::clone(&shared),
            ready: Some(ready),
            filter: options.filter.as_ref().map(|(_, filter)| filter.clone()),
            fds: FdTable::default(),
        };

        let thread = std::thread::Builder::new()
            .name("microscope-record-syscalls".into())
            .spawn(move || {
                bpf::trace_held(pid, &trace, engine, &stopped, &mut sink)
                    .map(|_| ())
                    .map_err(|error| error.to_string())
            })
            .map_err(|error| error.to_string())?;
        if seen.recv().is_ok() {
            return Ok(Some(Self {
                stop,
                thread,
                shared,
                stacks,
            }));
        }

        // The trace ended before it saw the process: why is its error.
        Err(match thread.join() {
            Ok(Err(why)) => why,
            _ => "the trace ended before it began".to_owned(),
        })
    }

    /// `samples` with the calls and counts of their span. Those after it
    /// stay for the next cut; a window's before it go.
    pub(super) fn cut(&self, samples: &Capture) -> Capture {
        cut(&self.shared, self.stacks.as_ref(), samples, false)
    }

    /// Ends the trace, its program having ended, and gives `samples` every
    /// call and count left.
    pub(super) fn merge(self, samples: &Capture) -> Result<Capture, String> {
        let _span = crate::span!("record syscalls merge");
        // Whatever the program's last calls sent is in the ring, which the
        // trace drains before it ends; a process the program left running
        // goes on untraced.
        self.stop.store(true, Ordering::Relaxed);
        self.thread
            .join()
            .map_err(|_| "the system call trace panicked".to_owned())??;
        Ok(cut(&self.shared, self.stacks.as_ref(), samples, true))
    }
}

/// `samples` with the events and counts `shared` keeps for their span,
/// or every one that is left at the `end`.
fn cut(
    shared: &Mutex<Shared>,
    stacks: Option<&SharedStacks>,
    samples: &Capture,
    end: bool,
) -> Capture {
    let (start, stop) = samples.span();
    let (start, stop) = (start.cast_unsigned(), stop.cast_unsigned());
    let mut capture = SyscallCapture::new(stacks.cloned());
    {
        let mut shared = lock(shared);
        for (key, value) in &shared.settings {
            capture.setting(key, value);
        }

        let windowed = shared.windowed;
        while windowed
            && shared
                .events
                .front()
                .is_some_and(|event| event.time() < start)
        {
            shared.events.pop_front();
        }

        while windowed
            && shared
                .counted
                .front()
                .is_some_and(|(time, _)| *time <= start)
        {
            shared.counted.pop_front();
        }

        while let Some(event) = shared
            .events
            .pop_front_if(|event| end || event.time() < stop)
        {
            capture.event(&event);
        }

        let mut counted = Folded::default();
        while let Some((_, stats)) = shared
            .counted
            .pop_front_if(|(time, _)| end || *time <= stop)
        {
            counted.merge(&stats);
        }

        if !counted.by_call.is_empty() {
            capture.counted(&counted);
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
    fn levels_read_back_by_name() {
        for level in SyscallLevel::ALL {
            assert_eq!(level.name().parse::<SyscallLevel>(), Ok(level));
        }

        assert!("all".parse::<SyscallLevel>().is_err());
    }
}
