use std::io::{BufRead as _, BufReader, Read as _};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

use anyhow::{Context as _, Result, bail};
use serde::{Deserialize, Serialize};

use super::overhead::{Placement, median};

pub const SETUPS: [&str; 4] = ["bare", "counts", "calls", "untargeted"];

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Run {
    pub setup: String,
    pub ns_per_call: f64,
}

#[derive(Debug, Clone)]
pub struct Options {
    pub programs: PathBuf,

    pub microscope: PathBuf,

    pub launcher: PathBuf,

    pub runs: usize,

    pub calls: u64,
}

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
    let calls = options.calls.max(1).to_string();

    let mut looping = Command::new(taskset);
    looping
        .arg("--cpu-list")
        .arg(placement.program_list())
        .arg(&burst)
        .arg(&calls);

    let recorded = |preset: &str, level: &str| {
        let mut command = Command::new(&options.launcher);
        command
            .arg(&options.microscope)
            .args(["record", "--preset", preset, "--syscalls", level])
            .args(["--syscall-filter", "trace=getppid", "-o", "/dev/null", "--"])
            .arg(taskset)
            .arg("--cpu-list")
            .arg(placement.program_list())
            .arg(&burst)
            .arg(&calls);
        command
    };

    match setup {
        "bare" => timed(looping),
        "counts" => timed(recorded("prod", "counts")),
        "calls" => timed(recorded("dev", "calls")),
        "untargeted" => {
            let mut other = Tracing::start(options)?;
            let measured = timed(looping);
            other.finish()?;
            measured
        }
        _ => bail!("no setup is named {setup}"),
    }
}

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

struct Tracing {
    child: Child,
}

impl Tracing {
    fn start(options: &Options) -> Result<Self> {
        let shell = which("sh")?;
        let mut child = Command::new(&options.launcher)
            .arg(&options.microscope)
            .args(["record", "--syscalls", "calls", "-o", "/dev/null", "--"])
            .arg(shell)
            .args(["-c", "echo ready; read line || true"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .context("recording another program")?;

        let stdout = child.stdout.take().context("the traced program's output")?;
        let mut line = String::new();
        BufReader::new(stdout).read_line(&mut line)?;
        if line.trim() != "ready" {
            let _ = child.kill();
            let _ = child.wait();
            bail!("the program recorded beside the loop never started");
        }

        Ok(Self { child })
    }

    fn finish(&mut self) -> Result<()> {
        drop(self.child.stdin.take());
        let status = self.child.wait()?;
        if !status.success() {
            let mut error = String::new();
            if let Some(mut stderr) = self.child.stderr.take() {
                stderr.read_to_string(&mut error)?;
            }

            bail!("the recording beside the loop ended with {status}: {error}");
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

fn which(name: &str) -> Result<PathBuf> {
    std::env::var_os("PATH")
        .iter()
        .flat_map(std::env::split_paths)
        .map(|directory| directory.join(name))
        .find(|path| path.is_file())
        .with_context(|| format!("{name} is not on PATH"))
}

#[derive(Debug, Clone, PartialEq)]
pub struct Summary {
    pub setup: String,
    pub ns_per_call: f64,

    pub added_ns: f64,

    pub noise_ns: f64,
}

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
