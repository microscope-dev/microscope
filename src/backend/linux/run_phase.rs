//! Every live inferior moves through `Idle` -> `Running` -> `Gathering` -> `Stopped`.
//! Separating `Gathering` from `Stopped` keeps half-stopped states unrepresentable to
//! callers and holds breakpoint/watchpoint edits until all threads have settled.

use nix::unistd::Pid;

use crate::protocol::{ExecutionId, StopReason};
use crate::{Error, ExitStatus};

use super::classify::visible_stop_priority;
use super::{ActiveExecution, Edit, PublicStop, Result, debug_thread_id};

pub(super) struct RunPhase(Phase);

enum Phase {
    Idle,
    Running(ActiveExecution),

    Gathering {
        execution: Option<ActiveExecution>,
        barrier: StopBarrier,
    },
    Stopped(PublicStop),
}

impl RunPhase {
    pub(super) const fn running(execution: ActiveExecution) -> Self {
        Self(Phase::Running(execution))
    }

    pub(super) const fn gathering(
        execution: Option<ActiveExecution>,
        barrier: StopBarrier,
    ) -> Self {
        Self(Phase::Gathering { execution, barrier })
    }

    pub(super) const fn stopped(stop: PublicStop) -> Self {
        Self(Phase::Stopped(stop))
    }

    pub(super) const fn idle() -> Self {
        Self(Phase::Idle)
    }

    pub(super) const fn is_idle(&self) -> bool {
        matches!(self.0, Phase::Idle)
    }

    pub(super) const fn is_running(&self) -> bool {
        matches!(self.0, Phase::Running(_))
    }

    pub(super) const fn execution(&self) -> Option<&ActiveExecution> {
        match &self.0 {
            Phase::Running(execution)
            | Phase::Gathering {
                execution: Some(execution),
                ..
            } => Some(execution),
            _ => None,
        }
    }

    pub(super) const fn execution_mut(&mut self) -> Option<&mut ActiveExecution> {
        match &mut self.0 {
            Phase::Running(execution)
            | Phase::Gathering {
                execution: Some(execution),
                ..
            } => Some(execution),
            _ => None,
        }
    }

    pub(super) const fn barrier(&self) -> Option<&StopBarrier> {
        match &self.0 {
            Phase::Gathering { barrier, .. } => Some(barrier),
            _ => None,
        }
    }

    pub(super) const fn barrier_mut(&mut self) -> Option<&mut StopBarrier> {
        match &mut self.0 {
            Phase::Gathering { barrier, .. } => Some(barrier),
            _ => None,
        }
    }

    pub(super) const fn stop(&self) -> Option<&PublicStop> {
        match &self.0 {
            Phase::Stopped(stop) => Some(stop),
            _ => None,
        }
    }

    pub(super) const fn stop_mut(&mut self) -> Option<&mut PublicStop> {
        match &mut self.0 {
            Phase::Stopped(stop) => Some(stop),
            _ => None,
        }
    }

    pub(super) fn begin(&mut self, execution: ActiveExecution) {
        debug_assert!(
            self.barrier()
                .is_none_or(|barrier| barrier.edits.is_empty()),
            "beginning execution {} drops the edits a stop holds",
            execution.id
        );
        self.0 = Phase::Running(execution);
    }

    pub(super) fn gather(&mut self, barrier: StopBarrier) -> Result<()> {
        self.0 = match std::mem::replace(&mut self.0, Phase::Idle) {
            Phase::Running(execution) => Phase::Gathering {
                execution: Some(execution),
                barrier,
            },
            Phase::Idle => Phase::Gathering {
                execution: None,
                barrier,
            },
            phase @ (Phase::Gathering { .. } | Phase::Stopped(_)) => {
                self.0 = phase;
                return Err(Error::internal(
                    "a stop is gathered only while the inferior runs",
                ));
            }
        };
        Ok(())
    }

