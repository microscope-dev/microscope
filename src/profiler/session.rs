use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use super::budget::{Budget, Controller, Observation, STEPS, WARM_UP};
use super::calls::{self, Calls};
use super::collector::{Collector, ProbeSelection};
use super::interpreted::{Layout, Walked};
use super::ladder::{HOLD, Ladder, Move, Period, Rung};
use super::record::{self, Sites};
use super::tasks::{self, Live, Reads, Snapshot, Snapshotter};
use super::{
    Cuts, DEEP_STACK_COPY, Profile, PythonEngine, RecordError, RecordOptions, SMALL_STACK_COPY,
    SampleSource, StackCopy,
};

use super::syscall_trace::{SyscallEngine, SyscallLevel, SyscallTrace, Traced};
use crate::backend::bpf::sampler::{Sampler, SamplerError};
use crate::backend::perf;
use crate::capture::records::{AccountKind, GapReason, MarkerKind};
use crate::model::Pid;

#[derive(Debug)]
pub struct Recording {
    pub profile: Profile,
    pub status: ExitStatus,
}

#[derive(Debug)]
pub struct Attachment {
    pub profile: Profile,

    pub exited: bool,
}

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

pub struct Watch<'a> {
    pub every: Duration,

    pub publish: &'a mut dyn FnMut(Profile),

    pub stop: &'a AtomicBool,

    pub save: Option<Save<'a>>,
}

pub struct Save<'a> {
    pub trigger: &'a AtomicBool,
    pub to: &'a mut dyn FnMut(Profile),
}

fn ready(command: &mut Command, options: &RecordOptions, processor: &mut record::Processor) {
    if options.probes == ProbeSelection::All {
        command.env(
            microscope_probes::abi::RECORDER,
            std::process::id().to_string(),
        );
    }

    let arguments = std::iter::once(command.get_program()).chain(command.get_args());
    processor
        .builder()
        .setting(super::ARGUMENTS, &super::encode_arguments(arguments));
}

pub fn record_watched(
    mut command: Command,
    options: &RecordOptions,
    watch: &mut Watch<'_>,
) -> Result<Recording, RecordError> {
    let _span = crate::span!("record");
    let options = &resolved(options)?;
    if options.kernel_stacks || options.python == Some(PythonEngine::Bpf) {
        crate::backend::bpf::sampler::check_namespace()?;
    }
    let program = PathBuf::from(command.get_program());
    let (source, breakpoint) =
        perf_source(&program, &options.source, None, options.function_period)?;

    let watched = watch.every != Duration::MAX;
    let mut processor = processor(options, breakpoint, !watched);
    ready(&mut command, options, &mut processor);
    let (stack_bytes, stack_copy) = stack_copy(&mut processor, &program, options.stack);
    let events = perf::EventOptions {
        source,
        stack_bytes,
        kernel: options.kernel_stacks,
        ring_pages: options.ring_pages,
        switches: options.off_cpu,
    };

    if let perf::Source::Breakpoint { period, .. } = source
        && period > 1
    {
        processor.fixed_period(period);
    }

    let launch = launch_options(options, breakpoint.is_some());

    let mut syscalls = None;
    let mut walker = None;
    let mut refused = None;
    let launched = perf::launch(command, &events, &launch, |pid, events| {
        if options.kernel_stacks || options.python == Some(PythonEngine::Bpf) {
            match Walker::attach(events, &mut processor, options.kernel_stacks) {
                Ok(mut attached) => {
                    attached.lay_out_before(&mut processor, pid, &program);
                    walker = Some(attached);
                }
                Err(error) => {
                    let why = error.to_string();
                    refused = Some(error);
                    return Err(why);
                }
            }
        }

        syscalls = SyscallTrace::beside(Traced::Held(pid), options, stack_bytes, watched)?;

        Ok(())
    });

    if let Some(error) = refused {
        return Err(error.into());
    }

    let perf::Launched {
        pid,
        events,
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

    let started = executed.recv().is_ok().then_some(pid);
    events.read_while(|| {
        drive(
            &mut processor,
            &events,
            watch,
            options,
            started,
            syscalls.as_ref(),
            walker.as_mut(),
            |timeout| killer.exited(timeout),
            || {
                record!("record: stopping, killing {pid}");
                killer.kill();
                false
            },
        );
    });
    processor.collection_cpu(events.reader_cpu());

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

fn launch_options(options: &RecordOptions, fixed: bool) -> perf::LaunchOptions {
    let ptraced = options.syscalls.engine == SyscallEngine::Ptrace
        && options.syscalls.level != SyscallLevel::Off;
    perf::LaunchOptions {
        no_address_randomization: fixed,
        filter: ptraced
            .then(|| crate::backend::syscall_engines::seccomp::filter(&options.syscalls.traced())),
    }
}

pub fn record_attached(
    pid: u32,
    options: &RecordOptions,
    watch: &mut Watch<'_>,
) -> Result<Attachment, RecordError> {
    let _span = crate::span!("record attached");
    let options = &resolved(options)?;
    if options.kernel_stacks || options.python == Some(PythonEngine::Bpf) {
        crate::backend::bpf::sampler::check_namespace()?;
    }
    let program = PathBuf::from(format!("/proc/{pid}/exe"));
    let bias = perf::program_bias(pid);
    let (source, breakpoint) =
        perf_source(&program, &options.source, bias, options.function_period)?;
    let watched = watch.every != Duration::MAX;
    let mut processor = processor(options, breakpoint, !watched);
    let (stack_bytes, stack_copy) = stack_copy(&mut processor, &program, options.stack);
    let events = perf::EventOptions {
        source,
        stack_bytes,
        kernel: options.kernel_stacks,
        ring_pages: options.ring_pages,
        switches: options.off_cpu,
    };

    if let perf::Source::Breakpoint { period, .. } = source
        && period > 1
    {
        processor.fixed_period(period);
    }

    let held = options.kernel_stacks || options.python == Some(PythonEngine::Bpf);
    let perf::Attached { events, process } = perf::attach(pid, &events, options.max_events, held)?;
    let mut walker = if held {
        Some(Walker::attach(
            &events,
            &mut processor,
            options.kernel_stacks,
        )?)
    } else {
        None
    };

    let syscalls = SyscallTrace::beside(Traced::Running(pid), options, stack_bytes, watched)
        .map_err(RecordError::Syscalls)?;
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
    if let Some(walker) = &mut walker {
        walker.refresh(&mut processor);
        events.start();
    }

    let mut stopped = false;
    events.read_while(|| {
        drive(
            &mut processor,
            &events,
            watch,
            options,
            Some(pid),
            syscalls.as_ref(),
            walker.as_mut(),
            |timeout| process.exited(timeout),
            || {
                record!("record: stopping, detaching from {pid}");
                stopped = true;
                true
            },
        );
    });
    processor.collection_cpu(events.reader_cpu());

    record!("record: detached from {pid}, stopped {stopped}");
    if !stopped {
        processor.exited(Pid::new(pid));
    }

    let mut capture = processor.finish();
    if let Some(syscalls) = syscalls {
        capture = syscalls.merge(&capture).map_err(RecordError::Syscalls)?;
    }

    Ok(Attachment {
        profile: Profile { capture },
        exited: !stopped,
    })
}

const EARLY_DRAINS: Duration = Duration::from_millis(50);

const WALK_REFRESH: Duration = Duration::from_millis(20);

const WALK_RECHECK: Duration = Duration::from_secs(1);

struct Walker {
    sampler: Sampler,
    next: std::time::Instant,

    every: Duration,
    changes: u64,
    #[expect(
        clippy::type_complexity,
        reason = "each process's pid, program, and threads"
    )]
    told: Vec<(u32, u64, Vec<(u32, u64, u64)>)>,

    laid_out: std::collections::BTreeMap<u32, u64>,
    refused: std::collections::BTreeSet<u32>,

    troubled: std::collections::BTreeSet<u32>,
}

