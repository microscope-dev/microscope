use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::{Duration, Instant};

use nix::unistd::Pid;

use super::{InspectorMessage, PauseRequest, send};
use crate::backend::linux::cdp::{Incoming, RequestId, Session};
use crate::backend::linux::native::LinuxTraceOps;
use crate::backend::linux::node_launch::Ownership;
use crate::backend::linux::{ActiveExecution, ActiveKind, BreakpointOwner, Controller, Edit};
use crate::runtime_model::inspector_protocol;
use crate::{Error, ExecutionId, Result, ResumeScope, RuntimeId, VirtualAddress};

#[derive(Debug)]
pub(in crate::backend::linux) struct Cleanup {
    runtime: RuntimeId,
    generation: u64,
    identity: Option<(VirtualAddress, Arc<str>)>,
    dispatch: Option<VirtualAddress>,
    return_hook: Option<VirtualAddress>,
    stop: Option<VirtualAddress>,
    message: Vec<u8>,
    deadline: Instant,
    phase: Phase,
}

#[derive(Debug, Clone)]
enum Phase {
    Starting,
    Quiescing,
    Waiting,
    Borrowed(RequestId),
    Captured {
        pid: Pid,
        stack: u64,
        returns_to: VirtualAddress,
    },
    Returning {
        pid: Pid,
        stack: u64,
        returns_to: VirtualAddress,
    },
    Done,
    Failed(Arc<str>),
}

impl Cleanup {
    const fn starting(runtime: RuntimeId, deadline: Instant) -> Self {
        Self {
            runtime,
            generation: 0,
            identity: None,
            dispatch: None,
            return_hook: None,
            stop: None,
            message: Vec::new(),
            deadline,
            phase: Phase::Starting,
        }
    }

    pub(super) fn sites(&self) -> Vec<VirtualAddress> {
        self.dispatch.into_iter().chain(self.return_hook).collect()
    }

    pub(in crate::backend::linux) fn deadline(&self) -> Option<Instant> {
        (!matches!(self.phase, Phase::Done | Phase::Failed(_))).then_some(self.deadline)
    }
}

impl<P: LinuxTraceOps> Controller<P> {
    pub(in crate::backend::linux) fn inspector_cleanup_running(&self) -> bool {
        self.inferior
            .as_ref()
            .is_some_and(|inferior| inferior.inspectors.cleanup.is_some())
    }

    pub(super) fn inspector_cleanup_starting(&self) -> bool {
        self.inferior
            .as_ref()
            .and_then(|inferior| inferior.inspectors.cleanup.as_ref())
            .is_some_and(|cleanup| matches!(cleanup.phase, Phase::Starting))
    }

    pub(super) fn capture_inspector_ownership(&mut self) {
        let Some(inferior) = self.inferior.as_ref() else {
            return;
        };
        let runtimes = self.runtimes(inferior);
        for bound in runtimes {
            let Some(inferior) = self.inferior.as_ref() else {
                return;
            };
            let Some(inspector) = inferior.inspectors.records.get(&bound.id) else {
                continue;
            };
            if inspector.ownership == Ownership::Borrowed || inspector.owned_agent.is_some() {
                continue;
            }
            let Some(target) = inspector.target.clone() else {
                continue;
            };
            let created = inspector.ownership == Ownership::Created;
            let launched = inferior.origin != crate::backend::linux::InferiorOrigin::Attached;
            if !created && launched {
                continue;
            }
            let identity =
                self.with_runtime_stop(inferior, &bound, inferior.memory_thread(), |stop| {
                    bound.model.inspector_agent(stop)
                });
            let Ok(Some((agent, id))) = identity else {
                continue;
            };
            if id != target {
                continue;
            }
            let ownership = if created {
                Ownership::Created
            } else {
                Ownership::Borrowed
            };
            if let Some(inspector) = self
                .inferior
                .as_mut()
                .and_then(|inferior| inferior.inspectors.records.get_mut(&bound.id))
            {
                inspector.ownership = ownership;
                if ownership == Ownership::Created {
                    inspector.owned_agent = Some(agent);
                }
            }
        }
    }

