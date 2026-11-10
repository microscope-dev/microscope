//! Process groups of CPython programs: a child interpreter a program
//! starts is recognized as a runtime microscope models, and joins.

use std::ffi::OsString;
use std::time::Duration;

use microscope::{
    BreakpointOptions, BreakpointSpec, DebugFileOptions, FollowPolicy, GroupEvent, LaunchOptions,
    MemberState, ProcessGroup, Program, StopReason,
};
use tokio::time::timeout;

use crate::support::python;

const DEADLINE: Duration = Duration::from_secs(60);

/// One interpreter on each side of every split a child's joining takes:
/// the launch bootstrap (3.12) or PEP 768 (3.14), GIL or free-threaded
/// (3.15t, and 3.13t, which takes no Python breakpoints), and a runtime in
/// libpython or in the executable (debian-3.13).
const SPLITS: [&str; 5] = ["3.12", "3.13t", "3.14", "3.15t", "debian-3.13"];

/// A child interpreter started as `subprocess` starts one, by vfork and
/// exec, joins its parent's group, whether its runtime is in the
/// executable or in a libpython it depends on.
#[tokio::test]
async fn a_child_interpreter_joins_its_parents_group() {
    for name in SPLITS {
        let interpreter = python::interpreter(name);
        let program = Program::load(&interpreter, &DebugFileOptions::default())
            .unwrap_or_else(|error| panic!("{name}: load the interpreter: {error}"));
        let group = ProcessGroup::new(program, FollowPolicy::default()).expect("start the group");
        let mut events = group.subscribe();
        group
            .root()
            .add_breakpoint_with(
                BreakpointSpec::Function("Py_RunMain".to_owned()),
                BreakpointOptions {
                    pending: true,
                    ..BreakpointOptions::default()
                },
            )
            .await
            .expect("add the breakpoint");
        group
            .root()
            .launch_with(LaunchOptions {
                arguments: [
                    "-c",
                    "import subprocess, sys; subprocess.run([sys.executable, '-c', 'pass'], check=True)",
                ]
                .map(OsString::from)
                .to_vec(),
                ..LaunchOptions::default()
            })
            .await
            .expect("launch");

        let mut stopped = Vec::new();
        loop {
            let event = timeout(DEADLINE, events.recv())
                .await
                .unwrap_or_else(|_| panic!("{name}: an event in time; stopped {stopped:?}"))
                .expect("events");
            match event {
                GroupEvent::GroupStopped { member, reason, .. } => {
                    if matches!(reason, StopReason::Breakpoint { .. }) {
                        stopped.push(member);
                    }
                    group.continue_all().await.expect("continue");
                }
                GroupEvent::GroupExited => break,
                _ => {}
            }
        }

        stopped.sort_unstable();
        assert_eq!(stopped, [1, 2], "{name}: {:#?}", group.members());
        let members = group.members();
        assert!(
            members
                .iter()
                .all(|member| member.state == MemberState::Exited(microscope::ExitStatus::Code(0))),
            "{name}: {members:#?}"
        );
        timeout(DEADLINE, group.shutdown())
            .await
            .expect("shut down in time")
            .expect("shut down");
    }
}

