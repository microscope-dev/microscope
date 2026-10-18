//! Hardware watchpoints: arming debug registers, attributing hits, and
//! invalidating watchpoints whose storage's lifetime ended.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use super::signals::Signal;
use nix::errno::Errno;
use nix::unistd::Pid;

use crate::backend::linux::debug_registers;
use crate::debug_info::StorageClass;
use crate::debug_info::VariableRuntimeError;
use crate::protocol::{
    ConditionOwner, DebuggerEvent, FrameScopeEvidence, HitCondition, InvalidatedWatchpoint,
    StopReason, WatchAccess, WatchScope, Watchpoint, WatchpointHit, WatchpointId,
    WatchpointInvalidation, WatchpointOptions, WatchpointSpec,
};
use crate::runtime_model::InterpretedSlot;

use crate::unwind::{CallerProvider, CallerResult, DEFAULT_MAX_FRAMES, FrameContext};
use crate::{
    AddressRange, Error, ImageAddress, InspectedValue, MemoryReadCompletion, Result, StackFrameId,
    UnwindTermination, VariableValueSource, VirtualAddress,
};

use super::debug_registers::{DebugRegisterPlan, SlotAccess};
use super::frames::{DwarfCallerProvider, StackRoot, frame_lookup_address};
use super::memory::{PtraceMemory, read_logical_memory};
use super::native::{InspectionOps, LinuxTraceOps, is_vanished_tracee};
use super::registers::x86_64_registers;
use super::{
    Controller, Inferior, LinuxError, NativeThreadState, WatchRecord, backend_error, debug_pid,
    debug_thread_id, validate_image_current,
};

/// The watched bytes as a watch observes them, and the value an
/// interpreted variable's bytes name, as its runtime presents it.
type Observation = (Option<Arc<[u8]>>, Option<Arc<str>>);

impl<P: LinuxTraceOps> Controller<P> {
    pub(super) fn add_watchpoint(
        &mut self,
        spec: WatchpointSpec,
        access: WatchAccess,
        options: WatchpointOptions,
    ) -> Result<Watchpoint> {
        let slot_access = match access {
            // A change is judged after the store that the hardware traps.
            WatchAccess::Change | WatchAccess::Write => SlotAccess::Write,
            WatchAccess::ReadWrite => SlotAccess::ReadWrite,
            WatchAccess::Read => return Err(Error::UnsupportedWatchAccess(access)),
        };

        let inferior = self.inferior.as_ref().ok_or(Error::NotRunning)?;
        if !self.sites_live() {
            return Err(Error::NotStopped);
        }

        validate_image_current(inferior)?;
        let (expression, address, byte_size, type_info, scope, frame, code) = match spec {
            WatchpointSpec::Target(target) => {
                let target = *target;

                // A target is armed only at the stop that resolved it, not
                // at a later stop or while the inferior runs.
                if inferior.public_stop.as_ref().map(|stop| stop.id) != Some(target.stop_id) {
                    return Err(Error::StaleStop);
                }

                (
                    Some(target.expression),
                    target.address,
                    target.byte_size,
                    target.type_info,
                    target.scope,
                    target.frame,
                    target.code,
                )
            }
            WatchpointSpec::Location { address, byte_size } => (
                None,
                address,
                byte_size,
                None,
                WatchScope::Location,
                None,
                None,
            ),
        };

        let task = self.task_watch(inferior, &scope, address)?;
        let chunks = debug_registers::split_range(address.get(), byte_size).map_err(|error| {
            watch_range_error(address, byte_size, error, inferior.watch.plan.free_slots())
        })?;
        let id = WatchpointId::new(self.next_watchpoint_id);
        let next_id = self
            .next_watchpoint_id
            .checked_add(1)
            .ok_or_else(|| backend_error(LinuxError::WatchpointIdExhausted))?;
        let plan = (options.enabled && !inferior.stack_moves.sets_aside(task))
            .then(|| planned_with(&inferior.watch.plan, id, &chunks, slot_access))
            .transpose()?;

        // Read before arming, so a failed read leaves nothing armed.
        let (observed, value) = self.observe(&scope, address, byte_size)?;
        if let Some(plan) = plan {
            self.arm_all_threads(plan)?;
        }

        let watchpoint = Watchpoint {
            id,
            access,
            expression,
            address,
            byte_size,
            type_info,
            scope,
            coverage: chunks
                .iter()
                .map(|chunk| AddressRange {
                    start: VirtualAddress::new(chunk.address),
                    end: VirtualAddress::new(chunk.end()),
                })
                .collect::<Vec<_>>()
                .into(),
            hit_condition: options.hit_condition,
            condition: options.condition,
            hit_count: 0,
            enabled: options.enabled,
        };

        self.inferior
            .as_mut()
            .ok_or(Error::NotRunning)?
            .watch
            .watchpoints
            .insert(
                id,
                WatchRecord {
                    watchpoint: watchpoint.clone(),
                    frame,
                    observed,
                    value,
                    code,
                    task,
                },
            );
        self.next_watchpoint_id = next_id;
        self.publish_watchpoints_changed();
        if let Err(error) = self.sync_stack_movers() {
            let _ = self.remove_watchpoint(id);
            return Err(error);
        }

        Ok(watchpoint)
    }

    /// Replaces a watchpoint's hit condition, keeping the hits it counted.
    pub(super) fn set_watchpoint_hit_condition(
        &mut self,
        id: WatchpointId,
        hit_condition: Option<HitCondition>,
    ) -> Result<Watchpoint> {
        self.edit_watchpoint(id, |watchpoint| watchpoint.hit_condition = hit_condition)
    }

