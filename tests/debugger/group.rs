//! Process groups: the processes a program starts, debugged together.

use super::*;

use std::collections::BTreeSet;

use microscope::{
    FollowMode, FollowPolicy, GroupEvent, MemberNumber, MemberState, ProcessGroup, Program,
};
use tokio::sync::broadcast;

const DEADLINE: Duration = Duration::from_secs(20);

/// A group for a fixture, with its events subscribed from the start.
struct Group {
    inner: ProcessGroup,
    events: broadcast::Receiver<GroupEvent>,
    seen: Vec<GroupEvent>,
}

impl Group {
    fn new(fixture: &str, policy: FollowPolicy) -> Self {
        let program = Program::load(
            Scenario::fixture(fixture),
            &microscope::DebugFileOptions::default(),
        )
        .expect("load the fixture");
        let group = ProcessGroup::new(program, policy).expect("start the group");
        let events = group.subscribe();
        Self {
            inner: group,
            events,
            seen: Vec::new(),
        }
    }

    async fn break_at(&self, function: &str) {
        self.inner
            .root()
            .add_breakpoint(BreakpointSpec::Function(function.to_owned()))
            .await
            .expect("add the breakpoint");
    }

    /// Adds a breakpoint the root's program does not have, for a program
    /// a member executes.
    async fn break_pending(&self, function: &str) {
        self.inner
            .root()
            .add_breakpoint_with(
                BreakpointSpec::Function(function.to_owned()),
                microscope::BreakpointOptions {
                    pending: true,
                    ..Default::default()
                },
            )
            .await
            .expect("add the pending breakpoint");
    }

    async fn launch(&self, arguments: &[&str]) {
        self.inner
            .root()
            .launch_with(LaunchOptions {
                arguments: arguments
                    .iter()
                    .map(|argument| (*argument).into())
                    .collect(),
                ..LaunchOptions::default()
            })
            .await
            .expect("launch the root");
    }

    /// The next group event, which must arrive before the deadline.
    async fn next(&mut self) -> GroupEvent {
        let event = timeout(DEADLINE, self.events.recv())
            .await
            .unwrap_or_else(|_| panic!("a group event in time; saw {:#?}", self.seen))
            .expect("the group reports events");
        self.seen.push(event.clone());
        event
    }

    /// Waits for the next group stop, returning the member that stopped
    /// and why; fails if the group exits first.
    async fn stop(&mut self) -> (MemberNumber, StopReason) {
        loop {
            match self.next().await {
                GroupEvent::GroupStopped { member, reason } => return (member, reason),
                GroupEvent::GroupExited => panic!("the group exited; saw {:#?}", self.seen),
                _ => {}
            }
        }
    }

    /// Continues the group through every stop until every member exited,
    /// returning the members that stopped, in order.
    async fn run_to_exit(&mut self) -> Vec<(MemberNumber, StopReason)> {
        let mut stops = Vec::new();
        loop {
            match self.next().await {
                GroupEvent::GroupStopped { member, reason } => {
                    self.assert_all_stopped();
                    stops.push((member, reason));
                    self.inner.continue_all().await.expect("continue the group");
                }
                GroupEvent::GroupExited => return stops,
                _ => {}
            }
        }
    }

    /// Every member that has a process is stopped, as /proc shows it.
    fn assert_all_stopped(&self) {
        for member in self.inner.members() {
            if !matches!(member.state, MemberState::Stopped(_)) {
                continue;
            }
            let process = member.process.expect("a stopped member has a process");
            assert_eq!(
                process_state(process),
                Some('t'),
                "member {} is stopped for its tracer",
                member.number
            );
        }
        assert!(
            self.inner
                .members()
                .iter()
                .all(|member| !matches!(member.state, MemberState::Running)),
            "no member runs at a group stop: {:#?}",
            self.inner.members()
        );
    }

    fn root_status(&self) -> MemberState {
        self.inner.members()[0].state.clone()
    }

    async fn shutdown(self) {
        timeout(DEADLINE, self.inner.shutdown())
            .await
            .expect("shut down in time")
            .expect("shut down the group");
    }
}

/// The state letter /proc gives a process, or none once it is gone.
fn process_state(process: ProcessId) -> Option<char> {
    let stat = fs::read_to_string(format!("/proc/{}/stat", process.get())).ok()?;
    stat.rsplit_once(") ")?.1.chars().next()
}

const fn function_breakpoint(reason: &StopReason) -> bool {
    matches!(reason, StopReason::Breakpoint { .. })
}

