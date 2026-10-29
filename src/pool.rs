//! The loader's worker pool: one per process, shared by every load.
//!
//! Worker count precedence: [`configure`]'s option, `MICROSCOPE_JOBS`, the
//! `jobs` configuration setting, then available CPUs capped at
//! [`MAX_DEFAULT_JOBS`] (two under nextest, which runs tests concurrently).
//! Work already running on a pool, such as a test's own, stays there.

use std::num::NonZeroUsize;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

/// The most workers the pool starts with when nothing says how many.
pub const MAX_DEFAULT_JOBS: usize = 16;
const DEFAULT_JOBS: NonZeroUsize = NonZeroUsize::new(MAX_DEFAULT_JOBS).expect("a positive default");
const TEST_JOBS: NonZeroUsize = NonZeroUsize::new(2).expect("a positive default");

/// The environment variable that sets the number of workers.
pub const JOBS_VARIABLE: &str = "MICROSCOPE_JOBS";

/// Why the pool could not be configured.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum JobsError {
    #[error("{JOBS_VARIABLE} must be a positive number of workers, not {0:?}")]
    Invalid(String),

    #[error("the loader already runs {running} workers, so it cannot run {requested}")]
    AlreadyStarted { running: usize, requested: usize },

    #[error("cannot start the loader's workers: {0}")]
    Start(String),
}

static POOL: OnceLock<rayon::ThreadPool> = OnceLock::new();

/// One worker, for loads too small to share: woken for each object a JIT
/// compiler registers, the whole pool's workers spun looking for work,
/// seconds of CPU time over a two-second program, on its own cores.
static SMALL: OnceLock<rayon::ThreadPool> = OnceLock::new();

/// The CPU-time clock of every worker either pool started.
static CLOCKS: Mutex<Vec<nix::time::ClockId>> = Mutex::new(Vec::new());

/// Registers the calling worker's CPU-time clock, as glibc's
/// `pthread_getcpuclockid` names a thread's: its id, inverted, shifted,
/// and marked a thread's scheduler clock.
fn register_worker() {
    let tid = nix::unistd::gettid().as_raw();
    let clock = nix::time::ClockId::from_raw((!tid << 3) | 6);
    CLOCKS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .push(clock);
}

/// The CPU time every worker of the loader has spent so far, whichever
/// load it worked for.
pub fn cpu() -> Duration {
    CLOCKS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .iter()
        .filter_map(|&clock| nix::time::clock_gettime(clock).ok())
        .map(Duration::from)
        .sum()
}

/// How many workers to run: `option`, else the variable's value `variable`,
/// else `config`, else `cpus` up to [`MAX_DEFAULT_JOBS`], or two when
/// `under_test`.
pub fn resolve(
    option: Option<NonZeroUsize>,
    variable: Option<&str>,
    config: Option<NonZeroUsize>,
    cpus: NonZeroUsize,
    under_test: bool,
) -> Result<NonZeroUsize, JobsError> {
    if let Some(jobs) = option {
        return Ok(jobs);
    }

    if let Some(text) = variable {
        return text
            .trim()
            .parse::<NonZeroUsize>()
            .map_err(|_| JobsError::Invalid(text.to_owned()));
    }

    if let Some(jobs) = config {
        return Ok(jobs);
    }

    let most = if under_test { TEST_JOBS } else { DEFAULT_JOBS };
    Ok(cpus.min(most))
}

/// Resolves the number of workers from the process's environment.
fn resolve_here(
    option: Option<NonZeroUsize>,
    config: Option<NonZeroUsize>,
) -> Result<NonZeroUsize, JobsError> {
    let variable = std::env::var(JOBS_VARIABLE).ok();
    let cpus = std::thread::available_parallelism().unwrap_or(NonZeroUsize::MIN);
    let under_test = std::env::var_os("NEXTEST_RUN_ID").is_some();
    resolve(option, variable.as_deref(), config, cpus, under_test)
}

fn start(jobs: NonZeroUsize) -> Result<&'static rayon::ThreadPool, JobsError> {
    if let Some(pool) = POOL.get() {
        return Ok(pool);
    }

    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(jobs.get())
        .thread_name(|index| format!("microscope-load-{index}"))
        .start_handler(|_| register_worker())
        .build()
        .map_err(|error| JobsError::Start(error.to_string()))?;

    // Start every worker before measuring, excluding startup allocations.
    pool.broadcast(|_| ());

    // Another thread may have started one first; either is the pool.
    Ok(POOL.get_or_init(|| pool))
}

/// Starts the pool with [`resolve`]'s worker count and returns that count.
/// Fails if the pool already runs a different count.
pub fn configure(
    option: Option<NonZeroUsize>,
    config: Option<NonZeroUsize>,
) -> Result<usize, JobsError> {
    let requested = resolve_here(option, config)?;
    let running = start(requested)?.current_num_threads();
    if running != requested.get() {
        return Err(JobsError::AlreadyStarted {
            running,
            requested: requested.get(),
        });
    }

    Ok(running)
}

/// Runs `work` on the pool, starting it if nothing has, unless `work`
/// already runs on a pool, whose workers it keeps using.
pub(crate) fn install<R: Send>(work: impl FnOnce() -> R + Send) -> Result<R, JobsError> {
    if rayon::current_thread_index().is_some() {
        return Ok(work());
    }

    let pool = match POOL.get() {
        Some(pool) => pool,
        None => start(resolve_here(None, None)?)?,
    };

    Ok(pool.install(work))
}

/// Runs `work`, a load too small to share, on a pool of one worker, as
/// [`install`] runs work on the shared pool.
pub(crate) fn install_small<R: Send>(work: impl FnOnce() -> R + Send) -> Result<R, JobsError> {
    if rayon::current_thread_index().is_some() {
        return Ok(work());
    }

    let pool = match SMALL.get() {
        Some(pool) => pool,
        None => {
            let pool = rayon::ThreadPoolBuilder::new()
                .num_threads(1)
                .thread_name(|_| "microscope-load-small".to_owned())
                .start_handler(|_| register_worker())
                .build()
                .map_err(|error| JobsError::Start(error.to_string()))?;
            SMALL.get_or_init(|| pool)
        }
    };

    Ok(pool.install(work))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_first_source_that_says_wins() {
        let n = |count| NonZeroUsize::new(count);
        let cpus = n(64).unwrap();
        let resolved = |option, variable, config, under_test| {
            resolve(option, variable, config, cpus, under_test).map(NonZeroUsize::get)
        };

        assert_eq!(resolved(n(3), Some("5"), n(7), false), Ok(3));
        assert_eq!(resolved(None, Some(" 5\n"), n(7), false), Ok(5));
        assert_eq!(resolved(None, None, n(7), true), Ok(7));
        assert_eq!(resolved(None, None, None, false), Ok(MAX_DEFAULT_JOBS));
        assert_eq!(resolved(None, None, None, true), Ok(2));
        assert_eq!(
            resolve(None, None, None, NonZeroUsize::MIN, false).map(NonZeroUsize::get),
            Ok(1)
        );
        for invalid in ["0", "-1", "", "many"] {
            assert_eq!(
                resolved(None, Some(invalid), n(7), false),
                Err(JobsError::Invalid(invalid.to_owned()))
            );
        }
    }
}
