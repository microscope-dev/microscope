//! Controller-owned agent files, outstanding requests and acknowledgements.
use std::sync::Arc;
use std::time::{Duration, Instant};

const ACK_TIMEOUT: Duration = Duration::from_secs(5);
const TIMED_OUT: &str =
    "runtime agent acknowledgement timed out; configuration remains unconfirmed";

use super::agent_files::AgentFiles;
use super::native::LinuxTraceOps;
use super::runtimes::BoundRuntime;
use super::{Controller, Edit, debug_pid, debug_thread_id};
use crate::RuntimeId;
use crate::runtime_model::{
    AgentFailure, AgentRequestTicket, AgentRetirement, AgentRetirementProof, Arming, HookPlace,
    TargetProcessId,
};

#[derive(Debug)]
pub(super) struct Installation {
    files: Option<Box<dyn AgentFiles>>,
    library: Option<Arc<str>>,
    ticket: Option<Arc<dyn AgentRequestTicket>>,
    next: u64,
    sequence: u64,
    state: State,
    sent: Option<Desired>,
    generation: Option<u64>,
    deadline: Option<Instant>,
    expired: bool,
    request_thread: Option<crate::ThreadId>,
    remaining: Duration,
}

#[derive(Debug)]
enum State {
    Waiting,
    Awaiting { generation: u64, source: bool },
    Ready,
    Refused(Arc<str>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Desired {
    breakpoints: Vec<crate::runtime_model::ManagedArm>,
    step: Option<crate::runtime_model::RuntimeStep>,
}

impl From<&Arming<'_>> for Desired {
    fn from(arming: &Arming<'_>) -> Self {
        Self {
            breakpoints: arming.breakpoints.to_vec(),
            step: arming.step.cloned(),
        }
    }
}

impl Installation {
    fn new() -> Self {
        Self {
            files: None,
            library: None,
            ticket: None,
            next: 1,
            sequence: 0,
            state: State::Waiting,
            sent: None,
            generation: None,
            deadline: None,
            expired: false,
            request_thread: None,
            remaining: ACK_TIMEOUT,
        }
    }
    pub(super) const fn deadline(&self) -> Option<Instant> {
        self.deadline
    }

    pub(super) fn start_deadline(&mut self, now: Instant) {
        if matches!(self.state, State::Awaiting { .. }) && !self.expired && self.deadline.is_none()
        {
            self.deadline = Some(now + self.remaining);
        }
    }
    pub(super) fn pause_deadline(&mut self, now: Instant) {
        if let Some(deadline) = self.deadline.take() {
            self.remaining = deadline.saturating_duration_since(now);
        }
    }

    pub(super) const fn needs_entry(&self) -> bool {
        matches!(self.state, State::Waiting)
    }
}

impl super::Inferior {
    pub(super) fn retire_agents_after_exit(&mut self) -> Result<(), Arc<str>> {
        let mut problems = Vec::new();
        for (_, mut installation) in std::mem::take(&mut self.agents) {
            if let Some(files) = installation.files.as_mut()
                && let Err(reason) = files.close_after_target_exit()
            {
                problems.push(reason.to_string());
            }
        }
        if problems.is_empty() {
            Ok(())
        } else {
            Err(problems.join("; ").into())
        }
    }
}

impl<P: LinuxTraceOps> Controller<P> {
    pub(super) fn agent_timeout(&mut self, now: Instant) -> crate::Result<()> {
        let Some(inferior) = self.inferior.as_mut() else {
            return Ok(());
        };
        let mut expired = false;
        for (&runtime, installation) in &mut inferior.agents {
            if installation
                .deadline
                .is_some_and(|deadline| deadline <= now)
            {
                installation.expired = true;
                installation.deadline = None;
                inferior.managed.unarmed.insert(runtime, TIMED_OUT.into());
                expired = true;
            }
        }
        if !expired {
            return Ok(());
        }
        let Some(thread) = inferior.threads.keys().next().copied() else {
            return Ok(());
        };
        self.publish_breakpoints_changed();
        // Use the ordinary stop barrier: no thread is declared stopped until
        // its actual wait status arrives.
        self.raise_barrier(
            thread,
            crate::StopReason::RuntimeEvent {
                description: TIMED_OUT.into(),
            },
        )
    }

    /// Publish disarming before removing traps. A mapped companion remains
    /// valid after unlink; its next callback consumes the empty configuration
    /// before dispatching any application event. Shutdown never runs a thread.
    pub(super) fn prepare_agents_for_detach(&mut self) -> Result<(), Arc<str>> {
        let runtimes = self
            .inferior
            .as_ref()
            .map(|inferior| self.runtimes(inferior))
            .unwrap_or_default();
        let mut problems = Vec::new();
        for bound in runtimes {
            let Some(mut installation) = self
                .inferior
                .as_mut()
                .and_then(|inferior| inferior.agents.remove(&bound.id))
            else {
                continue;
            };
            let outcome = (|| {
                if let Some(ticket) = installation.ticket.as_ref() {
                    let inferior = self.inferior.as_ref().ok_or("the process is gone")?;
                    let retirement = self.with_runtime_stop(
                        inferior,
                        &bound,
                        inferior.memory_thread(),
                        |stop| ticket.retire(stop, AgentRetirementProof::Cancel),
                    )?;
                    let AgentRetirement::Retired(writes) = retirement else {
                        return Err(Arc::<str>::from(
                            "preserving Python agent files: source consumption is unconfirmed at detach",
                        ));
                    };
                    self.write_for_runtime(&writes)?;
                    installation
                        .files
                        .as_mut()
                        .ok_or("the agent files are missing")?
                        .acknowledge(
                            installation
                                .generation
                                .ok_or("the agent generation is missing")?,
                        )?;
                    installation.ticket = None;
                } else if installation.library.is_some() {
                    let inferior = self.inferior.as_ref().ok_or("the process is gone")?;
                    let agent = bound.model.agent().ok_or("the runtime has no agent")?;
                    let control = self
                        .owned_agent_symbol(
                            inferior,
                            installation
                                .library
                                .as_deref()
                                .ok_or("the library is missing")?,
                            agent.control_symbol(),
                        )
                        .ok_or("the owned agent control symbol is unavailable at detach")?;
                    let arming = Arming {
                        breakpoints: &[],
                        step: None,
                        exceptions: crate::ExceptionStops::default(),
                    };
                    let writes = self.with_runtime_stop(
                        inferior,
                        &bound,
                        inferior.memory_thread(),
                        |stop| {
                            agent.command(
                                stop,
                                control,
                                installation.sequence,
                                installation.next,
                                &arming,
                            )
                        },
                    )?;
                    self.write_for_runtime(&writes)?;
                }
                if let Some(files) = installation.files.as_mut() {
                    files.close()?;
                }
                Ok(())
            })();
            if let Err(reason) = outcome {
                problems.push(reason.to_string());
            }
            if let Some(inferior) = self.inferior.as_mut() {
                inferior.agents.insert(bound.id, installation);
            }
        }
        if problems.is_empty() {
            Ok(())
        } else {
            Err(problems.join("; ").into())
        }
    }

