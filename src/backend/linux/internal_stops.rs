//! Internal all-stops: stopping every thread without publishing a stop, so
//! breakpoints and watchpoints can be edited, and breakpoint sites repaired,
//! while the inferior runs.
//!
//! An internal stop is a [`StopBarrier`] without a reason. Once every thread
//! is stopped its edits apply, and execution resumes under the same
//! [`ExecutionId`](crate::ExecutionId) without any stop or continue event.
//! A thread that stops for a visible reason meanwhile, such as a breakpoint
//! or a signal, gives the barrier that reason, and the stop is published as
//! usual once the edits apply.

use nix::unistd::Pid;

use crate::protocol::StopReason;
use crate::{Error, VirtualAddress};

use super::classify::visible_stop_priority;
use super::native::LinuxTraceOps;
use super::run_control::collect_repairs;
use super::{
    ActiveKind, Controller, Edit, Inferior, NativeThreadState, Result, StopBarrier, debug_thread_id,
};

impl<P: LinuxTraceOps> Controller<P> {
    /// Applies a breakpoint or watchpoint edit once every thread is stopped.
    ///
    /// At a published stop, before launch, and while a launch or attach is
    /// still establishing its first stop, the edit applies at once. While
    /// the inferior runs, every thread is stopped first.
    pub(super) fn edit(&mut self, edit: Edit) {
        if self.shutting_down {
            edit.reject(Error::RequestCancelled);
            return;
        }

        let running = self
            .inferior
            .as_ref()
            .is_some_and(|inferior| inferior.public_stop.is_none())
            && self.launch_reply.is_none()
            && self.attach_reply.is_none();
        if !running {
            self.apply_edit(edit);
            return;
        }

        if let Err(error) = self.begin_internal_stop() {
            edit.reject(error);
            return;
        }

        let Some(barrier) = self
            .inferior
            .as_mut()
            .and_then(|inferior| inferior.barrier.as_mut())
        else {
            edit.reject(Error::internal("an internal stop has a barrier"));
            return;
        };

        barrier.edits.push(edit);
        if let Err(error) = self.finish_barrier_if_ready() {
            self.fail_inferior(error);
        }
    }

    /// Answers the edits an internal stop still held when its process
    /// ended. They change the logical breakpoints and watchpoints, which
    /// outlive any process, unless the session is ending.
    pub(super) fn settle_pending_edits(&mut self, edits: Vec<Edit>) {
        for edit in edits {
            if self.shutting_down {
                edit.reject(Error::RequestCancelled);
            } else {
                self.apply_edit(edit);
            }
        }
    }

    /// Whether breakpoint traps and debug registers can be written now:
    /// the inferior has completed its first stop and every thread is
    /// settled.
    pub(super) fn sites_live(&self) -> bool {
        self.launch_reply.is_none()
            && self.attach_reply.is_none()
            && self.inferior.as_ref().is_some_and(|inferior| {
                inferior
                    .threads
                    .iter()
                    .all(|(&pid, thread)| inferior.settled(pid, thread))
            })
    }

