//! `microscope syscalls`: strace's flags over microscope's engines
//! (`plans/syscalls.html`).

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::fmt::Write as _;
use std::fs::File;
use std::io::{self, LineWriter, Write};
use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{Context as _, Result, bail};
use microscope::syscall_engines::bpf::{self, AttachMode, Attachment, BpfOptions};
use microscope::syscall_engines::capture::SyscallCapture;
use microscope::syscall_engines::cgroup::{self, Cgroup};
use microscope::syscall_engines::continuous::{Continuous, ContinuousOptions, Saved};
use microscope::syscall_engines::ptrace;
use microscope::syscall_engines::stacks::{Frame, SharedStacks};
use microscope::syscall_engines::{Finished, Sink, Target, TraceOptions, monotonic};
use microscope::syscalls::StatusSet;
use microscope::syscalls::captures::Limits;
use microscope::syscalls::fds::FdTable;
use microscope::syscalls::filter::{self, Filter};
use microscope::syscalls::stats::{Folded, SyscallStats, bytes_moved};
use microscope::syscalls::{
    Abi, CallStack, InFlight, InterpretedCall, Outcome, Pid, ProcessEnd, SyscallEvent, SyscallId,
    Tid, TraceEvent,
};
use serde_json::{Map, Value, json};

use super::Renderers;
use super::terminal::{ColorChoice, Renderer, Role};
use crate::present::syscalls::summary::{self, Column};
use crate::present::syscalls::{
    self as present, Constants, Context, FdStyle, Hex, Kind, Precision, Style, Tokens,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum EngineArg {
    /// eBPF in the kernel: no stops, needs `CAP_BPF` and `CAP_PERFMON`.
    Bpf,

    /// ptrace with a seccomp filter: exact, unprivileged, slower.
    Ptrace,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum OutputFormat {
    Text,
    Ndjson,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum ConstantsArg {
    Raw,
    Abbrev,
    Verbose,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum AttachArg {
    Auto,
    Tracepoint,
    Fentry,
}

/// The flags follow strace's; see `microscope syscalls --help`.
#[derive(Debug, clap::Args)]
#[command(about = "Trace the system calls a program makes, as strace does")]
#[expect(clippy::struct_excessive_bools, reason = "one per flag")]
pub struct SyscallsArgs {
    /// A selection: trace=, status=, signal=, read=, write=, trace-fds=,
    /// raw=, verbose=, or abbrev=. A bare set means trace=.
    #[arg(short = 'e', value_name = "EXPR")]
    expressions: Vec<String>,

    /// Trace the running process PID; repeat it, or separate pids with
    /// commas.
    #[arg(short = 'p', long = "attach-pid", value_name = "PID")]
    pids: Vec<String>,

    /// Trace children too (the default); twice, with -o, writes each
    /// process to FILE.PID.
    #[arg(short = 'f', action = clap::ArgAction::Count)]
    follow: u8,

    /// Trace only the targets, not the processes they create.
    #[arg(long, conflicts_with = "follow")]
    no_follow: bool,

    /// Stop tracing a process when it calls SYSCALL; only execve.
    #[arg(short = 'b', value_name = "SYSCALL")]
    detach_on: Option<String>,

    /// Count calls, errors, and time, and print a table at the end
    /// instead of the calls.
    #[arg(short = 'c', conflicts_with = "summary_too")]
    summary_only: bool,

    /// Print the calls and the table.
    #[arg(short = 'C')]
    summary_too: bool,

    /// Time calls by wall clock, from entry to exit. microscope always does.
    #[arg(short = 'w')]
    wall_clock: bool,

    /// Sort the table by a column: time, calls, errors, name, p99, or
    /// nothing.
    #[arg(short = 'S', value_name = "SORTBY", default_value = "time")]
    sort: String,

    /// The table's columns, separated by commas: time-percent, total-time,
    /// min-time, max-time, avg-time, calls, errors, name, p50, p90, p99,
    /// bytes, errnos.
    #[arg(short = 'U', value_name = "COLUMNS")]
    columns: Option<String>,

    /// Print what descriptors refer to; twice adds sockets' endpoints.
    #[arg(short = 'y', action = clap::ArgAction::Count)]
    decode_fds: u8,

    /// Bytes of each string shown.
    #[arg(short = 's', value_name = "SIZE", default_value_t = 32)]
    string_limit: usize,

    /// Print non-ASCII strings in hex; twice, every string.
    #[arg(short = 'x', action = clap::ArgAction::Count)]
    hex: u8,

    /// How constants print.
    #[arg(short = 'X', value_enum, value_name = "FORMAT")]
    constants: Option<ConstantsArg>,

    /// Print whole structures and environments.
    #[arg(short = 'v')]
    verbose: bool,

    /// Align results at COLUMN, as strace does.
    #[arg(short = 'a', value_name = "COLUMN", default_value_t = 40)]
    column: usize,

    /// Print each call's number.
    #[arg(short = 'n')]
    numbers: bool,

    /// Name each argument as the kernel does.
    #[arg(short = 'N')]
    arg_names: bool,

    /// Print each process's command name beside its pid.
    #[arg(short = 'Y')]
    comm: bool,

    /// Print the time of day; twice, with microseconds; three times, as
    /// seconds since the epoch.
    #[arg(short = 't', action = clap::ArgAction::Count)]
    timestamps: u8,

    /// Print the time since the previous line, in microseconds.
    #[arg(short = 'r', overrides_with = "relative_precision")]
    relative: bool,

    /// Print the time since the previous line in s, ms, us, or ns; us
    /// when no PRECISION is given.
    #[arg(
        long = "relative-timestamps",
        value_name = "PRECISION",
        num_args = 0..=1,
        require_equals = true,
        default_missing_value = "us",
        value_parser = precision,
        overrides_with_all = ["relative", "relative_precision"]
    )]
    relative_precision: Option<Precision>,

    /// Print the time each call took, in microseconds.
    #[arg(short = 'T', overrides_with = "durations_precision")]
    durations: bool,

    /// Print the time each call took in s, ms, us, or ns; us when no
    /// PRECISION is given. With -c or -C, ns gives the table's times in
    /// nanoseconds; -c takes only us or ns.
    #[arg(
        long = "syscall-times",
        value_name = "PRECISION",
        num_args = 0..=1,
        require_equals = true,
        default_missing_value = "us",
        value_parser = precision,
        overrides_with_all = ["durations", "durations_precision"]
    )]
    durations_precision: Option<Precision>,

    /// Print the stack of each call; -kk adds source lines. -k=dwarf has
    /// the BPF engine copy the stack and unwind it by call-frame
    /// information, rather than walk its frame pointers.
    #[arg(
        short = 'k',
        value_name = "UNWIND",
        num_args = 0..=1,
        require_equals = true,
        default_missing_value = "fp",
        action = clap::ArgAction::Append
    )]
    stacks: Vec<String>,

    /// The most frames of a stack -k prints.
    #[arg(long, value_name = "LIMIT", default_value_t = 256)]
    stack_trace_frame_limit: usize,

    /// Write the trace to FILE; `-` is standard output. A FILE ending in
    /// `.usprof` gets a capture, which `microscope report` and the web UI read.
    #[arg(short = 'o', value_name = "FILE")]
    output: Option<PathBuf>,

    /// Append to -o's file.
    #[arg(short = 'A', requires = "output")]
    append: bool,

    /// When to color the trace.
    #[arg(long, value_enum)]
    color: Option<ColorChoice>,

    /// Suppress messages about attaching; twice, about exits too.
    #[arg(short = 'q', action = clap::ArgAction::Count)]
    quiet: u8,

    /// Show only calls that succeeded.
    #[arg(short = 'z', conflicts_with = "failed_only")]
    successful_only: bool,

    /// Show only calls that failed.
    #[arg(short = 'Z')]
    failed_only: bool,

    /// Show only calls that name PATH, by argument or descriptor.
    #[arg(short = 'P', value_name = "PATH")]
    paths: Vec<String>,

    /// Show only calls on these descriptors, separated by commas.
    #[arg(long, value_name = "FDS")]
    trace_fds: Option<String>,

    /// Show only calls that took at least this long: 10ms, 2us, 1s.
    #[arg(long, value_name = "DURATION")]
    slower_than: Option<String>,

    /// Show one call in N, as 1/N; the table still counts every call.
    #[arg(long, value_name = "1/N")]
    sample: Option<String>,

    /// How the trace is written.
    #[arg(long, value_enum, default_value = "text")]
    format: OutputFormat,

    /// Report calls blocked longer than this, while they are: 1s, 500ms,
    /// or off.
    #[arg(long, value_name = "DURATION", default_value = "1s")]
    in_flight: String,

    /// Which engine sees the calls. There is never a fallback.
    #[arg(long, value_enum, default_value = "bpf")]
    engine: EngineArg,

    /// Allow the ptrace engine to attach, which stops the process at every
    /// system call it makes.
    #[arg(long)]
    stop_every_call: bool,

    /// Where the BPF engine attaches.
    #[arg(long, value_enum, default_value = "auto")]
    attach: AttachArg,

    /// The BPF engine's ring buffer: bytes, with K or M.
    #[arg(long, value_name = "SIZE")]
    ring: Option<String>,

    /// Hold the trace's cost to PERCENT of one CPU: over it, drop stacks,
    /// cut strings, sample calls, then only count them, a step a second.
    #[arg(long, value_name = "PERCENT")]
    budget: Option<f64>,

    /// Trace every process of a cgroup.
    #[arg(long, value_name = "PATH", conflicts_with = "container")]
    cgroup: Option<PathBuf>,

    /// Trace every process of a container, by its id.
    #[arg(long, value_name = "ID")]
    container: Option<String>,

    /// Stop tracing after this long: 30s, 5m.
    #[arg(long, value_name = "DURATION")]
    duration: Option<String>,

    /// Trace without end into DIR: a file every --segment holding that
    /// span's statistics, and the last --window of calls in DIR/windows on
    /// SIGUSR2 or a trigger. `microscope report DIR` reads the segments as one.
    #[arg(long, value_name = "DIR", conflicts_with = "output")]
    continuous: Option<PathBuf>,

    /// How long each segment of --continuous spans: 60s, 5m.
    #[arg(
        long,
        value_name = "DURATION",
        default_value = "60s",
        requires = "continuous"
    )]
    segment: String,

    /// How long --continuous keeps its files: 30m, 24h, 7d.
    #[arg(
        long,
        value_name = "DURATION",
        default_value = "24h",
        requires = "continuous"
    )]
    keep: String,

    /// How much of the latest calls --continuous holds for a trigger.
    #[arg(
        long,
        value_name = "DURATION",
        default_value = "10s",
        requires = "continuous"
    )]
    window: String,

    /// Save the window when a call takes at least this long: 100ms.
    #[arg(long, value_name = "DURATION", requires = "continuous")]
    trigger_slower_than: Option<String>,

    /// Save the window when a call fails with one of these: EIO,ENOSPC.
    #[arg(long, value_name = "ERRNOS", requires = "continuous")]
    trigger_errno: Option<String>,

    /// The program to trace and its arguments.
    #[arg(last = true, value_name = "PROGRAM")]
    command: Vec<OsString>,
}

