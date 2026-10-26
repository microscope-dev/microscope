//! A thread that unwinds the BPF engine's sampled stacks, so that the
//! thread that drains its ring spends its time draining it
//! (`dev/syscalls.md`).
//!
//! Each stack is unwound through its process's modules as they were when
//! its record was handled ([`Unwinding`]). The thread keeps its own
//! [`FrameRules`]. Stacks come back in the order they were sent; the reader
//! gives each event to the sink once its stack, and every one before it,
//! is back.

use std::sync::mpsc;
use std::thread::JoinHandle;

use super::bpf::SampledStack;
use super::stacks::Unwinding;
use crate::unwind::sample::{FrameRules, SampledFrame, SampledState, StackEnd};

/// A stack to unwind, numbered by the reader.
pub struct Job {
    pub id: u64,
    pub unwinding: Unwinding,
    pub sampled: SampledStack,
}

/// A stack unwound, with the sample it was unwound from, or the panic
/// unwinding it raised, which the reader raises again, as it would have
/// raised it unwinding the stack itself.
pub struct Done {
    pub id: u64,
    pub sampled: SampledStack,
    pub unwound: std::thread::Result<(Vec<SampledFrame>, StackEnd)>,
}

/// The unwinding thread.
pub struct Unwinder {
    jobs: Option<mpsc::Sender<Job>>,
    done: mpsc::Receiver<Done>,
    thread: Option<JoinHandle<()>>,
}

impl Unwinder {
    pub fn spawn() -> std::io::Result<Self> {
        let (finished, done) = mpsc::channel();
        let (jobs, taken) = mpsc::channel::<Job>();
        let thread = std::thread::Builder::new()
            .name("microscope-unwind".into())
            .spawn(move || {
                let mut rules = FrameRules::default();
                for job in taken {
                    let unwound = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        job.unwinding.unwind(
                            &mut rules,
                            SampledState {
                                registers: &job.sampled.registers,
                                stack: &job.sampled.copy,
                                chain: &job.sampled.chain,
                            },
                        )
                    }));
                    // What a panic left of the rules may be half made.
                    if unwound.is_err() {
                        rules = FrameRules::default();
                    }
                    let done = Done {
                        id: job.id,
                        sampled: job.sampled,
                        unwound,
                    };
                    if finished.send(done).is_err() {
                        break;
                    }
                }
            })?;

        Ok(Self {
            jobs: Some(jobs),
            done,
            thread: Some(thread),
        })
    }

    /// Gives `job` to the thread: whether it took it, as it does while it
    /// runs.
    #[must_use]
    pub fn send(&self, job: Job) -> bool {
        self.jobs
            .as_ref()
            .is_some_and(|jobs| jobs.send(job).is_ok())
    }

    /// A stack unwound since last asked, if any.
    #[must_use]
    pub fn try_done(&self) -> Option<Done> {
        self.done.try_recv().ok()
    }

    /// The next stack unwound, waiting for it; `None` once the thread no
    /// longer runs.
    #[must_use]
    pub fn wait_done(&self) -> Option<Done> {
        self.done.recv().ok()
    }
}

impl Drop for Unwinder {
    fn drop(&mut self) {
        self.jobs = None;
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}
