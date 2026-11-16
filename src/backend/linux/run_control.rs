use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use super::signals::Signal;
use nix::unistd::Pid;

use crate::protocol::{
    DebuggerEvent, ExceptionDisposition, ExecutionId, ProcessId, Reply, ResumeScope, SignalPolicy,
    StepKind, StopId, StopReason, WatchpointId,
};

use crate::{Error, Result, StackFrameId, VirtualAddress};

use super::awaits::Followed;
use super::breakpoints::{
    install_plan_breakpoint, remove_breakpoint_owner_from, runtime_breakpoint_address,
};

use super::classify::format_raw_stop;
use super::loops::LoopReach;
use super::native::{LinuxTraceOps, is_vanished_tracee};
use super::new_task::NewTask;
use super::trap::{AtTrap, Lift, Trap, TrapEvent};
use super::{
    ActiveExecution, ActiveKind, BreakpointOwner, ClassifiedStop, Controller, Edit, ExpectedStop,
    Inferior, LinuxError, NativeThreadState, PendingSignal, PublicStop, Resume, ResumeGuard,
    StepOwner, StopBarrier, backend_error, debug_thread_id, exception_info, pending_exception_info,
    process_id, scoped_threads, steps_instructions, validate_process, validate_public_stop,
    validate_resumable, validate_stopped_thread,
};