/// Runs `microscope syscalls`, exiting as the traced program did.
pub fn run(args: &SyscallsArgs) -> std::process::ExitCode {
    match trace(args) {
        Ok(code) => std::process::ExitCode::from(u8::try_from(code).unwrap_or(1)),
        Err(error) => {
            eprintln!("microscope: {error:#}");
            std::process::ExitCode::FAILURE
        }
    }
}

/// strace's PRECISION: s, ms, us, or ns.
fn precision(text: &str) -> Result<Precision, String> {
    Precision::named(text).ok_or_else(|| format!("{text:?} is not s, ms, us, or ns"))
}

/// The precision a short flag or its long form asks for, if either does:
/// the short flag's is microseconds.
fn precision_of(short: bool, long: Option<Precision>) -> Option<Precision> {
    long.or_else(|| short.then_some(Precision::Microseconds))
}

/// Nanoseconds in a duration such as `10ms`, `2us`, `1.5s`, or `5m`.
fn nanoseconds(text: &str) -> Result<u64> {
    let split = text
        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
        .unwrap_or(text.len());
    let (number, unit) = text.split_at(split);
    let scale = match unit {
        "" | "s" => 1e9,
        "ms" => 1e6,
        "us" | "µs" => 1e3,
        "ns" => 1.0,
        "m" => 60e9,
        "h" => 3600e9,
        _ => bail!("{text} is not a duration, such as 10ms, 2us, or 1s"),
    };

    let number: f64 = number
        .parse()
        .with_context(|| format!("{text} is not a duration, such as 10ms, 2us, or 1s"))?;
    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "checked to be finite and non-negative"
    )]
    if number.is_finite() && number >= 0.0 {
        Ok((number * scale) as u64)
    } else {
        bail!("{text} is not a duration")
    }
}

fn pids(texts: &[String]) -> Result<Vec<u32>> {
    let mut pids = Vec::new();

    for text in texts {
        for part in text.split([',', ' ']).filter(|part| !part.is_empty()) {
            let pid: u32 = part
                .parse()
                .with_context(|| format!("{part} is not a process id"))?;
            if pid == 0 {
                bail!("0 is not a process id");
            }

            if !pids.contains(&pid) {
                pids.push(pid);
            }
        }
    }

    Ok(pids)
}

/// A frame as strace's -k prints it: the module, the symbol and the
/// offset from it, and the address within the module.
fn write_frame(out: &mut String, frame: &Frame) {
    let _ = match (&frame.module, &frame.symbol) {
        (Some((module, address)), Some((symbol, offset))) => {
            write!(out, "{module}({symbol}+{offset:#x}) [{address:#x}]")
        }
        (Some((module, address)), None) => write!(out, "{module}() [{address:#x}]"),
        (None, _) => write!(out, "[unmapped] [{:#x}]", frame.address),
    };
}

/// An interpreted frame as `-k` names it: its function, and where in its
/// file it is, as the interpreter recorded them.
fn interpreted_text(call: &InterpretedCall) -> String {
    match (&call.file, call.line) {
        (Some(file), 0) => format!("{} at {file}", call.function),
        (Some(file), line) => format!("{} at {file}:{line}", call.function),
        (None, _) => call.function.to_string(),
    }
}

/// An interpreted frame as ndjson writes it.
fn interpreted_json(call: &InterpretedCall) -> Value {
    let mut object = Map::new();
    object.insert("function".into(), call.function.as_ref().into());
    if let Some(file) = &call.file {
        object.insert("file".into(), file.as_ref().into());
    }

    if call.line != 0 {
        object.insert("line".into(), call.line.into());
    }

    Value::Object(object)
}

/// The calls that end a thread or its process.
const ENDING: [&str; 2] = ["exit", "exit_group"];

/// The thread and process of an event that is no thread's, as a loss.
const NO_THREAD: Tid = Tid::new(0);
const NO_PROCESS: Pid = Pid::new(0);

/// Everything the flags decide, checked.
struct Plan {
    filter: Filter,
    target: Option<Target>,
    attached: Vec<u32>,
    follow: bool,
    sample: u64,
    in_flight: Option<u64>,
    columns: Vec<Column>,
    sort: Option<Column>,
    stacks: Option<Stacks>,
}

/// What -k asked for.
#[derive(Debug, Clone, Copy)]
struct Stacks {
    /// Unwind a copy of the stack by call-frame information, rather than
    /// walk the frame pointers; the ptrace engine always does.
    dwarf: bool,

    /// Name each frame's source line, as -kk does.
    lines: bool,
}

/// What -k asks for: given as -k, -kk, or -k=dwarf.
fn stacks(args: &SyscallsArgs) -> Result<Option<Stacks>> {
    let given: Vec<&str> = args.stacks.iter().map(String::as_str).collect();
    if given.is_empty() {
        return Ok(None);
    }

    if !given
        .iter()
        .all(|&unwind| unwind == "fp" || unwind == "dwarf")
    {
        bail!("-k takes nothing, or =dwarf");
    }

    let dwarf = given.contains(&"dwarf");
    if dwarf && args.engine == EngineArg::Ptrace {
        bail!(
            "-k=dwarf needs the BPF engine; the ptrace engine always unwinds by call-frame \
             information"
        );
    }

    Ok(Some(Stacks {
        dwarf,
        lines: given.len() > 1,
    }))
}

/// Whether the targets given can be traced as asked.
fn check_targets(args: &SyscallsArgs, attached: &[u32]) -> Result<()> {
    if args.engine == EngineArg::Ptrace {
        for (given, flag) in [
            (args.cgroup.is_some(), "--cgroup"),
            (args.container.is_some(), "--container"),
            (args.continuous.is_some(), "--continuous"),
            (args.ring.is_some(), "--ring"),
            (args.budget.is_some(), "--budget"),
            (args.attach != AttachArg::Auto, "--attach"),
        ] {
            if given {
                bail!("{flag} needs the BPF engine; the ptrace engine has no such choice");
            }
        }
    }

    let launching = !args.command.is_empty();
    let cgroup = args.cgroup.is_some() || args.container.is_some();
    if [launching, !attached.is_empty(), cgroup]
        .iter()
        .filter(|&&given| given)
        .count()
        > 1
    {
        bail!("give one target: a program after --, -p PID, --cgroup, or --container");
    }

    if args.summary_only
        && matches!(
            args.durations_precision,
            Some(Precision::Seconds | Precision::Milliseconds)
        )
    {
        bail!("-c's table shows microseconds or nanoseconds: give --syscall-times=us or ns");
    }

    if args.summary_only && (args.trigger_slower_than.is_some() || args.trigger_errno.is_some()) {
        bail!("a trigger needs the calls themselves, which -c does not keep");
    }

    if cgroup && args.no_follow {
        bail!("--no-follow cannot narrow a cgroup, whose every process is traced");
    }

    if !launching && attached.is_empty() && args.cgroup.is_none() && args.container.is_none() {
        bail!(
            "nothing to trace: give a program after --, or -p PID; microscope never traces the whole host"
        );
    }

    if args.detach_on.is_some() && launching && args.engine == EngineArg::Ptrace {
        bail!(
            "-b execve cannot detach a launched program from the ptrace engine: its seccomp filter \
             would then fail the traced calls with ENOSYS"
        );
    }

    if args.engine == EngineArg::Ptrace && !attached.is_empty() && !args.stop_every_call {
        bail!(
            "attaching the ptrace engine stops the process at every system call it makes, \
             a few microseconds each, because a running process cannot be given a filter. \
             Pass --stop-every-call to accept that, or use --engine bpf"
        );
    }

    Ok(())
}

