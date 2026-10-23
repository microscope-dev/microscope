//! A session of a process group: the selected member, the commands that
//! list and select members, and runs that wait for the whole group.

use std::time::Instant;

use anyhow::{Result, anyhow, bail};
use microscope::{
    ExitStatus, GroupEvent, GroupHandle, MemberNumber, MemberState, SourcePathMap, StopReason,
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
    /// between commands, so each prints once.
    events: tokio::sync::Mutex<broadcast::Receiver<GroupEvent>>,
}

impl Group {
    pub fn new(handle: GroupHandle, source_paths: SourcePathMap) -> Self {
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
        let started = Instant::now();
        match start {
            Start::Launch => {
                group
                    .handle
                    .root()
                    .launch_with(self.launch.options())
                    .await?;
            }
            Start::Continue => group.handle.continue_all().await?,
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
                Err(RecvError::Lagged(missed)) => {
                    lines.push(format!(
                        "{}: missed {missed} events of the group, so lines about its processes may be missing",
                        renderer.paint(Role::Warning, "warning"),
                    ));
                    continue;
                }
                Err(RecvError::Closed) => bail!("the group ended"),
            };

            match event {
                GroupEvent::GroupStopped { member, reason } => break (member, reason),
                GroupEvent::MemberLost { member } => {
                    bail!(
                        "the session of [{member}] failed, and released its process to run on its own"
                    );
                }
                GroupEvent::GroupExited => {
                    let status = match &group.handle.members()[0].state {
                        MemberState::Exited(status) => status.clone(),
                        _ => ExitStatus::Code(0),
                    };
                    break (1, StopReason::Exited(status));
                }
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
                        member_events.watch(debugger);
                    }
                    self.note_group_event(&event);
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
        member_events.finish(&mut lines, renderer);
        let lines = lines.finish(renderer).await;
        let stop = self
            .group_stop(&group.handle, member, reason, elapsed)
            .await?;
        Ok(join_lines(&lines, &stop))
    }

    /// What the group did since the prompt last read its events, which a
    /// run reports first, as the prompt would have printed it.
    fn earlier_notices(&self, events: &mut broadcast::Receiver<GroupEvent>, lines: &mut RunLines) {
        loop {
            match events.try_recv() {
                Ok(event) => {
                    if let Some(line) = self.group_notice(&event) {
                        lines.push(line);
                    }
                }
                Err(TryRecvError::Lagged(_)) => {}
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

    /// The group's next event that no run reported, which never comes for
    /// a session of one.
    pub(super) async fn next_group_event(&self) -> GroupEvent {
        let Some(group) = self.group.as_ref() else {
            return std::future::pending().await;
        };
        let mut events = group.events.lock().await;
        loop {
            match events.recv().await {
                Ok(event) => return event,
                Err(RecvError::Lagged(_)) => {}
                Err(RecvError::Closed) => return std::future::pending().await,
            }
        }
    }

    /// The line a group event prints between commands, if any.
    pub(super) fn group_notice(&self, event: &GroupEvent) -> Option<String> {
        self.note_group_event(event);
        let group = self.group.as_ref()?;
        format::group_event(event, &group.handle.members(), self.renderers.stdout)
    }

    /// Notes what a group event means for the session's outcome: a process
    /// the policy wanted that could not be followed fails a batch run.
    pub(super) fn note_group_event(&self, event: &GroupEvent) {
        if matches!(event, GroupEvent::ProcessUnfollowed { .. }) {
            self.unfollowed_process
                .store(true, std::sync::atomic::Ordering::Relaxed);
        }
    }
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
}

impl MemberEvents {
    /// The events of every member `group` has so far.
    fn of(group: &GroupHandle) -> Self {
        let (forward, received) = tokio::sync::mpsc::unbounded_channel();
        let mut events = Self {
            forward,
            received,
            forwarders: tokio::task::JoinSet::new(),
        };
        for member in group.members() {
            if let Some(debugger) = group.member(member.number) {
                events.watch(debugger);
            }
        }
        events
    }

    /// Forwards `debugger`'s events too.
    fn watch(&mut self, debugger: microscope::DebuggerHandle) {
        let mut events = debugger.subscribe();
        let forward = self.forward.clone();
        self.forwarders.spawn(async move {
            loop {
                let event = events.recv().await;
                if matches!(event, Err(RecvError::Closed))
                    || forward.send((debugger.clone(), event)).is_err()
                {
                    break;
                }
            }
        });
    }

    /// Records the events already forwarded.
    fn finish(mut self, lines: &mut RunLines, renderer: super::terminal::Renderer) {
        self.forwarders.abort_all();
        while let Ok((debugger, event)) = self.received.try_recv() {
            lines.record(&debugger, event, renderer);
        }
    }
}
