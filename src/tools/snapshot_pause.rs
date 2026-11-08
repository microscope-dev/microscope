//! How long stopped Python snapshots hold a program paused.
//!
//! Each interpreter runs the profiler's Python workload under the snapshot
//! engine, with its separate debug files when it has them, until the
//! recording holds a number of snapshots. The pauses are the spans the
//! recording marked as stopped for each snapshot.

use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, bail};
use serde::{Deserialize, Serialize};

use crate::profiler::{GapReason, PythonEngine, RecordOptions, Watch, record_watched};

/// The interpreters measured: the first three with separate debug files,
/// the others without.
pub const INTERPRETERS: [&str; 5] = ["3.12", "3.14", "3.14t", "3.15", "standalone-3.14"];

/// How often the engine asks for a snapshot. Shorter than any pause, so
/// that snapshots follow one another as fast as they can be taken.
const EVERY: Duration = Duration::from_millis(20);

/// How long a recording may take to gather its snapshots.
const DEADLINE: Duration = Duration::from_secs(120);

pub struct Options {
    pub snapshots: u64,
    pub only: Vec<String>,
}

/// One interpreter's snapshots, each pause in microseconds.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Run {
    pub interpreter: String,
    pub pauses_us: Vec<u64>,
}

/// The pauses of one interpreter's run, summarized.
#[derive(Debug, Clone)]
pub struct Summary {
    pub interpreter: String,
    pub snapshots: usize,
    pub median_us: u64,
    pub p90_us: u64,
    pub max_us: u64,
}

/// Records each interpreter `options` names in turn.
pub fn measure(options: &Options, mut each: impl FnMut(&Run)) -> Result<Vec<Run>> {
    let root = std::env::var_os("MICROSCOPE_CPYTHON")
        .context("MICROSCOPE_CPYTHON names the interpreters; run in the dev shell")?;
    let root = PathBuf::from(root);

    let mut runs = Vec::new();
    for interpreter in INTERPRETERS
        .into_iter()
        .filter(|name| options.only.is_empty() || options.only.iter().any(|only| only == name))
    {
        let run = run(&root, interpreter, options.snapshots)?;
        each(&run);
        runs.push(run);
    }

    Ok(runs)
}

fn run(root: &std::path::Path, interpreter: &str, snapshots: u64) -> Result<Run> {
    let mut command = Command::new(root.join(interpreter).join("python"));
    command
        .args(["-I", "-B"])
        .arg(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/python/overhead.py"))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());

    let debug = root.join(interpreter).join("debug/lib/debug");
    let options = RecordOptions {
        python: Some(PythonEngine::Snapshot { every: EVERY }),
        debug_directories: debug.is_dir().then_some(debug).into_iter().collect(),
        ..RecordOptions::default()
    };

    let stop = AtomicBool::new(false);
    let started = Instant::now();
    let recording = record_watched(
        command,
        &options,
        &mut Watch {
            every: Duration::from_millis(50),
            publish: &mut |profile| {
                if profile.task_snapshots().0 >= snapshots || started.elapsed() > DEADLINE {
                    stop.store(true, Ordering::Relaxed);
                }
            },
            stop: &stop,
            save: None,
        },
    )
    .with_context(|| format!("recording {interpreter}"))?;

    let (taken, _, failed) = recording.profile.task_snapshots();
    if failed > 0 {
        bail!("{interpreter}: {failed} of {taken} snapshots failed");
    }

    let pauses_us: Vec<u64> = recording
        .profile
        .paused(GapReason::TaskSnapshot)
        .into_iter()
        .map(|(start, end)| end.saturating_sub(start).unsigned_abs() / 1000)
        .collect();
    if pauses_us.len() < usize::try_from(snapshots)? {
        bail!(
            "{interpreter}: only {} of {snapshots} snapshots in {DEADLINE:?}",
            pauses_us.len()
        );
    }

    Ok(Run {
        interpreter: interpreter.to_owned(),
        pauses_us,
    })
}

/// Each run's pauses by their median, 90th percentile, and longest.
#[must_use]
pub fn summarize(runs: &[Run]) -> Vec<Summary> {
    runs.iter()
        .map(|run| {
            let mut pauses = run.pauses_us.clone();
            pauses.sort_unstable();
            // The pause `tenths` of the way from the shortest to the longest.
            let at = |tenths: usize| {
                let index = (pauses.len().saturating_sub(1) * tenths + 5) / 10;
                pauses.get(index).copied().unwrap_or(0)
            };
            Summary {
                interpreter: run.interpreter.clone(),
                snapshots: pauses.len(),
                median_us: at(5),
                p90_us: at(9),
                max_us: pauses.last().copied().unwrap_or(0),
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_summary_takes_order_statistics_of_the_pauses_unsorted() {
        let runs = [Run {
            interpreter: "3.12".into(),
            pauses_us: vec![9, 1, 5, 3, 7, 2, 8, 4, 6, 100, 0],
        }];

        let summary = &summarize(&runs)[0];

        assert_eq!(summary.snapshots, 11);
        assert_eq!(summary.median_us, 5);
        assert_eq!(summary.p90_us, 9);
        assert_eq!(summary.max_us, 100);
    }
}