fn plan(args: &SyscallsArgs) -> Result<Plan> {
    let mut expressions = args.expressions.clone();
    if args.successful_only {
        expressions.push("status=successful".to_owned());
    }

    if args.failed_only {
        expressions.push("status=failed".to_owned());
    }

    if let Some(fds) = &args.trace_fds {
        expressions.push(format!("trace-fds={fds}"));
    }

    let mut filter = Filter::parse(&expressions, &present::signal_named)
        .map_err(|error| anyhow::anyhow!("{error}"))?;
    filter.paths.clone_from(&args.paths);
    filter.slower_than = args.slower_than.as_deref().map(nanoseconds).transpose()?;
    let sample = match &args.sample {
        None => 1,
        Some(text) => text
            .strip_prefix("1/")
            .and_then(|n| n.parse::<u64>().ok())
            .filter(|&n| n > 0)
            .with_context(|| format!("--sample takes 1/N, not {text}"))?,
    };

    let in_flight = match args.in_flight.as_str() {
        "off" | "0" => None,
        text => Some(nanoseconds(text).context("--in-flight")?),
    };

    let columns = match &args.columns {
        Some(text) => summary::columns(text).map_err(anyhow::Error::msg)?,
        None => Column::DEFAULT.to_vec(),
    };

    let sort = summary::sort_key(&args.sort).map_err(anyhow::Error::msg)?;
    if let Some(call) = &args.detach_on
        && call != "execve"
    {
        bail!("-b takes only execve, not {call}");
    }

    let attached = pids(&args.pids)?;
    check_targets(args, &attached)?;
    let launching = !args.command.is_empty();
    let cgroup = args.cgroup.is_some() || args.container.is_some();

    if let Some(percent) = args.budget
        && !(percent.is_finite() && percent > 0.0)
    {
        bail!("--budget takes a percent of one CPU above 0, not {percent}");
    }

    let stacks = stacks(args)?;
    if cgroup && stacks.is_some() {
        bail!("-k cannot unwind a cgroup's stacks yet; trace a program or -p PID");
    }

    let target = if launching {
        let mut command = Command::new(&args.command[0]);
        command.args(&args.command[1..]);
        Some(Target::Launch(command))
    } else if !attached.is_empty() {
        Some(Target::Attach(attached.clone()))
    } else {
        None
    };

    Ok(Plan {
        filter,
        target,
        attached,
        follow: !args.no_follow,
        sample,
        in_flight,
        columns,
        sort,
        stacks,
    })
}

/// How the BPF engine runs, as the flags ask.
fn bpf_options(args: &SyscallsArgs, plan: &Plan, ring: Option<u32>) -> BpfOptions {
    // The kernel counts on its own what -c alone shows, when whether a
    // call counts depends only on which call it is.
    let count_only = args.summary_only
        && plan.filter.status == StatusSet::ALL
        && plan.filter.paths.is_empty()
        && plan.filter.fds.is_none()
        && plan.filter.slower_than.is_none();
    BpfOptions {
        count_only,
        ring,
        stack_copy: plan.stacks.is_some_and(|stacks| stacks.dwarf),
        attach: match args.attach {
            AttachArg::Auto => AttachMode::Auto,
            AttachArg::Tracepoint => AttachMode::Tracepoints,
            AttachArg::Fentry => AttachMode::Functions,
        },
        budget: args.budget,
    }
}

fn trace(args: &SyscallsArgs) -> Result<i32> {
    let mut plan = plan(args)?;
    let ring = args.ring.as_deref().map(ring_bytes).transpose()?;
    let target = plan.target.take();
    let cgroup = match (&args.cgroup, &args.container) {
        (Some(path), _) => Some(Cgroup::open(path)?),
        (None, Some(id)) => Some(Cgroup::container(id)?),
        (None, None) => None,
    };

    let stop = Arc::new(AtomicBool::new(false));
    let trigger = Arc::new(AtomicBool::new(false));
    super::profile::handle_signals(Some(stop.clone()), trigger.clone())?;
    if let Some(text) = &args.duration {
        stop_after(text, stop.clone())?;
    }

    // The descriptor table must see every call that changes it, shown or
    // not, for what -y, -P, and ndjson's paths say to be true.
    let mut calls = plan.filter.calls.clone();
    if args.decode_fds > 0 || !args.paths.is_empty() || args.format == OutputFormat::Ndjson {
        for id in microscope::syscalls::fds::followed() {
            calls.insert(id);
        }
    }

    // -n numbers a process's exit line with the call that ended it.
    if args.numbers {
        for abi in Abi::ALL {
            for name in ENDING {
                if let Some(id) = SyscallId::named(abi, name) {
                    calls.insert(id);
                }
            }
        }
    }

    let options = TraceOptions {
        calls,
        limits: Limits {
            string: args.string_limit.max(1),
            dump: if plan.filter.read_dumps.is_empty() && plan.filter.write_dumps.is_empty() {
                0
            } else {
                microscope::syscalls::captures::MOST_BYTES
            },
            ..Limits::default()
        },
        verbose: args.verbose,
        follow: plan.follow,
        stacks: plan.stacks.map(|_| SharedStacks::default()),
    };

    let mut printer = Printer::new(args, &plan)?;
    printer.stacks.clone_from(&options.stacks);
    printer.trigger = trigger;
    if let Some(directory) = &args.continuous {
        printer.continuous = Some(continuous(args, directory, options.stacks.clone())?);
    }

    if let Some((_, capture)) = &mut printer.capture {
        *capture = SyscallCapture::new(options.stacks.clone());
        let program: Vec<_> = args
            .command
            .iter()
            .map(|arg| arg.to_string_lossy())
            .collect();
        capture.setting("program", &program.join(" "));
        let engine = match args.engine {
            EngineArg::Bpf => "bpf",
            EngineArg::Ptrace => "ptrace",
        };

        capture.setting("syscalls", engine);
    }

    for &pid in &plan.attached {
        printer.processes.insert(Pid::new(pid));
    }

    let engine = bpf_options(args, &plan, ring);
    let finished = match (target, cgroup) {
        (Some(target), _) if args.engine == EngineArg::Ptrace => {
            ptrace::trace(target, &options, &stop, &mut printer).map_err(anyhow::Error::from)
        }
        (Some(target), _) => {
            bpf::trace(target, &options, engine, &stop, &mut printer).map_err(anyhow::Error::from)
        }
        (None, Some(cgroup)) => trace_cgroup(cgroup, &options, engine, &stop, &mut printer),
        (None, None) => bail!("nothing to trace"),
    };

    for &pid in &plan.attached {
        let process = Pid::new(pid);
        if printer.quiet == 0
            && printer.processes.contains(&process)
            && !printer.ended.contains(&process)
        {
            printer.message(&format!("Process {pid} detached"));
        }
    }

    printer.finish()?;
    let Finished { end } = finished?;

    // An attached process's end is its own business, as under strace.
    if !plan.attached.is_empty() {
        return Ok(0);
    }

    Ok(match end {
        Some(ProcessEnd::Exited(code)) => code & 0xff,
        Some(ProcessEnd::Killed { signal, .. }) => 128 + signal,
        None => 0,
    })
}

/// Traces every process in `cgroup`, with ids as its processes see them.
fn trace_cgroup(
    cgroup: Cgroup,
    options: &TraceOptions,
    engine: BpfOptions,
    stop: &AtomicBool,
    printer: &mut Printer,
) -> Result<Finished> {
    let namespace = match cgroup::pid_namespace(&cgroup.processes()) {
        Ok(namespace) => namespace,
        Err(reason) => {
            if printer.quiet == 0 {
                printer.message(&format!("Showing pids as the host sees them: {reason}"));
            }

            None
        }
    };

    printer.setting("cgroup", &cgroup.path.to_string_lossy());
    printer.cgroup = Some(cgroup.path.clone());

    Ok(bpf::trace_cgroup(
        cgroup, namespace, options, engine, stop, printer,
    )?)
}

/// Sets `stop` once `--duration`'s `text` has passed.
fn stop_after(text: &str, stop: Arc<AtomicBool>) -> Result<()> {
    let after = Duration::from_nanos(nanoseconds(text).context("--duration")?);
    std::thread::Builder::new()
        .name("microscope-syscalls-timer".into())
        .spawn(move || {
            std::thread::sleep(after);
            stop.store(true, Ordering::Relaxed);
        })
        .context("starting the --duration timer")?;

    Ok(())
}

/// A continuous trace into `directory`, as the flags ask.
fn continuous(
    args: &SyscallsArgs,
    directory: &std::path::Path,
    stacks: Option<SharedStacks>,
) -> Result<Continuous> {
    std::fs::create_dir_all(directory)
        .with_context(|| format!("creating {}", directory.display()))?;
    let segment = nanoseconds(&args.segment).context("--segment")?;
    if segment == 0 {
        bail!("--segment must be longer than nothing");
    }

    let errnos = match &args.trigger_errno {
        Some(names) => names
            .split(',')
            .map(|name| {
                present::errno_named(name)
                    .map(|errno| errno.0)
                    .with_context(|| format!("--trigger-errno: {name} is no errno"))
            })
            .collect::<Result<Vec<_>>>()?,
        None => Vec::new(),
    };

    let mut settings = vec![(
        "syscalls".to_owned(),
        match args.engine {
            EngineArg::Bpf => "bpf",
            EngineArg::Ptrace => "ptrace",
        }
        .to_owned(),
    )];
    let program: Vec<_> = args
        .command
        .iter()
        .map(|arg| arg.to_string_lossy())
        .collect();
    settings.push(("program".to_owned(), program.join(" ")));
    let options = ContinuousOptions {
        directory: directory.to_owned(),
        keep: super::profile::duration(&args.keep).context("--keep")?,
        segment,
        // -c keeps no calls to hold.
        window: if args.summary_only {
            0
        } else {
            nanoseconds(&args.window).context("--window")?
        },
        slower_than: args
            .trigger_slower_than
            .as_deref()
            .map(nanoseconds)
            .transpose()
            .context("--trigger-slower-than")?,
        errnos,
        settings,
    };

    eprintln!(
        "microscope: writing a segment every {} to {}, keeping {}; `kill -USR2 {}` saves the last {}",
        args.segment,
        directory.display(),
        args.keep,
        std::process::id(),
        args.window
    );

    Ok(Continuous::new(options, stacks, monotonic()))
}

/// Where one process's lines go.
type Output = Box<dyn Write + Send>;

/// The sink that prints: what strace's output loop does.
#[expect(clippy::struct_excessive_bools, reason = "one per output flag")]
struct Printer {
    filter: Filter,
    format: OutputFormat,
    style: Style,

    /// The renderer that colors text; none for files.
    renderer: Option<Renderer>,
    output: Output,

    /// With -ff: FILE, each process writing FILE.PID.
    per_process: Option<(PathBuf, BTreeMap<u32, Output>)>,