    pub(super) fn set_watchpoint_condition(
        &mut self,
        id: WatchpointId,
        condition: Option<crate::Condition>,
    ) -> Result<Watchpoint> {
        self.edit_watchpoint(id, |watchpoint| watchpoint.condition = condition)
    }

    /// Changes controller state only, so no stop is required.
    fn edit_watchpoint(
        &mut self,
        id: WatchpointId,
        edit: impl FnOnce(&mut Watchpoint),
    ) -> Result<Watchpoint> {
        let record = self
            .inferior
            .as_mut()
            .and_then(|inferior| inferior.watch.watchpoints.get_mut(&id))
            .ok_or(Error::WatchpointNotFound(id.get()))?;
        edit(&mut record.watchpoint);
        let watchpoint = record.watchpoint.clone();
        self.publish_watchpoints_changed();
        Ok(watchpoint)
    }

    /// Counts a hit for each watchpoint among `owners` that reports thread
    /// `pid`'s access, and returns those the hit stops at, with each hit's
    /// number: its hit condition and condition are met. A condition that
    /// cannot be evaluated stops, as a breakpoint's does.
    ///
    /// A declined hit's bytes become the watchpoint's last observed bytes,
    /// as gdb's old value does, so the next hit reports, and a change is
    /// judged, from them. While another thread's hit on the same watchpoint
    /// waits for every thread to stop, the bytes stay those that hit
    /// reports from.
    pub(super) fn stopping_watch_hits(
        &mut self,
        pid: Pid,
        owners: BTreeSet<WatchpointId>,
    ) -> Result<BTreeMap<WatchpointId, u64>> {
        let reason = StopReason::Watchpoint {
            hits: Arc::from([]),
        };

        let mut stopping = BTreeMap::new();
        for id in self.reportable_watch_hits(owners)? {
            let Some(watchpoint) = self
                .inferior
                .as_mut()
                .and_then(|inferior| inferior.watch.watchpoints.get_mut(&id))
                .map(|record| &mut record.watchpoint)
            else {
                continue;
            };

            debug_assert!(
                watchpoint.enabled,
                "a hit is reported for watchpoint {id}, which is disabled"
            );
            watchpoint.hit_count = watchpoint.hit_count.saturating_add(1);
            let hit_count = watchpoint.hit_count;
            let condition = watchpoint
                .hit_condition
                .is_none_or(|condition| condition.is_met(hit_count))
                .then(|| watchpoint.condition.clone());
            let stops = match condition {
                None => false,
                Some(None) => true,
                Some(Some(condition)) => self
                    .judge_condition(pid, &condition, &reason, ConditionOwner::Watchpoint(id))
                    .unwrap_or(true),
            };

            if stops {
                stopping.insert(id, hit_count);
            } else {
                self.observe_declined_hit(id)?;
            }
        }

        Ok(stopping)
    }

    /// Takes the bytes a declined hit left as a watchpoint's last observed
    /// bytes, unless another thread's hit on it awaits the stop.
    fn observe_declined_hit(&mut self, id: WatchpointId) -> Result<()> {
        let Some(inferior) = self.inferior.as_ref() else {
            return Ok(());
        };

        let Some(record) = inferior.watch.watchpoints.get(&id) else {
            return Ok(());
        };

        if inferior
            .threads
            .values()
            .any(|thread| thread.watch_hits.contains_key(&id))
        {
            return Ok(());
        }

        let (observed, value) = self.observe_record(record)?;
        if let Some(record) = self
            .inferior
            .as_mut()
            .and_then(|inferior| inferior.watch.watchpoints.get_mut(&id))
        {
            record.observed = observed;
            record.value = value;
        }

        Ok(())
    }

    /// Enables or disables a watchpoint. A disabled one releases its debug
    /// registers but stays recorded, so its storage's end still ends it.
    /// Enabling plans its registers again, failing when others took them,
    /// and takes the bytes it finds as the last observed ones, so stores
    /// made while it was disabled are not reported as a change.
    pub(super) fn set_watchpoint_enabled(
        &mut self,
        id: WatchpointId,
        enabled: bool,
    ) -> Result<Watchpoint> {
        let inferior = self
            .inferior
            .as_ref()
            .ok_or(Error::WatchpointNotFound(id.get()))?;
        let record = inferior
            .watch
            .watchpoints
            .get(&id)
            .ok_or(Error::WatchpointNotFound(id.get()))?;
        if record.watchpoint.enabled == enabled {
            return Ok(record.watchpoint.clone());
        }

        if !self.sites_live() {
            return Err(Error::NotStopped);
        }

        let watchpoint = &record.watchpoint;
        let (plan, observed) = if enabled {
            let slot_access = slot_access(watchpoint.access);
            let chunks =
                debug_registers::split_range(watchpoint.address.get(), watchpoint.byte_size)
                    .map_err(|error| {
                        watch_range_error(
                            watchpoint.address,
                            watchpoint.byte_size,
                            error,
                            inferior.watch.plan.free_slots(),
                        )
                    })?;
            let plan = if inferior.stack_moves.sets_aside(record.task) {
                None
            } else {
                Some(planned_with(
                    &inferior.watch.plan,
                    id,
                    &chunks,
                    slot_access,
                )?)
            };

            let observed = self.observe_record(record)?;
            (plan, Some(observed))
        } else {
            (Some(inferior.watch.plan.without_watchpoint(id)), None)
        };

        if let Some(plan) = plan {
            self.arm_all_threads(plan)?;
        }

        let record = self
            .inferior
            .as_mut()
            .and_then(|inferior| inferior.watch.watchpoints.get_mut(&id))
            .ok_or_else(|| Error::internal("armed watchpoint is recorded"))?;
        record.watchpoint.enabled = enabled;
        if let Some((observed, value)) = observed {
            record.observed = observed;
            record.value = value;
        }

        let watchpoint = record.watchpoint.clone();
        self.publish_watchpoints_changed();
        Ok(watchpoint)
    }

