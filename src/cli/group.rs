//! A session of a process group: the selected member, the commands that
//! list and select members, and runs that wait for the whole group.

use std::time::Instant;

use anyhow::{Result, anyhow, bail};
use microscope::{
    ExitStatus, GroupEvent, GroupHandle, InferiorState, MemberNumber, MemberState, SourcePathMap,
    StopReason,
};
use tokio::sync::broadcast::{self, error::RecvError, error::TryRecvError};

use super::commands::{RunLines, join_lines};
use super::terminal::Role;
use super::{Cli, Selected, format};

/// The group a session debugs, and how its members read source files.
pub struct Group {
    pub handle: GroupHandle,
    source_paths: SourcePathMap,

    /// The group's events, read by a run while it waits and by the prompt
    /// between commands, so each prints once. A holder awaits only a
    /// receive, which a cancelled one leaves as it found it.
    #[expect(clippy::disallowed_types, reason = "held only across a receive")]
    events: tokio::sync::Mutex<broadcast::Receiver<GroupEvent>>,
}

impl Group {
    pub fn new(handle: GroupHandle, source_paths: SourcePathMap) -> Self {
        #[expect(clippy::disallowed_types, reason = "held only across a receive")]
        let events = tokio::sync::Mutex::new(handle.subscribe());
        Self {
            handle,
            source_paths,
            events,
        }
    }
}

/// How a group run starts.
pub enum Start {
    /// Launches the program, as the session's settings say.
    Launch,
    Continue,
}

impl Cli {
    /// Selects member `number`, whose handle commands act on from now on.
    pub(super) fn select_member(&self, number: MemberNumber) -> Result<()> {
        let group = self
            .group
            .as_ref()
            .ok_or_else(|| anyhow!("the session debugs one process"))?;
        let handle = group
            .handle
            .member(number)
            .ok_or_else(|| anyhow!("no process is member [{number}]"))?
            .with_source_paths(group.source_paths.clone());
        *self
            .selected
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Selected {
            member: number,
            handle,
        };
        Ok(())
    }

    /// The members, for `processes`.
    pub(super) fn processes(&self) -> Result<String> {
        let group = self
            .group
            .as_ref()
            .ok_or_else(|| anyhow!("the session debugs one process"))?;
        Ok(format::processes(
            &group.handle.members(),
            self.selected_member(),
            self.renderers.stdout,
        ))
    }

    /// `process N`: selects a member, and says where it is.
    pub(super) async fn process(&self, argument: &str) -> Result<String> {
        let number = parse_member(argument)?;
        self.select_member(number)?;
        let renderer = self.renderers.stdout;
        let selected = format!(
            "{} {}",
            renderer.paint(Role::Success, "selected"),
            renderer.paint(Role::Metadata, format!("[{number}]"))
        );
        let place = self.describe_location(false).await.unwrap_or_default();
        Ok(join_lines(&selected, &place))
    }

    /// `detach process N`: releases a stopped member to run on its own.
    pub(super) async fn detach_member(
        &self,
        arguments: &[&str],
        spec: &super::commands::CommandSpec,
    ) -> Result<String> {
        let ["process", argument] = arguments else {
            return Err(spec.usage_error());
        };
        let number = parse_member(argument)?;
        let group = self
            .group
            .as_ref()
            .ok_or_else(|| anyhow!("the session debugs one process"))?;
        group.handle.detach(number).await?;
        if self.selected_member() == number {
            self.select_member(1)?;
        }

        let renderer = self.renderers.stdout;
        Ok(format!(
            "{} {}",
            renderer.paint(Role::Success, "detached"),
            renderer.paint(Role::Metadata, format!("[{number}]"))
        ))
    }

