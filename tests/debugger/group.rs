//! Process groups: the processes a program starts, debugged together.

use super::*;

use std::collections::BTreeSet;

use microscope::{
    BreakpointId, FollowMode, FollowPolicy, GroupEvent, MemberNumber, MemberState, ProcessGroup,
    Program,
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
        support::flight_recordings::watch_scenarios();
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
                GroupEvent::GroupStopped { member, reason, .. } => return (member, reason),
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
                GroupEvent::GroupStopped { member, reason, .. } => {
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
    let mut group = Group::new("process-tree", FollowPolicy::default());
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
async fn the_roots_signal_policy_is_every_members() {
    let usr1 = microscope::signal_named("SIGUSR1").expect("SIGUSR1");
    let mut group = Group::new("process-tree", FollowPolicy::default());
    group.break_at("child_work").await;
    group.launch(&["2", "signal", "serial"]).await;

    // [2] stops before it raises SIGUSR1, which stops by default. The
    // root's new policy must reach [2], already a member, and [3], which
    // joins after it.
    let (member, reason) = group.stop().await;
    assert_eq!(member, 2, "{reason:?}");
    group
        .inner
        .root()
        .set_signal_policy(
            usr1,
            microscope::SignalPolicy {
                stop: false,
                print: false,
                pass: true,
            },
        )
        .await
        .expect("change the root's policy");
    group.inner.continue_all().await.expect("continue");

    let stops = group.run_to_exit().await;
    assert!(
        stops
            .iter()
            .all(|(_, reason)| !matches!(reason, StopReason::Exception(_))),
        "no member stops at SIGUSR1: {stops:#?}"
    );
    assert_eq!(breakpoint_members(&stops), [3], "{stops:#?}");

    // Each child exits 0 only when its handler ran, so the signal was
    // delivered to both.
    assert_eq!(
        group.root_status(),
        MemberState::Exited(ExitStatus::Code(0))
    );
    group.shutdown().await;
}

#[tokio::test]
async fn the_roots_catchpoints_are_every_members() {
    let mut group = Group::new("process-tree", FollowPolicy::default());
    let tgkill =
        microscope::syscalls::SyscallId::named(microscope::syscalls::Abi::X86_64, "tgkill")
            .expect("tgkill");
    let catchpoint = group
        .inner
        .root()
        .add_catchpoint(Arc::from([tgkill]), microscope::CatchPhase::Entry, None)
        .await
        .expect("catch tgkill");
    group.launch(&["1", "signal"]).await;

    // Only the child calls tgkill, to raise its SIGUSR1.
    let (member, reason) = group.stop().await;
    assert_eq!(member, 2, "{reason:?}");
    let StopReason::Syscall(stop) = reason else {
        panic!("expected the catchpoint's stop, got {reason:?}");
    };
    // A catchpoint has the same id in every member.
    assert_eq!(*stop.catchpoints, [catchpoint.id]);
    group.shutdown().await;
}

#[tokio::test]
async fn a_restarted_child_joins_again_with_the_same_breakpoints() {
    let mut group = Group::new("process-tree", FollowPolicy::default());
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
    let mut group = Group::new("process-tree", named("exec-target"));
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
    let mut group = Group::new("process-tree", named("exec-target"));
    group.break_pending("target_work").await;
    group.break_at("children_started").await;
    group.launch(&["16", "spawn"]).await;

    let stops = group.run_to_exit().await;
    let members = group.inner.members();
    assert_eq!(members.len(), 17, "{members:#?}");
    for member in &members[1..] {
        // A vfork child never becomes a member before its exec, so it
        // joins by executing, and is not the root's fork child.
        assert!(member.executed.is_some(), "{member:#?}");
        assert_eq!(member.state, MemberState::Exited(ExitStatus::Code(42)));
    }
    assert_eq!(
        breakpoint_members(&stops),
        (1..=17).collect::<Vec<_>>(),
        "{stops:#?}"
    );
    assert_eq!(
        group.root_status(),
        MemberState::Exited(ExitStatus::Code(0))
    );
    group.shutdown().await;
}

#[tokio::test]
async fn shutdown_ends_every_process_the_group_launched() {
    let mut group = Group::new("process-tree", FollowPolicy::default());
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
            matches!(state, None | Some('Z' | 'X')),
            "{process:?} is {state:?}"
        );
    }
}