    pub(super) fn remove_watchpoint(&mut self, id: WatchpointId) -> Result<Watchpoint> {
        let inferior = self
            .inferior
            .as_ref()
            .filter(|inferior| inferior.watch.watchpoints.contains_key(&id))
            .ok_or(Error::WatchpointNotFound(id.get()))?;
        let plan = inferior.watch.plan.without_watchpoint(id);
        self.arm_all_threads(plan)?;
        let record = self
            .inferior
            .as_mut()
            .and_then(|inferior| inferior.watch.watchpoints.remove(&id))
            .ok_or_else(|| Error::internal("removed watchpoint was recorded"))?;
        self.publish_watchpoints_changed();
        self.sync_stack_movers()?;
        Ok(record.watchpoint)
    }

    pub(super) fn remove_all_watchpoints(&mut self) -> Result<Arc<[Watchpoint]>> {
        if self
            .inferior
            .as_ref()
            .is_none_or(|inferior| inferior.watch.watchpoints.is_empty())
        {
            return Ok(Arc::from([]));
        }

        self.arm_all_threads(DebugRegisterPlan::default())?;
        let removed = std::mem::take(
            &mut self
                .inferior
                .as_mut()
                .ok_or(Error::NotRunning)?
                .watch
                .watchpoints,
        );
        self.publish_watchpoints_changed();
        self.sync_stack_movers()?;
        Ok(removed
            .into_values()
            .map(|record| record.watchpoint)
            .collect::<Vec<_>>()
            .into())
    }

    pub(super) fn publish_watchpoints_changed(&mut self) {
        self.bump_revision();
        let _ = self.events.send(DebuggerEvent::WatchpointsChanged {
            revision: self.revision,
        });
    }

    /// Installs `plan` on every stopped thread or on none of them.
    ///
    /// A thread that has already begun exiting is skipped; its exit is
    /// processed normally. So is one that does not carry the current plan,
    /// its arming having failed: it cannot run until it is armed before a
    /// resume. Rolling a thread back therefore only rewrites slots the plan
    /// it previously carried already reserved, so rollback needs no new
    /// kernel capacity.
    pub(super) fn arm_all_threads(&mut self, plan: DebugRegisterPlan) -> Result<()> {
        let inferior = self.inferior.as_mut().ok_or(Error::NotRunning)?;
        let previous = inferior.watch.plan.clone();
        let generation = inferior.watch.generation;
        let threads = inferior
            .threads
            .iter()
            .filter_map(|(&pid, thread)| {
                (matches!(thread.state, NativeThreadState::Stopped)
                    && thread.armed == Some(generation))
                .then_some(pid)
            })
            .collect::<Vec<_>>();
        let mut programmed = Vec::with_capacity(threads.len());
        for pid in threads {
            match program_debug_registers(&self.ptrace, pid, &plan) {
                Ok(()) => programmed.push(pid),
                Err(ArmFailure::ThreadGone) => {}
                Err(failure) => {
                    let cause = arm_error(pid, &failure);

                    // The failing thread may have been partly rewritten, so it
                    // is restored along with every thread already armed.
                    for &armed in std::iter::once(&pid).chain(programmed.iter().rev()) {
                        match program_debug_registers(&self.ptrace, armed, &previous) {
                            Ok(()) | Err(ArmFailure::ThreadGone) => {}
                            Err(recovery) => {
                                let _ = self.ptrace.kill(inferior.tgid, Signal::SIGKILL);
                                return Err(backend_error(LinuxError::WatchpointArmRecovery {
                                    cause: cause.to_string(),
                                    recovery: arm_error(armed, &recovery).to_string(),
                                }));
                            }
                        }
                    }

                    return Err(cause);
                }
            }
        }

        inferior.watch.generation += 1;
        inferior.watch.plan = plan;
        let generation = inferior.watch.generation;
        for pid in programmed {
            inferior.thread_mut(pid)?.armed = Some(generation);
        }

        Ok(())
    }

    /// Programs any stopped thread whose registers do not carry the current
    /// plan, so no thread ever runs unwatched. Failure leaves the inferior
    /// stopped.
    pub(super) fn sync_debug_registers(&mut self) -> Result<()> {
        let inferior = self.inferior.as_mut().ok_or(Error::NotRunning)?;
        let generation = inferior.watch.generation;
        let stale = inferior
            .threads
            .iter()
            .filter(|(_, thread)| {
                matches!(thread.state, NativeThreadState::Stopped)
                    && thread.armed != Some(generation)
            })
            .map(|(&pid, _)| pid)
            .collect::<Vec<_>>();
        for pid in stale {
            match program_debug_registers(&self.ptrace, pid, &inferior.watch.plan) {
                Ok(()) => {
                    inferior.thread_mut(pid)?.armed = Some(generation);
                }
                Err(ArmFailure::ThreadGone) => {}
                Err(failure) => return Err(arm_error(pid, &failure)),
            }
        }

        Ok(())
    }

