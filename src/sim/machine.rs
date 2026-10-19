//! The simulated machine the controllers' calls reach: the kernel, each
//! session's waiter, and the faults that may strike between any two calls.
//!
//! The world performs its actions through the machine, and each
//! controller's edge holds it for preemption: before each call into the
//! kernel takes effect, threads may run and waiters may reap, as on Linux.

use std::cell::{Cell, RefCell};
use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::rc::Rc;

use nix::libc;

use super::choices::{Choices, Stream};
use super::faults::Faults;
use super::kernel::{Happening, Kernel, Parent, Planted, State, Tgid, Tid, signals};
use super::marks::{Mark, Marks};
use super::schedule::{Action, Scheduler, SessionId};

#[cfg(test)]
use super::world::Sabotage;
use super::world::TRACER;
use crate::backend::sim_edge::{Preemption, SimWaiter};

/// The most instructions a thread runs at one preemption point.
const PREEMPT_BURST: u64 = 8;

pub struct Machine {
    pub kernel: Rc<RefCell<Kernel>>,

    /// Each session's waiter, by session.
    pub waiters: RefCell<Vec<Rc<RefCell<SimWaiter>>>>,
    pub choices: Rc<RefCell<Choices>>,
    pub scheduler: RefCell<Scheduler>,
    pub faults: RefCell<Faults>,
    pub marks: Rc<RefCell<Marks>>,

    /// Processes something outside the session killed.
    pub killed: Rc<RefCell<BTreeSet<Tgid>>>,

    /// Processes that began to end as a whole.
    pub ending: Rc<RefCell<BTreeSet<Tgid>>>,

    /// Whether the first session holds the children its program forks.
    pub following: Rc<Cell<bool>>,

    /// Threads the debugger released with code not the program's: the
    /// address, the byte there, and the program's.
    pub unclean: RefCell<Vec<(Tid, Planted)>>,

    /// Threads that received a SIGCONT, which only a debugger sends.
    pub leaked: RefCell<Vec<Tid>>,

    /// The action in progress.
    pub step: Cell<u64>,

    /// How many instructions threads executed so far.
    pub ran: Cell<u64>,

    /// How often, in a thousand, a call is preempted.
    pub preempt: u64,

    #[cfg(test)]
    pub sabotage: Option<Sabotage>,
}

impl Machine {
    /// Counts what the kernel did since the last look, and fires a fault
    /// planned for a new thread. Returns lines for the trace.
    pub fn absorb(&self) -> Vec<String> {
        // The client may see any request about a process ending as a whole
        // fail, or about one already gone: an untraced process can end
        // within one action.
        let kernel = self.kernel.borrow();
        let exiting = kernel
            .processes
            .values()
            .filter(|process| process.group_exit.is_some())
            .map(|process| process.tgid);
        let gone = kernel.ended.keys().chain(kernel.zombies.keys()).copied();
        self.ending.borrow_mut().extend(exiting.chain(gone));
        drop(kernel);

        let happenings = std::mem::take(&mut self.kernel.borrow_mut().happenings);
        let mut lines = Vec::new();

        for happening in happenings {
            match happening {
                Happening::Cloned { parent, child } => {
                    self.marks.borrow_mut().hit(Mark::ThreadCreated);
                    self.note_seized_child(child);
                    lines.push(format!("{parent} created {child}"));
                    if self.faults.borrow_mut().cloned() {
                        lines.extend(self.kill(Mark::KilledNearClone));
                    }
                }
                Happening::GroupExit { tid } => lines.push(format!("{tid} exits its group")),
                Happening::PulledFromStop { tid } => {
                    let externally = self
                        .kernel
                        .borrow()
                        .process_of(tid)
                        .is_some_and(|process| process.killed_externally);
                    if !externally {
                        self.marks.borrow_mut().hit(Mark::GroupExitEndedStop);
                    }

                    lines.push(format!("{tid} was taken out of its stop"));
                }
                Happening::InterruptAheadOfTrap { tid } => {
                    self.marks.borrow_mut().hit(Mark::InterruptAheadOfTrap);
                    lines.push(format!("an interrupt stops {tid} ahead of its trap"));
                }
                Happening::Restarting { tid } => {
                    self.marks.borrow_mut().hit(Mark::CallRestarted);
                    lines.push(format!("{tid}'s call fails, to be made again"));
                }
                Happening::LeaderExitedAlone { tid } => {
                    self.marks.borrow_mut().hit(Mark::LeaderExitedAlone);
                    lines.push(format!("leader {tid} exited alone"));
                }
                Happening::Yielded { tid } => self.scheduler.borrow_mut().yielded(tid),
                Happening::Forked { parent, child } => {
                    lines.extend(self.note_forked(parent, child));
                }

                Happening::Released {
                    tid,
                    tracer,
                    planted,
                } => {
                    self.note_released(tid, tracer, planted);
                    lines.push(format!("{tid} was released"));
                }
                Happening::ReapedChild { parent, child } => {
                    self.marks.borrow_mut().hit(Mark::ChildReaped);
                    lines.push(format!("{parent} reaped {child}"));
                }
                Happening::ReapedOrphan { tgid } => {
                    self.marks.borrow_mut().hit(Mark::OrphanReaped);
                    lines.push(format!("init reaped {tgid}"));
                }
                Happening::SeizeRefused { tid } => {
                    self.marks.borrow_mut().hit(Mark::SeizeRefused);
                    lines.push(format!("{tid} could not be seized"));
                }
                Happening::InterruptWaits { tid } => {
                    self.marks.borrow_mut().hit(Mark::InterruptWaited);
                    lines.push(format!("an interrupt waits for stopped {tid}"));
                }
                Happening::JobStopped { tid } => lines.push(format!("{tid} stopped untraced")),
                Happening::Continued { tgid } => lines.push(format!("SIGCONT ended {tgid}'s stop")),
                Happening::Suppressed { tid, signal } => {
                    if signal == libc::SIGCONT {
                        self.marks.borrow_mut().hit(Mark::HeldContinueSuppressed);
                    }

                    lines.push(format!(
                        "{tid} was resumed without {}",
                        signals::name(signal)
                    ));
                }
                Happening::Delivered { tid, signal } => {
                    // Only a debugger sends SIGCONT, which the program must
                    // never receive.
                    if signal == libc::SIGCONT {
                        self.leaked.borrow_mut().push(tid);
                    }

                    lines.push(format!("{tid} received {}", signals::name(signal)));
                }
            }
        }

        lines
    }

