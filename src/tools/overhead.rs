use std::io::{BufRead as _, BufReader, Write as _};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result};
use nix::sched::{CpuSet, sched_setaffinity};
use nix::unistd::Pid;
use serde::{Deserialize, Serialize};

use crate::profiler::{Preset, Profile, PythonEngine, RecordOptions, Watch, record_watched};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Measure {
    Lines,

    Server,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Program {
    Built(&'static str),

    Python {
        interpreter: &'static str,
        script: &'static str,
    },
}

#[derive(Debug, Clone, Copy)]
pub struct Workload {
    pub name: &'static str,
    program: Program,
    arguments: &'static [&'static str],
    measure: Measure,
}

pub const WORKLOADS: [Workload; 5] = [
    Workload {
        name: "c",
        program: Program::Built("overhead"),
        arguments: &["4"],
        measure: Measure::Lines,
    },
    Workload {
        name: "go",
        program: Program::Built("overhead-go"),
        arguments: &[],
        measure: Measure::Lines,
    },
    Workload {
        name: "tokio",
        program: Program::Built("tokio-server-o3"),
        arguments: &[],
        measure: Measure::Server,
    },
    Workload {
        name: "probes",
        program: Program::Built("probes"),
        arguments: &["rate"],
        measure: Measure::Lines,
    },
    Workload {
        name: "python",
        program: Program::Python {
            interpreter: "3.14",
            script: "overhead.py",
        },
        arguments: &[],
        measure: Measure::Lines,
    },
];

pub const SETUPS: [(&str, Option<Preset>); 3] = [
    ("bare", None),
    ("prod", Some(Preset::Production)),
    ("dev", Some(Preset::Development)),
];

pub const PYTHON_SETUPS: [(&str, PythonEngine); 3] = [
    (
        "prod+live",
        PythonEngine::Live {
            every: Duration::from_millis(10),
        },
    ),
    (
        "prod+snapshot",
        PythonEngine::Snapshot {
            every: Duration::from_millis(100),
        },
    ),
    ("prod+bpf", PythonEngine::Bpf),
];

fn recorded_with(workload: &Workload, setup: &str) -> Option<RecordOptions> {
    if let Some((_, preset)) = SETUPS.iter().find(|(name, _)| *name == setup) {
        return preset.map(RecordOptions::preset);
    }

    let engine = PYTHON_SETUPS
        .iter()
        .find(|(name, _)| *name == setup)
        .map(|(_, engine)| *engine)
        .filter(|_| matches!(workload.program, Program::Python { .. }))?;
    Some(RecordOptions {
        python: Some(engine),
        ..RecordOptions::preset(Preset::Production)
    })
}

fn setups(workload: &Workload, privileged: bool) -> Vec<&'static str> {
    let mut setups: Vec<&str> = SETUPS.iter().map(|(name, _)| *name).collect();
    if matches!(workload.program, Program::Python { .. }) {
        setups.extend(
            PYTHON_SETUPS
                .iter()
                .filter(|(_, engine)| privileged || *engine != PythonEngine::Bpf)
                .map(|(name, _)| *name),
        );
    }

    setups
}

fn privileged() -> bool {
    crate::backend::syscall_engines::bpf::Probe::read()
        .lacks_privileges()
        .is_none()
}

const CLIENTS: usize = 4;
const BATCH: usize = 32;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Run {
    pub workload: String,
    pub setup: String,

    pub rate: f64,

    pub p99_ns: Option<u64>,

    pub recorder_cpu: Option<f64>,

    pub recording_bytes: Option<u64>,
}

#[derive(Debug, Clone)]
pub struct Options {
    pub programs: PathBuf,

    pub runs: usize,

    pub warmup: Duration,

    pub window: Duration,

    pub only: Vec<String>,
}

