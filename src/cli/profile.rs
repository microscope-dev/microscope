//! `microscope record`, `microscope report`, and `microscope convert`: the
//! sampling profiler from the command line.

use std::ffi::OsString;
use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use super::format::plural;
use anyhow::{Context, Result};
use microscope::profiler::budget::Budget;
use microscope::profiler::{
    Format, GapReason, Granularity, Preset, ProbeSelection, Profile, PythonEngine, RecordOptions,
    SampleSource, Save, Source, StackCopy, StackView, Watch,
};

#[derive(Debug, Clone, Copy, clap::ValueEnum)]
pub enum PresetArg {
    /// 49 samples a second, copying as little stack as unwinds the program:
    /// safe in production.
    Prod,

    /// 999 samples a second with a deep stack copy.
    Dev,

    /// 9999 samples a second.
    Max,
}

#[derive(Debug, Clone, Copy, clap::ValueEnum)]
pub enum SymbolsArg {
    /// From the debug information on this host, as the recording ends.
    Now,

    /// Wherever the profile is opened, from the modules' build ids: this
    /// host keeps only build ids and addresses.
    Later,
}

#[derive(Debug, Clone, Copy, clap::ValueEnum)]
pub enum OnOff {
    On,
    Off,
}

#[derive(Debug, clap::Args)]
pub struct RecordArgs {
    /// Write the profile to FILE.
    #[arg(short, long, value_name = "FILE", default_value = "microscope.usprof")]
    output: PathBuf,

    /// How much to record, and what it may cost the program.
    #[arg(long, value_enum, default_value = "dev")]
    preset: PresetArg,

    /// Samples a second of each thread's CPU time, over the preset's.
    #[arg(long, value_name = "HZ")]
    rate: Option<u32>,

    /// Bytes of stack each sample copies, over the preset's.
    #[arg(long, value_name = "BYTES")]
    stack_bytes: Option<u32>,

    /// Record each interval a thread spends off its CPU, blocked or
    /// preempted, over the preset's choice: on except in production.
    #[arg(long, value_enum)]
    off_cpu: Option<OnOff>,

    /// Hold microscope's own CPU time to PERCENT of one CPU by sampling less of
    /// each second, over the preset's: 1 in production, and otherwise
    /// none. 0 lifts it.
    #[arg(long, value_name = "PERCENT")]
    budget: Option<f64>,

    /// Sample each call of the function SYMBOL names, instead of CPU time.
    #[arg(long, value_name = "SYMBOL", conflicts_with = "rate")]
    function: Option<String>,

    /// Record the running process PID, without stopping it, until it exits
    /// or Ctrl-C.
    #[arg(long, value_name = "PID", conflicts_with = "command")]
    pid: Option<u32>,

    /// Stop recording after SECONDS; a started program is then killed.
    #[arg(long, value_name = "SECONDS")]
    duration: Option<f64>,

    /// Stop the program every SECONDS to read where its tasks, such as
    /// goroutines and tokio tasks, wait. Each stop pauses the program, and
    /// the report says for how long.
    #[arg(long, value_name = "SECONDS")]
    task_snapshots: Option<f64>,

    /// Read task snapshots from the program's memory as it runs, without
    /// stopping it. A task that changes while read is left out, and the
    /// report counts those.
    #[arg(long, requires = "task_snapshots")]
    live_snapshots: bool,

    /// Read where a Python or Node program's threads are, by ENGINE: `bpf`
    /// walks each sampled thread's Python or JavaScript frames as the
    /// sample is taken, which needs `CAP_BPF` and `CAP_PERFMON`;
    /// `snapshot[=SECONDS]` stops the program every SECONDS (0.1 if not
    /// given) and reads each thread's stack, interpreted and native;
    /// `live[=SECONDS]` reads each thread's Python frames every SECONDS
    /// (0.01 if not given) as the program runs, leaving out each that
    /// changes while read.
    #[arg(long, visible_alias = "node", value_name = "ENGINE")]
    python: Option<String>,

    /// Keep only the last SECONDS of the recording. SIGUSR2 saves the
    /// window as it is to FILE-1.usprof, FILE-2.usprof, and so on, as the
    /// recording goes on; FILE gets the last window.
    #[arg(long, value_name = "SECONDS")]
    window: Option<f64>,

    /// Record without end into DIR, as a segment file every --segment,
    /// each a complete profile of its span, deleting segments older than
    /// --keep. SIGUSR2 ends a segment early. `microscope report DIR` and
    /// `microscope web DIR` read them as one profile.
    #[arg(long, value_name = "DIR", conflicts_with_all = ["window", "output"])]
    continuous: Option<PathBuf>,