#[tokio::test]
async fn a_fork_child_joins_with_the_groups_breakpoints_before_it_runs() {
    let mut group = Group::new("fork", FollowPolicy::default());
    group.break_at("shared_work").await;
    group.launch(&[]).await;

    let stops = group.run_to_exit().await;

    // The parent and the child each stop once in shared_work. Had the
    // child run before its breakpoint was planted, it would not stop.
    let mut members = stops
        .iter()
        .filter(|(_, reason)| function_breakpoint(reason))
        .map(|(member, _)| *member)
        .collect::<Vec<_>>();
    members.sort_unstable();
    assert_eq!(members, [1, 2], "stops: {stops:#?}");

    let joined = group
        .seen
        .iter()
        .find_map(|event| match event {
            GroupEvent::MemberJoined {
                member,
                parent,
                executed,
                ..
            } => Some((*member, *parent, executed.clone())),
            _ => None,
        })
        .expect("the child joined");
    assert_eq!(joined, (2, Some(1), None));

    // The parent exits 0 only when its child exited normally.
    assert_eq!(
        group.root_status(),
        MemberState::Exited(ExitStatus::Code(0))
    );
    group.shutdown().await;
}

#[tokio::test]
async fn a_stop_in_one_member_stops_the_others() {
    let mut group = Group::new("fork", FollowPolicy::default());
    group.break_at("shared_work").await;
    group.launch(&[]).await;

    let (first, reason) = group.stop().await;
    assert!(function_breakpoint(&reason), "{reason:?}");
    group.assert_all_stopped();

    // The other member's own hit is reported at the next continue, or
    // after it resumes; either way each member stops in shared_work once.
    let mut stops = vec![(first, reason)];
    group.inner.continue_all().await.expect("continue");
    stops.extend(group.run_to_exit().await);
    let hits = stops
        .iter()
        .filter(|(_, reason)| function_breakpoint(reason))
        .count();
    assert_eq!(hits, 2, "{stops:#?}");
    group.shutdown().await;
}

#[tokio::test]
async fn without_following_a_fork_child_runs_on_its_own() {
    let mut group = Group::new(
        "fork",
        FollowPolicy {
            mode: FollowMode::None,
            ..FollowPolicy::default()
        },
    );
    group.break_at("shared_work").await;
    group.launch(&[]).await;

    let stops = group.run_to_exit().await;
    assert_eq!(
        stops.iter().map(|(member, _)| *member).collect::<Vec<_>>(),
        [1],
        "only the root stops: {stops:#?}"
    );
    assert_eq!(group.inner.members().len(), 1);
    assert_eq!(
        group.root_status(),
        MemberState::Exited(ExitStatus::Code(0))
    );
    group.shutdown().await;
}

/// The members that stopped at a breakpoint, in number order.
fn breakpoint_members(stops: &[(MemberNumber, StopReason)]) -> Vec<MemberNumber> {
    let mut members = stops
        .iter()
        .filter(|(_, reason)| function_breakpoint(reason))
        .map(|(member, _)| *member)
        .collect::<Vec<_>>();
    members.sort_unstable();
    members
}

fn named(program: &str) -> FollowPolicy {
    FollowPolicy {
        programs: vec![program.to_owned()],
        ..FollowPolicy::default()
    }
}

#[tokio::test]
async fn concurrent_children_each_join_and_stop() {
    let mut group = Group::new("tree", FollowPolicy::default());
    group.break_at("child_work").await;
    group.launch(&["3", "direct"]).await;

    let stops = group.run_to_exit().await;
    assert_eq!(breakpoint_members(&stops), [2, 3, 4], "{stops:#?}");

    // Every child exited 0, so none received a trap or a SIGCONT.
    assert_eq!(
        group.root_status(),
        MemberState::Exited(ExitStatus::Code(0))
    );
    let members = group.inner.members();
    assert!(
        members[1..].iter().all(|member| member.parent == Some(1)
            && member.state == MemberState::Exited(ExitStatus::Code(0))),
        "{members:#?}"
    );
    group.shutdown().await;
}

#[tokio::test]
async fn a_restarted_child_joins_again_with_the_same_breakpoints() {
    let mut group = Group::new("tree", FollowPolicy::default());
    group.break_at("child_work").await;
    group.launch(&["3", "direct", "serial"]).await;

    let stops = group.run_to_exit().await;
    assert_eq!(breakpoint_members(&stops), [2, 3, 4], "{stops:#?}");
    assert_eq!(
        group.root_status(),
        MemberState::Exited(ExitStatus::Code(0))
    );
    group.shutdown().await;
}

