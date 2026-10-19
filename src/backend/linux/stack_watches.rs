//! Watches of objects on stacks a language runtime moves, as Go's copies a
//! goroutine's stack elsewhere to grow or shrink it. Such a watch belongs
//! to its task, and is placed by how far below the top of the task's stack
//! the watched bytes are, which a move keeps.
//!
//! While a watched task's stack moves, the runtime's stack mover reads the
//! old stack and writes the new one, so the task's watches are put aside
//! from the mover's entry, where it says whose stack it moves, until its
//! return, where they are placed on the new stack. Both are breakpoints
//! planted only while such a watch exists, and each makes an internal stop,
//! since every thread's debug registers change. A move the debugger cannot
//! follow ends the watches it may have moved, rather than leave them
//! watching memory their objects left.

use std::collections::{BTreeMap, BTreeSet};

use nix::unistd::Pid;

use crate::protocol::{
    FrameScopeEvidence, InvalidatedWatchpoint, WatchScope, WatchpointId, WatchpointInvalidation,
};

use crate::unwind::DEFAULT_MAX_FRAMES;
use crate::{AddressRange, Error, Result, RuntimeId, TaskId, VirtualAddress};

use super::activation::{Activation, StackView, TaskStack};
use super::breakpoints::remove_breakpoint_owner_from;
use super::debug_registers;
use super::frames::StackRoot;
use super::native::{InspectionOps, LinuxTraceOps};
use super::registers::x86_64_registers;
use super::{BreakpointOwner, Controller, Edit, Inferior, WatchRecord};

/// Where a watch on a task's stack is: how far below the top of the task's
/// stack the watched bytes begin.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct TaskWatch {
    pub(super) task: TaskId,
    pub(super) below_top: u64,
}

/// The stack moves the debugger follows.
#[derive(Debug, Default)]
pub(super) struct StackMoves {
    /// The entry of each runtime's stack mover, planted while a task of
    /// that runtime has a watch.
    movers: BTreeMap<VirtualAddress, RuntimeId>,

    /// The moves of watched tasks' stacks under way.
    moving: Vec<Move>,

    /// Where the moves under way return, each planted.
    returns: BTreeSet<VirtualAddress>,

    /// Whether a stack moved that the runtime could not name the task of,
    /// which may have been any watched task's.
    unknown: bool,
}

impl StackMoves {
    /// Forgets every move and site, as a new image does.
    pub(super) fn clear(&mut self) {
        *self = Self::default();
    }

    /// Forgets a site whose code went away.
    pub(super) fn forget(&mut self, address: VirtualAddress) {
        self.movers.remove(&address);
        self.returns.remove(&address);
    }

    /// Every site planted: the movers' entries and the moves' returns.
    pub(super) fn planted(&self) -> BTreeSet<VirtualAddress> {
        self.movers.keys().chain(&self.returns).copied().collect()
    }

    /// Begins a move of `task`'s stack by `thread`, as entering the mover
    /// does.
    #[cfg(test)]
    pub(super) fn begin(&mut self, thread: Pid, task: TaskId) {
        self.moving.push(Move {
            thread,
            task,
            returns_to: VirtualAddress::new(0),
            stack: 0,
            returned: false,
        });
    }

    /// Whether a watch on `task`'s stack, if any, is set aside: it holds
    /// no debug registers until its stack lands, and the follow arms it
    /// there.
    pub(super) fn sets_aside(&self, task: Option<TaskWatch>) -> bool {
        task.is_some_and(|watch| {
            self.moving
                .iter()
                .any(|moving| !moving.returned && moving.task == watch.task)
        })
    }
}

/// One move of a watched task's stack: the thread in the mover, where it
/// returns, and the stack pointer it returns with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Move {
    thread: Pid,
    task: TaskId,
    returns_to: VirtualAddress,
    stack: u64,

    /// Whether the mover returned, so the task's stack is where it goes.
    returned: bool,
}

impl<P: InspectionOps> Controller<P> {
    /// Where on its task's stack a watch of the object at `address` is, for
    /// a watch with a task's scope: how far below the stack's top, which
    /// the runtime's moves keep.
    pub(super) fn task_watch(
        &self,
        inferior: &Inferior,
        scope: &WatchScope,
        address: VirtualAddress,
    ) -> Result<Option<TaskWatch>> {
        let WatchScope::Task { task, .. } = *scope else {
            return Ok(None);
        };

        let bounds = self
            .task_stack_bounds(inferior, task)
            .map_err(|reason| Error::TaskUnavailable { task, reason })?
            .ok_or(Error::UnknownTask(task))?;
        if !bounds.contains(&address.get()) {
            return Err(Error::WatchTargetUnsupported(
                "the object is not on its task's stack".into(),
            ));
        }

        Ok(Some(TaskWatch {
            task,
            below_top: bounds.end - address.get(),
        }))
    }

