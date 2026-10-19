//! Processes: fork, how a process ends and who reaps it, and what a child's
//! end tells its parent (K-FORK-1 to K-FORK-3).
//!
//! The tracer reaps a traced thread first. Once a process's last thread is
//! gone, its parent reaps it: the tracer, which launched it; whatever
//! started it untraced, at once; the process that forked it, with `wait4`,
//! after SIGCHLD; or, the parent gone, init, at once. A thread that exits
//! hands the children it forked to another of its group, or, the last, to
//! init.

use nix::errno::Errno;
use nix::libc;

use super::syscalls::failure;
use super::{
    Ended, ExitStatus, Happening, Kernel, Parent, Pending, Process, SigInfo, State, StopKind, Tgid,
    Thread, Tid, Tracing, Zombie,
};

use crate::sim::cpu::RAX;

/// `wait4`'s option to return at once when no child has exited.
const WNOHANG: u64 = 1;

/// The process id `getppid` reports once init has taken a process on.
pub(super) const INIT: Tid = 1;

/// The process id of whatever started a program untraced.
const LAUNCHER: Tid = 2;

/// The status `wait4` stores for a child that ended this way.
const fn wait_word(status: ExitStatus) -> i32 {
    match status {
        ExitStatus::Code(code) => (code & 0xff) << 8,
        ExitStatus::Signal(signal, core) => signal | if core { 0x80 } else { 0 },
    }
}

/// SIGCHLD's `si_code` for a child that ended this way.
const fn child_code(status: ExitStatus) -> i32 {
    match status {
        ExitStatus::Code(_) => libc::CLD_EXITED,
        ExitStatus::Signal(_, false) => libc::CLD_KILLED,
        ExitStatus::Signal(_, true) => libc::CLD_DUMPED,
    }
}

impl Kernel {
    /// `fork()` by `tid`. The child is a process of its own with one
    /// thread, a copy of the caller returning zero from the call, in a copy
    /// of the caller's memory, traps included. A caller tracing forks stops
    /// at `PTRACE_EVENT_FORK` inside the call, and the child, traced with
    /// the caller's options, first stops for a SIGSTOP nobody sent; neither
    /// order of the two stops is fixed. Otherwise the child is untraced
    /// (K-FORK-1). Made with a signal pending, it forks nothing, to be made
    /// again (K-SYS-7).
    pub(super) fn fork(&mut self, tid: Tid) {
        if self.signal_pending(tid) {
            self.fail_to_restart(tid);
            return;
        }

        let child = self.allocate_tid();
        let caller = &self.threads[&tid];
        let traced = caller.traced() && caller.options.trace_fork;
        let group = caller.tgid;

        // The child returns through its parent's calls, on its own copy of
        // the stack.
        let shadow = caller.shadow.clone();
        let thread = Thread::created(
            caller,
            child,
            Tgid::led_by(child),
            super::syscalls::SYS_FORK,
            traced,
            shadow,
        );

        let start_time = self.allocate_start_time();
        let parent = &self.processes[&group];
        let process = Process {
            tgid: Tgid::led_by(child),
            name: parent.name.clone(),
            space: parent.space.clone(),
            image: parent.image.clone(),
            parent: Parent::Process(group),
            creator: Some(tid),
            root: parent.root,
            shared: Pending::default(),
            group_exit: None,
            killed_externally: false,
            stopped: None,
            continued: false,
            start_time,
        };

        self.processes.insert(Tgid::led_by(child), process);
        self.threads.insert(child, thread);
        self.happenings
            .push(Happening::Forked { parent: tid, child });

        let result = u64::try_from(child).expect("process ids are positive");
        if traced {
            self.event_stop(tid, libc::PTRACE_EVENT_FORK, result, result);
        } else {
            self.threads
                .get_mut(&tid)
                .expect("the caller exists")
                .registers
                .general[RAX] = result;
        }
    }