impl<P: LinuxTraceOps> Controller<P> {
    pub(super) fn resume(
        &mut self,
        process_id: ProcessId,
        stop_id: StopId,
        scope: ResumeScope,
        exception: ExceptionDisposition,
        reply: Reply<ExecutionId>,
    ) {
        let result =
            self.begin_execution(process_id, stop_id, scope, ActiveKind::Continue, exception);
        self.reply_execution(result, scope, reply);
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "a step request names its stop, thread, frame, kind, scope, and exception disposition"
    )]
    pub(super) fn step(
        &mut self,
        process_id: ProcessId,
        stop_id: StopId,
        pid: Pid,
        frame: StackFrameId,
        (kind, call): (StepKind, Option<VirtualAddress>),
        scope: ResumeScope,
        exception: ExceptionDisposition,
        reply: Reply<ExecutionId>,
    ) {
        let valid = match (kind, call) {
            (StepKind::Advance, _) => Err(Error::AdvanceWithoutLocation),
            (StepKind::IntoSource, _) | (_, None) => {
                self.validate_step(process_id, stop_id, pid, frame, kind, scope)
            }
            (_, Some(call)) => Err(Error::NotAStepTarget(call)),
        }
        .and_then(|()| {
            call.map(|call| self.chosen_call(stop_id, pid, call))
                .transpose()
        });
        let into_call = match valid {
            Ok(into_call) => into_call,
            Err(error) => {
                let _ = reply.send(Err(error));
                return;
            }
        };

        let frame = match kind {
            StepKind::IntoSource | StepKind::OverSource | StepKind::Out
                if frame == StackFrameId::INNERMOST && into_call.is_none() =>
            {
                match self.reported_frame(stop_id, pid) {
                    Some(blamed) => blamed,
                    None if self.agent_hook(pid).is_some() => {
                        let _ = reply.send(Err(Error::FrameStepUnsupported(
                            "the agent's stop names no frame of the program to step from".into(),
                        )));
                        return;
                    }
                    None => frame,
                }
            }
            _ => frame,
        };
        if into_call.is_none()
            && let Some(interpreted) = self.runtime_step_origin(stop_id, pid, frame, kind)
        {
            let result = self.begin_runtime_step(
                process_id,
                stop_id,
                pid,
                interpreted,
                kind,
                scope,
                exception,
            );
            self.reply_execution(result, scope, reply);
            return;
        }

        let virtual_step = if into_call.is_some() {
            Ok(None)
        } else {
            self.try_virtual_step(process_id, stop_id, pid, kind)
        };

        match virtual_step {
            Ok(Some((execution, stopped))) => {
                let _ = reply.send(Ok(execution));
                let _ = self.events.send(stopped);
                return;
            }
            Err(error) => {
                let _ = reply.send(Err(error));
                return;
            }
            Ok(None) => {}
        }

        let task = self.step_task(pid);

        let (requested, kind) = (
            kind,
            match kind {
                StepKind::IntoNewTask => StepKind::OverSource,
                kind => kind,
            },
        );
        let result = self.step_start(pid, kind, frame).and_then(|mut start| {
            if requested == StepKind::IntoNewTask {
                start.new_task = Some(NewTask::Watching(self.task_starters()));
            }

            if frame.get() == 0 {
                start.awaiting = self.await_step(pid, kind, &start);
            }

            start.into_call = into_call;
            self.begin_execution(
                process_id,
                stop_id,
                scope,
                ActiveKind::Step {
                    owner: StepOwner { thread: pid, task },
                    kind,
                    requested,
                    start: Box::new(start),
                    progress_owed: false,
                },
                exception,
            )
        });
        self.reply_execution(result, scope, reply);
    }

    fn validate_step(
        &self,
        process_id: ProcessId,
        stop_id: StopId,
        pid: Pid,
        frame: StackFrameId,
        kind: StepKind,
        scope: ResumeScope,
    ) -> Result<()> {
        let inferior = self.session.inferior().ok_or(Error::NotRunning)?;
        validate_process(inferior, process_id)?;
        validate_public_stop(inferior, Some(stop_id))?;
        validate_resumable(inferior)?;
        validate_stopped_thread(inferior, pid)?;
        if let ResumeScope::Thread(resumed) = scope
            && resumed != debug_thread_id(pid)
        {
            return Err(Error::StepScopeMismatch {
                stepping: debug_thread_id(pid),
                resumed,
            });
        }

        if frame.get() != 0
            && kind != StepKind::Out
            && self.interpreted_frame(stop_id, pid, frame).is_none()
        {
            return Err(Error::FrameStepUnsupported(
                "only stepping out applies to an outer frame; other steps begin at the innermost frame"
                    .into(),
            ));
        }

        Ok(())
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "an advance names its stop, thread, frame, location, scope, and exception disposition"
    )]
    pub(super) fn advance(
        &mut self,
        process_id: ProcessId,
        stop_id: StopId,
        pid: Pid,
        frame: StackFrameId,
        spec: crate::BreakpointSpec,
        scope: ResumeScope,
        exception: ExceptionDisposition,
        reply: Reply<ExecutionId>,
    ) {
        let result = self
            .validate_step(process_id, stop_id, pid, frame, StepKind::Out, scope)
            .and_then(|()| self.advance_targets(spec))
            .and_then(|targets| {
                let mut start = self.step_start(pid, StepKind::Out, frame)?;
                start.targets = targets;

                if frame.get() == 0 {
                    start.awaiting = self.await_step(pid, StepKind::Out, &start);
                }

                self.begin_execution(
                    process_id,
                    stop_id,
                    scope,
                    ActiveKind::Step {
                        owner: StepOwner {
                            thread: pid,
                            task: self.step_task(pid),
                        },
                        kind: StepKind::Out,
                        requested: StepKind::Out,
                        start: Box::new(start),
                        progress_owed: false,
                    },
                    exception,
                )
            });
        self.reply_execution(result, scope, reply);
    }

    pub(super) fn advance_targets(
        &self,
        spec: crate::BreakpointSpec,
    ) -> Result<BTreeSet<VirtualAddress>> {
        let inferior = self.session.inferior().ok_or(Error::NotRunning)?;
        self.resolve_breakpoint(crate::BreakpointId::new(0), spec, false)?
            .locations
            .iter()
            .filter_map(|resolved| {
                runtime_breakpoint_address(inferior, &resolved.location).transpose()
            })
            .collect()
    }

    pub(super) fn begin_execution(
        &mut self,
        requested_process: ProcessId,
        stop_id: StopId,
        scope: ResumeScope,
        mut kind: ActiveKind,
        exception: ExceptionDisposition,
    ) -> Result<ExecutionId> {
        let inferior = self.session.inferior_mut().ok_or(Error::NotRunning)?;
        validate_process(inferior, requested_process)?;
        validate_public_stop(inferior, Some(stop_id))?;
        validate_resumable(inferior)?;
        scoped_threads(inferior, scope)?;
        self.validate_agent_resume(scope)?;
        self.sync_debug_registers()?;
        self.refresh_watch_baselines()?;

        let standing = match &kind {
            ActiveKind::Step { owner, start, .. } if !start.targets.is_empty() => {
                let at = self.ptrace.registers(owner.thread)?.program_counter();
                start.targets.contains(&at).then_some((owner.thread, at))
            }
            _ => None,
        };

        let inferior = self.session.inferior_mut().ok_or(Error::NotRunning)?;

        let resume_threads = scoped_threads(inferior, scope)?;
        inferior.next_execution += 1;
        let execution_id = ExecutionId::new(inferior.next_execution);
        self.install_plan(&kind, execution_id)?;
        let inferior = self.session.inferior_mut().ok_or(Error::NotRunning)?;

        if let Some((pid, at)) = standing
            && inferior.thread(pid)?.trap != (AtTrap::MustStepOver { at })
            && let ActiveKind::Step { start, .. } = &mut kind
        {
            start.standing = Some(at);
        }

        let mut suppressed = Vec::new();
        if exception == ExceptionDisposition::Suppress {
            for &pid in &resume_threads {
                let thread = inferior.thread_mut(pid)?;
                if let Some(pending) = thread.pending_signal.take() {
                    suppressed.push((pid, pending));
                }
            }
        }

        for &pid in &resume_threads {
            inferior.thread_mut(pid)?.reason = None;
        }

        inferior.run.begin(ActiveExecution {
            id: execution_id,
            kind,
            scope,
            resume_threads,
        });

        for installation in inferior.agents.values_mut() {
            installation.start_deadline(std::time::Instant::now());
        }

        self.open_inspectors();
        if let Err(error) = self.advance_execution() {
            if is_vanished_tracee(&error) && self.release_superseded_threads(None) {
                self.bump_revision();
                return Ok(execution_id);
            }

            self.restore_unconsumed_signals(&suppressed);
            let cause = error.to_string();
            if let Err(recovery) = self.recover_partial_resume() {
                let _ = self.kill_inferior();
                return Err(backend_error(LinuxError::ResumeRecovery {
                    cause,
                    recovery: recovery.to_string(),
                }));
            }

            return Err(error);
        }

        self.bump_revision();
        Ok(execution_id)
    }

    fn install_plan(&mut self, kind: &ActiveKind, execution: ExecutionId) -> Result<()> {
        if let ActiveKind::RuntimeStep { runtime, kind, .. } = kind {
            return self.install_native_entry(*runtime, *kind, execution);
        }
        let ActiveKind::Step { start, .. } = kind else {
            return Ok(());
        };

        let inferior = self.session.inferior_mut().ok_or(Error::NotRunning)?;
        let mut installed = Vec::new();
        let mut failure = None;
        for address in start.plan_sites() {
            if let Err(error) = install_plan_breakpoint(&self.ptrace, inferior, address, execution)
            {
                failure = Some(error);
                break;
            }

            installed.push(address);
        }

        let Some(error) = failure else {
            return Ok(());
        };

        if self.lost_to_sigkill(&error) {
            return Err(error);
        }

        let inferior = self.session.inferior_mut().ok_or(Error::NotRunning)?;
        let owner = BreakpointOwner::Plan(execution);
        for address in installed.into_iter().rev() {
            if let Err(recovery) =
                remove_breakpoint_owner_from(&self.ptrace, inferior, address, owner)
            {
                let _ = self.ptrace.kill(inferior.tgid, Signal::SIGKILL);
                return Err(backend_error(LinuxError::ResumeRecovery {
                    cause: error.to_string(),
                    recovery: recovery.to_string(),
                }));
            }
        }

        Err(error)
    }

    pub(super) fn restore_unconsumed_signals(&mut self, suppressed: &[(Pid, PendingSignal)]) {
        let Some(inferior) = self.session.inferior_mut() else {
            return;
        };

        for &(pid, pending) in suppressed {
            if let Some(thread) = inferior.threads.get_mut(&pid)
                && matches!(thread.state, NativeThreadState::Stopped)
                && thread.pending_signal.is_none()
            {
                thread.pending_signal = Some(pending);
            }
        }
    }

    pub(super) fn reply_execution(
        &self,
        result: Result<ExecutionId>,
        scope: ResumeScope,
        reply: Reply<ExecutionId>,
    ) {
        match result {
            Ok(execution_id) => {
                let Some(process_id) = self
                    .session
                    .inferior()
                    .map(|inferior| process_id(inferior.tgid))
                else {
                    let _ =
                        reply.send(Err(Error::internal("successful execution has an inferior")));
                    return;
                };

                let _ = reply.send(Ok(execution_id));
                let _ = self.events.send(DebuggerEvent::InferiorContinued {
                    revision: self.revision,
                    process_id,
                    execution_id,
                    resumed: scope,
                });
            }
            Err(error) => {
                let _ = reply.send(Err(error));
            }
        }
    }

    pub(super) fn begin_pause(&mut self, requested_process: ProcessId) -> Result<ExecutionId> {
        let inferior = self.session.inferior_mut().ok_or(Error::NotRunning)?;
        validate_process(inferior, requested_process)?;
        let execution_id = inferior.run.execution().ok_or(Error::AlreadyStopped)?.id;

        if self.interrupt_javascript() {
            return Ok(execution_id);
        }
        self.pause_natively()?;
        Ok(execution_id)
    }

    pub(super) fn pause_natively(&mut self) -> Result<()> {
        let inferior = self.session.inferior_mut().ok_or(Error::NotRunning)?;
        if let Some(barrier) = inferior.run.barrier_mut() {
            barrier.pause();
            return Ok(());
        }

        if inferior
            .threads
            .values()
            .all(|thread| matches!(thread.state, NativeThreadState::Exiting))
        {
            return Ok(());
        }

        let triggering_thread = inferior
            .threads
            .iter()
            .find_map(|(&pid, thread)| {
                (matches!(
                    thread.state,
                    NativeThreadState::Running | NativeThreadState::Starting
                ) || matches!(
                    thread.expected,
                    ExpectedStop::InitialExec | ExpectedStop::AdoptedExec
                ))
                .then_some(pid)
            })
            .or_else(|| {
                inferior.threads.iter().find_map(|(&pid, thread)| {
                    matches!(thread.state, NativeThreadState::Stopped).then_some(pid)
                })
            })
            .ok_or(Error::NotStopped)?;
        inferior
            .run
            .gather(StopBarrier::visible(triggering_thread, StopReason::Pause))?;

        self.request_stops()?;
        self.finish_barrier_if_ready()
    }
}