    pub(super) fn validate_agent_resume(&self, scope: crate::ResumeScope) -> crate::Result<()> {
        if matches!(scope, crate::ResumeScope::Process(_)) {
            return Ok(());
        }
        let Some(inferior) = self.inferior.as_ref() else {
            return Ok(());
        };
        if let crate::ResumeScope::Thread(thread) = scope
            && inferior.agents.values().any(|installation| {
                matches!(installation.state, State::Awaiting { source: true, .. })
                    && installation
                        .request_thread
                        .is_some_and(|consumer| consumer != thread)
            })
        {
            return Err(crate::Error::RuntimeResumeUnsupported(
                "the outstanding source request belongs to a different interpreter thread".into(),
            ));
        }
        for bound in self.runtimes(inferior) {
            // A waiting or refused agent is idle: the target's agent clears
            // its monitoring on every failure.
            if inferior.agents.get(&bound.id).is_some_and(|installation| {
                matches!(installation.state, State::Awaiting { .. })
                    || (matches!(installation.state, State::Ready)
                        && installation.sent.as_ref().is_some_and(|sent| {
                            !sent.breakpoints.is_empty() || sent.step.is_some()
                        }))
            }) && bound
                .model
                .agent()
                .is_some_and(crate::runtime_model::RuntimeAgent::process_scope_required)
            {
                return Err(crate::Error::RuntimeResumeUnsupported("free-threaded monitoring requires a process-wide resume for cooperation from every interpreter thread".into()));
            }
        }
        Ok(())
    }

    pub(super) fn arm_agent(
        &mut self,
        bound: &BoundRuntime,
        arming: &Arming<'_>,
    ) -> Result<(), Arc<str>> {
        self.write_for_runtime(&[])?;
        let inferior = self
            .inferior
            .as_mut()
            .ok_or_else(|| Arc::<str>::from("the process is gone"))?;
        let previous = inferior.agents.remove(&bound.id);
        if previous.is_none() && arming.breakpoints.is_empty() && arming.step.is_none() {
            return Ok(());
        }
        let mut installation = previous.unwrap_or_else(Installation::new);
        let wanted = Desired::from(arming);
        let result = match &installation.state {
            State::Awaiting { .. } => Err(if installation.expired {
                TIMED_OUT.into()
            } else {
                "waiting for the runtime agent to acknowledge its configuration".into()
            }),
            State::Ready if installation.sent.as_ref() == Some(&wanted) => Ok(()),
            State::Refused(reason) if installation.sent.as_ref() == Some(&wanted) => {
                Err(Arc::clone(reason))
            }
            _ => match self.configure_agent(bound, arming, &mut installation) {
                Ok(()) => {
                    installation.sent = Some(wanted);
                    Err("waiting for the runtime agent to acknowledge its configuration".into())
                }
                Err(AgentFailure::Initializing | AgentFailure::NoThread) => {
                    installation.sent = Some(wanted);
                    installation.state = State::Waiting;
                    Err("waiting for an initialized interpreter thread outside the agent".into())
                }
                Err(reason) => {
                    let reason: Arc<str> = reason.to_string().into();
                    // Still ready: no request reached the target, whose agent
                    // keeps the acknowledged configuration. The next arming
                    // tries again.
                    if !matches!(installation.state, State::Ready) {
                        installation.sent = Some(wanted);
                        if !matches!(installation.state, State::Awaiting { .. }) {
                            installation.state = State::Refused(Arc::clone(&reason));
                        }
                    }
                    Err(reason)
                }
            },
        };
        if let Some(inferior) = self.inferior.as_mut() {
            // An internal stop starts the deadline as it resumes.
            if inferior.active.is_some()
                && inferior.public_stop.is_none()
                && inferior.barrier.is_none()
            {
                installation.start_deadline(Instant::now());
            }
            inferior.agents.insert(bound.id, installation);
        }
        self.sync_runtime_hooks()
            .map_err(|error| Arc::<str>::from(error.to_string()))?;
        result
    }