    /// How long each segment of --continuous lasts, in seconds.
    #[arg(
        long,
        value_name = "SECONDS",
        default_value = "60",
        requires = "continuous"
    )]
    segment: f64,

    /// How long --continuous keeps segments: seconds, or a number of
    /// minutes, hours, or days, such as 30m, 24h, or 7d.
    #[arg(
        long,
        value_name = "DURATION",
        default_value = "24h",
        requires = "continuous"
    )]
    keep: String,

    /// The instrumentation sites to switch on in a program built with
    /// microscope's probes SDK: all, off, or names separated by commas, where a
    /// name that ends in `*` is a prefix.
    #[arg(long, value_name = "SITES", default_value = "all")]
    probes: String,

    /// How long the program goes on writing its instrumentation once microscope
    /// stops reading it, as when microscope is killed, before it switches it
    /// off.
    #[arg(long, value_name = "SECONDS", default_value_t = 10.0)]
    probe_timeout: f64,

    /// When frames get their source lines.
    #[arg(long, value_enum, default_value = "now")]
    symbols: SymbolsArg,

    /// Trace these system calls too, with their stacks, as `microscope syscalls
    /// -e trace=SET` names them: `all`, or names and classes such as
    /// `%file,%net,connect`. Needs the BPF engine's privileges.
    #[arg(long, value_name = "SET", conflicts_with = "pid")]
    syscalls: Option<String>,

    /// Also queue the profile for `microscope agent` to upload to microscope's
    /// service, and wait up to 30 seconds for the link to it.
    #[arg(long, conflicts_with = "continuous")]
    upload: bool,

    /// The program to record and its arguments.
    #[arg(last = true, required_unless_present = "pid", value_name = "PROGRAM")]
    command: Vec<OsString>,
}

#[derive(Debug, clap::Args)]
pub struct ReportArgs {
    /// The profile to report, or a directory of --continuous segments.
    file: PathBuf,

    /// Show the call tree instead of the most expensive functions.
    #[arg(long)]
    tree: bool,

    /// Show this many functions.
    #[arg(long, default_value_t = 20)]
    limit: usize,

    #[command(flatten)]
    look: LookArgs,

    /// Show where snapshots found tasks and threads, each weighted by the
    /// time it stands for, rather than the CPU samples.
    #[arg(long)]
    waits: bool,

    /// Tell frames apart by their lines, not only their functions.
    #[arg(long)]
    lines: bool,

    /// Hide call-tree nodes below this percentage of the total.
    #[arg(long, value_name = "PERCENT", default_value_t = 1.0)]
    min_percent: f64,
}

#[derive(Debug, clap::Args)]
pub struct SymbolizeArgs {
    /// The profile to give source lines.
    file: PathBuf,

    /// Write the profile to FILE rather than over the one read.
    #[arg(short, long, value_name = "FILE")]
    output: Option<PathBuf>,

    /// A directory of separate debug files, searched by build id before
    /// the system's.
    #[arg(long = "debug-dir", value_name = "DIR")]
    debug_dirs: Vec<PathBuf>,

    /// Ask the debuginfod servers `DEBUGINFOD_URLS` lists for the debug
    /// files no directory holds, sending them the modules' build ids.
    #[arg(long)]
    debuginfod: bool,
}

#[derive(Debug, clap::Args)]
pub struct ConvertArgs {
    /// The profile to read: microscope's own, pprof's, gzipped or not, or
    /// folded stacks.
    input: PathBuf,

    /// Where to write it, in the format its name says: `.usprof`,
    /// `.pb.gz` or `.pprof`, `.folded`, or `.json` for a Chrome trace.
    output: PathBuf,

    /// The format to write, whatever the output's name: usprof, pprof,
    /// folded, or chrome.
    #[arg(long, value_name = "FORMAT")]
    to: Option<Format>,

    #[command(flatten)]
    look: LookArgs,
}

/// How a report or a conversion sees each stack's frames.
#[derive(Debug, clap::Args)]
pub struct LookArgs {
    /// The frames threads ran, not each async task's own stack.
    #[arg(long, conflicts_with = "python")]
    native: bool,

    /// Each stack as its Python or JavaScript code sees it: from its
    /// outermost Python or JavaScript frame in, without the interpreter's
    /// own frames.
    #[arg(long, visible_alias = "js")]
    python: bool,
}

impl LookArgs {
    const fn view(&self) -> StackView {
        if self.native {
            StackView::Native
        } else if self.python {
            StackView::Python
        } else {
            StackView::Logical
        }
    }
}

/// How often each Python engine that reads the program reads it, unless
/// told: a stopped snapshot holds the program for tens of milliseconds
/// as its debugger attaches, and a live one costs it nothing.
const SNAPSHOT_EVERY: Duration = Duration::from_millis(100);
const LIVE_EVERY: Duration = Duration::from_millis(10);

/// The engine `--python ENGINE` names.
fn python_engine(engine: &str) -> Result<PythonEngine> {
    let (name, seconds) = engine
        .split_once('=')
        .map_or((engine, None), |(name, seconds)| (name, Some(seconds)));
    let every = |unless: Duration| -> Result<Duration> {
        let Some(seconds) = seconds else {
            return Ok(unless);
        };

        let seconds: f64 = seconds
            .parse()
            .with_context(|| format!("--python {engine}: {seconds} is not a number of seconds"))?;
        let every = Duration::try_from_secs_f64(seconds).context("--python")?;
        anyhow::ensure!(
            !every.is_zero(),
            "--python {engine}: reads take some time apart"
        );
        Ok(every)
    };

    match name {
        "bpf" if seconds.is_none() => Ok(PythonEngine::Bpf),
        "snapshot" => Ok(PythonEngine::Snapshot {
            every: every(SNAPSHOT_EVERY)?,
        }),
        "live" => Ok(PythonEngine::Live {
            every: every(LIVE_EVERY)?,
        }),
        _ => anyhow::bail!(
            "--python {engine}: the engines are bpf, snapshot[=SECONDS], and live[=SECONDS]"
        ),
    }
}

