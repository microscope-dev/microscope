//! Running a recording: starting a program under its events, or attaching
//! them to one that runs, then draining the events into a capture until the
//! program ends or the recording is stopped.

use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use super::budget::{Budget, Controller, Observation, STEPS};
use super::calls::{self, Calls};
use super::collector::{Collector, ProbeSelection};
use super::interpreted::{Layout, Walked};
use super::record::{self, Sites};
use super::tasks::{self, Live, Reads, Snapshot, Snapshotter};
use super::{
    DEEP_STACK_COPY, Profile, PythonEngine, RecordError, RecordOptions, SMALL_STACK_COPY,
    SampleSource, StackCopy,
};

use crate::backend::bpf::sampler::{Sampler, SamplerError};
use crate::backend::perf;
use crate::backend::syscall_engines::bpf::{self, BpfOptions};
use crate::backend::syscall_engines::capture::SyscallCapture;
use crate::backend::syscall_engines::stacks::SharedStacks;
use crate::backend::syscall_engines::{Sink, TraceOptions};
use crate::capture::Capture;
use crate::capture::builder::CaptureBuilder;
use crate::capture::records::{AccountKind, GapReason, MarkerKind};
use crate::model::Pid;

/// A recording and how its program ended.
#[derive(Debug)]
pub struct Recording {
    pub profile: Profile,
    pub status: ExitStatus,
}

/// A recording of a process that was running already.
#[derive(Debug)]
pub struct Attachment {
    pub profile: Profile,

    /// Whether the process exited, rather than the recording being stopped.
    pub exited: bool,
}

/// Runs `command` to its exit, recording it.
pub fn record(command: Command, options: &RecordOptions) -> Result<Recording, RecordError> {
    let stop = AtomicBool::new(false);
    record_watched(
        command,
        options,
        &mut Watch {
            every: Duration::MAX,
            publish: &mut |_| {},
            stop: &stop,
            save: None,
        },
    )
}

/// What a recording does while it runs, for a live view of it.
pub struct Watch<'a> {
    /// How often the profile so far is published, at most: the first
    /// comes within a quarter second, and each wait doubles up to this.
    pub every: Duration,

    /// Takes the profile so far: once when the program has started, when
    /// it holds nothing yet, and every `every` after.
    pub publish: &'a mut dyn FnMut(Profile),

    /// Ends the recording once set: a program the recording started is
    /// killed, and one it attached to goes on without it.
    pub stop: &'a AtomicBool,

    /// Saves the profile so far when triggered, as the recording goes on.
    pub save: Option<Save<'a>>,
}

/// A trigger, and what takes the profile when it fires: with a rolling
/// window, the window as it is, and with segments, the segment so far,
/// which ends there. Segments are saved as each ends too.
pub struct Save<'a> {
    /// Fires once set, and is cleared as the profile is taken.
    pub trigger: &'a AtomicBool,
    pub to: &'a mut dyn FnMut(Profile),
}

/// Records `command` as [`record`] does, publishing the profile so far as
/// `watch` asks.
pub fn record_watched(
    command: Command,
    options: &RecordOptions,
    watch: &mut Watch<'_>,
) -> Result<Recording, RecordError> {
    let _span = crate::span!("record");
    if options.python == Some(PythonEngine::Bpf) {
        crate::backend::bpf::sampler::check_namespace()?;
    }
    let program = PathBuf::from(command.get_program());
    let (source, breakpoint) = perf_source(&program, &options.source, None)?;
    let mut processor = processor(options, breakpoint);
    let (stack_bytes, stack_copy) = stack_copy(&mut processor, &program, options.stack);
    let events = perf::EventOptions {
        source,
        stack_bytes,
        ring_pages: options.ring_pages,
        switches: options.off_cpu,
    };

    let launch = perf::LaunchOptions {
        no_address_randomization: breakpoint.is_some(),
    };

    let mut syscalls = None;
    let mut walker = None;
    let mut refused = None;
    let launched = perf::launch(command, &events, launch, |pid, events| {
        // The program runs on the events before the program can start a
        // thread that would inherit them without it.
        if options.python == Some(PythonEngine::Bpf) {
            match Walker::attach(events, &mut processor) {
                Ok(attached) => walker = Some(attached),
                Err(error) => {
                    let why = error.to_string();
                    refused = Some(error);
                    return Err(why);
                }
            }
        }

        if let Some(calls) = &options.syscalls {
            syscalls = Some(SyscallTrace::start(pid, calls, options)?);
        }

        Ok(())
    });

    if let Some(error) = refused {
        return Err(error.into());
    }

    let perf::Launched {
        pid,
        mut events,
        exit,
        killer,
        executed,
    } = launched?;

    settings(
        &mut processor,
        options,
        &program,
        pid,
        events.pages(),
        stack_bytes,
        stack_copy,
    );

    // A program that failed to execute ends the recording with why, below,
    // without having been published as started.
    let started = executed.recv().is_ok().then_some(pid);
    drive(
        &mut processor,
        &mut events,
        watch,
        options,
        started,
        syscalls.as_ref(),
        walker.as_mut(),
        || exit.is_finished(),
        || {
            record!("record: stopping, killing {pid}");
            killer.kill();
            false
        },
    );

    let status = exit
        .join()
        .unwrap_or_else(|_| Err(std::io::Error::other("the program's waiter panicked")))
        .map_err(RecordError::Start)?;
    record!("record: the program ended: {status}");
    processor.exited(Pid::new(pid));
    let mut capture = processor.finish();

    if let Some(syscalls) = syscalls {
        capture = syscalls.merge(&capture).map_err(RecordError::Syscalls)?;
    }

    let profile = Profile { capture };
    if let Some(calls) = breakpoint {
        profile.check_bias(&program, calls.bias)?;
    }

    Ok(Recording { profile, status })
}

