//! What tracing system calls costs a program, measured on a getppid loop.
//!
//! The loop times its own calls and prints the nanoseconds each took, bare
//! and under each engine, and, as the tax every other process pays while
//! the BPF engine's tracepoints are attached, untraced beside a trace of
//! another program. Setups are interleaved so that drift in the machine
//! falls on every one alike. The loop runs on cores of its own, and microscope
//! on others that share their last-level cache, as `overhead` places them.

use std::io::{BufRead as _, BufReader, Read as _};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

use anyhow::{Context as _, Result, bail};
use serde::{Deserialize, Serialize};

use super::overhead::{Placement, median};

/// The ways the loop runs: bare, traced by each engine, and untraced while
/// the BPF engine traces another program.
pub const SETUPS: [&str; 5] = ["bare", "ptrace", "bpf-count", "bpf-stream", "untargeted"];

/// One run of the loop under one setup.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Run {
    pub setup: String,
    pub ns_per_call: f64,
}

/// What to measure.
#[derive(Debug, Clone)]
pub struct Options {
    /// Where the built fixtures are.
    pub programs: PathBuf,

    /// The microscope executable traced runs use.
    pub microscope: PathBuf,

    /// Starts microscope with `CAP_BPF` and `CAP_PERFMON`: the `microscope-bpf`
    /// wrapper, given the executable and its arguments.
    pub launcher: PathBuf,

    /// Runs of each setup.
    pub runs: usize,

    /// Calls each run of the loop makes; a hundredth of them under ptrace,
    /// which takes microseconds a call.
    pub calls: u64,
}

/// Runs every setup `options.runs` times, telling `progress` of each run as
/// it ends.
pub fn measure(options: &Options, mut progress: impl FnMut(&Run)) -> Result<Vec<Run>> {
    if !options.launcher.exists() {
        bail!(
            "{} is missing. The BPF engine needs CAP_BPF and CAP_PERFMON: import microscope's \
             nixosModules.microscope-bpf and rebuild, or pass a launcher that grants them with \
             --launcher",
            options.launcher.display()
        );
    }

    let placement = Placement::new()?;
    placement.microscope()?;
    let taskset = which("taskset")?;
    let mut runs = Vec::new();

    for _ in 0..options.runs.max(1) {
        for setup in SETUPS {
            let ns_per_call = run(setup, options, &placement, &taskset)
                .with_context(|| format!("the loop under {setup}"))?;
            let run = Run {
                setup: setup.to_owned(),
                ns_per_call,
            };

            progress(&run);
            runs.push(run);
        }
    }

    Ok(runs)
}

fn run(setup: &str, options: &Options, placement: &Placement, taskset: &Path) -> Result<f64> {
    let burst = options.programs.join("syscall-burst");
    let calls = if setup == "ptrace" {
        options.calls / 100
    } else {
        options.calls
    }
    .max(1)
    .to_string();

    // The loop runs on the program's CPUs, wherever microscope does.
    let mut looping = Command::new(taskset);
    looping
        .arg("--cpu-list")
        .arg(placement.program_list())
        .arg(&burst)
        .arg(&calls);

    let traced = |engine: &[&str]| {
        let mut command = Command::new(&options.launcher);
        command
            .arg(&options.microscope)
            .arg("syscalls")
            .args(engine)
            .args(["-e", "trace=getppid", "-o", "/dev/null", "--"])
            .arg(taskset)
            .arg("--cpu-list")
            .arg(placement.program_list())
            .arg(&burst)
            .arg(&calls);
        command
    };

    match setup {
        "bare" => timed(looping),
        // A tracer and its tracee hand the CPU back and forth at every
        // stop, so they share the program's CPUs, as the scheduler would
        // place them: on separate cores, each stop wakes an idle one.
        "ptrace" => {
            let mut command = Command::new(taskset);
            command
                .arg("--cpu-list")
                .arg(placement.program_list())
                .arg(&options.microscope)
                .args(["syscalls", "--engine", "ptrace", "-c"])
                .args(["-e", "trace=getppid", "-o", "/dev/null", "--"])
                .arg(&burst)
                .arg(&calls);
            timed(command)
        }
        "bpf-count" => timed(traced(&["--engine", "bpf", "-c"])),
        "bpf-stream" => timed(traced(&["--engine", "bpf"])),
        "untargeted" => {
            let mut other = Tracing::start(options)?;
            let measured = timed(looping);
            other.finish()?;
            measured
        }
        _ => bail!("no setup is named {setup}"),
    }
}