impl From<SamplerError> for RecordError {
    fn from(error: SamplerError) -> Self {
        Self::Python(error.to_string())
    }
}

impl Walker {
    fn attach(
        events: &perf::Events,
        processor: &mut record::Processor,
        kernel: bool,
    ) -> Result<Self, SamplerError> {
        let _span = crate::span!("record bpf load");
        let (sampler, outputs) = Sampler::load(events, kernel)?;
        processor.interpret(outputs);
        Ok(Self {
            sampler,
            next: std::time::Instant::now(),
            every: WALK_REFRESH,
            changes: 0,
            told: Vec::new(),
            laid_out: std::collections::BTreeMap::new(),
            refused: std::collections::BTreeSet::new(),
            troubled: std::collections::BTreeSet::new(),
        })
    }

    fn refresh(&mut self, processor: &mut record::Processor) {
        for walked in processor.walked_early() {
            if !self.laid_out.contains_key(&walked.pid) {
                self.lay_out(processor, &walked);
            }
        }

        let now = std::time::Instant::now();
        let changes = processor.thread_changes();
        if changes != self.changes {
            self.changes = changes;
            self.every = WALK_REFRESH;
            self.next = self.next.min(now + WALK_REFRESH);
        }
        if now < self.next {
            return;
        }

        let walked = processor.walked();
        let told: Vec<_> = walked
            .iter()
            .map(|walked| (walked.pid, walked.program, walked.threads.clone()))
            .collect();
        self.every = if told == self.told {
            (self.every * 2).min(WALK_RECHECK)
        } else {
            WALK_REFRESH
        };
        self.told = told;
        self.next = now + self.every;
        let sampler = &mut self.sampler;
        self.laid_out.retain(|pid, program| {
            let fits = walked
                .iter()
                .any(|walked| walked.pid == *pid && walked.program == *program);
            if !fits {
                sampler.forget(*pid);
            }

            fits
        });

        for walked in walked {
            if !self.laid_out.contains_key(&walked.pid) && !self.lay_out(processor, &walked) {
                continue;
            }

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

    fn lay_out_before(&mut self, processor: &mut record::Processor, pid: u32, program: &Path) {
        let Some(path) = which(program).and_then(|path| std::fs::canonicalize(path).ok()) else {
            return;
        };
        let Some(layout) = processor.program_v8_layout(&path) else {
            return;
        };
        match self.sampler.set_v8_layout(pid, &layout) {
            Ok(()) => {}
            #[cfg_attr(
                not(debug_assertions),
                expect(unused_variables, reason = "only recorded")
            )]
            Err(error) => record!("walker: {pid}'s layout before it runs was refused: {error}"),
        }
    }

