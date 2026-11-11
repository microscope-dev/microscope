//! A model of a session's life over random sequences of a client's
//! lifecycle requests and the kernel's statuses for one traced process at
//! a time and one fork child. The reference says what every reply should
//! say after each step, and whether the controller should still run. The
//! test drives the real controller through its messages and checks both,
//! then ends the session and checks that nothing is left unanswered.

use proptest::prelude::*;
use proptest_state_machine::{ReferenceStateMachine, StateMachineTest, prop_state_machine};

use super::lifecycle::{
    ATTACHED, CHILD, LAUNCHED, Reader, attach, idle, kill, launch, reader, send, shutdown,
};
use super::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Launched,
    Attached,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Step {
    Launch,
    Attach,
    Kill,
    Shutdown,

    /// Continues an attached process stopped at its attach.
    Continue,

    /// An attach to a process none of whose threads can be seized.
    AttachRefused,

    /// A kill whose signal the kernel refuses.
    KillRefused,

    /// A shutdown that kills a launched process, and whose signal the
    /// kernel refuses.
    ShutdownKillRefused,

    /// The process's first stop: a launch's exec, or an attach's
    /// interrupt.
    FirstStop,

    /// The stop a shutdown's interrupt makes of an attached process that
    /// runs.
    InterruptStop,

    /// The process exits on its own.
    Exit,

    /// SIGKILL ends the process.
    Death,

    /// The process forks the child.
    Fork,

    /// The fork child's first stop.
    ChildStop,

    /// Nothing, once the controller has ended.
    Nothing,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Run {
    /// Before its first stop.
    Starting,
    Running,
    Stopped,
}

/// The traced process, as the client and the kernel have left it.
#[derive(Debug, Clone)]
struct Process {
    kind: Kind,
    run: Run,

    /// Whether SIGKILL was sent, so that its death is the next status.
    killed: bool,

    /// Whether a shutdown interrupted it to detach.
    interrupted: bool,

    /// The launch or attach waiting for the first stop, by reply number.
    startup: Option<usize>,
    kill: Option<usize>,
}

/// The session as the rules say it should be.
#[derive(Debug, Clone)]
struct Model {
    process: Option<Process>,

    /// Whether the fork child is due its first stop, whether or not its
    /// parent still lives.
    child: bool,

    /// A launch or attach held until the child is released.
    held: Option<(Kind, usize)>,
    shutdown: bool,
    shutdown_reply: Option<usize>,
    ended: bool,

    /// How the last process ended, as a snapshot should say until the
    /// next is traced.
    last: &'static str,

    /// What each reply should say now, in the order they were asked.
    replies: Vec<String>,
}

const PENDING: &str = "pending";

/// The error the kernel refuses a signal with.
const REFUSED: Errno = Errno::EPERM;

fn refused() -> String {
    format!("{:?}", backend_error(LinuxError::System(REFUSED)))
}

fn exited_before_stop(status: ExitStatus) -> String {
    format!("{:?}", backend_error(LinuxError::ExitedBeforeStop(status)))
}

impl Model {
    /// The steps that can happen next.
    fn steps(&self) -> Vec<Step> {
        if self.ended {
            return vec![Step::Nothing];
        }

        let mut steps = vec![Step::Launch, Step::Attach, Step::Kill, Step::Shutdown];
        if self.process.is_none() && !self.child {
            steps.push(Step::AttachRefused);
        }
        if let Some(process) = &self.process {
            if process.kill.is_none() {
                steps.push(Step::KillRefused);
            }
            if process.kind == Kind::Launched && !self.shutdown {
                steps.push(Step::ShutdownKillRefused);
            }
            if process.killed {
                steps.push(Step::Death);
            } else if process.run == Run::Starting {
                steps.extend([Step::FirstStop, Step::Exit]);
            } else if process.run == Run::Stopped {
                if !self.shutdown {
                    steps.push(Step::Continue);
                }
            } else {
                steps.push(Step::Exit);
                if process.interrupted {
                    steps.push(Step::InterruptStop);
                }
                if !self.child && !self.shutdown {
                    steps.push(Step::Fork);
                }
            }
        }
        if self.child {
            steps.push(Step::ChildStop);
        }
        steps
    }

    fn ask(&mut self, answer: &str) -> usize {
        self.replies.push(answer.to_owned());
        self.replies.len() - 1
    }

    fn answer(&mut self, reply: Option<usize>, answer: &str) {
        if let Some(reply) = reply {
            assert_eq!(self.replies[reply], PENDING, "reply {reply} answered twice");
            self.replies[reply] = answer.to_owned();
        }
    }

    /// What a snapshot should say of the program.
    fn inferior(&self) -> &'static str {
        if self.process.is_some() {
            "traced"
        } else {
            self.last
        }
    }

    fn start(&mut self, kind: Kind, reply: usize) {
        self.process = Some(Process {
            kind,
            run: Run::Starting,
            killed: false,
            interrupted: false,
            startup: Some(reply),
            kill: None,
        });
    }

    /// Detaches an attached process under shutdown once it is stopped and
    /// its child released, which ends the controller.
    fn detach_if_ready(&mut self) {
        let Some(process) = &self.process else {
            return;
        };
        if !(self.shutdown
            && process.kind == Kind::Attached
            && process.run == Run::Stopped
            && !self.child)
        {
            return;
        }

        let kill = process.kill;
        self.answer(kill, "ok");
        let reply = self.shutdown_reply.take();
        self.answer(reply, "ok");
        self.process = None;
        self.last = "detached";
        self.ended = true;
    }

    /// The process ended with `status`.
    fn ended_with(&mut self, status: ExitStatus) {
        let Some(process) = self.process.take() else {
            return;
        };
        self.last = match status {
            ExitStatus::Code(_) => "exited",
            ExitStatus::Terminated(_) => "killed",
        };
        self.answer(process.startup, &exited_before_stop(status));
        if self.shutdown {
            let reply = self.shutdown_reply.take();
            self.answer(reply, "ok");
        }
        self.answer(process.kill, "ok");
        if self.shutdown && !self.child {
            self.ended = true;
        }
    }

    /// A launch or attach waits for its first stop, or for the child to
    /// be released, unless something holds its place.
    fn ask_start(&mut self, kind: Kind) {
        if self.process.is_some() {
            self.ask("AlreadyRunning");
        } else if self.child {
            if self.shutdown || self.held.is_some() {
                self.ask("AlreadyRunning");
            } else {
                let reply = self.ask(PENDING);
                self.held = Some((kind, reply));
            }
        } else {
            let reply = self.ask(PENDING);
            self.start(kind, reply);
        }
    }

    fn ask_kill(&mut self) {
        match &self.process {
            None => {
                self.ask("NotRunning");
            }
            Some(Process { kill: Some(_), .. }) => {
                self.ask("RequestCancelled");
            }
            Some(_) => {
                let reply = self.ask(PENDING);
                if let Some(process) = &mut self.process {
                    process.kill = Some(reply);
                    process.killed = true;
                }
            }
        }
    }

    fn begin_shutdown(&mut self, reply: usize, kill_refused: bool) {
        self.shutdown = true;
        self.shutdown_reply = Some(reply);
        if let Some((_, held)) = self.held.take() {
            self.answer(Some(held), "RequestCancelled");
        }

        let Some(process) = &mut self.process else {
            if !self.child {
                self.answer(Some(reply), "ok");
                self.shutdown_reply = None;
                self.ended = true;
            }
            return;
        };

        let startup = process.startup.take();
        match process.kind {
            // A refused kill fails the shutdown, which leaves the process
            // to be killed at its next stop.
            Kind::Launched if kill_refused => {
                self.answer(startup, "RequestCancelled");
                self.shutdown_reply = None;
                self.answer(Some(reply), &refused());
                return;
            }
            Kind::Launched => process.killed = true,
            Kind::Attached => {
                if process.run != Run::Stopped {
                    process.interrupted = true;
                }
            }
        }
        self.answer(startup, "RequestCancelled");
        self.detach_if_ready();
    }

    fn apply(&mut self, step: Step) {
        match step {
            Step::Nothing => {}
            Step::Launch => self.ask_start(Kind::Launched),
            Step::Attach => self.ask_start(Kind::Attached),
            Step::Kill => self.ask_kill(),
            Step::Shutdown => {
                if !self.shutdown {
                    let reply = self.ask(PENDING);
                    self.begin_shutdown(reply, false);
                } else if self.shutdown_reply.is_none() {
                    // The client's first shutdown was answered as the
                    // process ended; this one waits for the child.
                    let reply = self.ask(PENDING);
                    self.shutdown_reply = Some(reply);
                } else {
                    self.ask("RequestCancelled");
                }
            }
            Step::AttachRefused => {
                self.ask("NotRunning");
            }
            Step::KillRefused => {
                self.ask(&refused());
            }
            Step::ShutdownKillRefused => {
                let reply = self.ask(PENDING);
                self.begin_shutdown(reply, true);
            }
            Step::Continue => {
                self.ask("ok");
                if let Some(process) = &mut self.process {
                    process.run = Run::Running;
                }
            }
            Step::FirstStop => {
                let Some(process) = &mut self.process else {
                    return;
                };

                // A shutdown kills a launched process at each of its stops.
                if self.shutdown && process.kind == Kind::Launched {
                    process.killed = true;
                    return;
                }
                process.run = match process.kind {
                    Kind::Launched => Run::Running,
                    Kind::Attached => Run::Stopped,
                };
                process.interrupted = false;
                let startup = process.startup.take();
                self.answer(startup, "ok");
                self.detach_if_ready();
            }
            Step::InterruptStop => {
                if let Some(process) = &mut self.process {
                    process.run = Run::Stopped;
                    process.interrupted = false;
                }
                self.detach_if_ready();
            }
            Step::Exit => self.ended_with(ExitStatus::Code(0)),
            Step::Death => {
                self.ended_with(ExitStatus::Terminated(exception_info(Signal::SIGKILL)));
            }
            Step::Fork => self.child = true,
            Step::ChildStop => {
                self.child = false;
                if self.process.is_some() {
                    self.detach_if_ready();
                } else if self.shutdown {
                    let reply = self.shutdown_reply.take();
                    self.answer(reply, "ok");
                    self.ended = true;
                } else if let Some((kind, reply)) = self.held.take() {
                    self.start(kind, reply);
                }
            }
        }
    }
}

