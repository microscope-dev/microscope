//! Process groups of CPython programs: a child interpreter a program
//! starts is recognized as a runtime microscope models, and joins.

use std::ffi::OsString;
use std::time::Duration;

use microscope::{
    BreakpointOptions, BreakpointSpec, DebugFileOptions, FollowPolicy, GroupEvent, LaunchOptions,
    MemberState, ProcessGroup, Program, StopReason,
};
use tokio::time::timeout;

use crate::session::MATRIX;
use crate::support::python;

const DEADLINE: Duration = Duration::from_secs(60);

/// A child interpreter started as `subprocess` starts one, by vfork and
/// exec, joins the group of every interpreter in the matrix, whether its
/// runtime is in the executable or in a libpython it depends on.
#[tokio::test]
async fn a_child_interpreter_joins_its_parents_group() {
    for name in MATRIX {
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
                GroupEvent::GroupStopped { member, reason } => {
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

/// A Python breakpoint stops in a child interpreter of every version in the
/// matrix that takes Python breakpoints, 3.12 and 3.13 through the launch's
/// bootstrap, whether the child is a new interpreter `subprocess` started or a
/// fork of its parent's.
#[tokio::test]
async fn a_python_breakpoint_stops_in_a_child_interpreter() {
    for how in ["spawn", "fork"] {
        for name in MATRIX {
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
                    GroupEvent::GroupStopped { member, reason } => {
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