    pub(super) fn apply_edit(&mut self, edit: Edit) {
        match edit {
            Edit::AddBreakpoint {
                spec,
                options,
                reply,
            } => {
                let _ = reply.send(self.add_breakpoint(spec, *options));
            }
            Edit::SetBreakpointEnabled { id, enabled, reply } => {
                let _ = reply.send(self.set_breakpoint_enabled(id, enabled));
            }
            Edit::RemoveBreakpoint { id, reply } => {
                let _ = reply.send(self.remove_breakpoint(id));
            }
            Edit::RemoveAllBreakpoints { reply } => {
                let _ = reply.send(self.remove_all_breakpoints());
            }

            Edit::AddWatchpoint {
                spec,
                access,
                options,
                reply,
            } => {
                let _ = reply.send(self.add_watchpoint(spec, access, options));
            }
            Edit::SetWatchpointEnabled { id, enabled, reply } => {
                let _ = reply.send(self.set_watchpoint_enabled(id, enabled));
            }
            Edit::RemoveWatchpoint { id, reply } => {
                let _ = reply.send(self.remove_watchpoint(id));
            }
            Edit::RemoveAllWatchpoints { reply } => {
                let _ = reply.send(self.remove_all_watchpoints());
            }
            Edit::SetExceptionStops { stops, reply } => {
                let _ = reply.send(self.set_exception_stops(stops));
            }

            Edit::AddCatchpoint {
                id,
                calls,
                phase,
                condition,
                reply,
            } => {
                let _ = reply.send(Ok(self.add_catchpoint(id, calls, phase, condition)));
            }
            Edit::RemoveCatchpoint { id, reply } => {
                let _ = reply.send(self.remove_catchpoint(id));
            }
            Edit::RefreshModules => {
                // A failed refresh leaves the previous modules; the next stop
                // refreshes them again.
                let _ = self.refresh_libraries();
            }
            Edit::FollowStacks => self.follow_stacks(),
            Edit::AgentReport {
                runtime,
                generation,
                failure,
            } => self.apply_agent_report(runtime, generation, failure.as_ref()),
            Edit::ArmAgents => {
                if self.sync_runtime_arming() {
                    self.publish_breakpoints_changed();
                }
            }
            Edit::ReturnToRuntime { pid } => self.return_to_runtime(pid),
        }
    }

    /// Applies an edit of the debugger's own once every thread is stopped,
    /// by an internal stop unless one is already under way. An edit already
    /// queued is applied once.
    pub(super) fn queue_internal_edit(&mut self, edit: Edit) -> Result<()> {
        if self
            .inferior
            .as_ref()
            .ok_or(Error::NotRunning)?
            .barrier
            .is_none()
        {
            self.begin_internal_stop()?;
        }

        let barrier = self
            .inferior
            .as_mut()
            .and_then(|inferior| inferior.barrier.as_mut())
            .ok_or_else(|| Error::internal("an internal stop has a barrier"))?;

        let kind = std::mem::discriminant(&edit);
        if !barrier
            .edits
            .iter()
            .any(|queued| std::mem::discriminant(queued) == kind)
        {
            barrier.edits.push(edit);
        }

        Ok(())
    }

    /// Starts stopping every running thread without a reason to publish,
    /// unless a barrier is already doing so. The caller completes it with
    /// [`Self::finish_barrier_if_ready`].
    pub(super) fn begin_internal_stop(&mut self) -> Result<()> {
        let inferior = self.inferior.as_mut().ok_or(Error::NotRunning)?;
        if inferior.barrier.is_some() {
            return Ok(());
        }

        let triggering_thread = *inferior.threads.keys().next().ok_or(Error::NotRunning)?;
        inferior.barrier = Some(StopBarrier {
            triggering_thread,
            reason: None,
            paused: false,
            ended: None,
            edits: Vec::new(),
        });

        // An agent's acknowledgement budget counts only running time.
        for installation in inferior.agents.values_mut() {
            installation.pause_deadline(std::time::Instant::now());
        }
        self.request_stops()
    }

    /// Resumes the active execution from a completed internal stop: steps
    /// every thread stopped at a breakpoint over it, then resumes the
    /// execution's threads as it had them running.
    ///
    /// A thread whose slots another user took can't be armed, and running
    /// it would silently miss accesses the user asked to watch. The stop is
    /// then published from it, as for a new thread that can't be armed.
    pub(super) fn resume_after_internal_stop(&mut self) -> Result<()> {
        let inferior = self.inferior.as_mut().ok_or(Error::NotRunning)?;
        inferior.barrier = None;
        if inferior.active.is_none() {
            return Ok(());
        }
        for installation in inferior.agents.values_mut() {
            installation.start_deadline(std::time::Instant::now());
        }

        match self.sync_debug_registers() {
            Err(error @ Error::WatchpointHardwareBusy { thread }) => {
                let inferior = self.inferior.as_ref().ok_or(Error::NotRunning)?;
                let pid = inferior
                    .threads
                    .keys()
                    .copied()
                    .find(|&pid| debug_thread_id(pid) == thread)
                    .ok_or_else(|| Error::internal("an unarmed thread is the inferior's"))?;
                return self.begin_visible_stop(
                    pid,
                    StopReason::WatchpointArmFailed {
                        thread_id: thread,
                        description: error.to_string().into(),
                    },
                );
            }
            result => result?,
        }

        let inferior = self.inferior.as_mut().ok_or(Error::NotRunning)?;
        inferior.repairs = collect_repairs(inferior);
        self.advance_execution()
    }