/// A Python breakpoint stops in a child interpreter of each version that
/// takes Python breakpoints, 3.12 through the launch's bootstrap, whether
/// the child is a new interpreter `subprocess` started or a fork of its
/// parent's.
#[tokio::test]
async fn a_python_breakpoint_stops_in_a_child_interpreter() {
    for how in ["spawn", "fork"] {
        for name in SPLITS {
            let interpreter = python::interpreter(name);
            let program = Program::load(&interpreter, &DebugFileOptions::default())
                .unwrap_or_else(|error| panic!("{name}: load the interpreter: {error}"));
            let group =
                ProcessGroup::new(program, FollowPolicy::default()).expect("start the group");
            let mut events = group.subscribe();
            group
                .root()
                .add_breakpoint_with(
                    BreakpointSpec::Function("py:child_work".to_owned()),
                    BreakpointOptions {
                        pending: true,
                        ..BreakpointOptions::default()
                    },
                )
                .await
                .expect("add the breakpoint");
            let fixture = python::fixture("group_child.py");
            group
                .root()
                .launch_with(LaunchOptions {
                    arguments: vec![fixture.into_os_string(), how.into()],
                    ..LaunchOptions::default()
                })
                .await
                .expect("launch");

            let mut stopped = Vec::new();
            let mut seen = Vec::new();
            loop {
                let event = timeout(DEADLINE, events.recv())
                    .await
                    .unwrap_or_else(|_| panic!("{how} {name}: an event in time; saw {seen:#?}"))
                    .expect("events");
                seen.push(event.clone());
                match event {
                    GroupEvent::GroupStopped { member, reason, .. } => {
                        if matches!(reason, StopReason::Breakpoint { .. }) {
                            stopped.push(member);
                        }
                        group.continue_all().await.expect("continue");
                    }
                    GroupEvent::GroupExited => break,
                    _ => {}
                }
            }

            // 3.13's free-threaded build takes no Python breakpoints.
            let expected: &[u32] = if name == "3.13t" { &[] } else { &[2] };
            assert_eq!(stopped, expected, "{how} {name}: {seen:#?}");
            let members = group.members();
            assert_eq!(
                members[0].state,
                MemberState::Exited(microscope::ExitStatus::Code(0)),
                "{how} {name}: {members:#?}"
            );
            timeout(DEADLINE, group.shutdown())
                .await
                .expect("shut down in time")
                .expect("shut down");
        }
    }
}

/// A Python breakpoint stops in the workers of a multiprocessing pool, on
/// each version that takes Python breakpoints: workers fork from the
/// parent before 3.14, and from a forkserver interpreter, which joins at
/// its exec, from 3.14 on. The parent never runs the function.
#[tokio::test]
async fn a_python_breakpoint_stops_in_a_pools_workers() {
    for name in SPLITS {
        let interpreter = python::interpreter(name);
        let program = Program::load(&interpreter, &DebugFileOptions::default())
            .unwrap_or_else(|error| panic!("{name}: load the interpreter: {error}"));
        let group = ProcessGroup::new(program, FollowPolicy::default()).expect("start the group");
        let mut events = group.subscribe();
        group
            .root()
            .add_breakpoint_with(
                BreakpointSpec::Function("py:child_work".to_owned()),
                BreakpointOptions {
                    pending: true,
                    ..BreakpointOptions::default()
                },
            )
            .await
            .expect("add the breakpoint");
        group
            .root()
            .launch_with(LaunchOptions {
                arguments: vec![
                    python::fixture("group_child.py").into_os_string(),
                    "pool".into(),
                ],
                ..LaunchOptions::default()
            })
            .await
            .expect("launch");

        let mut stopped = Vec::new();
        let mut seen = Vec::new();
        loop {
            let event = timeout(DEADLINE, events.recv())
                .await
                .unwrap_or_else(|_| panic!("pool {name}: an event in time; saw {seen:#?}"))
                .expect("events");
            seen.push(event.clone());
            match event {
                GroupEvent::GroupStopped { member, reason, .. } => {
                    if matches!(reason, StopReason::Breakpoint { .. }) {
                        stopped.push(member);
                    }
                    group.continue_all().await.expect("continue");
                }
                GroupEvent::GroupExited => break,
                _ => {}
            }
        }

        // Each of the two calls stops in whichever worker runs it.
        if name == "3.13t" {
            assert!(stopped.is_empty(), "pool {name}: {seen:#?}");
        } else {
            assert_eq!(stopped.len(), 2, "pool {name}: {seen:#?}");
            assert!(!stopped.contains(&1), "pool {name}: {seen:#?}");
        }
        let members = group.members();
        assert_eq!(
            members[0].state,
            MemberState::Exited(microscope::ExitStatus::Code(0)),
            "pool {name}: {members:#?}"
        );
        timeout(DEADLINE, group.shutdown())
            .await
            .expect("shut down in time")
            .expect("shut down");
    }
}