impl ReferenceStateMachine for Model {
    type State = Self;
    type Transition = Step;

    fn init_state() -> BoxedStrategy<Self> {
        Just(Self {
            process: None,
            child: false,
            held: None,
            shutdown: false,
            shutdown_reply: None,
            ended: false,

            // The idle harness killed the process it began with.
            last: "killed",
            replies: Vec::new(),
        })
        .boxed()
    }

    /// Favors the kernel moving the session on over the client ending
    /// it, so that sequences reach forks, held starts, and detaches.
    fn transitions(state: &Self) -> BoxedStrategy<Step> {
        let weight = |step: Step| match step {
            Step::Kill
            | Step::Shutdown
            | Step::Exit
            | Step::AttachRefused
            | Step::KillRefused
            | Step::ShutdownKillRefused => 1,
            Step::ChildStop => 2,
            Step::Launch | Step::Attach | Step::Continue | Step::InterruptStop | Step::Death => 3,
            Step::FirstStop | Step::Fork | Step::Nothing => 4,
        };
        prop::strategy::Union::new_weighted(
            state
                .steps()
                .into_iter()
                .map(|step| (weight(step), Just(step).boxed()))
                .collect(),
        )
        .boxed()
    }

    fn preconditions(state: &Self, step: &Step) -> bool {
        state.steps().contains(step)
    }