    /// The prompt, which names the selected member once the group has more
    /// than one.
    pub(super) fn prompt(&self) -> String {
        let prompt = &self.settings.config.ui.prompt;
        let Some(group) = &self.group else {
            return prompt.clone();
        };
        if group.handle.members().len() < 2 {
            return prompt.clone();
        }

        let member = format!("[{}]", self.selected_member());
        prompt.trim_end().strip_suffix(')').map_or_else(
            || format!("{member} {prompt}"),
            |start| format!("{start} {member}){}", &prompt[prompt.trim_end().len()..]),
        )
    }

    /// `run` or `continue`: runs the group, or the one process.
    pub(super) async fn run_program(&self, start: Start) -> Result<String> {
        if self.group.is_some() {
            // A group's run is a large future, which most commands do
            // not need room for.
            return Box::pin(self.run_group(start)).await;
        }
        let debugger = self.debugger();
        match start {
            Start::Launch => {
                self.execute_until_stop(debugger.run_with(self.launch.options()))
                    .await
            }
            Start::Continue => self.execute_until_stop(debugger.resume()).await,
        }
    }

    /// Waits until the group has seen the selected member's command end,
    /// and reports what the group did meanwhile. When the command ended in
    /// a stop, it waits for the group stop with that stop's id. An older
    /// group stop that nobody read yet is printed as a notice instead.
    pub(super) async fn member_execution_end(
        &self,
        reason: &StopReason,
        lines: &mut RunLines,
    ) -> Result<()> {
        let Some(group) = &self.group else {
            return Ok(());
        };
        let member = self.selected_member();

        // Nothing else moves the member, so its current stop is the one
        // the command ended at.
        let stop = match self.debugger().snapshot().await?.inferior {
            InferiorState::Stopped { stop_id, .. } if !matches!(reason, StopReason::Exited(_)) => {
                Some(stop_id)
            }
            _ => None,
        };

        let mut events = group.events.lock().await;
        loop {
            let event = match events.recv().await {
                Ok(event) => event,

                // The events lost may have held the command's end, which
                // the group's state shows.
                Err(RecvError::Lagged(missed)) => {
                    lines.push(missed_warning(missed, self.renderers.stdout));
                    let members = group.handle.members();
                    let state = members
                        .iter()
                        .find(|candidate| candidate.number == member)
                        .map(|member| &member.state);
                    if state == Some(&MemberState::Lost) {
                        bail!(
                            "the session of [{member}] failed, so its process was released or killed"
                        );
                    }
                    let stopped = group
                        .handle
                        .last_stop()
                        .is_some_and(|last| last.member == member && Some(last.stop) == stop);
                    let ended = matches!(
                        state,
                        Some(MemberState::Exited(_) | MemberState::Watched(_))
                    );
                    if stopped || ended || !members.iter().any(|member| live(&member.state)) {
                        break;
                    }
                    continue;
                }
                Err(RecvError::Closed) => bail!("the group ended"),
            };
            match event {
                GroupEvent::GroupStopped {
                    member: stopped,
                    stop: reported,
                    ..
                } if stopped == member && Some(reported) == stop => break,
                GroupEvent::MemberExited { member: ended, .. }
                | GroupEvent::MemberWatched { member: ended, .. }
                    if ended == member =>
                {
                    if group
                        .handle
                        .members()
                        .iter()
                        .any(|member| live(&member.state))
                    {
                        break;
                    }
                }
                GroupEvent::GroupExited => break,
                GroupEvent::MemberLost { member: lost } if lost == member => {
                    bail!(
                        "the session of [{member}] failed, so its process was released or killed"
                    );
                }
                event => {
                    if let Some(line) = self.group_notice(&event) {
                        lines.push(line);
                    }
                }
            }
        }
        drop(events);
        Ok(())
    }

