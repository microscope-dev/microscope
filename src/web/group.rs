//! The processes a web session debugs together, and the one its tabs look
//! at.
//!
//! A launched or attached program is debugged as a [`microscope::ProcessGroup`].
//! The session selects one member at a time: the state every tab receives
//! describes it, and every request about the program acts on its handle,
//! except breakpoints, which are the group's and go to its root. The
//! selection belongs to the session rather than to a tab, as the state does.
//! A stop of the group selects the member that stopped.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex, PoisonError};

use microscope::{
    DebuggerEvent, DebuggerHandle, GroupEvent, GroupHandle, MemberNumber, StateSnapshot, StopId,
    StopReason,
};
use tokio::sync::broadcast::error::{RecvError, TryRecvError};
use tokio::sync::{Notify, broadcast, mpsc, watch};
use tokio::task::JoinSet;

use super::describe::{Credits, Describer, Images, Run, plain};
use super::protocol::{self, Joined, State, Stream};
use super::session::{Outlet, observe, publish};
use crate::cli::format;
use crate::cli::terminal::Renderer;

/// The member requests act on, and the images its code is read from.
#[derive(Clone)]
pub struct View {
    pub member: MemberNumber,
    pub handle: DebuggerHandle,
    pub images: Arc<Images>,
}

/// The session's selected member, shared by its requests and its pump.
pub struct Selection {
    view: Mutex<View>,

    /// Every member's images, made once, so each keeps what it fetched.
    images: Mutex<BTreeMap<MemberNumber, Arc<Images>>>,

    /// Wakes the pump to publish the newly selected member.
    changed: Notify,
}

impl Selection {
    /// Selects the group's root, or the one process debugged.
    pub fn new(handle: DebuggerHandle, images: Arc<Images>) -> Self {
        Self {
            images: Mutex::new(BTreeMap::from([(1, Arc::clone(&images))])),
            view: Mutex::new(View {
                member: 1,
                handle,
                images,
            }),
            changed: Notify::new(),
        }
    }

    pub fn view(&self) -> View {
        self.view
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    pub fn member(&self) -> MemberNumber {
        self.view
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .member
    }

    /// Member `member`'s images, made the first time they are asked for.
    fn images(&self, member: MemberNumber, handle: &DebuggerHandle) -> Arc<Images> {
        Arc::clone(
            self.images
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .entry(member)
                .or_insert_with(|| Arc::new(Images::new(handle.clone()))),
        )
    }

    /// Selects `member`, whose handle is `handle`, and has the pump publish
    /// its state.
    pub fn select(&self, member: MemberNumber, handle: DebuggerHandle) {
        let images = self.images(member, &handle);
        *self.view.lock().unwrap_or_else(PoisonError::into_inner) = View {
            member,
            handle,
            images,
        };
        self.changed.notify_one();
    }
}

/// A member's events, as the pump receives them.
type MemberEvent = (MemberNumber, Result<DebuggerEvent, RecvError>);

/// Publishes the selected member's state after every burst of events in
/// the group, and says what the group's processes did.
pub struct Pump {
    pub state: Arc<watch::Sender<Arc<State>>>,
    pub outlet: Outlet,
    pub group: Option<GroupHandle>,
    pub selection: Arc<Selection>,

    /// How the root joined: launched or attached.
    pub root: Joined,

    /// Each member's describer, which keeps its stops; the root's is made
    /// with the session.
    describers: BTreeMap<MemberNumber, Describer>,

    /// The member whose stop last stopped the group, and that stop.
    stopped: Option<MemberNumber>,
    seen_stop: Option<(MemberNumber, StopId)>,

    /// The members whose events the pump forwards.
    watched: BTreeSet<MemberNumber>,

    /// Each member's latest revision seen. A state's revision is their sum,
    /// which never goes back as the selection moves between members.
    revisions: BTreeMap<MemberNumber, u64>,

    forward: mpsc::UnboundedSender<MemberEvent>,
    received: mpsc::UnboundedReceiver<MemberEvent>,

    /// The tasks that forward members' events, which end with the pump.
    forwarders: JoinSet<()>,

