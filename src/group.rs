//! A process group: the processes a program starts that microscope debugs
//! together.
//!
//! Every process is traced by one tracer thread. The group's root is the
//! session a client made, which launches or attaches as a single session
//! does. Each process the root's program starts that the follow policy
//! debugs becomes a member, a [`DebuggerHandle`] of its own that works as a
//! single session's does; the rest are watched until they execute a program
//! the policy debugs. Members are numbered from 1, the root, in the order
//! they joined, and keep their numbers after they exit.
//!
//! The root's breakpoints are the group's: each member gets every one of
//! them before it runs an instruction, and changes to them reach every
//! member. A stop in any member stops the group: every other member is
//! paused, and [`GroupEvent::GroupStopped`] reports the stop once they are,
//! unless the policy's [`StopScope`] says otherwise.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};

use tokio::sync::{broadcast, mpsc, oneshot};

use crate::backend::{self, GroupNotice, Joined};
use crate::{
    Breakpoint, BreakpointId, BreakpointOptions, Debugger, DebuggerEvent, DebuggerHandle, Error,
    ExceptionDisposition, ExitStatus, FollowPolicy, InferiorState, ProcessId, Program, Result,
    ResumeScope, StopReason, StopScope,
};

/// A member's number in its group, stable for the group's life.
pub type MemberNumber = u32;

/// What a group reports about its processes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GroupEvent {
    /// A process became a member, with the group's breakpoints planted
    /// before it ran an instruction.
    MemberJoined {
        member: MemberNumber,
        process: ProcessId,

        /// The member whose process started it, or none when a watched
        /// process did.
        parent: Option<MemberNumber>,

        /// The program it joined by executing; none for a fork child,
        /// which runs its parent's.
        executed: Option<PathBuf>,
    },

    /// A member's process exited.
    MemberExited {
        member: MemberNumber,
        status: ExitStatus,
    },

    /// A member's process executed a program the group does not debug, and
    /// is watched from now on.
    MemberWatched {
        member: MemberNumber,
        program: PathBuf,
    },

    /// A process the group watches executed `program`, which it does not
    /// debug either.
    ProcessWatched {
        process: ProcessId,

        /// The process that started it, when the group knew it.
        parent: Option<ProcessId>,
        program: PathBuf,
    },

    /// A process executed a program the group would debug but cannot
    /// load, and runs on watched.
    ProcessUnfollowed {
        process: ProcessId,
        program: PathBuf,
        reason: String,
    },

    /// A member stopped, and every other member is stopped too.
    GroupStopped {
        member: MemberNumber,
        reason: StopReason,
    },

    /// Every member has exited or left; watched processes may still run.
    GroupExited,
}

/// Where a member is in its life.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MemberState {
    /// It has no process yet, as a root before its launch.
    NotStarted,
    Running,
    Stopped(StopReason),
    Exited(ExitStatus),

    /// Its process executed `program`, which the group watches.
    Watched(PathBuf),

    /// Its process was detached, and runs on its own.
    Detached,
}

/// A member as [`ProcessGroup::members`] lists it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Member {
    pub number: MemberNumber,
    pub process: Option<ProcessId>,
    pub parent: Option<MemberNumber>,

    /// The program it joined by executing, if it did.
    pub executed: Option<PathBuf>,

    /// Its command line, as `/proc` showed it when it joined.
    pub command: String,
    pub state: MemberState,
}

/// The processes a program starts that microscope debugs together.
pub struct ProcessGroup {
    shared: Arc<Shared>,
    commands: mpsc::UnboundedSender<Command>,
    events: broadcast::Sender<GroupEvent>,
    root: DebuggerHandle,
    task: Option<tokio::task::JoinHandle<()>>,
    backend: Option<backend::GroupBackend>,
}

struct Shared {
    members: Mutex<Vec<Member>>,
    handles: Mutex<BTreeMap<MemberNumber, DebuggerHandle>>,
}

impl Shared {
    fn update(&self, number: MemberNumber, change: impl FnOnce(&mut Member)) {
        let mut members = self.members.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(member) = members.iter_mut().find(|member| member.number == number) {
            change(member);
        }
    }
}

enum Command {
    ContinueAll(oneshot::Sender<Result<()>>),
    PauseAll(oneshot::Sender<Result<()>>),
    Shutdown(oneshot::Sender<Result<()>>),
}