    pub(in crate::backend::linux) fn begin_inspector_cleanup(&mut self) -> Result<bool> {
        if self.inspector_cleanup_running() && !self.inspector_cleanup_starting() {
            return Ok(true);
        }
        self.capture_inspector_ownership();
        let inferior = self.inferior.as_ref().ok_or(Error::NotRunning)?;
        let deadline = inferior.inspectors.cleanup.as_ref().map_or_else(
            || Instant::now() + crate::SHUTDOWN_TIMEOUT.saturating_sub(Duration::from_secs(1)),
            |cleanup| cleanup.deadline,
        );
        let owned = inferior
            .inspectors
            .records
            .iter()
            .filter(|(_, inspector)| {
                inspector.ownership == Ownership::Created || inspector.connection.is_some()
            })
            .map(|(id, _)| *id)
            .collect::<Vec<_>>();
        if owned.is_empty() {
            return Ok(false);
        }
        if owned.len() != 1 {
            return Err(Error::RuntimeAgent(
                "cannot close multiple created Node inspectors safely".into(),
            ));
        }
        let runtime = owned[0];
        let bound = self
            .runtimes(inferior)
            .into_iter()
            .find(|bound| bound.id == runtime)
            .ok_or_else(|| Error::RuntimeAgent("the created inspector's image is gone".into()))?;
        let inspector = inferior
            .inspectors
            .records
            .get(&runtime)
            .ok_or(Error::NotRunning)?;
        let created = inspector.ownership == Ownership::Created;
        let ready = inspector
            .target
            .clone()
            .zip(inspector.connection.as_ref())
            .filter(|(_, link)| {
                link.connected
                    && inspector.ownership != Ownership::Pending
                    && (!created || inspector.owned_agent.is_some())
            });
        let Some((target, link)) = ready else {
            // SIGUSR1 has already committed Node to opening a server. Run
            // its pending callback before closing it; detaching now leaks it.
            return self.start_inspector_cleanup(Cleanup::starting(runtime, deadline));
        };
        let generation = link.generation;
        let (dispatch, stop) = if created {
            let (dispatch, stop) = bound.model.inspector_close_functions().ok_or_else(|| {
                Error::RuntimeAgent(
                    "Node's native inspector close functions are unavailable".into(),
                )
            })?;
            (
                Some(bound.module.virtual_address(dispatch)?),
                Some(bound.module.virtual_address(stop)?),
            )
        } else {
            (None, None)
        };
        let identity = self
            .with_runtime_stop(inferior, &bound, inferior.memory_thread(), |stop| {
                bound.model.inspector_agent(stop)
            })
            .map_err(Error::RuntimeAgent)?;
        let (agent, id) = identity.ok_or_else(|| {
            Error::RuntimeAgent("the inspector server is gone before detach".into())
        })?;
        if id != target || (created && inspector.owned_agent != Some(agent)) {
            return Err(Error::RuntimeAgent(
                "the created inspector's agent or target changed before detach".into(),
            ));
        }
        self.start_inspector_cleanup(Cleanup {
            generation,
            identity: Some((agent, target)),
            dispatch,
            stop,
            phase: Phase::Waiting,
            ..Cleanup::starting(runtime, deadline)
        })
    }