/// Records the running process `pid`, which goes on as it was.
///
/// The events follow it from outside, and nothing stops it. The recording ends when
/// the process exits or `watch` stops it, and publishes as `watch` asks.
pub fn record_attached(
    pid: u32,
    options: &RecordOptions,
    watch: &mut Watch<'_>,
) -> Result<Attachment, RecordError> {
    let _span = crate::span!("record attached");
    if options.python == Some(PythonEngine::Bpf) {
        crate::backend::bpf::sampler::check_namespace()?;
    }
    let program = PathBuf::from(format!("/proc/{pid}/exe"));
    let bias = perf::program_bias(pid);
    let (source, breakpoint) = perf_source(&program, &options.source, bias)?;
    let mut processor = processor(options, breakpoint);
    let (stack_bytes, stack_copy) = stack_copy(&mut processor, &program, options.stack);
    let events = perf::EventOptions {
        source,
        stack_bytes,
        ring_pages: options.ring_pages,
        switches: options.off_cpu,
    };

    let perf::Attached {
        mut events,
        process,
    } = perf::attach(pid, &events, options.max_events)?;
    let mut walker = match options.python {
        Some(PythonEngine::Bpf) => Some(Walker::attach(&events, &mut processor)?),
        _ => None,
    };

    processor.attached(Pid::new(pid), monotonic());
    let name = std::fs::read_link(&program).unwrap_or(program);
    let pages = events.pages();
    settings(
        &mut processor,
        options,
        &name,
        pid,
        pages,
        stack_bytes,
        stack_copy,
    );

    let mut stopped = false;
    drive(
        &mut processor,
        &mut events,
        watch,
        options,
        Some(pid),
        None,
        walker.as_mut(),
        || process.exited(),
        || {
            record!("record: stopping, detaching from {pid}");
            stopped = true;
            true
        },
    );

    record!("record: detached from {pid}, stopped {stopped}");
    if !stopped {
        processor.exited(Pid::new(pid));
    }

    Ok(Attachment {
        profile: Profile {
            capture: processor.finish(),
        },
        exited: !stopped,
    })
}

/// How often the BPF engine is told each Python process's threads anew,
/// as they start and end.
const WALK_REFRESH: Duration = Duration::from_millis(20);

/// The BPF engine for Python stacks, and what it has been told.
struct Walker {
    sampler: Sampler,
    next: std::time::Instant,

    /// The processes whose layout it has, and those it was told it
    /// cannot have.
    laid_out: std::collections::BTreeSet<u32>,
    refused: std::collections::BTreeSet<u32>,

    /// The processes some of whose threads it could not be told of.
    troubled: std::collections::BTreeSet<u32>,
}

impl From<SamplerError> for RecordError {
    fn from(error: SamplerError) -> Self {
        Self::Python(error.to_string())
    }
}

impl Walker {
    /// Loads the engine onto `events`, which must not yet sample, and has
    /// `processor` splice what it writes.
    fn attach(
        events: &perf::Events,
        processor: &mut record::Processor,
    ) -> Result<Self, SamplerError> {
        let (sampler, outputs) = Sampler::load(events)?;
        processor.interpret(outputs);
        Ok(Self {
            sampler,
            next: std::time::Instant::now(),
            laid_out: std::collections::BTreeSet::new(),
            refused: std::collections::BTreeSet::new(),
            troubled: std::collections::BTreeSet::new(),
        })
    }

    /// Tells the engine each Python process's layout, once, and its
    /// threads, when due; and forgets the processes that are gone.
    fn refresh(&mut self, processor: &mut record::Processor) {
        let now = std::time::Instant::now();
        if now < self.next {
            return;
        }

        self.next = now + WALK_REFRESH;
        let walked = processor.walked();
        let sampler = &mut self.sampler;
        self.laid_out.retain(|pid| {
            let live = walked.iter().any(|walked| walked.pid == *pid);
            if !live {
                sampler.forget(*pid);
            }

            live
        });

        for walked in walked {
            if !self.laid_out.contains(&walked.pid) && !self.lay_out(processor, &walked) {
                continue;
            }

            // Threads it is not told of have unknown frames, which the
            // recording says why once.
            let failed = self
                .sampler
                .set_threads(walked.pid, &walked.threads)
                .err()
                .map(|error| error.to_string());
            if let Some(problem) = failed.or_else(|| walked.gaps.first().map(ToString::to_string))
                && self.troubled.insert(walked.pid)
            {
                let what = match walked.layout {
                    Ok(Layout::V8(_)) => format!("node {} isolates", walked.pid),
                    _ => format!("python {} threads", walked.pid),
                };
                processor.builder().setting(&what, &problem);
            }
        }
    }