impl ProcessGroup {
    /// Makes a group whose root is a session for `program`, which a client
    /// launches or attaches through [`Self::root`]. The processes it starts
    /// are followed as `policy` says.
    pub fn new(program: Program, policy: FollowPolicy) -> Result<Self> {
        let stops = policy.stop;
        let (root, mut backend) = Debugger::for_group(program, policy)?;
        let notices = std::mem::replace(&mut backend.notices, mpsc::unbounded_channel().1);
        let handle = root.handle();
        let shared = Arc::new(Shared {
            members: Mutex::new(vec![Member {
                number: 1,
                process: None,
                parent: None,
                executed: None,
                command: handle.launched().display().to_string(),
                state: MemberState::NotStarted,
            }]),
            handles: Mutex::new(BTreeMap::from([(1, handle.clone())])),
        });
        let (commands, received) = mpsc::unbounded_channel();
        let (events, _) = broadcast::channel(GROUP_EVENT_CAPACITY);
        let task =
            Driver::new(Arc::clone(&shared), events.clone(), root, stops).run(notices, received);

        Ok(Self {
            shared,
            commands,
            events,
            root: handle,
            task: Some(tokio::spawn(task)),
            backend: Some(backend),
        })
    }

    /// Attaches a group's root to `process`, following the processes it
    /// starts from now on as `policy` says, and returns once the root is
    /// stopped. Processes it started before are not adopted. Ending the
    /// group releases every process it debugs or watches.
    pub async fn attach(
        process: ProcessId,
        policy: FollowPolicy,
        debug_files: &crate::DebugFileOptions,
    ) -> Result<Self> {
        let mut source = backend::process_executable_source(process)?;
        source.debug_files = crate::debug_info::DebugFileSearch::new(debug_files);
        let group = Self::new(Program::from_executable_source(source)?, policy)?;
        let mut events = group.subscribe();
        let attached = group
            .root
            .request(|reply| crate::protocol::Request::Attach {
                process_id: process,
                held: false,
                reply,
            })
            .await;

        // The group is stopped once its driver has seen the root's stop.
        let stopped = async {
            loop {
                match events.recv().await {
                    Ok(GroupEvent::GroupStopped { .. }) => return Ok(()),
                    Ok(_) | Err(broadcast::error::RecvError::Lagged(_)) => {}
                    Err(broadcast::error::RecvError::Closed) => {
                        return Err(Error::RequestCancelled);
                    }
                }
            }
        };
        let attached = match attached {
            Ok(_) => stopped.await,
            Err(error) => Err(error),
        };

        match attached {
            Ok(()) => Ok(group),
            Err(error) => {
                let _ = group.shutdown().await;
                Err(error)
            }
        }
    }

    /// The root member's handle, through which the group's program is
    /// launched or attached, and whose breakpoints are the group's.
    #[must_use]
    pub fn root(&self) -> DebuggerHandle {
        self.root.clone()
    }

    /// The handle of member `number`, while it has one.
    #[must_use]
    pub fn member(&self, number: MemberNumber) -> Option<DebuggerHandle> {
        self.shared
            .handles
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&number)
            .cloned()
    }

    /// Every member so far, in the order they joined.
    #[must_use]
    pub fn members(&self) -> Vec<Member> {
        self.shared
            .members
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    #[must_use]
    pub fn subscribe(&self) -> broadcast::Receiver<GroupEvent> {
        self.events.subscribe()
    }

    /// Resumes every stopped member. A stop a member made while the group
    /// was stopping is reported instead, as a [`GroupEvent::GroupStopped`],
    /// without resuming anything.
    pub async fn continue_all(&self) -> Result<()> {
        self.command(Command::ContinueAll).await
    }

    /// Pauses every running member, and reports the group stop once they
    /// are stopped.
    pub async fn pause_all(&self) -> Result<()> {
        self.command(Command::PauseAll).await
    }

    async fn command(
        &self,
        command: impl FnOnce(oneshot::Sender<Result<()>>) -> Command,
    ) -> Result<()> {
        let (reply, answer) = oneshot::channel();
        self.commands
            .send(command(reply))
            .map_err(|_| Error::RequestCancelled)?;
        answer.await.map_err(|_| Error::RequestCancelled)?
    }

    /// Ends every member, killing launched processes and releasing attached
    /// ones, then the processes the group watches, and joins the tracer
    /// thread.
    pub async fn shutdown(mut self) -> Result<()> {
        let result = self.command(Command::Shutdown).await;
        if let Some(task) = self.task.take() {
            let _ = task.await;
        }

        if let Some(backend) = self.backend.take() {
            tokio::task::spawn_blocking(move || backend.join())
                .await
                .map_err(|_| Error::BackendThreadPanicked)?
                .map_err(|()| Error::BackendThreadPanicked)?;
        }

        result
    }
}