    /// Arms a newly started thread before it executes. Returns the failure
    /// to publish when it cannot be armed.
    pub(super) fn arm_new_thread(&mut self, pid: Pid) -> Option<Error> {
        let inferior = self.inferior.as_mut()?;
        let generation = inferior.watch.generation;

        // Debug registers are never inherited, so an unarmed thread already
        // matches an empty plan.
        let result = if inferior.watch.plan.is_empty() {
            Ok(())
        } else {
            program_debug_registers(&self.ptrace, pid, &inferior.watch.plan)
        };

        match result {
            Ok(()) => {
                inferior.threads.get_mut(&pid)?.armed = Some(generation);
                None
            }
            Err(ArmFailure::ThreadGone) => None,
            Err(failure) => Some(arm_error(pid, &failure)),
        }
    }

    /// Clears debug-register state a previous tracer may have left armed in
    /// an attached process, which would otherwise kill it with SIGTRAP.
    pub(super) fn clear_attached_debug_registers(&mut self) -> Result<()> {
        let inferior = self.inferior.as_mut().ok_or(Error::NotRunning)?;
        debug_assert!(
            inferior.watch.plan.is_empty(),
            "an attached process starts with {} debug-register slots planned",
            inferior.watch.plan.slots().iter().flatten().count()
        );

        // Each thread is marked as carrying the current plan.
        let generation = inferior.watch.generation;
        let empty = DebugRegisterPlan::default();
        for (&pid, thread) in &mut inferior.threads {
            match program_debug_registers(&self.ptrace, pid, &empty) {
                Ok(()) => thread.armed = Some(generation),
                Err(ArmFailure::ThreadGone) => {}
                Err(failure) => return Err(arm_error(pid, &failure)),
            }
        }

        Ok(())
    }

    /// Disarms every thread before the process leaves debugger control. The
    /// kernel keeps debug registers armed across `PTRACE_DETACH`, and an
    /// untraced hit would kill the process with SIGTRAP. Every thread is
    /// attempted even if one fails.
    pub(super) fn disarm_for_detach(&mut self) -> Result<()> {
        let inferior = self.inferior.as_mut().ok_or(Error::NotRunning)?;
        let empty = DebugRegisterPlan::default();
        let mut first_error = None;
        for (&pid, thread) in &inferior.threads {
            if thread.armed.is_none() && inferior.watch.plan.is_empty() {
                continue;
            }

            match program_debug_registers(&self.ptrace, pid, &empty) {
                Ok(()) | Err(ArmFailure::ThreadGone) => {}
                Err(failure) => {
                    first_error.get_or_insert_with(|| arm_error(pid, &failure));
                }
            }
        }

        first_error.map_or(Ok(()), Err)
    }

    /// Records the watched bytes just before execution resumes so the next
    /// hit reports what the access changed since the debugger last looked,
    /// including the debugger's own writes.
    pub(super) fn refresh_watch_baselines(&mut self) -> Result<()> {
        let Some(inferior) = self.inferior.as_ref() else {
            return Ok(());
        };

        let observed = inferior
            .watch
            .watchpoints
            .iter()
            .map(|(&id, record)| self.observe_record(record).map(|observed| (id, observed)))
            .collect::<Result<Vec<_>>>()?;
        let inferior = self.inferior.as_mut().ok_or(Error::NotRunning)?;
        for (id, (bytes, value)) in observed {
            let record = inferior
                .watch
                .watchpoints
                .get_mut(&id)
                .ok_or_else(|| Error::internal("watchpoint exists"))?;
            record.observed = bytes;
            record.value = value;
        }

        Ok(())
    }

    /// Resolves the pending watch evidence of every stopped thread once the
    /// whole process is stopped: invalidates scoped watchpoints whose storage
    /// ended, reports hits on the remaining ones, and publishes the result
    /// as each thread's stop reason.
    pub(super) fn evaluate_watchpoints(&mut self) -> Result<()> {
        let Some(inferior) = self.inferior.as_ref() else {
            return Ok(());
        };

        if inferior.watch.watchpoints.is_empty()
            && inferior
                .threads
                .values()
                .all(|thread| thread.watch_hits.is_empty())
        {
            return Ok(());
        }

        let mut invalid = BTreeMap::new();
        for (&id, record) in &inferior.watch.watchpoints {
            if let Some(reason) = self.watch_invalidation(inferior, record)? {
                invalid.insert(id, reason);
            }
        }

        let hit = inferior
            .threads
            .values()
            .flat_map(|thread| thread.watch_hits.keys().copied())
            .filter(|id| !invalid.contains_key(id))
            .collect::<BTreeSet<_>>();
        let current = hit
            .iter()
            .filter_map(|id| {
                inferior
                    .watch
                    .watchpoints
                    .get(id)
                    .map(|record| (*id, record))
            })
            .map(|(id, record)| self.observe_record(record).map(|observed| (id, observed)))
            .collect::<Result<BTreeMap<_, _>>>()?;
        let invalidated = invalid
            .iter()
            .map(|(id, reason)| InvalidatedWatchpoint {
                watchpoint: inferior.watch.watchpoints[id].watchpoint.clone(),
                reason: *reason,
            })
            .collect::<Vec<_>>();

        let inferior = self.inferior.as_mut().ok_or(Error::NotRunning)?;
        for (&pid, thread) in &mut inferior.threads {
            let owners = std::mem::take(&mut thread.watch_hits);
            if owners.is_empty() {
                continue;
            }

            let hits = owners
                .iter()
                .filter_map(|(id, &hit_count)| {
                    let record = inferior.watch.watchpoints.get(id)?;
                    current.get(id).map(|(bytes, value)| WatchpointHit {
                        watchpoint: *id,
                        thread: debug_thread_id(pid),
                        hit_count,
                        previous: record.observed.clone(),
                        current: bytes.clone(),
                        previous_value: record.value.clone(),
                        current_value: value.clone(),
                    })
                })
                .collect::<Vec<_>>();
            let reason = if hits.is_empty() {
                StopReason::WatchpointInvalidated {
                    invalidated: invalidated
                        .iter()
                        .filter(|entry| owners.contains_key(&entry.watchpoint.id))
                        .cloned()
                        .collect::<Vec<_>>()
                        .into(),
                }
            } else {
                StopReason::Watchpoint { hits: hits.into() }
            };

            if let Some(barrier) = inferior.barrier.as_mut()
                && barrier.triggering_thread == pid
                && matches!(barrier.reason, Some(StopReason::Watchpoint { .. }))
            {
                barrier.reason = Some(reason.clone());
            }

            thread.reason = Some(reason);
        }

        for (id, (bytes, value)) in current {
            if let Some(record) = inferior.watch.watchpoints.get_mut(&id) {
                record.observed = bytes;
                record.value = value;
            }
        }

        self.remove_invalidated_watchpoints(invalidated)
    }