pub fn measure(options: &Options, mut progress: impl FnMut(&Run)) -> Result<Vec<Run>> {
    let workloads: Vec<&Workload> = WORKLOADS
        .iter()
        .filter(|workload| {
            options.only.is_empty() || options.only.iter().any(|only| only == workload.name)
        })
        .collect();
    let placement = Placement::new()?;
    placement.microscope()?;
    let privileged = privileged();
    let mut runs = Vec::new();

    for _ in 0..options.runs.max(1) {
        for workload in &workloads {
            for setup in setups(workload, privileged) {
                let run = run(workload, setup, options, &placement)
                    .with_context(|| format!("{} under {setup}", workload.name))?;
                progress(&run);
                runs.push(run);
            }
        }
    }

    Ok(runs)
}

struct Measured {
    rate: f64,
    p99_ns: Option<u64>,
}

pub(super) struct Placement {
    program: CpuSet,
    microscope: CpuSet,
}

const PROGRAM_CORES: usize = 4;

impl Placement {
    pub(super) fn new() -> Result<Self> {
        let mut cores = Vec::new();
        for cpu in 0..CpuSet::count() {
            let siblings =
                format!("/sys/devices/system/cpu/cpu{cpu}/topology/thread_siblings_list");
            let Ok(siblings) = std::fs::read_to_string(siblings) else {
                continue;
            };

            let first = siblings
                .trim()
                .split([',', '-'])
                .next()
                .and_then(|first| first.parse().ok());
            if first == Some(cpu) {
                cores.push(cpu);
            }
        }

        anyhow::ensure!(
            cores.len() > PROGRAM_CORES,
            "the machine has {} cores, and the workloads need {} and microscope one",
            cores.len(),
            PROGRAM_CORES
        );
        let (program_cores, rest) = cores.split_at(PROGRAM_CORES);
        let first = program_cores
            .first()
            .context("the workloads need at least one core")?;
        let cache = cpu_list(&format!(
            "/sys/devices/system/cpu/cpu{first}/cache/index3/shared_cpu_list"
        ));
        let near: Vec<usize> = rest
            .iter()
            .copied()
            .filter(|cpu| cache.contains(cpu))
            .collect();
        let mut program = CpuSet::new();
        let mut microscope = CpuSet::new();

        for &cpu in program_cores {
            program.set(cpu)?;
        }

        for &cpu in if near.is_empty() { rest } else { &near } {
            microscope.set(cpu)?;
        }

        Ok(Self {
            program,
            microscope,
        })
    }

    fn program(&self) -> Result<()> {
        Ok(sched_setaffinity(Pid::from_raw(0), &self.program)?)
    }

    pub(super) fn program_list(&self) -> String {
        (0..CpuSet::count())
            .filter(|&cpu| self.program.is_set(cpu).unwrap_or(false))
            .map(|cpu| cpu.to_string())
            .collect::<Vec<_>>()
            .join(",")
    }

    pub(super) fn microscope(&self) -> Result<()> {
        Ok(sched_setaffinity(Pid::from_raw(0), &self.microscope)?)
    }
}

fn cpu_list(path: &str) -> Vec<usize> {
    let text = std::fs::read_to_string(path).unwrap_or_default();
    let mut cpus = Vec::new();
    for part in text.trim().split(',').filter(|part| !part.is_empty()) {
        let (first, last) = part.split_once('-').unwrap_or((part, part));
        if let (Ok(first), Ok(last)) = (first.parse::<usize>(), last.parse::<usize>()) {
            cpus.extend(first..=last);
        }
    }

    cpus
}