    #[expect(
        clippy::too_many_lines,
        reason = "one publication boundary preserves request ownership across partial writes"
    )]
    fn configure_agent(
        &mut self,
        bound: &BoundRuntime,
        arming: &Arming<'_>,
        installation: &mut Installation,
    ) -> Result<(), AgentFailure> {
        let agent = bound.model.agent().ok_or("the runtime has no agent")?;
        let inferior = self.inferior.as_ref().ok_or("the process is gone")?;
        installation.expired = false;
        installation.deadline = None;
        installation.remaining = ACK_TIMEOUT;
        let generation = installation.next;
        let next = generation
            .checked_add(1)
            .ok_or("runtime agent generations are exhausted")?;
        let active = installation
            .sent
            .as_ref()
            .is_some_and(|sent| !sent.breakpoints.is_empty() || sent.step.is_some());
        if matches!(installation.state, State::Ready) && active {
            let library = installation
                .library
                .as_deref()
                .ok_or("the agent library path is missing")?;
            let control = self
                .owned_agent_symbol(inferior, library, agent.control_symbol())
                .ok_or("the owned agent control symbol is unavailable")?;
            let writes =
                self.with_runtime_stop(inferior, bound, inferior.memory_thread(), |stop| {
                    agent.command(stop, control, installation.sequence, generation, arming)
                })?;
            return self.publish_agent_command(bound, installation, generation, next, &writes);
        }
        let preferred = inferior
            .public_stop
            .as_ref()
            .and_then(|stop| stop.selected_thread)
            .map(debug_thread_id);
        let thread = self.with_runtime_stop(inferior, bound, inferior.memory_thread(), |stop| {
            agent.eligible_thread(stop, preferred)
        })?;
        let mut identities = self
            .ptrace
            .target_thread_ids(inferior.tgid, &[inferior.tgid])
            .into_iter()
            .filter(|(host, _)| *host == inferior.tgid);
        let (_, target) = identities
            .next()
            .ok_or("the target process's PID namespace identity is unproven")?;
        if identities.next().is_some() {
            return Err("the target process's PID namespace identity is ambiguous".into());
        }
        let process =
            TargetProcessId::new(target.get()).ok_or("the target process id is invalid")?;
        let reader = debug_pid(thread).map_err(|error| Arc::<str>::from(error.to_string()))?;
        if let Some(files) = installation.files.as_mut() {
            files.anchor(reader)?;
        } else {
            installation.files = Some(self.ptrace.agent_files(reader, &agent.library())?);
        }
        let files = installation
            .files
            .as_mut()
            .ok_or("the agent files are missing")?;
        let library = files.library()?.to_owned();
        let source = agent.script(&library, generation, process, arming)?;
        let path = files.script(generation, &source)?;
        let prepared = self.with_runtime_stop(inferior, bound, inferior.memory_thread(), |stop| {
            agent.inject(stop, &path, thread)
        });
        let prepared = match prepared {
            Ok(prepared) => prepared,
            Err(reason) => {
                files.acknowledge(generation)?;
                return Err(reason);
            }
        };
        installation.library = Some(library.into());
        installation.request_thread = Some(thread);
        installation.ticket = Some(prepared.ticket);
        installation.next = next;
        installation.generation = Some(generation);
        installation.state = State::Awaiting {
            generation,
            source: true,
        };
        if let Err(reason) = self.write_for_runtime(&prepared.writes) {
            // No thread has resumed since preparation. Revalidate even a
            // partially written slot before retiring the unpublished source.
            if let Err(cleanup) = self.retire_agent_source(
                bound,
                installation,
                generation,
                AgentRetirementProof::Unresumed,
            ) {
                return Err(Arc::<str>::from(format!(
                    "{reason}; preserving the unconfirmed request: {cleanup}"
                ))
                .into());
            }
            installation.state = State::Refused(Arc::clone(&reason));
            if let Some(files) = installation.files.as_mut() {
                files.close()?;
            }
            installation.files = None;
            installation.library = None;
            // A replacement library's command slot starts unpublished.
            installation.sequence = 0;
            return Err(reason.into());
        }
        Ok(())
    }

    fn publish_agent_command(
        &mut self,
        bound: &BoundRuntime,
        installation: &mut Installation,
        generation: u64,
        next: u64,
        writes: &[crate::runtime_model::RuntimeWrite],
    ) -> Result<(), AgentFailure> {
        let publication = writes
            .last()
            .ok_or("the agent command has no publication write")?;
        let result = self.write_for_runtime(writes);
        installation.next = next;
        installation.request_thread = None;
        if let Err(reason) = &result {
            let inferior = self.inferior.as_ref().ok_or("the process is gone")?;
            let mut held = vec![0; publication.expected.len()];
            let readable =
                self.with_runtime_stop(inferior, bound, inferior.memory_thread(), |stop| {
                    stop.read(publication.address, &mut held)
                });
            if readable && held == publication.expected {
                // Payload or length may have changed, but this command was
                // not published: the agent reads neither until the sequence
                // changes, and keeps its acknowledged configuration.
                return Err(Arc::clone(reason).into());
            }
            if readable && held == publication.bytes {
                installation.sequence = generation;
            }
            // Unreadable/foreign publication bytes remain unconfirmed. Keep
            // one generation outstanding and bound the acknowledgement wait.
        } else {
            installation.sequence = generation;
        }
        installation.generation = Some(generation);
        installation.state = State::Awaiting {
            generation,
            source: false,
        };
        result.map_err(AgentFailure::from)
    }

    fn retire_agent_source(
        &mut self,
        bound: &BoundRuntime,
        installation: &mut Installation,
        generation: u64,
        proof: AgentRetirementProof,
    ) -> Result<(), Arc<str>> {
        let inferior = self.inferior.as_ref().ok_or("the process is gone")?;
        let ticket = installation
            .ticket
            .as_ref()
            .ok_or("the source ownership ticket is missing")?;
        let retirement =
            self.with_runtime_stop(inferior, bound, inferior.memory_thread(), |stop| {
                ticket.retire(stop, proof)
            })?;
        let AgentRetirement::Retired(writes) = retirement else {
            return Err("the acknowledged source could not be retired".into());
        };
        self.write_for_runtime(&writes)?;
        installation
            .files
            .as_mut()
            .ok_or("the agent files are missing")?
            .acknowledge(generation)?;
        installation.ticket = None;
        Ok(())
    }

    pub(super) fn queue_agent_report(
        &mut self,
        runtime: RuntimeId,
        generation: u64,
        failure: Option<Arc<str>>,
    ) -> Result<bool, Arc<str>> {
        let Some(inferior) = self.inferior.as_ref() else {
            return Ok(false);
        };
        let Some(installation) = inferior.agents.get(&runtime) else {
            return Ok(true);
        };
        if installation.generation != Some(generation) {
            return Ok(false);
        }
        self.begin_internal_stop()
            .map_err(|error| Arc::<str>::from(error.to_string()))?;
        let barrier = self
            .inferior
            .as_mut()
            .and_then(|inferior| inferior.barrier.as_mut())
            .ok_or("the report has no all-stop barrier")?;
        // Unlike ArmAgents these reports cannot be coalesced: two generations
        // or runtimes may report before their siblings finish stopping.
        barrier.edits.push(Edit::AgentReport {
            runtime,
            generation,
            failure,
        });
        Ok(true)
    }

    pub(super) fn apply_agent_report(
        &mut self,
        runtime: RuntimeId,
        generation: u64,
        failure: Option<&Arc<str>>,
    ) {
        if let Err(description) = self.agent_report(runtime, generation, failure)
            && let Some(barrier) = self
                .inferior
                .as_mut()
                .and_then(|inferior| inferior.barrier.as_mut())
        {
            barrier.reason = Some(crate::StopReason::RuntimeEvent { description });
        }
        if self.sync_runtime_arming() {
            self.publish_breakpoints_changed();
        }
    }

    fn agent_report(
        &mut self,
        runtime: RuntimeId,
        generation: u64,
        failure: Option<&Arc<str>>,
    ) -> Result<bool, Arc<str>> {
        let Some(inferior) = self.inferior.as_mut() else {
            return Ok(false);
        };
        let Some(mut installation) = inferior.agents.remove(&runtime) else {
            return Ok(true);
        };
        let result = (|| {
            if installation.generation != Some(generation) {
                return Ok(false);
            }
            let source = matches!(installation.state, State::Awaiting { generation: pending, source: true } if pending == generation);
            let awaiting = matches!(installation.state, State::Awaiting { generation: pending, .. } if pending == generation);
            if failure.is_none() && !awaiting {
                return Ok(false);
            }
            if awaiting && !source {
                // Source acknowledgements do not publish the native slot.
                // A matching native report does prove its consumed sequence,
                // including after an uncertain publication write/read.
                installation.sequence = generation;
            }
            if source {
                let inferior = self.inferior.as_ref().ok_or("the process is gone")?;
                let bound = self
                    .runtimes(inferior)
                    .into_iter()
                    .find(|bound| bound.id == runtime)
                    .ok_or("the runtime is no longer loaded")?;
                self.retire_agent_source(
                    &bound,
                    &mut installation,
                    generation,
                    AgentRetirementProof::Acknowledged,
                )?;
            }
            installation.deadline = None;
            installation.expired = false;
            installation.request_thread = None;
            installation.state =
                failure.map_or(State::Ready, |reason| State::Refused(Arc::clone(reason)));
            Ok(true)
        })();
        if let Some(inferior) = self.inferior.as_mut() {
            inferior.agents.insert(runtime, installation);
            if matches!(result, Ok(true)) {
                inferior.managed.mark_stale(runtime);
            }
        }
        result
    }

    pub(super) fn agent_entry(&mut self, runtime: RuntimeId) -> Result<(), Arc<str>> {
        if let Some(inferior) = self.inferior.as_mut() {
            inferior.managed.mark_stale(runtime);
        }
        self.queue_internal_edit(Edit::ArmAgents)
            .map_err(|error| error.to_string().into())
    }

    pub(super) fn agent_entry_hook(&self, bound: &BoundRuntime) -> Option<HookPlace> {
        let inferior = self.inferior.as_ref()?;
        if inferior
            .agents
            .get(&bound.id)
            .is_some_and(|agent| !agent.needs_entry())
        {
            return None;
        }
        if inferior.managed.arms(bound.id).is_empty()
            && inferior
                .runtime_step
                .as_ref()
                .is_none_or(|(runtime, _)| *runtime != bound.id)
        {
            return None;
        }
        bound.model.agent()?.entry().map(HookPlace::Own)
    }

    pub(super) fn agent_companion_matches(
        inferior: &super::Inferior,
        runtime: RuntimeId,
        image: &crate::ModuleImage,
    ) -> bool {
        inferior
            .agents
            .get(&runtime)
            .and_then(|agent| agent.library.as_deref())
            .is_none_or(|path| {
                image.path() == std::path::Path::new(path)
                    || std::path::Path::new(path)
                        .strip_prefix("/")
                        .is_ok_and(|suffix| image.path().ends_with(suffix))
            })
    }

    fn owned_agent_symbol(
        &self,
        inferior: &super::Inferior,
        library: &str,
        name: &str,
    ) -> Option<crate::VirtualAddress> {
        self.loaded_images(inferior)
            .filter(|(_, image)| {
                image.path() == std::path::Path::new(library)
                    || std::path::Path::new(library)
                        .strip_prefix("/")
                        .is_ok_and(|suffix| image.path().ends_with(suffix))
            })
            .find_map(|(module, image)| {
                module
                    .virtual_address(
                        image
                            .symbols_named(name)
                            .find(|symbol| symbol.exported())?
                            .address(),
                    )
                    .ok()
            })
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::{ScriptedRuntime, watch_harness};
    use super::*;
    use crate::{ExitStatus, StopReason};
    use std::sync::Mutex;

    fn awaiting() -> Installation {
        let mut installation = Installation::new();
        installation.state = State::Awaiting {
            generation: 5,
            source: false,
        };
        installation.generation = Some(5);
        installation.next = 6;
        installation
    }

    #[test]
    fn acknowledgement_deadlines_do_not_count_time_at_a_public_stop() {
        let now = Instant::now();
        let mut installation = awaiting();
        installation.start_deadline(now);
        assert_eq!(installation.deadline(), Some(now + ACK_TIMEOUT));
        installation.start_deadline(now + Duration::from_secs(1));
        assert_eq!(installation.deadline(), Some(now + ACK_TIMEOUT));
        installation.pause_deadline(now);
        assert_eq!(installation.deadline(), None);
        let later = now + Duration::from_secs(3600);
        installation.start_deadline(later);
        assert_eq!(installation.deadline(), Some(later + ACK_TIMEOUT));
        installation.expired = true;
        installation.pause_deadline(later);
        installation.start_deadline(later);
        assert_eq!(
            installation.deadline(),
            None,
            "expired requests cannot restart their timers"
        );
    }

    #[test]
    fn an_expired_agent_deadline_cannot_be_starved_by_queued_requests() {
        let mut harness = watch_harness(1);
        let mut installation = awaiting();
        installation.deadline = Some(Instant::now().checked_sub(Duration::from_secs(1)).unwrap());
        let inferior = harness.controller.inferior.as_mut().expect("inferior");
        let pid = inferior.tgid;
        inferior.agents.insert(RuntimeId::new(0), installation);
        harness
            .controller
            .pending
            .borrow_mut()
            .push_back(super::super::ControllerMessage::Wait(
                super::super::WaitEvent::Continued(pid),
            ));
        assert!(matches!(
            harness.controller.next_message(false),
            Some(super::super::ControllerMessage::AgentTimeout)
        ));
    }

    #[test]
    fn only_the_outstanding_generation_can_acknowledge_or_fail_an_installation() {
        let mut harness = watch_harness(1);
        let runtime = RuntimeId::new(0);
        harness
            .controller
            .inferior
            .as_mut()
            .expect("inferior")
            .agents
            .insert(runtime, awaiting());
        assert!(
            !harness
                .controller
                .agent_report(runtime, 4, None)
                .expect("stale acknowledgement")
        );
        assert!(matches!(
            harness.controller.inferior.as_ref().unwrap().agents[&runtime].state,
            State::Awaiting { .. }
        ));
        assert!(
            harness
                .controller
                .agent_report(runtime, 5, None)
                .expect("matching acknowledgement")
        );
        assert!(matches!(
            harness.controller.inferior.as_ref().unwrap().agents[&runtime].state,
            State::Ready
        ));
        assert!(
            !harness
                .controller
                .agent_report(runtime, 5, None)
                .expect("duplicate acknowledgement")
        );
        let failure = Arc::from("scripted failure");
        assert!(
            !harness
                .controller
                .agent_report(runtime, 4, Some(&failure))
                .expect("stale failure")
        );
        assert!(matches!(
            harness.controller.inferior.as_ref().unwrap().agents[&runtime].state,
            State::Ready
        ));
        assert!(
            harness
                .controller
                .agent_report(runtime, 5, Some(&failure))
                .expect("matching failure")
        );
        assert!(
            matches!(&harness.controller.inferior.as_ref().unwrap().agents[&runtime].state, State::Refused(reason) if reason == &failure)
        );
    }

    #[derive(Debug)]
    struct Files {
        calls: Arc<Mutex<Vec<String>>>,
        fail_exit: bool,
    }
    impl AgentFiles for Files {
        fn library(&self) -> Result<&str, Arc<str>> {
            Ok("/tmp/test-agent/hook.so")
        }
        fn script(&mut self, generation: u64, _: &str) -> Result<String, Arc<str>> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("script {generation}"));
            Ok("/tmp/test-agent/agent.py".into())
        }
        fn anchor(&mut self, _: nix::unistd::Pid) -> Result<(), Arc<str>> {
            Ok(())
        }
        fn acknowledge(&mut self, generation: u64) -> Result<(), Arc<str>> {
            self.calls.lock().unwrap().push(format!("ack {generation}"));
            Ok(())
        }
        fn close(&mut self) -> Result<(), Arc<str>> {
            self.calls.lock().unwrap().push("close".into());
            Ok(())
        }
        fn close_after_target_exit(&mut self) -> Result<(), Arc<str>> {
            self.calls.lock().unwrap().push("exit".into());
            if self.fail_exit {
                Err("scripted cleanup failure".into())
            } else {
                Ok(())
            }
        }
    }
    #[derive(Debug)]
    struct Ticket {
        consumed: bool,
    }
    impl AgentRequestTicket for Ticket {
        fn retire(
            &self,
            _: &dyn crate::runtime_model::RuntimeStop,
            proof: AgentRetirementProof,
        ) -> Result<AgentRetirement, Arc<str>> {
            assert_eq!(proof, AgentRetirementProof::Cancel);
            Ok(if self.consumed {
                AgentRetirement::Unconfirmed
            } else {
                AgentRetirement::Retired(Vec::new())
            })
        }
    }

    #[test]
    fn terminal_exit_retires_unacknowledged_files_and_reports_cleanup_failures_at_shutdown() {
        for fail_exit in [false, true] {
            let mut harness = watch_harness(1);
            let calls = Arc::new(Mutex::new(Vec::new()));
            let mut installation = awaiting();
            installation.files = Some(Box::new(Files {
                calls: Arc::clone(&calls),
                fail_exit,
            }));
            let inferior = harness.controller.inferior.as_mut().expect("inferior");
            let pid = inferior.tgid;
            inferior.agents.insert(RuntimeId::new(0), installation);
            harness
                .controller
                .handle_terminal(pid, ExitStatus::Code(0))
                .expect("terminal exit");
            assert_eq!(*calls.lock().unwrap(), ["exit"]);
            assert!(harness.controller.inferior.is_none());
            let (reply, mut result) = tokio::sync::oneshot::channel();
            harness.controller.begin_shutdown(Some(reply));
            let outcome = result.try_recv().expect("shutdown completed");
            if fail_exit {
                assert!(
                    matches!(outcome, Err(crate::Error::RuntimeAgent(reason)) if reason.contains("cleanup failure"))
                );
            } else {
                assert!(outcome.is_ok());
            }
        }
    }

    #[test]
    fn detach_cancels_a_pending_source_but_preserves_unconfirmed_consumption() {
        for consumed in [false, true] {
            let mut harness = watch_harness(1);
            let image = Arc::clone(&harness.controller.module_image);
            harness
                .controller
                .runtime_models
                .borrow_mut()
                .insert(image.id(), vec![Ok(Arc::new(ScriptedRuntime::default()))]);
            let runtime = harness
                .controller
                .runtimes(harness.controller.inferior.as_ref().unwrap())[0]
                .id;
            let calls = Arc::new(Mutex::new(Vec::new()));
            let mut installation = awaiting();
            installation.state = State::Awaiting {
                generation: 5,
                source: true,
            };
            installation.ticket = Some(Arc::new(Ticket { consumed }));
            installation.files = Some(Box::new(Files {
                calls: Arc::clone(&calls),
                fail_exit: false,
            }));
            harness
                .controller
                .inferior
                .as_mut()
                .unwrap()
                .agents
                .insert(runtime, installation);
            let result = harness.controller.prepare_agents_for_detach();
            if consumed {
                assert!(
                    result
                        .expect_err("unconfirmed consumption")
                        .contains("unconfirmed")
                );
                assert!(
                    calls.lock().unwrap().is_empty(),
                    "files still needed by an executing handler must survive detach"
                );
            } else {
                result.expect("cancelled source");
                assert_eq!(*calls.lock().unwrap(), ["ack 5", "close"]);
            }
            assert_eq!(
                harness
                    .controller
                    .inferior
                    .as_ref()
                    .unwrap()
                    .public_stop
                    .as_ref()
                    .unwrap()
                    .reason,
                StopReason::Pause
            );
        }
    }
    #[derive(Debug)]
    struct UnresumedTicket {
        refuse: bool,
        calls: Arc<Mutex<Vec<String>>>,
    }
    impl AgentRequestTicket for UnresumedTicket {
        fn retire(
            &self,
            _: &dyn crate::runtime_model::RuntimeStop,
            proof: AgentRetirementProof,
        ) -> Result<AgentRetirement, Arc<str>> {
            assert_eq!(proof, AgentRetirementProof::Unresumed);
            self.calls.lock().unwrap().push("validate unresumed".into());
            if self.refuse {
                Err("foreign slot change".into())
            } else {
                Ok(AgentRetirement::Retired(Vec::new()))
            }
        }
    }

    #[derive(Debug)]
    struct Agent {
        thread: crate::ThreadId,
        refuse: bool,
        process_scope: bool,
        calls: Arc<Mutex<Vec<String>>>,
    }
    impl crate::runtime_model::RuntimeAgent for Agent {
        fn library(&self) -> Vec<u8> {
            Vec::new()
        }
        fn control_symbol(&self) -> &'static str {
            "control"
        }
        fn entry(&self) -> Option<crate::ImageAddress> {
            None
        }
        fn eligible_thread(
            &self,
            _: &dyn crate::runtime_model::RuntimeStop,
            _: Option<crate::ThreadId>,
        ) -> Result<crate::ThreadId, AgentFailure> {
            Ok(self.thread)
        }
        fn script(
            &self,
            _: &str,
            _: u64,
            _: TargetProcessId,
            _: &Arming<'_>,
        ) -> Result<String, Arc<str>> {
            Ok("source".into())
        }
        fn inject(
            &self,
            _: &dyn crate::runtime_model::RuntimeStop,
            _: &str,
            _: crate::ThreadId,
        ) -> Result<crate::runtime_model::PreparedAgentRequest, AgentFailure> {
            Ok(crate::runtime_model::PreparedAgentRequest {
                writes: [0x100, 0x108]
                    .map(|address| crate::runtime_model::RuntimeWrite {
                        address: crate::VirtualAddress::new(address),
                        expected: vec![0; 8],
                        bytes: vec![1; 8],
                    })
                    .into(),
                ticket: Arc::new(UnresumedTicket {
                    refuse: self.refuse,
                    calls: Arc::clone(&self.calls),
                }),
            })
        }
        fn command(
            &self,
            _: &dyn crate::runtime_model::RuntimeStop,
            _: crate::VirtualAddress,
            _: u64,
            _: u64,
            _: &Arming<'_>,
        ) -> Result<Vec<crate::runtime_model::RuntimeWrite>, Arc<str>> {
            Err("not scripted".into())
        }
        fn process_scope_required(&self) -> bool {
            self.process_scope
        }
    }

    fn scripted_agent(harness: &super::super::tests::WatchHarness, agent: Agent) -> BoundRuntime {
        let mut model = ScriptedRuntime::default();
        model.agent = Some(Arc::new(agent));
        let image = Arc::clone(&harness.controller.module_image);
        harness
            .controller
            .runtime_models
            .borrow_mut()
            .insert(image.id(), vec![Ok(Arc::new(model))]);
        harness
            .controller
            .runtimes(harness.controller.inferior.as_ref().unwrap())
            .remove(0)
    }

    fn function_arm(number: u64, name: &str) -> crate::runtime_model::ManagedArm {
        crate::runtime_model::ManagedArm {
            number,
            place: crate::ManagedPlace::Function { name: name.into() },
        }
    }

    #[test]
    fn a_reconfiguration_refused_before_any_write_keeps_the_acknowledged_configuration() {
        let mut harness = watch_harness(1);
        let calls = Arc::new(Mutex::new(Vec::new()));
        let thread =
            super::super::debug_thread_id(harness.controller.inferior.as_ref().unwrap().tgid);
        let bound = scripted_agent(
            &harness,
            Agent {
                thread,
                refuse: false,
                process_scope: false,
                calls: Arc::clone(&calls),
            },
        );
        let first = [function_arm(0, "first")];
        let both = [function_arm(0, "first"), function_arm(1, "second")];
        let mut installation = Installation::new();
        installation.state = State::Ready;
        installation.generation = Some(1);
        installation.next = 2;
        installation.sequence = 1;
        installation.library = Some("/tmp/test-agent/hook.so".into());
        installation.files = Some(Box::new(Files {
            calls: Arc::clone(&calls),
            fail_exit: false,
        }));
        installation.sent = Some(Desired {
            breakpoints: first.to_vec(),
            step: None,
        });
        harness
            .controller
            .inferior
            .as_mut()
            .unwrap()
            .agents
            .insert(bound.id, installation);
        let arming = |breakpoints| Arming {
            breakpoints,
            step: None,
            exceptions: crate::ExceptionStops::default(),
        };
        assert!(
            harness
                .controller
                .arm_agent(&bound, &arming(&both))
                .is_err(),
            "the scripted command cannot be prepared"
        );
        let installation = &harness.controller.inferior.as_ref().unwrap().agents[&bound.id];
        assert!(matches!(installation.state, State::Ready));
        assert_eq!(installation.sent.as_ref().unwrap().breakpoints, first);
        harness
            .controller
            .arm_agent(&bound, &arming(&first))
            .expect("the agent still holds its acknowledged configuration");
        assert!(
            calls.lock().unwrap().is_empty(),
            "no source request replaced the loaded agent"
        );
    }

    #[test]
    fn free_threaded_resume_is_restricted_only_while_an_agent_may_be_active() {
        let mut harness = watch_harness(2);
        let calls = Arc::new(Mutex::new(Vec::new()));
        let inferior = harness.controller.inferior.as_ref().unwrap();
        let first = super::super::debug_thread_id(inferior.tgid);
        let bound = scripted_agent(
            &harness,
            Agent {
                thread: first,
                refuse: false,
                process_scope: true,
                calls,
            },
        );
        let active = Some(Desired {
            breakpoints: vec![function_arm(0, "first")],
            step: None,
        });
        let idle = Some(Desired {
            breakpoints: Vec::new(),
            step: None,
        });
        for (state, sent, restricted) in [
            (State::Ready, active.clone(), true),
            (State::Ready, idle.clone(), false),
            (
                State::Awaiting {
                    generation: 5,
                    source: false,
                },
                idle,
                true,
            ),
            (State::Waiting, active.clone(), false),
            (State::Refused("refused".into()), active, false),
        ] {
            let mut installation = Installation::new();
            let shown = format!("{state:?} {sent:?}");
            installation.state = state;
            installation.sent = sent;
            harness
                .controller
                .inferior
                .as_mut()
                .unwrap()
                .agents
                .insert(bound.id, installation);
            assert_eq!(
                harness
                    .controller
                    .validate_agent_resume(crate::ResumeScope::Thread(first))
                    .is_err(),
                restricted,
                "{shown}"
            );
        }
    }

    #[test]
    fn partial_source_publication_revalidates_before_cleanup_and_retains_unproven_requests() {
        for refuse in [false, true] {
            let mut harness = watch_harness(1);
            let calls = Arc::new(Mutex::new(Vec::new()));
            let thread =
                super::super::debug_thread_id(harness.controller.inferior.as_ref().unwrap().tgid);
            let mut model = ScriptedRuntime::default();
            model.agent = Some(Arc::new(Agent {
                thread,
                refuse,
                process_scope: false,
                calls: Arc::clone(&calls),
            }));
            let image = Arc::clone(&harness.controller.module_image);
            harness
                .controller
                .runtime_models
                .borrow_mut()
                .insert(image.id(), vec![Ok(Arc::new(model))]);
            let bound = harness
                .controller
                .runtimes(harness.controller.inferior.as_ref().unwrap())
                .remove(0);
            let mut installation = Installation::new();
            installation.sequence = 7;
            installation.files = Some(Box::new(Files {
                calls: Arc::clone(&calls),
                fail_exit: false,
            }));
            // The first target word succeeds; the second fails operationally.
            *harness.controller.ptrace.memory_write_failure.borrow_mut() = Some(1);
            let arming = Arming {
                breakpoints: &[],
                step: None,
                exceptions: crate::ExceptionStops::default(),
            };
            let error = harness
                .controller
                .configure_agent(&bound, &arming, &mut installation)
                .expect_err("second publication write failed");
            assert!(
                error
                    .to_string()
                    .contains("after 1 of the runtime's 2 writes"),
                "{error}"
            );
            if refuse {
                assert!(error.to_string().contains("foreign slot change"));
                assert_eq!(*calls.lock().unwrap(), ["script 1", "validate unresumed"]);
                assert!(installation.ticket.is_some());
                assert!(installation.files.is_some());
                assert!(matches!(
                    installation.state,
                    State::Awaiting { source: true, .. }
                ));
                harness
                    .controller
                    .inferior
                    .as_mut()
                    .unwrap()
                    .agents
                    .insert(bound.id, installation);
                assert!(harness.controller.arm_agent(&bound, &arming).is_err());
                assert_eq!(
                    *calls.lock().unwrap(),
                    ["script 1", "validate unresumed"],
                    "an outstanding failed request cannot publish or allocate another source"
                );
            } else {
                assert_eq!(
                    *calls.lock().unwrap(),
                    ["script 1", "validate unresumed", "ack 1", "close"]
                );
                assert!(installation.ticket.is_none());
                assert!(installation.files.is_none());
                assert_eq!(
                    installation.sequence, 0,
                    "a replacement library's command slot starts unpublished"
                );
                assert!(matches!(installation.state, State::Refused(_)));
            }
        }
    }
    #[test]
    fn internal_stops_do_not_spend_the_acknowledgement_budget() {
        let mut harness = watch_harness(1);
        harness
            .controller
            .inferior
            .as_mut()
            .unwrap()
            .agents
            .insert(RuntimeId::new(0), awaiting());
        harness.start_continue();
        let installation = |harness: &super::super::tests::WatchHarness| {
            harness.controller.inferior.as_ref().unwrap().agents[&RuntimeId::new(0)].deadline()
        };
        let started = Instant::now();
        harness
            .controller
            .inferior
            .as_mut()
            .unwrap()
            .agents
            .get_mut(&RuntimeId::new(0))
            .unwrap()
            .start_deadline(started);
        harness.controller.begin_internal_stop().expect("stopping");
        assert_eq!(installation(&harness), None, "paused while stopped");
        harness.settle_requested_stops();
        assert!(
            harness
                .controller
                .inferior
                .as_ref()
                .unwrap()
                .barrier
                .is_none()
        );
        assert!(
            installation(&harness).is_some(),
            "running again after the internal stop"
        );
    }

    #[test]
    fn interrupted_running_periods_share_one_acknowledgement_budget() {
        let now = Instant::now();
        let mut installation = awaiting();
        installation.deadline = Some(now + Duration::from_secs(2));
        installation.pause_deadline(now);
        let later = now + Duration::from_secs(3600);
        installation.start_deadline(later);
        assert_eq!(
            installation.deadline(),
            Some(later + Duration::from_secs(2))
        );
    }

    #[test]
    fn a_source_request_cannot_resume_only_a_different_gil_thread() {
        let mut harness = watch_harness(2);
        let calls = Arc::new(Mutex::new(Vec::new()));
        let inferior = harness.controller.inferior.as_ref().unwrap();
        let process = super::super::process_id(inferior.tgid);
        let stop = inferior.public_stop.as_ref().unwrap().id;
        let first = super::super::debug_thread_id(inferior.tgid);
        let second = super::super::debug_thread_id(*inferior.threads.keys().last().unwrap());
        let mut model = ScriptedRuntime::default();
        model.agent = Some(Arc::new(Agent {
            thread: second,
            refuse: false,
            process_scope: false,
            calls,
        }));
        let image = Arc::clone(&harness.controller.module_image);
        harness
            .controller
            .runtime_models
            .borrow_mut()
            .insert(image.id(), vec![Ok(Arc::new(model))]);
        let runtime = harness
            .controller
            .runtimes(harness.controller.inferior.as_ref().unwrap())[0]
            .id;
        let mut installation = awaiting();
        installation.state = State::Awaiting {
            generation: 5,
            source: true,
        };
        installation.request_thread = Some(second);
        harness
            .controller
            .inferior
            .as_mut()
            .unwrap()
            .agents
            .insert(runtime, installation);
        let result = harness.controller.begin_execution(
            process,
            stop,
            crate::ResumeScope::Thread(first),
            super::super::ActiveKind::Continue,
            crate::ExceptionDisposition::Pass,
        );
        assert!(
            matches!(result, Err(crate::Error::RuntimeResumeUnsupported(_))),
            "{result:?}"
        );
        assert_eq!(
            harness
                .controller
                .inferior
                .as_ref()
                .unwrap()
                .public_stop
                .as_ref()
                .unwrap()
                .id,
            stop
        );
        harness
            .controller
            .validate_agent_resume(crate::ResumeScope::Thread(second))
            .expect("the injection thread may consume its own GIL request");
    }

    #[test]
    fn failed_native_publication_does_not_invent_an_outstanding_sequence() {
        for successful_words in 0..=3 {
            let mut harness = watch_harness(1);
            let image = Arc::clone(&harness.controller.module_image);
            harness
                .controller
                .runtime_models
                .borrow_mut()
                .insert(image.id(), vec![Ok(Arc::new(ScriptedRuntime::default()))]);
            let bound = harness
                .controller
                .runtimes(harness.controller.inferior.as_ref().unwrap())
                .remove(0);
            let mut installation = Installation::new();
            installation.state = State::Ready;
            installation.generation = Some(1);
            installation.next = 2;
            *harness.controller.ptrace.memory_write_failure.borrow_mut() = Some(successful_words);
            let writes = [0x100, 0x108, 0x110].map(|address| crate::runtime_model::RuntimeWrite {
                address: crate::VirtualAddress::new(address),
                expected: vec![0; 8],
                bytes: vec![2; 8],
            });
            let result =
                harness
                    .controller
                    .publish_agent_command(&bound, &mut installation, 2, 3, &writes);
            if successful_words == 3 {
                result.expect("complete publication");
                assert_eq!(installation.sequence, 2);
                assert_eq!(installation.generation, Some(2));
                assert!(matches!(
                    installation.state,
                    State::Awaiting {
                        source: false,
                        generation: 2
                    }
                ));
                continue;
            }
            assert!(result.is_err());
            assert_eq!(
                installation.sequence, 0,
                "failed after {successful_words} writes"
            );
            assert_eq!(installation.generation, Some(1));
            assert!(
                matches!(installation.state, State::Ready),
                "the agent keeps its acknowledged configuration"
            );
        }
    }
    #[test]
    fn uncertain_native_publication_adopts_its_sequence_only_after_an_owned_acknowledgement() {
        let mut harness = watch_harness(1);
        let runtime = RuntimeId::new(0);
        // A failed write/read left publication unconfirmed, preserving the
        // previous proven native sequence while waiting for generation 5.
        let installation = awaiting();
        assert_eq!(installation.sequence, 0);
        harness
            .controller
            .inferior
            .as_mut()
            .unwrap()
            .agents
            .insert(runtime, installation);
        assert!(!harness.controller.agent_report(runtime, 4, None).unwrap());
        assert_eq!(
            harness.controller.inferior.as_ref().unwrap().agents[&runtime].sequence,
            0
        );
        assert!(harness.controller.agent_report(runtime, 5, None).unwrap());
        assert_eq!(
            harness.controller.inferior.as_ref().unwrap().agents[&runtime].sequence,
            5
        );
    }
}
