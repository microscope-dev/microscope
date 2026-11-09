//! What each wait status and event does to a pid that is not a live thread
//! of the inferior: one never seen, a thread that exited before an attach
//! seized it or before its exit was collected, a stop or exit that came
//! before the event announcing it, and a fork child on its way to being
//! released, killed, or handed to the session's group.
//!
//! Every record is made through the controller's messages and wait
//! statuses, and only [`record`] reads how the controller keeps them.

use super::lifecycle::{ATTACHED, CHILD, attach, idle};
use super::*;

/// What the controller holds of `pid`, wherever it holds it.
fn record(harness: &WatchHarness, pid: Pid) -> String {
    let mut held = Vec::new();
    match &harness.controller.session {
        Session::Traced(traced) => {
            let inferior = &traced.inferior;
            if inferior.threads.contains_key(&pid) {
                held.push("live".to_owned());
            }
            match inferior.tracked.get(&pid) {
                None => {}
                Some(Tracked::Retired) => held.push("retired".to_owned()),
                Some(Tracked::Vanished) => held.push("vanished".to_owned()),
                Some(Tracked::Unseized) => held.push("unseized".to_owned()),
                Some(Tracked::EarlyStop(status)) => held.push(format!("early {status:?}")),
                Some(Tracked::ForkChild { sites, vfork }) => held.push(format!(
                    "fork child of {} sites{}",
                    sites.len(),
                    if *vfork { " vfork" } else { "" }
                )),
                Some(Tracked::Killed) => held.push("killed".to_owned()),
                Some(Tracked::HandedOver) => held.push("handed over".to_owned()),
            }
        }
        Session::Draining { orphans, .. } => {
            if let Some(sites) = orphans.children.get(&pid) {
                held.push(format!("orphan of {} sites", sites.len()));
            }
            if orphans.killed.contains(&pid) {
                held.push("killed orphan".to_owned());
            }
        }
        Session::Idle { .. } => {}
    }

    if held.is_empty() {
        "nothing".to_owned()
    } else {
        held.join(" + ")
    }
}

fn session(harness: &WatchHarness) -> &'static str {
    match &harness.controller.session {
        Session::Idle { .. } => "idle",
        Session::Draining { .. } => "draining",
        Session::Traced(traced) => match traced.inferior.run.stop() {
            Some(_) => "stopped",
            None => "running",
        },
    }
}

/// A process running one thread, `parent`, with a trap at [`HIT_SITE`]
/// that a child it forks inherits.
fn running() -> (WatchHarness, Pid) {
    let harness = hit_harness(1, ">=1");
    let parent = harness.threads[0];
    (harness, parent)
}

/// A process attached to while [`CHILD`], one of its listed threads,
/// could not be seized, or was found exiting when interrupted.
fn attached(unseizable: bool) -> (WatchHarness, Pid) {
    let mut harness = idle();
    if unseizable {
        harness.trace().unseizable.borrow_mut().insert(CHILD);
    } else {
        harness.trace().dead.borrow_mut().insert(CHILD);
    }

    let (_, mut attached) = attach(&mut harness);
    harness
        .trace()
        .listed_threads
        .replace(vec![ATTACHED, CHILD]);
    harness.controller.handle_wait(WaitEvent::PtraceEvent(
        ATTACHED,
        Signal::SIGTRAP,
        libc::PTRACE_EVENT_STOP,
    ));
    assert_eq!(attached(), "ok", "the setup's attach");
    harness.resume().expect("the setup's continue");
    (harness, ATTACHED)
}

fn event(harness: &mut WatchHarness, parent: Pid, kind: i32) {
    harness.trace().clone.replace(Some((CHILD, CHILD)));
    harness
        .controller
        .handle_wait(WaitEvent::PtraceEvent(parent, Signal::SIGTRAP, kind));
    harness.trace().clone.replace(None);
}

fn first_stop(harness: &mut WatchHarness) {
    harness
        .controller
        .handle_wait(WaitEvent::Stopped(CHILD, Signal::SIGSTOP));
}