    pub(super) fn resume(&mut self) -> bool {
        self.0 = match std::mem::replace(&mut self.0, Phase::Idle) {
            Phase::Gathering {
                execution: Some(execution),
                barrier,
            } => {
                debug_assert!(
                    barrier.edits.is_empty(),
                    "an internal stop resumes with edits it never applied"
                );
                Phase::Running(execution)
            }
            Phase::Gathering {
                execution: None, ..
            } => Phase::Idle,
            phase => {
                debug_assert!(false, "only a stop being gathered resumes");
                phase
            }
        };
        self.execution().is_some()
    }

    pub(super) fn publish(&mut self, stop: PublicStop) -> Result<Option<ExecutionId>> {
        let execution = match std::mem::replace(&mut self.0, Phase::Idle) {
            Phase::Gathering { execution, barrier } => {
                debug_assert!(
                    barrier.edits.is_empty(),
                    "publishing stop {} drops edits queued after the stop's edits were applied",
                    stop.id
                );
                execution.map(|active| active.id)
            }
            phase => {
                self.0 = phase;
                return Err(Error::internal("only a stop being gathered is published"));
            }
        };
        self.0 = Phase::Stopped(stop);
        Ok(execution)
    }

    pub(super) fn release(&mut self) -> bool {
        if !matches!(self.0, Phase::Stopped(_)) {
            return false;
        }
        self.0 = Phase::Idle;
        true
    }
}

/// Tracks `paused` and `ended` separately from `reason` so that if a deferred edit deletes
/// or disables the breakpoint/watchpoint that triggered the barrier, `settle_dropped_hit`
/// still falls back to a visible stop instead of silently turning the barrier internal.
pub(super) struct StopBarrier {
    triggering_thread: Pid,

    reason: Option<StopReason>,

    paused: bool,

    ended: Option<StopReason>,

    edits: Vec<Edit>,
}

impl StopBarrier {
    pub(super) fn visible(triggering_thread: Pid, reason: StopReason) -> Self {
        Self {
            triggering_thread,
            paused: reason == StopReason::Pause,
            reason: Some(reason),
            ended: None,
            edits: Vec::new(),
        }
    }

    pub(super) const fn internal(triggering_thread: Pid) -> Self {
        Self {
            triggering_thread,
            reason: None,
            paused: false,
            ended: None,
            edits: Vec::new(),
        }
    }

    pub(super) const fn triggering_thread(&self) -> Pid {
        self.triggering_thread
    }

    pub(super) const fn reason(&self) -> Option<&StopReason> {
        self.reason.as_ref()
    }

    pub(super) fn publishable(&self) -> Option<(Pid, StopReason)> {
        Some((self.triggering_thread, self.reason.clone()?))
    }

    pub(super) fn pause(&mut self) {
        self.reason.get_or_insert(StopReason::Pause);
        self.paused = true;
    }

    pub(super) fn offer(&mut self, thread: Option<Pid>, reason: StopReason) {
        if self
            .reason
            .as_ref()
            .is_none_or(|current| visible_stop_priority(&reason) > visible_stop_priority(current))
        {
            if let Some(thread) = thread {
                self.triggering_thread = thread;
            }
            self.reason = Some(reason);
        }
    }

    pub(super) fn end_execution(&mut self, reason: StopReason) {
        self.reason.get_or_insert_with(|| reason.clone());
        self.ended = Some(reason);
    }

    pub(super) fn thread_exited(&mut self, pid: Pid, replacement: Pid, status: &ExitStatus) {
        if self.triggering_thread != pid {
            return;
        }
        self.triggering_thread = replacement;
        if self.reason.is_some() {
            self.reason = Some(StopReason::ThreadExited {
                thread_id: debug_thread_id(pid),
                status: status.clone(),
            });
        }
    }

    pub(super) fn restate_watchpoint(&mut self, pid: Pid, reason: &StopReason) {
        if self.triggering_thread == pid
            && matches!(self.reason, Some(StopReason::Watchpoint { .. }))
        {
            self.reason = Some(reason.clone());
        }
    }

