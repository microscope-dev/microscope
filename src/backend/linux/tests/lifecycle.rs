//! What each lifecycle request does in each phase of a session: idle,
//! starting, stopped, running, killing, ending, and releasing the children
//! of a program that exited. Every phase is reached and every request made
//! through the controller's messages and wait statuses, so the table holds
//! however the controller keeps its phase.

use tokio::sync::oneshot;

use super::*;

/// What a reply said when it was read: `pending` while unanswered,
/// `dropped` if the controller dropped it, `ok`, or the error.
fn answer<T>(receiver: &mut oneshot::Receiver<Result<T>>) -> String {
    match receiver.try_recv() {
        Ok(Ok(_)) => "ok".to_owned(),
        Ok(Err(error)) => format!("{error:?}"),
        Err(oneshot::error::TryRecvError::Empty) => "pending".to_owned(),
        Err(oneshot::error::TryRecvError::Closed) => "dropped".to_owned(),
    }
}

/// A reply, whatever it answers with, which says the same once answered.
pub(super) type Reader = Box<dyn FnMut() -> String>;

pub(super) fn reader<T: 'static>(mut receiver: oneshot::Receiver<Result<T>>) -> Reader {
    let mut answered: Option<String> = None;
    Box::new(move || {
        if let Some(said) = &answered {
            return said.clone();
        }

        let said = answer(&mut receiver);
        if said != "pending" {
            answered = Some(said.clone());
        }
        said
    })
}

/// Sends `request` and returns whether the controller keeps running, with
/// the request's reply.
pub(super) fn send(harness: &mut WatchHarness, request: Request) -> bool {
    harness
        .controller
        .handle_message(ControllerMessage::Request(request))
}

pub(super) fn launch(harness: &mut WatchHarness) -> (bool, Reader) {
    let (reply, receiver) = oneshot::channel();
    let running = send(
        harness,
        Request::Launch {
            options: Box::default(),
            reply,
        },
    );
    (running, reader(receiver))
}

pub(super) fn attach(harness: &mut WatchHarness) -> (bool, Reader) {
    let (reply, receiver) = oneshot::channel();
    harness.trace().listed_threads.replace(vec![ATTACHED]);
    let running = send(
        harness,
        Request::Attach {
            process_id: process_id(ATTACHED),
            observe: false,
            reply,
        },
    );
    (running, reader(receiver))
}

pub(super) fn kill(harness: &mut WatchHarness) -> (bool, Reader) {
    let (reply, receiver) = oneshot::channel();
    let running = send(harness, Request::Kill { reply });
    (running, reader(receiver))
}

pub(super) fn shutdown(harness: &mut WatchHarness) -> (bool, Reader) {
    let (reply, receiver) = oneshot::channel();
    let running = send(harness, Request::Shutdown { reply });
    (running, reader(receiver))
}

/// The process an attach takes: the harness's own threads are 5000 on.
pub(super) const ATTACHED: Pid = Pid::from_raw(4000);

/// The process a launch spawns, as the fake spawns it.
pub(super) const LAUNCHED: Pid = Pid::from_raw(7000);

/// A fork child the program leaves behind.
pub(super) const CHILD: Pid = Pid::from_raw(6000);