/// Runs `microscope convert`.
pub fn run_convert(args: &ConvertArgs) -> std::process::ExitCode {
    match convert(args) {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("error: {error:#}");
            std::process::ExitCode::FAILURE
        }
    }
}

fn convert(args: &ConvertArgs) -> Result<()> {
    let format = args
        .to
        .or_else(|| Format::of_path(&args.output))
        .with_context(|| {
            format!(
                "{} does not say which format to write; choose one with --to",
                args.output.display()
            )
        })?;
    let read =
        Profile::read(&args.input).with_context(|| format!("reading {}", args.input.display()))?;
    if read.truncated {
        eprintln!(
            "warning: {} was cut short; converting the part that was complete",
            args.input.display()
        );
    }

    let view = args.look.view();

    let bytes = read
        .profile
        .export(format, view)
        .with_context(|| format!("converting {}", args.input.display()))?;
    std::fs::write(&args.output, bytes)
        .with_context(|| format!("writing {}", args.output.display()))?;
    let kept = match format {
        Format::Usprof => return Ok(()),
        Format::Pprof => "stacks and their weights, without times, threads, or instrumentation",
        Format::Folded => "stacks and their weights only",
        Format::Chrome => "instrumentation and threads, without samples",
    };

    eprintln!("note: {} holds {kept}", args.output.display());
    Ok(())
}

/// Runs `microscope symbolize`.
pub fn run_symbolize(args: &SymbolizeArgs) -> std::process::ExitCode {
    match symbolize(args) {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("error: {error:#}");
            std::process::ExitCode::FAILURE
        }
    }
}

fn symbolize(args: &SymbolizeArgs) -> Result<()> {
    let output = args.output.as_ref().unwrap_or(&args.file);
    let read =
        Profile::read(&args.file).with_context(|| format!("reading {}", args.file.display()))?;
    anyhow::ensure!(
        !read.truncated,
        "{} was cut short; symbolizing would keep only its complete part",
        args.file.display()
    );
    let mut profile = read.profile;
    let found = profile.symbolize(&microscope::DebugFileOptions {
        directories: args.debug_dirs.clone(),
        debuginfod: args.debuginfod,
        ..microscope::DebugFileOptions::default()
    });
    if found.is_empty() {
        eprintln!("note: every frame of {} has its lines", args.file.display());
    }

    for module in &found {
        match &module.source {
            Ok(source) => eprintln!(
                "{}: {} from {}",
                module.module,
                plural(module.frames, "frame"),
                source.display()
            ),
            Err(why) => eprintln!("{}: keeps its addresses: {why}", module.module),
        }
    }

    profile
        .write(output)
        .with_context(|| format!("writing {}", output.display()))
}

/// Gives a profile recorded without symbols the lines this host's own
/// files and debug directories hold, writing a note for each module they
/// do not.
pub fn symbolize_here(profile: &mut Profile, out: &mut impl std::io::Write) -> Result<()> {
    for module in profile.symbolize(&microscope::DebugFileOptions::default()) {
        if let Err(why) = &module.source {
            writeln!(out, "note: {} keeps its addresses: {why}", module.module)?;
        }
    }

    Ok(())
}

/// Runs `microscope record`, exiting as the program did.
pub fn run_record(args: &RecordArgs) -> std::process::ExitCode {
    match record(args) {
        Ok(code) => std::process::ExitCode::from(u8::try_from(code).unwrap_or(1)),
        Err(error) => {
            eprintln!("error: {error:#}");
            std::process::ExitCode::FAILURE
        }
    }
}

/// Runs `microscope report` to standard output.
pub fn run_report(args: &ReportArgs) -> std::process::ExitCode {
    match report(args, &mut std::io::stdout().lock()) {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) if super::is_broken_pipe(&error) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("error: {error:#}");
            std::process::ExitCode::FAILURE
        }
    }
}