    /// Tells the engine the layout of the process `walked` is of, or the
    /// recording why it cannot: whether the engine has it.
    fn lay_out(&mut self, processor: &mut record::Processor, walked: &Walked) -> bool {
        let set = walked.layout.clone().and_then(|layout| {
            match layout {
                Layout::Python(layout) => self.sampler.set_layout(walked.pid, &layout),
                Layout::V8(layout) => self.sampler.set_v8_layout(walked.pid, &layout),
            }
            .map_err(|error| error.to_string())
        });
        match set {
            Ok(()) => self.laid_out.insert(walked.pid),
            Err(why) => {
                if self.refused.insert(walked.pid) {
                    processor.builder().setting(
                        &format!("python {}", walked.pid),
                        &format!("not read: {why}"),
                    );
                }

                false
            }
        }
    }

    /// Counts the records the rings had no room for, or says why they
    /// cannot be counted.
    fn finish(&self, processor: &mut record::Processor) {
        let builder = processor.builder();
        match self.sampler.lost() {
            Ok(lost) => {
                let had = builder.account(AccountKind::InterpretedLost as u32);
                builder.count(
                    AccountKind::InterpretedLost as u32,
                    lost.saturating_sub(had),
                );
            }
            Err(error) => builder.setting("python lost records", &format!("not counted: {error}")),
        }
        #[cfg(feature = "bpf-walk-timing")]
        match self.sampler.walk_times() {
            Ok([walks, ns, frames, most, full, full_ns]) => builder.setting(
                "v8 walk times",
                &format!(
                    "{walks} walks, {ns} ns, {frames} frames, {most} ns at most, \
                     {full} full walks, {full_ns} ns"
                ),
            ),
            Err(error) => builder.setting("v8 walk times", &format!("not read: {error}")),
        }
    }
}

/// A processor for `options`, and for a breakpoint's repeats.
fn processor(options: &RecordOptions, breakpoint: Option<Calls>) -> record::Processor {
    let search = crate::debug_info::DebugFileSearch::new(&crate::DebugFileOptions {
        directories: options.debug_directories.clone(),
        ..crate::DebugFileOptions::default()
    });
    let mut processor = record::Processor::new(search, options.max_frames);
    if options.symbols_later {
        processor.symbols_later();
    }

    if let Some(calls) = breakpoint
        && let Some(branch) = calls.repeats_when
    {
        processor.repeats_when(calls.loaded().get(), branch);
    }

    processor
}

/// Records how the recording was made.
fn settings(
    processor: &mut record::Processor,
    options: &RecordOptions,
    program: &Path,
    pid: u32,
    ring_pages: usize,
    stack_bytes: u32,
    stack_copy: String,
) {
    let builder = processor.builder();
    for (key, value) in [
        ("program", program.to_string_lossy().into_owned()),
        ("pid", pid.to_string()),
        ("ring pages", ring_pages.to_string()),
        ("source", options.source.to_string()),
        ("weight", options.source.unit().to_owned()),
        ("stack bytes", stack_bytes.to_string()),
        ("stack copy", stack_copy),
        ("off-CPU", if options.off_cpu { "on" } else { "off" }.into()),
        (
            "window",
            options.window.map_or_else(
                || "all".into(),
                |window| format!("the last {:.3} s", window.as_secs_f64()),
            ),
        ),
        (
            "segments",
            options
                .segment
                .filter(|_| options.window.is_none())
                .map_or_else(
                    || "off".into(),
                    |every| format!("every {:.3} s", every.as_secs_f64()),
                ),
        ),
        (
            "task snapshots",
            options.task_snapshots.map_or_else(
                || "off".into(),
                |every| {
                    let live = if options.live_snapshots {
                        ", as the program runs"
                    } else {
                        ""
                    };

                    format!("every {:.3} s{live}", every.as_secs_f64())
                },
            ),
        ),
        (
            "python",
            options
                .python
                .map_or_else(|| "off".into(), |engine| engine.to_string()),
        ),
        ("probes", options.probes.to_string()),
        (
            "budget",
            options.budget.map_or_else(
                || "none".into(),
                |budget| format!("{}% of a CPU", budget.cpu * 100.0),
            ),
        ),
    ] {
        builder.setting(key, &value);
    }
}