    /// Disarms and publishes watchpoints whose storage's lifetime ended.
    pub(super) fn remove_invalidated_watchpoints(
        &mut self,
        invalidated: Vec<InvalidatedWatchpoint>,
    ) -> Result<()> {
        if invalidated.is_empty() {
            return Ok(());
        }

        let inferior = self.inferior.as_mut().ok_or(Error::NotRunning)?;
        let plan = invalidated
            .iter()
            .fold(inferior.watch.plan.clone(), |plan, entry| {
                plan.without_watchpoint(entry.watchpoint.id)
            });
        self.arm_all_threads(plan)?;
        let inferior = self.inferior.as_mut().ok_or(Error::NotRunning)?;
        for entry in &invalidated {
            inferior.watch.watchpoints.remove(&entry.watchpoint.id);
        }

        self.bump_revision();
        let _ = self.events.send(DebuggerEvent::WatchpointsInvalidated {
            revision: self.revision,
            invalidated: invalidated.into(),
        });
        self.publish_watchpoints_changed();
        self.sync_stack_movers()
    }

    /// Discards the watchpoints of a process that no longer exists or whose
    /// image was replaced.
    pub(super) fn discard_watchpoints(&mut self) {
        let Some(inferior) = self.inferior.as_mut() else {
            return;
        };

        let had_watchpoints = !inferior.watch.watchpoints.is_empty();
        inferior.watch.watchpoints.clear();
        inferior.watch.plan = DebugRegisterPlan::default();
        inferior.watch.generation += 1;
        if had_watchpoints {
            self.publish_watchpoints_changed();
        }
    }
}

impl<P: InspectionOps> Controller<P> {
    /// A watch's bytes as [`Self::observe`] reads them.
    fn observe_record(&self, record: &WatchRecord) -> Result<Observation> {
        let watchpoint = &record.watchpoint;
        self.observe(&watchpoint.scope, watchpoint.address, watchpoint.byte_size)
    }

    /// The watched bytes, and an interpreted variable's value as its
    /// runtime presents it. Such a variable's word names its object by an
    /// address the runtime may tag; the object's address stands for its
    /// bytes, so that a variable given the same object again is unchanged.
    fn observe(
        &self,
        scope: &WatchScope,
        address: VirtualAddress,
        byte_size: u64,
    ) -> Result<Observation> {
        let bytes = self.read_watched_bytes(address, byte_size)?;
        let WatchScope::Interpreted { runtime, .. } = scope else {
            return Ok((bytes, None));
        };
        let inferior = self.inferior.as_ref().ok_or(Error::NotRunning)?;
        let Some(bound) = self
            .runtimes(inferior)
            .into_iter()
            .find(|bound| bound.id == *runtime)
        else {
            return Ok((bytes, None));
        };
        let Some(word) = bytes
            .as_deref()
            .and_then(|bytes| <[u8; 8]>::try_from(bytes).ok())
            .map(u64::from_le_bytes)
        else {
            return Ok((bytes, None));
        };

        let object = match bound.model.interpreted_watched(word) {
            Ok(object) => object,
            Err(reason) => return Ok((bytes, Some(reason))),
        };
        let value = object.map_or_else(
            || "unbound".into(),
            |object| {
                let mut budget =
                    crate::inspection::InspectionBudget::new(crate::InspectionLimits::default());
                let slot =
                    self.with_runtime_stop(inferior, &bound, inferior.memory_thread(), |stop| {
                        bound.model.interpreted_value(stop, object, &mut budget)
                    });
                match slot {
                    InterpretedSlot::Value(value) => value.summary,
                    InterpretedSlot::Unbound => "unbound".into(),
                    InterpretedSlot::Unreadable(reason) => format!("unreadable: {reason}").into(),
                    InterpretedSlot::Number(_) | InterpretedSlot::Limit(_) => {
                        "not read within the inspection's limits".into()
                    }
                }
            },
        );
        let object = object.map_or(0, VirtualAddress::get);
        Ok((Some(Arc::from(object.to_le_bytes())), Some(value)))
    }