/// The recording options `args` choose.
fn options(args: &RecordArgs) -> Result<RecordOptions> {
    let mut options = RecordOptions::preset(match args.preset {
        PresetArg::Prod => Preset::Production,
        PresetArg::Dev => Preset::Development,
        PresetArg::Max => Preset::Maximum,
    });

    if let Some(rate) = args.rate {
        options.source = SampleSource::Clock { rate };
    }

    if let Some(symbol) = &args.function {
        options.source = SampleSource::Function {
            symbol: symbol.clone(),
        };
    }

    if let Some(bytes) = args.stack_bytes {
        options.stack = StackCopy::Bytes(bytes);
    }

    if let Some(off_cpu) = args.off_cpu {
        options.off_cpu = matches!(off_cpu, OnOff::On);
    }

    if let Some(percent) = args.budget {
        anyhow::ensure!(
            percent.is_finite() && percent >= 0.0,
            "--budget is a percentage of one CPU"
        );
        options.budget = (percent > 0.0).then_some(Budget {
            cpu: percent / 100.0,
            ..Budget::PRODUCTION
        });
    }

    if let Some(seconds) = args.task_snapshots {
        let every = Duration::try_from_secs_f64(seconds).context("--task-snapshots")?;
        anyhow::ensure!(!every.is_zero(), "--task-snapshots is a number of seconds");
        options.task_snapshots = Some(every);
        options.live_snapshots = args.live_snapshots;
    }

    if let Some(engine) = &args.python {
        options.python = Some(python_engine(engine)?);
    }

    if let Some(seconds) = args.window {
        options.window = Some(Duration::try_from_secs_f64(seconds).context("--window")?);
    }

    if let Some(set) = &args.syscalls {
        let filter = microscope::syscalls::Filter::parse(
            &[format!("trace={set}")],
            &crate::present::syscalls::signal_named,
        )
        .map_err(|error| anyhow::anyhow!("--syscalls: {error}"))?;
        options.syscalls = Some(filter.calls);
    }

    if args.continuous.is_some() {
        let every = Duration::try_from_secs_f64(args.segment).context("--segment")?;
        anyhow::ensure!(!every.is_zero(), "--segment is a number of seconds");
        options.segment = Some(every);
    }

    options.probes = match args.probes.as_str() {
        "all" => ProbeSelection::All,
        "off" => ProbeSelection::Off,
        names => ProbeSelection::Named(names.split(',').map(str::to_owned).collect()),
    };

    options.probe_timeout =
        Duration::try_from_secs_f64(args.probe_timeout).context("--probe-timeout")?;
    options.symbols_later = matches!(args.symbols, SymbolsArg::Later);
    Ok(options)
}

fn record(args: &RecordArgs) -> Result<i32> {
    let options = options(args)?;
    let stop = Arc::new(AtomicBool::new(false));
    let trigger = Arc::new(AtomicBool::new(false));

    // Ctrl-C reaches a started program too, which ends as it chooses; an
    // attached one goes on, and the recording stops.
    handle_signals(
        args.pid.is_some().then(|| Arc::clone(&stop)),
        Arc::clone(&trigger),
    )?;

    if let Some(seconds) = args.duration {
        let duration = Duration::try_from_secs_f64(seconds).context("--duration")?;
        let stop = Arc::clone(&stop);
        std::thread::spawn(move || {
            std::thread::sleep(duration);
            stop.store(true, Ordering::Relaxed);
        });
    }

    let continuous = match &args.continuous {
        Some(directory) => {
            std::fs::create_dir_all(directory)
                .with_context(|| format!("creating {}", directory.display()))?;
            let keep = duration(&args.keep).context("--keep")?;
            eprintln!(
                "microscope: writing a segment every {} s to {}, keeping {}",
                args.segment,
                directory.display(),
                args.keep
            );
            Some((directory.as_path(), keep))
        }
        None => None,
    };

    let mut saves = 0;

    let mut save = |profile: Profile| {
        if let Some((directory, keep)) = continuous {
            if let Err(error) = profile.write_segment(directory, keep) {
                eprintln!(
                    "microscope: writing a segment to {} failed: {error}",
                    directory.display()
                );
            }

            return;
        }

        saves += 1;
        let path = numbered(&args.output, saves);
        match profile.write(&path) {
            Ok(()) => eprintln!(
                "microscope: saved {} samples of the window to {}",
                profile.samples(),
                path.display()
            ),
            Err(error) => eprintln!("microscope: writing {} failed: {error}", path.display()),
        }
    };

    if let Some(window) = options.window {
        eprintln!(
            "microscope: keeping the last {:.3} s; `kill -USR2 {}` saves it",
            window.as_secs_f64(),
            std::process::id()
        );
    }

    let mut watch = Watch {
        every: Duration::MAX,
        publish: &mut |_| {},
        stop: &stop,
        save: (options.window.is_some() || continuous.is_some()).then_some(Save {
            trigger: &trigger,
            to: &mut save,
        }),
    };

    let (profile, code) = if let Some(pid) = args.pid {
        let attachment = microscope::profiler::record_attached(pid, &options, &mut watch)?;
        (attachment.profile, 0)
    } else {
        let mut command = Command::new(&args.command[0]);
        command.args(&args.command[1..]);
        let recording = microscope::profiler::record_watched(command, &options, &mut watch)?;
        (recording.profile, recording.status.code().unwrap_or(1))
    };

    if let Some((directory, keep)) = continuous {
        let path = profile
            .write_segment(directory, keep)
            .with_context(|| format!("writing a segment to {}", directory.display()))?;
        eprintln!(
            "microscope: recorded the last segment to {}",
            path.display()
        );
    } else {
        profile
            .write(&args.output)
            .with_context(|| format!("writing {}", args.output.display()))?;
        eprintln!(
            "microscope: recorded {} samples to {}",
            profile.samples(),
            args.output.display()
        );
    }

    report_losses(&profile);
    if args.upload {
        upload(&profile, &args.output)?;
    }

    Ok(code)
}

fn report_losses(profile: &Profile) {
    let lost = profile.lost_samples();
    if lost > 0 {
        eprintln!("microscope: the kernel dropped {lost} samples");
    }
}