    /// Runs the group, by launching its root or continuing every member,
    /// until it stops or every member has exited, and reports what the
    /// group did meanwhile and the stop, in the member that stopped, which
    /// is selected.
    pub(super) async fn run_group(&self, start: Start) -> Result<String> {
        let group = self
            .group
            .as_ref()
            .ok_or_else(|| anyhow!("the session debugs one process"))?;
        let mut events = group.events.lock().await;
        let renderer = self.renderers.stdout;
        let mut lines = RunLines::default();
        self.earlier_notices(&mut events, &mut lines);
        let mut member_events = MemberEvents::of(&group.handle);

        // What the group's state held as the run began, which a run that
        // misses events compares it with.
        let before = group.handle.last_stop();
        let lost_before = lost(&group.handle);
        let started = Instant::now();

        // A continue ends at the group stop that names it. A launch's run
        // takes the first group stop: no member is left from before it to
        // stop late.
        let execution = match start {
            Start::Launch => {
                group
                    .handle
                    .root()
                    .launch_with(self.launch.options())
                    .await?;
                None
            }
            Start::Continue => Some(group.handle.continue_all().await?),
        };

        // Development builds panic on request once the group runs, to test
        // what a panic in the CLI leaves of the processes it debugs.
        #[cfg(debug_assertions)]
        {
            const PANIC_WHILE_RUNNING: &str = "MICROSCOPE_TEST_CLI_PANIC";
            assert!(
                std::env::var_os(PANIC_WHILE_RUNNING).is_none(),
                "a panic injected by {PANIC_WHILE_RUNNING}"
            );
        }

        let mut root_exit = None;
        let (member, reason) = loop {
            let received = tokio::select! {
                biased;
                Some((debugger, event)) = member_events.received.recv() => {
                    lines.record(&debugger, event, renderer);
                    continue;
                }
                received = events.recv() => received,
            };
            let event = match received {
                Ok(event) => event,
                // The events lost may have held the run's end, a member
                // it lost, or members that joined, which the group's state
                // shows.
                Err(RecvError::Lagged(missed)) => {
                    lines.push(missed_warning(missed, renderer));
                    member_events.catch_up(&group.handle);
                    if let Some(member) = lost(&group.handle)
                        .into_iter()
                        .find(|member| !lost_before.contains(member))
                    {
                        bail!(
                            "the session of [{member}] failed, so its process was released or killed"
                        );
                    }
                    if let Some(stop) = group.handle.last_stop()
                        && Some(&stop) != before.as_ref()
                        && (execution.is_none() || stop.execution == execution)
                    {
                        break (stop.member, stop.reason);
                    }
                    if !group
                        .handle
                        .members()
                        .iter()
                        .any(|member| live(&member.state))
                    {
                        break group_exit(&group.handle);
                    }
                    continue;
                }
                Err(RecvError::Closed) => bail!("the group ended"),
            };

            match event {
                GroupEvent::GroupStopped {
                    member,
                    execution: ended,
                    reason,
                    ..
                } if execution.is_none() || ended == execution => break (member, reason),
                GroupEvent::MemberLost { member } => {
                    bail!(
                        "the session of [{member}] failed, so its process was released or killed"
                    );
                }
                GroupEvent::GroupExited => break group_exit(&group.handle),
                // The root's exit reports as the run's end when the
                // group's follows it, so it waits to see which.
                GroupEvent::MemberExited { member: 1, .. } => {
                    root_exit = format::group_event(&event, &group.handle.members(), renderer);
                }
                event => {
                    if let Some(line) = root_exit.take() {
                        lines.push(line);
                    }
                    if let GroupEvent::MemberJoined { member, .. } = &event
                        && let Some(debugger) = group.handle.member(*member)
                    {
                        member_events.watch(*member, debugger);
                    }
                    if let Some(line) =
                        format::group_event(&event, &group.handle.members(), renderer)
                    {
                        lines.push(line);
                    }
                }
            }
        };

        // The prompt reads what the group does from here on.
        drop(events);
        let elapsed = started.elapsed();
        if let Some(line) = root_exit
            && !matches!(reason, StopReason::Exited(_))
        {
            lines.push(line);
        }
        member_events.finish(&mut lines, renderer).await;
        let lines = lines.finish(renderer).await;
        let stop = self
            .group_stop(&group.handle, member, reason, elapsed)
            .await?;
        Ok(join_lines(&lines, &stop))
    }

