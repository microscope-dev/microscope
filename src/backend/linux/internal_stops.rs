//! Breakpoint, watchpoint, and runtime mutations cannot be applied while threads are running;
//! when requested mid-run, they trigger an internal `StopBarrier`, execute once all threads
//! settle, and transparently resume unless a visible stop occurred concurrently.

use nix::unistd::Pid;

use crate::protocol::StopReason;
use crate::{Error, VirtualAddress};

use super::classify::visible_stop_priority;
use super::native::LinuxTraceOps;
use super::trap::AtTrap;
use super::{
    ActiveKind, Controller, Edit, Inferior, NativeThreadState, Result, StopBarrier, debug_thread_id,
};

impl<P: LinuxTraceOps> Controller<P> {
    pub(super) fn edit(&mut self, edit: Edit) {
        if self.shutdown.is_some() {
            edit.reject(Error::RequestCancelled);
            return;
        }

        let running = self
            .session
            .inferior()
            .is_some_and(|inferior| inferior.run.stop().is_none())
            && !self.session.starting();
        if !running {
            self.apply_edit(edit);
            return;
        }

        if let Err(error) = self.begin_internal_stop() {
            edit.reject(error);
            return;
        }

        let Some(barrier) = self
            .session
            .inferior_mut()
            .and_then(|inferior| inferior.run.barrier_mut())
        else {
            edit.reject(Error::internal("an internal stop has a barrier"));
            return;
        };

        barrier.queue(edit);
        if let Err(error) = self.finish_barrier_if_ready() {
            self.fail_inferior(error);
        }
    }

    pub(super) fn settle_pending_edits(&mut self, edits: Vec<Edit>) {
        for edit in edits {
            if self.shutdown.is_some() {
                edit.reject(Error::RequestCancelled);
            } else {
                self.apply_edit(edit);
            }
        }
    }

    pub(super) fn sites_live(&self) -> bool {
        !self.session.starting()
            && self.session.inferior().is_some_and(|inferior| {
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
                let _ = self.refresh_libraries();
            }
            Edit::FollowStacks => self.follow_stacks(),
            Edit::FollowInterpreted => self.follow_interpreted(),
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
            Edit::InspectorPaused if !self.inspector_cleanup_running() => {
                self.apply_inspector_pauses();
            }
            Edit::InspectorPaused => {}
            Edit::InspectorAssigned => self.apply_inspector_assignment(),
            Edit::InspectorCleanup => self.apply_inspector_cleanup(),
        }
    }

    pub(super) fn queue_internal_edit(&mut self, edit: Edit) -> Result<()> {
        if self
            .session
            .inferior()
            .ok_or(Error::NotRunning)?
            .run
            .barrier()
            .is_none()
        {
            self.begin_internal_stop()?;
        }

        let barrier = self
            .session
            .inferior_mut()
            .and_then(|inferior| inferior.run.barrier_mut())
            .ok_or_else(|| Error::internal("an internal stop has a barrier"))?;

        barrier.queue_once(edit);
        Ok(())
    }

    pub(super) fn begin_internal_stop(&mut self) -> Result<()> {
        let inferior = self.session.inferior_mut().ok_or(Error::NotRunning)?;
        if inferior.run.barrier().is_some() {
            return Ok(());
        }

        let triggering_thread = *inferior.threads.keys().next().ok_or(Error::NotRunning)?;
        inferior
            .run
            .gather(StopBarrier::internal(triggering_thread))?;

        for installation in inferior.agents.values_mut() {
            installation.pause_deadline(std::time::Instant::now());
        }
        self.request_stops()
    }