    /// Prefixes `PID ` rather than `[pid PID] `, as strace does in files.
    bare_pid: bool,
    show_calls: bool,
    summarize: bool,
    stats: BTreeMap<SyscallId, SyscallStats>,
    fds: FdTable,
    timestamps: u8,
    relative: Option<Precision>,
    durations: Option<Precision>,

    /// The table's unit: nanoseconds when -T's precision is.
    unit: summary::Unit,
    numbers: bool,
    column: usize,
    comm: bool,
    quiet: u8,
    follow: bool,
    sample: u64,
    sampled: u64,
    in_flight: Option<u64>,

    /// The cgroup traced, said once its programs are attached.
    cgroup: Option<PathBuf>,

    /// The budget's notes so far, each a setting of the capture's.
    budget_notes: u32,
    continuous: Option<Continuous>,

    /// Set by SIGUSR2, which saves a continuous trace's window.
    trigger: Arc<AtomicBool>,
    columns: Vec<Column>,
    sort: Option<Column>,

    /// `CLOCK_REALTIME` minus `CLOCK_MONOTONIC`, and the local zone's
    /// offset, in nanoseconds.
    realtime: i128,
    zone: i64,
    previous: Option<u64>,
    threads: BTreeSet<Tid>,
    processes: BTreeSet<Pid>,
    ended: BTreeSet<Pid>,
    names: BTreeMap<Pid, String>,

    /// The number of the call that ended each process, and each thread,
    /// when one did.
    endings: BTreeMap<Pid, u32>,
    thread_endings: BTreeMap<Tid, u32>,

    /// In-flight calls already reported, by thread and entry time.
    reported: BTreeSet<(Tid, u64)>,

    /// Each thread's last ABI, where it is not x86-64's.
    personalities: BTreeMap<Tid, Abi>,
    error: Option<io::Error>,

    /// The ndjson line of a call being written.
    line: Vec<u8>,

    /// Names the frames of each call's stack, with -k.
    stacks: Option<SharedStacks>,

    /// With -kk, each frame's source line too.
    stack_lines: bool,
    frame_limit: usize,

    /// With `-o FILE.usprof`, the file and the capture of what is shown.
    capture: Option<(PathBuf, SyscallCapture)>,
}

/// Writes a file on a thread of its own, so that the thread reading the
/// engine spends no time in the kernel's write path: at millions of calls
/// a second, it has none to spare.
struct Background {
    buffer: Vec<u8>,
    to_writer: Option<std::sync::mpsc::SyncSender<Vec<u8>>>,

    /// Buffers written, emptied for reuse, or the error writing one met.
    from_writer: std::sync::mpsc::Receiver<io::Result<Vec<u8>>>,

    /// Buffers sent and not yet returned.
    sent: usize,
    writer: Option<std::thread::JoinHandle<()>>,
}

/// The bytes collected before they are sent to be written.
const BACKGROUND_BYTES: usize = 256 * 1024;

impl Background {
    fn new(mut file: File) -> Self {
        let (to_writer, from_main) = std::sync::mpsc::sync_channel::<Vec<u8>>(8);
        let (to_main, from_writer) = std::sync::mpsc::channel();
        let writer = std::thread::Builder::new()
            .name("microscope-output".into())
            .spawn(move || {
                // An empty buffer asks for a flush.
                for mut buffer in from_main {
                    let written = if buffer.is_empty() {
                        file.flush()
                    } else {
                        file.write_all(&buffer)
                    };

                    buffer.clear();
                    if to_main.send(written.map(|()| buffer)).is_err() {
                        break;
                    }
                }
            })
            .ok();

        Self {
            buffer: Vec::with_capacity(BACKGROUND_BYTES),
            to_writer: Some(to_writer),
            from_writer,
            sent: 0,
            writer,
        }
    }

    fn send(&mut self, buffer: Vec<u8>) -> io::Result<()> {
        let gone = || io::Error::other("the output thread ended");
        if self.writer.is_none() {
            return Err(io::Error::other("the output thread could not start"));
        }

        self.to_writer
            .as_ref()
            .ok_or_else(gone)?
            .send(buffer)
            .map_err(|_| gone())?;
        self.sent += 1;
        Ok(())
    }

    /// Sends what is collected, reusing a written buffer when one is back.
    fn send_collected(&mut self) -> io::Result<()> {
        let mut next = None;
        while let Ok(returned) = self.from_writer.try_recv() {
            self.sent -= 1;
            next = Some(returned?);
        }

        let next = next.unwrap_or_else(|| Vec::with_capacity(BACKGROUND_BYTES));
        let full = std::mem::replace(&mut self.buffer, next);
        self.send(full)
    }
}

impl Write for Background {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.buffer.extend_from_slice(bytes);
        if self.buffer.len() >= BACKGROUND_BYTES {
            self.send_collected()?;
        }

        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        if !self.buffer.is_empty() {
            self.send_collected()?;
        }

        self.send(Vec::new())?;
        let mut result = Ok(());
        while self.sent > 0 {
            let returned = self
                .from_writer
                .recv()
                .map_err(|_| io::Error::other("the output thread ended"))?;
            self.sent -= 1;
            if let Err(error) = returned
                && result.is_ok()
            {
                result = Err(error);
            }
        }

        result
    }
}

impl Drop for Background {
    fn drop(&mut self) {
        let _ = self.flush();
        self.to_writer = None;
        if let Some(writer) = self.writer.take() {
            let _ = writer.join();
        }
    }
}

/// A call's ndjson record. A traced program can make millions of calls a
/// second, so it writes its own JSON rather than building a map; its keys
/// come in name order, as the other records' maps put theirs.
struct CallRecord {
    abi: Option<&'static str>,
    args: Vec<(String, String)>,
    duration_ns: Option<u64>,
    errno: Option<String>,
    name: std::borrow::Cow<'static, str>,
    number: u32,
    path: Option<String>,
    pid: u32,
    process_exited: bool,
    ret: Option<i64>,

    /// The stack, already JSON: an array of frames.
    stack: Option<String>,
    text: String,
    tid: u32,
    time_ns: i64,
    unavailable: bool,
    unfinished: bool,
}

impl CallRecord {
    fn write(&self, out: &mut Vec<u8>) {
        out.push(b'{');
        if let Some(abi) = self.abi {
            json_field(out, b"\"abi\":", abi);
        }

        out.extend_from_slice(b"\"args\":{");
        for (index, (name, value)) in self.args.iter().enumerate() {
            if index > 0 {
                out.push(b',');
            }

            json_string(out, name);
            out.push(b':');
            json_string(out, value);
        }

        out.extend_from_slice(b"},");
        if let Some(duration) = self.duration_ns {
            json_number(
                out,
                b"\"duration_ns\":",
                duration.try_into().unwrap_or(i64::MAX),
            );
        }

        if let Some(errno) = &self.errno {
            json_field(out, b"\"errno\":", errno);
        }

        json_field(out, b"\"name\":", &self.name);
        json_number(out, b"\"number\":", self.number.into());
        if let Some(path) = &self.path {
            json_field(out, b"\"path\":", path);
        }

        json_number(out, b"\"pid\":", self.pid.into());
        if self.process_exited {
            out.extend_from_slice(b"\"process_exited\":true,");
        }

        if let Some(ret) = self.ret {
            json_number(out, b"\"return\":", ret);
        }

        if let Some(stack) = &self.stack {
            out.extend_from_slice(b"\"stack\":");
            out.extend_from_slice(stack.as_bytes());
            out.push(b',');
        }

        json_field(out, b"\"text\":", &self.text);
        json_number(out, b"\"tid\":", self.tid.into());
        json_number(out, b"\"time_ns\":", self.time_ns);
        out.extend_from_slice(b"\"type\":\"call\"");
        if self.unavailable {
            out.extend_from_slice(b",\"unavailable\":true");
        }

        if self.unfinished {
            out.extend_from_slice(b",\"unfinished\":true");
        }

        out.push(b'}');
    }
}

/// The decimal pairs 00 to 99, for writing numbers two digits at a time,
/// as itoa does: times have nineteen.
const PAIRS: &[u8; 200] = b"0001020304050607080910111213141516171819\
    2021222324252627282930313233343536373839\
    4041424344454647484950515253545556575859\
    6061626364656667686970717273747576777879\
    8081828384858687888990919293949596979899";

/// Writes `"key":value,`, given `"key":`.
fn json_number(out: &mut Vec<u8>, key: &[u8], value: i64) {
    out.extend_from_slice(key);
    if value < 0 {
        out.push(b'-');
    }

    let mut digits = [0; 20];
    let mut rest = value.unsigned_abs();
    let mut at = digits.len();

    while rest >= 100 {
        let pair = usize::try_from(rest % 100).unwrap_or(0) * 2;
        rest /= 100;
        at -= 2;
        digits[at..at + 2].copy_from_slice(&PAIRS[pair..pair + 2]);
    }

    let pair = usize::try_from(rest).unwrap_or(0) * 2;
    if rest >= 10 {
        at -= 2;
        digits[at..at + 2].copy_from_slice(&PAIRS[pair..pair + 2]);
    } else {
        at -= 1;
        digits[at] = PAIRS[pair + 1];
    }

    out.extend_from_slice(&digits[at..]);
    out.push(b',');
}

/// Writes `"key":"value",`, given `"key":`.
fn json_field(out: &mut Vec<u8>, key: &[u8], value: &str) {
    out.extend_from_slice(key);
    json_string(out, value);
    out.push(b',');
}

