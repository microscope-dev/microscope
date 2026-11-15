use microscope::{
    DebuggerEvent, ExecutionContext, ExitStatus, ProcessId, StackFrameId, StopContext, StopId,
    StopReason, ThreadId,
};

#[derive(Debug, Clone)]
pub(super) struct Stop {
    pub id: StopId,
    pub thread: ThreadId,

    pub context: ExecutionContext,
    pub reason: StopReason,

    pub told: &'static str,

    pub selected: StackFrameId,
}

impl Stop {
    pub const fn innermost(&self) -> StopContext {
        StopContext {
            stop: self.id,
            execution: ExecutionContext::Thread(self.thread),
            frame: StackFrameId::INNERMOST,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub(super) enum Run {
    #[default]
    NoProcess,

    Running(ProcessId),

    Stopped(ProcessId, Stop),
}

impl Run {
    pub const fn process(&self) -> Option<ProcessId> {
        match self {
            Self::NoProcess => None,
            Self::Running(process) | Self::Stopped(process, _) => Some(*process),
        }
    }

    pub const fn stop(&self) -> Option<&Stop> {
        match self {
            Self::Stopped(_, stop) => Some(stop),
            Self::NoProcess | Self::Running(_) => None,
        }
    }

    pub const fn resumed(&self) -> Self {
        match self {
            Self::NoProcess => Self::NoProcess,
            Self::Running(process) | Self::Stopped(process, _) => Self::Running(*process),
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum News {
    Started(ProcessId),

    Stopped {
        process: ProcessId,
        stop: StopId,
        thread: ThreadId,
        reason: StopReason,
    },

    ResumedElsewhere,

    Exited(ExitStatus),

    Detached,

    None,
}

pub(super) fn news(run: &Run, resynced: u64, event: DebuggerEvent) -> News {
    let fresh = event.revision() > resynced;
    match event {
        DebuggerEvent::InferiorLaunched { process_id, .. }
        | DebuggerEvent::InferiorAttached { process_id, .. }
            if fresh && matches!(run, Run::NoProcess) =>
        {
            News::Started(process_id)
        }
        DebuggerEvent::InferiorStopped {
            process_id,
            stop_id,
            thread_id,
            reason,
            ..
        } if fresh => News::Stopped {
            process: process_id,
            stop: stop_id,
            thread: thread_id,
            reason,
        },

        DebuggerEvent::InferiorContinued { .. } if fresh && matches!(run, Run::Stopped(..)) => {
            News::ResumedElsewhere
        }
        DebuggerEvent::InferiorExited {
            process_id, status, ..
        } if fresh || shows(run, process_id) => News::Exited(status),
        DebuggerEvent::InferiorDetached { process_id, .. } if fresh || shows(run, process_id) => {
            News::Detached
        }
        _ => News::None,
    }
}

const fn shows(run: &Run, process: ProcessId) -> bool {
    match run.process() {
        Some(shown) => shown.get() == process.get(),
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use microscope::{ExecutionId, ResumeScope};

    const SHOWN: ProcessId = ProcessId::new(7);
    const OTHER: ProcessId = ProcessId::new(8);

    fn stopped() -> Run {
        Run::Stopped(
            SHOWN,
            Stop {
                id: StopId::new(1),
                thread: ThreadId::new(7),
                context: ExecutionContext::Thread(ThreadId::new(7)),
                reason: StopReason::Pause,
                told: "pause",
                selected: StackFrameId::INNERMOST,
            },
        )
    }

    fn event(kind: &str, revision: u64) -> DebuggerEvent {
        let process_id = SHOWN;
        match kind {
            "launched" => DebuggerEvent::InferiorLaunched {
                revision,
                process_id,
                execution_id: ExecutionId::new(1),
            },
            "attached" => DebuggerEvent::InferiorAttached {
                revision,
                process_id,
            },
            "stopped" => DebuggerEvent::InferiorStopped {
                revision,
                process_id,
                execution_id: None,
                stop_id: StopId::new(2),
                thread_id: ThreadId::new(7),
                reason: StopReason::Pause,
            },
            "continued" => DebuggerEvent::InferiorContinued {
                revision,
                process_id,
                execution_id: ExecutionId::new(1),
                resumed: ResumeScope::Process(process_id),
            },
            "exited" => DebuggerEvent::InferiorExited {
                revision,
                process_id,
                execution_id: None,
                status: ExitStatus::Code(0),
            },
            "detached" => DebuggerEvent::InferiorDetached {
                revision,
                process_id,
            },
            _ => DebuggerEvent::StateChanged { revision },
        }
    }

    fn stop() -> News {
        News::Stopped {
            process: SHOWN,
            stop: StopId::new(2),
            thread: ThreadId::new(7),
            reason: StopReason::Pause,
        }
    }

    fn exit() -> News {
        News::Exited(ExitStatus::Code(0))
    }

    #[test]
    fn events_are_news_once_and_only_for_the_program_shown() {
        let runs = [
            ("no process", Run::NoProcess),
            ("running", Run::Running(SHOWN)),
            ("stopped", stopped()),
            ("another running", Run::Running(OTHER)),
        ];

        #[rustfmt::skip]
        let table = [
            ("launched",  "fresh", [News::Started(SHOWN), News::None, News::None, News::None]),
            ("launched",  "stale", [News::None, News::None, News::None, News::None]),
            ("attached",  "fresh", [News::Started(SHOWN), News::None, News::None, News::None]),
            ("attached",  "stale", [News::None, News::None, News::None, News::None]),
            ("stopped",   "fresh", [stop(), stop(), stop(), stop()]),
            ("stopped",   "stale", [News::None, News::None, News::None, News::None]),
            ("continued", "fresh", [News::None, News::None, News::ResumedElsewhere, News::None]),
            ("continued", "stale", [News::None, News::None, News::None, News::None]),
            ("exited",    "fresh", [exit(), exit(), exit(), exit()]),
            ("exited",    "stale", [News::None, exit(), exit(), News::None]),
            ("detached",  "fresh", [News::Detached, News::Detached, News::Detached, News::Detached]),
            ("detached",  "stale", [News::None, News::Detached, News::Detached, News::None]),
            ("other",     "fresh", [News::None, News::None, News::None, News::None]),
        ];

        let mut wrong = Vec::new();
        for (kind, freshness, expected) in table {
            let revision = if freshness == "fresh" { 11 } else { 10 };
            for ((shown, run), expected) in runs.iter().zip(expected) {
                let got = news(run, 10, event(kind, revision));
                if got != expected {
                    wrong.push(format!(
                        "{freshness} {kind} shown {shown}: got {got:?}, want {expected:?}"
                    ));
                }
            }
        }

        assert!(wrong.is_empty(), "{}", wrong.join("\n"));
    }
}