impl<P: LinuxTraceOps> Controller<P> {
    pub(super) fn advance_execution(&mut self) -> Result<()> {
        let inferior = self.session.inferior().ok_or(Error::NotRunning)?;
        if inferior.repairing().is_some() || inferior.next_repair().is_some() {
            return self.start_next_repair();
        }

        let active = inferior.run.execution().ok_or(Error::NotRunning)?;
        let (kind, resume_threads) = (active.kind.clone(), active.resume_threads.clone());

        let inferior = self.session.inferior_mut().ok_or(Error::NotRunning)?;
        for thread in inferior.threads.values_mut() {
            if thread.retrap.is_some_and(|address| {
                !inferior
                    .breakpoints
                    .get(&address)
                    .is_some_and(|site| site.trap == Trap::Installed)
            }) {
                thread.retrap = None;
            }
        }

        if let Some(pid) = resume_threads.iter().copied().find(|pid| {
            inferior
                .threads
                .get(pid)
                .is_some_and(|thread| thread.retrap.is_some())
        }) {
            return self.resume_awaiting_thread(pid);
        }

        match kind {
            ActiveKind::Step {
                owner: StepOwner { thread, .. },
                kind,
                progress_owed,
                ..
            } => {
                let exiting = self
                    .session
                    .inferior()
                    .and_then(|inferior| inferior.threads.get(&thread))
                    .is_none_or(|stepping| !matches!(stepping.state, NativeThreadState::Stopped));
                if exiting {
                    return self.continue_scope_threads();
                }

                if progress_owed {
                    if let Some(ActiveKind::Step { progress_owed, .. }) = self
                        .session
                        .inferior_mut()
                        .and_then(|inferior| inferior.run.execution_mut())
                        .map(|active| &mut active.kind)
                    {
                        *progress_owed = false;
                    }

                    if self.step_runs_to_guard() {
                        self.start_user_step(thread, kind)?;
                    } else {
                        self.complete_user_step(thread, kind)?;
                    }
                } else if self.arrival_left_to_a_lost_trap(thread)? {
                    self.complete_user_step(thread, kind)?;
                } else {
                    self.start_user_step(thread, kind)?;
                }

                self.continue_scope_threads()?;
                self.resume_inspectors(&resume_threads)
            }
            ActiveKind::Launch
            | ActiveKind::Continue
            | ActiveKind::RuntimeStep { .. }
            | ActiveKind::RuntimeAssignment { .. } => {
                for &pid in &resume_threads {
                    self.continue_thread(pid)?;
                }

                self.resume_inspectors(&resume_threads)
            }
        }
    }

    pub(super) fn start_next_repair(&mut self) -> Result<()> {
        let inferior = self.session.inferior_mut().ok_or(Error::NotRunning)?;
        if inferior.repairing().is_some() {
            return Ok(());
        }

        let next = inferior.next_repair();
        let pid = inferior.memory_thread();
        let lifted = inferior
            .breakpoints
            .iter()
            .filter(|&(&address, site)| {
                site.trap == Trap::Lifted(Lift::Repair)
                    && next.is_none_or(|(_, next)| next != address)
            })
            .map(|(&address, _)| address)
            .collect::<Vec<_>>();
        for address in lifted {
            self.ptrace
                .reinstall_breakpoint(pid, &mut inferior.breakpoints, address)?;
        }

        let Some((pid, address)) = next else {
            return self.advance_execution();
        };

        if inferior.thread(pid)?.pending_signal.is_some() {
            return Err(backend_error(LinuxError::UnexpectedWait(format!(
                "repair thread {pid} has a pending signal"
            ))));
        }

        if inferior
            .breakpoints
            .get(&address)
            .is_some_and(|site| site.trap == Trap::Installed)
        {
            let memory = inferior.memory_thread();
            self.ptrace.lift_breakpoint(
                memory,
                &mut inferior.breakpoints,
                address,
                Lift::Repair,
            )?;
        }
        inferior.trap_event(pid, TrapEvent::RepairStarted)?;

        self.guard_unchosen_call(pid)?;
        self.resume_native(pid, Resume::Step, false, ExpectedStop::None)
    }

    pub(super) fn resume_awaiting_thread(&mut self, pid: Pid) -> Result<()> {
        debug_assert!(
            self.session
                .inferior()
                .and_then(|inferior| inferior.threads.get(&pid))
                .is_some_and(|thread| thread.retrap.is_some()),
            "thread {pid} runs alone with no trap to return to"
        );
        self.resume_native(pid, Resume::Continue, true, ExpectedStop::None)
    }

    pub(super) fn continue_thread(&mut self, pid: Pid) -> Result<()> {
        let inferior = self.session.inferior().ok_or(Error::NotRunning)?;
        if !matches!(inferior.thread(pid)?.state, NativeThreadState::Stopped) {
            return Ok(());
        }

        if self.step_gate_holds(pid) {
            return Ok(());
        }

        self.resume_native(pid, Resume::Continue, true, ExpectedStop::None)
    }