    fn apply(mut state: Self, step: &Step) -> Self {
        state.apply(*step);
        state
    }
}

/// The controller, the replies it was asked for, and the model as it was
/// before the step under way, which says which process a status is for.
struct Session {
    harness: WatchHarness,
    replies: Vec<Reader>,
    before: Model,
    running: bool,
}

impl Session {
    fn pid(&self) -> Pid {
        match self.before.process.as_ref().map(|process| process.kind) {
            Some(Kind::Attached) => ATTACHED,
            _ => LAUNCHED,
        }
    }

    fn wait(&mut self, status: WaitEvent) -> bool {
        self.harness.controller.handle_wait(status)
    }

    fn ask(&mut self, (running, reply): (bool, Reader)) -> bool {
        self.replies.push(reply);
        running
    }

    fn step(&mut self, step: Step) {
        if step == Step::Nothing {
            return;
        }

        let pid = self.pid();
        self.running = match step {
            Step::Nothing => self.running,
            Step::Launch => {
                let asked = launch(&mut self.harness);
                self.ask(asked)
            }
            Step::Attach => {
                let asked = attach(&mut self.harness);
                self.ask(asked)
            }
            Step::Kill => {
                let asked = kill(&mut self.harness);
                self.ask(asked)
            }
            Step::Shutdown => {
                let asked = shutdown(&mut self.harness);
                self.ask(asked)
            }
            Step::AttachRefused => {
                self.harness
                    .trace()
                    .unseizable
                    .borrow_mut()
                    .insert(ATTACHED);
                let asked = attach(&mut self.harness);
                self.harness.trace().unseizable.borrow_mut().clear();
                self.ask(asked)
            }
            Step::KillRefused | Step::ShutdownKillRefused => {
                self.harness
                    .trace()
                    .fail_point
                    .replace(Some(("kill", 0, REFUSED)));
                let asked = if step == Step::KillRefused {
                    kill(&mut self.harness)
                } else {
                    shutdown(&mut self.harness)
                };
                assert!(
                    self.harness.trace().fail_point.borrow().is_none(),
                    "{step:?} sent no signal"
                );
                self.ask(asked)
            }
            Step::Continue => {
                let (reply, receiver) = tokio::sync::oneshot::channel();
                let stop_id = self.harness.stop_id();
                let running = send(
                    &mut self.harness,
                    Request::Continue {
                        process_id: process_id(pid),
                        stop_id,
                        scope: ResumeScope::Process(process_id(pid)),
                        exception: ExceptionDisposition::Pass,
                        reply,
                    },
                );
                self.ask((running, reader(receiver)))
            }
            Step::FirstStop if pid == LAUNCHED => {
                self.wait(WaitEvent::Stopped(pid, Signal::SIGTRAP))
            }
            Step::FirstStop | Step::InterruptStop => self.wait(WaitEvent::PtraceEvent(
                pid,
                Signal::SIGTRAP,
                libc::PTRACE_EVENT_STOP,
            )),
            Step::Exit => self.wait(WaitEvent::Exited(pid, 0)),
            Step::Death => self.wait(WaitEvent::Signaled(pid, Signal::SIGKILL, false)),
            Step::Fork => {
                self.harness.trace().clone.replace(Some((CHILD, CHILD)));
                let running = self.wait(WaitEvent::PtraceEvent(
                    pid,
                    Signal::SIGTRAP,
                    libc::PTRACE_EVENT_FORK,
                ));
                self.harness.trace().clone.replace(None);
                running
            }
            Step::ChildStop => self.wait(WaitEvent::Stopped(CHILD, Signal::SIGSTOP)),
        };
    }