#[tokio::test]
async fn a_child_is_watched_through_a_shell_and_joins_at_the_program_it_executes() {
    let mut group = Group::new("tree", named("exec-target"));
    group.break_pending("target_work").await;
    group.launch(&["2", "shell"]).await;

    let stops = group.run_to_exit().await;
    // The children that ran exec-target stopped in it; the forked copies
    // of tree, which became shells, never did.
    let stopped = breakpoint_members(&stops);
    assert_eq!(stopped.len(), 2, "{stops:#?}");
    let members = group.inner.members();
    for number in &stopped {
        let member = &members[usize::try_from(*number).expect("a small number") - 1];
        assert!(
            member
                .executed
                .as_ref()
                .is_some_and(|program| program.ends_with("exec-target")),
            "{member:#?}"
        );
        assert_eq!(member.state, MemberState::Exited(ExitStatus::Code(42)));
    }

    // Each fork child left the group for watching when it became a shell.
    let watched = group
        .seen
        .iter()
        .filter(|event| {
            matches!(event, GroupEvent::MemberWatched { program, .. }
            if program.file_name().is_some_and(|name| name.to_string_lossy().contains("sh")))
        })
        .count();
    assert_eq!(watched, 2, "{:#?}", group.seen);
    assert_eq!(
        group.root_status(),
        MemberState::Exited(ExitStatus::Code(0))
    );
    group.shutdown().await;
}

#[tokio::test]
async fn a_vfork_child_is_watched_until_it_executes_a_program_to_debug() {
    let mut group = Group::new("tree", named("exec-target"));
    group.break_pending("target_work").await;
    group.break_at("children_started").await;
    group.launch(&["2", "spawn"]).await;

    let stops = group.run_to_exit().await;
    let members = group.inner.members();
    assert_eq!(members.len(), 3, "{members:#?}");
    for member in &members[1..] {
        // A vfork child never becomes a member before its exec, so it
        // joins by executing, and is not the root's fork child.
        assert!(member.executed.is_some(), "{member:#?}");
        assert_eq!(member.state, MemberState::Exited(ExitStatus::Code(42)));
    }
    assert_eq!(breakpoint_members(&stops), [1, 2, 3], "{stops:#?}");
    assert_eq!(
        group.root_status(),
        MemberState::Exited(ExitStatus::Code(0))
    );
    group.shutdown().await;
}

#[tokio::test]
async fn shutdown_ends_every_process_the_group_launched() {
    let mut group = Group::new("tree", FollowPolicy::default());
    group.break_at("children_started").await;
    group.launch(&["2", "hang"]).await;

    // Both children are shells by the time the root stops, or soon after;
    // wait until each has started sleep.
    let (member, _) = group.stop().await;
    assert_eq!(member, 1);
    group.inner.continue_all().await.expect("continue");
    let mut sleepers = BTreeSet::new();
    while sleepers.len() < 2 {
        if let GroupEvent::ProcessWatched {
            process, program, ..
        } = group.next().await
            // nixpkgs' sleep is coreutils' program of many names.
            && (program.ends_with("sleep") || program.ends_with("coreutils"))
        {
            sleepers.insert(process);
        }
    }
    let root = group.inner.members()[0].process.expect("the root runs");
    let shells = group
        .inner
        .members()
        .iter()
        .skip(1)
        .filter_map(|member| member.process)
        .collect::<Vec<_>>();

    group.shutdown().await;
    for process in sleepers.iter().chain(&shells).chain([&root]) {
        // A process killed and reported to the tracer stays a zombie
        // until its parent, or init once that parent died too, reaps it.
        let state = process_state(*process);
        assert!(
            matches!(state, None | Some('Z')),
            "{process:?} is {state:?}"
        );
    }
}

#[tokio::test]
async fn an_attached_groups_fork_children_join_and_are_released_with_it() {
    let mut parent = support::ExternalProcess::spawn(&Scenario::fixture("attach-fork"));
    let group = ProcessGroup::attach(
        parent.process_id(),
        FollowPolicy::default(),
        &microscope::DebugFileOptions::default(),
    )
    .await
    .expect("attach the group");
    let events = group.subscribe();
    let mut group = Group {
        inner: group,
        events,
        seen: Vec::new(),
    };
    group.break_at("child_work").await;
    group.inner.continue_all().await.expect("continue the root");
    parent.release();

    let (member, reason) = group.stop().await;
    assert_eq!(member, 2);
    assert!(function_breakpoint(&reason), "{reason:?}");
    group.inner.continue_all().await.expect("continue");
    loop {
        if let GroupEvent::MemberExited { member, status } = group.next().await {
            assert_eq!((member, status), (2, ExitStatus::Code(7)));
            break;
        }
    }

    // Released, the parent exits with its child's code: 7 for a child that
    // ran its work once and never received SIGCONT.
    group.shutdown().await;
    parent.release();
    wait_for_zombie(parent.process_id());
    assert_eq!(parent.wait().code(), Some(7));
}