    /// Whether a watch of an object on a task's stack ended: its task is
    /// gone, its activation returned, or its stack moved unfollowed.
    pub(super) fn task_watch_invalidation(
        &self,
        inferior: &Inferior,
        record: &WatchRecord,
        task: TaskId,
        activation: u64,
        evidence: &FrameScopeEvidence,
    ) -> Result<Option<WatchpointInvalidation>> {
        let bounds = match self.task_stack_bounds(inferior, task) {
            Ok(Some(bounds)) => bounds,
            Ok(None) => return Ok(Some(WatchpointInvalidation::ScopeExited)),

            #[cfg_attr(
                not(debug_assertions),
                expect(unused_variables, reason = "only recorded")
            )]
            Err(reason) => {
                record!("task {task}'s stack is unreadable: {reason}");
                return Ok(Some(WatchpointInvalidation::StackMoved));
            }
        };

        // A watch not set aside for a move is where its task's stack is;
        // anywhere else, the stack moved where the debugger did not see.
        let suspended = inferior
            .stack_moves
            .moving
            .iter()
            .any(|moving| moving.task == task);
        if let Some(watch) = record.task
            && !suspended
            && bounds.end.checked_sub(watch.below_top) != Some(record.watchpoint.address.get())
        {
            record!("task {task}'s stack moved unseen to {bounds:#x?}");
            return Ok(Some(WatchpointInvalidation::StackMoved));
        }

        let Some(root) = self.task_root(inferior, task, inferior.memory_thread())? else {
            return Ok(Some(WatchpointInvalidation::ScopeExited));
        };

        let view = StackView::task(
            root.thread().unwrap_or_else(|| root.reader()),
            TaskStack {
                task,
                low: bounds.start,
                high: bounds.end,
            },
        );
        let live = self.task_activation_is_live(
            inferior,
            &root,
            view,
            Activation::on_task(task, activation),
            evidence,
        )?;

        Ok((!live).then_some(WatchpointInvalidation::ScopeExited))
    }

    /// Whether the activation that declared an object still runs on a
    /// task's stack, inside the object's scope. The task's frames are read
    /// across the stacks its runtime runs it on, from the runtime's own
    /// stacks onto the task's.
    fn task_activation_is_live(
        &self,
        inferior: &Inferior,
        root: &StackRoot,
        view: StackView,
        activation: Activation,
        evidence: &FrameScopeEvidence,
    ) -> Result<bool> {
        let Some(module) = self
            .modules
            .get(&evidence.module)
            .filter(|module| module.loaded.image == evidence.image)
        else {
            return Ok(false);
        };

        let stack = self.physical_stack(inferior, root, DEFAULT_MAX_FRAMES)?;

        // Each frame's caller carries the frame's canonical frame address.
        for (index, caller) in stack.frames.iter().enumerate().skip(1) {
            let Some(cfa) = caller.context.cfa else {
                continue;
            };

            let frame = view.activation(cfa);
            if frame == activation {
                let Some(address) = stack
                    .lookup_address(index - 1)
                    .and_then(|address| module.loaded.image_address(address).ok())
                    .filter(|address| module.image.contains_address(*address))
                else {
                    return Ok(false);
                };

                let location = module.image.locate(address);
                return Ok(location.physical_instance == Some(evidence.function)
                    && evidence.ranges.iter().any(|range| range.contains(address)));
            }

            if activation.is_callee_of(frame) {
                return Ok(false);
            }
        }

        Ok(stack.termination != crate::UnwindTermination::Complete)
    }
}