#[tokio::test]
async fn an_attached_groups_fork_children_join_and_are_released_with_it() {
    let mut parent = support::ExternalProcess::spawn(&Scenario::fixture("attach-fork"));
    let group = ProcessGroup::attach(
        parent.process_id(),
        None,
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

/// The group follows every event of its members, however far its tasks
/// fall behind them, as a busy runtime leaves them. Here the runtime's only
/// thread is held while the root launches and starts hundreds of threads,
/// more events than a subscriber keeps: the group still knows the root's
/// process, that it ran, and that it stopped.
#[tokio::test(flavor = "current_thread")]
async fn the_group_misses_no_member_event_however_far_it_falls_behind() {
    use microscope::DebuggerEvent;
    use tokio::sync::broadcast::error::TryRecvError;

    let mut group = Group::new("thread-burst", FollowPolicy::default());
    group.break_at("burst_done").await;
    let mut probe = group.inner.root().subscribe();

    // Launched from a thread of its own, since this one runs no task until
    // the root has stopped.
    let root = group.inner.root();
    let launcher = std::thread::spawn(move || {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("a runtime")
            .block_on(root.launch())
            .expect("launch the root")
    });
    let deadline = std::time::Instant::now() + DEADLINE;
    let mut published = 0;
    loop {
        match probe.try_recv() {
            Ok(DebuggerEvent::InferiorStopped { .. }) => break,
            Ok(_) => published += 1,
            Err(TryRecvError::Lagged(missed)) => published += missed,
            Err(TryRecvError::Empty) => {
                assert!(std::time::Instant::now() < deadline, "the root stops");
                std::thread::sleep(Duration::from_millis(1));
            }
            Err(TryRecvError::Closed) => panic!("the root's session ended"),
        }
    }
    launcher.join().expect("the launcher");
    assert!(
        published > 256,
        "{published} events fit in a subscriber's queue"
    );

    let (member, reason) = group.stop().await;
    assert_eq!(member, 1);
    assert!(function_breakpoint(&reason), "{reason:?}");
    let InferiorState::Stopped { process_id, .. } = group
        .inner
        .root()
        .snapshot()
        .await
        .expect("the root's state")
        .inferior
    else {
        panic!("the root is stopped");
    };
    let root = group.inner.members().remove(0);
    assert_eq!(root.process, Some(process_id), "{root:?}");
    assert!(matches!(root.state, MemberState::Stopped(_)), "{root:?}");

    group.inner.continue_all().await.expect("continue the root");
    while group.next().await != GroupEvent::GroupExited {}
    group.shutdown().await;
}

/// Reading the program to attach to, which takes seconds for a large one,
/// runs off the runtime's threads, so other tasks, such as a web server's
/// tabs, go on meanwhile. A FIFO that no one writes holds the read until
/// the test opens it for writing.
#[tokio::test(flavor = "current_thread")]
async fn reading_the_program_to_attach_to_leaves_the_runtime_free() {
    use std::os::unix::fs::OpenOptionsExt as _;
    use std::sync::atomic::{AtomicBool, Ordering};

    let scratch = support::ScratchDir::new("attach-fifo");
    let fifo = scratch.path().join("program");
    nix::unistd::mkfifo(&fifo, nix::sys::stat::Mode::S_IRWXU).expect("make the FIFO");

    // A read that holds the runtime's only thread stops this test's task
    // too, so a thread of its own ends the read at the deadline.
    let (finished, watching) = std::sync::mpsc::channel::<()>();
    let expired = Arc::new(AtomicBool::new(false));
    let watchdog = std::thread::spawn({
        let (fifo, expired) = (fifo.clone(), Arc::clone(&expired));
        move || {
            if watching.recv_timeout(DEADLINE).is_err() {
                expired.store(true, Ordering::SeqCst);
                drop(fs::OpenOptions::new().write(true).open(&fifo));
            }
        }
    });

    let attach = tokio::spawn({
        let fifo = fifo.clone();
        async move {
            ProcessGroup::attach(
                ProcessId::new(u64::from(std::process::id())),
                Some(&fifo),
                FollowPolicy::default(),
                &microscope::DebugFileOptions::default(),
            )
            .await
        }
    });

    // A writer opens without waiting only once the read has the FIFO open.
    let writer = loop {
        assert!(
            !expired.load(Ordering::SeqCst),
            "the runtime ran nothing else while the program was read"
        );
        match fs::OpenOptions::new()
            .write(true)
            .custom_flags(nix::libc::O_NONBLOCK)
            .open(&fifo)
        {
            Ok(writer) => break writer,
            Err(error) if error.raw_os_error() == Some(nix::libc::ENXIO) => {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
            Err(error) => panic!("open the FIFO for writing: {error}"),
        }
    };
    drop(writer);

    let attached = timeout(DEADLINE, attach)
        .await
        .expect("the attach ends")
        .expect("the attach task");
    assert!(attached.is_err(), "an empty file is no program");
    let _ = finished.send(());
    watchdog.join().expect("the watchdog");
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

/// A breakpoint in a JavaScript file a member runs stops that member
/// through its own inspector: each file `node --test` runs in a node of
/// its own stops once at its line, which only it loads.
#[tokio::test]
async fn each_file_node_test_runs_stops_at_its_javascript_breakpoint() {
    support::flight_recordings::watch_scenarios();
    let scratch = support::ScratchDir::new("group-node-test-js");
    let names = ["a.test.js", "b.test.js"];
    for name in names {
        fs::write(
            scratch.path().join(name),
            "require('node:test')('runs', () => {\n  const answer = 42;\n  return answer;\n});\n",
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
    for name in names {
        group
            .inner
            .root()
            .add_breakpoint_with(
                BreakpointSpec::Source {
                    path: scratch.path().join(name),
                    line: microscope::LineNumber::new(3).expect("a line"),
                },
                microscope::BreakpointOptions {
                    pending: true,
                    ..Default::default()
                },
            )
            .await
            .expect("add the breakpoint");
    }
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
    assert!(
        stops
            .iter()
            .all(|(_, reason)| matches!(reason, StopReason::Breakpoint { address: None, .. })),
        "only the JavaScript breakpoints stop the group: {stops:#?}"
    );
    let members = breakpoint_members(&stops);
    assert_eq!(members.len(), 2, "one stop per file's node: {stops:#?}");
    assert!(
        !members.contains(&1),
        "the runner runs neither file: {stops:#?}"
    );
    assert_eq!(
        members.iter().collect::<BTreeSet<_>>().len(),
        2,
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
        "const port = 8080;\nconsole.log('listening', port);\n",
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
    group
        .inner
        .root()
        .add_breakpoint_with(
            BreakpointSpec::Source {
                path: scratch.path().join("server.js"),
                line: microscope::LineNumber::new(2).expect("a line"),
            },
            microscope::BreakpointOptions {
                pending: true,
                ..Default::default()
            },
        )
        .await
        .expect("add the JavaScript breakpoint");
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
        stops.iter().any(|(member, reason)| *member == app.number
            && matches!(reason, StopReason::Breakpoint { address: None, .. })),
        "the app stops at its JavaScript breakpoint, through its inspector: {stops:#?}"
    );
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

/// Waits for the group to report that `member` did not take a change to
/// the group's `breakpoint`.
async fn not_mirrored(group: &mut Group, member: MemberNumber, breakpoint: BreakpointId) {
    loop {
        if let GroupEvent::BreakpointNotMirrored {
            member: behind,
            breakpoint: missed,
            ..
        } = group.next().await
            && (behind, missed) == (member, breakpoint)
        {
            return;
        }
    }
}

/// A member that fails to take a change to the group's breakpoint must not
/// look as if it had it: the group reports the failure, and does not record
/// the change as mirrored, so the next change reports it again.
#[tokio::test]
async fn a_breakpoint_change_a_member_fails_to_take_is_reported_and_stays_due() {
    let mut group = Group::new("process-tree", FollowPolicy::default());
    let root = group.inner.root();
    let original = root
        .add_breakpoint(BreakpointSpec::Function("child_work".to_owned()))
        .await
        .expect("add the breakpoint");
    group.launch(&["1", "direct"]).await;

    let (member, reason) = group.stop().await;
    assert!(function_breakpoint(&reason), "{reason:?}");
    assert_eq!(member, 2);

    // The member's copy goes, so no change to it can take.
    let child = group.inner.member(member).expect("the member's handle");
    let copy = child
        .snapshot()
        .await
        .expect("the member's snapshot")
        .breakpoints[0]
        .id;
    assert_eq!(
        group.inner.group_breakpoint(member, copy),
        Some(original.id)
    );
    child
        .remove_breakpoint(copy)
        .await
        .expect("remove the copy");

    root.set_breakpoint_condition(
        original.id,
        Some(microscope::Condition::parse("1 == 0").expect("the condition")),
    )
    .await
    .expect("set the condition");
    not_mirrored(&mut group, member, original.id).await;

    // The change is still due, so the next one tries it again.
    root.add_breakpoint(BreakpointSpec::Function("children_started".to_owned()))
        .await
        .expect("add another breakpoint");
    not_mirrored(&mut group, member, original.id).await;

    group.inner.continue_all().await.expect("continue");
    group.run_to_exit().await;
    group.shutdown().await;
}

#[tokio::test]
async fn a_member_detached_alone_runs_on_without_its_traps() {
    let mut group = Group::new("process-tree", FollowPolicy::default());
    group.break_at("child_work").await;
    group.launch(&["2", "direct"]).await;

    let (member, reason) = group.stop().await;
    assert!(function_breakpoint(&reason), "{reason:?}");
    assert_ne!(member, 1);
    assert!(matches!(
        group.inner.detach(1).await,
        Err(microscope::Error::RootDetached)
    ));
    group.inner.detach(member).await.expect("detach the member");
    assert!(group.inner.member(member).is_none());

    // The detached child left its stop at the breakpoint without the trap,
    // so it exits 0, and its parent with it.
    group.inner.continue_all().await.expect("continue");
    let stops = group.run_to_exit().await;
    assert!(
        stops.iter().all(|(stopped, _)| *stopped != member),
        "{stops:#?}"
    );
    assert!(group.seen.contains(&GroupEvent::MemberDetached { member }));
    assert_eq!(
        group.root_status(),
        MemberState::Exited(ExitStatus::Code(0))
    );
    group.shutdown().await;
}