    pub(super) fn resume_native(
        &mut self,
        pid: Pid,
        resume: Resume,
        deliver_signal: bool,
        expected: ExpectedStop,
    ) -> Result<()> {
        let inferior = self.session.inferior_mut().ok_or(Error::NotRunning)?;
        let repairing = inferior.thread(pid)?.trap.repairing().is_some();
        let stepping_call = matches!(resume, Resume::Step)
            && (repairing || matches!(expected, ExpectedStop::UserStep { .. }))
            && !self.catchpoints.is_empty()
            && self.steps_into_call(pid)?;

        let inferior = self.session.inferior_mut().ok_or(Error::NotRunning)?;
        let holds = inferior.holds_signals(inferior.thread(pid)?, &expected);
        let thread = inferior.thread_mut(pid)?;

        debug_assert!(
            matches!(
                thread.state,
                NativeThreadState::Stopped | NativeThreadState::Exiting
            ),
            "resuming thread {pid} from {:?}, not from a stop",
            thread.state
        );

        let mut signal = deliver_signal
            .then(|| thread.pending_signal.map(|pending| pending.signal))
            .flatten()
            .filter(|signal| self.signals.get(*signal).pass);

        if matches!(resume, Resume::Continue) && signal.is_none() && !holds {
            signal = thread
                .held_signal
                .take()
                .map(|held| held.signal)
                .filter(|signal| self.signals.get(*signal).pass);
        }

        let stepping_call = stepping_call && signal.is_none();
        let to_call =
            stepping_call || matches!(resume, Resume::Continue) && !self.catchpoints.is_empty();
        thread.stepping_call = stepping_call;
        thread.call.resume(to_call);

        let result = match resume {
            _ if to_call => self.ptrace.continue_to_syscall(pid, signal),
            Resume::Continue => self.ptrace.continue_execution(pid, signal),
            Resume::Step => self.ptrace.step(pid, signal),
        };

        match result {
            Ok(()) => thread.state = NativeThreadState::Running,
            Err(error) if is_vanished_tracee(&error) => thread.state = NativeThreadState::Exiting,
            Err(error) => return Err(error),
        }

        if deliver_signal {
            thread.pending_signal = None;
        }

        thread.expected = expected;
        thread.reason = None;
        inferior.trap_event(pid, TrapEvent::Resumed)
    }

    pub(super) fn handle_classified_stop(&mut self, pid: Pid, stop: ClassifiedStop) -> Result<()> {
        record!("classified {pid} as {stop:?}");

        {
            let inferior = self.session.inferior_mut().ok_or(Error::NotRunning)?;
            inferior.thread_mut(pid)?.state = NativeThreadState::Stopped;
        }

        match stop {
            ClassifiedStop::ThreadStart => self.handle_thread_start(pid),
            ClassifiedStop::DebuggerRequested => {
                let thread = self
                    .session
                    .inferior_mut()
                    .ok_or(Error::NotRunning)?
                    .thread_mut(pid)?;
                thread.reason = None;
                thread.debugger_stop_pending = false;
                self.restart_after_internal(pid)
            }
            ClassifiedStop::RemovedTrap => {
                record!("{pid} executed a trap before its site was removed");
                self.session
                    .inferior_mut()
                    .ok_or(Error::NotRunning)?
                    .thread_mut(pid)?
                    .reason = None;
                self.restart_after_internal(pid)
            }

            ClassifiedStop::CarriedTrap => {
                record!("{pid} executed a trap carried by moved or copied code");
                self.session
                    .inferior_mut()
                    .ok_or(Error::NotRunning)?
                    .thread_mut(pid)?
                    .reason = None;
                let address = self.ptrace.registers(pid)?.program_counter().get();
                self.forget_copy_at(address);
                self.queue_module_refresh()?;
                self.restart_after_internal(pid)
            }
            ClassifiedStop::Breakpoint(address) => self.handle_breakpoint_stop(pid, address),
            ClassifiedStop::ProgramTrap(address) => self.begin_visible_stop(
                pid,
                StopReason::ProgramBreakpoint {
                    address: Some(address),
                },
            ),
            ClassifiedStop::Watch(owners) => self.handle_watch_stop(pid, owners),
            ClassifiedStop::Trace { watch } => self.handle_trace_stop(pid, watch),
            ClassifiedStop::SignalDelivery(pending) => self.handle_signal_stop(pid, pending),
            ClassifiedStop::GroupStop(signal) => {
                self.begin_visible_stop(pid, StopReason::Exception(exception_info(signal)))
            }

            ClassifiedStop::Superseded => {
                let inferior = self.session.inferior_mut().ok_or(Error::NotRunning)?;
                inferior.thread_mut(pid)?.state = NativeThreadState::Running;
                Ok(())
            }
            ClassifiedStop::Unclassifiable(raw) => self.begin_visible_stop(
                pid,
                StopReason::Unclassifiable {
                    description: format_raw_stop(&raw).into(),
                },
            ),
        }
    }

    pub(super) fn handle_breakpoint_stop(
        &mut self,
        pid: Pid,
        address: VirtualAddress,
    ) -> Result<()> {
        let inferior = self.session.inferior_mut().ok_or(Error::NotRunning)?;
        inferior.trap_event(pid, TrapEvent::Hit(address))?;
        let thread = inferior.thread_mut(pid)?;
        if thread.retrap == Some(address) {
            thread.retrap = None;
            debug_assert!(
                inferior
                    .run
                    .execution()
                    .is_some_and(|active| active.resume_threads.contains(&pid)),
                "thread {pid} returned to the trap at {address} outside the execution"
            );
            return self.start_next_repair();
        }

        self.forget_trap_judgement(pid, address);
        self.note_internal_sites(pid, address)?;
        let stopping = self.record_breakpoint_hits(pid, address);
        if !stopping.is_empty() {
            return self.begin_visible_stop(
                pid,
                StopReason::Breakpoint {
                    address: Some(address),
                    hits: stopping,
                },
            );
        }

        self.arrive_unstopped(pid, address)
    }

    fn arrive_unstopped(&mut self, pid: Pid, address: VirtualAddress) -> Result<()> {
        if let Some(reason) = self.runtime_hook_stop(pid, address) {
            return self.begin_visible_stop(pid, reason);
        }

        if self.reach_resumed_future(pid, address)? {
            return Ok(());
        }

        let inferior = self.session.inferior().ok_or(Error::NotRunning)?;
        let step = inferior
            .run
            .execution()
            .and_then(|active| match &active.kind {
                ActiveKind::Step { owner, kind, .. } if self.runs_step(*owner, pid) => {
                    Some((active.id, *kind))
                }
                _ => None,
            });
        let planned = step.filter(|(execution, _)| {
            inferior
                .breakpoints
                .get(&address)
                .is_some_and(|site| site.owners.contains(&BreakpointOwner::Plan(*execution)))
        });
        if step.is_some() {
            self.follow_step(pid);
        }

        if let Some((_, kind)) = planned {
            return self.reach_plan_site(pid, address, kind);
        }

        if self.reach_native_entry(pid, address) {
            return if self.barrier_active() {
                self.finish_barrier_if_ready()
            } else {
                self.repair_when_alone(pid, address)
            };
        }

        if let Some(kind) = self.runtime_step_arrival(pid, address) {
            return self.begin_visible_stop(pid, StopReason::Step { kind });
        }

        if let Some((_, kind)) = step
            && !steps_instructions(kind)
            && !self.step_runs_to_guard()
            && let Some(reason) = self.user_step_stop(pid, kind)?
        {
            return self.begin_visible_stop(pid, reason);
        }

        if self.barrier_active() {
            self.finish_barrier_if_ready()
        } else {
            self.repair_when_alone(pid, address)
        }
    }