    /// What the group did since the prompt last read its events, which a
    /// run reports first, as the prompt would have printed it. Reading them
    /// before the run starts also keeps the run's own rules, where a lost
    /// member fails it and an exit ends it, to what happens during it.
    fn earlier_notices(&self, events: &mut broadcast::Receiver<GroupEvent>, lines: &mut RunLines) {
        loop {
            match events.try_recv() {
                Ok(event) => {
                    if let Some(line) = self.group_notice(&event) {
                        lines.push(line);
                    }
                }
                Err(TryRecvError::Lagged(missed)) => {
                    lines.push(missed_warning(missed, self.renderers.stdout));
                }
                Err(TryRecvError::Empty | TryRecvError::Closed) => break,
            }
        }
    }

    /// Selects the member that stopped, and reports its stop.
    async fn group_stop(
        &self,
        group: &GroupHandle,
        member: MemberNumber,
        reason: StopReason,
        elapsed: std::time::Duration,
    ) -> Result<String> {
        let renderer = self.renderers.stdout;
        self.select_member(member)?;
        let reason = group_reason(group, member, reason);
        let stop = self.stop_report(&reason, elapsed).await;
        let members = group.members();
        let stop = match members.iter().find(|candidate| candidate.number == member) {
            Some(stopped) if members.len() > 1 && !matches!(reason, StopReason::Exited(_)) => {
                join_lines(&format!("in {}", format::member(stopped, renderer)), &stop)
            }
            _ => stop,
        };
        Ok(stop)
    }

    /// The line the group's next event that no run reported prints between
    /// commands, if any. It never comes for a session of one.
    pub(super) async fn next_group_notice(&self) -> Option<String> {
        let Some(group) = self.group.as_ref() else {
            return std::future::pending().await;
        };
        let mut events = group.events.lock().await;
        match events.recv().await {
            Ok(event) => self.group_notice(&event),
            Err(RecvError::Lagged(missed)) => Some(missed_warning(missed, self.renderers.stdout)),
            Err(RecvError::Closed) => std::future::pending().await,
        }
    }

    /// The line a group event prints between commands, if any.
    pub(super) fn group_notice(&self, event: &GroupEvent) -> Option<String> {
        let group = self.group.as_ref()?;
        format::group_event(event, &group.handle.members(), self.renderers.stdout)
    }
}

/// Whether a member has a process microscope debugs, or will.
const fn live(state: &MemberState) -> bool {
    matches!(
        state,
        MemberState::NotStarted | MemberState::Running | MemberState::Stopped(_)
    )
}

/// The members whose sessions failed.
fn lost(group: &GroupHandle) -> Vec<MemberNumber> {
    group
        .members()
        .iter()
        .filter(|member| member.state == MemberState::Lost)
        .map(|member| member.number)
        .collect()
}

/// A run's end once every member is gone, reported as the root's exit.
fn group_exit(group: &GroupHandle) -> (MemberNumber, StopReason) {
    let status = match group.members().first().map(|member| &member.state) {
        Some(MemberState::Exited(status)) => status.clone(),
        _ => ExitStatus::Code(0),
    };
    (1, StopReason::Exited(status))
}