    fn start_inspector_cleanup(&mut self, mut cleanup: Cleanup) -> Result<bool> {
        self.end_runtime_step();
        self.remove_all_breakpoints()?;
        self.remove_all_watchpoints()?;
        self.set_exception_stops(crate::ExceptionStops::default())?;
        let inferior = self.inferior.as_mut().ok_or(Error::NotRunning)?;
        if cleanup.stop.is_some()
            && let Some(startup) = inferior.inspectors.startup.get_mut(&cleanup.runtime)
        {
            startup.begin_drain();
            cleanup.phase = Phase::Quiescing;
        }
        let inspector = inferior
            .inspectors
            .records
            .get_mut(&cleanup.runtime)
            .ok_or(Error::NotRunning)?;
        inspector.desired.clear();
        inspector.step = None;
        inspector.pausing = "none";
        if !matches!(cleanup.phase, Phase::Starting) {
            Self::release_for_cleanup(inspector)?;
            let link = inspector.connection.as_ref().ok_or(Error::NotRunning)?;
            let mut nonce = [0; 16];
            getrandom::fill(&mut nonce).map_err(|error| {
                Error::RuntimeAgent(format!("cannot identify inspector cleanup: {error}").into())
            })?;
            let params = serde_json::json!({"microscopeCleanup": nonce});
            let id = send(
                &link.connection,
                &Session::main(),
                "Runtime.getIsolateId",
                params.clone(),
            )
            .map_err(|error| Error::RuntimeAgent(error.to_string().into()))?;
            cleanup.message = serde_json::json!({"id": id.number(), "method": "Runtime.getIsolateId", "params": params}).to_string().into_bytes();
            if let Some(dispatch) = cleanup.dispatch {
                self.ptrace.install_breakpoint(
                    inferior.memory_thread(),
                    &mut inferior.breakpoints,
                    dispatch,
                    BreakpointOwner::InspectorHook,
                )?;
            }
            if cleanup.stop.is_none() {
                cleanup.phase = Phase::Borrowed(id);
            }
        }
        inferior.inspectors.cleanup = Some(cleanup);
        inferior.inspectors.end_interrupt();
        inferior.public_stop = None;
        inferior.barrier = None;
        inferior.next_execution += 1;
        inferior.active = Some(ActiveExecution {
            id: ExecutionId::new(inferior.next_execution),
            kind: ActiveKind::Continue,
            scope: ResumeScope::Process(crate::backend::linux::process_id(inferior.tgid)),
            resume_threads: inferior.threads.keys().copied().collect::<BTreeSet<_>>(),
        });
        for thread in inferior.threads.values_mut() {
            thread.reason = None;
        }
        inferior.repairs = crate::backend::linux::run_control::collect_repairs(inferior);
        self.install_inspector_startup_hooks()?;
        self.advance_execution()?;
        Ok(true)
    }

    fn release_for_cleanup(inspector: &mut super::Inspector) -> Result<()> {
        let link = inspector.connection.as_ref().ok_or(Error::NotRunning)?;
        for (session, isolate) in &mut inspector.isolates {
            for (method, params) in [
                (
                    "Debugger.setBreakpointsActive",
                    serde_json::json!({"active": false}),
                ),
                (
                    "Debugger.setPauseOnExceptions",
                    serde_json::json!({"state": "none"}),
                ),
                (
                    "Debugger.setSkipAllPauses",
                    serde_json::json!({"skip": true}),
                ),
                ("Debugger.resume", serde_json::json!({})),
                ("Runtime.runIfWaitingForDebugger", serde_json::json!({})),
            ] {
                send(&link.connection, session, method, params)
                    .map_err(|error| Error::RuntimeAgent(error.to_string().into()))?;
            }
            isolate.paused = None;
            isolate.deferred = None;
            isolate.pause = PauseRequest::None;
            isolate.resuming = false;
        }
        Ok(())
    }