/// Makes the record `name` of [`CHILD`], returning the harness and the
/// inferior's one thread.
fn made(name: &str) -> (WatchHarness, Pid) {
    let (mut harness, parent) = match name {
        "unseized" => attached(true),
        "found exiting at attach" => attached(false),
        _ => running(),
    };

    match name {
        "unknown" | "unseized" | "found exiting at attach" => {}
        "stopped unannounced" => first_stop(&mut harness),
        "exited unannounced" => {
            harness.controller.handle_wait(WaitEvent::Exited(CHILD, 0));
        }
        "forked" => event(&mut harness, parent, libc::PTRACE_EVENT_FORK),
        "vforked" => event(&mut harness, parent, libc::PTRACE_EVENT_VFORK),
        "killed for its traps" => {
            event(&mut harness, parent, libc::PTRACE_EVENT_FORK);
            harness.trace().memory_write_failure.replace(Some(0));
            first_stop(&mut harness);
            harness.trace().memory_write_failure.replace(None);
        }
        "handed over" => {
            harness.follow_forks(CHILD);
            event(&mut harness, parent, libc::PTRACE_EVENT_FORK);
            first_stop(&mut harness);
        }
        _ => unreachable!("no record {name}"),
    }

    harness.trace().take_actions();
    harness.published();
    (harness, parent)
}

const RECORDS: [&str; 9] = [
    "unknown",
    "unseized",
    "found exiting at attach",
    "stopped unannounced",
    "exited unannounced",
    "forked",
    "vforked",
    "killed for its traps",
    "handed over",
];

const EVENTS: [&str; 8] = [
    "stops",
    "interrupt stops",
    "reaches exit",
    "exits",
    "is killed",
    "is cloned",
    "is forked",
    "parent exits",
];

/// One event of [`CHILD`] with one record of it: where the record and the
/// session end up, and what the controller asked of the kernel.
fn outcome(name: &str, happened: &str) -> String {
    let (mut harness, parent) = made(name);
    let before = record(&harness, CHILD);
    let one = |status| vec![status];
    let statuses = match happened {
        "stops" => one(WaitEvent::Stopped(CHILD, Signal::SIGSTOP)),
        "interrupt stops" => one(WaitEvent::PtraceEvent(
            CHILD,
            Signal::SIGTRAP,
            libc::PTRACE_EVENT_STOP,
        )),
        "reaches exit" => one(WaitEvent::PtraceEvent(
            CHILD,
            Signal::SIGTRAP,
            libc::PTRACE_EVENT_EXIT,
        )),
        "exits" => one(WaitEvent::Exited(CHILD, 0)),
        "is killed" => one(WaitEvent::Signaled(CHILD, Signal::SIGKILL, false)),
        "is cloned" => {
            harness.trace().clone.replace(Some((CHILD, parent)));
            one(WaitEvent::PtraceEvent(
                parent,
                Signal::SIGTRAP,
                libc::PTRACE_EVENT_CLONE,
            ))
        }
        "is forked" => {
            harness.trace().clone.replace(Some((CHILD, CHILD)));
            one(WaitEvent::PtraceEvent(
                parent,
                Signal::SIGTRAP,
                libc::PTRACE_EVENT_FORK,
            ))
        }
        "parent exits" => {
            // The tracer still traces the child, whatever became of it.
            harness.trace().traced_children.borrow_mut().push(CHILD);
            vec![
                WaitEvent::PtraceEvent(parent, Signal::SIGTRAP, libc::PTRACE_EVENT_EXIT),
                WaitEvent::Exited(parent, 0),
            ]
        }
        _ => unreachable!("no event {happened}"),
    };

    let mut kept = true;
    for status in statuses {
        kept &= harness.controller.handle_wait(status);
    }
    harness.trace().clone.replace(None);

    let published = harness
        .published()
        .into_iter()
        .filter_map(|event| match event {
            DebuggerEvent::InferiorStopped { reason, .. } => Some(format!("stopped {reason:?}")),
            DebuggerEvent::InferiorExited { .. } => Some("exited".to_owned()),
            DebuggerEvent::ChildReleased { child, .. } => Some(format!("released {child}")),
            _ => None,
        })
        .collect::<Vec<_>>();
    let calls = harness
        .trace()
        .take_actions()
        .into_iter()
        .filter(|call| !call.starts_with("read "))
        .collect::<Vec<_>>();
    format!(
        "{name} ({before}) / {happened}: {}{} [{}] {{{}}}",
        record(&harness, CHILD),
        if kept {
            format!(", {}", session(&harness))
        } else {
            ", controller ended".to_owned()
        },
        published.join(", "),
        calls.join(", ")
    )
}