    /// Steps a thread over the breakpoint site it stopped at, which no
    /// published stop reports. Removing the site lets any other running
    /// thread pass it unnoticed, so every other thread is stopped first.
    pub(super) fn repair_when_alone(&mut self, pid: Pid, address: VirtualAddress) -> Result<()> {
        let inferior = self.inferior.as_mut().ok_or(Error::NotRunning)?;
        inferior.thread_mut(pid)?.stopped_at_breakpoint = Some(address);
        if inferior.barrier.is_none() && others_stopped(inferior, pid) {
            self.queue_repair(pid, address)?;
            return self.start_next_repair();
        }

        // The repair runs when the stop resumes execution.
        self.begin_internal_stop()?;
        self.finish_barrier_if_ready()
    }

    /// Records that the stepping thread executed an instruction whose
    /// effect on the step is evaluated before the step resumes.
    pub(super) fn note_step_progress(&mut self, pid: Pid) {
        let owner = self
            .inferior
            .as_ref()
            .and_then(|inferior| inferior.active.as_ref())
            .and_then(|active| match active.kind {
                ActiveKind::Step { owner, .. } => Some(owner),
                _ => None,
            });
        if !owner.is_some_and(|owner| self.runs_step(owner, pid)) {
            return;
        }

        if let Some(ActiveKind::Step { progress_owed, .. }) = self
            .inferior
            .as_mut()
            .and_then(|inferior| inferior.active.as_mut())
            .map(|active| &mut active.kind)
        {
            *progress_owed = true;
        }
    }

    /// Resumes every stopped thread of the active execution's scope, unless
    /// a barrier is stopping them or the execution has ended.
    pub(super) fn continue_scope_threads(&mut self) -> Result<()> {
        let Some(inferior) = self.inferior.as_ref() else {
            return Ok(());
        };

        if inferior.barrier.is_some() {
            return Ok(());
        }

        let Some(active) = inferior.active.as_ref() else {
            return Ok(());
        };

        let threads = active
            .resume_threads
            .iter()
            .copied()
            .filter(|pid| {
                inferior
                    .threads
                    .get(pid)
                    .is_some_and(|thread| matches!(thread.state, NativeThreadState::Stopped))
            })
            .collect::<Vec<_>>();

        for pid in threads {
            self.continue_thread(pid)?;
        }

        Ok(())
    }