    /// Who started each run, which the root's describer shares.
    credits: Arc<Credits>,
}

impl Pump {
    pub fn new(
        state: Arc<watch::Sender<Arc<State>>>,
        outlet: Outlet,
        group: Option<GroupHandle>,
        selection: Arc<Selection>,
        root: Describer,
        joined: Joined,
    ) -> Self {
        let (forward, received) = mpsc::unbounded_channel();
        // An attached group starts stopped where its root was attached.
        let stopped = (joined == Joined::Attached && group.is_some()).then_some(1);
        let credits = Arc::clone(&root.credits);
        Self {
            state,
            outlet,
            group,
            selection,
            root: joined,
            describers: BTreeMap::from([(1, root)]),
            stopped,
            seen_stop: None,
            watched: BTreeSet::new(),
            revisions: BTreeMap::new(),
            forward,
            received,
            forwarders: JoinSet::new(),
            credits,
        }
    }

    /// The state to publish first, before the pump runs.
    pub async fn first_state(&mut self) -> Option<State> {
        self.describe().await
    }

    /// Runs until the session's debugger ends. `root` is the root's events,
    /// subscribed before the first state was described, and `group` the
    /// group's.
    pub async fn run(
        mut self,
        root: broadcast::Receiver<DebuggerEvent>,
        mut group: Option<broadcast::Receiver<GroupEvent>>,
    ) {
        self.watch(1, root);
        if let Some(handle) = self.group.clone() {
            for member in handle.members().iter().filter(|member| member.number != 1) {
                if let Some(debugger) = handle.member(member.number) {
                    self.watch(member.number, debugger.subscribe());
                }
            }
        }

        loop {
            let Some(next) = self.describe().await else {
                return;
            };
            let (writes, settings) = self.counters();
            publish(&self.state, next, &writes, &settings);

            tokio::select! {
                biased;
                Some((member, event)) = self.received.recv() => self.member_event(member, event),
                event = next_group_event(group.as_mut()) => match event {
                    Some(Ok(event)) => self.group_event(event),
                    Some(Err(missed)) => self.caught_up(missed),
                    None => group = None,
                },
                () = self.selection.changed.notified() => {}

                // A run credited after its stop was shown shows it now.
                () = self.credits.landed.notified() => {}
            }

            // Take every event already queued, so the burst publishes once.
            while let Ok((member, event)) = self.received.try_recv() {
                self.member_event(member, event);
            }
            while let Some(events) = group.as_mut() {
                match events.try_recv() {
                    Ok(event) => self.group_event(event),
                    Err(TryRecvError::Lagged(missed)) => self.caught_up(missed),
                    Err(TryRecvError::Closed) => group = None,
                    Err(TryRecvError::Empty) => break,
                }
            }
        }
    }

    fn counters(
        &self,
    ) -> (
        Arc<std::sync::atomic::AtomicU64>,
        Arc<std::sync::atomic::AtomicU64>,
    ) {
        self.describers.get(&1).map_or_else(
            || (Arc::default(), Arc::default()),
            |root| (Arc::clone(&root.writes), Arc::clone(&root.settings)),
        )
    }

    /// Forwards member `member`'s events to the pump.
    fn watch(&mut self, member: MemberNumber, mut events: broadcast::Receiver<DebuggerEvent>) {
        if !self.watched.insert(member) {
            return;
        }
        let forward = self.forward.clone();
        self.forwarders.spawn(async move {
            loop {
                let event = events.recv().await;
                let closed = matches!(event, Err(RecvError::Closed));
                if forward.send((member, event)).is_err() || closed {
                    return;
                }
            }
        });
    }

    /// The selected member's state, or none once the session's debugger
    /// has ended. A member whose session ended gives way to the root.
    async fn describe(&mut self) -> Option<State> {
        let mut view = self.selection.view();
        let snapshot = match view.handle.snapshot().await {
            Ok(snapshot) => snapshot,
            Err(_) if view.member != 1 => {
                let root = self.group.as_ref()?.root();
                self.selection.select(1, root);
                view = self.selection.view();
                view.handle.snapshot().await.ok()?
            }
            Err(_) => return None,
        };

        let seen = self.revisions.entry(view.member).or_default();
        *seen = (*seen).max(snapshot.revision);
        let revision = self
            .revisions
            .values()
            .fold(0_u64, |sum, seen| sum.saturating_add(*seen));
        let snapshot = self.translate(view.member, snapshot);
        let group = self.group_state(view.member);
        let describer = self.describer(view.member, &view.handle)?;
        let mut state = describer.describe(&snapshot).await;
        state.group = group;
        state.revision = revision;
        Some(state)
    }