    pub(super) fn settle_dropped_hit(
        &mut self,
        own: Option<StopReason>,
        best: Option<(Pid, StopReason)>,
    ) {
        if !matches!(
            self.reason,
            Some(StopReason::Breakpoint { .. } | StopReason::Watchpoint { .. })
        ) {
            return;
        }

        self.reason = own;
        if self.reason.is_some() {
            return;
        }

        if let Some((pid, reason)) = best {
            self.triggering_thread = pid;
            self.reason = Some(reason);
        } else if let Some(reason) = self.ended.clone() {
            self.reason = Some(reason);
        } else if self.paused {
            self.reason = Some(StopReason::Pause);
        }
    }

    pub(super) fn make_internal(&mut self) {
        self.reason = None;
        self.paused = false;
        self.ended = None;
    }

    pub(super) fn queue(&mut self, edit: Edit) {
        self.edits.push(edit);
    }

    pub(super) fn queue_once(&mut self, edit: Edit) {
        let kind = std::mem::discriminant(&edit);
        if !self
            .edits
            .iter()
            .any(|queued| std::mem::discriminant(queued) == kind)
        {
            self.edits.push(edit);
        }
    }

    pub(super) fn take_edits(&mut self) -> Vec<Edit> {
        std::mem::take(&mut self.edits)
    }
}

#[cfg(test)]
mod tests {

    use std::sync::Arc;

    use nix::unistd::Pid;
    use proptest::prelude::*;

    use super::StopBarrier;
    use crate::protocol::{ExceptionInfo, StopReason};
    use crate::{ExitStatus, VirtualAddress};

    const THREADS: [i32; 3] = [100, 101, 102];

    fn reason(kind: u8) -> StopReason {
        match kind % 7 {
            0 => StopReason::Pause,
            1 => StopReason::Attach,
            2 => StopReason::Exception(ExceptionInfo {
                code: 10,
                description: Arc::from("SIGUSR1"),
            }),
            3 => StopReason::Breakpoint {
                address: Some(VirtualAddress::new(0x40)),
                hits: Arc::from([]),
            },
            4 => StopReason::Watchpoint {
                hits: Arc::from([]),
            },
            5 => StopReason::Exec { followed: true },
            _ => StopReason::Unclassifiable {
                description: Arc::from("odd"),
            },
        }
    }

    #[derive(Debug, Clone)]
    enum Op {
        Offer(usize, u8),
        OfferUnowned(u8),
        Pause,
        End(u8),
        TriggeringExits(usize),
        Settle(Option<u8>, Option<(usize, u8)>),
        MakeInternal,
    }

    fn op() -> impl Strategy<Value = Op> {
        prop_oneof![
            (0..THREADS.len(), any::<u8>()).prop_map(|(thread, kind)| Op::Offer(thread, kind)),
            any::<u8>().prop_map(Op::OfferUnowned),
            Just(Op::Pause),
            any::<u8>().prop_map(Op::End),
            (0..THREADS.len()).prop_map(Op::TriggeringExits),
            (
                proptest::option::of(any::<u8>()),
                proptest::option::of((0..THREADS.len(), any::<u8>()))
            )
                .prop_map(|(own, best)| Op::Settle(own, best)),
            Just(Op::MakeInternal),
        ]
    }

    fn rank(reason: &StopReason) -> u8 {
        match reason {
            StopReason::Pause | StopReason::Attach => 0,
            StopReason::Exception(_) => 1,
            StopReason::Breakpoint { .. }
            | StopReason::Watchpoint { .. }
            | StopReason::ThreadExited { .. } => 2,
            StopReason::Exec { .. } => 3,
            StopReason::Unclassifiable { .. } => 4,
            other => unreachable!("the model makes no {other:?}"),
        }
    }

    fn priority(barrier: &StopBarrier) -> Option<u8> {
        barrier.reason().map(rank)
    }