    /// Checks the controller against the model after `step`.
    fn check(&mut self, model: &Model, step: Step) {
        let said: Vec<String> = self.replies.iter_mut().map(|read| read()).collect();
        assert_eq!(said, model.replies, "the replies after {step:?}");
        let inferior = match self.harness.controller.snapshot().inferior {
            InferiorState::Running { .. } | InferiorState::Stopped { .. } => "traced",
            InferiorState::NotStarted => "not started",
            InferiorState::Exited {
                status: ExitStatus::Code(0),
                ..
            } => "exited",
            InferiorState::Exited {
                status: ExitStatus::Terminated(info),
                ..
            } if info.code == 9 => "killed",
            InferiorState::Exited { .. } => "exited otherwise",
            InferiorState::Detached { .. } => "detached",
        };
        assert_eq!(inferior, model.inferior(), "the inferior after {step:?}");
        assert_eq!(
            self.running, !model.ended,
            "whether the controller runs after {step:?}"
        );
    }
}

impl StateMachineTest for Session {
    type SystemUnderTest = Self;
    type Reference = Model;

    fn init_test(model: &Model) -> Self {
        Self {
            harness: idle(),
            replies: Vec::new(),
            before: model.clone(),
            running: true,
        }
    }

    fn apply(mut session: Self, model: &Model, step: Step) -> Self {
        session.step(step);
        session.check(model, step);
        session.before = model.clone();
        session
    }

    /// Shuts the session down and lets the kernel finish what it owes,
    /// after which the controller has ended and answered every reply.
    fn teardown(mut session: Self, model: Model) {
        let mut model = model;
        for _ in 0..16 {
            if model.ended {
                break;
            }

            let steps = model.steps();
            let step = [
                Step::Shutdown,
                Step::ChildStop,
                Step::FirstStop,
                Step::InterruptStop,
                Step::Death,
                Step::Exit,
            ]
            .into_iter()
            .find(|step| steps.contains(step) && (*step != Step::Shutdown || !model.shutdown))
            .unwrap_or(Step::Nothing);
            model.apply(step);
            session.step(step);
            session.check(&model, step);
            session.before = model.clone();
        }

        assert!(model.ended, "the session never ended: {model:?}");
        assert!(
            !model.replies.iter().any(|said| said == PENDING),
            "replies left unanswered: {:?}",
            model.replies
        );
    }
}

/// Runs `steps` against the model and the controller, then ends the
/// session, checking both after each step.
fn replay(steps: &[Step]) {
    let mut model = <Model as ReferenceStateMachine>::init_state()
        .new_tree(&mut proptest::test_runner::TestRunner::deterministic())
        .expect("the initial state")
        .current();
    let mut session = <Session as StateMachineTest>::init_test(&model);
    for &step in steps {
        assert!(
            model.steps().contains(&step),
            "{step:?} cannot happen in {model:?}"
        );
        model.apply(step);
        session = <Session as StateMachineTest>::apply(session, &model, step);
    }
    <Session as StateMachineTest>::teardown(session, model);
}

/// A process that exits while its fork child is due, under a shutdown,
/// still says how it ended once the child is released. Found by the
/// model: the drain's end dropped it, so the snapshot said nothing ever
/// started.
#[test]
fn a_drained_session_says_how_its_program_ended() {
    replay(&[
        Step::Launch,
        Step::Kill,
        Step::Death,
        Step::Launch,
        Step::Exit,
        Step::Launch,
        Step::FirstStop,
        Step::Fork,
        Step::Exit,
    ]);
}

prop_state_machine! {
    #![proptest_config(ProptestConfig {
        cases: 1024,
        ..ProptestConfig::default()
    })]

    #[test]
    fn every_lifecycle_request_is_answered_as_the_session_rules_say(sequential 1..32 => Session);
}