    pub(super) fn resume_after_internal_stop(&mut self) -> Result<()> {
        let inferior = self.session.inferior_mut().ok_or(Error::NotRunning)?;
        if !inferior.run.resume() {
            return Ok(());
        }
        for installation in inferior.agents.values_mut() {
            installation.start_deadline(std::time::Instant::now());
        }

        match self.sync_debug_registers() {
            Err(error @ Error::WatchpointHardwareBusy { thread }) => {
                let inferior = self.session.inferior().ok_or(Error::NotRunning)?;
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

        self.advance_execution()
    }

    pub(super) fn repair_when_alone(&mut self, pid: Pid, address: VirtualAddress) -> Result<()> {
        let inferior = self.session.inferior_mut().ok_or(Error::NotRunning)?;
        debug_assert_eq!(
            inferior.thread(pid)?.trap,
            AtTrap::MustStepOver { at: address },
            "thread {pid} steps over a trap it does not stand at"
        );
        debug_assert!(
            inferior
                .run
                .execution()
                .is_some_and(|active| active.resume_threads.contains(&pid)),
            "thread {pid} steps over the trap at {address} outside the execution"
        );
        if inferior.run.barrier().is_none() && others_stopped(inferior, pid) {
            return self.start_next_repair();
        }

        self.begin_internal_stop()?;
        self.finish_barrier_if_ready()
    }

    pub(super) fn note_step_progress(&mut self, pid: Pid) {
        let owner = self
            .session
            .inferior()
            .and_then(|inferior| inferior.run.execution())
            .and_then(|active| match active.kind {
                ActiveKind::Step { owner, .. } => Some(owner),
                _ => None,
            });
        if !owner.is_some_and(|owner| self.runs_step(owner, pid)) {
            return;
        }

        if let Some(ActiveKind::Step { progress_owed, .. }) = self
            .session
            .inferior_mut()
            .and_then(|inferior| inferior.run.execution_mut())
            .map(|active| &mut active.kind)
        {
            *progress_owed = true;
        }
    }

    pub(super) fn continue_scope_threads(&mut self) -> Result<()> {
        let Some(inferior) = self.session.inferior() else {
            return Ok(());
        };

        if inferior.run.barrier().is_some() {
            return Ok(());
        }

        let Some(active) = inferior.run.execution() else {
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

    /// Re-evaluates pending thread stop reasons after deferred edits run at a barrier,
    /// dropping hits for removed/disabled breakpoints or watchpoints and updating the barrier.
    pub(super) fn settle_edited_reasons(&mut self) -> Result<()> {
        let unchanged = self.unchanged_watchpoints(
            self.session
                .inferior()
                .into_iter()
                .flat_map(|inferior| inferior.threads.values())
                .flat_map(|thread| thread.watch_hits.keys().copied()),
        )?;

        let breakpoints = &self.breakpoints;
        let exists = |id| {
            breakpoints
                .iter()
                .any(|breakpoint| breakpoint.id == id && breakpoint.enabled)
        };

        let Some(inferior) = self.session.inferior_mut() else {
            return Ok(());
        };

        let Some(resumed) = inferior
            .run
            .execution()
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

        let Some(triggering_thread) = inferior.run.barrier().map(StopBarrier::triggering_thread)
        else {
            return Ok(());
        };

        let own = inferior
            .threads
            .get(&triggering_thread)
            .and_then(|thread| thread.reason.clone());
        let best = inferior
            .threads
            .iter()
            .filter(|(pid, _)| resumed.contains(pid))
            .filter_map(|(&pid, thread)| thread.reason.clone().map(|reason| (pid, reason)))
            .max_by_key(|(_, reason)| visible_stop_priority(reason));
        if let Some(barrier) = inferior.run.barrier_mut() {
            barrier.settle_dropped_hit(own, best);
        }

        Ok(())
    }
}

impl Edit {
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
            | Self::FollowInterpreted
            | Self::ArmAgents
            | Self::ReturnToRuntime { .. }
            | Self::InspectorPaused
            | Self::InspectorAssigned
            | Self::InspectorCleanup
            | Self::AgentReport { .. } => {}
        }
    }
}

fn others_stopped(inferior: &Inferior, pid: Pid) -> bool {
    inferior
        .threads
        .iter()
        .all(|(&other, thread)| other == pid || inferior.settled(other, thread))
}