/// Hands the profile written to `path` to the agent, and prints its link
/// once the agent has uploaded it.
fn upload(profile: &Profile, path: &std::path::Path) -> Result<()> {
    let name = profile
        .setting("program")
        .and_then(|program| std::path::Path::new(program).file_name())
        .or_else(|| path.file_stem())
        .map_or_else(
            || "recording".to_owned(),
            |name| name.to_string_lossy().into_owned(),
        );

    eprintln!(
        "microscope: uploading {}; a recording holds the program's paths, command lines, and \
         function names, and is accessible to people with project read access",
        path.display()
    );
    match crate::agent::upload(path, &name)? {
        Some(url) => {
            std::io::Write::write_all(&mut std::io::stdout(), format!("{url}\n").as_bytes())
                .context("writing the link")?;
        }
        None => eprintln!(
            "microscope: the recording waits in the outbox until `microscope agent` connects"
        ),
    }
    Ok(())
}

/// A duration in seconds, or with a unit: `90`, `30m`, `24h`, or `7d`.
pub(super) fn duration(text: &str) -> Result<Duration> {
    let (number, unit) = match text.char_indices().last() {
        Some((at, unit @ ('s' | 'm' | 'h' | 'd'))) => (&text[..at], unit),
        _ => (text, 's'),
    };

    let scale = match unit {
        'm' => 60.0,
        'h' => 3600.0,
        'd' => 86400.0,
        _ => 1.0,
    };

    let number: f64 = number
        .parse()
        .with_context(|| format!("{text} is not a duration, such as 90, 30m, 24h, or 7d"))?;

    Ok(Duration::try_from_secs_f64(number * scale)?)
}

/// `path` with `-number` before its extension.
fn numbered(path: &std::path::Path, number: u32) -> PathBuf {
    let stem = path.file_stem().unwrap_or_default().to_string_lossy();
    let name = path.extension().map_or_else(
        || format!("{stem}-{number}"),
        |extension| format!("{stem}-{number}.{}", extension.to_string_lossy()),
    );
    path.with_file_name(name)
}

/// Keeps Ctrl-C from ending microscope, setting `stop` if there is one, and
/// sets `trigger` at each SIGUSR2.
pub(super) fn handle_signals(
    stop: Option<Arc<AtomicBool>>,
    trigger: Arc<AtomicBool>,
) -> Result<()> {
    use tokio::signal::unix::{SignalKind, signal};
    let (installed, ready) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let handled = tokio::runtime::Builder::new_current_thread()
            .enable_io()
            .build()
            .and_then(|runtime| {
                let signals = runtime.block_on(async {
                    Ok::<_, std::io::Error>((
                        signal(SignalKind::interrupt())?,
                        signal(SignalKind::user_defined2())?,
                    ))
                })?;
                Ok((runtime, signals))
            });

        let (runtime, (mut interrupts, mut triggers)) = match handled {
            Ok(handled) => handled,
            Err(error) => {
                let _ = installed.send(Err(error));
                return;
            }
        };

        let _ = installed.send(Ok(()));

        runtime.block_on(async {
            loop {
                tokio::select! {
                    Some(()) = interrupts.recv() => {
                        if let Some(stop) = &stop {
                            stop.store(true, Ordering::Relaxed);
                        }
                    }
                    Some(()) = triggers.recv() => trigger.store(true, Ordering::Relaxed),
                    else => break,
                }
            }
        });
    });

    // The handler is installed before the recording begins.
    ready
        .recv()
        .context("starting a signal handler")?
        .context("handling signals")
}

#[expect(
    clippy::cast_precision_loss,
    reason = "seconds and percentages need no more precision than f64's"
)]
fn report(args: &ReportArgs, out: &mut impl std::io::Write) -> Result<()> {
    let read =
        Profile::read(&args.file).with_context(|| format!("reading {}", args.file.display()))?;
    let mut profile = read.profile;
    let view = args.look.view();

    if read.truncated {
        writeln!(
            out,
            "The file was cut short; this is the part that was complete."
        )?;
    }

    symbolize_here(&mut profile, out)?;

    // A trace alone, which no recording sampled.
    if profile.has_syscalls() && profile.setting("weight").is_none() {
        return syscall_summary(&profile, out);
    }

    summary(&profile, out)?;

    if profile.has_syscalls() {
        writeln!(out)?;
        syscall_summary(&profile, out)?;
    }

    let (source, total) = if args.waits {
        let waited = profile.task_waits().iter().map(|wait| wait.weight).sum();
        (Source::Waits, waited)
    } else {
        (Source::Samples, profile.total_weight())
    };

    let granularity = if args.lines {
        Granularity::Line
    } else {
        Granularity::Function
    };

    let percent = |value: u64| {
        if total == 0 {
            0.0
        } else {
            value as f64 * 100.0 / total as f64
        }
    };

    writeln!(out)?;
    if args.tree {
        let tree = profile.call_tree_of(view, source, granularity);
        let mut depth = vec![0_usize; tree.len()];

        // Each node with its children after it, depth first.
        let mut children: Vec<Vec<usize>> = vec![Vec::new(); tree.len()];
        let mut roots = Vec::new();
        for (index, node) in tree.iter().enumerate() {
            match node.parent {
                Some(parent) => children[parent].push(index),
                None => roots.push(index),
            }
        }

        let by_total = |nodes: &mut Vec<usize>| {
            nodes.sort_by(|a, b| tree[*b].total.cmp(&tree[*a].total).then(a.cmp(b)));
        };

        for list in &mut children {
            by_total(list);
        }

        by_total(&mut roots);

        let mut pending: Vec<usize> = roots.into_iter().rev().collect();

        while let Some(index) = pending.pop() {
            let node = &tree[index];
            if percent(node.total) < args.min_percent {
                continue;
            }

            writeln!(
                out,
                "{:6.2}% {:6.2}%  {}{}",
                percent(node.total),
                percent(node.own),
                "  ".repeat(depth[index]),
                node.name
            )?;
            for &child in children[index].iter().rev() {
                depth[child] = depth[index] + 1;
                pending.push(child);
            }
        }
    } else {
        writeln!(out, "{:>7} {:>7}  function", "own", "total")?;

        for cost in profile
            .top_of(view, source, granularity)
            .iter()
            .take(args.limit)
        {
            writeln!(
                out,
                "{:6.2}% {:6.2}%  {}",
                percent(cost.own),
                percent(cost.total),
                cost.name
            )?;
        }
    }

    let truncations = profile.truncations();
    if !truncations.is_empty() {
        writeln!(out)?;
        writeln!(out, "Stacks that end short of their outermost frame:")?;
        for (reason, count) in truncations {
            writeln!(out, "  {count:>8}  {reason}")?;
        }
    }

    Ok(())
}