    /// Counts a fork, and fires a fault planned for one. Returns lines for
    /// the trace.
    fn note_forked(&self, parent: Tid, child: Tid) -> Vec<String> {
        let mut marks = self.marks.borrow_mut();
        marks.hit(Mark::Forked);
        if self
            .kernel
            .borrow()
            .process_of(parent)
            .is_some_and(|process| process.tgid.leader() != parent)
        {
            marks.hit(Mark::ForkedFromThread);
        }

        drop(marks);
        self.note_seized_child(child);
        let mut lines = vec![format!("{parent} forked {child}")];
        if self.faults.borrow_mut().forked() {
            lines.extend(self.kill(Mark::KilledNearFork));
        }

        lines
    }

    /// Counts a thread `tracer` released, and notes code it left that is not
    /// the program's. A session adopting a held child releases a fork
    /// child too, which the first session's marks do not count.
    fn note_released(&self, tid: Tid, tracer: i32, planted: Option<Planted>) {
        let kernel = self.kernel.borrow();
        let mut marks = self.marks.borrow_mut();
        let parent = kernel.process_of(tid).map(|process| process.parent);

        // A child held for a session of its own is detached stopping, not
        // released.
        let held = kernel.threads.get(&tid).is_some_and(|thread| {
            thread.state == State::JobStopped || thread.pending.contains(signals::SIGSTOP)
        });

        match parent.filter(|_| tracer == TRACER) {
            Some(Parent::Launcher) => marks.hit(Mark::Detached),
            Some(Parent::Process(_) | Parent::Init) if !held => {
                marks.hit(Mark::ForkChildReleased);
                if parent == Some(Parent::Init) {
                    marks.hit(Mark::ReleasedAfterParentExit);
                }

                if self.following.get() {
                    marks.hit(Mark::ReleasedWhileFollowing);
                }
            }
            Some(_) | None => {}
        }

        if let Some(planted) = planted {
            self.unclean.borrow_mut().push((tid, planted));
        }
    }

    /// Marks a new thread or process that starts in an interrupt's stop,
    /// its creator seized.
    fn note_seized_child(&self, child: Tid) {
        if self
            .kernel
            .borrow()
            .threads
            .get(&child)
            .is_some_and(super::kernel::Thread::seized)
        {
            self.marks.borrow_mut().hit(Mark::SeizedChildStarted);
        }
    }

    /// Kills the running program from outside, as the planned fault.
    /// Returns a line for the trace, or nothing when no program runs.
    pub fn kill(&self, mark: Mark) -> Option<String> {
        let mut kernel = self.kernel.borrow_mut();
        let tgid = *kernel
            .processes
            .iter()
            .find(|(_, process)| {
                matches!(process.parent, Parent::Tracer | Parent::Launcher)
                    && process.group_exit.is_none()
            })?
            .0;

        kernel.processes.get_mut(&tgid)?.killed_externally = true;
        kernel
            .kill(tgid, libc::SIGKILL)
            .expect("a live process can be killed");

        self.killed.borrow_mut().insert(tgid);
        self.ending.borrow_mut().insert(tgid);
        self.faults.borrow_mut().fired = Some(self.step.get());
        self.marks.borrow_mut().hit(mark);

        Some(format!("fault: SIGKILL from outside to {tgid}"))
    }