    pub(super) fn note_inspector_cleanup_hook(
        &mut self,
        pid: Pid,
        address: VirtualAddress,
    ) -> Result<bool> {
        let Some(inferior) = self.inferior.as_ref() else {
            return Ok(false);
        };
        let Some(cleanup) = inferior.inspectors.cleanup.as_ref() else {
            return Ok(false);
        };
        let registers = self.ptrace.registers(pid)?;
        let phase = match &cleanup.phase {
            Phase::Waiting if Some(address) == cleanup.dispatch => {
                let bound = self
                    .runtimes(inferior)
                    .into_iter()
                    .find(|bound| bound.id == cleanup.runtime)
                    .ok_or(Error::NotRunning)?;
                let matches = self
                    .with_runtime_stop(inferior, &bound, pid, |stop| {
                        bound.model.inspector_message_matches(
                            stop,
                            VirtualAddress::new(registers.rsi),
                            &cleanup.message,
                        )
                    })
                    .map_err(Error::RuntimeAgent)?;
                if !matches {
                    return Ok(false);
                }
                Phase::Captured {
                    pid,
                    stack: registers.rsp,
                    returns_to: VirtualAddress::new(self.ptrace.read_word(pid, registers.rsp)?),
                }
            }
            Phase::Returning {
                pid: owner,
                stack,
                returns_to,
            } if pid == *owner
                && address == *returns_to
                && registers.rsp == stack.wrapping_add(8) =>
            {
                Phase::Done
            }
            _ => return Ok(false),
        };
        if let Some(cleanup) = self
            .inferior
            .as_mut()
            .and_then(|inferior| inferior.inspectors.cleanup.as_mut())
        {
            cleanup.phase = phase;
        }
        self.queue_internal_edit(Edit::InspectorCleanup)?;
        Ok(true)
    }

    pub(in crate::backend::linux) fn apply_inspector_cleanup(&mut self) {
        if let Err(error) = self
            .progress_inspector_quiescence()
            .and_then(|()| self.redirect_inspector_cleanup())
        {
            self.fail_inspector_cleanup(error.to_string().into());
        }
    }

    fn progress_inspector_quiescence(&mut self) -> Result<()> {
        self.install_inspector_startup_hooks()?;
        let Some(cleanup) = self
            .inferior
            .as_ref()
            .and_then(|inferior| inferior.inspectors.cleanup.as_ref())
        else {
            return Ok(());
        };
        if !matches!(cleanup.phase, Phase::Quiescing) {
            return Ok(());
        }
        let runtime = cleanup.runtime;
        let agent = cleanup.identity.as_ref().ok_or(Error::NotRunning)?.0;
        if !self.verify_inspector_startup(runtime, agent)? {
            return Ok(());
        }
        let inferior = self.inferior.as_mut().ok_or(Error::NotRunning)?;
        let inspector = inferior
            .inspectors
            .records
            .get(&runtime)
            .ok_or(Error::NotRunning)?;
        let link = inspector.connection.as_ref().ok_or(Error::NotRunning)?;
        let cleanup = inferior
            .inspectors
            .cleanup
            .as_mut()
            .ok_or(Error::NotRunning)?;
        let mut nonce = [0; 16];
        getrandom::fill(&mut nonce)
            .map_err(|error| Error::RuntimeAgent(error.to_string().into()))?;
        let params = serde_json::json!({"microscopeCleanup": nonce});
        let id = send(
            &link.connection,
            &Session::main(),
            "Runtime.getIsolateId",
            params.clone(),
        )
        .map_err(|error| Error::RuntimeAgent(error.to_string().into()))?;
        cleanup.message = serde_json::json!({"id": id.number(), "method": "Runtime.getIsolateId", "params": params}).to_string().into_bytes();
        cleanup.phase = Phase::Waiting;
        Ok(())
    }