    fn lay_out(&mut self, processor: &mut record::Processor, walked: &Walked) -> bool {
        let set = walked.layout.clone().and_then(|layout| {
            match layout {
                Layout::Python(layout) => self.sampler.set_layout(walked.pid, &layout),
                Layout::V8(layout) => self.sampler.set_v8_layout(walked.pid, &layout),
            }
            .map_err(|error| error.to_string())
        });
        match set {
            Ok(()) => {
                self.laid_out.insert(walked.pid, walked.program);
                true
            }
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

fn processor(
    options: &RecordOptions,
    breakpoint: Option<Calls>,
    defer_debug: bool,
) -> record::Processor {
    let search = crate::debug_info::DebugFileSearch::new(&crate::DebugFileOptions {
        directories: options.debug_directories.clone(),
        ..crate::DebugFileOptions::default()
    });
    let mut processor = record::Processor::new(search, options.max_frames);
    processor.cuts(options.cuts);
    if defer_debug
        && options.task_snapshots.is_none()
        && options.python.is_none()
        && options.cuts == Cuts::Whole
    {
        processor.defer_debug_information();
    }
    if options.symbols_later {
        processor.symbols_later();
    }

    if let Some(calls) = breakpoint
        && let Some(branch) = calls.repeats_when
    {
        processor.repeats_when(calls.loaded().get(), branch);
        processor
            .builder()
            .setting("function retries", "Go stack checks");
    }

    processor
}

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
    if let SampleSource::Function { .. } = options.source {
        builder.setting("function stacks", "sampled");
    }

    for (key, value) in options.syscalls.settings() {
        builder.setting(key, &value);
    }
    if ring_pages < options.ring_pages.next_power_of_two() {
        builder.setting(
            "ring warning",
            &format!(
                "perf rings shrank from {} to {ring_pages} pages per CPU ({} KiB); raise ulimit -l or kernel.perf_event_mlock_kb",
                options.ring_pages.next_power_of_two(),
                ring_pages * perf::sys::page_size() / 1024,
            ),
        );
    }

    for (key, value) in [
        ("program", program.to_string_lossy().into_owned()),
        ("pid", pid.to_string()),
        ("ring pages", ring_pages.to_string()),
        ("requested ring pages", options.ring_pages.to_string()),
        (
            "kernel stacks",
            if options.kernel_stacks { "on" } else { "off" }.into(),
        ),
        ("source", options.source.to_string()),
        ("recorder", env!("MICROSCOPE_SOURCES_DIGEST").to_owned()),
        ("weight", options.source.unit().to_owned()),
        ("stack bytes", stack_bytes.to_string()),
        ("stack copy", stack_copy),
        ("off-CPU", if options.off_cpu { "on" } else { "off" }.into()),
        (
            "window",
            match options.cuts {
                Cuts::Window(window) => format!("the last {:.3} s", window.as_secs_f64()),
                Cuts::Whole | Cuts::Segments(_) => "all".into(),
            },
        ),
        (
            "segments",
            match options.cuts {
                Cuts::Segments(every) => format!("every {:.3} s", every.as_secs_f64()),
                Cuts::Whole | Cuts::Window(_) => "off".into(),
            },
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

fn resolved(options: &RecordOptions) -> Result<RecordOptions, RecordError> {
    let mut options = options.clone();
    options.syscalls.resolve().map_err(RecordError::Syscalls)?;
    let stops = (options.task_snapshots.is_some() && !options.live_snapshots)
        || matches!(options.python, Some(PythonEngine::Snapshot { .. }));
    if options.syscalls.engine == SyscallEngine::Ptrace
        && options.syscalls.level != SyscallLevel::Off
        && stops
    {
        return Err(RecordError::Syscalls(
            "the ptrace engine and snapshots that stop the program both need to trace it; \
             take live snapshots, or record system calls with the BPF engine"
                .to_owned(),
        ));
    }

    Ok(options)
}

#[expect(
    clippy::too_many_arguments,
    clippy::too_many_lines,
    reason = "the recording loop keeps collection, processing, and publication in order"
)]
fn drive(
    processor: &mut record::Processor,
    events: &perf::Events,
    watch: &mut Watch<'_>,
    options: &RecordOptions,
    started: Option<u32>,
    syscalls: Option<&SyscallTrace>,
    mut walker: Option<&mut Walker>,
    mut ended: impl FnMut(Duration) -> bool,
    mut stop: impl FnMut() -> bool,
) {
    processor.set_up();
    let mut pacer = options
        .budget
        .filter(|_| started.is_some())
        .map(|budget| Pacer::new(budget, monotonic(), processor, syscalls));
    let mut snapshots = started.map_or_else(Vec::new, |pid| snapshotters(pid, options));
    let mut probes = started
        .filter(|_| options.probes != ProbeSelection::Off)
        .map(|pid| Collector::new(pid, options.probes.clone(), options.probe_timeout));
    let started = started.is_some();

    let observing = match options.cuts {
        Cuts::Whole => None,
        Cuts::Window(span) | Cuts::Segments(span) => Some(
            (span / 8)
                .min(watch.every)
                .min(Duration::from_millis(250))
                .max(Duration::from_millis(10)),
        ),
    };
    if observing.is_some() {
        processor.observe(monotonic(), events.counts());
    }
    let mut observed = std::time::Instant::now();
    let mut segments = match options.cuts {
        Cuts::Segments(every) => Some((every, std::time::Instant::now())),
        Cuts::Whole | Cuts::Window(_) => None,
    };
    let mut pending_save = None;
    let watched = watch.every != Duration::MAX;
    let live = |processor: &mut record::Processor| {
        let capture = processor.snapshot();
        Profile {
            capture: match syscalls {
                Some(trace) if watched => trace.live(&capture),
                _ => capture,
            },
        }
    };
    if started {
        (watch.publish)(live(processor));
    }

    let mut published = std::time::Instant::now();
    let begun = std::time::Instant::now();
    let mut drained_at = begun;
    let mut wait = watch.every.min(Duration::from_millis(250));
    let mut stopping = false;
    let mut now = false;
    let end = loop {
        if !stopping && watch.stop.load(Ordering::Relaxed) {
            stopping = true;
            now = stop();
        }

        let finished = now || ended(Duration::ZERO);
        let disabled_at = finished.then(|| {
            events.stop();
            monotonic()
        });

        if !finished && observing.is_some_and(|every| observed.elapsed() >= every) {
            processor.observe(monotonic(), events.counts());
            observed = std::time::Instant::now();
        }

        let watermark = monotonic().saturating_sub(WATERMARK_DELAY);
        if !finished {
            let mut wait = if walker.is_some() && begun.elapsed() < EARLY_DRAINS {
                Duration::from_millis(1)
            } else {
                Duration::from_millis(10)
            }
            .saturating_sub(drained_at.elapsed());
            if let Some(pacer) = &mut pacer {
                let edge = pacer.pace(monotonic(), processor, |on| {
                    let toggling = monotonic();
                    events.hold(perf::Hold::Budget, !on);
                    (toggling, monotonic())
                });
                wait = if pacer.sampling() {
                    wait.min(edge)
                } else {
                    edge.min(Duration::from_millis(100))
                };
            }
            let due = snapshots.iter().map(Snapshots::due_in).min();
            let wait = if processor.stopped_at_load() {
                Duration::ZERO
            } else {
                due.map_or(wait, |due| wait.min(due))
            };
            if pacer.as_ref().is_none_or(Pacer::sampling) {
                events.wait(wait);
            } else {
                ended(wait);
            }
        }

        let mut drained = 0;
        drained_at = std::time::Instant::now();
        {
            let _span = crate::span!("record drain");
            let drain = |blocks: &mut perf::Blocks| drained = events.drain_blocks(blocks);
            if probes.is_some() && !finished {
                processor.ingest_drained_after_probes(drain, watermark);
            } else {
                processor.ingest_drained(drain, (!finished).then_some(watermark));
            }
        }
        record!("record: drained {drained} bytes, finished {finished}");
        for (start, end) in events.behind(None) {
            processor.gap(start, end, GapReason::Behind);
        }
        processor.collection_cpu(events.reader_cpu());
        if let Some(walker) = &mut walker
            && pacer.as_ref().is_none_or(Pacer::sampling)
        {
            walker.refresh(processor);
        }

        if let Some(probes) = &mut probes {
            if finished {
                probes.finish(processor, !ended(Duration::ZERO));
            } else {
                probes.drain(processor);
            }
        }
        processor.load_stopped();

        if let Some(end) = disabled_at {
            break end;
        }

        if !processor.stopped_at_load() {
            for snapshots in &mut snapshots {
                snapshots.when_due(processor, events);
            }
        }

        let triggered = processor.take_triggered();
        let called = syscalls.is_some_and(SyscallTrace::take_trigger);
        if let Some(save) = &mut watch.save {
            save_when_due(
                save,
                processor,
                events,
                syscalls,
                triggered || called,
                &mut pending_save,
                &mut segments,
            );
        }

        if started && published.elapsed() >= wait {
            if observing.is_none() {
                processor.observe(monotonic(), events.counts());
            }
            (watch.publish)(live(processor));
            published = std::time::Instant::now();
            wait = (wait * 2).min(watch.every);
        }
    };

    for (start, end) in events.behind(Some(end)) {
        processor.gap(start, end, GapReason::Behind);
    }
    if let Some(pacer) = pacer {
        pacer.finish(end, processor);
    }

    if let Some(walker) = walker {
        walker.finish(processor);
    }

    for snapshots in &mut snapshots {
        snapshots.finish(processor);
    }
    processor.observe(end, events.counts());
    if observing.is_some() {
        processor.ingest(&[], None);
    }
    let called = syscalls.is_some_and(SyscallTrace::take_trigger);
    if (pending_save.is_some() || called)
        && let Some(save) = &mut watch.save
    {
        save_when_due(
            save,
            processor,
            events,
            syscalls,
            called,
            &mut pending_save,
            &mut segments,
        );
    }
}

fn save_when_due(
    save: &mut Save<'_>,
    processor: &mut record::Processor,
    events: &perf::Events,
    syscalls: Option<&SyscallTrace>,
    triggered: bool,
    pending: &mut Option<u64>,
    segments: &mut Option<(Duration, std::time::Instant)>,
) {
    let requested = save.trigger.swap(false, Ordering::Relaxed) || triggered;
    if requested && pending.is_none() {
        let at = monotonic();
        processor.observe(at, events.counts());
        *pending = Some(at);
    }

    let due = segments
        .as_ref()
        .is_some_and(|(every, cut)| cut.elapsed() >= *every);
    let ready = pending.map_or(due && processor.observed(0), |at| processor.observed(at));
    if !ready {
        return;
    }

    let _span = crate::span!("record save");
    let Some(capture) = processor.save() else {
        return;
    };
    *pending = None;
    record!("record: saved, segment due {due}");
    if let Some((_, cut)) = segments {
        *cut = std::time::Instant::now();
    }

    let capture = syscalls.map_or_else(|| capture.clone(), |trace| trace.cut(&capture));
    (save.to)(Profile { capture });
}

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

struct Snapshots {
    pid: u32,
    every: Duration,
    live: bool,
    reads: Reads,
    loading: Option<std::thread::JoinHandle<Result<Snapshotter, RecordError>>>,
    load_error: Option<String>,
    snapshotter: Option<Snapshotter>,
    next: std::time::Instant,
    observed: bool,

    previous: u64,
}

impl Snapshots {
    fn new(pid: u32, every: Duration, live: bool, reads: Reads, options: &RecordOptions) -> Self {
        let debug_files = crate::DebugFileOptions {
            directories: options.debug_directories.clone(),
            ..crate::DebugFileOptions::default()
        };

        let program = PathBuf::from(format!("/proc/{pid}/exe"));
        let loading = std::thread::Builder::new()
            .name("microscope-snapshot-load".into())
            .spawn(move || Snapshotter::new(pid, &program, &debug_files));
        let (loading, load_error) = match loading {
            Ok(loading) => (Some(loading), None),
            Err(error) => (
                None,
                Some(format!("cannot start task snapshot loader: {error}")),
            ),
        };

        Self {
            pid,
            every,
            live,
            reads,
            loading,
            load_error,
            snapshotter: None,
            next: std::time::Instant::now() + every,
            observed: false,
            previous: monotonic(),
        }
    }

    fn due_in(&self) -> Duration {
        if self.snapshotter.is_none() {
            return Duration::MAX;
        }
        self.next
            .saturating_duration_since(std::time::Instant::now())
    }

    fn loaded(&mut self, processor: &mut record::Processor) {
        if let Some(error) = self.load_error.take() {
            processor.snapshot_failed(monotonic(), &error);
            self.observed = true;
        }
        if let Some(loading) = self.loading.take_if(|loading| loading.is_finished()) {
            match loading.join() {
                Ok(Ok(snapshotter)) => {
                    self.snapshotter = Some(snapshotter);
                    self.next = std::time::Instant::now();
                }

                Ok(Err(error)) => {
                    self.observed = true;
                    processor.snapshot_failed(
                        monotonic(),
                        &format!("task snapshots cannot read the program: {error}"),
                    );
                }
                Err(_) => {
                    self.observed = true;
                    processor.snapshot_failed(
                        monotonic(),
                        "task snapshots cannot read the program: reading it panicked",
                    );
                }
            }
        }
    }

    fn finish(&mut self, processor: &mut record::Processor) {
        self.loaded(processor);
        if !self.observed {
            let why = if self.loading.is_some() {
                "debug information was still loading"
            } else {
                "startup or the requested interval outlasted the recording"
            };
            processor.snapshot_noted(
                monotonic(),
                &format!("no task snapshot was taken before recording ended: {why}; record a longer interval"),
            );
        }
    }

    fn when_due(&mut self, processor: &mut record::Processor, events: &perf::Events) {
        if std::time::Instant::now() < self.next {
            return;
        }
        self.loaded(processor);

        let Some(snapshotter) = &mut self.snapshotter else {
            return;
        };

        if self.live {
            let code = processor.code_for_read(
                Pid::new(self.pid),
                || monotonic().saturating_sub(WATERMARK_DELAY),
                |blocks| {
                    events.drain_blocks(blocks);
                },
            );
            let start = monotonic();
            let late = std::time::Instant::now().saturating_duration_since(self.next);
            let modules = processor.modules_of(Pid::new(self.pid));
            let taken = snapshotter.take_live(self.reads, &modules, &code);
            let now = std::time::Instant::now();
            self.next += self.every;
            if self.next <= now {
                self.next = now + self.every;
            }

            if let Live::Taken(snapshot) = taken {
                self.observed = true;
                let end = monotonic();
                let weight = start.saturating_sub(self.previous);
                processor.live_task_snapshot(Pid::new(self.pid), (start, end), weight, &snapshot);
                processor.map_live_code(Pid::new(self.pid), end, &snapshot.unmapped_code);
                self.previous = start;
                let took = Duration::from_nanos(end.saturating_sub(start));
                if let Some(note) = fell_behind(late, took, self.every) {
                    processor.snapshot_noted(start, &note);
                }
            }

            return;
        }

        events.hold(perf::Hold::Snapshot, true);

        let modules = processor.modules_of(Pid::new(self.pid));
        let start = monotonic();
        let taken = snapshotter.take(self.reads, &modules);
        let end = monotonic();

        processor.ingest_drained(
            |blocks| {
                events.drain_blocks(blocks);
            },
            Some(start),
        );
        events.hold(perf::Hold::Snapshot, false);

        self.next = std::time::Instant::now() + self.every;

        match taken {
            Ok(snapshot) => {
                self.observed = true;
                let weight = start.saturating_sub(self.previous);
                processor.task_snapshot(Pid::new(self.pid), (start, end), weight, &snapshot);
                self.previous = start;
            }

            Err(_) if tasks::exited(self.pid) => {}
            Err(error) => {
                self.observed = true;
                processor.task_snapshot(Pid::new(self.pid), (start, end), 0, &Snapshot::default());
                processor.snapshot_failed(start, &format!("a task snapshot failed: {error}"));
            }
        }
    }
}

fn nanoseconds(duration: Duration) -> u64 {
    u64::try_from(duration.as_nanos())
        .unwrap_or(u64::MAX)
        .max(1)
}

struct Pacer<'a> {
    budget: Budget,
    controller: Controller,

    ladder: Ladder,
    syscalls: Option<&'a SyscallTrace>,

    period: u64,
    length: u64,
    window: (u64, u64),

    toggled: (u64, u64),

    pace: Pace,

    sampled: u64,
    cpu: Duration,
    loading: Duration,
    kernel: Duration,
    calls: u64,

    paused_cpu: Duration,
    paused_at: Duration,
    random: u64,
}

impl<'a> Pacer<'a> {
    fn new(
        budget: Budget,
        now: u64,
        processor: &record::Processor,
        syscalls: Option<&'a SyscallTrace>,
    ) -> Self {
        let rungs = syscalls.map_or_else(Vec::new, SyscallTrace::rungs);
        Self {
            budget,
            controller: Controller::default(),
            ladder: Ladder::new(rungs.into_iter().chain([Rung::Samples])),
            syscalls,
            period: now,
            length: nanoseconds(budget.period),
            window: (now, u64::MAX),
            toggled: (0, 0),
            pace: Pace::On { since: now },
            sampled: 0,
            paused_cpu: Duration::ZERO,
            paused_at: Duration::ZERO,
            cpu: process_cpu(),
            loading: processor.loading(),
            kernel: Duration::ZERO,
            calls: 0,
            random: now | 1,
        }
    }

    const fn sampling(&self) -> bool {
        matches!(self.pace, Pace::On { .. })
    }

    fn pace(
        &mut self,
        now: u64,
        processor: &mut record::Processor,
        mut toggle: impl FnMut(bool) -> (u64, u64),
    ) -> Duration {
        let current = |pacer: &Self| {
            if pacer.controller.warm() {
                pacer.length
            } else {
                pacer.length / u64::from(WARM_UP)
            }
        };
        if now >= self.period.saturating_add(current(self)) {
            let toggling = Duration::from_nanos(self.toggled.0.saturating_add(self.toggled.1));
            self.length = nanoseconds(self.budget.period_for(toggling));
            self.next_period(now, self.length, processor);
        }

        let (from, until) = self.window;
        let wanted = from <= now && now < until;
        match self.pace {
            Pace::Paused { since } if wanted => {
                self.paused_cpu += process_cpu().saturating_sub(self.paused_at);
                let (began, on) = toggle(true);
                processor.gap(since, began, GapReason::Budget);
                self.toggled.0 = on.saturating_sub(began);
                self.pace = Pace::On { since: on };
            }
            Pace::On { since } if !wanted => {
                let (began, off) = toggle(false);
                self.toggled.1 = off.saturating_sub(began);
                self.sampled += off.saturating_sub(since);
                self.paused_at = process_cpu();
                self.pace = Pace::Paused { since: off };
            }
            Pace::On { .. } | Pace::Paused { .. } => {}
        }

        let next = [from, until, self.period.saturating_add(current(self))]
            .into_iter()
            .find(|edge| *edge > now)
            .unwrap_or(now);
        Duration::from_nanos(next - now)
    }

    fn next_period(&mut self, now: u64, length: u64, processor: &mut record::Processor) {
        let cpu = process_cpu();
        match &mut self.pace {
            Pace::On { since } => {
                self.sampled += now.saturating_sub(*since);
                *since = now;
            }
            Pace::Paused { .. } => {
                self.paused_cpu += cpu.saturating_sub(self.paused_at);
                self.paused_at = cpu;
            }
        }

        let loading = processor.loading();
        let (calls, kernel) = self
            .syscalls
            .map_or((0, Duration::ZERO), SyscallTrace::cost);
        let kernel_cost = kernel.saturating_sub(self.kernel);
        let period = Duration::from_nanos(now.saturating_sub(self.period));
        let sampled = Duration::from_nanos(self.sampled);
        let paused = period.saturating_sub(sampled).max(Duration::from_nanos(1));
        let spent = cpu
            .saturating_sub(self.cpu)
            .saturating_sub(loading.saturating_sub(self.loading))
            + kernel_cost;
        let observed = Observation {
            cpu: spent,
            sampled,
            period,
            fixed: (kernel_cost + self.paused_cpu.mul_f64(period.div_duration_f64(paused)))
                .min(spent),
        };

        let traced = calls.saturating_sub(self.calls);
        (self.cpu, self.loading, self.kernel, self.calls) = (cpu, loading, kernel, calls);
        (self.sampled, self.period, self.paused_cpu) = (0, now, Duration::ZERO);
        let warm = self.controller.warm();
        let decision = self.controller.step(self.budget, observed);
        if warm {
            self.climb(now, observed, traced, processor);
        }

        if !self.ladder.sheds(Rung::Samples) {
            self.controller.whole();
        } else if let Some(decision) = decision {
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

        self.random ^= self.random << 13;
        self.random ^= self.random >> 7;
        self.random ^= self.random << 17;
        let offset = self.random % (length - span + 1);
        self.window = (now + offset, now + offset + span);
    }

    fn climb(
        &mut self,
        now: u64,
        observed: Observation,
        traced: u64,
        processor: &mut record::Processor,
    ) {
        let seconds = observed.period.as_secs_f64().max(f64::EPSILON);
        let used = observed.cpu.as_secs_f64() / seconds;
        let period = Period {
            over: used > self.budget.cpu
                || (self.ladder.sheds(Rung::Samples) && self.controller.steps() < STEPS),
            used: used / self.budget.cpu,
            #[expect(clippy::cast_precision_loss, reason = "a rate needs no precision")]
            calls: traced as f64 / seconds,
        };
        let Some(moved) = self.ladder.judge(period, now) else {
            return;
        };

        if let Move::Down(rung) = moved
            && rung != Rung::Samples
        {
            self.controller.forgive();
        }

        let text = match moved {
            Move::Down(rung) => format!(
                "gave up {}: recording used {:.2}% of a CPU, against a budget of {:.2}%",
                rung.text(),
                used * 100.0,
                self.budget.cpu * 100.0
            ),
            Move::Up(rung) => format!(
                "took back {}: recording stayed under its budget for {} s",
                rung.text(),
                HOLD.as_secs()
            ),
        };

        record!("record: budget: {text}");
        processor
            .builder()
            .marker(now.cast_signed(), MarkerKind::Budget, &text);
        if let Some(syscalls) = self.syscalls {
            syscalls.shed(self.ladder.shed());
        }
    }

    fn finish(self, now: u64, processor: &mut record::Processor) {
        if let Pace::Paused { since } = self.pace {
            processor.gap(since, now, GapReason::Budget);
        }
    }
}

fn process_cpu() -> Duration {
    use nix::sys::resource::{UsageWho, getrusage};
    getrusage(UsageWho::RUSAGE_SELF).map_or(Duration::ZERO, |usage| {
        let time = |time: nix::sys::time::TimeVal| {
            Duration::new(
                u64::try_from(time.tv_sec()).unwrap_or(0),
                u32::try_from(time.tv_usec()).unwrap_or(0) * 1_000,
            )
        };

        time(usage.user_time()) + time(usage.system_time())
    })
}

#[derive(Debug, Clone, Copy)]
enum Pace {
    On { since: u64 },
    Paused { since: u64 },
}

fn perf_source(
    program: &Path,
    source: &SampleSource,
    bias: Option<crate::LoadBias>,
    period: u64,
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
                    period: if calls.repeats_when.is_some() {
                        1
                    } else {
                        period.max(1)
                    },
                },
                Some(calls),
            )
        }
    })
}

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

