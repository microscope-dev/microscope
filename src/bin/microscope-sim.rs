//! Sweeps and replays deterministic simulations of debugging sessions.
//!
//! `microscope-sim sweep` runs random seeds on every core for a while and
//! reports each kind of failure once, with its shortest run. `microscope-sim
//! replay SEED` reruns one seed and prints its whole trace. `microscope-sim
//! shrink SEED` finds a smaller run that fails as the seed's does.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::panic::AssertUnwindSafe;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use microscope::sim::{Corpus, Outcome, Reach, Settings, describe_failure, run};

#[derive(Parser)]
#[command(about = "Deterministic simulation of microscope debugging sessions")]
struct Args {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Runs random seeds on every core and reports each kind of failure.
    Sweep {
        /// How long to sweep.
        #[arg(long, default_value_t = 30)]
        seconds: u64,

        /// How many worker threads; every core by default.
        #[arg(long)]
        threads: Option<usize>,

        /// The first seed; chosen from the clock by default.
        #[arg(long, value_parser = parse_seed)]
        start: Option<u64>,
    },

    /// Reruns one seed, printing its whole trace.
    Replay {
        #[arg(value_parser = parse_seed)]
        seed: u64,

        /// Stops after this action and prints the state there.
        #[arg(long)]
        at: Option<u64>,

        /// The fingerprint the seed's run had, which the replay must match.
        #[arg(long, value_parser = parse_seed)]
        fingerprint: Option<u64>,

        /// A run `shrink` wrote, whose simpler shape and choices the replay
        /// takes.
        #[arg(long)]
        shrunk: Option<PathBuf>,
    },

    /// Finds a smaller run that fails as the seed's does, and writes it
    /// for `replay --shrunk`.
    Shrink {
        #[arg(value_parser = parse_seed)]
        seed: u64,

        /// The most runs to make while shrinking.
        #[arg(long, default_value_t = 2000)]
        runs: u64,
    },
}

fn parse_seed(text: &str) -> Result<u64, String> {
    text.strip_prefix("0x")
        .map_or_else(|| text.parse(), |hex| u64::from_str_radix(hex, 16))
        .map_err(|error| format!("not a seed: {error}"))
}

/// How many things per second, for people to read.
#[expect(clippy::cast_precision_loss, reason = "a rate for people to read")]
fn rate(count: u64, elapsed: Duration, threads: usize) -> f64 {
    count as f64 / elapsed.as_secs_f64() / threads as f64
}

fn main() -> Result<ExitCode> {
    let args = Args::parse();
    let corpus = Corpus::load().context("loading the golden corpus")?;
    match args.command {
        Command::Sweep {
            seconds,
            threads,
            start,
        } => Ok(sweep(&corpus, Duration::from_secs(seconds), threads, start)),
        Command::Replay {
            seed,
            at,
            fingerprint,
            shrunk,
        } => replay(&corpus, seed, at, fingerprint, shrunk),
        Command::Shrink { seed, runs } => shrink(&corpus, seed, runs),
    }
}

/// Where a seed's replays and shrunk runs are written.
fn directory(seed: u64) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("target/sim")
        .join(format!("{seed:#x}"))
}

fn shrink(corpus: &Corpus, seed: u64, runs: u64) -> Result<ExitCode> {
    let Some(shrunk) = microscope::sim::shrink::shrink(seed, corpus, &Settings::default(), runs)
    else {
        println!("seed {seed:#x} passes; there is nothing to shrink");
        return Ok(ExitCode::FAILURE);
    };

    let directory = directory(seed);
    std::fs::create_dir_all(&directory)?;
    let path = directory.join("shrunk.txt");
    std::fs::write(&path, shrunk.to_text())?;

    print!("{}", describe_failure(&shrunk.outcome));
    let simpler = shrunk
        .simpler
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>();
    println!(
        "shrunk in {} runs to {} actions and {} choices{}",
        shrunk.runs,
        shrunk.outcome.steps,
        shrunk.tape.len(),
        if simpler.is_empty() {
            String::new()
        } else {
            format!(", with {}", simpler.join(" "))
        }
    );
    println!(
        "replay   just sim-seed {seed:#x} --shrunk {} --fingerprint {:#x}",
        path.display(),
        shrunk.outcome.fingerprint
    );

    Ok(ExitCode::SUCCESS)
}

