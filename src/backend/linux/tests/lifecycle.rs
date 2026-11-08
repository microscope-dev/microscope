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
type Reader = Box<dyn FnMut() -> String>;

fn reader<T: 'static>(mut receiver: oneshot::Receiver<Result<T>>) -> Reader {
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
fn send(harness: &mut WatchHarness, request: Request) -> bool {
    harness
        .controller
        .handle_message(ControllerMessage::Request(request))
}

fn launch(harness: &mut WatchHarness) -> (bool, Reader) {
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

fn attach(harness: &mut WatchHarness) -> (bool, Reader) {
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

fn kill(harness: &mut WatchHarness) -> (bool, Reader) {
    let (reply, receiver) = oneshot::channel();
    let running = send(harness, Request::Kill { reply });
    (running, reader(receiver))
}

fn shutdown(harness: &mut WatchHarness) -> (bool, Reader) {
    let (reply, receiver) = oneshot::channel();
    let running = send(harness, Request::Shutdown { reply });
    (running, reader(receiver))
}

/// The process an attach takes: the harness's own threads are 5000 on.
const ATTACHED: Pid = Pid::from_raw(4000);

/// The process a launch spawns, as the fake spawns it.
const LAUNCHED: Pid = Pid::from_raw(7000);

/// A fork child the program leaves behind.
const CHILD: Pid = Pid::from_raw(6000);

/// A harness in one phase, with the replies the phase is waiting to give
/// and the wait status that ends it.
struct Phase {
    harness: WatchHarness,
    waiting: Vec<(&'static str, Reader)>,
    end: Option<WaitEvent>,
}

/// A launched program stopped at a published stop, killed and gone.
fn idle() -> WatchHarness {
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