/// A harness in one phase, with the replies the phase is waiting to give
/// and the wait status that ends it.
struct Phase {
    harness: WatchHarness,
    waiting: Vec<(&'static str, Reader)>,
    end: Option<WaitEvent>,
}

/// A launched program stopped at a published stop, killed and gone.
pub(super) fn idle() -> WatchHarness {
    let mut harness = watch_harness(1);
    let pid = harness.threads[0];
    let (_, mut killed) = kill(&mut harness);
    harness
        .controller
        .handle_wait(WaitEvent::Signaled(pid, Signal::SIGKILL, false));
    assert_eq!(killed(), "ok", "the setup's kill");
    harness.trace().take_actions();
    harness.published();
    harness
}

/// Builds the phase `name`.
fn phase(name: &str) -> Phase {
    let mut waiting = Vec::new();
    let (harness, end) = match name {
        "idle" => (idle(), None),
        "launching" => {
            let mut harness = idle();
            let (_, launched) = launch(&mut harness);
            waiting.push(("launch", launched));
            (
                harness,
                Some(WaitEvent::Signaled(LAUNCHED, Signal::SIGKILL, false)),
            )
        }
        "attaching" => {
            let mut harness = idle();
            let (_, attached) = attach(&mut harness);
            waiting.push(("attach", attached));
            (
                harness,
                Some(WaitEvent::Signaled(ATTACHED, Signal::SIGKILL, false)),
            )
        }
        "stopped" | "stopped attached" => {
            let mut harness = watch_harness(1);
            if name == "stopped attached" {
                harness.attached();
            }
            let pid = harness.threads[0];
            (
                harness,
                Some(WaitEvent::Signaled(pid, Signal::SIGKILL, false)),
            )
        }
        "running" => {
            let mut harness = watch_harness(1);
            harness.start_continue();
            let pid = harness.threads[0];
            (
                harness,
                Some(WaitEvent::Signaled(pid, Signal::SIGKILL, false)),
            )
        }
        "killing" => {
            let mut harness = watch_harness(1);
            let (_, killed) = kill(&mut harness);
            waiting.push(("kill", killed));
            let pid = harness.threads[0];
            (
                harness,
                Some(WaitEvent::Signaled(pid, Signal::SIGKILL, false)),
            )
        }
        "detaching" => {
            // An attached program that runs is detached once it stops.
            let mut harness = watch_harness(1);
            harness.attached();
            harness.start_continue();
            let (_, ended) = shutdown(&mut harness);
            waiting.push(("shutdown", ended));
            let pid = harness.threads[0];
            (
                harness,
                Some(WaitEvent::Signaled(pid, Signal::SIGKILL, false)),
            )
        }
        "releasing" | "releasing, launch held" => {
            // The program exited, leaving a fork child on its way to its
            // first stop, which releases it.
            let mut harness = hit_harness(1, ">=1");
            let parent = harness.threads[0];
            harness.trace().clone.replace(Some((CHILD, CHILD)));
            for status in [
                WaitEvent::PtraceEvent(parent, Signal::SIGTRAP, libc::PTRACE_EVENT_FORK),
                WaitEvent::PtraceEvent(parent, Signal::SIGTRAP, libc::PTRACE_EVENT_EXIT),
                WaitEvent::Exited(parent, 0),
            ] {
                harness.controller.handle_wait(status);
            }
            harness.trace().clone.replace(None);
            if name == "releasing, launch held" {
                let (_, launched) = launch(&mut harness);
                waiting.push(("launch", launched));
            }
            (harness, Some(WaitEvent::Stopped(CHILD, Signal::SIGSTOP)))
        }
        _ => unreachable!("no phase {name}"),
    };

    let mut phase = Phase {
        harness,
        waiting,
        end,
    };
    phase.harness.trace().take_actions();
    phase.harness.published();
    phase
}

const PHASES: [&str; 10] = [
    "idle",
    "launching",
    "attaching",
    "stopped",
    "stopped attached",
    "running",
    "killing",
    "detaching",
    "releasing",
    "releasing, launch held",
];

/// One request in one phase: whether the controller kept running after
/// it, what the request was answered, and what each reply the phase was
/// waiting to give says then.
fn outcome(name: &str, request: &str) -> String {
    let mut phase = phase(name);
    let harness = &mut phase.harness;
    let (running, mut asked) = match request {
        "launch" => launch(harness),
        "attach" => attach(harness),
        "kill" => kill(harness),
        "shutdown" => shutdown(harness),
        "end" => {
            let running = phase
                .end
                .take()
                .is_none_or(|status| harness.controller.handle_wait(status));
            (running, Box::new(|| "-".to_owned()) as Reader)
        }
        _ => unreachable!("no request {request}"),
    };

    let state = |asked: &mut Reader, running: bool, waiting: &mut Vec<(&str, Reader)>| {
        std::iter::once(format!(
            "{} -> {}",
            asked(),
            if running { "runs" } else { "ends" }
        ))
        .chain(
            waiting
                .iter_mut()
                .map(|(name, read)| format!("{name} {}", read())),
        )
        .collect::<Vec<_>>()
        .join(", ")
    };
    let mut line = format!(
        "{name} / {request}: {}",
        state(&mut asked, running, &mut phase.waiting)
    );

    // The phase ends as it would have, and the replies its end answers
    // are answered then.
    if let (true, Some(status)) = (running, phase.end.take()) {
        let running = harness.controller.handle_wait(status);
        line = format!(
            "{line}; then end: {}",
            state(&mut asked, running, &mut phase.waiting)
        );
    }

    line
}

/// Every lifecycle request in every phase answers as it always has. A
/// reply is never dropped while the controller runs, and the controller
/// ends only once a shutdown leaves nothing to release.
#[test]
fn each_lifecycle_request_is_answered_in_each_phase() {
    let actual: Vec<String> = PHASES
        .iter()
        .flat_map(|name| {
            ["launch", "attach", "kill", "shutdown", "end"]
                .iter()
                .map(move |request| outcome(name, request))
        })
        .collect();

    let expected = [
        "idle / launch: pending -> runs",
        "idle / attach: pending -> runs",
        "idle / kill: NotRunning -> runs",
        "idle / shutdown: ok -> ends",
        "idle / end: - -> runs",
        r#"launching / launch: AlreadyRunning -> runs, launch pending; then end: AlreadyRunning -> runs, launch Backend(ExitedBeforeStop(Terminated(ExceptionInfo { code: 9, description: "SIGKILL" })))"#,
        r#"launching / attach: AlreadyRunning -> runs, launch pending; then end: AlreadyRunning -> runs, launch Backend(ExitedBeforeStop(Terminated(ExceptionInfo { code: 9, description: "SIGKILL" })))"#,
        r#"launching / kill: pending -> runs, launch pending; then end: ok -> runs, launch Backend(ExitedBeforeStop(Terminated(ExceptionInfo { code: 9, description: "SIGKILL" })))"#,
        "launching / shutdown: pending -> runs, launch RequestCancelled; then end: ok -> ends, launch RequestCancelled",
        r#"launching / end: - -> runs, launch Backend(ExitedBeforeStop(Terminated(ExceptionInfo { code: 9, description: "SIGKILL" })))"#,
        r#"attaching / launch: AlreadyRunning -> runs, attach pending; then end: AlreadyRunning -> runs, attach Backend(ExitedBeforeStop(Terminated(ExceptionInfo { code: 9, description: "SIGKILL" })))"#,
        r#"attaching / attach: AlreadyRunning -> runs, attach pending; then end: AlreadyRunning -> runs, attach Backend(ExitedBeforeStop(Terminated(ExceptionInfo { code: 9, description: "SIGKILL" })))"#,
        r#"attaching / kill: pending -> runs, attach pending; then end: ok -> runs, attach Backend(ExitedBeforeStop(Terminated(ExceptionInfo { code: 9, description: "SIGKILL" })))"#,
        "attaching / shutdown: pending -> runs, attach RequestCancelled; then end: ok -> ends, attach RequestCancelled",
        r#"attaching / end: - -> runs, attach Backend(ExitedBeforeStop(Terminated(ExceptionInfo { code: 9, description: "SIGKILL" })))"#,
        "stopped / launch: AlreadyRunning -> runs; then end: AlreadyRunning -> runs",
        "stopped / attach: AlreadyRunning -> runs; then end: AlreadyRunning -> runs",
        "stopped / kill: pending -> runs; then end: ok -> runs",
        "stopped / shutdown: pending -> runs; then end: ok -> ends",
        "stopped / end: - -> runs",
        "stopped attached / launch: AlreadyRunning -> runs; then end: AlreadyRunning -> runs",
        "stopped attached / attach: AlreadyRunning -> runs; then end: AlreadyRunning -> runs",
        "stopped attached / kill: pending -> runs; then end: ok -> runs",
        "stopped attached / shutdown: ok -> ends",
        "stopped attached / end: - -> runs",
        "running / launch: AlreadyRunning -> runs; then end: AlreadyRunning -> runs",
        "running / attach: AlreadyRunning -> runs; then end: AlreadyRunning -> runs",
        "running / kill: pending -> runs; then end: ok -> runs",
        "running / shutdown: pending -> runs; then end: ok -> ends",
        "running / end: - -> runs",
        "killing / launch: AlreadyRunning -> runs, kill pending; then end: AlreadyRunning -> runs, kill ok",
        "killing / attach: AlreadyRunning -> runs, kill pending; then end: AlreadyRunning -> runs, kill ok",
        "killing / kill: RequestCancelled -> runs, kill pending; then end: RequestCancelled -> runs, kill ok",
        "killing / shutdown: pending -> runs, kill pending; then end: ok -> ends, kill ok",
        "killing / end: - -> runs, kill ok",
        "detaching / launch: AlreadyRunning -> runs, shutdown pending; then end: AlreadyRunning -> ends, shutdown ok",
        "detaching / attach: AlreadyRunning -> runs, shutdown pending; then end: AlreadyRunning -> ends, shutdown ok",
        "detaching / kill: pending -> runs, shutdown pending; then end: ok -> ends, shutdown ok",
        "detaching / shutdown: RequestCancelled -> runs, shutdown pending; then end: RequestCancelled -> ends, shutdown ok",
        "detaching / end: - -> ends, shutdown ok",
        "releasing / launch: pending -> runs; then end: pending -> runs",
        "releasing / attach: pending -> runs; then end: pending -> runs",
        "releasing / kill: NotRunning -> runs; then end: NotRunning -> runs",
        "releasing / shutdown: pending -> runs; then end: ok -> ends",
        "releasing / end: - -> runs",
        "releasing, launch held / launch: AlreadyRunning -> runs, launch pending; then end: AlreadyRunning -> runs, launch pending",
        "releasing, launch held / attach: AlreadyRunning -> runs, launch pending; then end: AlreadyRunning -> runs, launch pending",
        "releasing, launch held / kill: NotRunning -> runs, launch pending; then end: NotRunning -> runs, launch pending",
        "releasing, launch held / shutdown: pending -> runs, launch RequestCancelled; then end: ok -> ends, launch RequestCancelled",
        "releasing, launch held / end: - -> runs, launch pending",
    ];
    assert_eq!(actual, expected, "\n{}", actual.join("\n"));
}

/// A session attached to `pid`, which is the test's own process when the
/// test needs `/proc` to name its program, stopped at the attach.
fn attached_to(pid: Pid) -> WatchHarness {
    let mut harness = idle();
    harness.trace().listed_threads.replace(vec![pid]);
    let (reply, receiver) = oneshot::channel();
    send(
        &mut harness,
        Request::Attach {
            process_id: process_id(pid),
            observe: false,
            reply,
        },
    );
    let mut attached = reader(receiver);
    harness.controller.handle_wait(WaitEvent::PtraceEvent(
        pid,
        Signal::SIGTRAP,
        libc::PTRACE_EVENT_STOP,
    ));
    assert_eq!(attached(), "ok", "the setup's attach");
    harness.trace().take_actions();
    harness.published();
    harness
}

/// A group member that executes a program the group only watches leaves
/// the session at that exec. A kill sent before it is answered then, as
/// the session ends, rather than left waiting with nothing to answer it.
#[test]
fn a_kill_waiting_when_a_member_is_handed_off_at_an_exec_is_answered() {
    let me = Pid::from_raw(i32::try_from(std::process::id()).expect("pid fits"));
    let mut harness = attached_to(me);
    let following = Arc::new(super::follow::Following::new(super::follow::Policy {
        policy: crate::FollowPolicy {
            mode: crate::FollowMode::None,
            ..crate::FollowPolicy::default()
        },
        launched: None,
    }));
    harness.controller.join_group(super::follow::Membership {
        following,
        root: false,
        parent: None,
        settings_taken: 0,
    });
    harness.start_continue();

    let (_, mut killed) = kill(&mut harness);
    assert_eq!(killed(), "pending");

    // The exec's event names the thread that executed it.
    harness.trace().clone.replace(Some((me, me)));
    assert!(harness.controller.handle_wait(WaitEvent::PtraceEvent(
        me,
        Signal::SIGTRAP,
        libc::PTRACE_EVENT_EXEC
    )));

    assert!(
        harness
            .published()
            .iter()
            .any(|event| matches!(event, DebuggerEvent::InferiorDetached { .. })),
        "the member left the session at its exec"
    );
    assert_eq!(killed(), "ok");

    // The session is over, so another can start.
    let (running, mut launched) = launch(&mut harness);
    assert!(running);
    assert_eq!(launched(), "pending");
}

/// A waiter that cannot be joined at the process's exit does not cost the
/// exit its event or the kill its answer, and leaves the session idle.
#[test]
fn a_waiter_that_fails_at_the_exit_still_publishes_it_and_answers_the_kill() {
    let mut harness = watch_harness(1);
    let pid = harness.threads[0];
    harness.inferior().waiter = Some(Waiter {
        thread: Some(WaiterThread {
            stop: Arc::new(AtomicBool::new(false)),
            // Unwinds without the panic hook, so the test prints nothing.
            handle: thread::spawn(|| std::panic::resume_unwind(Box::new(()))),
        }),
        hub: None,
    });

    let (_, mut killed) = kill(&mut harness);
    harness.published();
    assert!(
        harness
            .controller
            .handle_wait(WaitEvent::Signaled(pid, Signal::SIGKILL, false))
    );

    assert!(
        harness
            .published()
            .iter()
            .any(|event| matches!(event, DebuggerEvent::InferiorExited { .. })),
        "the exit was published"
    );
    assert_eq!(killed(), "ok");
    let (running, mut launched) = launch(&mut harness);
    assert!(running);
    assert_eq!(launched(), "pending");
}

/// What an attach is answered when each time its threads stop, the process
/// has made another it has not traced, `new_threads` times in all.
fn attach_with_threads_appearing(new_threads: i32) -> String {
    let mut harness = idle();
    let mut listed = vec![ATTACHED];
    harness.trace().listed_threads.replace(listed.clone());
    let (_, mut attached) = attach(&mut harness);

    let mut last = ATTACHED;
    for offset in 1..=new_threads {
        let thread = Pid::from_raw(ATTACHED.as_raw() + offset);
        listed.push(thread);
        harness.trace().listed_threads.replace(listed.clone());
        harness.controller.handle_wait(WaitEvent::PtraceEvent(
            last,
            Signal::SIGTRAP,
            libc::PTRACE_EVENT_STOP,
        ));
        if attached() != "pending" {
            return attached();
        }
        last = thread;
    }

    harness.controller.handle_wait(WaitEvent::PtraceEvent(
        last,
        Signal::SIGTRAP,
        libc::PTRACE_EVENT_STOP,
    ));
    attached()
}

/// An attach seizes threads that appear while it stops the others, up to
/// a limit, past which it gives up on a thread list that never settles.
#[test]
fn an_attach_gives_up_on_threads_that_keep_appearing_past_its_limit() {
    assert_eq!(attach_with_threads_appearing(0), "ok");
    assert_eq!(attach_with_threads_appearing(128), "ok");
    assert_eq!(
        attach_with_threads_appearing(129),
        "Backend(AttachThreadsUnstable)"
    );
}

/// A launch held while an exited program's fork children are released
/// starts once every child is, not once the first is.
#[test]
fn a_held_launch_waits_for_every_fork_child_to_be_released() {
    const SECOND: Pid = Pid::from_raw(6001);

    let mut harness = hit_harness(1, ">=1");
    let parent = harness.threads[0];
    for child in [CHILD, SECOND] {
        harness.trace().clone.replace(Some((child, child)));
        harness.controller.handle_wait(WaitEvent::PtraceEvent(
            parent,
            Signal::SIGTRAP,
            libc::PTRACE_EVENT_FORK,
        ));
    }
    harness.trace().clone.replace(None);
    for status in [
        WaitEvent::PtraceEvent(parent, Signal::SIGTRAP, libc::PTRACE_EVENT_EXIT),
        WaitEvent::Exited(parent, 0),
    ] {
        harness.controller.handle_wait(status);
    }

    let (_, mut launched) = launch(&mut harness);
    let launched_pid = |harness: &WatchHarness| {
        harness
            .controller
            .session
            .inferior()
            .map(|inferior| inferior.tgid)
    };

    harness
        .controller
        .handle_wait(WaitEvent::Stopped(CHILD, Signal::SIGSTOP));
    assert_eq!(
        launched_pid(&harness),
        None,
        "the second child is not released yet"
    );
    assert_eq!(launched(), "pending");

    harness
        .controller
        .handle_wait(WaitEvent::Stopped(SECOND, Signal::SIGSTOP));
    assert_eq!(launched_pid(&harness), Some(LAUNCHED));
    assert_eq!(launched(), "pending", "the launch waits for its first stop");
}

/// A shutdown detaches an attached program once every thread has stopped,
/// and passes on a signal a thread stopped for before the interrupt took it.
#[test]
fn a_shutdown_detaches_once_every_thread_stops_and_passes_on_their_signals() {
    let mut harness = watch_harness(2);
    harness.attached();
    harness.start_continue();
    let [first, second] = [harness.threads[0], harness.threads[1]];
    let (_, mut ended) = shutdown(&mut harness);
    harness.trace().take_actions();

    harness.controller.handle_wait(WaitEvent::PtraceEvent(
        first,
        Signal::SIGTRAP,
        libc::PTRACE_EVENT_STOP,
    ));
    assert_eq!(
        harness.trace().take_actions(),
        Vec::<String>::new(),
        "the second thread still runs"
    );
    assert_eq!(ended(), "pending");

    harness.trace().set_siginfo(second, libc::SI_USER, Some(1));
    harness
        .controller
        .handle_wait(WaitEvent::Stopped(second, Signal::SIGUSR1));
    let actions = harness.trace().take_actions();
    for expected in [
        format!("detach {first} None"),
        format!("detach {second} Some(SIGUSR1)"),
    ] {
        assert!(actions.contains(&expected), "{expected} in {actions:#?}");
    }
    assert_eq!(ended(), "ok");
}

/// A kill sent with a launch takes the new process out of its exec stop
/// while the controller handles it, so setting its options fails with
/// ESRCH. That the process left its stop explains the failure, as it does
/// any SIGKILL: the launch fails as one whose process exited, the kill
/// succeeds, and the debugger does not give up on the program and kill it
/// again.
#[test]
fn a_kill_that_overtakes_a_launchs_first_stop_ends_it_in_its_exit() {
    let mut harness = idle();
    let (_, mut launched) = launch(&mut harness);
    let (_, mut killed) = kill(&mut harness);
    harness.trace().take_actions();
    *harness.trace().kill_point.borrow_mut() = Some((LAUNCHED, "set_options", 0));

    for status in [
        WaitEvent::Stopped(LAUNCHED, Signal::SIGTRAP),
        WaitEvent::Signaled(LAUNCHED, Signal::SIGKILL, false),
    ] {
        assert!(harness.controller.handle_wait(status));
    }

    assert!(
        harness.trace().kill_point.borrow().is_none(),
        "the kill landed as the stop was handled"
    );
    assert_eq!(
        launched(),
        r#"Backend(ExitedBeforeStop(Terminated(ExceptionInfo { code: 9, description: "SIGKILL" })))"#
    );
    assert_eq!(killed(), "ok");
    assert!(harness.controller.session.is_idle());
    assert!(
        !harness.trace().took("kill"),
        "the debugger does not give up on its own kill"
    );
}