fn missed_warning(missed: u64, renderer: super::terminal::Renderer) -> String {
    format!(
        "{}: missed {missed} events of the group, so lines about its processes may be missing",
        renderer.paint(Role::Warning, "warning"),
    )
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

fn parse_member(argument: &str) -> Result<MemberNumber> {
    argument
        .trim_start_matches('[')
        .trim_end_matches(']')
        .parse()
        .map_err(|_| anyhow!("invalid process number: {argument}"))
}

/// A member's events, as they arrive.
type MemberEvent = (
    microscope::DebuggerHandle,
    Result<microscope::DebuggerEvent, RecvError>,
);

/// Each member's own events, such as signals and logged messages, which a
/// run reports as a single process's run reports them.
struct MemberEvents {
    forward: tokio::sync::mpsc::UnboundedSender<MemberEvent>,
    received: tokio::sync::mpsc::UnboundedReceiver<MemberEvent>,

    /// The tasks that forward them, which end when this drops.
    forwarders: tokio::task::JoinSet<()>,

    /// Tells the forwarders to pass on what their members already sent,
    /// and end.
    stop: tokio::sync::watch::Sender<bool>,

    /// The members whose events are forwarded.
    watched: std::collections::BTreeSet<MemberNumber>,
}

impl MemberEvents {
    /// The events of every member `group` has so far.
    fn of(group: &GroupHandle) -> Self {
        let (forward, received) = tokio::sync::mpsc::unbounded_channel();
        let mut events = Self {
            forward,
            received,
            forwarders: tokio::task::JoinSet::new(),
            stop: tokio::sync::watch::Sender::new(false),
            watched: std::collections::BTreeSet::new(),
        };
        events.catch_up(group);
        events
    }

    /// Forwards the events of every member of `group` not yet watched.
    fn catch_up(&mut self, group: &GroupHandle) {
        for member in group.members() {
            if let Some(debugger) = group.member(member.number) {
                self.watch(member.number, debugger);
            }
        }
    }

    /// Forwards member `number`'s events too, through `debugger`.
    fn watch(&mut self, number: MemberNumber, debugger: microscope::DebuggerHandle) {
        if !self.watched.insert(number) {
            return;
        }
        let mut events = debugger.subscribe();
        let forward = self.forward.clone();
        let mut stop = self.stop.subscribe();
        self.forwarders.spawn(async move {
            loop {
                let event = tokio::select! {
                    biased;
                    event = events.recv() => event,
                    _ = stop.changed() => break,
                };
                if matches!(event, Err(RecvError::Closed))
                    || forward.send((debugger.clone(), event)).is_err()
                {
                    return;
                }
            }

            // A member sends its events before the stop that ends the run,
            // so those still queued belong to the run.
            loop {
                let event = match events.try_recv() {
                    Ok(event) => Ok(event),
                    Err(TryRecvError::Lagged(missed)) => Err(RecvError::Lagged(missed)),
                    Err(TryRecvError::Empty | TryRecvError::Closed) => return,
                };
                if forward.send((debugger.clone(), event)).is_err() {
                    return;
                }
            }
        });
    }

    /// Records every event the members sent during the run.
    async fn finish(mut self, lines: &mut RunLines, renderer: super::terminal::Renderer) {
        let _ = self.stop.send(true);
        while self.forwarders.join_next().await.is_some() {}
        while let Ok((debugger, event)) = self.received.try_recv() {
            lines.record(&debugger, event, renderer);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use microscope::{
        DebugFileOptions, FollowPolicy, GroupEvent, InferiorState, ProcessGroup, Program,
    };
    use tokio::sync::{broadcast, watch};

    use super::super::{Cli, LaunchSettings, Renderers, config::Settings};

    /// A CLI session of a group, driven by calling its commands directly,
    /// with no terminal. The group's events reach the session through a
    /// channel the test owns, so a test can add its own events to it.
    /// `forwarded` counts the group stops and exits copied into it so far,
    /// and an event put in `slipped` is sent just before the next of them.
    /// When `flood` is set, the next stop or exit is followed by enough
    /// other events to push it out of the session's channel, as a burst
    /// of events would.
    struct Session {
        cli: Cli,
        group: ProcessGroup,
        sender: broadcast::Sender<GroupEvent>,
        forwarded: watch::Receiver<usize>,
        slipped: Arc<Mutex<Option<GroupEvent>>>,
        flood: Arc<AtomicBool>,
    }

    /// How many events the session's channel holds.
    const CAPACITY: usize = 64;

    fn ends_a_run(event: &GroupEvent) -> bool {
        matches!(
            event,
            GroupEvent::GroupStopped { .. } | GroupEvent::GroupExited
        )
    }

    impl Session {
        fn of(program: &str) -> Self {
            let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
            let program = Program::load(
                root.join("build/test-programs").join(program),
                &DebugFileOptions::default(),
            )
            .expect("load the program");
            let group =
                ProcessGroup::new(program, FollowPolicy::default()).expect("make the group");
            let mut cli = Cli::new(
                group.handle().root(),
                Renderers::console(false, root.to_owned()),
                Settings::defaults(root.to_owned()),
                LaunchSettings::default(),
            )
            .with_group(group.handle(), microscope::SourcePathMap::default());

            let (sender, received) = broadcast::channel(CAPACITY);
            let (count, forwarded) = watch::channel(0);
            let slipped = Arc::new(Mutex::new(None));
            let mut events = group.handle().subscribe();
            let forward = sender.clone();
            let slip = Arc::clone(&slipped);
            let flood = Arc::new(AtomicBool::new(false));
            let flooding = Arc::clone(&flood);
            tokio::spawn(async move {
                while let Ok(event) = events.recv().await {
                    let ends = ends_a_run(&event);
                    if ends && let Some(early) = slip.lock().expect("the slipped event").take() {
                        let _ = forward.send(early);
                    }
                    let _ = forward.send(event);
                    if ends && flooding.swap(false, Ordering::Relaxed) {
                        for _ in 0..CAPACITY {
                            let _ = forward.send(GroupEvent::MemberDetached { member: 99 });
                        }
                    }
                    if ends {
                        count.send_modify(|count| *count += 1);
                    }
                }
            });
            #[expect(clippy::disallowed_types, reason = "held only across a receive")]
            let received = tokio::sync::Mutex::new(received);
            cli.group.as_mut().expect("a group session").events = received;

            Self {
                cli,
                group,
                sender,
                forwarded,
                slipped,
                flood,
            }
        }

        /// Runs command `line`, and returns what it printed.
        async fn run(&self, line: &str) -> String {
            let control = tokio::time::timeout(Duration::from_secs(10), self.cli.execute(line))
                .await
                .unwrap_or_else(|_| panic!("`{line}` did not finish"))
                .unwrap_or_else(|error| panic!("`{line}` failed: {error:#}"));
            match control {
                super::super::Control::Continue(output) => output,
                _ => String::new(),
            }
        }

        /// Waits until `count` of the group's stops and exits have reached
        /// the session.
        async fn forwarded(&mut self, count: usize) {
            tokio::time::timeout(
                Duration::from_secs(10),
                self.forwarded.wait_for(|&forwarded| forwarded >= count),
            )
            .await
            .expect("the group's stops reach the session")
            .expect("the group runs");
        }

        /// The group stops and exits the session received and no command
        /// read.
        async fn unread_stops(&self) -> Vec<GroupEvent> {
            let group = self.cli.group.as_ref().expect("a group session");
            let mut events = group.events.lock().await;
            std::iter::from_fn(|| events.try_recv().ok())
                .filter(ends_a_run)
                .collect()
        }

        /// A copy of the root's current stop, as if nothing read it yet.
        async fn stale_root_stop(&self) -> GroupEvent {
            let InferiorState::Stopped {
                stop_id, reason, ..
            } = self
                .group
                .handle()
                .root()
                .snapshot()
                .await
                .expect("the root's state")
                .inferior
            else {
                panic!("the root is not stopped");
            };
            GroupEvent::GroupStopped {
                member: 1,
                stop: stop_id,
                execution: None,
                reason,
            }
        }

        async fn end(self) {
            drop(self.cli);
            tokio::time::timeout(Duration::from_secs(10), self.group.shutdown())
                .await
                .expect("end the group in time")
                .expect("end the group");
        }
    }

    /// A step waits for the group stop its own step made. An older group
    /// stop that nothing read yet, like one the prompt did not get to
    /// print, must not end it. If it did, the step's own stop would stay
    /// unread, and the next command would take it as its own end.
    #[tokio::test]
    async fn a_step_ends_at_its_own_group_stop_not_an_older_one() {
        let mut session = Session::of("basic");
        session.run("break breakpoint_target").await;
        session.run("run").await;
        session.forwarded(1).await;
        assert_eq!(session.unread_stops().await, [], "the run read its stop");

        let stale = session.stale_root_stop().await;
        session.sender.send(stale).expect("send the stale stop");

        session.run("stepi").await;
        session.forwarded(2).await;
        assert_eq!(
            session.unread_stops().await,
            [],
            "the step ended at the old stop and left its own unread"
        );

        session.end().await;
    }

    /// A continue ends at the group stop or exit it caused. An older group
    /// stop that arrives while it runs, after the notices from before the
    /// continue were printed, must not end it early.
    #[tokio::test]
    async fn a_continue_ends_at_its_own_end_not_an_older_stop() {
        let mut session = Session::of("basic");
        session.run("break breakpoint_target").await;
        session.run("run").await;
        session.forwarded(1).await;

        let stale = session.stale_root_stop().await;
        *session.slipped.lock().expect("the slipped event") = Some(stale);
        session.run("continue").await;
        session.forwarded(2).await;
        assert_eq!(
            session.unread_stops().await,
            [],
            "the continue ended at the old stop and left its own end unread"
        );

        session.end().await;
    }

    /// An event from before a continue, which the prompt had no turn to
    /// print, is a notice of what happened then. A member lost before
    /// must not fail the continue as if it was lost during it.
    #[tokio::test]
    async fn a_continue_prints_an_older_loss_without_failing() {
        let mut session = Session::of("basic");
        session.run("break breakpoint_target").await;
        session.run("run").await;
        session.forwarded(1).await;

        let lost = GroupEvent::MemberLost { member: 2 };
        session.sender.send(lost).expect("send the old loss");
        session.run("continue").await;
        session.forwarded(2).await;

        session.end().await;
    }

    /// A step whose group stop was pushed out of the session's events by
    /// a burst learns of it from the group, and ends at it.
    #[tokio::test]
    async fn a_step_whose_group_stop_was_lost_ends_at_it() {
        let mut session = Session::of("basic");
        session.run("break breakpoint_target").await;
        session.run("run").await;
        session.forwarded(1).await;

        session.flood.store(true, Ordering::Relaxed);
        let stepped = session.run("stepi").await;
        session.forwarded(2).await;
        assert!(stepped.contains("missed"), "{stepped}");
        assert!(session.unread_stops().await.is_empty());

        session.end().await;
    }

    /// A continue whose group stop, and then whose exit, was pushed out
    /// of the session's events learns of it from the group, and ends.
    #[tokio::test]
    async fn a_continue_whose_end_was_lost_ends_at_it() {
        let mut session = Session::of("basic");
        session.run("break breakpoint_target").await;
        session.run("run").await;
        session.forwarded(1).await;

        session.flood.store(true, Ordering::Relaxed);
        let stopped = session.run("continue").await;
        session.forwarded(2).await;
        assert!(stopped.contains("breakpoint 1"), "{stopped}");

        session.run("delete 1").await;
        session.flood.store(true, Ordering::Relaxed);
        let exited = session.run("continue").await;
        session.forwarded(3).await;
        assert!(exited.contains("exited"), "{exited}");

        session.end().await;
    }

    /// The prompt says when it missed some of the group's events, rather
    /// than printing what came after as if nothing was lost.
    #[tokio::test]
    async fn the_prompt_says_it_missed_group_events() {
        let session = Session::of("basic");
        for _ in 0..2 * CAPACITY {
            session
                .sender
                .send(GroupEvent::MemberDetached { member: 99 })
                .expect("send an event");
        }

        let notice = tokio::time::timeout(Duration::from_secs(5), session.cli.next_group_notice())
            .await
            .expect("a notice");
        assert!(
            notice.as_ref().is_some_and(|line| line.contains("missed")),
            "{notice:?}"
        );

        session.end().await;
    }
}