/// What recording cost microscope, and how the budget held it.
#[expect(
    clippy::cast_precision_loss,
    reason = "seconds and percentages need no more precision than f64's"
)]
fn cost(profile: &Profile, out: &mut impl std::io::Write) -> Result<()> {
    let seconds = |nanoseconds: u64| nanoseconds as f64 / 1e9;
    let (start, end) = profile.span();
    let span = u64::try_from(end - start).unwrap_or(0).max(1);
    let (used, loading) = profile.recorder_cpu();

    if used > 0 {
        writeln!(
            out,
            "Recording took {:.3} s of microscope's CPU time, {:.2}% of one CPU, {:.3} s of it \
             reading debug information.",
            seconds(used),
            used as f64 * 100.0 / span as f64,
            seconds(loading)
        )?;
    }

    let gaps = profile.paused(GapReason::Budget);
    if !gaps.is_empty() {
        let paused: u64 = gaps
            .iter()
            .map(|(start, end)| u64::try_from(end - start).unwrap_or(0))
            .sum();
        writeln!(
            out,
            "Sampling paused for {:.3} s of the {:.3} s recorded, to hold microscope to {}; \
             the share sampled changed {}.",
            seconds(paused),
            seconds(span),
            profile.setting("budget").unwrap_or("its budget"),
            match profile.budget_changes().len() {
                1 => "once".to_owned(),
                changes => format!("{changes} times"),
            }
        )?;
    } else if profile.setting("budget") == Some("none") && used * 20 > span {
        writeln!(
            out,
            "That is more than --preset prod allows, which holds microscope to 1% of a CPU."
        )?;
    }

    let (snapshots, held, _) = profile.task_snapshots();
    let (live, changed) = profile.live_snapshots();

    if live > 0 {
        writeln!(
            out,
            "{} read where its tasks wait and its threads are as it ran, leaving out {} that \
             changed while read.",
            plural(live, "snapshot"),
            plural(changed, "task or thread"),
        )?;
    }

    let stopped = snapshots - live;
    if stopped > 0 {
        writeln!(
            out,
            "{} held the program stopped for {:.3} s, {:.1} ms each, to read where its tasks \
             wait or its threads are.",
            plural(stopped, "snapshot"),
            seconds(held),
            held as f64 / 1e6 / stopped as f64,
        )?;
    }

    if snapshots > 0 {
        writeln!(
            out,
            "{} of the {} samples ran a task the snapshots either side of them agree on.",
            profile.task_samples(),
            profile.samples()
        )?;
    }

    let problems = profile.task_snapshot_problems();
    if let Some((_, first)) = problems.first() {
        writeln!(
            out,
            "Task snapshots missed tasks {}: {first}.",
            match problems.len() {
                1 => "once".to_owned(),
                times => format!("{times} times; the first time"),
            }
        )?;
    }

    Ok(())
}

/// The system calls a capture holds, as `microscope syscalls -c` shows them.
#[expect(
    clippy::cast_precision_loss,
    reason = "seconds need no more precision than f64's"
)]
fn syscall_summary(profile: &Profile, out: &mut impl std::io::Write) -> Result<()> {
    use crate::present::syscalls::{errno, summary};
    let syscalls = profile.syscalls();
    let calls = syscalls.stats().per_call();
    let (start, end) = profile.span();
    let program = profile.setting("program").unwrap_or("the program");
    let made: u64 = calls.values().map(|stats| stats.calls).sum();
    let failed: u64 = calls.values().map(|stats| stats.errors).sum();

    writeln!(
        out,
        "{program}: {made} system calls, {failed} failed, over {:.3} s",
        end.saturating_sub(start) as f64 / 1e9
    )?;

    if syscalls.lost > 0 {
        writeln!(
            out,
            "The kernel had no room for {} records: every count is a lower bound.",
            syscalls.lost
        )?;
    }

    writeln!(out)?;
    let errno = |number: u16| errno(microscope::syscalls::Errno(number)).0;
    let sort = summary::sort_key("time").map_err(anyhow::Error::msg)?;

    for line in summary::table(
        &calls,
        &summary::Column::DEFAULT,
        sort,
        summary::Unit::Microseconds,
        &errno,
    ) {
        writeln!(out, "{line}")?;
    }

    Ok(())
}

