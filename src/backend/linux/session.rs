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
    Idle {
        ended: Option<Ended>,
        cleanup_error: Option<Arc<str>>,
    },

    Traced(Traced),

    Draining {
        orphans: Orphans,

        deferred: Option<Start>,
        ended: Ended,
        cleanup_error: Option<Arc<str>>,
    },
}

#[derive(Debug, Clone)]
pub(super) enum Ended {
    Exited(ProcessId, ExitStatus),

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

pub(super) struct Traced {
    pub(super) inferior: Inferior,

    pub(super) startup: Option<Startup>,

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

#[derive(Default)]
pub(super) struct Shutdown {
    pub(super) reply: Option<Reply<()>>,
}

pub(super) struct Orphans {
    pub(super) parent: Pid,

    pub(super) children: BTreeMap<Pid, Vec<(VirtualAddress, u8)>>,

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

    pub(super) const fn ended(&self) -> Option<&Ended> {
        match self {
            Self::Idle { ended, .. } => ended.as_ref(),
            Self::Draining { ended, .. } => Some(ended),
            Self::Traced(_) => None,
        }
    }

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