impl Drop for ProcessGroup {
    fn drop(&mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

const GROUP_EVENT_CAPACITY: usize = 256;

/// A member as the group's driver keeps it.
struct Live {
    debugger: Debugger,
    handle: DebuggerHandle,
    process: Option<ProcessId>,

    /// The stop the member joined in, which is not one of its own.
    joined_at: Option<crate::StopId>,

    /// The member's copy of each of the root's breakpoints, by the root's
    /// id, with the root's settings it was given.
    mirrored: BTreeMap<BreakpointId, (BreakpointId, Settings)>,
    running: bool,
    gone: bool,
}

/// What a breakpoint's copy must agree on with the root's.
#[derive(Clone, PartialEq, Eq)]
struct Settings {
    enabled: bool,
    condition: Option<crate::Condition>,
    hit_condition: Option<crate::HitCondition>,
    log_message: Option<crate::LogMessage>,
}

impl Settings {
    fn of(breakpoint: &Breakpoint) -> Self {
        Self {
            enabled: breakpoint.enabled,
            condition: breakpoint.condition.clone(),
            hit_condition: breakpoint.hit_condition,
            log_message: breakpoint.log_message.clone(),
        }
    }
}

/// Where the group is in a group stop.
enum Phase {
    Running,

    /// Members are being paused for `first`'s stop, or for a pause of the
    /// whole group when it is none.
    Stopping {
        first: Option<(MemberNumber, StopReason)>,
        waiting: BTreeSet<MemberNumber>,
    },
    Stopped,
}

enum Internal {
    Event(MemberNumber, DebuggerEvent),
    PauseFailed(MemberNumber),
}

/// The task that runs a group: it adopts members, keeps their breakpoints
/// the root's, and stops them together.
struct Driver {
    shared: Arc<Shared>,
    root: DebuggerHandle,
    events: broadcast::Sender<GroupEvent>,
    members: BTreeMap<MemberNumber, Live>,
    next_number: MemberNumber,
    phase: Phase,
    stops: StopScope,

    /// Stops members made while the group stopped for another's, each
    /// reported at a later continue instead of resuming.
    pending: VecDeque<(MemberNumber, StopReason)>,
    internal: (
        mpsc::UnboundedSender<Internal>,
        mpsc::UnboundedReceiver<Internal>,
    ),
}

impl Driver {
    fn new(
        shared: Arc<Shared>,
        events: broadcast::Sender<GroupEvent>,
        root: Debugger,
        stops: StopScope,
    ) -> Self {
        let handle = root.handle();
        let mut driver = Self {
            shared,
            root: handle.clone(),
            events,
            members: BTreeMap::new(),
            next_number: 2,
            phase: Phase::Running,
            stops,
            pending: VecDeque::new(),
            internal: mpsc::unbounded_channel(),
        };
        driver.forward(1, &handle);
        driver.members.insert(
            1,
            Live {
                debugger: root,
                handle,
                process: None,
                joined_at: None,
                mirrored: BTreeMap::new(),
                running: false,
                gone: false,
            },
        );
        driver
    }

    async fn run(
        mut self,
        mut notices: mpsc::UnboundedReceiver<GroupNotice>,
        mut commands: mpsc::UnboundedReceiver<Command>,
    ) {
        loop {
            // A member's events are served before a client's command, so
            // that a command acts on the group as the events left it.
            tokio::select! {
                biased;
                Some(internal) = self.internal.1.recv() => match internal {
                    Internal::Event(member, event) => self.event(member, event).await,
                    Internal::PauseFailed(member) => self.paused(member, None),
                },
                Some(notice) = notices.recv() => self.notice(notice).await,
                command = commands.recv() => match command {
                    Some(Command::ContinueAll(reply)) => {
                        let _ = reply.send(self.continue_all().await);
                    }
                    Some(Command::PauseAll(reply)) => {
                        self.pause_all(None);
                        let _ = reply.send(Ok(()));
                    }
                    Some(Command::Shutdown(reply)) => {
                        let _ = reply.send(self.shutdown().await);
                        return;
                    }
                    None => {
                        let _ = self.shutdown().await;
                        return;
                    }
                },
            }
        }
    }

    /// Forwards a member's events to the driver.
    fn forward(&self, member: MemberNumber, handle: &DebuggerHandle) {
        let mut events = handle.subscribe();
        let internal = self.internal.0.clone();
        tokio::spawn(async move {
            loop {
                match events.recv().await {
                    Ok(event) => {
                        if internal.send(Internal::Event(member, event)).is_err() {
                            return;
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => {}
                    Err(broadcast::error::RecvError::Closed) => return,
                }
            }
        });
    }

    fn emit(&self, event: GroupEvent) {
        let _ = self.events.send(event);
    }

    async fn notice(&mut self, notice: GroupNotice) {
        match notice {
            GroupNotice::Joined(joined) => self.join(joined).await,
            GroupNotice::Watched {
                process,
                parent,
                program,
            } => {
                // The member's own report that its process left it may
                // have come first.
                let left = self
                    .shared
                    .members
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .iter()
                    .rev()
                    .find(|member| {
                        member.process == Some(process)
                            && !matches!(
                                member.state,
                                MemberState::Exited(_) | MemberState::Watched(_)
                            )
                    })
                    .map(|member| member.number);
                if let Some(number) = left
                    && let Some(live) = self.members.get_mut(&number)
                {
                    live.gone = true;
                    live.running = false;
                    self.shared.update(number, |member| {
                        member.state = MemberState::Watched(program.clone());
                    });
                    self.emit(GroupEvent::MemberWatched {
                        member: number,
                        program,
                    });
                    self.paused(number, None);
                    self.check_exited();
                } else {
                    self.emit(GroupEvent::ProcessWatched {
                        process,
                        parent,
                        program,
                    });
                }
            }
            GroupNotice::Unfollowed {
                process,
                program,
                reason,
            } => self.emit(GroupEvent::ProcessUnfollowed {
                process,
                program,
                reason,
            }),
        }
    }

    /// Adopts a new member: plants the root's breakpoints while it is in
    /// its first stop, then resumes it unless the group is stopped.
    async fn join(&mut self, joined: Joined) {
        let process = joined.process;
        let parent = joined.parent.and_then(|parent| self.number_of(parent));
        let executed = joined.executed.clone();
        let started = Debugger::joined(joined);
        let Ok((debugger, started)) = started else {
            return;
        };

        let handle = debugger.handle();
        let number = self.next_number;
        self.next_number += 1;
        self.forward(number, &handle);

        let ready = started.wait().await;
        let mut live = Live {
            debugger,
            handle: handle.clone(),
            process: Some(process),
            joined_at: None,
            mirrored: BTreeMap::new(),
            running: false,
            gone: false,
        };
        match ready {
            Ok(()) => {
                live.joined_at = handle
                    .snapshot()
                    .await
                    .ok()
                    .and_then(|snapshot| snapshot.stop_id);
                if let Ok(snapshot) = self.root.snapshot().await {
                    mirror(&snapshot.breakpoints, &mut live).await;
                }
            }
            #[cfg_attr(
                not(debug_assertions),
                expect(unused_variables, reason = "only recorded")
            )]
            Err(error) => {
                record!("member {number} did not start: {error}");
                live.gone = true;
            }
        }

        let command = command_line(process);
        self.shared
            .members
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(Member {
                number,
                process: Some(process),
                parent,
                executed: executed.clone(),
                command,
                state: if live.gone {
                    MemberState::Detached
                } else {
                    MemberState::Running
                },
            });
        self.shared
            .handles
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(number, handle.clone());

        let resume = !live.gone && matches!(self.phase, Phase::Running);
        if resume {
            live.running = continue_stopped(&handle).await.is_ok();
        } else if let Ok(snapshot) = handle.snapshot().await
            && let InferiorState::Stopped { reason, .. } = snapshot.inferior
        {
            self.shared
                .update(number, |member| member.state = MemberState::Stopped(reason));
        }
        self.members.insert(number, live);

        self.emit(GroupEvent::MemberJoined {
            member: number,
            process,
            parent,
            executed,
        });
    }

    fn number_of(&self, process: ProcessId) -> Option<MemberNumber> {
        self.shared
            .members
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .rev()
            .find(|member| member.process == Some(process))
            .map(|member| member.number)
    }

    async fn event(&mut self, number: MemberNumber, event: DebuggerEvent) {
        match event {
            DebuggerEvent::InferiorLaunched { process_id, .. } => {
                self.started(number, process_id);
            }
            DebuggerEvent::InferiorAttached { process_id, .. } => {
                self.started(number, process_id);
                self.set_running(number, false);
            }
            DebuggerEvent::InferiorContinued { .. } => {
                self.set_running(number, true);
                self.shared
                    .update(number, |member| member.state = MemberState::Running);
            }
            DebuggerEvent::InferiorStopped {
                reason, stop_id, ..
            } => {
                if self
                    .members
                    .get(&number)
                    .is_some_and(|live| live.joined_at == Some(stop_id))
                {
                    return;
                }

                self.set_running(number, false);
                let state = MemberState::Stopped(reason.clone());
                self.shared.update(number, |member| member.state = state);
                self.stopped(number, reason);
            }
            DebuggerEvent::InferiorExited { status, .. } => {
                if let Some(live) = self.members.get_mut(&number) {
                    live.running = false;
                    live.gone = true;
                }
                let state = MemberState::Exited(status.clone());
                self.shared.update(number, |member| member.state = state);
                self.emit(GroupEvent::MemberExited {
                    member: number,
                    status,
                });
                self.paused(number, None);
                self.check_exited();
            }
            DebuggerEvent::InferiorDetached { .. } => {
                if let Some(live) = self.members.get_mut(&number) {
                    live.running = false;
                    live.gone = true;
                }
                self.shared.update(number, |member| {
                    if !matches!(member.state, MemberState::Watched(_)) {
                        member.state = MemberState::Detached;
                    }
                });
                self.paused(number, None);
                self.check_exited();
            }
            DebuggerEvent::BreakpointsChanged { .. } if number == 1 => self.mirror_all().await,
            _ => {}
        }
    }

    fn started(&mut self, number: MemberNumber, process: ProcessId) {
        if let Some(live) = self.members.get_mut(&number) {
            live.gone = false;
            live.running = true;
            live.process = Some(process);
        }
        self.shared.update(number, |member| {
            member.process = Some(process);
            member.state = MemberState::Running;
        });
    }

    fn set_running(&mut self, number: MemberNumber, running: bool) {
        if let Some(live) = self.members.get_mut(&number) {
            live.running = running;
        }
    }

    /// Applies the group's stop rule to a stop member `number` published.
    fn stopped(&mut self, number: MemberNumber, reason: StopReason) {
        if self.stops == StopScope::Process {
            self.emit(GroupEvent::GroupStopped {
                member: number,
                reason,
            });
            return;
        }

        match &mut self.phase {
            Phase::Running => {
                self.pause_all(Some((number, reason)));
            }
            Phase::Stopping { .. } => self.paused(number, Some(reason)),
            // A member a client resumed alone, as a step resumes it, stops
            // again while the rest of the group stays stopped.
            Phase::Stopped => self.emit(GroupEvent::GroupStopped {
                member: number,
                reason,
            }),
        }
    }

    /// Pauses every running member but the one whose stop `first` is.
    fn pause_all(&mut self, first: Option<(MemberNumber, StopReason)>) {
        let stopped = first.as_ref().map(|(number, _)| *number);
        let mut waiting = BTreeSet::new();
        for (&number, live) in &self.members {
            if Some(number) == stopped || live.gone || !live.running {
                continue;
            }

            waiting.insert(number);
            let handle = live.handle.clone();
            let internal = self.internal.0.clone();
            tokio::spawn(async move {
                if handle.pause().await.is_err() {
                    let _ = internal.send(Internal::PauseFailed(number));
                }
            });
        }

        self.phase = Phase::Stopping { first, waiting };
        self.paused(0, None);
    }

    /// Notes that member `number` stopped, or will not, while the group
    /// stops, and reports the group stop once none is left running.
    fn paused(&mut self, number: MemberNumber, reason: Option<StopReason>) {
        let Phase::Stopping { first, waiting } = &mut self.phase else {
            return;
        };

        if waiting.remove(&number)
            && let Some(reason) = reason
            && reason != StopReason::Pause
        {
            if first.is_none() {
                *first = Some((number, reason));
            } else {
                self.pending.push_back((number, reason));
            }
        }

        if !waiting.is_empty() {
            return;
        }

        let first = first.take();
        self.phase = Phase::Stopped;
        let (member, reason) = first.unwrap_or((1, StopReason::Pause));
        self.emit(GroupEvent::GroupStopped { member, reason });
    }

    async fn continue_all(&mut self) -> Result<()> {
        if let Some((member, reason)) = self.pending.pop_front() {
            self.phase = Phase::Stopped;
            self.emit(GroupEvent::GroupStopped { member, reason });
            return Ok(());
        }

        self.phase = Phase::Running;
        let mut result = Err(Error::NotStopped);
        for live in self.members.values_mut() {
            if live.gone || live.running {
                continue;
            }

            match continue_stopped(&live.handle).await {
                Ok(()) => {
                    live.running = true;
                    result = Ok(());
                }
                Err(Error::NotRunning | Error::NotStopped) => {}
                Err(error) => result = Err(error),
            }
        }

        result
    }

    /// Makes every member's breakpoints the root's again.
    async fn mirror_all(&mut self) {
        let Ok(snapshot) = self.root.snapshot().await else {
            return;
        };

        for (_, live) in self.members.iter_mut().filter(|(number, _)| **number != 1) {
            if !live.gone {
                mirror(&snapshot.breakpoints, live).await;
            }
        }
    }

    fn check_exited(&self) {
        if self.members.values().all(|live| live.gone) {
            self.emit(GroupEvent::GroupExited);
        }
    }

    async fn shutdown(&mut self) -> Result<()> {
        let mut result = Ok(());
        // Members end before the root, whose process started theirs.
        let numbers = self.members.keys().rev().copied().collect::<Vec<_>>();
        for number in numbers {
            if let Some(live) = self.members.remove(&number)
                && let Err(error) = live.debugger.shutdown().await
                && number == 1
            {
                result = Err(error);
            }
        }
        self.shared
            .handles
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clear();

        result
    }
}

/// Continues a member stopped at its current stop.
async fn continue_stopped(handle: &DebuggerHandle) -> Result<()> {
    let snapshot = handle.snapshot().await?;
    let InferiorState::Stopped {
        process_id,
        stop_id,
        ..
    } = snapshot.inferior
    else {
        return Err(Error::NotStopped);
    };

    handle
        .continue_execution(
            stop_id,
            ResumeScope::Process(process_id),
            ExceptionDisposition::Pass,
        )
        .await
        .map(drop)
}

/// Makes a member's breakpoints the root's `breakpoints`: each one the
/// member lacks is added, kept pending where nothing it has loaded has it,
/// each changed one is changed, and each the root removed is removed.
async fn mirror(breakpoints: &[Breakpoint], live: &mut Live) {
    let handle = live.handle.clone();
    let current = breakpoints
        .iter()
        .map(|breakpoint| breakpoint.id)
        .collect::<BTreeSet<_>>();

    let removed = live
        .mirrored
        .keys()
        .filter(|id| !current.contains(id))
        .copied()
        .collect::<Vec<_>>();
    for id in removed {
        if let Some((copy, _)) = live.mirrored.remove(&id) {
            let _ = handle.remove_breakpoint(copy).await;
        }
    }

    for breakpoint in breakpoints {
        let settings = Settings::of(breakpoint);
        match live.mirrored.get(&breakpoint.id) {
            None => {
                let options = BreakpointOptions {
                    hit_condition: settings.hit_condition,
                    condition: settings.condition.clone(),
                    log_message: settings.log_message.clone(),
                    pending: true,
                    enabled: settings.enabled,
                    temporary: breakpoint.temporary,
                };
                if let Ok(copy) = handle
                    .add_breakpoint_with(breakpoint.spec.clone(), options)
                    .await
                {
                    live.mirrored.insert(breakpoint.id, (copy.id, settings));
                }
            }
            Some((copy, old)) if *old != settings => {
                let copy = *copy;
                let _ = handle.set_breakpoint_enabled(copy, settings.enabled).await;
                let _ = handle
                    .set_breakpoint_condition(copy, settings.condition.clone())
                    .await;
                let _ = handle
                    .set_breakpoint_hit_condition(copy, settings.hit_condition)
                    .await;
                let _ = handle
                    .set_breakpoint_log_message(copy, settings.log_message.clone())
                    .await;
                live.mirrored.insert(breakpoint.id, (copy, settings));
            }
            Some(_) => {}
        }
    }
}

/// A process's command line, its arguments joined by spaces.
fn command_line(process: ProcessId) -> String {
    std::fs::read(format!("/proc/{}/cmdline", process.get()))
        .map(|line| {
            line.split(|byte| *byte == 0)
                .filter(|argument| !argument.is_empty())
                .map(String::from_utf8_lossy)
                .collect::<Vec<_>>()
                .join(" ")
        })
        .unwrap_or_default()
}
