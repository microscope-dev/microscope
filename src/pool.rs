//! Dedicated `rayon` worker pools for debug-info loading, with per-worker CPU
//! time accounting.

use std::num::NonZeroUsize;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

pub const MAX_DEFAULT_JOBS: usize = 16;
const DEFAULT_JOBS: NonZeroUsize = NonZeroUsize::new(MAX_DEFAULT_JOBS).expect("a positive default");
const TEST_JOBS: NonZeroUsize = NonZeroUsize::new(2).expect("a positive default");

pub const JOBS_VARIABLE: &str = "MICROSCOPE_JOBS";

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

static SMALL: OnceLock<rayon::ThreadPool> = OnceLock::new();

static CLOCKS: Mutex<Vec<nix::time::ClockId>> = Mutex::new(Vec::new());

static MEASURED: Mutex<Measured> = Mutex::new(Measured {
    running: 0,
    cpu: None,
});

struct Measured {
    running: usize,
    cpu: Option<Duration>,
}

fn measured() -> std::sync::MutexGuard<'static, Measured> {
    MEASURED
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

struct Running;

impl Running {
    fn start() -> Self {
        let mut measured = measured();
        measured.running += 1;
        measured.cpu = None;
        Self
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        let mut measured = measured();
        debug_assert!(measured.running > 0, "a pool job ends only after it starts");
        measured.running -= 1;
        measured.cpu = None;
    }
}

fn register_worker() {
    let tid = nix::unistd::gettid().as_raw();
    // Linux `MAKE_THREAD_CPUCLOCK(tid, CPUCLOCK_SCHED)`: `(~tid << 3) | 6`.
    let clock = nix::time::ClockId::from_raw((!tid << 3) | 6);
    CLOCKS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .push(clock);
    measured().cpu = None;
}

pub fn cpu() -> Duration {
    let mut measured = measured();
    if measured.running == 0
        && let Some(cpu) = measured.cpu
    {
        return cpu;
    }

    let cpu = CLOCKS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .iter()
        .filter_map(|&clock| nix::time::clock_gettime(clock).ok())
        .map(Duration::from)
        .sum();
    measured.cpu = (measured.running == 0).then_some(cpu);
    cpu
}

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

    pool.broadcast(|_| ());

    Ok(POOL.get_or_init(|| pool))
}

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

/// Runs `work` on the shared loader pool, or inline if already on a `rayon`
/// worker thread to avoid nested pool switches.
pub(crate) fn install<R: Send>(work: impl FnOnce() -> R + Send) -> Result<R, JobsError> {
    if rayon::current_thread_index().is_some() {
        return Ok(work());
    }

    let pool = match POOL.get() {
        Some(pool) => pool,
        None => start(resolve_here(None, None)?)?,
    };

    let _running = Running::start();
    Ok(pool.install(work))
}

/// Runs small loads on a dedicated single-thread pool so they don't spin up the
/// full worker pool or contend with large parallel loads.
pub(crate) fn install_small<R: Send>(work: impl FnOnce() -> R + Send) -> Result<R, JobsError> {
    if rayon::current_thread_index().is_some() {
        return Ok(work());
    }

    let pool = if let Some(pool) = SMALL.get() {
        pool
    } else {
        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(1)
            .thread_name(|_| "microscope-load-small".to_owned())
            .start_handler(|_| register_worker())
            .build()
            .map_err(|error| JobsError::Start(error.to_string()))?;
        SMALL.get_or_init(|| pool)
    };

    let _running = Running::start();
    Ok(pool.install(work))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::Ordering;

    #[test]
    fn work_still_running_on_the_pool_is_counted() {
        use std::sync::atomic::AtomicBool;

        let before = cpu();
        assert_eq!(cpu(), before);

        let seen = AtomicBool::new(false);
        let grew = std::thread::scope(|scope| {
            scope.spawn(|| {
                install(|| {
                    rayon::broadcast(|_| {
                        while !seen.load(Ordering::Relaxed) {
                            std::hint::spin_loop();
                        }
                    });
                })
                .expect("the pool");
            });

            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            let mut grew = Duration::ZERO;
            while grew < Duration::from_millis(20) && std::time::Instant::now() < deadline {
                grew = cpu().saturating_sub(before);
            }
            seen.store(true, Ordering::Relaxed);
            grew
        });
        assert!(grew >= Duration::from_millis(20), "{grew:?}");

        let after = cpu();
        assert!(after >= before + grew, "{after:?} {before:?} {grew:?}");
    }

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