    fn reach_plan_site(&mut self, pid: Pid, address: VirtualAddress, kind: StepKind) -> Result<()> {
        self.note_returned_values(pid);

        if self.is_advance_target(address) {
            return self.reach_advance_target(pid, address);
        }

        if self.reach_waypoint(pid, address)? {
            return Ok(());
        }

        match self.reach_loop(pid, address, kind)? {
            LoopReach::Elsewhere => {}
            LoopReach::Complete => {
                return self.begin_visible_stop(pid, StopReason::Step { kind });
            }
            LoopReach::Pass => return self.repair_when_alone(pid, address),
            LoopReach::Restarted => {
                return self.go_on_without_plan(pid, address, Some(kind));
            }
        }

        match self.follow_poll_return(pid, kind)? {
            Some(Followed::Waits) => return self.go_on_without_plan(pid, address, None),
            Some(Followed::Ended(reason)) => return self.begin_visible_stop(pid, reason),
            None => {}
        }

        if self.begin_following(pid, kind)? || self.wait_for_loop(pid, kind)? {
            return self.go_on_without_plan(pid, address, Some(kind));
        }

        let mode = self.step_mode(kind);

        if !steps_instructions(kind) {
            self.begin_epilogue_traversal(pid)?;
        }

        self.note_returned_activation(pid, mode)?;

        if self.source_step_returned_to_runtime(pid, mode)?
            || self.native_step_returned_to_javascript(pid, mode)?
        {
            self.let_step_run_on()?;
            self.queue_internal_edit(Edit::ReturnToRuntime { pid })?;
            return self.go_on_without_plan(pid, address, None);
        }

        if self.source_step_returned_to_undescribed_code(pid, mode)? {
            self.let_step_run_on()?;
            return self.go_on_without_plan(pid, address, None);
        }

        if let Some(reason) = self.user_step_stop(pid, kind)? {
            return self.begin_visible_stop(pid, reason);
        }

        self.mark_epilogue_return_for_retirement(address);
        self.mark_return_guard_for_retirement(pid, address)?;
        self.repair_when_alone(pid, address)
    }

    fn step_runs_to_guard(&self) -> bool {
        self.session
            .inferior()
            .and_then(|inferior| inferior.run.execution())
            .is_some_and(|active| {
                matches!(&active.kind, ActiveKind::Step { start, .. } if start.resume_guard.is_some())
            })
    }

    fn note_internal_sites(&mut self, pid: Pid, address: VirtualAddress) -> Result<()> {
        if self.is_loader_site(address) {
            self.queue_module_refresh()?;
        }

        self.note_stack_move(pid, address)?;
        self.note_interpreted_move(pid, address)?;
        self.note_resolver(pid, address)?;
        self.note_inspector_hook(pid, address)
    }

    pub(super) fn reach_advance_target(&mut self, pid: Pid, address: VirtualAddress) -> Result<()> {
        if !self.take_standing_arrival(address) {
            return self.begin_visible_stop(
                pid,
                StopReason::Step {
                    kind: StepKind::Advance,
                },
            );
        }

        if self.barrier_active() {
            self.finish_barrier_if_ready()
        } else {
            self.repair_when_alone(pid, address)
        }
    }

    pub(super) fn go_on_without_plan(
        &mut self,
        pid: Pid,
        address: VirtualAddress,
        stepping: Option<StepKind>,
    ) -> Result<()> {
        let trapped = self
            .session
            .inferior()
            .ok_or(Error::NotRunning)?
            .thread(pid)?
            .trap
            == (AtTrap::MustStepOver { at: address });
        if trapped {
            self.repair_when_alone(pid, address)
        } else if self.barrier_active() {
            self.finish_barrier_if_ready()
        } else if let Some(kind) = stepping {
            self.start_user_step(pid, kind)
        } else {
            self.continue_thread(pid)
        }
    }

    pub(super) fn handle_trace_stop(
        &mut self,
        pid: Pid,
        watch: BTreeSet<WatchpointId>,
    ) -> Result<()> {
        let expected = self
            .session
            .inferior_mut()
            .and_then(|inferior| inferior.threads.get_mut(&pid))
            .map(|thread| std::mem::replace(&mut thread.expected, ExpectedStop::None))
            .ok_or(Error::NotRunning)?;
        let watch = self.stopping_watch_hits(pid, watch)?;
        if !watch.is_empty() {
            return self.finish_watched_instruction(pid, &expected, watch);
        }

        self.finish_traced_instruction(pid, expected)
    }

    fn finish_traced_instruction(&mut self, pid: Pid, expected: ExpectedStop) -> Result<()> {
        if self.barrier_active() {
            return self.settle_trace_during_barrier(pid, expected);
        }

        if self.repairs(pid) {
            return self.complete_repair(pid);
        }

        match expected {
            ExpectedStop::UserStep { kind } => self.complete_user_step(pid, kind),
            other => self.begin_visible_stop(
                pid,
                StopReason::Unclassifiable {
                    description: format!("unexpected trace stop in {other:?}").into(),
                },
            ),
        }
    }

    fn handle_watch_stop(&mut self, pid: Pid, owners: BTreeSet<WatchpointId>) -> Result<()> {
        let expected = self
            .session
            .inferior_mut()
            .and_then(|inferior| inferior.threads.get_mut(&pid))
            .map(|thread| std::mem::replace(&mut thread.expected, ExpectedStop::None))
            .ok_or(Error::NotRunning)?;
        let owners = self.stopping_watch_hits(pid, owners)?;
        if !owners.is_empty() {
            return self.finish_watched_instruction(pid, &expected, owners);
        }

        match expected {
            _ if self.repairs(pid) => self.finish_traced_instruction(pid, expected),
            ExpectedStop::UserStep { .. } => self.finish_traced_instruction(pid, expected),
            expected => {
                self.session
                    .inferior_mut()
                    .ok_or(Error::NotRunning)?
                    .thread_mut(pid)?
                    .expected = expected;
                self.restart_after_internal(pid)
            }
        }
    }

    pub(super) fn finish_watched_instruction(
        &mut self,
        pid: Pid,
        expected: &ExpectedStop,
        owners: BTreeMap<WatchpointId, u64>,
    ) -> Result<()> {
        let repaired = self.repairs(pid);
        let inferior = self.session.inferior_mut().ok_or(Error::NotRunning)?;
        if repaired {
            inferior.trap_event(pid, TrapEvent::Stepped)?;
        }

        inferior.thread_mut(pid)?.watch_hits.extend(owners);
        if repaired || matches!(expected, ExpectedStop::UserStep { .. }) {
            self.note_step_progress(pid);
        }

        self.begin_visible_stop(
            pid,
            StopReason::Watchpoint {
                hits: Arc::from([]),
            },
        )
    }