    /// Member `member`'s describer, made the first time it is needed with
    /// the session's counters and the member's images.
    fn describer(
        &mut self,
        member: MemberNumber,
        handle: &DebuggerHandle,
    ) -> Option<&mut Describer> {
        if !self.describers.contains_key(&member) {
            let root = self.describers.get(&1)?;
            let mut describer = Describer::new(
                root.id.clone(),
                root.target.clone(),
                handle.clone(),
                Arc::clone(&root.credits),
            );
            describer.writes = Arc::clone(&root.writes);
            describer.settings = Arc::clone(&root.settings);
            describer.images = self.selection.images(member, handle);
            self.describers.insert(member, describer);
        }

        self.describers.get_mut(&member)
    }

    /// A member's snapshot with its copies of the group's breakpoints under
    /// the group's ids, which tabs edit them by.
    fn translate(&self, member: MemberNumber, mut snapshot: StateSnapshot) -> StateSnapshot {
        let Some(group) = self.group.as_ref().filter(|_| member != 1) else {
            return snapshot;
        };

        snapshot.breakpoints = snapshot
            .breakpoints
            .iter()
            .filter_map(|breakpoint| {
                let id = group.group_breakpoint(member, breakpoint.id)?;
                Some(microscope::Breakpoint {
                    id,
                    ..breakpoint.clone()
                })
            })
            .collect();
        if let microscope::InferiorState::Stopped { reason, .. } = &mut snapshot.inferior {
            *reason = group_reason(group, member, reason.clone());
        }

        snapshot
    }

    /// The group as tabs see it, with `selected` selected.
    fn group_state(&self, selected: MemberNumber) -> Option<protocol::Group> {
        let group = self.group.as_ref()?;
        let members = group.members();
        let stopped = self.stopped.filter(|stopped| {
            members.iter().any(|member| {
                member.number == *stopped
                    && matches!(member.state, microscope::MemberState::Stopped(_))
            })
        });

        Some(protocol::Group {
            members: members
                .iter()
                .map(|member| describe_member(group, member, self.root))
                .collect(),
            selected,
            stopped,
        })
    }

    fn member_event(&mut self, member: MemberNumber, event: Result<DebuggerEvent, RecvError>) {
        // The state the page shows is read whole each time; only the lines
        // the lost events would have logged are missing.
        let event = match event {
            Ok(event) => event,
            Err(RecvError::Lagged(missed)) => {
                self.outlet.publish(
                    Stream::Log,
                    &format!(
                        "warning: [{member}] missed {missed} events, so lines about signals, \
                         logged messages, conditions, and library views may be missing\n"
                    ),
                );
                return;
            }
            Err(RecvError::Closed) => return,
        };
        let tag = self
            .group
            .as_ref()
            .filter(|group| group.members().len() > 1)
            .map(|_| format!("[{member}] "))
            .unwrap_or_default();
        let handle = match &self.group {
            Some(group) => group.member(member),
            None => self.describers.get(&1).map(|root| root.handle.clone()),
        };
        let Some(handle) = handle else {
            return;
        };
        let outlet = self.outlet.clone();
        if let Some(describer) = self.describer(member, &handle) {
            if let DebuggerEvent::InferiorStopped {
                stop_id,
                execution_id: Some(execution),
                ..
            } = &event
            {
                describer.ran_to(*stop_id, Run::Member(member, *execution));
            }
            observe(describer, &outlet, &event, &tag);
        }
    }

    fn group_event(&mut self, event: GroupEvent) {
        let Some(group) = self.group.clone() else {
            return;
        };
        let members = group.members();
        if let Some(line) = group_line(&group, &event, &members) {
            self.outlet.publish(Stream::Log, &(line + "\n"));
        }

        let selected = self.selection.member();
        match event {
            GroupEvent::MemberJoined { member, .. } => {
                if let Some(debugger) = group.member(member) {
                    self.watch(member, debugger.subscribe());
                }
            }
            GroupEvent::GroupStopped {
                member,
                stop,
                execution,
                reason,
            } => {
                if let Some(execution) = execution
                    && let Some(handle) = group.member(member)
                    && let Some(describer) = self.describer(member, &handle)
                {
                    describer.ran_to(stop, Run::Group(execution));
                }
                self.stopped = Some(member);
                self.seen_stop = Some((member, stop));
                // A pause stops no member in particular, so the selection
                // stays unless its member is gone.
                if (reason != StopReason::Pause || !live(&members, selected))
                    && is_stopped(&members, member)
                    && let Some(handle) = group.member(member)
                {
                    self.selection.select(member, handle);
                }
            }
            GroupEvent::MemberExited { member, .. }
            | GroupEvent::MemberWatched { member, .. }
            | GroupEvent::MemberDetached { member }
            | GroupEvent::MemberLost { member }
                if member == selected =>
            {
                if let Some(next) = members.iter().find(|member| live(&members, member.number))
                    && let Some(handle) = group.member(next.number)
                {
                    self.selection.select(next.number, handle);
                }
            }
            // The program's end shows as the root's.
            GroupEvent::GroupExited if selected != 1 => {
                self.selection.select(1, group.root());
            }
            _ => {}
        }
    }