    fn redirect_inspector_cleanup(&mut self) -> Result<()> {
        let inferior = self.inferior.as_ref().ok_or(Error::NotRunning)?;
        let Some(cleanup) = inferior.inspectors.cleanup.as_ref() else {
            return Ok(());
        };
        let Phase::Captured {
            pid,
            stack,
            returns_to,
        } = cleanup.phase
        else {
            return Ok(());
        };
        if !self.inspector_startup_verified(cleanup.runtime) {
            return Err(Error::RuntimeAgent(
                "Node's inspector startup postings changed before native close".into(),
            ));
        }
        let bound = self
            .runtimes(inferior)
            .into_iter()
            .find(|bound| bound.id == cleanup.runtime)
            .ok_or(Error::NotRunning)?;
        let identity = self
            .with_runtime_stop(inferior, &bound, pid, |stop| {
                bound.model.inspector_agent(stop)
            })
            .map_err(Error::RuntimeAgent)?;
        if identity != cleanup.identity {
            return Err(Error::RuntimeAgent(
                "the created inspector's identity changed at native close dispatch".into(),
            ));
        }
        let mut registers = self.ptrace.registers(pid)?;
        if Some(VirtualAddress::new(registers.rip)) != cleanup.dispatch || registers.rsp != stack {
            return Err(Error::RuntimeAgent(
                "the inspector cleanup dispatch moved before all-stop".into(),
            ));
        }
        let agent = cleanup
            .identity
            .as_ref()
            .ok_or_else(|| Error::internal("a captured cleanup has an inspector identity"))?
            .0;
        let stop = cleanup
            .stop
            .ok_or_else(|| Error::internal("a captured cleanup has a native Stop"))?;
        let inferior = self.inferior.as_mut().ok_or(Error::NotRunning)?;
        self.ptrace.install_breakpoint(
            pid,
            &mut inferior.breakpoints,
            returns_to,
            BreakpointOwner::InspectorHook,
        )?;
        if let Some(cleanup) = inferior.inspectors.cleanup.as_mut() {
            cleanup.return_hook = Some(returns_to);
        }
        // Both functions return void. Tail-calling at Dispatch's entry retains
        // its caller's stack and callee-saved ABI, without running a JS getter.
        registers.rip = stop.get();
        registers.rdi = agent.get();
        self.ptrace.set_registers(pid, registers)?;
        inferior.thread_mut(pid)?.stopped_at_breakpoint = None;
        if let Some(cleanup) = inferior.inspectors.cleanup.as_mut() {
            cleanup.phase = Phase::Returning {
                pid,
                stack,
                returns_to,
            };
        }
        Ok(())
    }

    pub(in crate::backend::linux) fn inspector_cleanup_barrier(&mut self) -> Result<bool> {
        if self.inspector_cleanup_starting() {
            match self.begin_inspector_cleanup() {
                Ok(true) => {}
                Ok(false) => self.detach_inferior(),
                Err(error) => self.detach_inferior_reporting(Some(error)),
            }
            return Ok(true);
        }
        let Some(inferior) = self.inferior.as_ref() else {
            return Ok(false);
        };
        let Some(cleanup) = inferior.inspectors.cleanup.as_ref() else {
            return Ok(false);
        };
        let problem = match &cleanup.phase {
            Phase::Done => {
                let bound = self
                    .runtimes(inferior)
                    .into_iter()
                    .find(|bound| bound.id == cleanup.runtime)
                    .ok_or(Error::NotRunning)?;
                match self.with_runtime_stop(inferior, &bound, inferior.memory_thread(), |stop| {
                    bound.model.inspector_agent(stop)
                }) {
                    Ok(Some(identity))
                        if cleanup.stop.is_none()
                            && Some(&identity) == cleanup.identity.as_ref() =>
                    {
                        None
                    }
                    Ok(None) if cleanup.stop.is_some() => None,
                    Ok(None) => Some(Arc::from("the borrowed inspector vanished during detach")),
                    Ok(Some(_)) => Some(Arc::from(
                        "Node's native Stop returned with an inspector still open",
                    )),
                    Err(reason) => Some(reason),
                }
            }
            Phase::Failed(reason) => Some(Arc::clone(reason)),
            _ => {
                if let Some(barrier) = self
                    .inferior
                    .as_mut()
                    .and_then(|inferior| inferior.barrier.as_mut())
                {
                    barrier.reason = None;
                }
                return Ok(false);
            }
        };
        self.detach_inferior_reporting(problem.map(Error::RuntimeAgent));
        Ok(true)
    }