impl<P: LinuxTraceOps> Controller<P> {
    /// Plants the stack mover's entry in each runtime one of whose tasks
    /// has a watch, and removes the rest, with the returns of moves no
    /// watch waits for any more.
    pub(super) fn sync_stack_movers(&mut self) -> Result<()> {
        let Some(inferior) = self.inferior.as_ref() else {
            return Ok(());
        };

        let watched = watched_tasks(inferior);
        let wanted = self
            .runtimes(inferior)
            .into_iter()
            .filter(|runtime| watched.iter().any(|task| task.runtime == runtime.id))
            .filter_map(|runtime| {
                let entry = runtime.model.stack_mover()?;
                Some((runtime.module.virtual_address(entry).ok()?, runtime.id))
            })
            .collect::<BTreeMap<_, _>>();
        let inferior = self.inferior.as_mut().ok_or(Error::NotRunning)?;
        inferior
            .stack_moves
            .moving
            .retain(|moving| watched.contains(&moving.task) && !moving.returned);

        let returns = inferior
            .stack_moves
            .moving
            .iter()
            .map(|moving| moving.returns_to)
            .collect::<BTreeSet<_>>();
        let planted = inferior.stack_moves.planted();
        let needed = wanted
            .keys()
            .chain(&returns)
            .copied()
            .collect::<BTreeSet<_>>();

        // The record changes with each site, so that a failed edit leaves
        // it naming exactly the sites planted.
        for &address in planted.difference(&needed) {
            remove_breakpoint_owner_from(
                &self.ptrace,
                inferior,
                address,
                BreakpointOwner::StackMove,
            )?;
            inferior.stack_moves.forget(address);
        }

        let pid = inferior.memory_thread();
        for &address in needed.difference(&planted) {
            self.ptrace.install_breakpoint(
                pid,
                &mut inferior.breakpoints,
                address,
                BreakpointOwner::StackMove,
            )?;
            match wanted.get(&address) {
                Some(&runtime) => {
                    inferior.stack_moves.movers.insert(address, runtime);
                }
                None => {
                    inferior.stack_moves.returns.insert(address);
                }
            }
        }

        inferior.stack_moves.movers = wanted;
        inferior.stack_moves.returns = returns;
        Ok(())
    }

    /// Notes a thread that reached a stack mover's entry or return, if
    /// `address` is one, and stops every thread to set its task's watches
    /// aside, or place them on its new stack.
    pub(super) fn note_stack_move(&mut self, pid: Pid, address: VirtualAddress) -> Result<()> {
        let inferior = self.inferior.as_ref().ok_or(Error::NotRunning)?;
        let moves = &inferior.stack_moves;
        if !moves.movers.contains_key(&address) && !moves.returns.contains(&address) {
            return Ok(());
        }

        let registers = self.ptrace.registers(pid)?;
        if let Some(&runtime) = moves.movers.get(&address) {
            let Some(runtime) = self
                .runtimes(inferior)
                .into_iter()
                .find(|bound| bound.id == runtime)
            else {
                return Ok(());
            };

            let task = self.with_runtime_stop(inferior, &runtime, pid, |stop| {
                runtime
                    .model
                    .moving_task(stop, &x86_64_registers(&registers))
            });
            let task = match task {
                Ok(number) => TaskId {
                    runtime: runtime.id,
                    number,
                },
                // A move of a stack the runtime cannot name may be any
                // watched task's.
                #[cfg_attr(
                    not(debug_assertions),
                    expect(unused_variables, reason = "only recorded")
                )]
                Err(reason) => {
                    record!("a stack moves whose task is unknown: {reason}");
                    let inferior = self.inferior.as_mut().ok_or(Error::NotRunning)?;
                    inferior.stack_moves.unknown = true;
                    return self.queue_stack_follow();
                }
            };

            if !watched_tasks(inferior).contains(&task) {
                return Ok(());
            }

            let returns_to = VirtualAddress::new(self.ptrace.read_word(pid, registers.rsp)?);
            record!("task {task}'s stack moves on {pid}, returning to {returns_to}");

            let inferior = self.inferior.as_mut().ok_or(Error::NotRunning)?;
            inferior.stack_moves.moving.push(Move {
                thread: pid,
                task,
                returns_to,
                stack: registers.rsp.wrapping_add(8),
                returned: false,
            });