    /// Reads the current watched bytes through a stopped thread, hiding
    /// software-breakpoint bytes. Unreadable memory is reported as `None`, as
    /// is memory no stopped thread can reach because each was killed out of
    /// its ptrace-stop, as a sibling's `exit_group` does. Any other failure
    /// is an error, never a value.
    fn read_watched_bytes(
        &self,
        address: VirtualAddress,
        byte_size: u64,
    ) -> Result<Option<Arc<[u8]>>> {
        let inferior = self.inferior.as_ref().ok_or(Error::NotRunning)?;
        let size = usize::try_from(byte_size).map_err(|_| Error::AddressOverflow)?;
        let stopped = inferior
            .threads
            .iter()
            .filter(|(_, thread)| matches!(thread.state, NativeThreadState::Stopped));
        for (&pid, _) in stopped {
            match read_logical_memory(&self.ptrace, pid, &inferior.breakpoints, address, size) {
                Ok(read) => {
                    return Ok(matches!(read.completion, MemoryReadCompletion::Complete)
                        .then(|| read.bytes.into()));
                }
                Err(error) if is_vanished_tracee(&error) => {}
                Err(error) => return Err(error),
            }
        }

        Ok(None)
    }

    /// The change watchpoints among `owners` whose watched bytes equal those
    /// the debugger last observed, so the access changed nothing. Bytes that
    /// became unreadable, or readable, count as a change.
    pub(super) fn unchanged_watchpoints(
        &self,
        owners: impl IntoIterator<Item = WatchpointId>,
    ) -> Result<BTreeSet<WatchpointId>> {
        let Some(inferior) = self.inferior.as_ref() else {
            return Ok(BTreeSet::new());
        };

        let mut unchanged = BTreeSet::new();
        for id in owners {
            let Some(record) = inferior.watch.watchpoints.get(&id) else {
                continue;
            };

            // A store that ends an interpreted variable's frame, as a new
            // frame's in its place, is the watch's end, changed or not.
            if record.watchpoint.access == WatchAccess::Change
                && self.observe_record(record)?.0 == record.observed
                && !matches!(self.interpreted_watch_live(inferior, record)?, Some(false))
            {
                unchanged.insert(id);
            }
        }

        Ok(unchanged)
    }

    /// The watchpoints among a thread's hit `owners` that report its
    /// access: all but the change watchpoints it left unchanged.
    pub(super) fn reportable_watch_hits(
        &self,
        mut owners: BTreeSet<WatchpointId>,
    ) -> Result<BTreeSet<WatchpointId>> {
        let unchanged = self.unchanged_watchpoints(owners.iter().copied())?;
        owners.retain(|id| !unchanged.contains(id));
        Ok(owners)
    }

    fn watch_invalidation(
        &self,
        inferior: &Inferior,
        record: &WatchRecord,
    ) -> Result<Option<WatchpointInvalidation>> {
        Ok(match &record.watchpoint.scope {
            WatchScope::Location => None,
            WatchScope::Static { module } => (!self.modules.contains_key(module))
                .then_some(WatchpointInvalidation::ModuleUnloaded),
            WatchScope::ThreadLocal { thread } => (!debug_pid(*thread)
                .is_ok_and(|pid| inferior.threads.contains_key(&pid)))
            .then_some(WatchpointInvalidation::OwnerThreadExited),
            WatchScope::Frame { thread, activation } => {
                let pid = debug_pid(*thread)?;
                if inferior.threads.contains_key(&pid) {
                    let evidence = record.frame.as_ref().ok_or_else(|| {
                        Error::internal("frame-scoped watchpoints carry scope evidence")
                    })?;
                    (!self.frame_scope_is_live(inferior, pid, *activation, evidence)?)
                        .then_some(WatchpointInvalidation::ScopeExited)
                } else {
                    Some(WatchpointInvalidation::OwnerThreadExited)
                }
            }
            WatchScope::Task { task, activation } => {
                let evidence = record.frame.as_ref().ok_or_else(|| {
                    Error::internal("frame-scoped watchpoints carry scope evidence")
                })?;
                self.task_watch_invalidation(inferior, record, *task, *activation, evidence)?
            }
            WatchScope::Interpreted { thread, .. } => {
                if debug_pid(*thread).is_ok_and(|pid| inferior.threads.contains_key(&pid)) {
                    matches!(self.interpreted_watch_live(inferior, record)?, Some(false))
                        .then_some(WatchpointInvalidation::ScopeExited)
                } else {
                    Some(WatchpointInvalidation::OwnerThreadExited)
                }
            }
        })
    }