fn run(workload: &Workload, setup: &str, options: &Options, placement: &Placement) -> Result<Run> {
    let (reader, writer) = std::io::pipe()?;
    let mut command = match workload.program {
        Program::Built(program) => Command::new(options.programs.join(program)),
        Program::Python {
            interpreter,
            script,
        } => {
            let root = std::env::var_os("MICROSCOPE_CPYTHON")
                .context("MICROSCOPE_CPYTHON names the interpreters; run in the dev shell")?;
            let mut command = Command::new(PathBuf::from(root).join(interpreter).join("python"));
            command.args(["-I", "-B"]).arg(
                PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                    .join("tests/fixtures/python")
                    .join(script),
            );
            command
        }
    };

    let recorded = recorded_with(workload, setup);
    command
        .args(workload.arguments)
        .stdin(Stdio::null())
        .stdout(writer)
        .stderr(Stdio::null());
    let stop = AtomicBool::new(false);
    let (started, on) = mpsc::channel();
    let resident = recorded.as_ref().and_then(|_| reset_peak_rss());

    std::thread::scope(|scope| {
        let (bare, recording) = match recorded {
            None => {
                placement.program()?;
                let child = command.spawn().context("starting the program");
                placement.microscope()?;
                let child = child?;
                drop(command);
                started.send(()).ok();
                (Some(child), None)
            }
            Some(recorded) => {
                let stop = &stop;
                let recording = scope.spawn(move || {
                    let mut sent = false;

                    placement.program()?;
                    Ok::<_, anyhow::Error>(record_watched(
                        command,
                        &recorded,
                        &mut Watch {
                            every: Duration::MAX,
                            publish: &mut |_| {
                                if !std::mem::replace(&mut sent, true) {
                                    placement.microscope().ok();
                                    started.send(()).ok();
                                }
                            },
                            stop,
                            save: None,
                        },
                    )?)
                });
                (None, Some(recording))
            }
        };

        on.recv_timeout(Duration::from_mins(1))
            .context("the program never started")?;
        let reader = BufReader::new(reader);
        let measured = match workload.measure {
            Measure::Lines => lines(reader, options),
            Measure::Server => server(reader, options),
        };

        if let Some(mut child) = bare {
            if workload.measure == Measure::Lines {
                child.kill().ok();
            }

            child.wait()?;
        }

        let mut run = Run {
            workload: workload.name.to_owned(),
            setup: setup.to_owned(),
            rate: 0.0,
            p99_ns: None,
            recorder_cpu: None,
            recording_bytes: None,
        };

        if let Some(recording) = recording {
            stop.store(true, Ordering::Relaxed);
            let recording = recording
                .join()
                .map_err(|_| anyhow::anyhow!("recording panicked"))??;
            run.recorder_cpu = Some(recorder_share(&recording.profile));
            run.recording_bytes = resident
                .zip(memory("VmHWM:"))
                .map(|(resident, peak)| peak.saturating_sub(resident));
        }

        let measured = measured?;
        run.rate = measured.rate;
        run.p99_ns = measured.p99_ns;
        Ok(run)
    })
}

fn lines(mut reader: BufReader<std::io::PipeReader>, options: &Options) -> Result<Measured> {
    let begin = Instant::now() + options.warmup;
    let end = begin + options.window;
    let mut counted = 0_u32;
    let mut line = String::new();

    loop {
        line.clear();
        anyhow::ensure!(
            reader.read_line(&mut line)? > 0,
            "the program stopped writing"
        );
        let now = Instant::now();
        if now >= end {
            break;
        }

        counted += u32::from(now >= begin);
    }

    Ok(Measured {
        rate: f64::from(counted) / options.window.as_secs_f64(),
        p99_ns: None,
    })
}

fn server(mut reader: BufReader<std::io::PipeReader>, options: &Options) -> Result<Measured> {
    let mut ready = String::new();
    reader.read_line(&mut ready)?;

    let address = ready
        .trim()
        .strip_prefix("READY ")
        .with_context(|| format!("the server said {ready:?}"))?
        .to_owned();
    let begin = Instant::now() + options.warmup;
    let end = begin + options.window;
    let latencies = std::thread::scope(|scope| {
        let clients: Vec<_> = (0..CLIENTS)
            .map(|_| scope.spawn(|| client(&address, begin, end)))
            .collect();
        let mut latencies = Vec::new();
        for client in clients {
            latencies.extend(
                client
                    .join()
                    .map_err(|_| anyhow::anyhow!("a client panicked"))??,
            );
        }

        Ok::<_, anyhow::Error>(latencies)
    })?;
    std::net::TcpStream::connect(&address)?.write_all(b"quit\n")?;

    let mut sorted = latencies;
    sorted.sort_unstable();

    let p99 = sorted
        .get(sorted.len().saturating_sub(1) * 99 / 100)
        .copied();

    #[expect(clippy::cast_precision_loss, reason = "a count of requests")]
    let rate = (sorted.len() * BATCH) as f64 / options.window.as_secs_f64();

    Ok(Measured {
        rate,
        p99_ns: p99.map(|latency| u64::try_from(latency.as_nanos()).unwrap_or(u64::MAX)),
    })
}