    /// Catches up from the group's state after missing `missed` of its
    /// events, which may have held a group stop, members that joined, or
    /// the selected member's end.
    fn caught_up(&mut self, missed: u64) {
        self.missed(missed);
        let Some(group) = self.group.clone() else {
            return;
        };

        for member in group.members() {
            if !self.watched.contains(&member.number)
                && let Some(debugger) = group.member(member.number)
            {
                self.watch(member.number, debugger.subscribe());
            }
        }

        if let Some(last) = group.last_stop()
            && Some((last.member, last.stop)) != self.seen_stop
        {
            self.group_event(GroupEvent::GroupStopped {
                member: last.member,
                stop: last.stop,
                execution: last.execution,
                reason: last.reason,
            });
        }

        let members = group.members();
        let selected = self.selection.member();
        if !live(&members, selected) {
            match members.iter().find(|member| live(&members, member.number)) {
                Some(next) => {
                    if let Some(handle) = group.member(next.number) {
                        self.selection.select(next.number, handle);
                    }
                }
                None if selected != 1 => self.selection.select(1, group.root()),
                None => {}
            }
        }
    }

    fn missed(&self, missed: u64) {
        self.outlet.publish(
            Stream::Log,
            &format!(
                "warning: missed {missed} events of the group, so lines about its processes may be missing\n"
            ),
        );
    }
}

/// The next event of the group, or how many it missed, or none once it has
/// ended; forever none without a group.
async fn next_group_event(
    events: Option<&mut broadcast::Receiver<GroupEvent>>,
) -> Option<Result<GroupEvent, u64>> {
    let Some(events) = events else {
        return std::future::pending().await;
    };

    match events.recv().await {
        Ok(event) => Some(Ok(event)),
        Err(RecvError::Lagged(missed)) => Some(Err(missed)),
        Err(RecvError::Closed) => None,
    }
}

/// Whether member `number` has a process microscope debugs.
fn live(members: &[microscope::Member], number: MemberNumber) -> bool {
    members.iter().any(|member| {
        member.number == number
            && matches!(
                member.state,
                microscope::MemberState::Running | microscope::MemberState::Stopped(_)
            )
    })
}

fn is_stopped(members: &[microscope::Member], number: MemberNumber) -> bool {
    members.iter().any(|member| {
        member.number == number && matches!(member.state, microscope::MemberState::Stopped(_))
    })
}

/// A member's stop with the group's breakpoint ids in place of its copies'.
fn group_reason(group: &GroupHandle, member: MemberNumber, reason: StopReason) -> StopReason {
    match reason {
        StopReason::Breakpoint { address, hits } => StopReason::Breakpoint {
            address,
            hits: hits
                .iter()
                .map(|hit| microscope::BreakpointHit {
                    breakpoint: group
                        .group_breakpoint(member, hit.breakpoint)
                        .unwrap_or(hit.breakpoint),
                    hit_count: hit.hit_count,
                })
                .collect(),
        },
        reason => reason,
    }
}

/// A member as tabs see it. `root` is how the group's root joined.
fn describe_member(
    group: &GroupHandle,
    member: &microscope::Member,
    root: Joined,
) -> protocol::Member {
    use microscope::MemberState;

    let (state, description) = match &member.state {
        MemberState::NotStarted => (protocol::MemberState::NotStarted, "not started".to_owned()),
        MemberState::Running => (protocol::MemberState::Running, "running".to_owned()),
        MemberState::Stopped(reason) => (
            protocol::MemberState::Stopped,
            plain(&group_reason(group, member.number, reason.clone())),
        ),
        MemberState::Exited(status) => (
            protocol::MemberState::Exited,
            plain(&StopReason::Exited(status.clone())),
        ),
        MemberState::Watched(program) => (
            protocol::MemberState::Watched,
            format!("watched, running {}", program.display()),
        ),
        MemberState::Detached => (protocol::MemberState::Detached, "detached".to_owned()),
        MemberState::Lost => (
            protocol::MemberState::Lost,
            "lost: its session failed, so the process was released or killed".to_owned(),
        ),
    };
    let joined = match (member.number, &member.executed) {
        (1, _) => root,
        (_, Some(_)) => Joined::Executed,
        (_, None) => Joined::Forked,
    };

    protocol::Member {
        number: member.number,
        pid: member.process.map(microscope::ProcessId::get),
        parent: member.parent,
        command: member.command.clone(),
        joined,
        program: member
            .executed
            .as_ref()
            .map(|program| program.display().to_string()),
        state,
        description,
    }
}

/// The line the console shows for a group event, if any: the terminal's,
/// and also the processes the group watches, and which member stopped the
/// group once it has several.
fn group_line(
    group: &GroupHandle,
    event: &GroupEvent,
    members: &[microscope::Member],
) -> Option<String> {
    let renderer = Renderer::new(false);
    match event {
        GroupEvent::ProcessWatched {
            process, program, ..
        } => Some(format!(
            "process {process} executed {}, which microscope watches until it runs a program to debug",
            program.display()
        )),
        GroupEvent::GroupStopped { member, reason, .. }
            if members.len() > 1 && *reason != StopReason::Pause =>
        {
            let stopped = members
                .iter()
                .find(|candidate| candidate.number == *member)?;
            Some(format!(
                "{} stopped the group: {}",
                format::member(stopped, renderer),
                plain(&group_reason(group, *member, reason.clone()))
            ))
        }
        event => format::group_event(event, members, renderer),
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    /// A pump whose group events fell behind still shows the group stopped,
    /// as the group's own state says, when the stop was among those lost.
    #[tokio::test]
    async fn a_pump_that_missed_the_group_stop_shows_it() {
        let directory = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let program = microscope::Program::load(
            directory.join("build/test-programs/basic"),
            &microscope::DebugFileOptions::default(),
        )
        .expect("load the program");
        let group = microscope::ProcessGroup::new(program, microscope::FollowPolicy::default())
            .expect("make the group");
        let handle = group.handle();
        let root = handle.root();
        root.add_breakpoint(microscope::BreakpointSpec::Function(
            "breakpoint_target".to_owned(),
        ))
        .await
        .expect("add the breakpoint");
        let mut seen = handle.subscribe();
        root.launch_with(microscope::LaunchOptions::default())
            .await
            .expect("launch the root");
        tokio::time::timeout(Duration::from_secs(10), async {
            while !matches!(seen.recv().await, Ok(GroupEvent::GroupStopped { .. })) {}
        })
        .await
        .expect("the group stops");

        // The pump's group events, from which a burst pushed the stop.
        let (sender, events) = broadcast::channel(4);
        for _ in 0..8 {
            sender
                .send(GroupEvent::MemberDetached { member: 99 })
                .expect("send an event");
        }

        let (state, mut published) = watch::channel(Arc::new(State::idle(None)));
        let target = protocol::Target {
            kind: protocol::TargetKind::Launch,
            program: "basic".to_owned(),
            arguments: Vec::new(),
            pid: None,
        };
        let describer = Describer::new("session".to_owned(), target, root.clone(), Arc::default());
        let selection = Arc::new(Selection::new(root.clone(), Arc::clone(&describer.images)));
        let pump = Pump::new(
            Arc::new(state),
            Outlet::unread(),
            Some(handle.clone()),
            selection,
            describer,
            Joined::Launched,
        );
        let pumping = tokio::spawn(pump.run(root.subscribe(), Some(events)));

        let shown = tokio::time::timeout(
            Duration::from_secs(5),
            published.wait_for(|state| {
                state
                    .group
                    .as_ref()
                    .is_some_and(|group| group.stopped == Some(1))
            }),
        )
        .await
        .is_ok();
        assert!(shown, "the page never shows the group stopped");

        // The pump ends with the root's session.
        drop(sender);
        tokio::time::timeout(Duration::from_secs(10), group.shutdown())
            .await
            .expect("end the group in time")
            .expect("end the group");
        tokio::time::timeout(Duration::from_secs(10), pumping)
            .await
            .expect("the pump ends with the group")
            .expect("the pump does not panic");
    }
}