    pub(super) fn barrier_active(&self) -> bool {
        self.session
            .inferior()
            .is_some_and(|inferior| inferior.run.barrier().is_some())
    }

    pub(super) fn settle_trace_during_barrier(
        &mut self,
        pid: Pid,
        expected: ExpectedStop,
    ) -> Result<()> {
        match expected {
            _ if self.repairs(pid) => {
                self.session
                    .inferior_mut()
                    .ok_or(Error::NotRunning)?
                    .trap_event(pid, TrapEvent::Stepped)?;
                self.note_step_progress(pid);
            }
            ExpectedStop::UserStep { .. } => self.note_step_progress(pid),
            other => {
                return self.begin_visible_stop(
                    pid,
                    StopReason::Unclassifiable {
                        description: format!("unexpected trace stop during a stop in {other:?}")
                            .into(),
                    },
                );
            }
        }

        self.finish_barrier_if_ready()
    }

    pub(super) fn handle_signal_stop(&mut self, pid: Pid, pending: PendingSignal) -> Result<()> {
        const QUIET: SignalPolicy = SignalPolicy {
            stop: false,
            print: false,
            pass: true,
        };
        let inferior = self.session.inferior_mut().ok_or(Error::NotRunning)?;

        let terminating = inferior
            .terminating
            .as_mut()
            .filter(|terminating| terminating.signal == pending.signal);
        let policy = if let Some(terminating) = terminating {
            terminating.delivered = true;
            QUIET
        } else {
            self.signals.get(pending.signal)
        };

        if policy.stop {
            return self.stop_for_signal(pid, pending);
        }

        let inferior = self.session.inferior().ok_or(Error::NotRunning)?;
        if policy.print {
            let _ = self.events.send(DebuggerEvent::SignalReceived {
                revision: self.revision,
                process_id: process_id(inferior.tgid),
                thread_id: debug_thread_id(pid),
                exception: pending_exception_info(pending),
            });
        }

        let thread = inferior.thread(pid)?;
        let expected = thread.expected.clone();
        let repairing = thread.trap.repairing();
        let mut delivered = policy.pass.then_some(pending);
        if let Some(pending) = delivered.filter(|pending| {
            inferior.holds_signals(thread, &expected) && self.defers(pending.signal)
        }) {
            record!("hold {} in {pid}", pending.signal);
            self.session
                .inferior_mut()
                .ok_or(Error::NotRunning)?
                .thread_mut(pid)?
                .held_signal = Some(pending);
            delivered = None;
        }

        if let Some(address) = repairing {
            return self.signal_during_repair(pid, address, delivered);
        }

        match expected {
            ExpectedStop::UserStep { kind } => self.signal_during_step(pid, kind, delivered),
            expected => {
                self.session
                    .inferior_mut()
                    .ok_or(Error::NotRunning)?
                    .thread_mut(pid)?
                    .pending_signal = delivered;
                if self.barrier_active() {
                    return self.finish_barrier_if_ready();
                }

                self.resume_native(pid, Resume::Continue, true, expected)
            }
        }
    }

    fn stop_for_signal(&mut self, pid: Pid, pending: PendingSignal) -> Result<()> {
        let inferior = self.session.inferior_mut().ok_or(Error::NotRunning)?;
        if let Some(address) = inferior.thread(pid)?.trap.repairing() {
            inferior.trap_event(pid, TrapEvent::RepairInterrupted)?;
            inferior.thread_mut(pid)?.retrap = Some(address);
            self.restore_active_breakpoints()?;
        }

        let inferior = self.session.inferior_mut().ok_or(Error::NotRunning)?;
        let thread = inferior.thread_mut(pid)?;
        thread.expected = ExpectedStop::None;
        thread.pending_signal = Some(pending);
        self.begin_visible_stop(pid, StopReason::Exception(pending_exception_info(pending)))
    }

    fn signal_during_repair(
        &mut self,
        pid: Pid,
        address: VirtualAddress,
        delivered: Option<PendingSignal>,
    ) -> Result<()> {
        let barrier = self.barrier_active();
        let inferior = self.session.inferior_mut().ok_or(Error::NotRunning)?;
        let Some(pending) = delivered else {
            inferior.thread_mut(pid)?.pending_signal = None;
            if barrier {
                return self.finish_barrier_if_ready();
            }

            return self.resume_native(pid, Resume::Step, false, ExpectedStop::None);
        };

        if inferior
            .breakpoints
            .get(&address)
            .is_some_and(|site| site.trap == Trap::Lifted(Lift::Repair))
        {
            let memory = inferior.memory_thread();
            self.ptrace
                .reinstall_breakpoint(memory, &mut inferior.breakpoints, address)?;
        }
        inferior.trap_event(pid, TrapEvent::RepairInterrupted)?;
        let thread = inferior.thread_mut(pid)?;
        thread.retrap = Some(address);
        thread.pending_signal = Some(pending);
        thread.expected = ExpectedStop::None;
        if barrier {
            self.finish_barrier_if_ready()
        } else {
            self.resume_awaiting_thread(pid)
        }
    }

    fn signal_during_step(
        &mut self,
        pid: Pid,
        kind: StepKind,
        delivered: Option<PendingSignal>,
    ) -> Result<()> {
        let barrier = self.barrier_active();
        let Some(pending) = delivered else {
            self.session
                .inferior_mut()
                .ok_or(Error::NotRunning)?
                .thread_mut(pid)?
                .pending_signal = None;
            if barrier {
                return self.finish_barrier_if_ready();
            }

            return self.resume_native(pid, Resume::Step, false, ExpectedStop::UserStep { kind });
        };

        let registers = self.ptrace.registers(pid)?;
        let guard = ResumeGuard {
            address: registers.program_counter(),
            stack: self.stack_position(pid, &registers),
            call: false,
        };

        let execution = self.active_execution()?;
        self.install_additional_plan_breakpoints(execution, &BTreeSet::from([guard.address]))?;
        if let Some(start) = self.active_step_mut() {
            start.resume_guard = Some(guard);
        }

        let thread = self
            .session
            .inferior_mut()
            .ok_or(Error::NotRunning)?
            .thread_mut(pid)?;
        thread.pending_signal = Some(pending);
        thread.expected = ExpectedStop::None;
        if barrier {
            self.finish_barrier_if_ready()
        } else {
            self.continue_thread(pid)
        }
    }

    fn reach_waypoint(&mut self, pid: Pid, address: VirtualAddress) -> Result<bool> {
        if self.reach_new_task(pid, address)? {
            return Ok(true);
        }

        if let Some(start) = self.active_step_mut() {
            start.escape = None;
        }

        self.reach_resume_guard(pid, address)
    }

