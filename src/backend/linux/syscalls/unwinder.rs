use std::sync::mpsc;
use std::thread::JoinHandle;

use super::bpf::SampledStack;
use super::stacks::Unwinding;
use crate::unwind::sample::{FrameRules, SampledFrame, SampledState, StackEnd};

pub struct Job {
    pub id: u64,
    pub unwinding: Unwinding,
    pub sampled: SampledStack,
}

pub struct Done {
    pub id: u64,
    pub unwinding: Unwinding,
    pub sampled: SampledStack,
    pub unwound: std::thread::Result<(Vec<SampledFrame>, StackEnd)>,
}

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
                    if unwound.is_err() {
                        rules = FrameRules::default();
                    }
                    let done = Done {
                        id: job.id,
                        unwinding: job.unwinding,
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

    #[must_use]
    pub fn send(&self, job: Job) -> bool {
        self.jobs
            .as_ref()
            .is_some_and(|jobs| jobs.send(job).is_ok())
    }

    #[must_use]
    pub fn try_done(&self) -> Option<Done> {
        self.done.try_recv().ok()
    }

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