/// Drains `events` into `processor` until `ended`, publishing and saving
/// as `watch` asks, and holding to the budget, window, and snapshots of
/// `options`, once the program `started` names has started. When `watch`
/// stops the recording, `stop` ends it at once when it says so, or else
/// waits for `ended`.
#[expect(clippy::too_many_arguments, reason = "each is one part of a recording")]
fn drive(
    processor: &mut record::Processor,
    events: &mut perf::Events,
    watch: &mut Watch<'_>,
    options: &RecordOptions,
    started: Option<u32>,
    syscalls: Option<&SyscallTrace>,
    mut walker: Option<&mut Walker>,
    mut ended: impl FnMut() -> bool,
    mut stop: impl FnMut() -> bool,
) {
    let mut pacer = options
        .budget
        .filter(|_| started.is_some())
        .map(|budget| Pacer::new(budget, monotonic(), processor));
    let mut snapshots = started.map_or_else(Vec::new, |pid| snapshotters(pid, options));
    let mut probes = started
        .filter(|_| options.probes != ProbeSelection::Off)
        .map(|pid| Collector::new(pid, options.probes.clone(), options.probe_timeout));
    let started = started.is_some();

    // A window moves by eighths of itself.
    let marks = options.window.map(|window| {
        processor.keep_window(window);
        (window / 8).max(Duration::from_millis(10))
    });
    let mut marked = std::time::Instant::now();
    let segment = options.segment.filter(|_| options.window.is_none());
    let mut segmented = std::time::Instant::now();
    if started {
        (watch.publish)(Profile {
            capture: processor.snapshot(),
        });
    }

    let mut published = std::time::Instant::now();
    let mut wait = watch.every.min(Duration::from_millis(250));
    let mut stopping = false;
    let mut now = false;
    let mut buffer = Vec::new();
    loop {
        if !stopping && watch.stop.load(Ordering::Relaxed) {
            stopping = true;
            now = stop();
        }

        let finished = now || ended();

        // Every record stamped before this has been written by the time
        // the next drain begins, but for a stalled CPU.
        let watermark = monotonic().saturating_sub(WATERMARK_DELAY);
        if !finished {
            let mut wait = Duration::from_millis(10);
            if let Some(pacer) = &mut pacer {
                wait = wait.min(pacer.pace(monotonic(), events, processor));
            }
            // A snapshot asked for more often than the drain's own pace
            // is taken when due, not at the next drain.
            let due = snapshots.iter().map(Snapshots::due_in).min();
            events.wait(due.map_or(wait, |due| wait.min(due)));
        }

        buffer.clear();
        events.drain(&mut buffer);
        record!(
            "record: drained {} bytes, finished {finished}",
            buffer.len()
        );
        processor.ingest(&buffer, (!finished).then_some(watermark));
        if let Some(walker) = &mut walker {
            walker.refresh(processor);
        }

        if let Some(probes) = &mut probes {
            if finished {
                probes.finish(processor, !ended());
            } else {
                probes.drain(processor);
            }
        }

        if finished {
            break;
        }

        for snapshots in &mut snapshots {
            let sampling = pacer.as_ref().is_none_or(Pacer::sampling);
            snapshots.when_due(processor, events, sampling);
        }

        if let Some(every) = marks
            && marked.elapsed() >= every
        {
            processor.counts(events.counts());
            processor.mark();
            marked = std::time::Instant::now();
        }

        // A program's trigger saves the window as microscope's does.
        let triggered = processor.take_triggered();
        if let Some(save) = &mut watch.save {
            save_when_due(
                save,
                processor,
                events,
                syscalls,
                triggered,
                (segment, &mut segmented),
            );
        }

        if started && published.elapsed() >= wait {
            processor.counts(events.counts());
            (watch.publish)(Profile {
                capture: processor.snapshot(),
            });
            published = std::time::Instant::now();
            wait = (wait * 2).min(watch.every);
        }
    }

    events.enable(false);
    if let Some(pacer) = pacer {
        pacer.finish(monotonic(), processor);
    }

    if let Some(walker) = walker {
        walker.finish(processor);
    }

    processor.counts(events.counts());
}

/// Saves the profile so far when `save`'s trigger fires, the program
/// `triggered` one, or a segment of `every` since it was last `cut` is
/// due: the window, or the segment, which ends there.
fn save_when_due(
    save: &mut Save<'_>,
    processor: &mut record::Processor,
    events: &perf::Events,
    syscalls: Option<&SyscallTrace>,
    triggered: bool,
    (every, cut): (Option<Duration>, &mut std::time::Instant),
) {
    let due = every.is_some_and(|every| cut.elapsed() >= every);
    if !(save.trigger.swap(false, Ordering::Relaxed) || triggered || due) {
        return;
    }

    processor.counts(events.counts());
    record!("record: saved, segment due {due}");
    let capture = if every.is_some() {
        *cut = std::time::Instant::now();
        processor.segment()
    } else {
        processor.snapshot()
    };

    // The calls of the same span, on the same clock.
    let capture = syscalls.map_or_else(|| capture.clone(), |trace| trace.cut(&capture));
    (save.to)(Profile { capture });
}

/// What takes the snapshots `options` asks of process `pid`: of its
/// tasks, and of its threads for a Python engine that reads them.
fn snapshotters(pid: u32, options: &RecordOptions) -> Vec<Snapshots> {
    let tasks = options.task_snapshots.map(|every| {
        let reads = Reads {
            tasks: true,
            threads: false,
        };
        (every, options.live_snapshots, reads)
    });
    let threads = options.python.and_then(|engine| {
        let reads = Reads {
            tasks: false,
            threads: true,
        };
        match engine {
            PythonEngine::Snapshot { every } => Some((every, false, reads)),
            PythonEngine::Live { every } => Some((every, true, reads)),
            PythonEngine::Bpf => None,
        }
    });

    [tasks, threads]
        .into_iter()
        .flatten()
        .map(|(every, live, reads)| Snapshots::new(pid, every, live, reads, options))
        .collect()
}