/// Writes a JSON string as `serde_json` does.
fn json_string(out: &mut Vec<u8>, text: &str) {
    out.push(b'"');
    let bytes = text.as_bytes();
    if !bytes
        .iter()
        .any(|&byte| byte < 0x20 || byte == b'"' || byte == b'\\')
    {
        out.extend_from_slice(bytes);
        out.push(b'"');
        return;
    }

    let mut start = 0;

    for (index, &byte) in bytes.iter().enumerate() {
        let escape: &[u8] = match byte {
            b'"' => b"\\\"",
            b'\\' => b"\\\\",
            b'\n' => b"\\n",
            b'\r' => b"\\r",
            b'\t' => b"\\t",
            0x08 => b"\\b",
            0x0c => b"\\f",
            0..0x20 => b"",
            _ => continue,
        };

        out.extend_from_slice(&bytes[start..index]);
        start = index + 1;
        if escape.is_empty() {
            const HEX: &[u8; 16] = b"0123456789abcdef";
            out.extend_from_slice(b"\\u00");
            out.push(HEX[usize::from(byte >> 4)]);
            out.push(HEX[usize::from(byte & 0xf)]);
        } else {
            out.extend_from_slice(escape);
        }
    }

    out.extend_from_slice(&bytes[start..]);
    out.push(b'"');
}

/// What the wall clock read when the monotonic clock began, in
/// nanoseconds, given both clocks' readings now; and the second the wall
/// clock reads now, at which the local zone's offset is taken.
fn clocks(wall: i128, monotonic: u64) -> (i128, i64) {
    let realtime = wall - i128::from(monotonic);
    #[expect(
        clippy::cast_possible_truncation,
        reason = "seconds since the epoch fit"
    )]
    (realtime, (wall / 1_000_000_000) as i64)
}

/// The local zone's offset from UTC at `seconds` since the epoch.
#[allow(
    unsafe_code,
    reason = "libc's localtime_r is the only way to read the zone"
)]
fn zone_offset(seconds: i64) -> i64 {
    // SAFETY: zeroes are a valid `tm`, which is plain data.
    let mut local: nix::libc::tm = unsafe { std::mem::zeroed() };

    // SAFETY: both pointers name live locals of the right types, and
    // localtime_r writes only through the second.
    let converted = unsafe { nix::libc::localtime_r(&raw const seconds, &raw mut local) };
    if converted.is_null() {
        0
    } else {
        local.tm_gmtoff
    }
}

/// How a thread or process ended, as ndjson writes it.
fn insert_end(object: &mut Map<String, Value>, end: ProcessEnd) {
    match end {
        ProcessEnd::Exited(code) => {
            object.insert("code".into(), code.into());
        }
        ProcessEnd::Killed { signal, core } => {
            object.insert("signal".into(), present::signal(signal).into());
            object.insert("core".into(), core.into());
        }
    }
}

/// How the flags say calls print.
const fn style(args: &SyscallsArgs) -> Style {
    Style {
        string_limit: args.string_limit,
        hex: match args.hex {
            0 => Hex::Never,
            1 => Hex::NonAscii,
            _ => Hex::All,
        },
        constants: match args.constants {
            None | Some(ConstantsArg::Abbrev) => Constants::Abbrev,
            Some(ConstantsArg::Raw) => Constants::Raw,
            Some(ConstantsArg::Verbose) => Constants::Verbose,
        },
        verbose: args.verbose,
        fds: match args.decode_fds {
            0 => FdStyle::Number,
            1 => FdStyle::Paths,
            _ => FdStyle::All,
        },
        arg_names: args.arg_names,
        raw: false,
    }
}

impl Printer {
    fn new(args: &SyscallsArgs, plan: &Plan) -> Result<Self> {
        let open = |path: &std::path::Path| -> Result<File> {
            std::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .append(args.append)
                .truncate(!args.append)
                .open(path)
                .with_context(|| format!("opening {}", path.display()))
        };

        let to_file = args.output.as_ref().filter(|path| path.as_os_str() != "-");
        let capture = to_file.filter(|path| path.extension().is_some_and(|ext| ext == "usprof"));
        if capture.is_some() && (args.follow >= 2 || args.append) {
            bail!("-ff and -A write text: a capture is one file, written once");
        }

        let color = args.color.unwrap_or_default();
        let (output, renderer, per_process): (Output, Option<Renderer>, _) =
            match (&args.output, to_file) {
                (_, Some(_)) if capture.is_some() => (Box::new(io::sink()), None, None),
                _ if args.continuous.is_some() => (Box::new(io::sink()), None, None),
                (_, Some(path)) if args.follow >= 2 => (
                    Box::new(io::sink()),
                    None,
                    Some((path.clone(), BTreeMap::new())),
                ),
                (_, Some(path)) => (Box::new(Background::new(open(path)?)), None, None),
                (Some(_), None) => (
                    Box::new(LineWriter::new(io::stdout())),
                    Some(Renderers::detect(color, false).stdout),
                    None,
                ),
                (None, None) => (
                    Box::new(LineWriter::new(io::stderr())),
                    Some(Renderers::detect(color, false).stderr),
                    None,
                ),
            };

        let wall = nix::time::clock_gettime(nix::time::ClockId::CLOCK_REALTIME).map_or(0, |time| {
            i128::from(time.tv_sec()) * 1_000_000_000 + i128::from(time.tv_nsec())
        });
        let (realtime, now) = clocks(wall, monotonic());
        let zone = zone_offset(now);

        Ok(Self {
            filter: plan.filter.clone(),
            format: args.format,
            style: style(args),
            renderer,
            output,
            bare_pid: to_file.is_some() && plan.follow,
            per_process,
            show_calls: !args.summary_only && args.continuous.is_none(),
            summarize: args.summary_only || args.summary_too,
            stats: BTreeMap::new(),
            fds: FdTable::default(),
            timestamps: args.timestamps,
            relative: precision_of(args.relative, args.relative_precision),
            durations: precision_of(args.durations, args.durations_precision),
            unit: if args.durations_precision == Some(Precision::Nanoseconds) {
                summary::Unit::Nanoseconds
            } else {
                summary::Unit::Microseconds
            },
            numbers: args.numbers,
            column: args.column,
            comm: args.comm,
            // As under strace, a trace written to a file says nothing
            // more on standard error.
            quiet: if to_file.is_some() || args.continuous.is_some() {
                args.quiet.max(1)
            } else {
                args.quiet
            },
            follow: plan.follow,
            sample: plan.sample,
            sampled: 0,
            in_flight: plan.in_flight,
            cgroup: None,
            budget_notes: 0,
            continuous: None,
            trigger: Arc::new(AtomicBool::new(false)),
            columns: plan.columns.clone(),
            sort: plan.sort,
            realtime,
            zone,
            previous: None,
            threads: BTreeSet::new(),
            processes: BTreeSet::new(),
            ended: BTreeSet::new(),
            names: BTreeMap::new(),
            endings: BTreeMap::new(),
            thread_endings: BTreeMap::new(),
            reported: BTreeSet::new(),
            personalities: BTreeMap::new(),
            error: None,
            line: Vec::new(),
            stacks: None,
            stack_lines: plan.stacks.is_some_and(|stacks| stacks.lines),
            frame_limit: args.stack_trace_frame_limit,
            capture: capture.map(|path| (path.clone(), SyscallCapture::new(None))),
        })
    }

    /// Records how the trace was made in its capture and in a continuous
    /// trace's files.
    fn setting(&mut self, key: &str, value: &str) {
        if let Some((_, capture)) = &mut self.capture {
            capture.setting(key, value);
        }

        if let Some(continuous) = &mut self.continuous {
            continuous.setting(key, value);
        }
    }

    /// A line of microscope's own, as strace's `strace: ...` lines.
    fn message(&mut self, text: &str) {
        if self.format == OutputFormat::Ndjson {
            return;
        }

        let line = format!("microscope: {text}\n");
        if let Err(error) = io::stderr().write_all(line.as_bytes()) {
            self.error.get_or_insert(error);
        }
    }

    fn write(&mut self, process: Pid, line: &str) {
        self.write_bytes(process.get(), line.as_bytes());
    }

    /// Writes a line of one thread's, which -ff writes to the thread's own
    /// file, as strace does.
    fn write_thread(&mut self, process: Pid, thread: Tid, line: &str) {
        let file = if self.per_process.is_some() && thread != NO_THREAD {
            thread.get()
        } else {
            process.get()
        };

        self.write_bytes(file, line.as_bytes());
    }

    /// Writes a line to the trace, or with -ff to `file`'s.
    fn write_bytes(&mut self, file: u32, line: &[u8]) {
        let output = match &mut self.per_process {
            Some((path, outputs)) => match outputs.entry(file) {
                std::collections::btree_map::Entry::Occupied(entry) => Some(entry.into_mut()),
                std::collections::btree_map::Entry::Vacant(entry) => {
                    let mut name = path.clone().into_os_string();
                    name.push(format!(".{file}"));
                    match File::create(&name) {
                        Ok(file) => Some(entry.insert(Box::new(Background::new(file)))),
                        Err(error) => {
                            self.error.get_or_insert(error);
                            return;
                        }
                    }
                }
            },
            None => Some(&mut self.output),
        };

        if let Some(output) = output
            && let Err(error) = output
                .write_all(line)
                .and_then(|()| output.write_all(b"\n"))
        {
            self.error.get_or_insert(error);
        }
    }

    fn paint(&self, tokens: &Tokens) -> String {
        let Some(renderer) = self.renderer else {
            return tokens.text();
        };

        tokens
            .0
            .iter()
            .map(|token| {
                let role = match token.kind {
                    Kind::Name | Kind::Pid => Some(Role::Name),
                    Kind::Number => Some(Role::Number),
                    Kind::String => Some(Role::String),
                    Kind::Constant => Some(Role::Type),
                    Kind::Fd | Kind::Path | Kind::Address => Some(Role::Metadata),
                    Kind::Error => Some(Role::Error),
                    Kind::Comment | Kind::Time => Some(Role::Muted),
                    Kind::Missing => Some(Role::Warning),
                    Kind::Punctuation | Kind::Text => None,
                };

                role.map_or_else(
                    || token.text.clone(),
                    |role| renderer.paint(role, &token.text).to_string(),
                )
            })
            .collect()
    }