    /// Whether the frame of an interpreted variable a watch watches still
    /// runs on its thread: `None` for another watch, and for one whose
    /// frame cannot be read, which is kept rather than ended unproven.
    #[cfg_attr(
        not(debug_assertions),
        expect(unused_variables, reason = "only recorded")
    )]
    fn interpreted_watch_live(
        &self,
        inferior: &Inferior,
        record: &WatchRecord,
    ) -> Result<Option<bool>> {
        let WatchScope::Interpreted {
            thread,
            runtime,
            frame,
        } = record.watchpoint.scope
        else {
            return Ok(None);
        };
        let code = record
            .code
            .ok_or_else(|| Error::internal("an interpreted watch carries its frame's code"))?;
        let Ok(pid) = debug_pid(thread) else {
            return Ok(Some(false));
        };
        let Some(bound) = self
            .runtimes(inferior)
            .into_iter()
            .find(|bound| bound.id == runtime)
        else {
            return Ok(Some(false));
        };
        let reader = if inferior.threads.contains_key(&pid) {
            pid
        } else {
            inferior.memory_thread()
        };
        match self.with_runtime_stop(inferior, &bound, reader, |stop| {
            bound
                .model
                .interpreted_frame_runs(stop, thread, frame, code)
        }) {
            Ok(runs) => Ok(Some(runs)),
            Err(reason) => {
                record!(
                    "watchpoint {} keeps its frame unread: {reason}",
                    record.watchpoint.id
                );
                Ok(None)
            }
        }
    }

    /// Whether the owner thread still executes the activation that declared
    /// a frame-scoped object, inside the object's lexical scope.
    ///
    /// The activation is the frame whose canonical frame address matches. A
    /// frame there running another function means a tail call replaced it,
    /// and one outside the object's scope ranges means its block ended. When
    /// unwinding fails first, the stack pointer still proves a return.
    fn frame_scope_is_live(
        &self,
        inferior: &Inferior,
        pid: Pid,
        activation: VirtualAddress,
        evidence: &FrameScopeEvidence,
    ) -> Result<bool> {
        let Some(module) = self
            .modules
            .get(&evidence.module)
            .filter(|module| module.loaded.image == evidence.image)
        else {
            return Ok(false);
        };

        let native = self.ptrace.registers(pid)?;
        let mut provider = DwarfCallerProvider {
            modules: self.unwind_modules(inferior),
            registers: x86_64_registers(&native),
            memory: PtraceMemory {
                ptrace: &self.ptrace,
                pid,
            },
            first: true,
        };

        let mut context = FrameContext {
            instruction: VirtualAddress::new(native.rip),
            cfa: None,
            signal_frame: false,
        };

        let view = self.stack_view(pid);
        let activation = view.activation(activation);
        let unproven = !activation.has_returned(view.position(native.rsp));
        for level in 0..DEFAULT_MAX_FRAMES {
            let caller = match provider.caller(&context) {
                CallerResult::Caller(caller) => caller,
                CallerResult::Finished(UnwindTermination::Complete) => return Ok(false),
                CallerResult::Finished(_) => return Ok(unproven),
            };

            let frame = caller.cfa.map(|cfa| view.activation(cfa));
            if frame == Some(activation) {
                let level =
                    u32::try_from(level).map_err(|_| Error::internal("frame limit fits u32"))?;
                let Some(image_address) = frame_lookup_address(level, &context)
                    .and_then(|address| module.loaded.image_address(address).ok())
                    .filter(|address| module.image.contains_address(*address))
                else {
                    return Ok(false);
                };

                let location = module.image.locate(image_address);
                return Ok(location.physical_instance == Some(evidence.function)
                    && evidence
                        .ranges
                        .iter()
                        .any(|range| range.contains(image_address)));
            }

            if frame.is_some_and(|frame| activation.is_callee_of(frame)) {
                return Ok(false);
            }

            context = caller;
        }

        Ok(unproven)
    }

    /// The lifetime of a data object's storage, which a watch of memory
    /// inside it shares. `local` is the frame's address in the object's
    /// image when the object is a local or parameter of the frame.
    pub(super) fn root_watch_scope(
        &self,
        inferior: &Inferior,
        pid: Pid,
        frame: StackFrameId,
        module: crate::ModuleId,
        storage: crate::debug_info::ObjectStorage,
        local: Option<ImageAddress>,
    ) -> Result<(WatchScope, Option<FrameScopeEvidence>)> {
        match storage.class {
            StorageClass::Static => Ok((WatchScope::Static { module }, None)),
            StorageClass::ThreadLocal => Ok((
                WatchScope::ThreadLocal {
                    thread: debug_thread_id(pid),
                },
                None,
            )),
            StorageClass::Indirect => Ok((WatchScope::Location, None)),
            StorageClass::NotMemory | StorageClass::Frame { stable: false, .. } => {
                Err(Error::WatchTargetUnsupported(
                    "the object's location changes within its scope".into(),
                ))
            }
            StorageClass::Frame { moving_stack, .. } => {
                let Some(address) = local else {
                    return Err(Error::WatchTargetUnsupported(
                        "a frame-relative global has no owning activation".into(),
                    ));
                };

                let image = &self
                    .modules
                    .get(&module)
                    .ok_or(Error::ModuleNotLoaded(module))?
                    .image;
                let activation = self
                    .resolve_frame(inferior, &StackRoot::of_thread(pid), frame)?
                    .cfa
                    .map_err(|error| {
                        let reason: Arc<str> = match error {
                            VariableRuntimeError::Unavailable(reason) => reason.to_string().into(),
                            VariableRuntimeError::Malformed(reason)
                            | VariableRuntimeError::Fatal(reason) => reason,
                        };

                        Error::WatchTargetUnsupported(
                            format!("the declaring activation is unavailable: {reason}").into(),
                        )
                    })?;
                let function = image.locate(address).physical_instance.ok_or_else(|| {
                    Error::WatchTargetUnsupported(
                        "no function describes the declaring activation".into(),
                    )
                })?;
                let evidence = FrameScopeEvidence {
                    module,
                    image: image.id(),
                    function,
                    ranges: storage.ranges,
                };

                if !moving_stack {
                    let scope = WatchScope::Frame {
                        thread: debug_thread_id(pid),
                        activation,
                    };

                    return Ok((scope, Some(evidence)));
                }

                // A stack its runtime may move is watched as its task's,
                // wherever the task's stack is.
                let (task, below_top) = self
                    .stack_view(pid)
                    .activation(activation)
                    .on_task_stack()
                    .ok_or_else(|| {
                        Error::WatchTargetUnsupported(
                            "the language runtime may move this stack, and does not say whose it is"
                                .into(),
                        )
                    })?;
                Ok((
                    WatchScope::Task {
                        task,
                        activation: below_top,
                    },
                    Some(evidence),
                ))
            }
        }
    }
}

