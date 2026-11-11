//! Where a controller's session is in its life, and the replies each phase
//! owes. A reply lives in the one phase that answers it, so the transition
//! that leaves the phase has to take it.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use nix::unistd::Pid;

use crate::protocol::{ExecutionId, ExitStatus, InferiorState, ProcessId, Reply, StopId};
use crate::{Error, VirtualAddress};

use super::{Inferior, Start, Waiter};

#[expect(
    clippy::large_enum_variant,
    reason = "a controller holds one session in place; boxing the inferior adds a pointer chase to every access"
)]
pub(super) enum Session {
    /// No process is traced. `ended` is how the last one ended, and
    /// `cleanup_error` why its runtime agents could not be retired, which
    /// a shutdown reports.
    Idle {
        ended: Option<Ended>,
        cleanup_error: Option<Arc<str>>,
    },

    Traced(Traced),

    /// The process ended with fork children still on their way to their
    /// first stop, which are released before another process is traced.
    Draining {
        orphans: Orphans,

        /// A launch or attach held until the children are released.
        deferred: Option<Start>,
        ended: Ended,
        cleanup_error: Option<Arc<str>>,
    },
}

/// How a traced process's session ended.
#[derive(Debug, Clone)]
pub(super) enum Ended {
    Exited(ProcessId, ExitStatus),

    /// Released, handed to a group's watcher, or vanished while attached.
    Detached(ProcessId),
}

impl Ended {
    pub(super) fn state(&self) -> InferiorState {
        match self {
            Self::Exited(process_id, status) => InferiorState::Exited {
                process_id: *process_id,
                status: status.clone(),
            },
            Self::Detached(process_id) => InferiorState::Detached {
                process_id: *process_id,
            },
        }
    }
}

/// A traced process, and the requests about it still waiting for an
/// answer. A kill can be sent while a launch or attach is still starting,
/// so the two are held side by side.
pub(super) struct Traced {
    pub(super) inferior: Inferior,

    /// The launch or attach waiting for the process's first stop.
    pub(super) startup: Option<Startup>,

    /// The client waiting for a killed process to be gone.
    pub(super) kill: Option<Reply<()>>,
}

pub(super) enum Startup {
    Launch(Reply<ExecutionId>),
    Attach {
        reply: Reply<StopId>,

        /// How many times the attach found threads it had not traced.
        rescans: u32,
    },
}

/// A session that is ending, held alongside whichever phase the session
/// is in: it ends once that phase leaves nothing to release.
#[derive(Default)]
pub(super) struct Shutdown {
    /// The client's shutdown. One the controller began itself, as a failed
    /// attach does, has none until a client asks.
    pub(super) reply: Option<Reply<()>>,
}

/// Fork children still on their way to their first stop when their parent's
/// process ended, with the waiter that reports them. Each is scrubbed of the
/// traps it inherited and released at that stop; the waiter, which would
/// otherwise poll them forever, is joined once none remains.
pub(super) struct Orphans {
    /// The process that forked them.
    pub(super) parent: Pid,

    /// Each child with the traps it may hold.
    pub(super) children: BTreeMap<Pid, Vec<(VirtualAddress, u8)>>,

    /// Children that could not be scrubbed, killed and awaiting their exit.
    pub(super) killed: BTreeSet<Pid>,
    pub(super) waiter: Option<Waiter>,
}

impl Session {
    pub(super) const fn idle() -> Self {
        Self::Idle {
            ended: None,
            cleanup_error: None,
        }
    }

    /// How the last traced process ended, until another is traced.
    pub(super) const fn ended(&self) -> Option<&Ended> {
        match self {
            Self::Idle { ended, .. } => ended.as_ref(),
            Self::Draining { ended, .. } => Some(ended),
            Self::Traced(_) => None,
        }
    }

    /// A traced process with no request waiting on it.
    pub(super) const fn live(inferior: Inferior) -> Self {
        Self::Traced(Traced {
            inferior,
            startup: None,
            kill: None,
        })
    }

    pub(super) const fn is_idle(&self) -> bool {
        matches!(self, Self::Idle { .. })
    }

    pub(super) const fn traced(&self) -> Option<&Traced> {
        match self {
            Self::Traced(traced) => Some(traced),
            Self::Idle { .. } | Self::Draining { .. } => None,
        }
    }

    pub(super) const fn traced_mut(&mut self) -> Option<&mut Traced> {
        match self {
            Self::Traced(traced) => Some(traced),
            Self::Idle { .. } | Self::Draining { .. } => None,
        }
    }

    pub(super) const fn inferior(&self) -> Option<&Inferior> {
        match self.traced() {
            Some(traced) => Some(&traced.inferior),
            None => None,
        }
    }

    pub(super) const fn inferior_mut(&mut self) -> Option<&mut Inferior> {
        match self.traced_mut() {
            Some(traced) => Some(&mut traced.inferior),
            None => None,
        }
    }

    pub(super) const fn orphans_mut(&mut self) -> Option<&mut Orphans> {
        match self {
            Self::Draining { orphans, .. } => Some(orphans),
            Self::Idle { .. } | Self::Traced(_) => None,
        }
    }

    /// Whether a launch or attach is still waiting for its first stop.
    pub(super) const fn starting(&self) -> bool {
        matches!(
            self,
            Self::Traced(Traced {
                startup: Some(_),
                ..
            })
        )
    }

    pub(super) const fn attaching(&self) -> bool {
        matches!(
            self,
            Self::Traced(Traced {
                startup: Some(Startup::Attach { .. }),
                ..
            })
        )
    }

    /// Takes the launch's reply, if a launch is starting.
    pub(super) fn take_launch(&mut self) -> Option<Reply<ExecutionId>> {
        let startup = &mut self.traced_mut()?.startup;
        match startup.take() {
            Some(Startup::Launch(reply)) => Some(reply),
            other => {
                *startup = other;
                None
            }
        }
    }

    /// Takes the attach's reply, if an attach is starting.
    pub(super) fn take_attach(&mut self) -> Option<Reply<StopId>> {
        let startup = &mut self.traced_mut()?.startup;
        match startup.take() {
            Some(Startup::Attach { reply, .. }) => Some(reply),
            other => {
                *startup = other;
                None
            }
        }
    }

    /// Ends the traced phase as `ended` says, leaving the session idle,
    /// and returns what the phase still owes.
    pub(super) fn take_traced(&mut self, ended: impl FnOnce(ProcessId) -> Ended) -> Option<Traced> {
        let tgid = self.inferior()?.tgid;
        let idle = Self::Idle {
            ended: Some(ended(super::process_id(tgid))),
            cleanup_error: None,
        };
        match std::mem::replace(self, idle) {
            Self::Traced(traced) => Some(traced),
            Self::Idle { .. } | Self::Draining { .. } => None,
        }
    }
}

impl Traced {
    pub(super) const fn starting(inferior: Inferior, startup: Startup) -> Self {
        Self {
            inferior,
            startup: Some(startup),
            kill: None,
        }
    }
}

impl Startup {
    /// Answers the launch or attach with `error`.
    pub(super) fn fail(self, error: Error) {
        match self {
            Self::Launch(reply) => {
                let _ = reply.send(Err(error));
            }
            Self::Attach { reply, .. } => {
                let _ = reply.send(Err(error));
            }
        }
    }
}