fn client(address: &str, begin: Instant, end: Instant) -> Result<Vec<Duration>> {
    let stream = std::net::TcpStream::connect(address)?;
    stream.set_nodelay(true)?;
    let mut writer = stream.try_clone()?;
    let mut reader = BufReader::new(stream);
    let mut latencies = Vec::new();
    let mut answer = String::new();

    loop {
        let asked = Instant::now();
        if asked >= end {
            return Ok(latencies);
        }

        writer.write_all(&b"ask\n".repeat(BATCH))?;

        for _ in 0..BATCH {
            answer.clear();
            anyhow::ensure!(reader.read_line(&mut answer)? > 0, "the server hung up");
        }

        if asked >= begin {
            latencies.push(asked.elapsed());
        }
    }
}

fn recorder_share(profile: &Profile) -> f64 {
    let (cpu, loading) = profile.recorder_cpu();
    let (start, end) = profile.span();
    #[expect(clippy::cast_precision_loss, reason = "a ratio")]
    let share = cpu.saturating_sub(loading) as f64 / (end - start).max(1) as f64;
    share
}

fn reset_peak_rss() -> Option<u64> {
    std::fs::write("/proc/self/clear_refs", "5").ok()?;
    memory("VmRSS:")
}

fn memory(key: &str) -> Option<u64> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    let line = status.lines().find_map(|line| line.strip_prefix(key))?;
    let kilobytes: u64 = line.trim().strip_suffix("kB")?.trim().parse().ok()?;
    Some(kilobytes * 1024)
}

#[derive(Debug, Clone, PartialEq)]
pub struct Summary {
    pub workload: String,
    pub setup: String,
    pub rate: f64,

    pub cost_percent: f64,

    pub noise_percent: f64,
    pub p99_ns: Option<u64>,
    pub recorder_cpu: Option<f64>,
    pub recording_bytes: Option<u64>,
}

#[must_use]
pub fn summarize(runs: &[Run]) -> Vec<Summary> {
    let mut summaries = Vec::new();

    for workload in WORKLOADS {
        let of = |setup: &str| -> Vec<&Run> {
            runs.iter()
                .filter(|run| run.workload == workload.name && run.setup == setup)
                .collect()
        };

        let bare_rates: Vec<f64> = of("bare").iter().map(|run| run.rate).collect();
        let Some(base) = median(bare_rates.iter().copied()) else {
            continue;
        };

        let spread = bare_rates.iter().copied().fold(f64::NAN, f64::max)
            - bare_rates.iter().copied().fold(f64::NAN, f64::min);

        for setup in setups(&workload, true) {
            let these = of(setup);
            let Some(rate) = median(these.iter().map(|run| run.rate)) else {
                continue;
            };

            summaries.push(Summary {
                workload: workload.name.to_owned(),
                setup: setup.to_owned(),
                rate,
                cost_percent: (1.0 - rate / base) * 100.0,
                noise_percent: spread / base * 100.0,
                p99_ns: median_u64(these.iter().filter_map(|run| run.p99_ns)),
                recorder_cpu: median(these.iter().filter_map(|run| run.recorder_cpu)),
                recording_bytes: median_u64(these.iter().filter_map(|run| run.recording_bytes)),
            });
        }
    }

    summaries
}

pub(super) fn median(values: impl Iterator<Item = f64>) -> Option<f64> {
    let mut values: Vec<f64> = values.collect();
    values.sort_unstable_by(f64::total_cmp);
    values.get(values.len() / 2).copied()
}

fn median_u64(values: impl Iterator<Item = u64>) -> Option<u64> {
    let mut values: Vec<u64> = values.collect();
    values.sort_unstable();
    values.get(values.len() / 2).copied()
}