/// What a report's profile holds, and how much of the program it covers.
#[expect(
    clippy::cast_precision_loss,
    reason = "seconds and percentages need no more precision than f64's"
)]
fn summary(profile: &Profile, out: &mut impl std::io::Write) -> Result<()> {
    let total = profile.total_weight();
    let program = profile.setting("program").unwrap_or("the program");
    let unit = profile.setting("weight").unwrap_or("nanoseconds");
    let weight = |value: u64| match unit {
        "nanoseconds" => format!("{:.3} s", value as f64 / 1e9),
        unit => format!("{value} {unit}"),
    };

    writeln!(
        out,
        "{program}: {} samples, {}",
        profile.samples(),
        weight(total)
    )?;

    if !profile.timed() {
        let source = profile.setting("source").unwrap_or("its file");
        writeln!(out, "Read from {source}, which keeps no times or threads.")?;
    }

    let lost = profile.lost_samples();
    if lost > 0 {
        writeln!(out, "The kernel dropped {lost} samples.")?;
    }

    if let Some(window) = profile.setting("window").filter(|window| *window != "all") {
        writeln!(out, "The recording kept {window} it reached.")?;
    }

    let counted = profile.counted();
    let paused = !profile.gaps().is_empty();
    if unit == "nanoseconds" && counted > total {
        writeln!(
            out,
            "The samples cover {:.0}% of the {} of CPU time the program ran{}.",
            total as f64 * 100.0 / counted as f64,
            weight(counted),
            if paused { " while sampling was on" } else { "" }
        )?;
    }

    cost(profile, out)?;

    if profile.setting("off-CPU") == Some("on") {
        let threads = profile.threads();
        let blocked = threads.iter().map(|thread| thread.blocked).sum::<u64>();
        let preempted = threads.iter().map(|thread| thread.preempted).sum::<u64>();
        let seconds = |value: u64| format!("{:.3} s", value as f64 / 1e9);

        writeln!(
            out,
            "Threads were off their CPUs for {}: {} blocked, {} preempted.",
            seconds(blocked + preempted),
            seconds(blocked),
            seconds(preempted)
        )?;
    }

    instrumentation(profile, out)
}

/// What the program's instrumentation recorded, by site, and what of it
/// was lost.
#[expect(
    clippy::cast_precision_loss,
    reason = "times need no more precision than f64's"
)]
fn instrumentation(profile: &Profile, out: &mut impl std::io::Write) -> Result<()> {
    for (_, problem) in profile.probe_problems() {
        writeln!(out, "{problem}.")?;
    }

    let counts = profile.probe_counts();
    if counts.events == 0 && !counts.lost() {
        return Ok(());
    }

    let zones = profile.zones();

    // Each site's zones: how many, their total and longest time, and how
    // many are flagged.
    let mut sites: std::collections::BTreeMap<&str, (u64, u64, u64, u64)> =
        std::collections::BTreeMap::new();

    for zone in &zones {
        let site = sites.entry(zone.name.as_str()).or_default();
        let length = u64::try_from(zone.end - zone.start).unwrap_or(0);
        site.0 += 1;
        site.1 += length;
        site.2 = site.2.max(length);
        site.3 += u64::from(zone.flags != 0);
    }

    writeln!(
        out,
        "Instrumentation: {}, in {} from {}.",
        plural(counts.events, "event"),
        plural(zones.len() as u64, "zone"),
        plural(sites.len() as u64, "site")
    )?;

    let lost = [
        (
            counts.overwritten,
            "overwritten before microscope read them",
        ),
        (counts.no_ring, "events found no ring to take them"),
        (
            counts.interrupted,
            "events were dropped by a signal handler that interrupted a write",
        ),
        (counts.malformed, "records broke the probes ABI"),
    ];

    for (count, what) in lost.into_iter().filter(|(count, _)| *count > 0) {
        let what = if what.starts_with("overwritten") {
            format!("{} {what}", plural(count, "record"))
        } else {
            format!("{count} {what}")
        };

        writeln!(out, "  {what}; zones they cut short are flagged.")?;
    }

    let mut by_total: Vec<_> = sites.into_iter().collect();
    by_total.sort_by(|a, b| b.1.1.cmp(&a.1.1).then(a.0.cmp(b.0)));
    writeln!(
        out,
        "  {:>8} {:>12} {:>12} {:>12} {:>7}  zone",
        "count", "total ms", "mean us", "max us", "flagged"
    )?;

    for (name, (count, total, longest, flagged)) in by_total.iter().take(20) {
        writeln!(
            out,
            "  {count:>8} {:>12.3} {:>12.3} {:>12.3} {flagged:>7}  {name}",
            *total as f64 / 1e6,
            *total as f64 / 1e3 / *count as f64,
            *longest as f64 / 1e3,
        )?;
    }

    Ok(())
}

