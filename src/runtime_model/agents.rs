//! Optional runtime agents. Models describe requests and cleanup; the
//! controller owns artifacts, generations, all-stop writes and resuming.

use std::num::NonZeroU32;
use std::sync::Arc;

use super::{Arming, RuntimeStop, RuntimeWrite};
use crate::{ImageAddress, ThreadId, VirtualAddress};

/// A process identifier in the target's PID namespace, proven by the edge.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TargetProcessId(NonZeroU32);

impl TargetProcessId {
    pub fn new(value: u64) -> Option<Self> {
        let value = u32::try_from(value).ok()?;
        i32::try_from(value)
            .is_ok()
            .then_some(Self(NonZeroU32::new(value)?))
    }

    pub const fn get(self) -> NonZeroU32 {
        self.0
    }
}

/// Outcomes of one preparation attempt, not permanent capabilities.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AgentFailure {
    #[error("the interpreter has not finished initialization")]
    Initializing,
    #[error("no thread is running interpreted code")]
    NoThread,
    #[error("the interpreter thread already has a pending remote-debugging request")]
    PendingRequest,
    #[error("the remote-debugging script slot at {address} has not been cleared")]
    UnclearedSlot { address: VirtualAddress },
    #[error("{0}")]
    ReadLimit(Arc<str>),
    #[error("{0}")]
    Unavailable(Arc<str>),
    #[error("{0}")]
    Refused(Arc<str>),
}

impl From<Arc<str>> for AgentFailure {
    fn from(reason: Arc<str>) -> Self {
        Self::Unavailable(reason)
    }
}

impl From<&'static str> for AgentFailure {
    fn from(reason: &'static str) -> Self {
        Self::Refused(reason.into())
    }
}

/// What the lifecycle owner proved about this request before cleanup.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentRetirementProof {
    /// The owned agent acknowledged this source generation.
    Acknowledged,
    /// No target thread ran after the publication attempt.
    Unresumed,
    /// Execution may have consumed the request; cancel only if still pending.
    Cancel,
}

#[derive(Debug)]
pub enum AgentRetirement {
    /// Source files can be retired after these cleanup writes succeed.
    Retired(Vec<RuntimeWrite>),
    /// Consumption began but its completion has not been established.
    Unconfirmed,
}

/// A request's ownership evidence, revalidated at every later all-stop.
pub trait AgentRequestTicket: std::fmt::Debug + Send + Sync {
    fn retire(
        &self,
        stop: &dyn RuntimeStop,
        proof: AgentRetirementProof,
    ) -> Result<AgentRetirement, Arc<str>>;
}

#[derive(Debug)]
pub struct PreparedAgentRequest {
    pub writes: Vec<RuntimeWrite>,
    pub ticket: Arc<dyn AgentRequestTicket>,

    /// Whether the script goes to the bootstrap a launch gave the runtime,
    /// rather than through `writes`, for a runtime that cannot be asked
    /// to run a script.
    pub bootstrapped: bool,
}

/// A request the launch bootstrap takes, which leaves the process's memory
/// as it was: retiring it makes no write.
#[derive(Debug)]
pub struct BootstrapTicket;

impl AgentRequestTicket for BootstrapTicket {
    fn retire(
        &self,
        _stop: &dyn RuntimeStop,
        _proof: AgentRetirementProof,
    ) -> Result<AgentRetirement, Arc<str>> {
        Ok(AgentRetirement::Retired(Vec::new()))
    }
}

/// A runtime's optional transport. No model creates files or makes writes.
pub trait RuntimeAgent: std::fmt::Debug + Send + Sync {
    fn library(&self) -> Vec<u8>;
    fn control_symbol(&self) -> &'static str;
    /// Where a thread begins to run the runtime's code, which the
    /// controller watches while the agent waits for such a thread.
    fn entries(&self) -> &[ImageAddress];

    /// The controller prepares files in this particular thread's namespace.
    /// A step back of `arming` begins where its thread runs native code
    /// the runtime called, and binds on that thread, wherever it is.
    fn eligible_thread(
        &self,
        stop: &dyn RuntimeStop,
        preferred: Option<ThreadId>,
        arming: &Arming<'_>,
    ) -> Result<ThreadId, AgentFailure>;

    fn script(
        &self,
        stop: &dyn RuntimeStop,
        library: &str,
        generation: u64,
        process: TargetProcessId,
        arming: &Arming<'_>,
    ) -> Result<String, Arc<str>>;

    fn inject(
        &self,
        stop: &dyn RuntimeStop,
        script: &str,
        thread: ThreadId,
        arming: &Arming<'_>,
    ) -> Result<PreparedAgentRequest, AgentFailure>;

    /// Payload and length precede the aligned sequence publication.
    fn command(
        &self,
        stop: &dyn RuntimeStop,
        control: VirtualAddress,
        previous: u64,
        generation: u64,
        arming: &Arming<'_>,
    ) -> Result<Vec<RuntimeWrite>, Arc<str>>;

    /// Configuration/discovery may require runtime cooperation by siblings.
    fn process_scope_required(&self) -> bool;

    /// Why the agent cannot give a variable a value exactly in this
    /// runtime, whatever the stop; `None` where it can.
    fn assignments_refused(&self) -> Option<&'static str> {
        None
    }

    /// Whether `arming` asks anything of the agent, which is installed only
    /// when something does: a breakpoint, a step, or, for a runtime whose
    /// agent reports them, an exception that stops.
    fn wanted(&self, arming: &Arming<'_>) -> bool {
        !arming.breakpoints.is_empty() || arming.step.is_some()
    }
}
