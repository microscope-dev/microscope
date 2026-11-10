//! What a session told its client of the program: whether it runs, and
//! the stop it shows.
//!
//! The client learns of the program from the debugger's events, and from a
//! snapshot where it starts showing a process the debugger already had, or
//! catches up after missing events. Events the snapshot already reflects
//! come after it all the same, and are not news.

use microscope::{
    DebuggerEvent, ExecutionContext, ProcessId, StackFrameId, StopContext, StopId, StopReason,
    ThreadId,
};

/// The stop the client was told about.
#[derive(Debug, Clone)]
pub(super) struct Stop {
    pub id: StopId,
    pub thread: ThreadId,

    /// What the client knows as the thread that stopped: the task the
    /// thread runs, when the client's threads are tasks, or the thread.
    pub context: ExecutionContext,
    pub reason: StopReason,

    /// The reason the client was told, as its stopped event named it.
    pub told: &'static str,

    /// The frame of the stopped context the debugger selected, as at an
    /// exception the frame that raised it; the innermost otherwise.
    pub selected: StackFrameId,
}

impl Stop {
    /// The innermost frame of the stopped thread.
    pub const fn innermost(&self) -> StopContext {
        StopContext {
            stop: self.id,
            execution: ExecutionContext::Thread(self.thread),
            frame: StackFrameId::INNERMOST,
        }
    }
}

/// The program as the client was told of it.
#[derive(Debug, Clone, Default)]
pub(super) enum Run {
    /// No process: none started yet, or it ended.
    #[default]
    NoProcess,

    /// The process runs.
    Running(ProcessId),

    /// The process, or a core dump's, is stopped at `Stop`.
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

    /// The program once it runs: a stop it was at is left.
    pub const fn resumed(&self) -> Self {
        match self {
            Self::NoProcess => Self::NoProcess,
            Self::Running(process) | Self::Stopped(process, _) => Self::Running(*process),
        }
    }
}

/// What a debugger event tells the client of the program.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum News {
    /// The process started, or was attached.
    Started(ProcessId),

    /// The process stopped at a new stop.
    Stopped,

    /// The process left the stop the client shows for a reason the client
    /// did not ask for, such as another session's continue.
    ResumedElsewhere,

    /// The process exited.
    Exited,

    /// The debugger let the process go.
    Detached,

    /// Nothing new about where the program is.
    None,
}

/// What `event` tells a client shown `run`, which a snapshot at revision
/// `resynced` brought up to date. An event at or before that revision is
/// already shown, except the end of the shown process: a snapshot never
/// ends it, so its exit or detach is news however late it is read.
pub(super) const fn news(run: &Run, resynced: u64, event: &DebuggerEvent) -> News {
    let fresh = event.revision() > resynced;
    match event {
        DebuggerEvent::InferiorLaunched { process_id, .. }
        | DebuggerEvent::InferiorAttached { process_id, .. }
            if fresh && matches!(run, Run::NoProcess) =>
        {
            News::Started(*process_id)
        }
        DebuggerEvent::InferiorStopped { .. } if fresh => News::Stopped,

        // The client's own requests leave the stop as they resume it.
        DebuggerEvent::InferiorContinued { .. } if fresh && matches!(run, Run::Stopped(..)) => {
            News::ResumedElsewhere
        }
        DebuggerEvent::InferiorExited { process_id, .. } if fresh || shows(run, *process_id) => {
            News::Exited
        }
        DebuggerEvent::InferiorDetached { process_id, .. } if fresh || shows(run, *process_id) => {
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
    use microscope::{ExecutionId, ExitStatus, ResumeScope};

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

    /// Each event about process 7, read before (stale) and after (fresh) a
    /// snapshot at revision 10, by each program the client may be shown.
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
            // Shown: no process, running, stopped, another running.
            ("launched",  "fresh", [News::Started(SHOWN), News::None, News::None, News::None]),
            ("launched",  "stale", [News::None, News::None, News::None, News::None]),
            ("attached",  "fresh", [News::Started(SHOWN), News::None, News::None, News::None]),
            ("attached",  "stale", [News::None, News::None, News::None, News::None]),
            ("stopped",   "fresh", [News::Stopped, News::Stopped, News::Stopped, News::Stopped]),
            ("stopped",   "stale", [News::None, News::None, News::None, News::None]),
            ("continued", "fresh", [News::None, News::None, News::ResumedElsewhere, News::None]),
            ("continued", "stale", [News::None, News::None, News::None, News::None]),
            ("exited",    "fresh", [News::Exited, News::Exited, News::Exited, News::Exited]),
            ("exited",    "stale", [News::None, News::Exited, News::Exited, News::None]),
            ("detached",  "fresh", [News::Detached, News::Detached, News::Detached, News::Detached]),
            ("detached",  "stale", [News::None, News::Detached, News::Detached, News::None]),
            ("other",     "fresh", [News::None, News::None, News::None, News::None]),
        ];

        let mut wrong = Vec::new();
        for (kind, freshness, expected) in table {
            let revision = if freshness == "fresh" { 11 } else { 10 };
            for ((shown, run), expected) in runs.iter().zip(expected) {
                let got = news(run, 10, &event(kind, revision));
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
