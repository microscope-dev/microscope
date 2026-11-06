//! A process group: the processes a program starts that microscope debugs
//! together.
//!
//! Each process has one tracer thread. The root launches or attaches as a
//! single session does. Children accepted by the follow policy become
//! members with their own [`DebuggerHandle`]; the rest are watched until
//! they execute a program the policy accepts. Members are numbered in join
//! order from 1 (the root), and keep their numbers after exiting.
//!
//! Each member gets the root's breakpoints before running an instruction,
//! and subsequent changes reach every member. Unless [`StopScope`] says
//! otherwise, a stop in any member pauses all other members before
//! [`GroupEvent::GroupStopped`] is published.

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
    /// before it ran an instruction, except any that a
    /// [`GroupEvent::BreakpointNotMirrored`] after this one names.
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

    /// Member `member` failed to take a change to the group's breakpoint
    /// `breakpoint`, so its copy may differ from the root's or be missing.
    /// Each later change to the root's breakpoints tries again.
    BreakpointNotMirrored {
        member: MemberNumber,
        breakpoint: BreakpointId,
        reason: String,
    },

    /// A member stopped, and every other member is stopped too.
    GroupStopped {
        member: MemberNumber,
        reason: StopReason,
    },

    /// A member's process was released to run on its own.
    MemberDetached { member: MemberNumber },

    /// A member's session ended while its process was still debugged, as a
    /// failure in microscope ends one. Its process was killed if the session
    /// launched it and would have killed it on shutdown, and released to run
    /// on its own otherwise.
    MemberLost { member: MemberNumber },

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

    /// Its session failed, and released or killed its process.
    Lost,
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
///
/// Its [`GroupHandle`], which it dereferences to, is what clients share.
pub struct ProcessGroup {
    handle: GroupHandle,
    task: Option<tokio::task::JoinHandle<()>>,
    backend: Option<backend::GroupBackend>,
}

/// A clonable client of a running [`ProcessGroup`].
#[derive(Clone)]
pub struct GroupHandle {
    shared: Arc<Shared>,
    commands: mpsc::UnboundedSender<Command>,
    events: broadcast::Sender<GroupEvent>,
    root: DebuggerHandle,

    /// What a stop in one member stops.
    stops: StopScope,
}

struct Shared {
    members: Mutex<Vec<Member>>,
    handles: Mutex<BTreeMap<MemberNumber, DebuggerHandle>>,

    /// The root's breakpoint each member's copy of one is, by the member
    /// and the copy's id.
    copies: Mutex<BTreeMap<(MemberNumber, BreakpointId), BreakpointId>>,
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
    ResumeAll(
        Option<(MemberNumber, crate::StopId)>,
        oneshot::Sender<Result<()>>,
    ),
    PauseAll(oneshot::Sender<Result<()>>),
    Detach(MemberNumber, oneshot::Sender<Result<()>>),
    Shutdown(oneshot::Sender<Result<()>>),
}