/// Each wait status and event of a pid that is not a live thread ends as
/// it always has, for each thing the controller can know of it: which
/// record it keeps after, whether the session lives on, and what it asks
/// the kernel to do with the pid and its parent.
#[test]
fn each_event_of_a_pid_that_is_not_a_live_thread_settles_the_same() {
    let actual: Vec<String> = RECORDS
        .iter()
        .flat_map(|name| EVENTS.iter().map(move |happened| outcome(name, happened)))
        .collect();

    let expected = [
        "unknown (nothing) / stops: early Stopped(Pid(6000), SIGSTOP), running [] {}",
        "unknown (nothing) / interrupt stops: early PtraceEvent(Pid(6000), SIGTRAP, 128), running [] {}",
        "unknown (nothing) / reaches exit: vanished, running [] {continue 6000 None}",
        "unknown (nothing) / exits: vanished, running [] {}",
        "unknown (nothing) / is killed: vanished, running [] {}",
        "unknown (nothing) / is cloned: live, running [] {continue 5000 None}",
        "unknown (nothing) / is forked: fork child of 1 sites, running [] {continue 5000 None}",
        "unknown (nothing) / parent exits: orphan of 1 sites, draining [exited] {continue 5000 None}",
        "unseized (unseized) / stops: early Stopped(Pid(6000), SIGSTOP), running [] {}",
        "unseized (unseized) / interrupt stops: early PtraceEvent(Pid(6000), SIGTRAP, 128), running [] {}",
        "unseized (unseized) / reaches exit: unseized, running [] {continue 6000 None}",
        "unseized (unseized) / exits: vanished, running [] {}",
        "unseized (unseized) / is killed: vanished, running [] {}",
        "unseized (unseized) / is cloned: live, running [] {continue 4000 None}",
        "unseized (unseized) / is forked: fork child of 0 sites, running [] {continue 4000 None}",
        "unseized (unseized) / parent exits: orphan of 0 sites, draining [exited] {continue 4000 None}",
        "found exiting at attach (retired) / stops: early Stopped(Pid(6000), SIGSTOP), running [] {}",
        "found exiting at attach (retired) / interrupt stops: early PtraceEvent(Pid(6000), SIGTRAP, 128), running [] {}",
        "found exiting at attach (retired) / reaches exit: retired, running [] {continue 6000 None}",
        "found exiting at attach (retired) / exits: nothing, running [] {}",
        "found exiting at attach (retired) / is killed: nothing, running [] {}",
        "found exiting at attach (retired) / is cloned: live, running [] {continue 4000 None}",
        "found exiting at attach (retired) / is forked: fork child of 0 sites, running [] {continue 4000 None}",
        "found exiting at attach (retired) / parent exits: orphan of 0 sites, draining [exited] {continue 4000 None}",
        "stopped unannounced (early Stopped(Pid(6000), SIGSTOP)) / stops: early Stopped(Pid(6000), SIGSTOP), running [] {}",
        "stopped unannounced (early Stopped(Pid(6000), SIGSTOP)) / interrupt stops: early PtraceEvent(Pid(6000), SIGTRAP, 128), running [] {}",
        "stopped unannounced (early Stopped(Pid(6000), SIGSTOP)) / reaches exit: vanished, running [] {continue 6000 None}",
        "stopped unannounced (early Stopped(Pid(6000), SIGSTOP)) / exits: vanished, running [] {}",
        "stopped unannounced (early Stopped(Pid(6000), SIGSTOP)) / is killed: vanished, running [] {}",
        "stopped unannounced (early Stopped(Pid(6000), SIGSTOP)) / is cloned: live, running [] {continue 5000 None, set_options 6000, continue 6000 None}",
        "stopped unannounced (early Stopped(Pid(6000), SIGSTOP)) / is forked: nothing, running [released 6000] {write_word 6000 0x40 0x90, detach 6000 None, continue 5000 None}",
        "stopped unannounced (early Stopped(Pid(6000), SIGSTOP)) / parent exits: nothing, idle [released 6000, exited] {write_word 6000 0x40 0x90, detach 6000 None, continue 5000 None}",
        "exited unannounced (vanished) / stops: early Stopped(Pid(6000), SIGSTOP), running [] {}",
        "exited unannounced (vanished) / interrupt stops: early PtraceEvent(Pid(6000), SIGTRAP, 128), running [] {}",
        "exited unannounced (vanished) / reaches exit: vanished, running [] {continue 6000 None}",
        "exited unannounced (vanished) / exits: vanished, running [] {}",
        "exited unannounced (vanished) / is killed: vanished, running [] {}",
        "exited unannounced (vanished) / is cloned: nothing, running [] {continue 5000 None}",
        "exited unannounced (vanished) / is forked: fork child of 1 sites, running [] {continue 5000 None}",
        "exited unannounced (vanished) / parent exits: orphan of 1 sites, draining [exited] {continue 5000 None}",
        "forked (fork child of 1 sites) / stops: nothing, running [released 6000] {write_word 6000 0x40 0x90, detach 6000 None}",
        "forked (fork child of 1 sites) / interrupt stops: nothing, running [released 6000] {write_word 6000 0x40 0x90, detach 6000 None}",
        "forked (fork child of 1 sites) / reaches exit: fork child of 1 sites, running [] {continue 6000 None}",
        "forked (fork child of 1 sites) / exits: nothing, running [] {}",
        "forked (fork child of 1 sites) / is killed: nothing, running [] {}",
        "forked (fork child of 1 sites) / is cloned: live, running [] {continue 5000 None}",
        "forked (fork child of 1 sites) / is forked: fork child of 1 sites, running [] {continue 5000 None}",
        "forked (fork child of 1 sites) / parent exits: orphan of 1 sites, draining [exited] {continue 5000 None}",
        "vforked (fork child of 0 sites vfork) / stops: nothing, running [released 6000] {detach 6000 None}",
        "vforked (fork child of 0 sites vfork) / interrupt stops: nothing, running [released 6000] {detach 6000 None}",
        "vforked (fork child of 0 sites vfork) / reaches exit: fork child of 0 sites vfork, running [] {continue 6000 None}",
        "vforked (fork child of 0 sites vfork) / exits: nothing, running [] {}",
        "vforked (fork child of 0 sites vfork) / is killed: nothing, running [] {}",
        "vforked (fork child of 0 sites vfork) / is cloned: live, running [] {}",
        "vforked (fork child of 0 sites vfork) / is forked: fork child of 1 sites, running [] {}",
        "vforked (fork child of 0 sites vfork) / parent exits: orphan of 0 sites, draining [exited] {continue 5000 None, reinstall_site 0x40}",
        "killed for its traps (killed) / stops: early Stopped(Pid(6000), SIGSTOP), running [] {}",
        "killed for its traps (killed) / interrupt stops: early PtraceEvent(Pid(6000), SIGTRAP, 128), running [] {}",
        "killed for its traps (killed) / reaches exit: killed, running [] {continue 6000 None}",
        "killed for its traps (killed) / exits: nothing, running [] {}",
        "killed for its traps (killed) / is killed: nothing, running [] {}",
        "killed for its traps (killed) / is cloned: live, running [] {continue 5000 None}",
        "killed for its traps (killed) / is forked: fork child of 1 sites, running [] {continue 5000 None}",
        "killed for its traps (killed) / parent exits: killed orphan, draining [exited] {continue 5000 None}",
        "handed over (handed over) / stops: early Stopped(Pid(6000), SIGSTOP), running [] {}",
        "handed over (handed over) / interrupt stops: early PtraceEvent(Pid(6000), SIGTRAP, 128), running [] {}",
        "handed over (handed over) / reaches exit: handed over, running [] {continue 6000 None}",
        "handed over (handed over) / exits: vanished, running [] {}",
        "handed over (handed over) / is killed: vanished, running [] {}",
        "handed over (handed over) / is cloned: live, running [] {continue 5000 None}",
        "handed over (handed over) / is forked: fork child of 1 sites, running [] {continue 5000 None}",
        "handed over (handed over) / parent exits: nothing, idle [exited] {continue 5000 None}",
    ];
    assert_eq!(actual, expected, "\n{}", actual.join("\n"));
}