    /// `wait4(pid, status, options, NULL)` by `tid`, for one child, with
    /// `WNOHANG`: reaps the child if it ended, storing its status, and
    /// returns its id; returns zero while it lives, and `ECHILD` for a
    /// process that is not the caller's child. Returns the result as `rax`
    /// holds it.
    pub(super) fn wait4(&mut self, tid: Tid, pid: u64, status: u64, options: u64) -> u64 {
        let Ok(child) = Tid::try_from(pid.cast_signed()) else {
            return failure(Errno::ECHILD);
        };

        let process = Tgid::new(child);
        if child <= 0 || options != WNOHANG {
            self.gap(format!(
                "wait4 for {} with options {options:#x}",
                pid.cast_signed()
            ));
            return failure(Errno::EINVAL);
        }

        let group = self.threads[&tid].tgid;
        if let Some(zombie) = self
            .zombies
            .get(&process)
            .filter(|zombie| zombie.parent == group)
        {
            let word = wait_word(zombie.status);
            if status != 0 {
                let process = self
                    .processes
                    .get_mut(&group)
                    .expect("the caller's process");

                // Linux reaps the child before it stores the status, and
                // then fails with EFAULT; no probe pins that.
                if process.space.write(status, &word.to_ne_bytes()).is_err() {
                    self.gap("wait4 storing a status where it cannot");
                    return failure(Errno::ENOSYS);
                }
            }

            let zombie = self.zombies.remove(&process).expect("the zombie");
            self.ended.insert(
                process,
                Ended {
                    status: zombie.status,
                    root: zombie.root,
                    reaper: Parent::Process(group),
                    killed_externally: zombie.killed_externally,
                },
            );

            self.happenings
                .push(Happening::ReapedChild { parent: tid, child });

            return u64::try_from(child).expect("process ids are positive");
        }

        let lives = self
            .processes
            .get(&process)
            .is_some_and(|process| process.parent == Parent::Process(group));

        if lives { 0 } else { failure(Errno::ECHILD) }
    }

    /// `getppid()` by `tid`.
    pub(super) fn parent_of(&self, tid: Tid) -> u64 {
        let parent = match self.processes[&self.threads[&tid].tgid].parent {
            Parent::Tracer => self.tracer,
            Parent::Launcher => LAUNCHER,
            Parent::Process(parent) => parent.id(),
            Parent::Init => INIT,
        };

        u64::try_from(parent).expect("process ids are positive")
    }

    /// The children `tid` forked that their parent has not reaped, as
    /// `/proc/<tgid>/task/<tid>/children` lists them.
    #[must_use]
    pub fn children(&self, tid: Tid) -> Vec<Tid> {
        let live = self
            .processes
            .values()
            .filter(|process| process.creator == Some(tid))
            .map(|process| process.tgid.leader());
        let ended = self
            .zombies
            .iter()
            .filter(|(_, zombie)| zombie.creator == Some(tid))
            .map(|(child, _)| child.leader());
        let mut children = live.chain(ended).collect::<Vec<_>>();
        children.sort_unstable();
        children
    }

    /// Hands the children `tid` forked to another live thread of its
    /// group, as a thread exits, or, none being left, to init, which reaps
    /// those that ended (K-FORK-3).
    pub(super) fn forget_children(&mut self, tid: Tid) {
        let group = self.threads[&tid].tgid;
        let heir = self
            .threads_of(group)
            .find(|thread| thread.tid != tid && !matches!(thread.state, State::Zombie(_)))
            .map(|thread| thread.tid);
        let orphans = self
            .processes
            .values_mut()
            .filter(|process| process.creator == Some(tid));
        for process in orphans {
            process.creator = heir;
            if heir.is_none() {
                process.parent = Parent::Init;
            }
        }

        for zombie in self.zombies.values_mut() {
            if zombie.creator == Some(tid) {
                zombie.creator = heir;
            }
        }

        if heir.is_none() {
            let ended = self
                .zombies
                .iter()
                .filter(|(_, zombie)| zombie.parent == group)
                .map(|(&child, _)| child)
                .collect::<Vec<_>>();
            for child in ended {
                let zombie = self.zombies.remove(&child).expect("the zombie");
                self.reap_orphan(child, zombie.status, zombie.root, zombie.killed_externally);
            }
        }
    }