/// A debugging session's profile of its program: recording on a thread of
/// its own, or recorded.
#[derive(Default)]
pub struct Profiling {
    recording: Option<SessionRecording>,
    recorded: Option<Profile>,
}

struct SessionRecording {
    stop: Arc<AtomicBool>,
    trigger: Arc<AtomicBool>,
    saved: std::sync::mpsc::Receiver<Profile>,
    thread: std::thread::JoinHandle<Result<Profile, String>>,
}

impl SessionRecording {
    /// The profile so far: the window as it is, or all of it once the
    /// recording has ended.
    fn so_far(&self) -> Option<Profile> {
        self.trigger.store(true, Ordering::Relaxed);
        let deadline = std::time::Instant::now() + Duration::from_secs(10);

        while std::time::Instant::now() < deadline && !self.thread.is_finished() {
            if let Ok(profile) = self.saved.recv_timeout(Duration::from_millis(10)) {
                return Some(profile);
            }
        }

        None
    }
}

impl Profiling {
    /// Takes the recording's profile once it has ended.
    fn settle(&mut self) -> Result<()> {
        if self
            .recording
            .as_ref()
            .is_some_and(|recording| recording.thread.is_finished())
        {
            self.end()?;
        }

        Ok(())
    }

    /// Ends the recording, keeping its profile.
    fn end(&mut self) -> Result<()> {
        if let Some(recording) = self.recording.take() {
            recording.stop.store(true, Ordering::Relaxed);
            let profile = recording
                .thread
                .join()
                .map_err(|_| anyhow::anyhow!("the profiler panicked"))?
                .map_err(|error| anyhow::anyhow!("profiling failed: {error}"))?;
            self.recorded = Some(profile);
        }

        Ok(())
    }
}

impl super::Cli {
    /// `profile start|stop|save`: samples the session's program from
    /// outside, as `microscope record --pid` does, while it runs.
    pub(super) async fn profile(&self, arguments: &[&str]) -> Result<String> {
        let live = if arguments.first() == Some(&"start") {
            self.live_process().await
        } else {
            None
        };

        self.profiling
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .command(arguments, live)
    }
}

impl Profiling {
    /// Runs one `profile` command; `live` is the process to start on.
    fn command(
        &mut self,
        arguments: &[&str],
        live: Option<microscope::ProcessId>,
    ) -> Result<String> {
        self.settle()?;

        match arguments {
            ["start", window @ ..] if window.len() <= 1 => {
                anyhow::ensure!(
                    self.recording.is_none(),
                    "already profiling; `profile stop` ends it"
                );

                let Some(pid) = live else {
                    anyhow::bail!("no program is running to profile");
                };

                let pid = u32::try_from(pid.get()).context("the program's process id")?;

                let mut options = RecordOptions::default();
                if let [seconds] = window {
                    let seconds: f64 = seconds.parse().context("a window in seconds")?;
                    options.window =
                        Some(Duration::try_from_secs_f64(seconds).context("a window in seconds")?);
                }

                self.recording = Some(start_recording(pid, options));
                self.recorded = None;

                Ok(format!(
                    "Profiling process {pid}{}.\n",
                    window
                        .first()
                        .map(|seconds| format!(", keeping the last {seconds} s"))
                        .unwrap_or_default()
                ))
            }
            ["stop"] => {
                anyhow::ensure!(
                    self.recording.is_some() || self.recorded.is_some(),
                    "not profiling; `profile start` begins"
                );

                self.end()?;
                let samples = self.recorded.as_ref().map_or(0, Profile::samples);
                Ok(format!("Profiled {samples} samples.\n"))
            }
            ["save", file] => {
                let profile = self.recording.as_ref().and_then(SessionRecording::so_far);
                self.settle()?;
                let Some(profile) = profile.or_else(|| self.recorded.clone()) else {
                    anyhow::bail!("nothing has been profiled; `profile start` begins");
                };

                profile
                    .write(std::path::Path::new(file))
                    .with_context(|| format!("writing {file}"))?;

                Ok(format!("Saved {} samples to {file}.\n", profile.samples()))
            }
            _ => anyhow::bail!("usage: profile start|stop|save [window-seconds|file]"),
        }
    }
}

/// Records `pid` on a thread of its own until it exits or is stopped.
fn start_recording(pid: u32, options: RecordOptions) -> SessionRecording {
    let stop = Arc::new(AtomicBool::new(false));
    let trigger = Arc::new(AtomicBool::new(false));
    let (send, saved) = std::sync::mpsc::channel();

    let thread = std::thread::spawn({
        let (stop, trigger) = (Arc::clone(&stop), Arc::clone(&trigger));
        move || {
            let mut save = |profile| {
                let _ = send.send(profile);
            };

            microscope::profiler::record_attached(
                pid,
                &options,
                &mut Watch {
                    every: Duration::MAX,
                    publish: &mut |_| {},
                    stop: &stop,
                    save: Some(Save {
                        trigger: &trigger,
                        to: &mut save,
                    }),
                },
            )
            .map(|attachment| attachment.profile)
            .map_err(|error| error.to_string())
        }
    });

    SessionRecording {
        stop,
        trigger,
        saved,
        thread,
    }
}