    /// strace's leader: the pid, the time, and the call's number.
    fn leader(&mut self, thread: Tid, process: Pid, time: u64, number: Option<u32>) -> Tokens {
        let mut out = Tokens::default();

        // A loss is no thread's, and names none.
        if self.per_process.is_none() && self.threads.len() > 1 && thread != NO_THREAD {
            let name = self
                .comm
                .then(|| self.names.get(&process).map(|name| format!("<{name}>")))
                .flatten()
                .unwrap_or_default();
            if self.bare_pid {
                out.push(Kind::Pid, format!("{thread:<5}{name}"));
                out.push(Kind::Punctuation, " ");
            } else {
                out.push(Kind::Punctuation, "[pid ");
                out.push(Kind::Pid, format!("{thread:>5}{name}"));
                out.push(Kind::Punctuation, "] ");
            }
        }

        if let Some(precision) = self.relative {
            let since = self
                .previous
                .map_or(0, |previous| time.saturating_sub(previous));

            // strace pads the whole seconds to six places.
            let since = present::seconds(since, precision);
            let width = since.find('.').map_or(6, |point| since.len() - point + 6);
            out.push(Kind::Time, format!("{since:>width$} "));
        }

        self.previous = Some(time);
        if self.timestamps > 0 {
            let wall = self.realtime + i128::from(time);
            #[expect(
                clippy::cast_possible_truncation,
                clippy::cast_sign_loss,
                reason = "the epoch's seconds and a fraction"
            )]
            let (seconds, micros) = (
                (wall / 1_000_000_000) as i64,
                (wall % 1_000_000_000 / 1_000) as u32,
            );
            let text = if self.timestamps >= 3 {
                format!("{seconds}.{micros:06}")
            } else {
                let local = (seconds + self.zone).rem_euclid(86_400);
                let clock = format!(
                    "{:02}:{:02}:{:02}",
                    local / 3600,
                    local / 60 % 60,
                    local % 60
                );
                if self.timestamps == 2 {
                    format!("{clock}.{micros:06}")
                } else {
                    clock
                }
            };