/// What a live read that began `late` after it was due and took `took`
/// says of reads asked for `every` so often: that they ran less often than
/// asked, where the next was due before this one ended.
fn fell_behind(late: Duration, took: Duration, every: Duration) -> Option<String> {
    (late + took > every).then(|| {
        format!(
            "a live read took {:.1} ms and began {:.1} ms after it was due, longer than the \
             {:.1} ms asked between reads, so reads ran less often than asked",
            took.as_secs_f64() * 1e3,
            late.as_secs_f64() * 1e3,
            every.as_secs_f64() * 1e3
        )
    })
}

/// Takes a task snapshot every so often, once the program's debug
/// information is read, with every event off while it holds the program
/// stopped.
struct Snapshots {
    pid: u32,
    every: Duration,
    live: bool,
    reads: Reads,
    loading: Option<std::thread::JoinHandle<Result<Snapshotter, RecordError>>>,
    snapshotter: Option<Snapshotter>,
    next: std::time::Instant,

    /// When the last snapshot, or the recording, began.
    previous: u64,
}

impl Snapshots {
    fn new(pid: u32, every: Duration, live: bool, reads: Reads, options: &RecordOptions) -> Self {
        let debug_files = crate::DebugFileOptions {
            directories: options.debug_directories.clone(),
            ..crate::DebugFileOptions::default()
        };

        // Read beside the recording, which drains on meanwhile.
        let program = PathBuf::from(format!("/proc/{pid}/exe"));
        let loading = std::thread::Builder::new()
            .name("microscope-snapshot-load".into())
            .spawn(move || Snapshotter::new(pid, &program, &debug_files))
            .ok();

        Self {
            pid,
            every,
            live,
            reads,
            loading,
            snapshotter: None,
            next: std::time::Instant::now() + every,
            previous: monotonic(),
        }
    }

    /// How long until the next snapshot is due; none is due while the
    /// snapshotter still loads, which the drain's own pace checks on.
    fn due_in(&self) -> Duration {
        if self.snapshotter.is_none() {
            return Duration::MAX;
        }
        self.next
            .saturating_duration_since(std::time::Instant::now())
    }

    /// Takes a snapshot when one is due, leaving the events as `sampling`
    /// says they were.
    fn when_due(
        &mut self,
        processor: &mut record::Processor,
        events: &mut perf::Events,
        sampling: bool,
    ) {
        if std::time::Instant::now() < self.next {
            return;
        }

        if let Some(loading) = self.loading.take_if(|loading| loading.is_finished()) {
            match loading.join() {
                Ok(Ok(snapshotter)) => {
                    // Reads are due from now on, not from when the
                    // recording began loading what they need.
                    self.snapshotter = Some(snapshotter);
                    self.next = std::time::Instant::now();
                }

                Ok(Err(error)) => {
                    processor.snapshot_failed(
                        monotonic(),
                        &format!("task snapshots cannot read the program: {error}"),
                    );
                }
                Err(_) => processor.snapshot_failed(
                    monotonic(),
                    "task snapshots cannot read the program: reading it panicked",
                ),
            }
        }

        let Some(snapshotter) = &mut self.snapshotter else {
            return;
        };

        if self.live {
            // The program runs on, and the samples it writes meanwhile
            // arrive as any do.
            let start = monotonic();
            let late = std::time::Instant::now().saturating_duration_since(self.next);
            let modules = processor.modules_of(Pid::new(self.pid));
            let code = processor.mapped_code(Pid::new(self.pid));
            let taken = snapshotter.take_live(self.reads, &modules, &code);
            // Reads keep their rate, whatever each takes up to the
            // interval. One that ran late is followed an interval after it
            // ended, so that reads do not bunch on a loaded machine, and
            // none makes up for those missed.
            let now = std::time::Instant::now();
            self.next += self.every;
            if self.next <= now {
                self.next = now + self.every;
            }

            // A program that exited, with which the recording ends, has no
            // tasks.
            if let Live::Taken(snapshot) = taken {
                let end = monotonic();
                let weight = start.saturating_sub(self.previous);
                processor.live_task_snapshot(Pid::new(self.pid), (start, end), weight, &snapshot);
                self.previous = start;
                // A snapshot keeps its first problem whole, which may be
                // another, so falling behind is a note of its own: the read
                // did not fail, it was late.
                let took = Duration::from_nanos(end.saturating_sub(start));
                if let Some(note) = fell_behind(late, took, self.every) {
                    processor.snapshot_noted(start, &note);
                }
            }

            return;
        }

        if sampling {
            events.enable(false);
        }

        let modules = processor.modules_of(Pid::new(self.pid));
        let start = monotonic();
        let taken = snapshotter.take(self.reads, &modules);
        let end = monotonic();

        // With the events off, every record from before the pause is in
        // the rings, and the snapshot claims each sample before it.
        let mut records = Vec::new();
        events.drain(&mut records);
        processor.ingest(&records, Some(start));
        if sampling {
            events.enable(true);
        }

        self.next = std::time::Instant::now() + self.every;

        match taken {
            Ok(snapshot) => {
                let weight = start.saturating_sub(self.previous);
                processor.task_snapshot(Pid::new(self.pid), (start, end), weight, &snapshot);
                self.previous = start;
            }

            // A program that exited is not attached to, and has no tasks.
            Err(_) if tasks::exited(self.pid) => {}
            Err(error) => {
                processor.task_snapshot(Pid::new(self.pid), (start, end), 0, &Snapshot::default());
                processor.snapshot_failed(start, &format!("a task snapshot failed: {error}"));
            }
        }
    }
}