            return self.queue_stack_follow();
        }

        let inferior = self.inferior.as_mut().ok_or(Error::NotRunning)?;
        if let Some(moving) = inferior.stack_moves.moving.iter_mut().find(|moving| {
            moving.thread == pid
                && moving.returns_to == address
                && moving.stack == registers.rsp
                && !moving.returned
        }) {
            record!("task {}'s stack moved", moving.task);
            moving.returned = true;
            return self.queue_stack_follow();
        }

        Ok(())
    }

    /// Stops every thread to follow the stack moves noted.
    fn queue_stack_follow(&mut self) -> Result<()> {
        self.queue_internal_edit(Edit::FollowStacks)
    }

    /// With every thread stopped, sets the watches of tasks whose stacks are
    /// moving aside, and places those whose moves finished on their new
    /// stacks. Watches that could not be are ended, rather than left where
    /// their objects may no longer be.
    pub(super) fn follow_stacks(&mut self) {
        #[cfg_attr(
            not(debug_assertions),
            expect(unused_variables, reason = "only recorded")
        )]
        if let Err(error) = self.try_follow_stacks() {
            record!("stack watches could not follow their stacks: {error}");
            #[cfg_attr(
                not(debug_assertions),
                expect(unused_variables, reason = "only recorded")
            )]
            if let Err(error) = self.end_task_watches(|_| true) {
                record!("stack watches could not be ended: {error}");
            }
        }
    }

    fn try_follow_stacks(&mut self) -> Result<()> {
        let inferior = self.inferior.as_mut().ok_or(Error::NotRunning)?;
        if std::mem::take(&mut inferior.stack_moves.unknown) {
            return self.end_task_watches(|_| true);
        }

        let inferior = self.inferior.as_ref().ok_or(Error::NotRunning)?;
        let moving = inferior
            .stack_moves
            .moving
            .iter()
            .filter(|moving| !moving.returned)
            .map(|moving| moving.task)
            .collect::<BTreeSet<_>>();
        let mut placed = Vec::new();
        let mut lost = Vec::new();
        for (&id, record) in &inferior.watch.watchpoints {
            let Some(watch) = record.task else {
                continue;
            };

            if moving.contains(&watch.task) {
                placed.push((id, None));
                continue;
            }

            match self.task_stack_bounds(inferior, watch.task) {
                Ok(Some(bounds)) => match bounds.end.checked_sub(watch.below_top) {
                    Some(address) => placed.push((id, Some(VirtualAddress::new(address)))),
                    None => lost.push(id),
                },
                // A task that is gone ended its activation; the next stop
                // says so.
                Ok(None) => {}

                #[cfg_attr(
                    not(debug_assertions),
                    expect(unused_variables, reason = "only recorded")
                )]
                Err(reason) => {
                    record!("task {}'s stack is unreadable: {reason}", watch.task);
                    lost.push(id);
                }
            }
        }

        let mut plan = inferior.watch.plan.clone();
        let mut moved = BTreeMap::new();
        for (id, address) in placed {
            let record = &inferior.watch.watchpoints[&id];
            plan = plan.without_watchpoint(id);
            let Some(address) = address else {
                continue;
            };

            let access = super::watchpoints::slot_access(record.watchpoint.access);

            // A disabled watch follows its stack too, so that enabling it
            // watches where the value is, but holds no slots meanwhile.
            let placed = debug_registers::split_range(address.get(), record.watchpoint.byte_size)
                .ok()
                .and_then(|chunks| {
                    if !record.watchpoint.enabled {
                        return Some((None, chunks));
                    }

                    plan.with_watchpoint(id, &chunks, access)
                        .ok()
                        .map(|next| (Some(next), chunks))
                });
            match placed {
                Some((next, chunks)) => {
                    if let Some(next) = next {
                        plan = next;
                    }

                    if address != record.watchpoint.address {
                        moved.insert(id, (address, chunks));
                    }
                }
                None => lost.push(id),
            }
        }

        self.arm_all_threads(plan)?;

        let inferior = self.inferior.as_mut().ok_or(Error::NotRunning)?;
        for (id, (address, chunks)) in &moved {
            let record = inferior
                .watch
                .watchpoints
                .get_mut(id)
                .ok_or_else(|| Error::internal("a moved watchpoint exists"))?;
            record!(
                "watchpoint {id} follows its stack from {} to {address}",
                record.watchpoint.address
            );
            record.watchpoint.address = *address;
            record.watchpoint.coverage = chunks
                .iter()
                .map(|chunk| AddressRange {
                    start: VirtualAddress::new(chunk.address),
                    end: VirtualAddress::new(chunk.end()),
                })
                .collect::<Vec<_>>()
                .into();
        }

        if !moved.is_empty() {
            self.publish_watchpoints_changed();
        }

        self.end_task_watches(|id| lost.contains(&id))?;
        self.sync_stack_movers()
    }

    /// Ends the task watches `ending` names, whose stacks moved where the
    /// debugger could not follow.
    fn end_task_watches(&mut self, ending: impl Fn(WatchpointId) -> bool) -> Result<()> {
        let inferior = self.inferior.as_ref().ok_or(Error::NotRunning)?;
        let invalidated = inferior
            .watch
            .watchpoints
            .iter()
            .filter(|(id, record)| record.task.is_some() && ending(**id))
            .map(|(_, record)| InvalidatedWatchpoint {
                watchpoint: record.watchpoint.clone(),
                reason: WatchpointInvalidation::StackMoved,
            })
            .collect::<Vec<_>>();
        self.remove_invalidated_watchpoints(invalidated)
    }
}

/// The tasks with a watch on their stacks.
fn watched_tasks(inferior: &Inferior) -> BTreeSet<TaskId> {
    inferior
        .watch
        .watchpoints
        .values()
        .filter_map(|record| match record.watchpoint.scope {
            WatchScope::Task { task, .. } => Some(task),
            _ => None,
        })
        .collect()
}