            out.push(Kind::Time, text);
            out.push(Kind::Punctuation, " ");
        }

        if self.numbers
            && let Some(number) = number
        {
            out.push(Kind::Punctuation, format!("[{number:>4}] "));
        }

        out
    }

    fn call(&mut self, event: &SyscallEvent) {
        if !self.follow && !self.processes.is_empty() && !self.processes.contains(&event.process) {
            return;
        }

        if event.call.name().is_some_and(|name| ENDING.contains(&name)) {
            self.endings.insert(event.process, event.call.number);
            self.thread_endings.insert(event.thread, event.call.number);
        }

        let paths = filter::paths(event);
        let path_refs: Vec<&str> = paths.iter().map(String::as_str).collect();
        let fds = &self.fds;
        let shows = self.filter.shows(event, &path_refs, &|fd| {
            fds.describe(event.process, fd)
                .map(|description| description.text.to_string())
        });
        if shows && let Some((_, capture)) = &mut self.capture {
            capture.call(event);
        }

        if shows && self.continuous.is_some() {
            self.continue_with(&TraceEvent::Call(event.clone()));
        }

        if shows && self.summarize {
            self.stats.entry(event.call).or_default().add(
                event.outcome,
                event.duration(),
                bytes_moved(event),
            );
        }

        let mut style = self.style.clone();
        style.raw = self.filter.raw.contains(event.call);
        style.verbose = style.verbose || !self.filter.verbose.contains(event.call);

        // strace prints what descriptors named before the call, and what
        // the result names after it.
        let entry = (shows && self.show_calls).then(|| {
            (
                present::entry(
                    event,
                    &style,
                    Context {
                        fds: Some(&self.fds),
                        before: true,
                    },
                ),
                present::argument_values(
                    event,
                    &style,
                    Context {
                        fds: Some(&self.fds),
                        before: true,
                    },
                ),
            )
        });
        self.fds.apply_call(event);
        let Some((entry, arguments)) = entry else {
            return;
        };

        self.sampled += 1;
        if !(self.sampled - 1).is_multiple_of(self.sample) {
            return;
        }

        let returned = present::returned(
            event,
            &style,
            Context {
                fds: Some(&self.fds),
                before: false,
            },
        );
        self.personality(event);
        match self.format {
            OutputFormat::Text => self.text(event, entry, returned),
            OutputFormat::Ndjson => {
                let mut text = String::with_capacity(entry.text_len() + returned.text_len());
                entry.append_to(&mut text);
                returned.append_to(&mut text);
                let record = self.record(event, arguments, paths, text);
                let mut line = std::mem::take(&mut self.line);
                line.clear();
                record.write(&mut line);
                self.write_bytes(event.process.get(), &line);
                self.line = line;
            }
        }
    }

    /// A stack as ndjson writes it: each native frame's module, its
    /// address there, symbol, offset, and source line; each interpreted
    /// frame's function, file, and line; and last, why it ends short.
    fn stack_json(&self, process: Pid, stack: &CallStack) -> Value {
        let Some(stacks) = &self.stacks else {
            return Value::Null;
        };

        let frames = stacks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .frames(process, &stack.addresses);
        let mut array: Vec<Value> = stack
            .frames()
            .filter_map(|frame| {
                let frame = match frame {
                    Ok(index) => frames.get(index)?,
                    Err(call) => return Some(interpreted_json(call)),
                };

                let mut object = Map::new();

                // A frame in no module has only its virtual address.
                let address = frame
                    .module
                    .as_ref()
                    .map_or_else(|| frame.address.get(), |(_, address)| address.get());
                object.insert("address".into(), format!("{address:#x}").into());
                if let Some((module, _)) = &frame.module {
                    object.insert("module".into(), module.as_ref().into());
                }

                if let Some((symbol, offset)) = &frame.symbol {
                    object.insert("symbol".into(), symbol.as_ref().into());
                    object.insert("offset".into(), (*offset).into());
                }

                if let Some((file, line)) = &frame.source {
                    object.insert("file".into(), file.as_str().into());
                    object.insert("line".into(), (*line).into());
                }

                Some(Value::Object(object))
            })
            .collect();
        if let Some(reason) = stack.truncation() {
            array.push(json!({ "ends": reason.describe() }));
        }

        Value::Array(array)
    }

    /// The lines naming the frames of a call's stack, up to the limit, and
    /// why it ends short, if it does.
    fn stack_lines(&self, process: Pid, stack: &CallStack) -> Vec<String> {
        let Some(stacks) = &self.stacks else {
            return Vec::new();
        };

        let shown: Vec<_> = stack.frames().take(self.frame_limit).collect();
        let natives = shown.iter().filter(|frame| frame.is_ok()).count();
        let frames = stacks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .frames(process, &stack.addresses[..natives]);
        let mut lines: Vec<String> = shown
            .iter()
            .filter_map(|frame| {
                let frame = match frame {
                    Ok(index) => frames.get(*index)?,
                    Err(call) => return Some(format!(" > {}", interpreted_text(call))),
                };

                let mut line = String::from(" > ");
                write_frame(&mut line, frame);
                if self.stack_lines
                    && let Some((file, number)) = &frame.source
                {
                    let _ = write!(line, " {file}:{number}");
                }

                Some(line)
            })
            .collect();
        let total = stack.addresses.len() + stack.interpreted.len();
        if shown.len() < total {
            lines.push(format!(" > ... {} more frames", total - shown.len()));
        } else if let Some(reason) = stack.truncation() {
            lines.push(format!(" > [stack ends: {}]", reason.describe()));
        }

        lines
    }

    /// Dumps what a call read or wrote through a descriptor `-e read=` or
    /// `-e write=` names.
    fn dump(&mut self, event: &SyscallEvent) {
        let Some((fd, direction)) = present::dumps::moves(event) else {
            return;
        };

        let dumps = match direction {
            present::dumps::Direction::Read => &self.filter.read_dumps,
            present::dumps::Direction::Write => &self.filter.write_dumps,
        };

        if !dumps.contains(&fd) {
            return;
        }

        for line in present::dumps::dump(event) {
            self.write_thread(event.process, event.thread, &line);
        }
    }

    /// A call's line as strace writes it, with its dumps and stack.
    fn text(&mut self, event: &SyscallEvent, entry: Tokens, returned: Tokens) {
        let (thread, process) = (event.thread, event.process);
        let mut line = self.leader(thread, process, event.time(), Some(event.call.number));
        line.0.extend(entry.0);
        let width = line.text().len();

        // strace pads so that ` = ` begins at the column.
        if width + 1 < self.column {
            line.push(Kind::Punctuation, " ".repeat(self.column - 1 - width));
        }

        line.0.extend(returned.0);
        if let Some(precision) = self.durations
            && let Some(duration) = event.duration()
        {
            line.push(
                Kind::Time,
                format!(" <{}>", present::seconds(duration, precision)),
            );
        }

        let text = self.paint(&line);
        self.write_thread(process, thread, &text);
        self.dump(event);
        if let Some(stack) = &event.stack {
            for line in self.stack_lines(process, stack) {
                self.write_thread(process, thread, &line);
            }
        }
    }

    /// strace's note that a thread changed its ABI.
    fn personality(&mut self, event: &SyscallEvent) {
        let personality = self
            .personalities
            .get(&event.thread)
            .copied()
            .unwrap_or(Abi::X86_64);
        if personality == event.call.abi {
            return;
        }

        if event.call.abi == Abi::X86_64 {
            self.personalities.remove(&event.thread);
        } else {
            self.personalities.insert(event.thread, event.call.abi);
        }

        if self.format == OutputFormat::Text && self.quiet == 0 {
            let mode = match event.call.abi {
                Abi::X86_64 => "64 bit",
                Abi::X86_32 => "32 bit",
                Abi::X32 => "x32",
            };

            let note = format!("[ Process PID={} runs in {mode} mode. ]", event.thread);
            self.write(event.process, &note);
        }
    }

    /// A call's ndjson record.
    fn record(
        &self,
        event: &SyscallEvent,
        arguments: Vec<(String, Tokens)>,
        paths: Vec<String>,
        text: String,
    ) -> CallRecord {
        let wall = self.realtime + i128::from(event.time());
        let (ret, errno) = match event.outcome {
            Outcome::Returned(value) => (Some(value), None),
            Outcome::Failed(errno) => (Some(-i64::from(errno.0)), Some(present::errno(errno).0)),
            _ => (None, None),
        };

        let path = paths.into_iter().next().or_else(|| {
            filter::descriptors(event)
                .find_map(|fd| self.fds.describe(event.process, fd))
                .map(|description| description.text.to_string())
        });
        let mut args: Vec<(String, String)> = arguments
            .into_iter()
            .map(|(name, tokens)| (name, tokens.text()))
            .collect();
        args.sort_unstable_by(|a, b| a.0.cmp(&b.0));

        CallRecord {
            abi: (event.call.abi != Abi::X86_64).then(|| event.call.abi.suffix()),
            args,
            duration_ns: event.duration(),
            errno,
            name: event.call.display_name(),
            number: event.call.number,
            path,
            pid: event.process.get(),
            process_exited: event.outcome == Outcome::ProcessExited,
            ret,
            stack: event
                .stack
                .as_ref()
                .map(|stack| self.stack_json(event.process, stack).to_string()),
            text,
            tid: event.thread.get(),
            time_ns: i64::try_from(wall).unwrap_or(0),
            unavailable: event.outcome == Outcome::Unavailable,
            unfinished: event.outcome == Outcome::Unfinished,
        }
    }

    fn base(&self, kind: &str, time: u64, thread: Option<Tid>, process: Pid) -> Map<String, Value> {
        let mut object = Map::new();
        object.insert("type".into(), kind.into());
        let wall = self.realtime + i128::from(time);
        object.insert("time_ns".into(), json!(i64::try_from(wall).unwrap_or(0)));
        object.insert("pid".into(), process.get().into());
        if let Some(thread) = thread {
            object.insert("tid".into(), thread.get().into());
        }

        object
    }

    fn json(&mut self, process: Pid, object: Map<String, Value>) {
        let line = Value::Object(object).to_string();
        self.write(process, &line);
    }

    /// Adds an event to the continuous trace, if there is one.
    fn continue_with(&mut self, event: &TraceEvent) {
        if let Some(continuous) = &mut self.continuous {
            let saved = continuous.event(event);
            self.saved(saved);
        }
    }

    /// Says where a triggered window went.
    fn saved(&mut self, saved: io::Result<Option<Saved>>) {
        match saved {
            Ok(Some(saved)) if self.quiet < 2 => self.message(&format!(
                "{}: saved the last {} calls to {}",
                saved.reason,
                saved.calls,
                saved.path.display()
            )),
            Err(error) => self.message(&format!("saving a window failed: {error}")),
            _ => {}
        }
    }

    /// Prints a signal, exit, or loss line.
    fn other(&mut self, event: &TraceEvent) {
        let (thread, process) = match *event {
            TraceEvent::Signal {
                thread,
                process,
                signal,
                ..
            } => {
                if !self.filter.signals.contains(&signal) {
                    return;
                }

                (thread, process)
            }
            TraceEvent::Exit { process, .. } => {
                if self.quiet >= 2 {
                    return;
                }

                (process.leader(), process)
            }

            TraceEvent::ThreadExit {
                thread, process, ..
            } => {
                if self.quiet >= 2 {
                    return;
                }

                (thread, process)
            }
            TraceEvent::Lost { .. } => (NO_THREAD, NO_PROCESS),
            _ => return,
        };

        if !self.follow && !self.processes.contains(&process) && process != NO_PROCESS {
            return;
        }

        if !self.show_calls && !matches!(event, TraceEvent::Lost { .. }) {
            return;
        }

        match self.format {
            OutputFormat::Text => {
                let Some(tokens) = present::other(event) else {
                    return;
                };

                let number = match event {
                    TraceEvent::Exit { process, .. } => self.endings.get(process).copied(),
                    TraceEvent::ThreadExit { thread, .. } => {
                        self.thread_endings.get(thread).copied()
                    }
                    _ => None,
                };

                let mut line = self.leader(thread, process, event.time(), number);
                line.0.extend(tokens.0);
                let text = self.paint(&line);
                self.write_thread(process, thread, &text);

                if let TraceEvent::Signal {
                    stack: Some(stack), ..
                } = event
                {
                    for line in self.stack_lines(process, stack) {
                        self.write_thread(process, thread, &line);
                    }
                }
            }
            OutputFormat::Ndjson => {
                if let Some(object) = self.other_json(event) {
                    self.json(process, object);
                }
            }
        }
    }

    /// A signal, exit, or loss as ndjson writes it.
    fn other_json(&self, event: &TraceEvent) -> Option<Map<String, Value>> {
        Some(match *event {
            TraceEvent::Signal {
                thread,
                process,
                time,
                signal,
                code,
                sender,
                ..
            } => {
                let mut object = self.base("signal", time, Some(thread), process);
                object.insert("signal".into(), present::signal(signal).into());
                if let Some(code) = code {
                    object.insert("code".into(), code.into());
                }

                if let Some(sender) = sender {
                    object.insert("sender".into(), sender.get().into());
                }

                object
            }

            TraceEvent::ThreadExit {
                thread,
                process,
                time,
                end,
            } => {
                let mut object = self.base("thread_exit", time, Some(thread), process);
                insert_end(&mut object, end);
                object
            }
            TraceEvent::Exit { process, time, end } => {
                let mut object = self.base("exit", time, None, process);
                insert_end(&mut object, end);
                object
            }
            TraceEvent::Lost { time, count } => {
                let mut object = self.base("lost", time, None, NO_PROCESS);
                object.remove("pid");
                object.insert("count".into(), count.into());
                object
            }
            _ => return None,
        })
    }

    /// Prints the table and flushes every output.
    fn finish(&mut self) -> Result<()> {
        if let Some(mut continuous) = self.continuous.take() {
            let path = continuous.end_segment(monotonic()).with_context(|| {
                format!("writing a segment to {}", continuous.directory().display())
            })?;
            if self.quiet < 2 {
                self.message(&format!("recorded the last segment to {}", path.display()));
            }
        }

        if self.summarize {
            let errno = |number: u16| present::errno(microscope::syscalls::Errno(number)).0;

            match self.format {
                OutputFormat::Text => {
                    let lines =
                        summary::table(&self.stats, &self.columns, self.sort, self.unit, &errno);
                    let mut out = self.output_for_summary();
                    for line in lines {
                        writeln!(out, "{line}")?;
                    }
                }
                OutputFormat::Ndjson => {
                    let rows: Vec<Value> = self
                        .stats
                        .iter()
                        .map(|(id, stats)| {
                            let errnos: Map<String, Value> = stats
                                .errors_by_errno
                                .iter()
                                .map(|(&number, &count)| (errno(number), count.into()))
                                .collect();
                            json!({
                                "name": id.display_name(),
                                "calls": stats.calls,
                                "errors": stats.errors,
                                "total_ns": stats.total_ns,
                                "min_ns": stats.min_ns,
                                "max_ns": stats.max_ns,
                                "p50_ns": stats.quantile_ns(0.5),
                                "p90_ns": stats.quantile_ns(0.9),
                                "p99_ns": stats.quantile_ns(0.99),
                                "bytes": stats.bytes,
                                "errnos": errnos,
                            })
                        })
                        .collect();
                    let line = json!({ "type": "summary", "calls": rows }).to_string();
                    writeln!(self.output_for_summary(), "{line}")?;
                }
            }
        }

        self.output.flush()?;
        if let Some((path, capture)) = self.capture.take() {
            capture
                .finish()
                .write(&path)
                .with_context(|| format!("writing {}", path.display()))?;
        }

        if let Some((_, outputs)) = &mut self.per_process {
            for output in outputs.values_mut() {
                output.flush()?;
            }
        }

        match self.error.take() {
            Some(error) if error.kind() != io::ErrorKind::BrokenPipe => {
                Err(error).context("writing the trace")
            }
            _ => Ok(()),
        }
    }

    /// strace writes -c's table where the trace goes, or to stderr with
    /// -ff; a capture holds its own.
    fn output_for_summary(&mut self) -> Box<dyn Write + '_> {
        if self.per_process.is_some() || self.capture.is_some() {
            Box::new(io::stderr())
        } else {
            Box::new(&mut self.output)
        }
    }

    fn note_thread(&mut self, thread: Tid, process: Pid) {
        self.threads.insert(thread);
        if self.comm
            && !self.names.contains_key(&process)
            && let Ok(name) = std::fs::read_to_string(format!("/proc/{process}/comm"))
        {
            self.names.insert(process, name.trim_end().to_owned());
        }
    }
}

impl Sink for Printer {
    fn event(&mut self, event: TraceEvent) {
        if let Some((_, capture)) = &mut self.capture
            && !matches!(event, TraceEvent::Call(_))
        {
            capture.event(&event);
        }

        if !matches!(event, TraceEvent::Call(_)) {
            self.continue_with(&event);
        }

        match &event {
            TraceEvent::Call(call) => {
                if self.processes.is_empty() {
                    self.processes.insert(call.process);
                }

                self.note_thread(call.thread, call.process);
                self.call(call);
                return;
            }

            TraceEvent::Spawn {
                child,
                parent,
                parent_thread,
                thread,
                ..
            } => {
                self.threads.insert(*parent_thread);
                self.threads.insert(*child);
                if !*thread && let Some(name) = self.names.get(parent).cloned() {
                    self.names.insert(Pid::led_by(*child), name);
                }

                // strace calls every thread a process.
                if self.follow && self.quiet == 0 {
                    self.message(&format!("Process {child} attached"));
                }
            }
            TraceEvent::Exec { process, path, .. } => {
                if self.processes.is_empty() {
                    self.processes.insert(*process);
                }

                let name: String = path
                    .rsplit('/')
                    .next()
                    .unwrap_or_default()
                    .chars()
                    .take(15)
                    .collect();
                self.names.insert(*process, name);
                if self.fds.of(*process).next().is_none() {
                    let mut seeds = Seeds::default();
                    ptrace::seed_descriptors(process.get(), &mut seeds);

                    for seed in seeds.0 {
                        if let Some((_, capture)) = &mut self.capture {
                            capture.event(&seed);
                        }

                        self.fds.apply(&seed);
                    }
                }
            }
            TraceEvent::Exit { process, .. } => {
                self.ended.insert(*process);
            }

            TraceEvent::Signal {
                thread, process, ..
            } => self.note_thread(*thread, *process),
            TraceEvent::ThreadExit { .. }
            | TraceEvent::Lost { .. }
            | TraceEvent::Descriptor { .. } => {}
        }

        self.other(&event);
        self.fds.apply(&event);
    }