/// The memory a resolved value occupies, or why it cannot be watched.
pub(super) fn watchable_storage(value: &InspectedValue) -> Result<(VirtualAddress, u64)> {
    let source = match &value.state {
        crate::VariableState::Available { source, .. }
        | crate::VariableState::Invalid { source, .. } => source,
        crate::VariableState::Unavailable(reason) => {
            return Err(Error::WatchTargetUnavailable(reason.to_string().into()));
        }
        crate::VariableState::Malformed(reason) => {
            return Err(Error::WatchTargetUnavailable(Arc::clone(
                &reason.description,
            )));
        }
    };

    let address = match source {
        VariableValueSource::Memory(address) => *address,
        VariableValueSource::Register(register) => {
            return Err(Error::WatchTargetNotInMemory(
                format!("the value is held in register {}", register.name).into(),
            ));
        }
        VariableValueSource::Constant => {
            return Err(Error::WatchTargetNotInMemory(
                "the value is a debug-information constant".into(),
            ));
        }
        VariableValueSource::Computed => {
            return Err(Error::WatchTargetNotInMemory(
                "the value is computed, such as a bit-field or an optimized expression".into(),
            ));
        }
        VariableValueSource::ImplicitPointer => {
            return Err(Error::WatchTargetNotInMemory(
                "optimization eliminated the pointer's address".into(),
            ));
        }
        VariableValueSource::Composite => {
            return Err(Error::WatchTargetNotInMemory(
                "the value is split across several places".into(),
            ));
        }
    };

    let byte_size = value
        .type_info
        .as_ref()
        .and_then(|info| info.byte_size)
        .filter(|size| *size > 0)
        .ok_or_else(|| Error::WatchTargetUnsupported("the value's size is unknown".into()))?;
    Ok((address, byte_size))
}

/// `plan` with a watchpoint's slots added, or the capacity error.
fn planned_with(
    plan: &DebugRegisterPlan,
    id: WatchpointId,
    chunks: &[debug_registers::Chunk],
    access: SlotAccess,
) -> Result<DebugRegisterPlan> {
    plan.with_watchpoint(id, chunks, access)
        .map_err(|error| Error::WatchpointCapacity {
            required: error.required as u64,
            available: error.available as u64,
        })
}

fn watch_range_error(
    address: VirtualAddress,
    byte_size: u64,
    error: debug_registers::RangeError,
    available: usize,
) -> Error {
    let reason = match error {
        debug_registers::RangeError::Empty => "a watchpoint must cover at least one byte",
        debug_registers::RangeError::Overflow => "the range wraps the address space",
        debug_registers::RangeError::OutsideUserSpace => {
            "the range reaches memory the kernel does not let user debuggers watch"
        }
        debug_registers::RangeError::TooLarge { required } => {
            return Error::WatchpointCapacity {
                required,
                available: available as u64,
            };
        }
    };

    Error::InvalidWatchRange {
        address,
        byte_size,
        reason: reason.into(),
    }
}

/// Why one thread's debug registers could not be programmed.
#[derive(Debug)]
enum ArmFailure {
    /// The thread is exiting.
    ThreadGone,

    /// Other hardware-breakpoint users hold the thread's slots.
    Busy,

    /// The kernel refused a value or the readback disagreed, as in sandboxes
    /// that ignore debug-register writes.
    Unsupported(String),
    System(Errno),
}

fn arm_failure(error: Errno) -> ArmFailure {
    match error {
        Errno::ESRCH => ArmFailure::ThreadGone,
        Errno::ENOSPC => ArmFailure::Busy,
        Errno::EINVAL | Errno::EIO => {
            ArmFailure::Unsupported(format!("the kernel refused a debug register: {error}"))
        }
        error => ArmFailure::System(error),
    }
}

fn arm_error(pid: Pid, failure: &ArmFailure) -> Error {
    match failure {
        ArmFailure::ThreadGone => Error::NotRunning,
        ArmFailure::Busy => Error::WatchpointHardwareBusy {
            thread: debug_thread_id(pid),
        },
        ArmFailure::Unsupported(description) => {
            Error::HardwareWatchpointsUnavailable(description.as_str().into())
        }
        ArmFailure::System(error) => backend_error(LinuxError::System(*error)),
    }
}

/// Installs `plan` in one stopped thread's debug registers and reads it
/// back. A readback mismatch means the target silently ignored the writes.
fn program_debug_registers(
    ptrace: &dyn LinuxTraceOps,
    pid: Pid,
    plan: &DebugRegisterPlan,
) -> std::result::Result<(), ArmFailure> {
    for (register, value) in plan.programming_sequence() {
        ptrace
            .write_debug_register(pid, register, value)
            .map_err(arm_failure)?;
    }

    let expected = std::iter::once((debug_registers::CONTROL_REGISTER, plan.control())).chain(
        plan.slots()
            .iter()
            .enumerate()
            .filter_map(|(index, slot)| slot.as_ref().map(|slot| (index, slot.chunk.address))),
    );
    for (register, value) in expected {
        let actual = ptrace
            .read_debug_register(pid, register)
            .map_err(arm_failure)?;
        if actual != value {
            return Err(ArmFailure::Unsupported(format!(
                "debug register {register} read back {actual:#x} after writing {value:#x}"
            )));
        }
    }

    Ok(())
}

/// The debug-register access that reports a watch's accesses. A read watch
/// is refused when added, as x86 has no read-only condition.
pub(super) fn slot_access(access: WatchAccess) -> SlotAccess {
    debug_assert_ne!(
        access,
        WatchAccess::Read,
        "a read watch was added, which x86 cannot report"
    );
    match access {
        WatchAccess::ReadWrite => SlotAccess::ReadWrite,
        WatchAccess::Change | WatchAccess::Write | WatchAccess::Read => SlotAccess::Write,
    }
}