    /// Reaps one status for `session`, if its waiter can, choosing among
    /// the ready ones.
    pub fn collect(&self, session: SessionId) -> String {
        let mut kernel = self.kernel.borrow_mut();
        let waiter = Rc::clone(&self.waiters.borrow()[session]);
        let collected = waiter.borrow_mut().collect(&mut kernel, |ready| {
            *self.choices.borrow_mut().pick(Stream::Schedule, ready)
        });

        let mut line = if session == 0 {
            String::from("collect")
        } else {
            format!("collect for session {session}")
        };

        if let Some(status) = collected.reaped {
            let _ = write!(line, " {status}");
        }

        if collected.held {
            self.marks.borrow_mut().hit(Mark::QueueFull);
            line.push_str(" -> held, the queue is full");
        } else {
            line.push_str(" -> queued");
        }

        line
    }

    /// Whether `session`'s waiter can collect now.
    pub fn can_collect(&self, session: SessionId) -> bool {
        #[cfg(test)]
        if self.sabotage == Some(Sabotage::DeafWaiter) {
            return false;
        }

        self.waiters.borrow()[session]
            .borrow()
            .can_collect(&self.kernel.borrow())
    }

    /// The sessions whose waiters can collect now.
    pub fn collectable(&self) -> impl Iterator<Item = SessionId> + '_ {
        (0..self.waiters.borrow().len()).filter(|&session| self.can_collect(session))
    }

    /// Runs `tid` for up to `budget` instructions. Returns a line for the
    /// trace.
    pub fn run_thread(&self, tid: Tid, budget: u64) -> String {
        let mut kernel = self.kernel.borrow_mut();
        let ran = kernel.run(tid, budget);
        self.ran.set(self.ran.get() + ran.executed);
        let state = kernel.threads.get(&tid).map(|thread| thread.state);

        format!(
            "run {tid} x{}{} -> {}",
            ran.executed,
            if ran.yielded { " yielding" } else { "" },
            describe_state(state)
        )
    }
}

impl Preemption for Machine {
    fn before_call(&self) {
        if self.faults.borrow_mut().call()
            && let Some(line) = self.kill(Mark::KilledInsideCall)
        {
            record_inside_call(&line);
        }

        if !self
            .choices
            .borrow_mut()
            .chance(Stream::Preempt, self.preempt)
        {
            return;
        }

        let actors = self.choices.borrow_mut().below(Stream::Preempt, 3) + 1;
        for _ in 0..actors {
            let mut options = self
                .kernel
                .borrow()
                .runnable()
                .map(Action::Run)
                .collect::<Vec<_>>();
            options.extend(self.collectable().map(Action::Collect));
            if options.is_empty() {
                return;
            }

            let action = *self.choices.borrow_mut().pick(Stream::Preempt, &options);

            let line = match action {
                Action::Run(tid) => {
                    let budget = self
                        .choices
                        .borrow_mut()
                        .below(Stream::Preempt, PREEMPT_BURST)
                        + 1;
                    self.marks.borrow_mut().hit(Mark::PreemptedCall);
                    self.run_thread(tid, budget)
                }
                Action::Collect(session) => {
                    self.marks.borrow_mut().hit(Mark::ReapedInsideCall);
                    self.collect(session)
                }
                Action::Deliver(_) | Action::Poll(_) | Action::Follow => {
                    unreachable!("only threads and waiters act inside a call")
                }
            };

            record_inside_call(&format!("preempt: {line}"));

            for line in self.absorb() {
                record_inside_call(&format!("preempt: {line}"));
            }
        }
    }
}

/// Records what happened inside one of the controller's calls among the
/// controller's own lines, in the order it happened. Only development builds
/// record.
#[cfg_attr(
    not(debug_assertions),
    expect(clippy::missing_const_for_fn, reason = "development builds record")
)]
fn record_inside_call(line: &str) {
    #[cfg(debug_assertions)]
    record!("{line}");
    #[cfg(not(debug_assertions))]
    let _ = line;
}

fn describe_state(state: Option<State>) -> String {
    match state {
        None => "reaped".to_owned(),
        Some(State::Running) => "running".to_owned(),
        Some(State::Stopped { kind, .. }) => format!("stopped {kind:?}"),
        Some(State::JobStopped) => "job-control stopped".to_owned(),
        Some(State::Exiting(exit)) => format!("exiting {exit:?}"),
        Some(State::Zombie(exit)) => format!("zombie {exit:?}"),
    }
}