    fn in_flight(&mut self, calls: &[InFlight]) {
        let Some(after) = self.in_flight else {
            return;
        };

        let now = monotonic();
        for call in calls {
            if !self.reported.insert((call.thread, call.entered)) {
                continue;
            }

            let event = SyscallEvent {
                thread: call.thread,
                process: call.process,
                call: call.call,
                args: call.args,
                captured: Vec::new(),
                outcome: Outcome::Unfinished,
                entered: Some(call.entered),
                exited: None,
                stack: None,
                flags: microscope::syscalls::EventFlags::default(),
            };

            if !self.filter.calls.contains(call.call) || !self.show_calls {
                continue;
            }

            let blocked = now.saturating_sub(call.entered).max(after);
            match self.format {
                OutputFormat::Text => {
                    let mut line = self.leader(
                        call.thread,
                        call.process,
                        call.entered,
                        Some(call.call.number),
                    );
                    let entry = present::entry(
                        &event,
                        &self.style,
                        Context {
                            fds: Some(&self.fds),
                            before: false,
                        },
                    );
                    line.0.extend(entry.0);
                    line.push(
                        Kind::Comment,
                        format!(" … in flight {}", present::duration(blocked)),
                    );
                    let text = self.paint(&line);
                    self.write(call.process, &text);
                }
                OutputFormat::Ndjson => {
                    let mut object =
                        self.base("in_flight", call.entered, Some(call.thread), call.process);
                    object.insert("name".into(), call.call.display_name().into());
                    object.insert("blocked_ns".into(), blocked.into());
                    let entry = present::entry(
                        &event,
                        &self.style,
                        Context {
                            fds: Some(&self.fds),
                            before: false,
                        },
                    );
                    object.insert("text".into(), entry.text().into());
                    self.json(call.process, object);
                }
            }
        }

        // Forget calls that have returned.
        let live: BTreeSet<(Tid, u64)> = calls
            .iter()
            .map(|call| (call.thread, call.entered))
            .collect();
        self.reported.retain(|key| live.contains(key));
    }

    fn in_flight_after(&self) -> Option<u64> {
        self.in_flight
    }

    fn tick(&mut self, time: u64) {
        if self.trigger.swap(false, Ordering::Relaxed)
            && let Some(continuous) = &mut self.continuous
        {
            let saved = continuous.trigger("SIGUSR2", time);
            self.saved(saved);
        }
    }

    fn segment_due(&mut self, time: u64) -> bool {
        self.continuous
            .as_ref()
            .is_some_and(|continuous| continuous.segment_due(time))
    }

    fn end_segment(&mut self, time: u64) {
        if let Some(continuous) = &mut self.continuous
            && let Err(error) = continuous.end_segment(time)
        {
            let directory = continuous.directory().display().to_string();
            self.message(&format!("writing a segment to {directory} failed: {error}"));
        }
    }

    fn budget(&mut self, _time: u64, note: &str) {
        self.budget_notes += 1;
        self.setting(&format!("budget {}", self.budget_notes), note);

        // What the trace leaves out from here, said even beside a file's
        // trace; -qq silences it.
        if self.quiet < 2 {
            self.message(&format!("budget: {note}"));
        }
    }

    fn attached_by(&mut self, attachment: &Attachment) {
        self.setting("attach", attachment.name());
        self.setting("attach reason", &attachment.reason);

        // What tracing costs everyone else, which only tracepoints charge,
        // said even beside a file's trace; -qq silences it.
        if attachment.functions.is_empty() && self.quiet < 2 {
            self.message(&format!(
                "tracing through tp_btf ({}), which costs every other process on this host \
                 about 33 ns a system call; a list of at most 32 calls attaches to their \
                 functions instead and costs them nothing",
                attachment.reason
            ));
        }

        if let Some(cgroup) = &self.cgroup
            && self.quiet == 0
        {
            self.message(&format!("Tracing cgroup {}", cgroup.display()));
        }
    }

    fn attached(&mut self, process: u32) {
        if self.quiet == 0 {
            self.message(&format!("Process {process} attached"));
        }

        ptrace::seed_descriptors(process, self);
    }

    fn counted(&mut self, mut stats: Folded) {
        stats.by_call.retain(|&(process, call), _| {
            self.filter.calls.contains(call)
                && (self.follow || self.processes.is_empty() || self.processes.contains(&process))
        });

        for (&(_, call), stats) in &stats.by_call {
            self.stats.entry(call).or_default().merge(stats);
        }

        if let Some(continuous) = &mut self.continuous {
            continuous.counted(&stats);
        }

        if let Some((_, capture)) = &mut self.capture {
            capture.counted(&stats);
        }
    }
}

/// The BPF engine's ring size from `--ring`: bytes, with K or M, a power
/// of two of whole pages.
fn ring_bytes(text: &str) -> Result<u32> {
    let scale = match text.chars().last() {
        Some('K' | 'k') => 1 << 10,
        Some('M' | 'm') => 1 << 20,
        _ => 1,
    };

    let number = text.trim_end_matches(['K', 'k', 'M', 'm']);
    let bytes = number
        .parse::<u32>()
        .ok()
        .and_then(|number| number.checked_mul(scale))
        .with_context(|| format!("--ring takes bytes, as 16M or 512K, not {text}"))?;
    if !bytes.is_power_of_two() || bytes < 4096 {
        bail!("--ring must be a power of two of at least 4K, not {text}");
    }

    Ok(bytes)
}

/// Collects the descriptors a process holds as it begins.
#[derive(Default)]
struct Seeds(Vec<TraceEvent>);

impl Sink for Seeds {
    fn event(&mut self, event: TraceEvent) {
        self.0.push(event);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_call_record_is_the_json_serde_writes() {
        let tricky = "q\"b\\n\nr\rt\tb\u{8}f\u{c}c\u{1}d\u{1f}e\u{7f}é😀";
        let record = CallRecord {
            abi: Some("32"),
            args: vec![("buf".into(), tricky.into()), ("fd".into(), "3".into())],
            duration_ns: Some(1200),
            errno: Some("ENOENT".into()),
            name: "openat".into(),
            number: 257,
            path: Some(tricky.into()),
            pid: 10,
            process_exited: true,
            ret: Some(-2),
            stack: Some(json!([{"module": tricky}]).to_string()),
            text: tricky.into(),
            tid: 11,
            time_ns: -5,
            unavailable: true,
            unfinished: true,
        };

        let mut written = Vec::new();
        record.write(&mut written);
        let expected = json!({
            "abi": "32",
            "args": {"buf": tricky, "fd": "3"},
            "duration_ns": 1200,
            "errno": "ENOENT",
            "name": "openat",
            "number": 257,
            "path": tricky,
            "pid": 10,
            "process_exited": true,
            "return": -2,
            "stack": [{"module": tricky}],
            "text": tricky,
            "tid": 11,
            "time_ns": -5,
            "type": "call",
            "unavailable": true,
            "unfinished": true,
        });
        assert_eq!(String::from_utf8(written).ok(), Some(expected.to_string()));
        let bare = CallRecord {
            abi: None,
            args: Vec::new(),
            duration_ns: None,
            errno: None,
            name: "getppid".into(),
            number: 110,
            path: None,
            pid: 1,
            process_exited: false,
            ret: None,
            stack: None,
            text: "getppid()".into(),
            tid: 1,
            time_ns: 0,
            unavailable: false,
            unfinished: false,
        };

        let mut written = Vec::new();
        bare.write(&mut written);
        let expected = json!({
            "args": {}, "name": "getppid", "number": 110, "pid": 1,
            "text": "getppid()", "tid": 1, "time_ns": 0, "type": "call",
        });
        assert_eq!(String::from_utf8(written).ok(), Some(expected.to_string()));
        for value in [0, 9, 10, 99, 100, 101, -7, -1000, i64::MIN, i64::MAX] {
            let mut written = Vec::new();
            json_number(&mut written, b"", value);
            assert_eq!(written, format!("{value},").into_bytes());
        }
    }

    #[test]
    fn durations_and_pids_parse_as_strace_writes_them() {
        assert_eq!(nanoseconds("10ms").ok(), Some(10_000_000));
        assert_eq!(nanoseconds("2us").ok(), Some(2_000));
        assert_eq!(nanoseconds("1.5").ok(), Some(1_500_000_000));
        assert!(nanoseconds("soon").is_err());
        assert_eq!(
            pids(&["1,2".to_owned(), "3 2".to_owned()]).ok(),
            Some(vec![1, 2, 3])
        );
        assert!(pids(&["0".to_owned()]).is_err());
    }

    /// The zone is the one in force now, not when the machine booted: a
    /// clock change since then would shift every time by an hour.
    #[test]
    fn the_zone_is_taken_now_and_not_at_boot() {
        const SECOND: i128 = 1_000_000_000;
        let now = 1_700_000_000;
        let up = 90 * 24 * 3600;
        let wall = i128::from(now) * SECOND + 5;
        let (realtime, at) = clocks(wall, u64::try_from(i128::from(up) * SECOND).unwrap());
        assert_eq!(realtime, i128::from(now - up) * SECOND + 5);
        assert_eq!(at, now);
    }
}