/// One kind of failure: how often it happened, and the report of its
/// shortest run, the easiest to read.
struct Group {
    count: u64,

    /// The shortest run's length in actions, then its seed, which breaks
    /// ties so that a sweep's choice does not depend on its threads.
    shortest: (u64, u64),
    fingerprint: u64,
    report: String,
}

/// Counts a failed run in its failure's group, which keeps the shortest
/// run's report.
fn record_failure(groups: &Mutex<BTreeMap<String, Group>>, outcome: &Outcome) {
    let Some(failure) = &outcome.failure else {
        return;
    };

    let run = (outcome.steps, outcome.seed);
    let mut groups = groups.lock().unwrap_or_else(PoisonError::into_inner);
    let group = groups.entry(failure.signature()).or_insert_with(|| Group {
        count: 0,
        shortest: run,
        fingerprint: outcome.fingerprint,
        report: describe_failure(outcome),
    });
    group.count += 1;

    if run < group.shortest {
        group.shortest = run;
        group.fingerprint = outcome.fingerprint;
        group.report = describe_failure(outcome);
    }

    drop(groups);
}

fn sweep(
    corpus: &Corpus,
    duration: Duration,
    threads: Option<usize>,
    start: Option<u64>,
) -> ExitCode {
    let threads = threads.unwrap_or_else(|| {
        std::thread::available_parallelism().map_or(1, std::num::NonZeroUsize::get)
    });
    let start = start.unwrap_or_else(|| {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |since| {
                since
                    .as_secs()
                    .wrapping_mul(1_000_000_000)
                    .wrapping_add(u64::from(since.subsec_nanos()))
            })
    });
    println!("sweeping from seed {start:#x} on {threads} threads for {duration:?}");
    let next = AtomicU64::new(0);
    let done = AtomicBool::new(false);
    let sessions = AtomicU64::new(0);
    let groups = Mutex::new(BTreeMap::<String, Group>::new());
    let escaped = Mutex::new(Vec::<u64>::new());

    // How many sessions planned each kind of fault, and how many of those
    // fired.
    let faults = Mutex::new(BTreeMap::<&'static str, (u64, u64)>::new());
    let reach = Mutex::new(Reach::default());
    let began = Instant::now();
    let settings = Settings::default();

    std::thread::scope(|scope| {
        for _ in 0..threads {
            scope.spawn(|| {
                let mut reached = Reach::default();

                while !done.load(Ordering::Relaxed) {
                    let seed = start.wrapping_add(next.fetch_add(1, Ordering::Relaxed));

                    // A run catches the panics of its actions, but not of
                    // what sets it up and tears it down; one there must not
                    // end the sweep without naming its seed.
                    let Ok(outcome) =
                        std::panic::catch_unwind(AssertUnwindSafe(|| run(seed, corpus, &settings)))
                    else {
                        sessions.fetch_add(1, Ordering::Relaxed);
                        escaped
                            .lock()
                            .unwrap_or_else(PoisonError::into_inner)
                            .push(seed);
                        continue;
                    };

                    sessions.fetch_add(1, Ordering::Relaxed);

                    if let Some(plan) = outcome.swarm.fault {
                        let mut faults = faults.lock().unwrap_or_else(PoisonError::into_inner);
                        let counts = faults.entry(plan.kind()).or_default();
                        counts.0 += 1;
                        counts.1 += u64::from(outcome.unfired.is_none());
                        drop(faults);
                    }

                    reached.add(&outcome.marks);
                    record_failure(&groups, &outcome);
                }

                reach
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .merge(&reached);
            });
        }

        while began.elapsed() < duration {
            std::thread::sleep(Duration::from_secs(1).min(duration));
            let count = sessions.load(Ordering::Relaxed);
            println!(
                "{:>5.0}s  {count} sessions, {:.0} per second",
                began.elapsed().as_secs_f64(),
                rate(count, began.elapsed(), 1)
            );
        }

        done.store(true, Ordering::Relaxed);
    });
    let groups = groups.into_inner().unwrap_or_else(PoisonError::into_inner);
    let count = sessions.load(Ordering::Relaxed);
    println!(
        "{count} sessions in {:.1}s, {:.0} per second per thread",
        began.elapsed().as_secs_f64(),
        rate(count, began.elapsed(), threads)
    );

    for (kind, (planned, fired)) in faults.into_inner().unwrap_or_else(PoisonError::into_inner) {
        println!("{kind}: fired in {fired} of {planned} sessions that planned it");
    }

    // A mark no session reached says what this sweep never tested.
    let unreached = reach
        .into_inner()
        .unwrap_or_else(PoisonError::into_inner)
        .below(1)
        .into_iter()
        .map(|(mark, _)| format!("{mark:?}"))
        .collect::<Vec<_>>();
    if !unreached.is_empty() {
        println!("no session reached {}", unreached.join(", "));
    }

    let escaped = escaped.into_inner().unwrap_or_else(PoisonError::into_inner);
    report_failures(corpus, &settings, &groups, escaped)
}