/// Samples a share of each budget period, in one span at a random offset
/// so that it cannot keep step with a periodic program, and asks the
/// controller once a period how large the next share is.
struct Pacer {
    budget: Budget,
    controller: Controller,

    /// When the period began, and the span of it to sample.
    period: u64,
    window: (u64, u64),

    /// Since when sampling has been on, or off.
    on_since: Option<u64>,
    paused_since: Option<u64>,

    /// What the period has sampled and cost so far.
    sampled: u64,
    cpu: Duration,
    loading: Duration,
    random: u64,
}

impl Pacer {
    fn new(budget: Budget, now: u64, processor: &record::Processor) -> Self {
        Self {
            budget,
            controller: Controller::default(),
            period: now,
            window: (now, u64::MAX),
            on_since: Some(now),
            paused_since: None,
            sampled: 0,
            cpu: record::thread_cpu(),
            loading: processor.loading(),
            random: now | 1,
        }
    }

    /// Whether sampling is on.
    const fn sampling(&self) -> bool {
        self.on_since.is_some()
    }

    /// Turns sampling on or off as the schedule says at `now`, and says
    /// how long until it next changes.
    fn pace(
        &mut self,
        now: u64,
        events: &perf::Events,
        processor: &mut record::Processor,
    ) -> Duration {
        let length = u64::try_from(self.budget.period.as_nanos())
            .unwrap_or(u64::MAX)
            .max(1);
        if now >= self.period.saturating_add(length) {
            self.next_period(now, length, processor);
        }

        let (from, until) = self.window;
        let wanted = from <= now && now < until;
        if wanted && let Some(paused) = self.paused_since.take() {
            processor.gap(paused, monotonic(), GapReason::Budget);
            events.enable(true);
            self.on_since = Some(monotonic());
        } else if !wanted && let Some(on) = self.on_since.take() {
            events.enable(false);
            let off = monotonic();
            self.sampled += off.saturating_sub(on);
            self.paused_since = Some(off);
        }

        let next = [from, until, self.period.saturating_add(length)]
            .into_iter()
            .find(|edge| *edge > now)
            .unwrap_or(now);
        Duration::from_nanos(next - now)
    }

    /// Ends the period at `now`, lets the controller choose the share of
    /// the next one, and places its window.
    fn next_period(&mut self, now: u64, length: u64, processor: &mut record::Processor) {
        if let Some(on) = &mut self.on_since {
            self.sampled += now.saturating_sub(*on);
            *on = now;
        }

        let cpu = record::thread_cpu();
        let loading = processor.loading();
        let observed = Observation {
            cpu: cpu
                .saturating_sub(self.cpu)
                .saturating_sub(loading.saturating_sub(self.loading)),
            sampled: Duration::from_nanos(self.sampled),
            period: Duration::from_nanos(now.saturating_sub(self.period)),
        };

        (self.cpu, self.loading, self.sampled, self.period) = (cpu, loading, 0, now);
        if let Some(decision) = self.controller.step(self.budget, observed) {
            let text = format!(
                "sampling {:.1}% of each period: recording used {:.2}% of a CPU, against a budget of {:.2}%",
                f64::from(decision.steps) * 100.0 / f64::from(STEPS),
                decision.used * 100.0,
                self.budget.cpu * 100.0
            );
            record!("record: budget: {text}");
            processor
                .builder()
                .marker(now.cast_signed(), MarkerKind::Budget, &text);
        }

        let span = length / u64::from(STEPS) * u64::from(self.controller.steps());

        // xorshift: a period's offset needs only to be unpredictable.
        self.random ^= self.random << 13;
        self.random ^= self.random >> 7;
        self.random ^= self.random << 17;
        let offset = self.random % (length - span + 1);
        self.window = (now + offset, now + offset + span);
    }

    /// Ends the recording at `now`: a pause it ends in is a gap to the end.
    fn finish(self, now: u64, processor: &mut record::Processor) {
        if let Some(paused) = self.paused_since {
            processor.gap(paused, now, GapReason::Budget);
        }
    }
}

