//! Whether an inferior runs an execution, is gathering a stop, or has one
//! published, and the reason a stop being gathered will publish.
//!
//! The phase is one value with private parts, so it changes only through
//! the transitions here: an execution begins at a published stop, a
//! barrier gathers a stop while it runs, and the stop is published or the
//! execution resumes.

use nix::unistd::Pid;

use crate::protocol::{ExecutionId, StopReason};
use crate::{Error, ExitStatus};

use super::classify::visible_stop_priority;
use super::{ActiveExecution, Edit, PublicStop, Result, debug_thread_id};

pub(super) struct RunPhase(Phase);

enum Phase {
    /// No execution runs, no stop is being gathered, and none is
    /// published: only while an attach has yet to ask its threads to stop,
    /// and while a shutdown runs threads to collect the signals still
    /// queued for them before it detaches them.
    Idle,
    Running(ActiveExecution),

    /// Every thread is being stopped, for the execution, if any, that the
    /// stop ends or resumes.
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

    /// Whether an execution runs with no stop being gathered.
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

    /// Runs `execution` from a published stop, or, for a shutdown's
    /// inspector cleanup, in place of whatever the inferior was doing once
    /// the edits of any stop being gathered are applied.
    pub(super) fn begin(&mut self, execution: ActiveExecution) {
        debug_assert!(
            self.barrier()
                .is_none_or(|barrier| barrier.edits.is_empty()),
            "beginning execution {} drops the edits a stop holds",
            execution.id
        );
        self.0 = Phase::Running(execution);
    }

    /// Begins gathering a stop of whatever runs. A published stop has
    /// nothing running to stop, and a stop already being gathered keeps
    /// its own barrier.
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

    /// Ends an internal stop, running its execution on, and returns
    /// whether there is one.
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

    /// Publishes `stop`, ending the stop being gathered and the execution
    /// it stopped, whose id it returns.
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

    /// Leaves a published stop as a shutdown runs its threads to collect
    /// their queued signals, and returns whether there was one.
    pub(super) fn release(&mut self) -> bool {
        if !matches!(self.0, Phase::Stopped(_)) {
            return false;
        }
        self.0 = Phase::Idle;
        true
    }
}

/// Stops every thread of the inferior, then either publishes a stop or,
/// when no thread gave a visible reason, applies its edits and resumes the
/// active execution as if nothing happened.
pub(super) struct StopBarrier {
    triggering_thread: Pid,

    /// The stop to publish, or `None` while the stop is internal.
    reason: Option<StopReason>,

    /// Whether a client paused, so the stop is published as a pause when
    /// every reason it would have published is dropped. Never set while
    /// the stop is internal.
    paused: bool,

    /// Why the active execution ended during the stop, such as its
    /// stepping thread exiting. The execution cannot resume, so the stop
    /// publishes this when every other reason is dropped.
    ended: Option<StopReason>,

    /// Client edits applied once every thread is stopped.
    edits: Vec<Edit>,
}

impl StopBarrier {
    /// A barrier that publishes `reason` from `triggering_thread`.
    pub(super) fn visible(triggering_thread: Pid, reason: StopReason) -> Self {
        Self {
            triggering_thread,
            paused: reason == StopReason::Pause,
            reason: Some(reason),
            ended: None,
            edits: Vec::new(),
        }
    }

    /// A barrier that publishes nothing unless a thread gives it a reason.
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

    /// The thread and reason the stop publishes, unless it is internal.
    pub(super) fn publishable(&self) -> Option<(Pid, StopReason)> {
        Some((self.triggering_thread, self.reason.clone()?))
    }

    /// A client's pause: the stop publishes at least a pause.
    pub(super) fn pause(&mut self) {
        self.reason.get_or_insert(StopReason::Pause);
        self.paused = true;
    }

    /// Publishes `reason` instead of the current one if it matters more,
    /// from `thread` when the reason is a thread's.
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

    /// The execution ended in `reason` and cannot resume, so the stop
    /// publishes at least that.
    pub(super) fn end_execution(&mut self, reason: StopReason) {
        self.reason.get_or_insert_with(|| reason.clone());
        self.ended = Some(reason);
    }

    /// The triggering thread `pid` exited with `status`; `replacement`
    /// presents the stop instead. An internal stop stays internal.
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

    /// The watchpoint hit `pid` stopped for reads as `reason` now that the
    /// watched bytes are known.
    pub(super) fn restate_watchpoint(&mut self, pid: Pid, reason: &StopReason) {
        if self.triggering_thread == pid
            && matches!(self.reason, Some(StopReason::Watchpoint { .. }))
        {
            self.reason = Some(reason.clone());
        }
    }

    /// Settles the published reason once edits may have dropped the hit it
    /// was: the triggering thread's own reason as it now stands, `own`,
    /// then the most important of the other threads', `best`, then the end
    /// of an execution that cannot resume, then a pause the hit outranked.
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

    /// Makes the stop internal, so the threads run on once it applies its
    /// edits, as a shutdown's inspector cleanup needs.
    pub(super) fn make_internal(&mut self) {
        self.reason = None;
        self.paused = false;
        self.ended = None;
    }

    pub(super) fn queue(&mut self, edit: Edit) {
        self.edits.push(edit);
    }

    /// Queues `edit` unless an edit of its kind is already queued.
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
    //! The stop reason a barrier publishes, over random sequences of the
    //! reasons threads give, pauses, executions ending, the triggering
    //! thread exiting, and edits dropping hits. A small model says only
    //! whether the stop is visible, which priority it has at least, and
    //! what an offer must leave; the rules come from the all-stop
    //! description in `dev/process-control.md`.

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
        /// A reason no thread gave, as an agent's report.
        OfferUnowned(u8),
        Pause,
        End(u8),
        TriggeringExits(usize),
        /// Edits leave the triggering thread `own` and the best of the
        /// others `best`, each by its reason's kind, or nothing.
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

    /// Which of two reasons a barrier publishes, written out here rather
    /// than taken from the controller's own ranking, so a change to that
    /// ranking is a change to this table: a pause or attach gives way to a
    /// signal, a signal to a breakpoint, a watchpoint or the triggering
    /// thread's exit, those to an exec,
    /// and everything to a stop the controller cannot classify.
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

            // Whether something other than a hit an edit may drop, a pause
            // or an execution's end, keeps the stop visible.
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

                        // The more important reason wins; an equal one
                        // does not displace the first.
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
                        // A pause adds a reason, never replaces one.
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
                            // Only a hit can be dropped.
                            prop_assert_eq!(barrier.reason().cloned(), reason_before);
                        } else if let Some(own) = own {
                            prop_assert_eq!(barrier.reason(), Some(&own));
                            prop_assert_eq!(barrier.triggering_thread(), triggering);
                        } else if let Some((thread, best)) = best {
                            prop_assert_eq!(barrier.reason(), Some(&best));
                            prop_assert_eq!(barrier.triggering_thread(), thread);
                        } else {
                            // With nothing left of the hits, a pause or an
                            // end keeps the stop visible.
                            prop_assert_eq!(barrier.reason().is_some(), floor);
                        }
                    }
                    Op::MakeInternal => {
                        barrier.make_internal();
                        floor = false;
                        prop_assert_eq!(barrier.reason(), None);
                    }
                }

                // A pause or an end is never lost.
                if floor {
                    prop_assert!(barrier.reason().is_some(), "the stop turned internal");
                }
            }
        }
    }
}