const WATERMARK_DELAY: u64 = 20_000_000;

pub(super) fn monotonic() -> u64 {
    nix::time::clock_gettime(nix::time::ClockId::CLOCK_MONOTONIC).map_or(0, |time| {
        u64::try_from(time.tv_sec()).unwrap_or(0) * 1_000_000_000
            + u64::try_from(time.tv_nsec()).unwrap_or(0)
    })
}

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

#[cfg(test)]
mod tests {
    use std::time::Duration;

    #[test]
    fn a_budget_pause_is_a_gap_from_sampling_off_to_sampling_on() {
        let processor =
            || super::record::Processor::new(crate::debug_info::DebugFileSearch::default(), 64);

        let budget = super::Budget {
            cpu: 0.01,
            period: Duration::from_hours(1),
        };
        let mut toggled = Vec::new();
        let mut pace = |pacer: &mut super::Pacer<'_>, processor: &mut _, now| {
            pacer.pace(now, processor, |on| {
                toggled.push((now, on));
                (now, now + 100)
            })
        };

        let mut paused = processor();
        let mut pacer = super::Pacer::new(budget, 1_000, &paused, None);
        pacer.window = (1_000, 3_000);
        assert_eq!(
            pace(&mut pacer, &mut paused, 1_500),
            Duration::from_nanos(1_500)
        );
        pace(&mut pacer, &mut paused, 3_000);
        pacer.window = (5_000, 7_000);
        pace(&mut pacer, &mut paused, 4_500);
        pace(&mut pacer, &mut paused, 5_000);
        pace(&mut pacer, &mut paused, 7_000);
        pace(&mut pacer, &mut paused, 8_000);
        pacer.finish(9_000, &mut paused);