    /// Ends a process whose last thread is gone: its parent reaps it (see
    /// the module documentation).
    pub(super) fn end_process(&mut self, group: Tgid, exit: ExitStatus) {
        let process = self.processes.remove(&group).expect("the process");
        match process.parent {
            reaper @ (Parent::Tracer | Parent::Launcher) => {
                self.ended.insert(
                    group,
                    Ended {
                        status: exit,
                        root: process.root,
                        reaper,
                        killed_externally: process.killed_externally,
                    },
                );
            }
            Parent::Process(parent) if self.processes.contains_key(&parent) => {
                self.zombies.insert(
                    group,
                    Zombie {
                        parent,
                        creator: process.creator,
                        status: exit,
                        root: process.root,
                        killed_externally: process.killed_externally,
                    },
                );
                self.processes
                    .get_mut(&parent)
                    .expect("the parent lives")
                    .shared
                    .insert(SigInfo {
                        signal: libc::SIGCHLD,
                        code: child_code(exit),
                        pid: group.id(),
                        address: 0,
                    });
            }
            Parent::Process(_) | Parent::Init => {
                self.reap_orphan(group, exit, process.root, process.killed_externally);
            }
        }
    }

    fn reap_orphan(&mut self, tgid: Tgid, status: ExitStatus, root: Tid, killed_externally: bool) {
        self.ended.insert(
            tgid,
            Ended {
                status,
                root,
                reaper: Parent::Init,
                killed_externally,
            },
        );
        self.happenings.push(Happening::ReapedOrphan { tgid });
    }

    /// `tracer` exits, releasing every thread it still traces (K-WAIT-3).
    /// A running or exiting one runs on untraced, one at its exit event
    /// finishes exiting, and a zombie, which no request reaches, joins its
    /// process's end as though never traced. A thread held in another
    /// ptrace-stop, or `PTRACE_O_EXITKILL` ending live threads, is not
    /// modeled.
    pub fn forget_tracer(&mut self, tracer: i32) {
        let tracees = self
            .threads
            .values()
            .filter(|thread| thread.tracer() == Some(tracer))
            .map(|thread| thread.tid)
            .collect::<Vec<_>>();
        for tid in tracees {
            let thread = &self.threads[&tid];
            let group = thread.tgid;
            let live = self
                .threads_of(group)
                .any(|other| !matches!(other.state, State::Zombie(_) | State::Exiting(_)));
            if thread.held() || (thread.options.exit_kill && live) {
                self.gap(format!("the tracer exiting while it holds thread {tid}"));
                return;
            }

            // Only a thread that runs on can meet a trap left in its code.
            let planted = matches!(thread.state, State::Running)
                .then(|| self.planted(group))
                .flatten();
            self.happenings.push(Happening::Released {
                tid,
                tracer,
                planted,
            });

            let thread = self.threads.get_mut(&tid).expect("a traced thread");
            thread.tracing = Tracing::Untraced;
            thread.single_step = false;

            // K-SYS-6: an untraced thread stops at no call.
            thread.syscall_stops = false;
            thread.options = super::Options::default();
            thread.report = None;

            match thread.state {
                State::Zombie(_) => self.reap_untraced(tid),
                State::Stopped {
                    kind: StopKind::Exit(exit),
                    ..
                } => self.become_zombie(tid, exit),
                _ => {}
            }
        }
    }

    /// Ends an untraced thread that became a zombie: one other than its
    /// group's leader is reaped at once, and a leader, once alone, ends its
    /// process.
    pub(super) fn reap_untraced(&mut self, tid: Tid) {
        let thread = &self.threads[&tid];
        let group = thread.tgid;
        if tid != group.leader() {
            self.threads.remove(&tid);
        }

        self.end_with_untraced_leader(group);
    }

    /// Ends `group` when only its leader is left, an untraced zombie, which
    /// its parent then reaps (K-EXIT-5).
    pub(super) fn end_with_untraced_leader(&mut self, group: Tgid) {
        let leader_alone = self
            .threads
            .get(&group.leader())
            .is_some_and(|leader| !leader.traced() && matches!(leader.state, State::Zombie(_)))
            && self.threads_of(group).count() == 1;
        if leader_alone {
            let leader = self.threads.remove(&group.leader()).expect("the leader");
            let State::Zombie(own) = leader.state else {
                unreachable!("the leader is a zombie")
            };

            let process = &self.processes[&group];
            let exit = process.group_exit.unwrap_or(own);

            self.end_process(group, exit);
        }
    }
}