/// The node on `PATH`, which the dev shell provides.
fn node() -> std::path::PathBuf {
    std::env::var_os("PATH")
        .and_then(|path| {
            std::env::split_paths(&path)
                .map(|directory| directory.join("node"))
                .find(|candidate| candidate.is_file())
        })
        .expect("node is on PATH in the dev shell")
}

/// `node --test` runs each file in a node of its own, concurrently: each
/// joins the group, through its fork and its exec of node, and stops at a
/// native breakpoint set before the run.
#[tokio::test]
async fn each_file_node_test_runs_joins_the_group() {
    let scratch = support::ScratchDir::new("group-node-test");
    for name in ["a.test.js", "b.test.js"] {
        fs::write(
            scratch.path().join(name),
            "require('node:test')('runs', () => {});\n",
        )
        .expect("write a test file");
    }

    let program =
        Program::load(node(), &microscope::DebugFileOptions::default()).expect("load node");
    let group = ProcessGroup::new(program, FollowPolicy::default()).expect("start the group");
    let events = group.subscribe();
    let mut group = Group {
        inner: group,
        events,
        seen: Vec::new(),
    };
    group.break_at("uv_setup_args").await;
    group
        .inner
        .root()
        .launch_with(LaunchOptions {
            arguments: ["--test", "a.test.js", "b.test.js"]
                .map(std::ffi::OsString::from)
                .to_vec(),
            working_directory: Some(scratch.path().to_owned()),
            ..LaunchOptions::default()
        })
        .await
        .expect("launch node");

    let stops = group.run_to_exit().await;
    let members = breakpoint_members(&stops);
    assert_eq!(
        members.len(),
        3,
        "the runner and one node per file: {stops:#?}"
    );
    assert_eq!(
        members.iter().collect::<BTreeSet<_>>().len(),
        3,
        "each stops once: {stops:#?}"
    );
    assert_eq!(
        group.root_status(),
        MemberState::Exited(ExitStatus::Code(0))
    );
    group.shutdown().await;
}

/// `npm start` runs npm's own node, which starts a shell, which executes
/// the app's node. npm and the app are members; the shell is watched
/// between them, and the app stops at a breakpoint set before the run.
#[tokio::test]
async fn npm_start_is_followed_through_its_shell_into_the_app() {
    let scratch = support::ScratchDir::new("group-npm-start");
    fs::write(
        scratch.path().join("package.json"),
        r#"{"name": "app", "version": "1.0.0", "scripts": {"start": "node server.js"}}"#,
    )
    .expect("write package.json");
    fs::write(
        scratch.path().join("server.js"),
        "console.log('listening');\n",
    )
    .expect("write server.js");
    let npm = node().with_file_name("npm");

    let program = Program::load(&npm, &microscope::DebugFileOptions::default()).expect("load npm");
    let group = ProcessGroup::new(program, FollowPolicy::default()).expect("start the group");
    let events = group.subscribe();
    let mut group = Group {
        inner: group,
        events,
        seen: Vec::new(),
    };
    group.break_at("uv_setup_args").await;
    let cache = scratch.path().join("npm-cache");
    group
        .inner
        .root()
        .launch_with(LaunchOptions {
            arguments: vec!["start".into()],
            working_directory: Some(scratch.path().to_owned()),
            environment: vec![
                ("npm_config_cache".into(), Some(cache.into_os_string())),
                ("npm_config_update_notifier".into(), Some("false".into())),
            ],
            ..LaunchOptions::default()
        })
        .await
        .expect("launch npm");

    let stops = group.run_to_exit().await;
    let members = group.inner.members();
    let app = members
        .iter()
        .find(|member| {
            member
                .executed
                .as_ref()
                .is_some_and(|program| program.ends_with("node"))
        })
        .unwrap_or_else(|| panic!("the app joined by executing node: {members:#?}"));
    let stopped = breakpoint_members(&stops);
    assert!(stopped.contains(&1), "npm's own node stops: {stops:#?}");
    assert!(stopped.contains(&app.number), "the app stops: {stops:#?}");
    assert!(
        group.seen.iter().any(|event| matches!(event,
            GroupEvent::MemberWatched { program, .. }
                if program.file_name().is_some_and(|name| name.to_string_lossy().contains("sh")))),
        "the shell between them is watched: {:#?}",
        group.seen
    );
    assert_eq!(
        group.root_status(),
        MemberState::Exited(ExitStatus::Code(0))
    );
    group.shutdown().await;
}