    /// Reconciles the threads' stop reasons with the edits the barrier just
    /// applied and the watched bytes as they now are.
    ///
    /// As with gdb's moribund locations, a hit whose breakpoint or watchpoint
    /// was removed or disabled while it was reported is dropped, and so is a change
    /// watchpoint's hit once another thread restored the bytes. The barrier
    /// then publishes the next most important reason, or turns internal.
    pub(super) fn settle_edited_reasons(&mut self) -> Result<()> {
        let unchanged = self.unchanged_watchpoints(
            self.inferior
                .iter()
                .flat_map(|inferior| inferior.threads.values())
                .flat_map(|thread| thread.watch_hits.keys().copied()),
        )?;

        // A stop holds a hit or two, so looking each up beats indexing every
        // breakpoint at every stop.
        let breakpoints = &self.breakpoints;
        let exists = |id| {
            breakpoints
                .iter()
                .any(|breakpoint| breakpoint.id == id && breakpoint.enabled)
        };

        let Some(inferior) = self.inferior.as_mut() else {
            return Ok(());
        };

        let Some(resumed) = inferior
            .active
            .as_ref()
            .map(|active| active.resume_threads.clone())
        else {
            return Ok(());
        };

        for (pid, thread) in &mut inferior.threads {
            if !resumed.contains(pid) {
                continue;
            }

            thread.watch_hits.retain(|id, _| {
                inferior
                    .watch
                    .watchpoints
                    .get(id)
                    .is_some_and(|record| record.watchpoint.enabled)
                    && !unchanged.contains(id)
            });

            match &thread.reason {
                Some(StopReason::Breakpoint { address, hits }) => {
                    let hits = hits
                        .iter()
                        .copied()
                        .filter(|hit| exists(hit.breakpoint))
                        .collect::<Vec<_>>();
                    let address = *address;
                    thread.reason = (!hits.is_empty()).then(|| StopReason::Breakpoint {
                        address,
                        hits: hits.into(),
                    });
                }
                Some(StopReason::Watchpoint { .. }) if thread.watch_hits.is_empty() => {
                    thread.reason = None;
                }
                _ => {}
            }
        }

        let Some(barrier) = inferior.barrier.as_mut() else {
            return Ok(());
        };

        if !matches!(
            barrier.reason,
            Some(StopReason::Breakpoint { .. } | StopReason::Watchpoint { .. })
        ) {
            return Ok(());
        }

        // A barrier publishes its triggering thread's own reason.
        barrier.reason = inferior
            .threads
            .get(&barrier.triggering_thread)
            .and_then(|thread| thread.reason.clone());
        if barrier.reason.is_some() {
            return Ok(());
        }

        // The published hit is gone; another thread's reason, if any, takes
        // its place, then the end of an execution that cannot resume, and a
        // pause the hit outranked otherwise.
        if let Some((pid, reason)) = inferior
            .threads
            .iter()
            .filter(|(pid, _)| resumed.contains(pid))
            .filter_map(|(&pid, thread)| thread.reason.clone().map(|reason| (pid, reason)))
            .max_by_key(|(_, reason)| visible_stop_priority(reason))
        {
            barrier.triggering_thread = pid;
            barrier.reason = Some(reason);
        } else if let Some(reason) = barrier.ended.clone() {
            barrier.reason = Some(reason);
        } else if barrier.paused {
            barrier.reason = Some(StopReason::Pause);
        }

        Ok(())
    }
}

impl Edit {
    /// Answers the edit's client with `error` without applying it.
    fn reject(self, error: Error) {
        match self {
            Self::AddBreakpoint { reply, .. }
            | Self::SetBreakpointEnabled { reply, .. }
            | Self::RemoveBreakpoint { reply, .. } => {
                let _ = reply.send(Err(error));
            }
            Self::RemoveAllBreakpoints { reply } => {
                let _ = reply.send(Err(error));
            }

            Self::AddWatchpoint { reply, .. }
            | Self::SetWatchpointEnabled { reply, .. }
            | Self::RemoveWatchpoint { reply, .. } => {
                let _ = reply.send(Err(error));
            }
            Self::RemoveAllWatchpoints { reply } => {
                let _ = reply.send(Err(error));
            }
            Self::SetExceptionStops { reply, .. } => {
                let _ = reply.send(Err(error));
            }
            Self::AddCatchpoint { reply, .. } | Self::RemoveCatchpoint { reply, .. } => {
                let _ = reply.send(Err(error));
            }
            Self::RefreshModules
            | Self::FollowStacks
            | Self::ArmAgents
            | Self::ReturnToRuntime { .. }
            | Self::AgentReport { .. } => {}
        }
    }
}

/// Whether every thread except `pid` is stopped.
fn others_stopped(inferior: &Inferior, pid: Pid) -> bool {
    inferior
        .threads
        .iter()
        .all(|(&other, thread)| other == pid || inferior.settled(other, thread))
}