        let mut sampling = processor();
        let mut pacer = super::Pacer::new(budget, 1_000, &sampling, None);
        pacer.window = (1_000, 3_000);
        pace(&mut pacer, &mut sampling, 2_000);
        pacer.finish(2_500, &mut sampling);

        assert_eq!(toggled, [(3_000, false), (5_000, true), (7_000, false)]);
        let paused = super::Profile {
            capture: paused.finish(),
        };
        assert_eq!(
            paused.paused(super::GapReason::Budget),
            [(3_100, 5_000), (7_100, 9_000)]
        );
        let sampling = super::Profile {
            capture: sampling.finish(),
        };
        assert_eq!(sampling.gaps(), []);
    }

    #[test]
    fn a_program_ending_before_its_first_task_snapshot_explains_the_empty_result() {
        let program = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("build/test-programs/profile-kernel");
        let mut command = std::process::Command::new(program);
        command.arg("2000000");
        let recording = crate::profiler::record(
            command,
            &super::RecordOptions {
                task_snapshots: Some(Duration::from_secs(60)),
                ..super::RecordOptions::default()
            },
        )
        .expect("record short target");
        assert!(recording.status.success());
        let (taken, _, failed) = recording.profile.task_snapshots();
        assert_eq!((taken, failed), (0, 0));
        let notes = recording.profile.task_snapshot_problems();
        assert!(
            notes
                .iter()
                .any(|(_, note)| note.contains("no task snapshot was taken before recording ended")),
            "{notes:?}"
        );
    }

    #[test]
    fn breakpoint_samples_are_read_while_processing_is_busy() {
        use nix::sched::{CpuSet, sched_getaffinity, sched_setaffinity};
        use nix::unistd::Pid;

        let allowed = sched_getaffinity(Pid::from_raw(0)).expect("read allowed CPUs");
        let cpu = (0..CpuSet::count())
            .find(|&cpu| allowed.is_set(cpu).unwrap_or(false))
            .expect("an allowed CPU");
        let mut affinity = CpuSet::new();
        affinity.set(cpu).expect("select one CPU");
        let program = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("build/test-programs/profile-stacks-gcc-o0");
        let (source, _) = super::perf_source(
            &program,
            &super::SampleSource::Function {
                symbol: "leaf".into(),
            },
            None,
            1,
        )
        .expect("find leaf");
        let options = super::perf::EventOptions {
            source,
            stack_bytes: 32768,
            kernel: false,
            ring_pages: 32,
            switches: false,
        };
        sched_setaffinity(Pid::from_raw(0), &affinity).expect("pin the reader");
        let mut command = std::process::Command::new("chrt");
        command.args(["--idle", "0"]).arg(program).arg("1000000");
        let super::perf::Launched { events, exit, .. } = super::perf::launch(
            command,
            &options,
            &super::perf::LaunchOptions {
                no_address_randomization: true,
                filter: None,
            },
            |pid, _| {
                sched_setaffinity(Pid::from_raw(pid.cast_signed()), &affinity)
                    .map_err(|error| error.to_string())
            },
        )
        .expect("launch the program");
        let status = events
            .read_while(|| exit.join())
            .expect("the waiter ends")
            .expect("the program exits");
        assert!(status.success(), "{status}");
        let counts = events.counts();
        assert_eq!(counts.counted, 6, "every call is counted: {counts:?}");
        assert_eq!(counts.lost_samples, 0, "{counts:?}");
        assert_eq!(events.behind(Some(super::monotonic())), []);

        let mut records = Vec::new();
        events.drain(&mut records);
        let mut remaining = records.as_slice();
        let mut samples = 0;
        while let Some(record) = super::perf::decode::next(remaining) {
            let (record, bytes) = record.expect("a whole perf record");
            if matches!(record, super::perf::decode::Record::Sample(_)) {
                samples += 1;
            }
            remaining = &remaining[bytes..];
        }
        assert!(remaining.is_empty(), "no partial record remains");
        assert_eq!(samples, 6, "every counted call has its sample");
    }

    #[test]
    fn collection_at_a_low_rate_stops_at_once() {
        let program = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("build/test-programs/profile-calls");
        let options = super::perf::EventOptions {
            source: super::perf::Source::Clock {
                period: Duration::from_millis(20),
            },
            stack_bytes: 1024,
            kernel: false,
            ring_pages: 8,
            switches: false,
        };
        let mut command = std::process::Command::new(program);
        command.arg("1000000000");
        let super::perf::Launched {
            events,
            exit,
            killer,
            ..
        } = super::perf::launch(
            command,
            &options,
            &super::perf::LaunchOptions {
                no_address_randomization: true,
                filter: None,
            },
            |_, _| Ok(()),
        )
        .expect("launch the program");
        let stopping = events.read_while(|| {
            std::thread::sleep(Duration::from_millis(30));
            std::time::Instant::now()
        });
        let stopped = stopping.elapsed();
        killer.kill();
        let _ = exit.join();
        assert!(stopped < Duration::from_millis(20), "took {stopped:?}");
    }

    #[test]
    fn a_full_breakpoint_queue_keeps_counting_calls() {
        use nix::sched::{CpuSet, sched_getaffinity, sched_setaffinity};
        use nix::unistd::Pid;

        let allowed = sched_getaffinity(Pid::from_raw(0)).expect("read allowed CPUs");
        let cpu = (0..CpuSet::count())
            .find(|&cpu| allowed.is_set(cpu).unwrap_or(false))
            .expect("an allowed CPU");
        let mut affinity = CpuSet::new();
        affinity.set(cpu).expect("select one CPU");
        let program = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("build/test-programs/profile-calls");
        let (source, _) = super::perf_source(
            &program,
            &super::SampleSource::Function {
                symbol: "hot".into(),
            },
            None,
            1,
        )
        .expect("find hot");
        let options = super::perf::EventOptions {
            source,
            stack_bytes: 32768,
            kernel: false,
            ring_pages: 128,
            switches: false,
        };
        let mut command = std::process::Command::new(program);
        command.arg("20000");
        let super::perf::Launched {
            mut events, exit, ..
        } = super::perf::launch(
            command,
            &options,
            &super::perf::LaunchOptions {
                no_address_randomization: true,
                filter: None,
            },
            |pid, _| {
                sched_setaffinity(Pid::from_raw(pid.cast_signed()), &affinity)
                    .map_err(|error| error.to_string())
            },
        )
        .expect("launch the program");
        let queue = 1024 * 1024;
        events.set_queue_bytes(queue);
        let status = events
            .read_while(|| exit.join())
            .expect("the waiter ends")
            .expect("the program exits");
        assert!(status.success(), "{status}");
        let counts = events.counts();
        assert_eq!(counts.counted, 20000, "{counts:?}");
        assert!(counts.lost_samples > 0, "the queue overflows: {counts:?}");
        assert_eq!(events.behind(Some(super::monotonic())), []);
        let mut records = Vec::new();
        events.drain(&mut records);
        assert!(records.len() >= queue, "the queue filled");
        let ring = options.ring_pages * 4096;
        assert!(
            records.len() <= queue + 2 * ring,
            "collection is bounded: {}",
            records.len()
        );
    }

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