    fn fail_inspector_cleanup(&mut self, reason: Arc<str>) {
        record!("native inspector cleanup failed: {reason}");
        if let Some(cleanup) = self
            .inferior
            .as_mut()
            .and_then(|inferior| inferior.inspectors.cleanup.as_mut())
        {
            cleanup.phase = Phase::Failed(reason);
        }
    }

    pub(in crate::backend::linux) fn inspector_cleanup_timeout(
        &mut self,
        now: Instant,
    ) -> Result<()> {
        let expired = self
            .inferior
            .as_ref()
            .and_then(|inferior| inferior.inspectors.cleanup.as_ref())
            .and_then(Cleanup::deadline)
            .is_some_and(|deadline| deadline <= now);
        if expired {
            self.fail_inspector_cleanup(
                "Node's native inspector cleanup exceeded its deadline".into(),
            );
            self.queue_internal_edit(Edit::InspectorCleanup)?;
            self.finish_barrier_if_ready()?;
        }
        Ok(())
    }

    pub(super) fn inspector_cleanup_message(&mut self, message: InspectorMessage) -> Result<()> {
        let Some(inferior) = self.inferior.as_mut() else {
            return Ok(());
        };
        let Some(cleanup) = inferior.inspectors.cleanup.as_ref() else {
            return Ok(());
        };
        if message.runtime != cleanup.runtime || message.generation != cleanup.generation {
            return Ok(());
        }
        let returning = matches!(cleanup.phase, Phase::Returning { .. } | Phase::Done);
        let borrowed_reply = matches!(&message.incoming, Incoming::Reply { id, .. } if matches!(cleanup.phase, Phase::Borrowed(request) if request == *id));
        let Some(inspector) = inferior.inspectors.records.get_mut(&message.runtime) else {
            return Ok(());
        };
        match message.incoming {
            Incoming::Reply { result, .. } if borrowed_reply => {
                if let Some(cleanup) = inferior.inspectors.cleanup.as_mut() {
                    cleanup.phase = match result {
                        Ok(_) => Phase::Done,
                        Err(reason) => Phase::Failed(
                            format!("the borrowed inspector did not acknowledge detach: {reason}")
                                .into(),
                        ),
                    };
                }
                self.queue_internal_edit(Edit::InspectorCleanup)?;
                self.finish_barrier_if_ready()?;
            }
            Incoming::Event {
                session,
                method,
                params,
            } => {
                if let Some(link) = inspector.connection.as_ref() {
                    if method.as_ref() == inspector_protocol::PAUSED {
                        let _ = send(
                            &link.connection,
                            &session,
                            inspector_protocol::RESUME,
                            serde_json::json!({}),
                        );
                    } else if method.as_ref() == inspector_protocol::WORKER_ATTACHED
                        && let Some(id) = params["sessionId"].as_str()
                    {
                        let worker = session.worker(id);
                        let _ = send(
                            &link.connection,
                            &worker,
                            "Debugger.setSkipAllPauses",
                            serde_json::json!({"skip": true}),
                        );
                        let _ = send(
                            &link.connection,
                            &worker,
                            inspector_protocol::RELEASE,
                            serde_json::json!({}),
                        );
                    }
                }
            }
            Incoming::Closed { reason } => {
                inspector.connection = None;
                inspector.isolates.clear();
                if !returning {
                    self.fail_inspector_cleanup(
                        format!("the inspector disconnected before native cleanup: {reason}")
                            .into(),
                    );
                    self.queue_internal_edit(Edit::InspectorCleanup)?;
                    self.finish_barrier_if_ready()?;
                }
            }
            _ => {}
        }
        Ok(())
    }
}