/// Prints each group of failures, and the seeds whose runs panicked outside
/// their actions. Succeeds when there were none.
fn report_failures(
    corpus: &Corpus,
    settings: &Settings,
    groups: &BTreeMap<String, Group>,
    mut escaped: Vec<u64>,
) -> ExitCode {
    escaped.sort_unstable();

    if groups.is_empty() && escaped.is_empty() {
        println!("no failures");
        return ExitCode::SUCCESS;
    }

    for seed in &escaped {
        println!("\nseed {seed:#x} panicked outside its actions: just sim-seed {seed:#x}");
    }

    for (signature, group) in groups {
        let (steps, seed) = group.shortest;
        println!(
            "\n{signature}\n{} sessions; the shortest, seed {seed:#x}, failed at action #{steps}\n{}",
            group.count, group.report
        );

        // A seed is worth replaying only if it names the run that failed.
        let again = run(seed, corpus, settings).fingerprint;
        if again != group.fingerprint {
            println!(
                "seed {seed:#x} ran differently when run again ({again:#x}, not {:#x}): \
                 the simulation is not deterministic",
                group.fingerprint
            );
        }
    }

    ExitCode::FAILURE
}

fn replay(
    corpus: &Corpus,
    seed: u64,
    at: Option<u64>,
    fingerprint: Option<u64>,
    shrunk: Option<PathBuf>,
) -> Result<ExitCode> {
    let (simpler, tape) = match shrunk {
        Some(path) => {
            let text = std::fs::read_to_string(&path)
                .with_context(|| format!("reading {}", path.display()))?;
            let (shrunk_seed, simpler, tape) = microscope::sim::shrink::read(&text)
                .map_err(|error| anyhow::anyhow!("{}: {error}", path.display()))?;
            anyhow::ensure!(
                shrunk_seed == seed,
                "{} shrank seed {shrunk_seed:#x}, not {seed:#x}",
                path.display()
            );
            (simpler, Some(tape))
        }
        None => (Vec::new(), None),
    };

    let settings = Settings {
        keep: None,
        stop_at: at,
        simpler,
        tape,
        ..Settings::default()
    };

    let outcome = run(seed, corpus, &settings);
    let text = report(&outcome);
    print!("{text}");
    let directory = directory(seed);
    std::fs::create_dir_all(&directory)?;
    std::fs::write(directory.join("trace.log"), &text)?;
    println!("written to {}", directory.join("trace.log").display());

    if let Some(expected) = fingerprint
        && at.is_none()
        && expected != outcome.fingerprint
    {
        println!(
            "the replay's fingerprint {:#x} differs from {expected:#x}: the run is not \
             deterministic, or the code changed since",
            outcome.fingerprint
        );
        return Ok(ExitCode::FAILURE);
    }

    Ok(if outcome.failure.is_some() {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    })
}

fn report(outcome: &Outcome) -> String {
    let mut text = describe_failure(outcome);
    if outcome.failure.is_none() {
        text = format!(
            "seed {:#x} passed: {} {:?}, {} actions\nswarm {}\n{text}",
            outcome.seed, outcome.program, outcome.arguments, outcome.steps, outcome.swarm
        );
    }

    if let Some(state) = &outcome.state {
        let _ = writeln!(text, "state after action #{}:{state}", outcome.steps);
    }

    text
}