/// The event that takes `source`'s samples in `program`, and for a
/// breakpoint, where it counts calls. A breakpoint's addresses assume the
/// program loads at `bias`, or where it does without address randomization.
fn perf_source(
    program: &Path,
    source: &SampleSource,
    bias: Option<crate::LoadBias>,
) -> Result<(perf::Source, Option<Calls>), RecordError> {
    Ok(match source {
        SampleSource::Clock { rate } => (
            perf::Source::Clock {
                period: Duration::from_nanos(1_000_000_000 / u64::from((*rate).max(1))),
            },
            None,
        ),
        SampleSource::Function { symbol } => {
            let calls = calls::plan(program, symbol, bias)?;
            (
                perf::Source::Breakpoint {
                    address: calls.loaded().get(),
                },
                Some(calls),
            )
        }
    })
}

/// The bytes of stack each sample of `program` copies, and why.
fn stack_copy(
    processor: &mut record::Processor,
    program: &Path,
    stack: StackCopy,
) -> (u32, String) {
    let bytes = match stack {
        StackCopy::Bytes(bytes) => return (bytes.next_multiple_of(8), "as asked".into()),
        StackCopy::Auto => DEEP_STACK_COPY,
    };

    let Some(path) = which(program).and_then(|path| std::fs::canonicalize(path).ok()) else {
        return (bytes, "auto: the program was not found".into());
    };

    match processor.call_sites(&path) {
        None => (bytes, "auto: the program's code could not be read".into()),
        Some(Sites::Dynamic) => (
            bytes,
            "auto: the program loads libraries, which are known only once it runs".into(),
        ),
        Some(Sites::Static(sites, _)) if sites.keep_the_chain() => (
            SMALL_STACK_COPY,
            "auto: the program keeps the frame-pointer chain where it calls".into(),
        ),
        Some(Sites::Static(_, function)) => (
            bytes,
            format!(
                "auto: {} calls without keeping the frame-pointer chain",
                function.as_deref().unwrap_or("a function")
            ),
        ),
    }
}

/// How far behind a drain records are processed.
const WATERMARK_DELAY: u64 = 20_000_000;

/// Now, by `CLOCK_MONOTONIC`, which times what perf records.
pub(super) fn monotonic() -> u64 {
    nix::time::clock_gettime(nix::time::ClockId::CLOCK_MONOTONIC).map_or(0, |time| {
        u64::try_from(time.tv_sec()).unwrap_or(0) * 1_000_000_000
            + u64::try_from(time.tv_nsec()).unwrap_or(0)
    })
}

/// The file a command names, as `execvp` would find it.
pub(super) fn which(program: &Path) -> Option<PathBuf> {
    if program.components().count() > 1 {
        return Some(program.to_owned());
    }

    std::env::var_os("PATH").and_then(|path| {
        std::env::split_paths(&path)
            .map(|directory| directory.join(program))
            .find(|candidate| candidate.is_file())
    })
}

/// The system call trace a recording runs beside its samples, on the
/// same clock, with the BPF engine.
struct SyscallTrace {
    stop: std::sync::Arc<AtomicBool>,
    thread: std::thread::JoinHandle<Result<(), String>>,
    shared: std::sync::Arc<std::sync::Mutex<Shared>>,
    stacks: Option<SharedStacks>,
}

/// What a recording keeps of its trace until the profile is taken.
enum Kept {
    /// Every call, for a profile of the whole recording.
    All(Box<SyscallCapture>),

    /// The calls since the last segment.
    Since(std::collections::VecDeque<crate::syscalls::TraceEvent>),

    /// The calls of the last span of this many nanoseconds.
    Window(u64, std::collections::VecDeque<crate::syscalls::TraceEvent>),
}

impl Kept {
    /// Keeps `event`, letting a window's oldest go.
    fn keep(&mut self, event: crate::syscalls::TraceEvent) {
        match self {
            Self::All(capture) => capture.event(&event),
            Self::Since(events) => events.push_back(event),
            Self::Window(span, events) => {
                events.push_back(event);
                Self::trim(*span, events, monotonic());
            }
        }
    }

    fn trim(
        span: u64,
        events: &mut std::collections::VecDeque<crate::syscalls::TraceEvent>,
        now: u64,
    ) {
        let since = now.saturating_sub(span);
        while events.front().is_some_and(|event| event.time() < since) {
            events.pop_front();
        }
    }
}

/// The trace's settings and what it keeps, shared by its sink and the
/// recording that cuts it.
struct Shared {
    kept: Kept,
    settings: Vec<(String, String)>,
}

/// The trace's sink, which says when the trace sees its process.
struct Held {
    shared: std::sync::Arc<std::sync::Mutex<Shared>>,
    ready: Option<std::sync::mpsc::Sender<()>>,
}