impl ProcessGroup {
    /// Makes a group whose root is a session for `program`, which a client
    /// launches or attaches through [`GroupHandle::root`]. The processes it
    /// starts are followed as `policy` says.
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
            copies: Mutex::default(),
        });
        let (commands, received) = mpsc::unbounded_channel();
        let (events, _) = broadcast::channel(GROUP_EVENT_CAPACITY);
        let task =
            Driver::new(Arc::clone(&shared), events.clone(), root, stops).run(notices, received);

        Ok(Self {
            handle: GroupHandle {
                shared,
                commands,
                events,
                root: handle,
                stops,
            },
            task: Some(tokio::spawn(task)),
            backend: Some(backend),
        })
    }

    /// Attaches a group's root to `process`, following the processes it
    /// starts from now on as `policy` says, and returns once the root is
    /// stopped. Its program is read from `executable` when given, and
    /// otherwise from the process. Processes it started before are not
    /// adopted. Ending the group releases every process it debugs or
    /// watches.
    pub async fn attach(
        process: ProcessId,
        executable: Option<&std::path::Path>,
        policy: FollowPolicy,
        debug_files: &crate::DebugFileOptions,
    ) -> Result<Self> {
        let program = Program::of_process(process, executable, debug_files).await?;
        let group = Self::new(program, policy)?;
        let mut events = group.subscribe();
        let attached = group
            .root
            .request(|reply| crate::protocol::Request::Attach {
                process_id: process,
                observe: false,
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

    /// A clonable handle for the group's clients.
    #[must_use]
    pub fn handle(&self) -> GroupHandle {
        self.handle.clone()
    }

    /// Ends every member, killing launched processes and releasing attached
    /// ones, then the processes the group watches, and joins the tracer
    /// thread.
    pub async fn shutdown(mut self) -> Result<()> {
        let result = self.handle.command(Command::Shutdown).await;
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

impl std::ops::Deref for ProcessGroup {
    type Target = GroupHandle;

    fn deref(&self) -> &GroupHandle {
        &self.handle
    }
}

/// A group dropped without [`ProcessGroup::shutdown`] ends the same way,
/// on its driver's task, without waiting for it, as a dropped [`Debugger`]
/// does. Aborting the driver would stop it partway through whatever it was
/// doing to its members.
impl Drop for ProcessGroup {
    fn drop(&mut self) {
        if self.task.take().is_some() {
            let (reply, _) = oneshot::channel();
            let _ = self.handle.commands.send(Command::Shutdown(reply));
        }
    }
}

impl GroupHandle {
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

    /// What a stop in one member stops: the group, or that member alone.
    #[must_use]
    pub const fn stops(&self) -> StopScope {
        self.stops
    }

    /// Resumes every stopped member, including those whose stops
    /// [`continue_all`](Self::continue_all) would report first, for a client
    /// that already shows each member's stop on its own. `observed` is the
    /// stop the client showed, from the member's own events, which the
    /// group may not have seen yet; it is resumed all the same.
    pub async fn resume_all(&self, observed: Option<(MemberNumber, crate::StopId)>) -> Result<()> {
        self.command(|reply| Command::ResumeAll(observed, reply))
            .await
    }

    /// Pauses every running member, and reports the group stop once they
    /// are stopped.
    pub async fn pause_all(&self) -> Result<()> {
        self.command(Command::PauseAll).await
    }

    /// The group's breakpoint, which is the root's, that member `number`'s
    /// breakpoint `id` is a copy of; the root's own are themselves.
    #[must_use]
    pub fn group_breakpoint(&self, number: MemberNumber, id: BreakpointId) -> Option<BreakpointId> {
        if number == 1 {
            return Some(id);
        }

        self.shared
            .copies
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&(number, id))
            .copied()
    }

    /// Releases member `number`'s process, stopped, to run on its own; the
    /// rest of the group is unchanged. The root cannot be detached alone.
    pub async fn detach(&self, number: MemberNumber) -> Result<()> {
        self.command(|reply| Command::Detach(number, reply)).await
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
}

const GROUP_EVENT_CAPACITY: usize = 256;

/// A member as the group's driver keeps it.
struct Live {
    debugger: Debugger,
    handle: DebuggerHandle,
    process: Option<ProcessId>,

    /// The stop the member joined in, which is not one of its own.
    joined_at: Option<crate::StopId>,

    /// A stop a client resumed before the member's event for it reached
    /// the group, which the event must not report again.
    passed: Option<crate::StopId>,

    /// The revision of the member's state a resync after a lag took in
    /// whole; its events up to that revision came before it, and applying
    /// them now would take the member back.
    resynced: u64,

    /// The last stop the group handled, which a resync may find again.
    stopped_at: Option<crate::StopId>,

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
    temporary: bool,
    condition: Option<crate::Condition>,
    hit_condition: Option<crate::HitCondition>,
    log_message: Option<crate::LogMessage>,
}

impl Settings {
    fn of(breakpoint: &Breakpoint) -> Self {
        Self {
            enabled: breakpoint.enabled,
            temporary: breakpoint.temporary,
            condition: breakpoint.condition.clone(),
            hit_condition: breakpoint.hit_condition,
            log_message: breakpoint.log_message.clone(),
        }
    }
}

/// Where the group is in a group stop.
#[derive(Debug)]
enum Phase {
    Running,

    /// Members are being paused for `first`'s stop, or for a pause of the
    /// whole group when it is none. `paused` is the first member that
    /// stopped for it, and `was_stopped` whether the group was stopped
    /// before: a stop nobody stopped for leaves the group as it was.
    Stopping {
        first: Option<(MemberNumber, StopReason)>,
        waiting: BTreeSet<MemberNumber>,
        paused: Option<MemberNumber>,
        was_stopped: bool,
    },
    Stopped,
}

/// The group's stop rule, apart from the members it pauses and resumes:
/// where the group is in a group stop, the stops it has yet to report,
/// and the client resumes `C` that wait for it to finish stopping. Each
/// method says what the driver must do.
#[derive(Debug)]
struct Stops<C> {
    scope: StopScope,
    phase: Phase,

    /// Stops members made while the group stopped for another's, each
    /// reported at a later continue instead of resuming.
    pending: VecDeque<(MemberNumber, StopReason)>,

    /// Continues and resumes a client asked for while the group stopped,
    /// served once it has: the pauses under way are part of that stop.
    deferred: Vec<C>,
}

/// What a member's stop asks of the driver.
#[derive(Debug, PartialEq, Eq)]
enum StopAction {
    /// Report a group stop.
    Report(MemberNumber, StopReason),

    /// Pause every other running member for this stop.
    PauseOthers(MemberNumber, StopReason),
}

impl<C> Stops<C> {
    const fn new(scope: StopScope) -> Self {
        Self {
            scope,
            phase: Phase::Running,
            pending: VecDeque::new(),
            deferred: Vec::new(),
        }
    }

    const fn is_running(&self) -> bool {
        matches!(self.phase, Phase::Running)
    }

    const fn is_stopping(&self) -> bool {
        matches!(self.phase, Phase::Stopping { .. })
    }

    /// Applies the group's stop rule to a stop member `number` published.
    fn stopped(&mut self, number: MemberNumber, reason: StopReason) -> Option<StopAction> {
        // Each member's stop is its own, and one of those a pause of the
        // whole group waits for.
        if self.scope == StopScope::Process {
            let _ = self.paused(number, Some(reason.clone()));
            return Some(StopAction::Report(number, reason));
        }

        match &mut self.phase {
            Phase::Running => Some(StopAction::PauseOthers(number, reason)),
            Phase::Stopping { .. } => self
                .paused(number, Some(reason))
                .map(|(member, reason)| StopAction::Report(member, reason)),
            // A member a client resumed alone, as a step resumes it, stops
            // again while the rest of the group stays stopped.
            Phase::Stopped => Some(StopAction::Report(number, reason)),
        }
    }

    /// Begins a group stop for `first`'s stop, or for a pause of the whole
    /// group, waiting for the members being paused; returns the group
    /// stop to report if none is.
    fn pause_all(
        &mut self,
        first: Option<(MemberNumber, StopReason)>,
        waiting: BTreeSet<MemberNumber>,
    ) -> Option<(MemberNumber, StopReason)> {
        // A client's pause while the group stops joins the stop under way,
        // which keeps the stop it is for.
        if let Phase::Stopping {
            waiting: awaited, ..
        } = &mut self.phase
        {
            debug_assert!(
                first.is_none(),
                "member {first:?}'s stop began a group stop inside another"
            );
            awaited.extend(waiting);
            return self.paused(0, None);
        }

        self.phase = Phase::Stopping {
            paused: first.as_ref().map(|(number, _)| *number),
            first,
            waiting,
            was_stopped: matches!(self.phase, Phase::Stopped),
        };
        self.paused(0, None)
    }

    /// Notes that a member was continued through its own session. Where
    /// each member's stop is its own, the group is then no longer stopped
    /// as a whole, so members that join are resumed.
    fn continued(&mut self) {
        if self.scope == StopScope::Process && matches!(self.phase, Phase::Stopped) {
            self.phase = Phase::Running;
        }
    }

    /// Notes that member `number` stopped for `reason`, or will not stop
    /// when it is none, while the group stops, and returns the group stop
    /// to report once none is left running.
    fn paused(
        &mut self,
        number: MemberNumber,
        reason: Option<StopReason>,
    ) -> Option<(MemberNumber, StopReason)> {
        let Phase::Stopping {
            first,
            waiting,
            paused,
            was_stopped,
        } = &mut self.phase
        else {
            return None;
        };

        waiting.remove(&number);
        if let Some(reason) = reason {
            paused.get_or_insert(number);

            // Where each member's stop is reported as its own, none is
            // the group's.
            if self.scope == StopScope::Group && reason != StopReason::Pause {
                if first.is_none() {
                    *first = Some((number, reason));
                } else {
                    self.pending.push_back((number, reason));
                }
            }
        }

        if !waiting.is_empty() {
            return None;
        }

        let stop = first
            .take()
            .or_else(|| paused.map(|number| (number, StopReason::Pause)));
        self.phase = if stop.is_some() || *was_stopped {
            Phase::Stopped
        } else {
            Phase::Running
        };

        if self.scope == StopScope::Process {
            return None;
        }

        stop
    }

    /// Keeps a client's continue or resume while members are still
    /// pausing, or gives it back to be served now.
    fn defer(&mut self, command: C) -> Option<C> {
        if self.is_stopping() {
            self.deferred.push(command);
            return None;
        }

        Some(command)
    }

    /// The continues and resumes that waited for the group to finish
    /// stopping, once it has.
    fn take_deferred(&mut self) -> Vec<C> {
        if self.is_stopping() {
            return Vec::new();
        }

        std::mem::take(&mut self.deferred)
    }

    /// Forgets the stops yet to report, for a client that showed them.
    fn discard_pending(&mut self) {
        self.pending.clear();
    }

    /// Begins a continue of the group: returns a stop to report instead,
    /// or sets the group running, for the driver to resume its members.
    fn continue_all(&mut self) -> Option<(MemberNumber, StopReason)> {
        if let Some(stop) = self.pending.pop_front() {
            self.phase = Phase::Stopped;
            return Some(stop);
        }

        self.phase = Phase::Running;
        None
    }
}

enum Internal {
    Event(MemberNumber, DebuggerEvent),
    PauseFailed(MemberNumber),

    /// The member's events came faster than the group read them, and the
    /// oldest were lost.
    Lagged(MemberNumber),

    /// The member's session ended.
    Ended(MemberNumber),
}

/// The task that runs a group: it adopts members, keeps their breakpoints
/// the root's, and stops them together.
struct Driver {
    shared: Arc<Shared>,
    root: DebuggerHandle,
    events: broadcast::Sender<GroupEvent>,
    members: BTreeMap<MemberNumber, Live>,
    next_number: MemberNumber,
    stops: Stops<Command>,
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
            stops: Stops::new(stops),
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
                passed: None,
                resynced: 0,
                stopped_at: None,
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
                Some(internal) = self.internal.1.recv() => {
                    match internal {
                        Internal::Event(member, event) => self.event(member, event).await,
                        Internal::PauseFailed(member) => self.paused(member, None),
                        Internal::Lagged(member) => self.resync(member).await,
                        Internal::Ended(member) => self.ended(member),
                    }
                    self.after_internal().await;
                }
                Some(notice) = notices.recv() => self.notice(notice).await,
                command = commands.recv() => match command {
                    Some(command @ (Command::ContinueAll(_) | Command::ResumeAll(..))) => {
                        self.resume(command).await;
                    }
                    Some(Command::Detach(number, reply)) => {
                        let _ = reply.send(self.detach(number).await);
                    }
                    Some(Command::PauseAll(reply)) => {
                        if self.members.values().any(|live| live.running && !live.gone) {
                            self.pause_all(None);
                            let _ = reply.send(Ok(()));
                        } else {
                            // With nothing running yet, the root pauses as
                            // it starts, or says it is not running.
                            let root = self.root.clone();
                            tokio::spawn(async move {
                                let paused = match root.pause().await {
                                    Ok(_) | Err(Error::AlreadyStopped) => Ok(()),
                                    Err(error) => Err(error),
                                };
                                let _ = reply.send(paused);
                            });
                        }
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
        let handle = handle.clone();
        let internal = self.internal.0.clone();
        tokio::spawn(async move {
            loop {
                // Events go first, so that the session's end follows every
                // event it sent.
                let event = tokio::select! {
                    biased;
                    event = events.recv() => event,
                    () = handle.ended() => {
                        while let Ok(event) = events.try_recv() {
                            let _ = internal.send(Internal::Event(member, event));
                        }
                        let _ = internal.send(Internal::Ended(member));
                        return;
                    }
                    () = internal.closed() => return,
                };
                match event {
                    Ok(event) => {
                        if internal.send(Internal::Event(member, event)).is_err() {
                            return;
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => {
                        if internal.send(Internal::Lagged(member)).is_err() {
                            return;
                        }
                    }
                    Err(broadcast::error::RecvError::Closed) => return,
                }
            }
        });
    }

    /// Catches up with member `number` after its oldest events were lost.
    /// A lag loses the oldest events, never the newest, so the stop or
    /// exit that ends a burst still comes; what is lost is how the member
    /// got there: its process, that it ran, and breakpoint changes.
    async fn resync(&mut self, number: MemberNumber) {
        let Some(handle) = self.members.get(&number).map(|live| live.handle.clone()) else {
            return;
        };
        let Ok(snapshot) = handle.snapshot().await else {
            return;
        };

        // A member not running is left to its newest events, which a lag
        // keeps, as its exit or detach is.
        let applies = !matches!(snapshot.inferior, InferiorState::NotRunning);
        if let Some(live) = self.members.get_mut(&number)
            && applies
        {
            live.resynced = snapshot.revision;
        }

        match snapshot.inferior {
            InferiorState::NotRunning => {}
            InferiorState::Running { process_id, .. } => {
                self.adopt_process(number, process_id);
                self.set_running(number, true);
                self.stops.continued();
                self.shared
                    .update(number, |member| member.state = MemberState::Running);
            }
            InferiorState::Stopped {
                process_id,
                stop_id,
                reason,
                ..
            } => {
                self.adopt_process(number, process_id);
                self.stop_event(number, stop_id, reason).await;
            }
        }

        if number == 1 {
            self.mirror_all().await;
        }
    }

    /// Notes member `number`'s process, whose start the group may have
    /// missed, leaving whether it runs to the caller.
    fn adopt_process(&mut self, number: MemberNumber, process: ProcessId) {
        let Some(live) = self.members.get_mut(&number) else {
            return;
        };
        if live.process == Some(process) {
            return;
        }
        live.process = Some(process);
        live.gone = false;
        let command = command_line(process);
        self.shared.update(number, |member| {
            member.process = Some(process);
            if !command.is_empty() {
                member.command = command;
            }
        });
    }

    /// Notes that member `number`'s session ended. One that still debugged
    /// its process failed, and released or killed it.
    fn ended(&mut self, number: MemberNumber) {
        let Some(live) = self.members.get_mut(&number) else {
            return;
        };
        if live.gone {
            return;
        }
        live.running = false;
        live.gone = true;
        self.shared
            .update(number, |member| member.state = MemberState::Lost);
        self.emit(GroupEvent::MemberLost { member: number });
        self.paused(number, None);
        self.check_exited();
    }

    fn emit(&self, event: GroupEvent) {
        record!("group event {event:?}");
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
            passed: None,
            resynced: 0,
            stopped_at: None,
            mirrored: BTreeMap::new(),
            running: false,
            gone: false,
        };
        let mut failures = Vec::new();
        match ready {
            Ok(()) => {
                live.joined_at = handle
                    .snapshot()
                    .await
                    .ok()
                    .and_then(|snapshot| snapshot.stop_id);
                if let Ok(snapshot) = self.root.snapshot().await {
                    failures = mirror(&self.shared, number, &snapshot.breakpoints, &mut live).await;
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
        let mut member = Member {
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
        };
        let resume = !live.gone && self.stops.is_running();
        if resume {
            live.running = continue_stopped(&handle).await.is_ok();
        } else if let Ok(snapshot) = handle.snapshot().await
            && let InferiorState::Stopped { reason, .. } = snapshot.inferior
        {
            member.state = MemberState::Stopped(reason);
        }

        // A stopped group must not expose a joining member as running
        // while its snapshot is still on the way.
        self.shared
            .members
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(member);
        self.shared
            .handles
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(number, handle.clone());
        self.members.insert(number, live);

        self.emit(GroupEvent::MemberJoined {
            member: number,
            process,
            parent,
            executed,
        });
        for failure in failures {
            self.emit(failure);
        }
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
        if self
            .members
            .get(&number)
            .is_some_and(|live| event.revision() <= live.resynced)
        {
            return;
        }

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
                self.stops.continued();
                self.shared
                    .update(number, |member| member.state = MemberState::Running);
            }
            DebuggerEvent::InferiorStopped {
                reason, stop_id, ..
            } => self.stop_event(number, stop_id, reason).await,
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

    /// Handles member `number`'s stop `stop_id`, from its event or from a
    /// resync after a lag.
    async fn stop_event(
        &mut self,
        number: MemberNumber,
        stop_id: crate::StopId,
        reason: StopReason,
    ) {
        let Some(live) = self.members.get_mut(&number) else {
            return;
        };
        if [live.joined_at, live.passed, live.stopped_at].contains(&Some(stop_id)) {
            return;
        }
        live.stopped_at = Some(stop_id);

        self.set_running(number, false);
        let state = MemberState::Stopped(reason.clone());
        self.shared.update(number, |member| member.state = state);
        self.end_temporaries(number, &reason).await;
        self.stopped(number, reason);
    }

    fn started(&mut self, number: MemberNumber, process: ProcessId) {
        if let Some(live) = self.members.get_mut(&number) {
            live.gone = false;
            live.running = true;
            live.process = Some(process);
        }
        let command = command_line(process);
        self.shared.update(number, |member| {
            member.process = Some(process);
            member.state = MemberState::Running;
            if !command.is_empty() {
                member.command = command;
            }
        });
    }

    fn set_running(&mut self, number: MemberNumber, running: bool) {
        if let Some(live) = self.members.get_mut(&number) {
            live.running = running;
        }
    }

    /// Applies the group's stop rule to a stop member `number` published.
    fn stopped(&mut self, number: MemberNumber, reason: StopReason) {
        match self.stops.stopped(number, reason) {
            Some(StopAction::Report(member, reason)) => {
                self.emit(GroupEvent::GroupStopped { member, reason });
            }
            Some(StopAction::PauseOthers(number, reason)) => {
                self.pause_all(Some((number, reason)));
            }
            None => {}
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
                // AlreadyStopped means its stop event is still on the way.
                if let Err(error) = handle.pause().await
                    && !matches!(error, Error::AlreadyStopped)
                {
                    record!("member {number} pause failed: {error}");
                    let _ = internal.send(Internal::PauseFailed(number));
                }
            });
        }

        if let Some((member, reason)) = self.stops.pause_all(first, waiting) {
            self.emit(GroupEvent::GroupStopped { member, reason });
        }
    }

    /// Notes that member `number` stopped, or will not, while the group
    /// stops, and reports the group stop once none is left running.
    fn paused(&mut self, number: MemberNumber, reason: Option<StopReason>) {
        if let Some((member, reason)) = self.stops.paused(number, reason) {
            self.emit(GroupEvent::GroupStopped { member, reason });
        }
    }

    /// Resumes member `number` at `stop`, which a client showed from the
    /// member's own events before they reached the group, and returns
    /// whether it did. A member the group already saw stop is left to
    /// [`Self::continue_all`].
    async fn catch_up(&mut self, number: MemberNumber, stop: crate::StopId) -> bool {
        let Some(live) = self.members.get_mut(&number) else {
            return false;
        };
        let Some(process) = live.process.filter(|_| live.running && !live.gone) else {
            return false;
        };

        let resumed = live
            .handle
            .continue_execution(
                stop,
                ResumeScope::Process(process),
                ExceptionDisposition::Pass,
            )
            .await
            .is_ok();
        if resumed {
            live.passed = Some(stop);
        }
        resumed
    }

    /// Serves the continues and resumes that waited for the group to
    /// finish stopping, once it has.
    async fn after_internal(&mut self) {
        for command in self.stops.take_deferred() {
            self.resume(command).await;
        }
    }

    /// Serves a client's continue or resume of the group. One asked while
    /// members are still pausing waits for them, or a pause under way
    /// would land after the resume and stop the group again.
    async fn resume(&mut self, command: Command) {
        let Some(command) = self.stops.defer(command) else {
            return;
        };
        match command {
            Command::ContinueAll(reply) => {
                let _ = reply.send(self.continue_all().await);
            }
            Command::ResumeAll(observed, reply) => {
                self.stops.discard_pending();
                let caught_up = match observed {
                    Some((number, stop)) => self.catch_up(number, stop).await,
                    None => false,
                };
                let resumed = self.continue_all().await;
                let _ = reply.send(if caught_up {
                    resumed.or(Ok(()))
                } else {
                    resumed
                });
            }
            _ => {}
        }
    }

    async fn continue_all(&mut self) -> Result<()> {
        if let Some((member, reason)) = self.stops.continue_all() {
            self.emit(GroupEvent::GroupStopped { member, reason });
            return Ok(());
        }

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

    /// Deletes the group's temporary breakpoints a member's stop hit, as
    /// the member deleted its copies: the first stop a temporary breakpoint
    /// causes in any member deletes it.
    async fn end_temporaries(&mut self, number: MemberNumber, reason: &StopReason) {
        let StopReason::Breakpoint { hits, .. } = reason else {
            return;
        };
        if number == 1 {
            return;
        }

        let Some(live) = self.members.get_mut(&number) else {
            return;
        };
        let ended = live
            .mirrored
            .iter()
            .filter(|(_, (copy, settings))| {
                settings.temporary && hits.iter().any(|hit| hit.breakpoint == *copy)
            })
            .map(|(original, _)| *original)
            .collect::<Vec<_>>();
        for original in ended {
            live.mirrored.remove(&original);
            let _ = self.root.remove_breakpoint(original).await;
        }
    }

    /// Makes every member's breakpoints the root's again.
    async fn mirror_all(&mut self) {
        let Ok(snapshot) = self.root.snapshot().await else {
            return;
        };

        let mut failures = Vec::new();
        for (&number, live) in self.members.iter_mut().filter(|(number, _)| **number != 1) {
            if !live.gone {
                failures.extend(mirror(&self.shared, number, &snapshot.breakpoints, live).await);
            }
        }
        for failure in failures {
            self.emit(failure);
        }
    }

    fn check_exited(&self) {
        if self.members.values().all(|live| live.gone) {
            self.emit(GroupEvent::GroupExited);
        }
    }

    /// Releases member `number`, which must be stopped, and ends its
    /// session.
    async fn detach(&mut self, number: MemberNumber) -> Result<()> {
        if number == 1 {
            return Err(Error::RootDetached);
        }

        let Some(live) = self.members.get(&number) else {
            return Err(Error::UnknownMember(number));
        };
        if live.gone {
            return Err(Error::NotRunning);
        }
        if live.running {
            return Err(Error::NotStopped);
        }

        let Some(live) = self.members.remove(&number) else {
            return Err(Error::UnknownMember(number));
        };
        live.handle
            .request(|reply| crate::protocol::Request::ReleaseOnShutdown { reply })
            .await?;
        live.debugger.shutdown().await?;
        self.shared
            .update(number, |member| member.state = MemberState::Detached);
        self.shared
            .handles
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&number);
        self.emit(GroupEvent::MemberDetached { member: number });
        self.check_exited();
        Ok(())
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
/// Returns what the member failed to take, which stays due, so that the
/// next mirror tries it again.
async fn mirror(
    shared: &Shared,
    number: MemberNumber,
    breakpoints: &[Breakpoint],
    live: &mut Live,
) -> Vec<GroupEvent> {
    let mut failures = Vec::new();
    let mut failed = |breakpoint: BreakpointId, error: &Error| {
        let reason = match error {
            Error::BreakpointNotFound(_) => "the member no longer has its copy".to_owned(),
            error => error.to_string(),
        };
        failures.push(GroupEvent::BreakpointNotMirrored {
            member: number,
            breakpoint,
            reason,
        });
    };

    let mut copies = BTreeMap::new();
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
        let Some(&(copy, _)) = live.mirrored.get(&id) else {
            continue;
        };
        match handle.remove_breakpoint(copy).await {
            // A copy already gone, as a temporary one the member hit
            // deletes itself, is as good as removed.
            Ok(_) | Err(Error::BreakpointNotFound(_)) => {
                live.mirrored.remove(&id);
                copies.insert(copy, None);
            }
            Err(error) => failed(id, &error),
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
                    temporary: settings.temporary,
                };
                match handle
                    .add_breakpoint_with(breakpoint.spec.clone(), options)
                    .await
                {
                    Ok(copy) => {
                        live.mirrored.insert(breakpoint.id, (copy.id, settings));
                        copies.insert(copy.id, Some(breakpoint.id));
                    }
                    Err(error) => failed(breakpoint.id, &error),
                }
            }
            Some((copy, old)) if *old != settings => {
                let copy = *copy;

                // Each setting is tried even after one fails, so the copy
                // gets as close to the root's as it can.
                let results = [
                    handle.set_breakpoint_enabled(copy, settings.enabled).await,
                    handle
                        .set_breakpoint_condition(copy, settings.condition.clone())
                        .await,
                    handle
                        .set_breakpoint_hit_condition(copy, settings.hit_condition)
                        .await,
                    handle
                        .set_breakpoint_log_message(copy, settings.log_message.clone())
                        .await,
                ];
                match results.into_iter().find_map(Result::err) {
                    None => {
                        live.mirrored.insert(breakpoint.id, (copy, settings));
                    }
                    Some(error) => failed(breakpoint.id, &error),
                }
            }
            Some(_) => {}
        }
    }

    let mut known = shared.copies.lock().unwrap_or_else(PoisonError::into_inner);
    for (copy, original) in copies {
        match original {
            Some(original) => known.insert((number, copy), original),
            None => known.remove(&(number, copy)),
        };
    }

    failures
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

#[cfg(test)]
mod stops_model;

#[cfg(test)]
mod tests {
    use super::*;

    /// A driver for an unlaunched root, and the group's events.
    fn driver(scope: StopScope) -> (Driver, broadcast::Receiver<GroupEvent>) {
        let root = Debugger::new(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("build/test-programs/fork"),
        )
        .expect("make the root session");
        let shared = Arc::new(Shared {
            members: Mutex::default(),
            handles: Mutex::default(),
            copies: Mutex::default(),
        });
        let (events, received) = broadcast::channel(GROUP_EVENT_CAPACITY);
        (Driver::new(shared, events, root, scope), received)
    }

    fn breakpoint_stop() -> StopReason {
        StopReason::Breakpoint {
            address: Some(crate::VirtualAddress::new(0x1234)),
            hits: Arc::from([crate::BreakpointHit {
                breakpoint: BreakpointId::new(1),
                hit_count: 1,
            }]),
        }
    }

    /// A client that shows each member's stop may resume before the group
    /// has stopped the rest. The pause still on its way to a member is
    /// part of that group stop, not a stop of its own.
    #[tokio::test]
    async fn a_resume_while_the_group_stops_waits_for_the_pauses_under_way() {
        let (mut driver, mut received) = driver(StopScope::Group);
        driver.stops.phase = Phase::Stopping {
            first: Some((2, breakpoint_stop())),
            waiting: BTreeSet::from([1]),
            paused: Some(2),
            was_stopped: false,
        };
        let (reply, resumed) = oneshot::channel();

        driver.resume(Command::ResumeAll(None, reply)).await;
        driver.stopped(1, StopReason::Pause);
        driver.after_internal().await;

        assert_eq!(
            received.try_recv().expect("the group stop"),
            GroupEvent::GroupStopped {
                member: 2,
                reason: breakpoint_stop(),
            }
        );
        assert!(
            received.try_recv().is_err(),
            "the pause under way stops nothing more"
        );
        assert!(resumed.await.is_ok(), "the resume is answered");
        assert!(driver.stops.is_running(), "the group runs");
        tokio::time::timeout(std::time::Duration::from_secs(5), driver.shutdown())
            .await
            .expect("end the root session in time")
            .expect("end the root session");
    }

    /// Found by the model in `stops_model`: where each member's stop is
    /// its own, a pause of the whole group still ends once each member
    /// stopped or will not, reporting no stop of its own.
    #[test]
    fn a_pause_of_members_that_stop_alone_ends_with_their_stops() {
        let mut stops = Stops::new(StopScope::Process);
        assert_eq!(stops.pause_all(None, BTreeSet::from([1, 2])), None);
        assert_eq!(
            stops.stopped(1, StopReason::Pause),
            Some(StopAction::Report(1, StopReason::Pause))
        );
        assert_eq!(stops.paused(2, None), None, "member 2 never stopped");
        assert_eq!(
            stops.defer("continue"),
            Some("continue"),
            "a continue is served"
        );

        let mut stops = Stops::new(StopScope::Process);
        assert_eq!(stops.defer("continue"), Some("continue"));
        assert_eq!(stops.pause_all(None, BTreeSet::from([1])), None);
        assert_eq!(stops.paused(1, None), None, "member 1 never stopped");
    }

    /// Found by the model in `stops_model`: a pause that fails for member
    /// 1 must not report member 1 as stopped. The group stop names a
    /// member that did stop, and with none, nothing stopped the group.
    #[test]
    fn a_pause_of_the_group_names_a_member_that_stopped() {
        let mut stops = Stops::<()>::new(StopScope::Group);
        assert_eq!(stops.pause_all(None, BTreeSet::from([1, 2])), None);
        assert_eq!(stops.paused(1, None), None, "member 1's pause failed");
        assert_eq!(
            stops.stopped(2, StopReason::Pause),
            Some(StopAction::Report(2, StopReason::Pause))
        );

        let mut stops = Stops::<()>::new(StopScope::Group);
        assert_eq!(stops.pause_all(None, BTreeSet::from([1])), None);
        assert_eq!(stops.paused(1, None), None, "member 1's pause failed");
        assert!(stops.is_running(), "nothing stopped the group");
    }

    /// Where each member's stop is its own, a pause of the whole group
    /// leaves it stopped only until a client continues a member alone:
    /// then a fork child that joins is resumed rather than left stopped.
    #[tokio::test]
    async fn a_member_continued_alone_ends_a_pause_of_members_that_stop_alone() {
        let (mut driver, _received) = driver(StopScope::Process);
        driver.stops.phase = Phase::Stopped;

        driver
            .event(
                2,
                DebuggerEvent::InferiorContinued {
                    revision: 0,
                    process_id: ProcessId::new(2),
                    execution_id: crate::ExecutionId::new(1),
                    resumed: ResumeScope::Process(ProcessId::new(2)),
                },
            )
            .await;
        assert!(driver.stops.is_running(), "the group runs");
        tokio::time::timeout(std::time::Duration::from_secs(5), driver.shutdown())
            .await
            .expect("end the root session in time")
            .expect("end the root session");
    }

    /// Found by the model in `stops_model`: a client pauses the group
    /// just as a member's breakpoint has begun to stop it.
    #[test]
    fn a_pause_during_a_group_stop_keeps_the_stop_it_is_for() {
        let mut stops = Stops::<()>::new(StopScope::Group);
        let breakpoint = breakpoint_stop();
        assert_eq!(
            stops.stopped(1, breakpoint.clone()),
            Some(StopAction::PauseOthers(1, breakpoint.clone()))
        );
        assert_eq!(
            stops.pause_all(Some((1, breakpoint.clone())), BTreeSet::from([2, 4])),
            None
        );
        assert_eq!(stops.pause_all(None, BTreeSet::from([2, 4])), None);
        assert_eq!(stops.stopped(2, StopReason::Pause), None);
        assert_eq!(
            stops.stopped(4, StopReason::Pause),
            Some(StopAction::Report(1, breakpoint))
        );
    }

    #[tokio::test]
    async fn a_stop_arriving_after_a_pause_failure_is_still_reported() {
        let (mut driver, mut received) = driver(StopScope::Group);
        driver.stops.phase = Phase::Stopping {
            first: Some((2, StopReason::Pause)),
            waiting: BTreeSet::from([1, 3]),
            paused: Some(2),
            was_stopped: false,
        };
        let breakpoint = breakpoint_stop();

        driver.paused(1, None);
        driver.stopped(1, breakpoint.clone());
        driver.paused(3, None);
        assert_eq!(
            received.try_recv().expect("the first group stop"),
            GroupEvent::GroupStopped {
                member: 2,
                reason: StopReason::Pause,
            }
        );

        tokio::time::timeout(std::time::Duration::from_secs(5), driver.continue_all())
            .await
            .expect("report the queued stop in time")
            .expect("report the queued stop");
        assert_eq!(
            received.try_recv().expect("the coincident breakpoint"),
            GroupEvent::GroupStopped {
                member: 1,
                reason: breakpoint,
            }
        );
        tokio::time::timeout(std::time::Duration::from_secs(5), driver.shutdown())
            .await
            .expect("end the root session in time")
            .expect("end the root session");
    }
}