    fn reach_resume_guard(&mut self, pid: Pid, address: VirtualAddress) -> Result<bool> {
        let Some((execution, kind, guard, planned)) = self
            .session
            .inferior()
            .and_then(|inferior| inferior.run.execution())
            .and_then(|active| match &active.kind {
                ActiveKind::Step {
                    owner, kind, start, ..
                } if self.runs_step(*owner, pid) => start
                    .resume_guard
                    .filter(|guard| guard.address == address)
                    .map(|guard| {
                        (
                            active.id,
                            *kind,
                            guard,
                            start.plan_addresses.contains(&address),
                        )
                    }),
                _ => None,
            })
        else {
            return Ok(false);
        };

        if self.stack_position(pid, &self.ptrace.registers(pid)?) != guard.stack {
            if planned {
                return Ok(false);
            }

            self.repair_when_alone(pid, address)?;
            return Ok(true);
        }

        if let Some(start) = self.active_step_mut() {
            start.resume_guard = None;
        }

        if planned {
            return Ok(false);
        }

        self.remove_breakpoint_owner(address, BreakpointOwner::Plan(execution))?;
        let trapped = matches!(
            self.session
                .inferior()
                .ok_or(Error::NotRunning)?
                .thread(pid)?
                .trap,
            AtTrap::MustStepOver { .. }
        );
        if trapped {
            if !steps_instructions(kind)
                && let Some(reason) = self.user_step_stop(pid, kind)?
            {
                self.begin_visible_stop(pid, reason)?;
                return Ok(true);
            }

            self.repair_when_alone(pid, address)?;
        } else if guard.call {
            self.complete_user_step(pid, kind)?;
        } else {
            self.start_user_step(pid, kind)?;
        }

        Ok(true)
    }
}

impl<P: LinuxTraceOps> Controller<P> {
    pub(super) fn repairs(&self, pid: Pid) -> bool {
        self.session
            .inferior()
            .and_then(|inferior| inferior.threads.get(&pid))
            .is_some_and(|thread| thread.trap.repairing().is_some())
    }

    pub(super) fn complete_repair(&mut self, pid: Pid) -> Result<()> {
        self.session
            .inferior_mut()
            .ok_or(Error::NotRunning)?
            .trap_event(pid, TrapEvent::Stepped)?;

        self.note_step_progress(pid);
        self.start_next_repair()
    }

    pub(super) fn begin_visible_stop(&mut self, pid: Pid, reason: StopReason) -> Result<()> {
        let reason = self.dropped_step_reason(self.requested_step_reason(reason));
        let inferior = self.session.inferior_mut().ok_or(Error::NotRunning)?;
        let thread = inferior.thread_mut(pid)?;
        thread.state = NativeThreadState::Stopped;
        thread.reason = Some(reason.clone());
        thread.expected = ExpectedStop::None;

        self.raise_barrier(pid, reason)
    }

    pub(super) fn raise_barrier(&mut self, pid: Pid, reason: StopReason) -> Result<()> {
        let inferior = self.session.inferior_mut().ok_or(Error::NotRunning)?;
        if let Some(barrier) = inferior.run.barrier_mut() {
            barrier.offer(Some(pid), reason);
        } else {
            inferior.run.gather(StopBarrier::visible(pid, reason))?;
            self.request_stops()?;
        }

        self.finish_barrier_if_ready()
    }

    pub(super) fn recover_partial_resume(&mut self) -> Result<()> {
        self.restore_active_breakpoints()?;

        {
            let inferior = self.session.inferior_mut().ok_or(Error::NotRunning)?;
            let triggering_thread = inferior
                .threads
                .iter()
                .find_map(|(&pid, thread)| {
                    matches!(thread.state, NativeThreadState::Stopped).then_some(pid)
                })
                .or_else(|| inferior.threads.keys().next().copied())
                .ok_or(Error::NotRunning)?;
            let reason = StopReason::Unclassifiable {
                description: "execution resume failed; the debugger recovered to all-stop".into(),
            };

            inferior.thread_mut(triggering_thread)?.reason = Some(reason.clone());
            if let Some(barrier) = inferior.run.barrier_mut() {
                barrier.offer(Some(triggering_thread), reason);
            } else {
                inferior
                    .run
                    .gather(StopBarrier::visible(triggering_thread, reason))?;
            }
        }

        self.request_stops()?;
        self.finish_barrier_if_ready()
    }

    pub(super) fn request_stops(&mut self) -> Result<()> {
        let inferior = self.session.inferior_mut().ok_or(Error::NotRunning)?;
        let tgid = inferior.tgid;
        let attached = inferior.origin.seized();
        let running: Vec<_> = inferior
            .threads
            .iter()
            .filter_map(|(&pid, thread)| {
                matches!(thread.state, NativeThreadState::Running).then_some(pid)
            })
            .collect();

        for pid in running {
            let thread = inferior.thread_mut(pid)?;
            if attached {
                let _ = self.ptrace.interrupt(pid)?;
            } else if !thread.debugger_stop_pending {
                match self.ptrace.request_stop(tgid, pid) {
                    Err(error) if is_vanished_tracee(&error) => {}
                    result => result?,
                }
            }

            thread.debugger_stop_pending = true;
            thread.state = NativeThreadState::StopRequested;
        }

        Ok(())
    }

    fn presenting_thread(&self, triggering_thread: Pid) -> Pid {
        self.session
            .inferior()
            .filter(|inferior| {
                inferior
                    .threads
                    .get(&triggering_thread)
                    .is_some_and(|thread| inferior.exited_leader(triggering_thread, thread))
            })
            .and_then(|inferior| {
                inferior.threads.iter().find_map(|(&pid, thread)| {
                    matches!(thread.state, NativeThreadState::Stopped).then_some(pid)
                })
            })
            .unwrap_or(triggering_thread)
    }

    fn apply_pending_edits(&mut self) -> Result<Option<(Pid, VirtualAddress)>> {
        let edits = self
            .session
            .inferior_mut()
            .map(Inferior::take_pending_edits)
            .unwrap_or_default();
        let stepping_hit = if edits.is_empty() {
            None
        } else {
            self.stepping_thread_hit()
        };

        for edit in edits {
            self.apply_edit(edit);
        }

        self.settle_edited_reasons()?;
        Ok(stepping_hit.filter(|(pid, _)| {
            self.session
                .inferior()
                .and_then(|inferior| inferior.threads.get(pid))
                .is_some_and(|thread| thread.reason.is_none())
        }))
    }

    fn stepping_thread_hit(&self) -> Option<(Pid, VirtualAddress)> {
        let inferior = self.session.inferior()?;
        let ActiveKind::Step { owner, .. } = inferior.run.execution()?.kind else {
            return None;
        };

        inferior
            .threads
            .iter()
            .find_map(|(&pid, thread)| match thread.reason {
                Some(StopReason::Breakpoint {
                    address: Some(address),
                    ..
                }) if self.runs_step(owner, pid) => Some((pid, address)),
                _ => None,
            })
    }