impl Held {
    fn shared(&self) -> std::sync::MutexGuard<'_, Shared> {
        self.shared
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl Sink for Held {
    fn event(&mut self, event: crate::syscalls::TraceEvent) {
        self.shared().kept.keep(event);
    }

    fn attached_by(&mut self, attachment: &crate::syscall_engines::bpf::Attachment) {
        let mut shared = self.shared();
        shared
            .settings
            .push(("attach".into(), attachment.name().into()));
        shared
            .settings
            .push(("attach reason".into(), attachment.reason.clone()));
        if let Kept::All(capture) = &mut shared.kept {
            capture.setting("attach", attachment.name());
            capture.setting("attach reason", &attachment.reason);
        }
    }

    fn attached(&mut self, _process: u32) {
        if let Some(ready) = self.ready.take() {
            let _ = ready.send(());
        }
    }
}

impl SyscallTrace {
    /// Traces `calls` of process `pid`, held at its gate, and every
    /// process it starts; returns once the trace sees it.
    fn start(
        pid: u32,
        calls: &crate::syscalls::CallSet,
        recording: &RecordOptions,
    ) -> Result<Self, String> {
        let options = TraceOptions {
            calls: calls.clone(),
            limits: crate::syscalls::captures::Limits::default(),
            verbose: false,
            follow: true,
            stacks: Some(SharedStacks::default()),
        };

        let stacks = options.stacks.clone();
        let kept = match (recording.window, recording.segment) {
            (Some(window), _) => Kept::Window(
                u64::try_from(window.as_nanos()).unwrap_or(u64::MAX),
                std::collections::VecDeque::new(),
            ),
            (None, Some(_)) => Kept::Since(std::collections::VecDeque::new()),
            (None, None) => {
                let mut capture = SyscallCapture::new(stacks.clone());
                capture.setting("syscalls", "bpf");
                Kept::All(Box::new(capture))
            }
        };

        let shared = std::sync::Arc::new(std::sync::Mutex::new(Shared {
            kept,
            settings: vec![("syscalls".into(), "bpf".into())],
        }));
        let stop = std::sync::Arc::new(AtomicBool::new(false));
        let (ready, seen) = std::sync::mpsc::channel();
        let stopped = std::sync::Arc::clone(&stop);
        let mut sink = Held {
            shared: std::sync::Arc::clone(&shared),
            ready: Some(ready),
        };

        let thread = std::thread::Builder::new()
            .name("microscope-record-syscalls".into())
            .spawn(move || {
                bpf::trace_held(pid, &options, BpfOptions::default(), &stopped, &mut sink)
                    .map(|_| ())
                    .map_err(|error| error.to_string())
            })
            .map_err(|error| error.to_string())?;
        if seen.recv().is_ok() {
            return Ok(Self {
                stop,
                thread,
                shared,
                stacks,
            });
        }

        // The trace ended before it saw the process: why is its error.
        Err(match thread.join() {
            Ok(Err(why)) => why,
            _ => "the trace ended before it began".to_owned(),
        })
    }

    /// `samples` with the calls of their span: a segment's since the last,
    /// or a window's last span.
    fn cut(&self, samples: &Capture) -> Capture {
        cut(&self.shared, self.stacks.as_ref(), samples)
    }

    /// Ends the trace, its program having ended, and merges it with the
    /// samples.
    fn merge(self, samples: &Capture) -> Result<Capture, String> {
        // Whatever the program's last calls sent is in the ring, which the
        // trace drains before it ends; a process the program left running
        // goes on untraced.
        let Self {
            stop,
            thread,
            shared,
            stacks,
        } = self;
        stop.store(true, Ordering::Relaxed);
        thread
            .join()
            .map_err(|_| "the system call trace panicked".to_owned())??;
        let kept = {
            let mut shared = shared
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            std::mem::replace(
                &mut shared.kept,
                Kept::Since(std::collections::VecDeque::new()),
            )
        };

        match kept {
            Kept::All(capture) => {
                let mut builder = CaptureBuilder::new();
                builder.absorb(samples);
                builder.absorb(&capture.finish().capture);
                Ok(builder.finish())
            }
            kept => {
                shared
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .kept = kept;
                Ok(cut(&shared, stacks.as_ref(), samples))
            }
        }
    }
}

/// `samples` with the calls `shared` keeps for their span.
fn cut(
    shared: &std::sync::Mutex<Shared>,
    stacks: Option<&SharedStacks>,
    samples: &Capture,
) -> Capture {
    let mut capture = SyscallCapture::new(stacks.cloned());
    {
        let mut shared = shared
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for (key, value) in &shared.settings {
            capture.setting(key, value);
        }

        match &mut shared.kept {
            Kept::All(_) => return samples.clone(),
            Kept::Since(events) => {
                for event in events.drain(..) {
                    capture.event(&event);
                }
            }
            Kept::Window(span, events) => {
                Kept::trim(*span, events, monotonic());
                for event in events.iter() {
                    capture.event(event);
                }
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
    use std::time::Duration;

    /// A live read says reads ran less often than asked once the next was
    /// due before it ended, whether it took long or began late, as on a
    /// machine too busy to wake the recorder in time.
    #[test]
    fn a_live_read_that_ends_after_the_next_was_due_says_so() {
        let ms = Duration::from_millis;
        let every = ms(1);
        assert!(super::fell_behind(ms(0), Duration::from_micros(300), every).is_none());
        assert!(
            super::fell_behind(ms(0), ms(3), every).is_some(),
            "took long"
        );
        let late = super::fell_behind(ms(3), Duration::from_micros(200), every);
        assert!(
            late.is_some_and(|note| note.starts_with("a live read took")),
            "began late"
        );
    }
}