/// Runs the loop, or microscope tracing it, and reads the nanoseconds a call
/// the loop printed.
fn timed(mut command: Command) -> Result<f64> {
    let output = command
        .stdin(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .with_context(|| format!("starting {command:?}"))?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    if !output.status.success() {
        bail!(
            "{command:?} failed with {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        );
    }

    stdout
        .lines()
        .find_map(|line| line.split_once(' ')?.1.trim().parse::<f64>().ok())
        .with_context(|| format!("the loop printed no time: {stdout:?}"))
}

/// The BPF engine tracing a program that waits on its input, so that its
/// tracepoints stay attached while the loop runs untraced.
struct Tracing {
    child: Child,
}

impl Tracing {
    fn start(options: &Options) -> Result<Self> {
        let shell = which("sh")?;
        let mut child = Command::new(&options.launcher)
            .arg(&options.microscope)
            .args(["syscalls", "-o", "/dev/null", "--"])
            .arg(shell)
            .args(["-c", "echo ready; read line || true"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .context("starting the BPF engine on another program")?;

        // The program runs only once the engine's programs are attached.
        let stdout = child.stdout.take().context("the traced program's output")?;
        let mut line = String::new();
        BufReader::new(stdout).read_line(&mut line)?;
        if line.trim() != "ready" {
            let _ = child.kill();
            let _ = child.wait();
            bail!("the program the BPF engine traced never started");
        }

        Ok(Self { child })
    }

    /// Ends the traced program by closing its input, and waits for microscope.
    fn finish(&mut self) -> Result<()> {
        drop(self.child.stdin.take());
        let status = self.child.wait()?;
        if !status.success() {
            let mut error = String::new();
            if let Some(mut stderr) = self.child.stderr.take() {
                stderr.read_to_string(&mut error)?;
            }

            bail!("the BPF engine ended with {status}: {error}");
        }

        Ok(())
    }
}

impl Drop for Tracing {
    fn drop(&mut self) {
        drop(self.child.stdin.take());
        let _ = self.child.wait();
    }
}

/// The program named `name` on `PATH`.
fn which(name: &str) -> Result<PathBuf> {
    std::env::var_os("PATH")
        .iter()
        .flat_map(std::env::split_paths)
        .map(|directory| directory.join(name))
        .find(|path| path.is_file())
        .with_context(|| format!("{name} is not on PATH"))
}

/// One setup, over its runs.
#[derive(Debug, Clone, PartialEq)]
pub struct Summary {
    pub setup: String,
    pub ns_per_call: f64,

    /// Nanoseconds a call more than bare.
    pub added_ns: f64,

    /// Bare's spread, the range of its runs, in nanoseconds: the noise a
    /// cost is read against.
    pub noise_ns: f64,
}

/// Each setup by the medians of its runs.
#[must_use]
pub fn summarize(runs: &[Run]) -> Vec<Summary> {
    fn of<'a>(runs: &'a [Run], setup: &'a str) -> impl Iterator<Item = f64> + 'a {
        runs.iter()
            .filter(move |run| run.setup == setup)
            .map(|run| run.ns_per_call)
    }

    let unhindered: Vec<f64> = of(runs, "bare").collect();
    let Some(base) = median(unhindered.iter().copied()) else {
        return Vec::new();
    };

    let noise = unhindered.iter().copied().fold(f64::NAN, f64::max)
        - unhindered.iter().copied().fold(f64::NAN, f64::min);

    SETUPS
        .iter()
        .filter_map(|&setup| {
            let ns_per_call = median(of(runs, setup))?;
            Some(Summary {
                setup: setup.to_owned(),
                ns_per_call,
                added_ns: ns_per_call - base,
                noise_ns: noise,
            })
        })
        .collect()
}
