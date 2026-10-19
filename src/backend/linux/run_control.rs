//! All-stop execution control: resuming threads, repairing breakpoint
//! sites, and publishing coherent stops.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
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

use super::classify::{format_raw_stop, visible_stop_priority};
use super::loops::LoopReach;
use super::native::{LinuxTraceOps, is_vanished_tracee};
use super::new_task::NewTask;
use super::{
    ActiveExecution, ActiveKind, BreakpointOwner, ClassifiedStop, Controller, Edit, ExpectedStop,
    Inferior, LinuxError, NativeThreadState, PendingSignal, PublicStop, RepairGroup, Resume,
    ResumeGuard, StepOwner, StopBarrier, backend_error, debug_thread_id, exception_info,
    pending_exception_info, process_id, scoped_threads, steps_instructions, validate_process,
    validate_public_stop, validate_resumable, validate_stopped_thread,
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

        // At an agent's hook, the innermost frames are the agent's: a
        // step by lines from the innermost, or out of it, begins at the
        // program's frame the stop blames, which it selected.
        let frame = match kind {
            StepKind::IntoSource | StepKind::OverSource | StepKind::Out
                if frame == StackFrameId::INNERMOST && into_call.is_none() =>
            {
                match self.reported_frame(stop_id, pid) {
                    Some(blamed) => blamed,
                    // A native step from here would step the agent's own
                    // machinery.
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
            && let Some(interpreted) = self.interpreted_frame(stop_id, pid, frame)
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

        // A step into a chosen call executes the line up to it, never only
        // entering an inline frame.
        let virtual_step = if into_call.is_some() {
            Ok(None)
        } else {
            self.try_virtual_step(process_id, stop_id, pid, kind)
        };

        match virtual_step {
            Ok(Some((execution, stopped))) => {
                // Acknowledge the step before publishing the stop it caused.
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

        // A step into a new task steps over its line until a task starts.
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

    /// Fails a stale or invalid step before any unwinding can.
    fn validate_step(
        &self,
        process_id: ProcessId,
        stop_id: StopId,
        pid: Pid,
        frame: StackFrameId,
        kind: StepKind,
        scope: ResumeScope,
    ) -> Result<()> {
        let inferior = self.inferior.as_ref().ok_or(Error::NotRunning)?;
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

        // A runtime steps from a frame it interprets, wherever it is.
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

    /// Runs until thread `pid` reaches a location `spec` resolves to, or
    /// `frame` returns first: a step out whose plan also holds the
    /// location's addresses, so the stop that ends it removes them.
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

                // An async function's frame returns once its future is
                // ready, not each time its poll returns `Pending`.
                if frame.get() == 0 {
                    start.awaiting = self.await_step(pid, StepKind::Out, &start);
                }

                // Reaching a location stops as an advance; the frame
                // returning first, as the step out it runs as.
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

    /// The addresses an advance's location resolves to.
    pub(super) fn advance_targets(
        &self,
        spec: crate::BreakpointSpec,
    ) -> Result<BTreeSet<VirtualAddress>> {
        let inferior = self.inferior.as_ref().ok_or(Error::NotRunning)?;
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
        let inferior = self.inferior.as_mut().ok_or(Error::NotRunning)?;
        validate_process(inferior, requested_process)?;
        validate_public_stop(inferior, Some(stop_id))?;
        validate_resumable(inferior)?;
        scoped_threads(inferior, scope)?;
        self.validate_agent_resume(scope)?;
        self.sync_debug_registers()?;
        self.refresh_watch_baselines()?;

        // A thread standing at its advance's location goes on until it
        // comes round.
        let standing = match &kind {
            ActiveKind::Step { owner, start, .. } if !start.targets.is_empty() => {
                let at = VirtualAddress::new(self.ptrace.registers(owner.thread)?.rip);
                start.targets.contains(&at).then_some((owner.thread, at))
            }
            _ => None,
        };

        let inferior = self.inferior.as_mut().ok_or(Error::NotRunning)?;

        let resume_threads = scoped_threads(inferior, scope)?;
        inferior.next_execution += 1;
        let execution_id = ExecutionId::new(inferior.next_execution);
        self.install_plan(&kind, execution_id)?;
        let inferior = self.inferior.as_mut().ok_or(Error::NotRunning)?;

        // One that trapped there steps over the trap as any other does; one
        // that has yet to execute the trap arrives there first.
        if let Some((pid, at)) = standing
            && inferior.thread(pid)?.stopped_at_breakpoint != Some(at)
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

        inferior.public_stop = None;
        debug_assert!(
            inferior.active.is_none(),
            "beginning execution {execution_id} while execution {:?} is active",
            inferior.active.as_ref().map(|active| active.id)
        );

        inferior.active = Some(ActiveExecution {
            id: execution_id,
            kind,
            scope,
            resume_threads,
        });
        inferior.repairs = collect_repairs(inferior);

        for installation in inferior.agents.values_mut() {
            installation.start_deadline(std::time::Instant::now());
        }

        if let Err(error) = self.advance_execution() {
            // SIGKILL took threads out of the stop as they were resumed: the
            // execution has begun, and runs on into the program's end.
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

    /// Plants a step's plan breakpoints for `execution`, removing those it
    /// planted when one cannot be.
    fn install_plan(&mut self, kind: &ActiveKind, execution: ExecutionId) -> Result<()> {
        let ActiveKind::Step { start, .. } = kind else {
            return Ok(());
        };

        let inferior = self.inferior.as_mut().ok_or(Error::NotRunning)?;
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

        let inferior = self.inferior.as_mut().ok_or(Error::NotRunning)?;
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
        let Some(inferior) = self.inferior.as_mut() else {
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
                    .inferior
                    .as_ref()
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
        let inferior = self.inferior.as_mut().ok_or(Error::NotRunning)?;
        validate_process(inferior, requested_process)?;
        let execution_id = inferior.active.as_ref().ok_or(Error::AlreadyStopped)?.id;
        if let Some(barrier) = inferior.barrier.as_mut() {
            // A stop in progress now ends in the pause, unless it publishes
            // a more important reason.
            barrier.reason.get_or_insert(StopReason::Pause);
            barrier.paused = true;
            return Ok(execution_id);
        }

        // Every thread is past its exit event, so none is left to stop: the
        // exit ends the execution, and with it the pause.
        if inferior
            .threads
            .values()
            .all(|thread| matches!(thread.state, NativeThreadState::Exiting))
        {
            return Ok(execution_id);
        }

        // A launching or starting thread has no stop to request: its first
        // stop completes the barrier.
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
            // With no thread to ask, the stopped threads wait only for those
            // past their exit events, or for none, as when a main thread
            // resumed alone exited and left its siblings held.
            .or_else(|| {
                inferior.threads.iter().find_map(|(&pid, thread)| {
                    matches!(thread.state, NativeThreadState::Stopped).then_some(pid)
                })
            })
            .ok_or(Error::NotStopped)?;
        inferior.barrier = Some(StopBarrier::visible(triggering_thread, StopReason::Pause));

        self.request_stops()?;
        self.finish_barrier_if_ready()?;
        Ok(execution_id)
    }
}

impl<P: LinuxTraceOps> Controller<P> {
    pub(super) fn advance_execution(&mut self) -> Result<()> {
        if !self
            .inferior
            .as_ref()
            .ok_or(Error::NotRunning)?
            .repairs
            .is_empty()
        {
            return self.start_next_repair();
        }

        let (kind, resume_threads) = {
            let inferior = self.inferior.as_mut().ok_or(Error::NotRunning)?;

            // A thread awaiting a breakpoint that has since been removed will
            // never trap there; resuming it alone would strand its siblings.
            for thread in inferior.threads.values_mut() {
                if thread.awaiting_breakpoint.is_some_and(|address| {
                    !inferior
                        .breakpoints
                        .get(&address)
                        .is_some_and(|site| site.installed)
                }) {
                    thread.awaiting_breakpoint = None;
                }
            }

            let active = inferior.active.as_ref().ok_or(Error::NotRunning)?;
            (active.kind.clone(), active.resume_threads.clone())
        };

        // A thread re-trapping at a breakpoint after its signal handler runs
        // alone, holding its siblings back until its repair.
        if let Some(pid) = resume_threads.iter().copied().find(|pid| {
            self.inferior
                .as_ref()
                .and_then(|inferior| inferior.threads.get(pid))
                .is_some_and(|thread| thread.awaiting_breakpoint.is_some())
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
                // A stepping thread that is exiting, or that SIGKILL took out
                // of its stop on the way to its exit, can step no further. A
                // leader exiting alone is reported only after every other
                // thread, which run on in the step's scope meanwhile.
                let exiting = self
                    .inferior
                    .as_ref()
                    .and_then(|inferior| inferior.threads.get(&thread))
                    .is_none_or(|stepping| !matches!(stepping.state, NativeThreadState::Stopped));
                if exiting {
                    return self.continue_scope_threads();
                }

                if progress_owed {
                    if let Some(ActiveKind::Step { progress_owed, .. }) = self
                        .inferior
                        .as_mut()
                        .and_then(|inferior| inferior.active.as_mut())
                        .map(|active| &mut active.kind)
                    {
                        *progress_owed = false;
                    }

                    // A thread running freely to a guard has no step to judge
                    // until it returns there.
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

                self.continue_scope_threads()
            }
            ActiveKind::Launch
            | ActiveKind::Continue
            | ActiveKind::RuntimeStep { .. }
            | ActiveKind::RuntimeAssignment { .. } => {
                for pid in resume_threads {
                    self.continue_thread(pid)?;
                }

                Ok(())
            }
        }
    }

    pub(super) fn start_next_repair(&mut self) -> Result<()> {
        let next = {
            let inferior = self.inferior.as_mut().ok_or(Error::NotRunning)?;

            // Only a stopped thread steps over a trap. One exiting, or one
            // SIGKILL took out of its stop to run to its exit, as when its
            // process ends, steps over nothing.
            let exiting = |inferior: &Inferior, pid: &Pid| {
                inferior
                    .threads
                    .get(pid)
                    .is_none_or(|thread| !matches!(thread.state, NativeThreadState::Stopped))
            };

            let group = inferior
                .repairs
                .front()
                .ok_or_else(|| Error::internal("repair group exists"))?;
            if group.current.is_some() {
                return Ok(());
            }

            let remaining = group
                .remaining
                .iter()
                .copied()
                .filter(|pid| !exiting(inferior, pid))
                .collect::<VecDeque<_>>();
            let group = inferior
                .repairs
                .front_mut()
                .ok_or_else(|| Error::internal("repair group exists"))?;
            group.remaining = remaining;
            group.remaining.pop_front().map(|pid| {
                group.current = Some(pid);
                (pid, group.address, !group.site_removed)
            })
        };

        let Some((pid, address, remove_site)) = next else {
            self.finish_repair_group()?;
            return self.advance_execution();
        };

        // A thread stopped at a breakpoint cannot also hold a pending signal:
        // a signal stop rewinds no PC and repairs nothing.
        if self
            .inferior
            .as_ref()
            .ok_or(Error::NotRunning)?
            .thread(pid)?
            .pending_signal
            .is_some()
        {
            return Err(backend_error(LinuxError::UnexpectedWait(format!(
                "repair thread {pid} has a pending signal"
            ))));
        }

        if remove_site {
            let inferior = self.inferior.as_mut().ok_or(Error::NotRunning)?;
            let pid = inferior.memory_thread();
            let group = inferior
                .repairs
                .front_mut()
                .ok_or_else(|| Error::internal("repair group exists"))?;
            self.ptrace
                .remove_breakpoint(pid, &mut inferior.breakpoints, address)?;
            group.site_removed = true;
        }

        // Stepping over the trap of a call that a step into another passes
        // enters it, to run to its return.
        self.guard_unchosen_call(pid)?;

        self.resume_native(
            pid,
            Resume::Step,
            false,
            ExpectedStop::BreakpointRepair { address },
        )
    }

    pub(super) fn finish_repair_group(&mut self) -> Result<()> {
        let inferior = self.inferior.as_mut().ok_or(Error::NotRunning)?;
        let group = inferior
            .repairs
            .pop_front()
            .ok_or_else(|| Error::internal("repair group exists"))?;
        if group.site_removed {
            self.ptrace.reinstall_breakpoint(
                inferior.memory_thread(),
                &mut inferior.breakpoints,
                group.address,
            )?;
        }

        Ok(())
    }

    pub(super) fn resume_awaiting_thread(&mut self, pid: Pid) -> Result<()> {
        let address = self
            .inferior
            .as_ref()
            .ok_or(Error::NotRunning)?
            .thread(pid)?
            .awaiting_breakpoint
            .ok_or_else(|| Error::internal("breakpoint is awaited"))?;
        self.resume_native(
            pid,
            Resume::Continue,
            true,
            ExpectedStop::AwaitBreakpoint { address },
        )
    }

    pub(super) fn continue_thread(&mut self, pid: Pid) -> Result<()> {
        let inferior = self.inferior.as_ref().ok_or(Error::NotRunning)?;
        if !matches!(inferior.thread(pid)?.state, NativeThreadState::Stopped) {
            return Ok(());
        }

        self.resume_native(pid, Resume::Continue, true, ExpectedStop::None)
    }

    /// Resumes one stopped thread and records the stop it should report next.
    ///
    /// With `deliver_signal`, the thread's pending signal is delivered and
    /// consumed; otherwise it stays pending. The thread's own stop reason is
    /// cleared either way, since it no longer describes a current stop.
    pub(super) fn resume_native(
        &mut self,
        pid: Pid,
        resume: Resume,
        deliver_signal: bool,
        expected: ExpectedStop,
    ) -> Result<()> {
        // A step that makes a system call, the user's or one over a
        // breakpoint's trap, stops at its entry and exit, which the
        // catchpoints judge.
        let stepping_call = matches!(resume, Resume::Step)
            && matches!(
                expected,
                ExpectedStop::UserStep { .. } | ExpectedStop::BreakpointRepair { .. }
            )
            && !self.catchpoints.is_empty()
            && self.steps_into_call(pid)?;

        let inferior = self.inferior.as_mut().ok_or(Error::NotRunning)?;
        let holds = inferior.holds_signals(&expected);
        let thread = inferior.thread_mut(pid)?;

        // A thread in its exit stop runs on to its exit.
        debug_assert!(
            matches!(
                thread.state,
                NativeThreadState::Stopped | NativeThreadState::Exiting
            ),
            "resuming thread {pid} from {:?}, not from a stop",
            thread.state
        );

        // A pending signal is delivered only if its policy passes it.
        let mut signal = deliver_signal
            .then(|| thread.pending_signal.map(|pending| pending.signal))
            .flatten()
            .filter(|signal| self.signals.get(*signal).pass);

        // A held signal arrives with the thread's next continue that holds
        // none, unless another signal arrives then.
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
        thread.inside_call = false;

        // Only a resume to the next system-call-stop reaches a call's exit.
        if !to_call {
            thread.syscall = None;
        }

        let result = match resume {
            _ if to_call => self.ptrace.continue_to_syscall(pid, signal),
            Resume::Continue => self.ptrace.continue_execution(pid, signal),
            Resume::Step => self.ptrace.step(pid, signal),
        };

        match result {
            Ok(()) => thread.state = NativeThreadState::Running,
            // Only SIGKILL takes a thread out of its ptrace-stop, typically
            // because a sibling resumed just before it called `exit_group`.
            // Its exit status is still delivered and retires it.
            Err(error) if is_vanished_tracee(&error) => thread.state = NativeThreadState::Exiting,
            Err(error) => return Err(error),
        }

        if deliver_signal {
            thread.pending_signal = None;
        }

        thread.expected = expected;
        thread.reason = None;

        // With no step over its trap left to run, the thread runs on.
        if thread.stopped_at_breakpoint.is_none() {
            thread.trapped_at = None;
        }

        Ok(())
    }

    pub(super) fn handle_classified_stop(&mut self, pid: Pid, stop: ClassifiedStop) -> Result<()> {
        record!("classified {pid} as {stop:?}");

        {
            let inferior = self.inferior.as_mut().ok_or(Error::NotRunning)?;
            inferior.thread_mut(pid)?.state = NativeThreadState::Stopped;
        }

        match stop {
            ClassifiedStop::ThreadStart => self.handle_thread_start(pid),
            ClassifiedStop::DebuggerRequested => {
                let thread = self
                    .inferior
                    .as_mut()
                    .ok_or(Error::NotRunning)?
                    .thread_mut(pid)?;
                thread.reason = None;
                thread.debugger_stop_pending = false;
                self.restart_after_internal(pid)
            }
            ClassifiedStop::RemovedTrap => {
                record!("{pid} executed a trap before its site was removed");
                self.inferior
                    .as_mut()
                    .ok_or(Error::NotRunning)?
                    .thread_mut(pid)?
                    .reason = None;
                self.restart_after_internal(pid)
            }

            // Resumes once the refresh has taken the trap out, and the
            // breakpoints that follow the code are installed where it went.
            ClassifiedStop::CarriedTrap => {
                record!("{pid} executed a trap carried by moved code");
                self.inferior
                    .as_mut()
                    .ok_or(Error::NotRunning)?
                    .thread_mut(pid)?
                    .reason = None;
                self.queue_module_refresh()?;
                self.restart_after_internal(pid)
            }
            ClassifiedStop::Breakpoint(address) => self.handle_breakpoint_stop(pid, address),
            // The program goes on after its own trap, which it raised for
            // the debugger; the SIGTRAP is not delivered.
            ClassifiedStop::ProgramTrap(address) => {
                self.begin_visible_stop(pid, StopReason::ProgramBreakpoint { address })
            }
            ClassifiedStop::Watch(owners) => self.handle_watch_stop(pid, owners),
            ClassifiedStop::Trace { watch } => self.handle_trace_stop(pid, watch),
            ClassifiedStop::SignalDelivery(pending) => self.handle_signal_stop(pid, pending),
            ClassifiedStop::GroupStop(signal) => {
                self.begin_visible_stop(pid, StopReason::Exception(exception_info(signal)))
            }

            // Its exit event or exit status arrives next.
            ClassifiedStop::Superseded => {
                let inferior = self.inferior.as_mut().ok_or(Error::NotRunning)?;
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
        // A thread re-executing a trap after signal delivery interrupted its
        // repair reaches a hit that was already counted. It ran alone, so
        // its siblings are stopped.
        let awaited = {
            let inferior = self.inferior.as_mut().ok_or(Error::NotRunning)?;
            let thread = inferior.thread_mut(pid)?;
            thread.stopped_at_breakpoint = Some(address);
            thread.trapped_at = Some(address);
            let awaited = thread.awaiting_breakpoint == Some(address)
                && matches!(thread.expected, ExpectedStop::AwaitBreakpoint { address: expected } if expected == address);
            if awaited {
                thread.awaiting_breakpoint = None;
                thread.expected = ExpectedStop::None;
            }

            awaited
        };

        if awaited {
            self.queue_repair(pid, address)?;
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

    /// Goes on from a trap at `address` at which no user breakpoint stops:
    /// none owns the site, each declined the hit by its hit condition, or
    /// an edit applied while the stop formed dropped every hit that
    /// stopped. Whatever else owns the site, such as the active step's
    /// plan, meets the thread there.
    fn arrive_unstopped(&mut self, pid: Pid, address: VirtualAddress) -> Result<()> {
        if let Some(reason) = self.runtime_hook_stop(pid, address) {
            return self.begin_visible_stop(pid, reason);
        }

        if self.reach_resumed_future(pid, address)? {
            return Ok(());
        }

        let inferior = self.inferior.as_ref().ok_or(Error::NotRunning)?;
        let step = inferior
            .active
            .as_ref()
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

        // A runtime step into native code ends where its own thread enters
        // the function it called.
        if let Some(kind) = self.runtime_step_arrival(pid, address) {
            return self.begin_visible_stop(pid, StopReason::Step { kind });
        }

        // A declined user hit, or another thread at a stepping plan's site,
        // which is not the user's to see. A source step that single-stepped
        // onto a declined site may end there, with the hit counted once;
        // one running freely to a guard, through a signal handler or a call
        // it passes, does not.
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

    /// Goes on with the active step at a site of its plan, which its own
    /// thread or task reached.
    fn reach_plan_site(&mut self, pid: Pid, address: VirtualAddress, kind: StepKind) -> Result<()> {
        // A step out may go on from its function's return address, so
        // what the function returned is read as it arrives there.
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
            // The step goes on by single steps, or by its new plan.
            return self.go_on_without_plan(pid, address, Some(kind));
        }

        let mode = self.step_mode(kind);

        if !steps_instructions(kind) {
            self.begin_epilogue_traversal(pid)?;
        }

        self.note_returned_activation(pid, mode)?;

        if self.source_step_returned_to_runtime(pid, mode)? {
            self.let_step_run_on()?;
            self.queue_internal_edit(Edit::ReturnToRuntime { pid })?;
            return self.go_on_without_plan(pid, address, None);
        }

        if self.source_step_returned_to_undescribed_code(pid, mode)? {
            self.let_step_run_on()?;
            return self.go_on_without_plan(pid, address, None);
        }

        if let Some(reason) = self.user_step_stop(pid, kind)? {
            // The plan's sites, this one among them, are removed when
            // the stop is published.
            return self.begin_visible_stop(pid, reason);
        }

        self.mark_epilogue_return_for_retirement(address);
        self.mark_return_guard_for_retirement(pid, address)?;
        self.repair_when_alone(pid, address)
    }

    /// Whether the active step's thread runs freely until it returns to a
    /// guard.
    fn step_runs_to_guard(&self) -> bool {
        self.inferior
            .as_ref()
            .and_then(|inferior| inferior.active.as_ref())
            .is_some_and(|active| {
                matches!(&active.kind, ActiveKind::Step { start, .. } if start.resume_guard.is_some())
            })
    }

    /// Notes a hit at a site of the debugger's own: the loader's report of
    /// a change to the loaded libraries, a stack mover, or a resolver.
    fn note_internal_sites(&mut self, pid: Pid, address: VirtualAddress) -> Result<()> {
        if self.is_loader_site(address) {
            self.queue_module_refresh()?;
        }

        self.note_stack_move(pid, address)?;
        self.note_resolver(pid, address)
    }

    /// Ends an advance whose thread reached one of its targets, unless it
    /// arrived where it stood when the advance began, and so goes on.
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

    /// Goes on from a site at which a step removed its own plan: a user
    /// breakpoint that declined this hit still owns the site, which the
    /// thread then steps over; otherwise the thread goes on by single
    /// steps of `stepping`, or runs.
    pub(super) fn go_on_without_plan(
        &mut self,
        pid: Pid,
        address: VirtualAddress,
        stepping: Option<StepKind>,
    ) -> Result<()> {
        let lifted = self
            .inferior
            .as_ref()
            .ok_or(Error::NotRunning)?
            .thread(pid)?
            .stopped_at_breakpoint
            .is_none();
        if !lifted {
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
            .inferior
            .as_mut()
            .and_then(|inferior| inferior.threads.get_mut(&pid))
            .map(|thread| std::mem::replace(&mut thread.expected, ExpectedStop::None))
            .ok_or(Error::NotRunning)?;
        let watch = self.stopping_watch_hits(pid, watch)?;
        if !watch.is_empty() {
            return self.finish_watched_instruction(pid, &expected, watch);
        }

        self.finish_traced_instruction(pid, expected)
    }

    /// Completes the single-step a thread was expected to make, which no
    /// reported watchpoint ended.
    fn finish_traced_instruction(&mut self, pid: Pid, expected: ExpectedStop) -> Result<()> {
        if self.barrier_active() {
            return self.settle_trace_during_barrier(pid, expected);
        }

        match expected {
            ExpectedStop::BreakpointRepair { address } => self.complete_repair(pid, address),
            ExpectedStop::UserStep { kind } => self.complete_user_step(pid, kind),
            other => self.begin_visible_stop(
                pid,
                StopReason::Unclassifiable {
                    description: format!("unexpected trace stop in {other:?}").into(),
                },
            ),
        }
    }

    /// Handles a debug exception a running thread took after accessing
    /// watched memory. When no watchpoint it reported stops at the access,
    /// because the access changed nothing a change watchpoint watches or a
    /// condition declined it, the thread carries on as if it had not
    /// trapped.
    fn handle_watch_stop(&mut self, pid: Pid, owners: BTreeSet<WatchpointId>) -> Result<()> {
        let expected = self
            .inferior
            .as_mut()
            .and_then(|inferior| inferior.threads.get_mut(&pid))
            .map(|thread| std::mem::replace(&mut thread.expected, ExpectedStop::None))
            .ok_or(Error::NotRunning)?;
        let owners = self.stopping_watch_hits(pid, owners)?;
        if !owners.is_empty() {
            return self.finish_watched_instruction(pid, &expected, owners);
        }

        match expected {
            ExpectedStop::BreakpointRepair { .. } | ExpectedStop::UserStep { .. } => {
                self.finish_traced_instruction(pid, expected)
            }
            expected => {
                self.inferior
                    .as_mut()
                    .ok_or(Error::NotRunning)?
                    .thread_mut(pid)?
                    .expected = expected;
                self.restart_after_internal(pid)
            }
        }
    }

    /// Stops for an instruction that accessed watched memory. A repair step's
    /// instruction has executed, which completes the repair. The stop turns
    /// internal if no hit remains to report once every thread is stopped, and
    /// a stepping thread's instruction then counts toward its step.
    pub(super) fn finish_watched_instruction(
        &mut self,
        pid: Pid,
        expected: &ExpectedStop,
        owners: BTreeMap<WatchpointId, u64>,
    ) -> Result<()> {
        let inferior = self.inferior.as_mut().ok_or(Error::NotRunning)?;
        if let ExpectedStop::BreakpointRepair { address } = *expected {
            inferior.finish_current_repair(pid, address)?;
        }

        inferior.thread_mut(pid)?.watch_hits.extend(owners);
        if matches!(
            expected,
            ExpectedStop::BreakpointRepair { .. } | ExpectedStop::UserStep { .. }
        ) {
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
        self.inferior
            .as_ref()
            .is_some_and(|inferior| inferior.barrier.is_some())
    }

    /// Records a trace stop that arrived while every thread is being
    /// stopped. A completed repair stays complete; the stepping thread's
    /// instruction is evaluated if the execution resumes.
    pub(super) fn settle_trace_during_barrier(
        &mut self,
        pid: Pid,
        expected: ExpectedStop,
    ) -> Result<()> {
        match expected {
            ExpectedStop::BreakpointRepair { address } => {
                self.inferior
                    .as_mut()
                    .ok_or(Error::NotRunning)?
                    .finish_current_repair(pid, address)?;
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

    /// Applies the signal's policy to a signal-delivery stop: a visible
    /// stop, or delivering or discarding the signal as the thread resumes
    /// what it was doing.
    pub(super) fn handle_signal_stop(&mut self, pid: Pid, pending: PendingSignal) -> Result<()> {
        let held_continue = self.take_held_continue(pending.signal, pending.sender);
        let inferior = self.inferior.as_mut().ok_or(Error::NotRunning)?;

        // The debugger's own request to end the inferior never stops it.
        let terminating = inferior
            .terminating
            .as_mut()
            .filter(|terminating| terminating.signal == pending.signal);
        let policy = if held_continue {
            SignalPolicy {
                stop: false,
                print: false,
                pass: false,
            }
        } else if let Some(terminating) = terminating {
            terminating.delivered = true;
            SignalPolicy {
                stop: false,
                print: false,
                pass: true,
            }
        } else {
            self.signals.get(pending.signal)
        };

        if policy.stop {
            return self.stop_for_signal(pid, pending);
        }

        let inferior = self.inferior.as_ref().ok_or(Error::NotRunning)?;
        if policy.print {
            let _ = self.events.send(DebuggerEvent::SignalReceived {
                revision: self.revision,
                process_id: process_id(inferior.tgid),
                thread_id: debug_thread_id(pid),
                exception: pending_exception_info(pending),
            });
        }

        let expected = inferior.thread(pid)?.expected.clone();
        let mut delivered = policy.pass.then_some(pending);
        if let Some(pending) = delivered
            .filter(|pending| inferior.holds_signals(&expected) && self.defers(pending.signal))
        {
            record!("hold {} in {pid}", pending.signal);
            self.inferior
                .as_mut()
                .ok_or(Error::NotRunning)?
                .thread_mut(pid)?
                .held_signal = Some(pending);
            delivered = None;
        }

        match expected {
            ExpectedStop::BreakpointRepair { address } => {
                self.signal_during_repair(pid, address, delivered)
            }
            ExpectedStop::UserStep { kind } => self.signal_during_step(pid, kind, delivered),
            expected => {
                self.inferior
                    .as_mut()
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
        let repair_address = self
            .inferior
            .as_ref()
            .and_then(|inferior| inferior.threads.get(&pid))
            .and_then(|thread| match thread.expected {
                ExpectedStop::BreakpointRepair { address } => Some(address),
                _ => None,
            });
        if let Some(address) = repair_address {
            self.restore_active_breakpoints()?;
            let inferior = self.inferior.as_mut().ok_or(Error::NotRunning)?;
            let thread = inferior.thread_mut(pid)?;
            thread.stopped_at_breakpoint = None;
            thread.trapped_at = None;
            thread.awaiting_breakpoint = Some(address);
        }

        let inferior = self.inferior.as_mut().ok_or(Error::NotRunning)?;
        let thread = inferior.thread_mut(pid)?;
        thread.expected = ExpectedStop::None;
        thread.pending_signal = Some(pending);
        self.begin_visible_stop(pid, StopReason::Exception(pending_exception_info(pending)))
    }

    /// Handles a signal that arrived before a thread stepped over its
    /// breakpoint site. A delivered signal runs its handler first, with the
    /// site restored so no other thread can pass it, and the thread repairs
    /// the site when it reaches it again; a discarded one repeats the step.
    fn signal_during_repair(
        &mut self,
        pid: Pid,
        address: VirtualAddress,
        delivered: Option<PendingSignal>,
    ) -> Result<()> {
        let barrier = self.barrier_active();
        let inferior = self.inferior.as_mut().ok_or(Error::NotRunning)?;
        let Some(pending) = delivered else {
            inferior.thread_mut(pid)?.pending_signal = None;
            if barrier {
                return self.finish_barrier_if_ready();
            }

            return self.resume_native(
                pid,
                Resume::Step,
                false,
                ExpectedStop::BreakpointRepair { address },
            );
        };

        let memory = inferior.memory_thread();
        if let Some(group) = inferior
            .repairs
            .front_mut()
            .filter(|group| group.address == address && group.current == Some(pid))
        {
            group.current = None;
            if group.site_removed {
                self.ptrace
                    .reinstall_breakpoint(memory, &mut inferior.breakpoints, address)?;
                group.site_removed = false;
            }
        }

        let thread = inferior.thread_mut(pid)?;
        thread.stopped_at_breakpoint = None;
        thread.trapped_at = None;
        thread.awaiting_breakpoint = Some(address);
        thread.pending_signal = Some(pending);
        thread.expected = ExpectedStop::None;
        if barrier {
            self.finish_barrier_if_ready()
        } else {
            self.resume_awaiting_thread(pid)
        }
    }

    /// Handles a signal that arrived before the stepping thread executed
    /// its next instruction. A discarded signal repeats the step. A
    /// delivered one runs its handler at full speed: a guard at the
    /// interrupted instruction resumes the step when the handler returns
    /// there, so the step never stops inside the handler.
    fn signal_during_step(
        &mut self,
        pid: Pid,
        kind: StepKind,
        delivered: Option<PendingSignal>,
    ) -> Result<()> {
        let barrier = self.barrier_active();
        let Some(pending) = delivered else {
            self.inferior
                .as_mut()
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
            address: VirtualAddress::new(registers.rip),
            stack: self.stack_position(pid, &registers),
            call: false,
        };

        let execution = self.active_execution()?;
        self.install_additional_plan_breakpoints(execution, &BTreeSet::from([guard.address]))?;
        if let Some(start) = self.active_step_mut() {
            start.resume_guard = Some(guard);
        }

        let thread = self
            .inferior
            .as_mut()
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

    /// Handles a site of a step's plan that its task reached on the way to
    /// where the step may end: the start of a task the step goes into, or
    /// the return from a signal handler. Returns whether it did.
    fn reach_waypoint(&mut self, pid: Pid, address: VirtualAddress) -> Result<bool> {
        if self.reach_new_task(pid, address)? {
            return Ok(true);
        }

        // The step's task is back on its plan, from wherever it escaped to.
        if let Some(start) = self.active_step_mut() {
            start.escape = None;
        }

        self.reach_resume_guard(pid, address)
    }

    /// Recognizes the stepping thread's return from a signal handler to the
    /// instruction it interrupted, where the step resumes. Returns whether
    /// the site was the guard and has been handled.
    fn reach_resume_guard(&mut self, pid: Pid, address: VirtualAddress) -> Result<bool> {
        let Some((execution, kind, guard, planned)) = self
            .inferior
            .as_ref()
            .and_then(|inferior| inferior.active.as_ref())
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
            // The handler itself ran the interrupted code. A site only the
            // guard owns is passed; a planned one is evaluated as usual.
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
            // The step's own site is evaluated as if no signal had arrived.
            return Ok(false);
        }

        // Removing the guard restores the interrupted instruction, which the
        // step now executes, unless a breakpoint still owns the site: then
        // the thread hit that breakpoint, and steps over its trap.
        self.remove_breakpoint_owner(address, BreakpointOwner::Plan(execution))?;
        let trapped = self
            .inferior
            .as_ref()
            .ok_or(Error::NotRunning)?
            .thread(pid)?
            .stopped_at_breakpoint
            .is_some();
        if trapped {
            // A source step that reached a breakpoint's site judges it at the
            // trap there, as a hit the breakpoint declines; the signal came
            // first, so it judges the site now.
            if !steps_instructions(kind)
                && let Some(reason) = self.user_step_stop(pid, kind)?
            {
                self.begin_visible_stop(pid, reason)?;
                return Ok(true);
            }

            self.repair_when_alone(pid, address)?;
        } else if guard.call {
            // A passed call may return to where the line ends.
            self.complete_user_step(pid, kind)?;
        } else {
            self.start_user_step(pid, kind)?;
        }

        Ok(true)
    }
}

impl<P: LinuxTraceOps> Controller<P> {
    pub(super) fn complete_repair(&mut self, pid: Pid, address: VirtualAddress) -> Result<()> {
        self.inferior
            .as_mut()
            .ok_or(Error::NotRunning)?
            .finish_current_repair(pid, address)?;

        // A repaired stepping thread executed its next instruction.
        self.note_step_progress(pid);
        self.start_next_repair()
    }

    pub(super) fn queue_repair(&mut self, pid: Pid, address: VirtualAddress) -> Result<()> {
        let inferior = self.inferior.as_mut().ok_or(Error::NotRunning)?;
        debug_assert!(
            !inferior
                .repairs
                .iter()
                .any(|group| { group.current == Some(pid) || group.remaining.contains(&pid) }),
            "thread {pid} is queued to step over the trap at {address} while it \
             already waits to step over another"
        );

        if let Some(group) = inferior
            .repairs
            .iter_mut()
            .find(|group| group.address == address)
        {
            group.remaining.push_front(pid);
        } else {
            inferior.repairs.push_front(RepairGroup {
                address,
                remaining: VecDeque::from([pid]),
                current: None,
                site_removed: false,
            });
        }

        Ok(())
    }

    /// Records a stop the client must see and begins stopping every other
    /// thread, unless a barrier is already doing so. Among coincident stops,
    /// the highest-priority reason is published.
    pub(super) fn begin_visible_stop(&mut self, pid: Pid, reason: StopReason) -> Result<()> {
        let reason = self.dropped_step_reason(self.requested_step_reason(reason));
        let inferior = self.inferior.as_mut().ok_or(Error::NotRunning)?;
        let thread = inferior.thread_mut(pid)?;
        thread.state = NativeThreadState::Stopped;
        thread.reason = Some(reason.clone());
        thread.expected = ExpectedStop::None;

        self.raise_barrier(pid, reason)
    }

    /// Publishes `reason` from `pid` once every thread is stopped. A barrier
    /// already in progress publishes the most important reason it sees.
    pub(super) fn raise_barrier(&mut self, pid: Pid, reason: StopReason) -> Result<()> {
        let inferior = self.inferior.as_mut().ok_or(Error::NotRunning)?;
        if let Some(barrier) = inferior.barrier.as_mut() {
            if barrier.reason.as_ref().is_none_or(|current| {
                visible_stop_priority(&reason) > visible_stop_priority(current)
            }) {
                barrier.triggering_thread = pid;
                barrier.reason = Some(reason);
            }
        } else {
            inferior.barrier = Some(StopBarrier::visible(pid, reason));
            self.request_stops()?;
        }

        self.finish_barrier_if_ready()
    }

    pub(super) fn recover_partial_resume(&mut self) -> Result<()> {
        self.restore_active_breakpoints()?;

        {
            let inferior = self.inferior.as_mut().ok_or(Error::NotRunning)?;
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
            inferior.barrier = Some(StopBarrier::visible(triggering_thread, reason));
        }

        self.request_stops()?;
        self.finish_barrier_if_ready()
    }

    pub(super) fn request_stops(&mut self) -> Result<()> {
        let inferior = self.inferior.as_mut().ok_or(Error::NotRunning)?;
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
                // Any ptrace-stop after an interrupt, such as a clone event,
                // satisfies it without the stop it asked for, so a seized
                // thread is always asked again. Interrupts coalesce, and one
                // that outlives its barrier is absorbed when it stops.
                let _ = self.ptrace.interrupt(pid)?;
            } else if !thread.debugger_stop_pending {
                // A clone or another ptrace event can stop this thread and
                // satisfy an earlier barrier before its requested SIGSTOP is
                // delivered. Standard signals coalesce, so retain that
                // outstanding request instead of sending an indistinguishable
                // duplicate for the next barrier.
                match self.ptrace.request_stop(tgid, pid) {
                    // The waiter already reaped the thread, whose exit
                    // status is on its way and retires it.
                    Err(error) if is_vanished_tracee(&error) => {}
                    result => result?,
                }
            }

            thread.debugger_stop_pending = true;
            thread.state = NativeThreadState::StopRequested;
        }

        Ok(())
    }

    /// The thread that presents a stop `triggering_thread` caused. A leader
    /// that exited alone, as one a pause asked to stop, has no frame to
    /// present; a stopped thread presents the stop instead.
    fn presenting_thread(&self, triggering_thread: Pid) -> Pid {
        self.inferior
            .as_ref()
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

    /// Applies the edits requested while the stop formed, then settles the
    /// reasons of threads whose breakpoint or watchpoint they changed.
    ///
    /// Returns the trap at which the active step's thread stopped for a
    /// breakpoint hit the edits dropped. A hit that stops preempts what
    /// else owns the site, such as the step's own plan, so the step has
    /// yet to meet the thread there.
    fn apply_pending_edits(&mut self) -> Result<Option<(Pid, VirtualAddress)>> {
        let edits = self
            .inferior
            .as_mut()
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
            self.inferior
                .as_ref()
                .and_then(|inferior| inferior.threads.get(pid))
                .is_some_and(|thread| thread.reason.is_none())
        }))
    }

    /// The thread running the active step and the site at which a user
    /// breakpoint's hit stopped it, if one did.
    fn stepping_thread_hit(&self) -> Option<(Pid, VirtualAddress)> {
        let inferior = self.inferior.as_ref()?;
        let ActiveKind::Step { owner, .. } = inferior.active.as_ref()?.kind else {
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
        let ready = self.inferior.as_ref().is_some_and(|inferior| {
            inferior.barrier.is_some()
                && inferior
                    .threads
                    .iter()
                    .all(|(&pid, thread)| inferior.settled(pid, thread))
        });
        if !ready {
            return Ok(());
        }

        // A vfork child runs in the memory its parent's traps are in, so the
        // stop waits until that child execs or exits.
        if self.settle_vfork_windows()? {
            return Ok(());
        }

        // From the last thread stopping to the stop's publication.
        let _span = crate::span!("stop");
        if self.attach_reply.is_some() {
            if self.seize_untraced_threads()? {
                return Ok(());
            }

            self.initialize_attached_inferior()?;
        } else if self.drain_queued_traps()? {
            return Ok(());
        }

        self.restore_active_breakpoints()?;

        if let Some((pid, address)) = self.apply_pending_edits()? {
            // The step meets its thread at the trap now. However it goes
            // on, it completes this stop again, with no edit left to drop
            // a hit.
            return self.arrive_unstopped(pid, address);
        }

        if self
            .inferior
            .as_ref()
            .and_then(|inferior| inferior.barrier.as_ref())
            .is_some_and(|barrier| barrier.reason.is_none())
        {
            return self.resume_after_internal_stop();
        }

        if let Some(execution) = self
            .inferior
            .as_ref()
            .and_then(|inferior| inferior.active.as_ref())
            .map(|active| active.id)
        {
            self.cleanup_plan_breakpoints(execution)?;
        }

        self.end_runtime_step();
        self.end_runtime_assignment();

        // An image exec(2) replaced has no loader rendezvous to follow.
        let exec_replaced = self
            .inferior
            .as_ref()
            .is_some_and(|inferior| inferior.exec_unsupported);
        if !exec_replaced {
            self.refresh_libraries()?;
        }

        self.evaluate_watchpoints()?;
        self.refresh_thread_names();

        let (triggering_thread, reason) = self
            .inferior
            .as_ref()
            .and_then(|inferior| inferior.barrier.as_ref())
            .and_then(|barrier| Some((barrier.triggering_thread, barrier.reason.clone()?)))
            .ok_or_else(|| Error::internal("ready barrier publishes a reason"))?;
        self.publish_stop(triggering_thread, reason)
    }

    /// Publishes the stop a ready barrier makes, which `triggering_thread`
    /// made for `reason`.
    fn publish_stop(&mut self, triggering_thread: Pid, reason: StopReason) -> Result<()> {
        let triggering_thread = self.presenting_thread(triggering_thread);
        let presentation = self.presentation_for_thread(triggering_thread, Some(&reason))?;
        let returned = self.capture_returned(triggering_thread, &reason);

        // Last, so that a process ending while the stop forms keeps the
        // temporary breakpoints of a stop never published.
        let temporaries_removed = self.remove_stopped_temporaries()?;
        let stop_id = self.ptrace.allocate_stop_id()?;
        let inferior = self.inferior.as_mut().ok_or(Error::NotRunning)?;
        check_publishable(inferior, stop_id);

        for thread in inferior.threads.values_mut() {
            thread.expected = ExpectedStop::None;
        }

        inferior.barrier = None;

        // Once the request to end has been delivered, a stop hands the
        // signal back to its policy.
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
        inferior.public_stop = Some(stop);

        for installation in inferior.agents.values_mut() {
            installation.pause_deadline(std::time::Instant::now());
        }

        let execution = inferior.active.take().map(|active| active.id);
        let process_id = process_id(inferior.tgid);
        // A stop an agent reported is about the program's frame, not the
        // agent's hook it entered; and a store to an interpreted variable is
        // about the variable's frame, not the interpreter's code that made
        // it.
        let reported = match reason {
            StopReason::LanguageException(_) | StopReason::Breakpoint { address: None, .. } => true,
            StopReason::Step { .. } | StopReason::StepIncomplete { .. } => {
                self.agent_hook(triggering_thread).is_some()
            }
            _ => false,
        };

        let watched = match reason {
            StopReason::Watchpoint { ref hits } => hits.first().and_then(|hit| {
                match self
                    .inferior
                    .as_ref()?
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

        if self.attach_reply.is_some() {
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

        // The temporary breakpoints the stop deleted are gone from every
        // snapshot of it; their change is published after it.
        if temporaries_removed {
            self.publish_breakpoints_changed();
        }

        if let Some(reply) = self.attach_reply.take() {
            let _ = reply.send(Ok(stop_id));
        }

        self.answer_assignment(stop_id, &reason_published);

        Ok(())
    }

    /// Reads every thread's name, which it may have changed while running.
    pub(super) fn refresh_thread_names(&mut self) {
        let Some(inferior) = self.inferior.as_mut() else {
            return;
        };

        for (&pid, thread) in &mut inferior.threads {
            thread.name = self.ptrace.thread_name(inferior.tgid, pid);
        }
    }

    /// Resumes a thread whose stop publishes nothing as it was going, or
    /// leaves it stopped for the barrier in progress.
    pub(super) fn restart_after_internal(&mut self, pid: Pid) -> Result<()> {
        if self.barrier_active() {
            return self.finish_barrier_if_ready();
        }

        let expected = self
            .inferior
            .as_ref()
            .and_then(|inferior| inferior.threads.get(&pid))
            .map(|thread| thread.expected.clone())
            .ok_or(Error::NotRunning)?;
        match expected {
            ExpectedStop::UserStep { kind } => self.start_user_step(pid, kind),
            ExpectedStop::BreakpointRepair { address } => self.resume_native(
                pid,
                Resume::Step,
                false,
                ExpectedStop::BreakpointRepair { address },
            ),
            ExpectedStop::AwaitBreakpoint { .. } => self.resume_awaiting_thread(pid),
            ExpectedStop::InitialExec
            | ExpectedStop::AdoptedExec
            | ExpectedStop::InitialAttach
            | ExpectedStop::None => self.continue_thread(pid),
        }
    }
}

pub(super) fn collect_repairs(inferior: &Inferior) -> VecDeque<RepairGroup> {
    let Some(active) = inferior.active.as_ref() else {
        // Without an execution no thread resumes, so none steps over a trap.
        return VecDeque::new();
    };

    let mut grouped: BTreeMap<VirtualAddress, VecDeque<Pid>> = BTreeMap::new();

    for &pid in &active.resume_threads {
        if let Some(address) = inferior
            .threads
            .get(&pid)
            .and_then(|thread| thread.stopped_at_breakpoint)
        {
            grouped.entry(address).or_default().push_back(pid);
        }
    }

    grouped
        .into_iter()
        .map(|(address, remaining)| RepairGroup {
            address,
            remaining,
            current: None,
            site_removed: false,
        })
        .collect()
}

/// Panics if `inferior` is not in the state a stop is published from: no
/// stop already published, every thread stopped, every trap in place, no
/// repair outstanding, and none of the active execution's traps left.
fn check_publishable(inferior: &Inferior, stop_id: StopId) {
    debug_assert!(
        inferior.public_stop.is_none(),
        "publishing stop {stop_id} over stop {:?}",
        inferior.public_stop.as_ref().map(|stop| stop.id)
    );
    for (&pid, thread) in &inferior.threads {
        debug_assert!(
            inferior.settled(pid, thread),
            "publishing stop {stop_id} while thread {pid} is {:?}",
            thread.state
        );
    }

    debug_assert!(
        inferior.repairs.is_empty(),
        "publishing stop {stop_id} with {} breakpoint repairs outstanding",
        inferior.repairs.len()
    );

    let execution = inferior.active.as_ref().map(|active| active.id);
    for (&address, site) in &inferior.breakpoints {
        debug_assert!(
            site.installed,
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

    debug_assert!(
        inferior
            .barrier
            .as_ref()
            .is_none_or(|barrier| barrier.edits.is_empty()),
        "publishing stop {stop_id} drops edits queued after the stop's edits were applied"
    );
}