    pub(super) fn finish_barrier_if_ready(&mut self) -> Result<()> {
        let ready = self.session.inferior().is_some_and(|inferior| {
            inferior.run.barrier().is_some()
                && inferior
                    .threads
                    .iter()
                    .all(|(&pid, thread)| inferior.settled(pid, thread))
        });
        if !ready {
            return Ok(());
        }

        if self.settle_vfork_windows()? {
            return Ok(());
        }

        let _span = crate::span!("stop");
        if self.session.attaching() {
            if self.seize_untraced_threads()? {
                return Ok(());
            }

            self.initialize_attached_inferior()?;
        } else if self.drain_queued_traps()? {
            return Ok(());
        }

        self.restore_active_breakpoints()?;

        if let Some((pid, address)) = self.apply_pending_edits()? {
            return self.arrive_unstopped(pid, address);
        }

        if self.inspector_cleanup_barrier()? {
            return Ok(());
        }

        if self
            .session
            .inferior()
            .and_then(|inferior| inferior.run.barrier())
            .is_some_and(|barrier| barrier.reason().is_none())
        {
            return self.resume_after_internal_stop();
        }

        if let Some(execution) = self
            .session
            .inferior()
            .and_then(|inferior| inferior.run.execution())
            .map(|active| active.id)
        {
            self.cleanup_plan_breakpoints(execution)?;
        }

        self.end_runtime_step();
        self.end_runtime_assignment();

        let exec_replaced = self
            .session
            .inferior()
            .is_some_and(|inferior| inferior.unfollowed_exec.is_some());
        if !exec_replaced {
            self.refresh_libraries()?;
        }

        self.evaluate_watchpoints()?;
        self.refresh_thread_names();

        let (triggering_thread, reason) = self
            .session
            .inferior()
            .and_then(|inferior| inferior.run.barrier())
            .and_then(StopBarrier::publishable)
            .ok_or_else(|| Error::internal("ready barrier publishes a reason"))?;
        self.publish_stop(triggering_thread, reason)
    }

    fn publish_stop(&mut self, triggering_thread: Pid, reason: StopReason) -> Result<()> {
        let triggering_thread = self.presenting_thread(triggering_thread);
        let presentation = self.presentation_for_thread(triggering_thread, Some(&reason))?;
        let returned = self.capture_returned(triggering_thread, &reason);

        let temporaries_removed = self.remove_stopped_temporaries()?;
        self.mark_published_pause(triggering_thread, &reason);
        let stop_id = self.ptrace.allocate_stop_id()?;
        let inferior = self.session.inferior_mut().ok_or(Error::NotRunning)?;
        check_publishable(inferior, stop_id);

        for thread in inferior.threads.values_mut() {
            thread.expected = ExpectedStop::None;
        }

        inferior.inspectors.end_interrupt();

        inferior.terminating = inferior
            .terminating
            .filter(|terminating| !terminating.delivered);
        let mut stop = PublicStop::new(
            stop_id,
            triggering_thread,
            reason.clone(),
            BTreeMap::from([(triggering_thread, presentation)]),
        );
        stop.returned = returned;
        let execution = inferior.run.publish(stop)?;

        for installation in inferior.agents.values_mut() {
            installation.pause_deadline(std::time::Instant::now());
        }

        let process_id = process_id(inferior.tgid);
        let reported = self.inspector_holds(debug_thread_id(triggering_thread))
            || match reason {
                StopReason::LanguageException(_) | StopReason::Breakpoint { address: None, .. } => {
                    true
                }
                StopReason::Step { .. } | StopReason::StepIncomplete { .. } => {
                    self.agent_hook(triggering_thread).is_some()
                }
                _ => false,
            };

        let watched = match reason {
            StopReason::Watchpoint { ref hits } => hits.first().and_then(|hit| {
                match self
                    .session
                    .inferior()?
                    .watch
                    .watchpoints
                    .get(&hit.watchpoint)?
                    .watchpoint
                    .scope
                {
                    crate::WatchScope::Interpreted { frame, .. } => Some(frame),
                    _ => None,
                }
            }),
            _ => None,
        };
        if reported {
            self.select_blamed_frame(triggering_thread);
        } else if let Some(frame) = watched {
            self.select_interpreted_frame(triggering_thread, frame);
        }

        self.select_assigned_frame(&reason);

        self.bump_revision();

        if self.session.attaching() {
            let _ = self.events.send(DebuggerEvent::InferiorAttached {
                revision: self.revision,
                process_id,
            });
        }

        let reason_published = reason.clone();
        let _ = self.events.send(DebuggerEvent::InferiorStopped {
            revision: self.revision,
            process_id,
            execution_id: execution,
            stop_id,
            thread_id: debug_thread_id(triggering_thread),
            reason,
        });

        if temporaries_removed {
            self.publish_breakpoints_changed();
        }

        if let Some(reply) = self.session.take_attach() {
            let _ = reply.send(Ok(stop_id));
        }

        self.answer_assignment(stop_id, &reason_published);

        Ok(())
    }

    pub(super) fn refresh_thread_names(&mut self) {
        let Some(inferior) = self.session.inferior_mut() else {
            return;
        };

        for (&pid, thread) in &mut inferior.threads {
            thread.name = self.ptrace.thread_name(inferior.tgid, pid);
        }
    }

    pub(super) fn restart_after_internal(&mut self, pid: Pid) -> Result<()> {
        if self.barrier_active() {
            return self.finish_barrier_if_ready();
        }

        let thread = self
            .session
            .inferior()
            .ok_or(Error::NotRunning)?
            .thread(pid)?;
        if thread.trap.repairing().is_some() {
            return self.resume_native(pid, Resume::Step, false, ExpectedStop::None);
        }
        if thread.retrap.is_some() {
            return self.resume_awaiting_thread(pid);
        }

        match thread.expected.clone() {
            ExpectedStop::UserStep { kind } => self.start_user_step(pid, kind),
            ExpectedStop::InitialExec
            | ExpectedStop::AdoptedExec
            | ExpectedStop::InitialAttach
            | ExpectedStop::None => self.continue_thread(pid),
        }
    }
}

fn check_publishable(inferior: &Inferior, stop_id: StopId) {
    for (&pid, thread) in &inferior.threads {
        debug_assert!(
            inferior.settled(pid, thread),
            "publishing stop {stop_id} while thread {pid} is {:?}",
            thread.state
        );
        debug_assert!(
            thread.trap.repairing().is_none(),
            "publishing stop {stop_id} while thread {pid} steps over a trap"
        );
    }

    let execution = inferior.run.execution().map(|active| active.id);
    for (&address, site) in &inferior.breakpoints {
        debug_assert!(
            site.trap == Trap::Installed,
            "publishing stop {stop_id} with the trap at {address} lifted"
        );
        debug_assert!(
            execution.is_none_or(|execution| {
                !site.owners.contains(&BreakpointOwner::Plan(execution))
            }),
            "publishing stop {stop_id} while execution {execution:?} still owns the \
             trap at {address}"
        );
    }
}