    proptest! {
        #[test]
        fn a_barrier_publishes_what_its_rules_say(
            visible_start: bool,
            ops in proptest::collection::vec(op(), 0..24),
        ) {
            let first = Pid::from_raw(THREADS[0]);
            let mut barrier = if visible_start {
                StopBarrier::visible(first, StopReason::Pause)
            } else {
                StopBarrier::internal(first)
            };

            let mut floor = visible_start;

            for op in ops {
                let before = priority(&barrier);
                let was_hit = matches!(
                    barrier.reason(),
                    Some(StopReason::Breakpoint { .. } | StopReason::Watchpoint { .. })
                );
                let triggering = barrier.triggering_thread();
                match op {
                    Op::Offer(thread, kind) => {
                        let offered = reason(kind);
                        let thread = Pid::from_raw(THREADS[thread]);
                        barrier.offer(Some(thread), offered.clone());

                        let offered_priority = rank(&offered);
                        let wins = before.is_none_or(|before| offered_priority > before);
                        prop_assert_eq!(
                            priority(&barrier),
                            Some(before.map_or(offered_priority, |before| before.max(offered_priority)))
                        );
                        if wins {
                            prop_assert_eq!(barrier.reason(), Some(&offered));
                            prop_assert_eq!(barrier.triggering_thread(), thread);
                        } else {
                            prop_assert_eq!(barrier.triggering_thread(), triggering);
                        }
                    }
                    Op::OfferUnowned(kind) => {
                        barrier.offer(None, reason(kind));
                        prop_assert_eq!(barrier.triggering_thread(), triggering);
                        prop_assert!(priority(&barrier) >= Some(rank(&reason(kind))));
                    }
                    Op::Pause => {
                        barrier.pause();
                        floor = true;
                        prop_assert_eq!(priority(&barrier), Some(before.unwrap_or(0)));
                    }
                    Op::End(kind) => {
                        let ended = reason(kind);
                        barrier.end_execution(ended.clone());
                        floor = true;
                        if before.is_none() {
                            prop_assert_eq!(barrier.reason(), Some(&ended));
                        } else {
                            prop_assert_eq!(priority(&barrier), before);
                        }
                    }
                    Op::TriggeringExits(replacement) => {
                        let replacement = Pid::from_raw(THREADS[replacement]);
                        barrier.thread_exited(triggering, replacement, &ExitStatus::Code(0));
                        prop_assert_eq!(barrier.triggering_thread(), replacement);
                        prop_assert_eq!(barrier.reason().is_some(), before.is_some());
                        if before.is_some() {
                            prop_assert!(
                                matches!(barrier.reason(), Some(StopReason::ThreadExited { .. })),
                                "{:?}",
                                barrier.reason()
                            );
                        }
                    }
                    Op::Settle(own, best) => {
                        let own = own.map(reason);
                        let best = best.map(|(thread, kind)| (Pid::from_raw(THREADS[thread]), reason(kind)));
                        let reason_before = barrier.reason().cloned();
                        barrier.settle_dropped_hit(own.clone(), best.clone());
                        if !was_hit {
                            prop_assert_eq!(barrier.reason().cloned(), reason_before);
                        } else if let Some(own) = own {
                            prop_assert_eq!(barrier.reason(), Some(&own));
                            prop_assert_eq!(barrier.triggering_thread(), triggering);
                        } else if let Some((thread, best)) = best {
                            prop_assert_eq!(barrier.reason(), Some(&best));
                            prop_assert_eq!(barrier.triggering_thread(), thread);
                        } else {
                            prop_assert_eq!(barrier.reason().is_some(), floor);
                        }
                    }
                    Op::MakeInternal => {
                        barrier.make_internal();
                        floor = false;
                        prop_assert_eq!(barrier.reason(), None);
                    }
                }

                if floor {
                    prop_assert!(barrier.reason().is_some(), "the stop turned internal");
                }
            }
        }
    }
}
