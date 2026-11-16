use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::task::{Context, Poll, Wake, Waker};

use nix::libc;
use tokio::sync::mpsc;

use super::audit::Auditor;
use super::catches;
use super::choices::{Choices, Stream, Tape};
use super::client::{Client, Observation, Script, Shared};
use super::corpus::{Corpus, Program, Run, Variant};
use super::faults::{Faults, Plan};
use super::kernel::shadow::Tracking;
use super::kernel::watching::UserWatch;
use super::kernel::{Kernel, ModelGap, Parent, State, StopKind, Tgid, Tid};
use super::machine::Machine;
use super::marks::{Mark, Marks};
use super::oracles::{self, HeardTrap};
use super::report::{Failure, Trace};
use super::samples::Sampler;
use super::schedule::{Action, Scheduler, SessionId};
use super::semantics::{self, Begun, Inspected, Judged, Unwound};
use super::swarm::{Simpler, Swarm};
use super::tracing::TraceModel;
use super::views;
use super::watches::{self, Intent};
use crate::backend::sim_edge::{
    Preemption, SimController, SimExecutable, SimLaunch, SimParts, SimWaiter,
};

use crate::{DebuggerEvent, DebuggerHandle, ProcessId};

pub const TRACER: i32 = 100;

const FIRST: SessionId = 0;

#[derive(Debug, Clone)]
pub struct Settings {
    pub keep: Option<usize>,

    pub max_steps: u64,

    pub stop_at: Option<u64>,

    pub simpler: Vec<Simpler>,

    pub tape: Option<Tape>,

    #[cfg(test)]
    pub sabotage: Option<Sabotage>,

    #[cfg(test)]
    pub muted: &'static [&'static str],
}

struct Alone {
    stop: u64,
    process: Tgid,
    thread: Tid,
    retired: BTreeMap<Tid, u64>,
}

#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sabotage {
    LosePokes,

    DeafWaiter,

    ResumeBehindTheController,

    SkipTraps,

    SkewReturnAddresses,

    LateSingleSteps,

    SkewSmallStackWords,

    FlickeringStackWords,

    SkewSmallRegisters,

    PhantomArming,

    MissWatchTraps,

    RepeatWatchTraps,

    SkipLinkedNodes,

    HideSteppedCalls,

    SkewSampledReturnAddresses,

    SkipCallStops,

    ReturnBeforeEvents,

    MisreportCallArguments,

    SkipTemporaryTraps,

    MisreportFirstCallArgument,

    PassTrapsOnDetach,

    ResumeSiblings,

    ShiftMappings,

    DropRequestsWhileEnding,

    LoseStopRequests,

    ResumeWhileGathering,

    DropKillWhileStarting,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            keep: Some(400),
            max_steps: 2_000_000,
            stop_at: None,
            simpler: Vec::new(),
            tape: None,

            #[cfg(test)]
            sabotage: None,
            #[cfg(test)]
            muted: &[],
        }
    }
}

pub struct Outcome {
    pub seed: u64,
    pub swarm: Swarm,

    pub program: String,
    pub arguments: Vec<String>,
    pub fingerprint: u64,
    pub steps: u64,
    pub failure: Option<Failure>,

    pub trace: Vec<String>,
    pub dropped: u64,

    pub state: Option<String>,

    pub marks: Marks,

    pub unfired: Option<Plan>,

    pub tape: Option<Tape>,
}

#[must_use]
pub fn run(seed: u64, corpus: &Corpus, settings: &Settings) -> Outcome {
    let mut choices = settings
        .tape
        .clone()
        .map_or_else(|| Choices::new(seed), |tape| Choices::replaying(seed, tape));
    let mut swarm = Swarm::choose(&mut choices, corpus);
    for simpler in &settings.simpler {
        simpler.apply(&mut swarm);
    }

    let program = &corpus.programs[swarm.program];
    let variant = &program.variants[swarm.variant];
    let run = &program.runs[swarm.run];
    let mut world = World::new(choices, swarm.clone(), program, variant, run, settings);
    let mut failure = None;
    let mut state = None;

    loop {
        match catch_panic(|| world.step_once()) {
            Ok(Ok(Progress::Continue)) => {}
            Ok(Ok(Progress::Finished)) => break,
            Ok(Err(mut found)) => {
                found.step = world.step();
                failure = Some(found);
                break;
            }
            Err(mut panic) => {
                panic.step = world.step();
                world.flush();
                failure = Some(panic);
                break;
            }
        }

        if settings.stop_at == Some(world.step()) {
            state = Some(world.dump());
            break;
        }

        if world.step() >= settings.max_steps {
            failure = Some(Failure {
                step: world.step(),
                ..Failure::debugger(
                    "liveness",
                    format!("still going after {} actions", world.step()),
                )
            });
            break;
        }
    }

    let failure = failure.map(|found| blame_gap(found, world.machine.kernel.borrow().gap.as_ref()));
    let (lines, dropped) = world.trace.lines();
    let faults = world.machine.faults.borrow();

    Outcome {
        seed,
        swarm,
        program: variant.name.clone(),
        arguments: run.arguments.clone(),
        fingerprint: world.trace.fingerprint(),
        steps: world.step(),
        failure,
        trace: lines.iter().cloned().collect(),
        dropped,
        state,
        marks: world.machine.marks.borrow().clone(),
        unfired: faults.plan.filter(|_| faults.fired.is_none()),
        tape: world.machine.choices.borrow().drawn().cloned(),
    }
}

fn blame_gap(failure: Failure, gap: Option<&ModelGap>) -> Failure {
    match gap {
        Some(gap) => Failure {
            step: failure.step,
            ..Failure::model_gap(gap.0.clone())
        },
        None => failure,
    }
}

enum Progress {
    Continue,
    Finished,
}

struct Woken(AtomicBool);

impl Wake for Woken {
    fn wake(self: Arc<Self>) {
        self.0.store(true, Ordering::Relaxed);
    }

    fn wake_by_ref(self: &Arc<Self>) {
        self.0.store(true, Ordering::Relaxed);
    }
}

type ClientTask = Pin<Box<dyn Future<Output = Result<(), Failure>>>>;

struct Session {
    tracer: i32,

    controller: Option<SimController>,

    client: Option<ClientTask>,

    notes: Rc<RefCell<Vec<String>>>,
    woken: Arc<Woken>,
    waker: Waker,

    shutting_down: bool,
}

impl Session {
    fn new(
        tracer: i32,
        controller: SimController,
        client: ClientTask,
        notes: Rc<RefCell<Vec<String>>>,
    ) -> Self {
        let woken = Arc::new(Woken(AtomicBool::new(true)));
        let waker = Waker::from(Arc::clone(&woken));

        Self {
            tracer,
            controller: Some(controller),
            client: Some(client),
            notes,
            woken,
            waker,
            shutting_down: false,
        }
    }
}

struct World<'a> {
    swarm: Swarm,
    program: &'a Program,
    variant: &'a Variant,
    run: &'a Run,

    machine: Rc<Machine>,

    sessions: Vec<Session>,
    shared: Shared,
    auditor: Auditor,

    arrivals: BTreeMap<Tid, Arrival>,

    stepping: Option<Begun>,

    alone: Option<Alone>,

    muted: &'static [&'static str],

    watches: Vec<Intent>,
    baselines: BTreeMap<u64, Vec<u8>>,

    watch_counts: BTreeMap<u64, u64>,

    judged_stop: Option<u64>,

    catch_reasons: catches::Reasons,

    sampler: Sampler,

    tracing: RefCell<TraceModel>,
    trace: Trace,

    #[cfg(debug_assertions)]
    capture: crate::flight_recorder::Capture,
}

impl<'a> World<'a> {
    fn new(
        mut choices: Choices,
        swarm: Swarm,
        program: &'a Program,
        variant: &'a Variant,
        run: &'a Run,
        settings: &Settings,
    ) -> Self {
        #[cfg(debug_assertions)]
        let capture = crate::flight_recorder::Capture::start();
        let mut random = [0; 16];
        choices.fill(Stream::Program, &mut random);
        let scheduler = Scheduler::new(swarm.policy, &mut choices);
        let sampler = Sampler::new(&mut choices);
        let tracing = TraceModel::default();

        let choices = Rc::new(RefCell::new(choices));
        let kernel = Rc::new(RefCell::new(Kernel::new(TRACER)));
        kernel.borrow_mut().debug_behavior = swarm.debug;
        #[cfg(test)]
        {
            kernel.borrow_mut().sabotage = settings.sabotage;
        }

        let marks = Rc::new(RefCell::new(Marks::default()));
        let shared = Shared::default();
        let attach = swarm
            .attach
            .map(|_| start_untraced(&mut kernel.borrow_mut(), variant, run, random));

        let machine = Rc::new(Machine {
            kernel: Rc::clone(&kernel),
            waiters: RefCell::new(Vec::new()),
            choices: Rc::clone(&choices),
            scheduler: RefCell::new(scheduler),
            faults: RefCell::new(Faults::new(swarm.fault)),
            marks: Rc::clone(&marks),
            killed: Rc::clone(&shared.killed),
            ending: Rc::clone(&shared.ending),
            unclean: RefCell::new(Vec::new()),
            leaked: RefCell::new(Vec::new()),
            step: Cell::new(0),
            ran: Cell::new(0),
            preempt: swarm.preempt,

            #[cfg(test)]
            sabotage: settings.sabotage,
        });
        let (controller, handle, events) =
            start_controller(&machine, variant, &swarm, random, TRACER);
        let auditor = Auditor::new(events, Rc::clone(&shared.published));

        let client = Client {
            handle,
            choices,
            marks,
            shared: shared.clone(),
            script: script(&swarm, program, variant, run, attach),
            alone: Cell::new(None),
            stepping: Cell::new(None),
            baseline: RefCell::new(None),
            disabled_watches: RefCell::new(BTreeMap::new()),
            released_while_running: Cell::new(false),
        };

        let first = Session::new(
            TRACER,
            controller,
            Box::pin(client.run()),
            Rc::clone(&shared.notes),
        );
        let mut trace = Trace::new(settings.keep);
        trace.line(format!(
            "program {} {:?}; {swarm}",
            variant.name, run.arguments
        ));

        #[cfg(test)]
        let muted = settings.muted;
        #[cfg(not(test))]
        let muted = &[];

        Self {
            muted,
            swarm,
            program,
            variant,
            run,
            machine,
            sessions: vec![first],
            shared,
            auditor,
            arrivals: BTreeMap::new(),
            stepping: None,
            alone: None,
            watches: Vec::new(),
            baselines: BTreeMap::new(),
            watch_counts: BTreeMap::new(),
            judged_stop: None,
            catch_reasons: BTreeMap::new(),
            sampler,
            tracing: RefCell::new(tracing),
            trace,

            #[cfg(debug_assertions)]
            capture,
        }
    }

    fn step(&self) -> u64 {
        self.machine.step.get()
    }

    fn client_may_act(&self) -> bool {
        let Some(after) = self.swarm.attach else {
            return true;
        };

        self.machine.ran.get() >= after
            || self
                .machine
                .kernel
                .borrow()
                .processes
                .values()
                .all(|process| process.parent != Parent::Launcher)
    }

    fn actions(&self) -> Vec<Action> {
        let mut actions = self
            .machine
            .kernel
            .borrow()
            .runnable()
            .map(Action::Run)
            .collect::<Vec<_>>();

        for (id, session) in self.sessions.iter().enumerate() {
            if let Some(controller) = &session.controller {
                if self.machine.can_collect(id) {
                    actions.push(Action::Collect(id));
                }

                if controller.has_message() {
                    actions.push(Action::Deliver(id));
                }
            }

            if session.client.is_some()
                && session.woken.0.load(Ordering::Relaxed)
                && (id != FIRST || self.client_may_act())
            {
                actions.push(Action::Poll(id));
            }
        }

        actions
    }

    fn step_once(&mut self) -> Result<Progress, Failure> {
        let step = self.step() + 1;
        let fault_due = self.machine.faults.borrow().at_step(step);
        if fault_due && let Some(line) = self.machine.kill(Mark::KilledAtStep) {
            self.machine.step.set(step);
            self.trace.line(format!("#{step} {line}"));
            self.check()?;
            return Ok(Progress::Continue);
        }

        let actions = self.actions();
        if actions.is_empty() {
            return self.finish();
        }

        let action = self.machine.scheduler.borrow_mut().choose(
            &actions,
            step,
            &mut self.machine.choices.borrow_mut(),
        );

        self.machine.step.set(step);
        let performed = self.perform(action);
        let line = match &performed {
            Ok(line) => line.clone(),
            Err(failure) => format!("{action:?} failed: {}", failure.check),
        };

        self.trace.line(format!("#{step} {line}"));
        for line in self.machine.absorb() {
            self.trace.line(format!("    {line}"));
        }

        self.flush();
        performed?;
        if action == Action::Poll(FIRST) {
            self.machine.kernel.borrow_mut().user_breakpoints =
                self.shared.addresses(self.variant.image.bias());
            #[cfg(test)]
            {
                let bias = self.variant.image.bias();
                let others = self
                    .shared
                    .intent(bias)
                    .into_values()
                    .flatten()
                    .collect::<BTreeSet<_>>();
                self.machine.kernel.borrow_mut().temporary_breakpoints = self
                    .shared
                    .temporaries
                    .borrow()
                    .values()
                    .flatten()
                    .map(|address| address + bias)
                    .filter(|address| !others.contains(address))
                    .collect();
            }
            self.machine.kernel.borrow_mut().catching.rules =
                self.shared.catches.borrow().values().cloned().collect();
            self.follow_watches();
            self.judge()?;
        }

        #[cfg(test)]
        self.sabotage();
        self.check()?;
        Ok(Progress::Continue)
    }

    #[cfg(test)]
    fn sabotage(&self) {
        let Some(controller) = self.controller() else {
            return;
        };

        let held = match self.machine.sabotage {
            Some(Sabotage::ResumeBehindTheController) if controller.public_stop().is_some() => None,
            Some(Sabotage::ResumeWhileGathering) if controller.public_stop().is_none() => {
                Some(controller.truth().stopped)
            }
            _ => return,
        };

        let mut kernel = self.machine.kernel.borrow_mut();
        if let Some(thread) = kernel.threads.values_mut().find(|thread| {
            held.as_ref().is_none_or(|held| held.contains(&thread.tid))
                && matches!(
                    thread.state,
                    State::Stopped {
                        kind: StopKind::Signal(_),
                        ..
                    }
                )
        }) {
            thread.state = State::Running;
            thread.report = None;
        }
    }

    fn perform(&mut self, action: Action) -> Result<String, Failure> {
        match action {
            Action::Run(tid) => {
                let budget = self
                    .machine
                    .choices
                    .borrow_mut()
                    .below(Stream::Schedule, self.swarm.burst)
                    + 1;
                let mut line = self.machine.run_thread(tid, budget);
                let sampled = self.sampler.after_burst(
                    &mut self.machine.choices.borrow_mut(),
                    &mut self.machine.kernel.borrow_mut(),
                    self.variant,
                    tid,
                    &mut self.machine.marks.borrow_mut(),
                )?;
                if let Some(sampled) = sampled {
                    line = format!("{line}; {sampled}");
                }

                Ok(line)
            }
            Action::Collect(id) => Ok(self.machine.collect(id)),
            Action::Deliver(FIRST) => self.deliver(),
            Action::Deliver(id) => self.deliver_to(id),
            Action::Poll(id) => {
                let session = &mut self.sessions[id];
                session.woken.0.store(false, Ordering::Relaxed);
                let task = session.client.as_mut().expect("poll needs a client");
                let polled = if id == FIRST {
                    "poll client".to_owned()
                } else {
                    format!("poll session {id}")
                };

                match task.as_mut().poll(&mut Context::from_waker(&session.waker)) {
                    Poll::Pending => Ok(polled),
                    Poll::Ready(result) => {
                        session.client = None;
                        self.flush();
                        result.map(|()| format!("{polled} -> done"))
                    }
                }
            }
        }
    }

    fn controller(&self) -> Option<&SimController> {
        self.sessions[FIRST].controller.as_ref()
    }

    fn deliver_to(&mut self, id: SessionId) -> Result<String, Failure> {
        let session = &mut self.sessions[id];
        let controller = session
            .controller
            .as_mut()
            .expect("deliver needs a controller");
        let delivery = controller.take().expect("deliver needs a message");
        let description = format!("deliver to session {id}: {}", delivery.description);
        session.shutting_down |= delivery.shutdown;
        if controller.handle(delivery) {
            return Ok(description);
        }

        self.controller_exited(id)?;

        Ok(format!("{description} -> controller exited"))
    }

    fn controller_exited(&mut self, id: SessionId) -> Result<(), Failure> {
        let session = &mut self.sessions[id];
        session.controller = None;
        let tracer = session.tracer;
        let mut kernel = self.machine.kernel.borrow_mut();

        if let Some(thread) = kernel
            .threads
            .values()
            .find(|thread| thread.tracer() == Some(tracer) && thread.held())
        {
            return Err(Failure::debugger(
                "clean exit",
                format!(
                    "the controller exited holding thread {} in a stop",
                    thread.tid
                ),
            ));
        }

        kernel.forget_tracer(tracer);

        Ok(())
    }

    fn deliver(&mut self) -> Result<String, Failure> {
        let session = &mut self.sessions[FIRST];
        let controller = session
            .controller
            .as_mut()
            .expect("deliver needs a controller");
        let delivery = controller.take().expect("deliver needs a message");
        let description = delivery.description.clone();

        #[cfg(test)]
        if self.machine.sabotage == Some(Sabotage::DropRequestsWhileEnding)
            && delivery.request
            && !delivery.shutdown
            && controller
                .inferior()
                .is_some_and(|tgid| oracles::ending(&self.machine.kernel.borrow(), tgid))
        {
            return Ok(format!("deliver {description} -> dropped"));
        }

        #[cfg(test)]
        if self.machine.sabotage == Some(Sabotage::DropKillWhileStarting)
            && description == "kill"
            && {
                let truth = controller.truth();
                truth.inferior.is_some() && !truth.established
            }
        {
            return Ok(format!("deliver {description} -> dropped"));
        }

        let before = controller.truth();

        let trapped = delivery
            .trap
            .and_then(|tid| trap_of(&self.machine.kernel.borrow(), tid));
        let trap = trapped.map(|(tid, address, retired)| HeardTrap {
            tid,
            address,
            again: self
                .arrivals
                .get(&tid)
                .filter(|arrival| arrival.address == address && arrival.retired == retired)
                .map(|arrival| arrival.counted.clone()),
            in_shutdown: session.shutting_down,
        });
        session.shutting_down |= delivery.shutdown;
        let running = controller.handle(delivery);
        if !running {
            self.controller_exited(FIRST)?;
            return Ok(format!("deliver {description} -> controller exited"));
        }

        let after = controller.truth();

        let disturbed = trap.as_ref().is_some_and(|trap| {
            self.machine
                .kernel
                .borrow()
                .process_of(trap.tid)
                .is_none_or(|process| process.group_exit.is_some())
        });
        oracles::hit_counts(&before, &after, trap.as_ref(), disturbed)
            .map_err(|message| Failure::debugger("breakpoint accounting", message))?;
        if let Some((tid, address, retired)) = trapped {
            let counted = after.breakpoints.iter().filter_map(|(&id, breakpoint)| {
                let previous = before
                    .breakpoints
                    .get(&id)
                    .map_or(0, |earlier| earlier.hit_count);
                (breakpoint.hit_count > previous).then_some(id)
            });

            let arrival = self.arrivals.entry(tid).or_default();
            if (arrival.address, arrival.retired) != (address, retired) {
                *arrival = Arrival {
                    address,
                    retired,
                    counted: BTreeSet::new(),
                };
            }

            arrival.counted.extend(counted);
        }

        for arrival in self.arrivals.values_mut() {
            oracles::still_counting(&mut arrival.counted, &after, arrival.address);
        }

        Ok(format!("deliver {description}"))
    }

    fn judge_backtrace(&self, backtrace: &crate::Backtrace) -> Result<(), Failure> {
        let kernel = self.machine.kernel.borrow();
        let unwound = semantics::backtrace(&kernel, backtrace)
            .map_err(|message| Failure::debugger("backtrace", message))?;
        let tails = semantics::tail_frames(
            &kernel,
            &self.variant.facts,
            self.variant.image.bias(),
            backtrace,
        )
        .map_err(|message| Failure::debugger("tail calls", message))?;
        let source = self
            .program
            .source
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or_default();

        semantics::frame_lines(
            &self.variant.facts,
            self.variant.image.bias(),
            source,
            backtrace,
        )
        .map_err(|message| Failure::debugger("frames", message))?;

        let mut marks = self.machine.marks.borrow_mut();
        if tails > 0 {
            marks.hit(Mark::TailCallFrames);
        }

        match unwound {
            Some(Unwound::Whole) => marks.hit(Mark::WholeBacktrace),
            Some(Unwound::Truncated) => marks.hit(Mark::TruncatedBacktrace),
            Some(Unwound::Corrupt) => marks.hit(Mark::CorruptCaller),
            None => {}
        }

        Ok(())
    }

    fn judge(&mut self) -> Result<(), Failure> {
        let observations = std::mem::take(&mut *self.shared.observations.borrow_mut());

        for observation in observations {
            match observation {
                Observation::Backtrace { stop, backtrace } if self.still_at(stop) => {
                    self.judge_backtrace(&backtrace)?;
                }

                Observation::StepBegins {
                    thread,
                    kind,
                    presentation,
                    targets,
                    call,
                    outer,
                } => self.begin_step(thread, kind, presentation.as_ref(), &targets, call, outer),
                Observation::StepEnded(reason) => self.judge_step(reason.as_ref())?,
                Observation::Alone(Some((stop, thread))) if self.still_at(stop) => {
                    self.begin_alone(stop, thread);
                }
                Observation::Alone(_) => self.alone = None,
                Observation::ShortWrite {
                    process,
                    address,
                    length,
                    written,
                } => self.judge_short_write(process, address, length, written)?,
                Observation::Variables {
                    stop,
                    variables,
                    backtrace,
                    evaluations,
                } if self.still_at(stop) => {
                    self.judge_variables(&variables, &backtrace, &evaluations)?;
                }

                Observation::Registers {
                    stop,
                    frame,
                    registers,
                    backtrace,
                } if self.still_at(stop) => {
                    let judged = semantics::registers(
                        &self.machine.kernel.borrow(),
                        &registers,
                        frame,
                        &backtrace,
                    );
                    let reached = self.judged("registers", judged)?.unwrap_or_default();
                    let mut marks = self.machine.marks.borrow_mut();
                    for mark in reached {
                        marks.hit(mark);
                    }
                }

                Observation::Presented {
                    stop,
                    name,
                    value,
                    whole,
                    paged,
                } if self.still_at(stop) => {
                    let reached = self
                        .judge_presented(&name, &value, &whole, &paged)
                        .map_err(|message| {
                            Failure::debugger("views", format!("`{name}`: {message}"))
                        })?;
                    let mut marks = self.machine.marks.borrow_mut();
                    for mark in reached {
                        marks.hit(mark);
                    }
                }

                Observation::Backtrace { .. }
                | Observation::Variables { .. }
                | Observation::Registers { .. }
                | Observation::Presented { .. } => {}
            }
        }

        Ok(())
    }

    fn judge_short_write(
        &self,
        process: ProcessId,
        address: u64,
        length: u64,
        written: u64,
    ) -> Result<(), Failure> {
        let tgid = tgid_of(process);
        let kernel = self.machine.kernel.borrow();
        let killed = kernel
            .processes
            .get(&tgid)
            .map(|process| process.killed_externally)
            .or_else(|| kernel.ended.get(&tgid).map(|ended| ended.killed_externally));
        if killed == Some(true) {
            return Ok(());
        }

        Err(Failure::debugger(
            "memory",
            format!(
                "writing {length} bytes of code at {address:#x} wrote {written}, though \
                 nothing killed the program"
            ),
        ))
    }

    fn judge_presented(
        &self,
        name: &str,
        value: &crate::InspectedValue,
        whole: &Result<Vec<crate::ValueChildPage>, String>,
        paged: &Result<Vec<crate::ValueChildPage>, String>,
    ) -> Result<Vec<Mark>, String> {
        let Some((_, image_address, _)) = self
            .variant
            .globals
            .iter()
            .find(|(global, ..)| global == name)
        else {
            return Ok(Vec::new());
        };

        let address = image_address + self.variant.image.bias();

        match &value.state {
            crate::VariableState::Available {
                source: crate::VariableValueSource::Memory(source),
                ..
            } if source.get() == address => {}
            state => {
                return Err(format!(
                    "the program holds it at {address:#x}, but the debugger read {state:?}"
                ));
            }
        }

        let kernel = self.machine.kernel.borrow();
        let Some(tgid) = self.controller().and_then(SimController::inferior) else {
            return Ok(Vec::new());
        };

        let Some(process) = kernel.processes.get(&tgid) else {
            return Ok(Vec::new());
        };

        let memory = |at: u64, size: u64| process.space.read_user(at, size).ok();
        let Some(truth) = views::truth(name, address, &memory) else {
            return Ok(Vec::new());
        };

        let items = |fetched: &Result<Vec<crate::ValueChildPage>, String>| match fetched {
            Ok(fetched) => {
                for one in fetched {
                    views::within(one, crate::InspectionLimits::default())?;
                }

                views::items(fetched)
            }
            Err(error) => Err(format!("reading its elements failed: {error}")),
        };
        let truncated = |fetched: &Result<Vec<crate::ValueChildPage>, String>| {
            fetched.as_ref().is_ok_and(|fetched| {
                fetched
                    .iter()
                    .any(|page| page.completion != crate::InspectionCompletion::Complete)
            })
        };

        let shown = views::shown(
            value,
            items(whole)?,
            items(paged)?,
            [truncated(whole), truncated(paged)],
        )
        .ok_or_else(|| format!("its view presented nothing: {:?}", value.state))?;

        views::judge(&truth, &shown)
    }

    fn follow_watches(&mut self) {
        let watches = self
            .shared
            .watches
            .borrow()
            .values()
            .cloned()
            .collect::<Vec<_>>();
        if watches == self.watches {
            return;
        }

        let mut kernel = self.machine.kernel.borrow_mut();
        let tgid = self.controller().and_then(SimController::inferior);
        self.baselines
            .retain(|id, _| watches.iter().any(|watch| watch.id == *id));

        for watch in &watches {
            if !self.baselines.contains_key(&watch.id)
                && let Some(bytes) = tgid.and_then(|tgid| watches::bytes(&kernel, tgid, watch))
            {
                self.baselines.insert(watch.id, bytes);
            }
        }

        kernel.watching.watches = watches
            .iter()
            .map(|watch| UserWatch {
                id: watch.id,
                spans: watch.spans.clone(),
                loads: matches!(
                    watch.access,
                    crate::WatchAccess::ReadWrite | crate::WatchAccess::Read
                ),
                every: watch.reports_every_access(),
            })
            .collect();
        drop(kernel);
        self.watches = watches;
    }

    fn account_watches(&mut self, truth: &crate::backend::sim_edge::Truth) -> Result<(), Failure> {
        let (Some(stop), Some(tgid)) = (truth.public_stop, truth.inferior) else {
            return Ok(());
        };

        if self.judged_stop == Some(stop) {
            return Ok(());
        }

        self.judged_stop = Some(stop);
        let mut kernel = self.machine.kernel.borrow_mut();
        let ending = kernel
            .processes
            .get(&tgid)
            .is_none_or(|process| process.group_exit.is_some());

        if !ending {
            let reached = watches::judge(&watches::Stop {
                kernel: &kernel,
                tgid,
                reasons: &truth.reasons,
                intents: &self.watches,
                baselines: &self.baselines,
                last_counts: &self.watch_counts,
                counts: &truth.watchpoints,
            })
            .map_err(|message| Failure::debugger("watch accounting", message))?;
            let mut marks = self.machine.marks.borrow_mut();
            for mark in reached {
                marks.hit(mark);
            }
        }

        self.watch_counts.clone_from(&truth.watchpoints);

        for intent in self.shared.watches.borrow_mut().values_mut() {
            intent
                .policies
                .drain(..intent.policies.len().saturating_sub(1));
        }

        kernel.watching.restart();

        for watch in &self.watches {
            if let Some(bytes) = watches::bytes(&kernel, tgid, watch) {
                self.baselines.insert(watch.id, bytes);
            }
        }

        Ok(())
    }

    fn still_at(&self, stop: crate::StopId) -> bool {
        let Some(controller) = self.controller() else {
            return false;
        };

        controller.public_stop() == Some(stop.get())
            && controller.inferior().is_some_and(|tgid| {
                self.machine
                    .kernel
                    .borrow()
                    .processes
                    .get(&tgid)
                    .is_some_and(|process| process.group_exit.is_none())
            })
    }

    fn begin_step(
        &mut self,
        thread: crate::ThreadId,
        kind: crate::StepKind,
        presentation: Option<&crate::FramePresentation>,
        targets: &BTreeSet<u64>,
        call: Option<u64>,
        outer: usize,
    ) {
        let mut kernel = self.machine.kernel.borrow_mut();
        let tid = Tid::try_from(thread.get()).expect("a simulated tid fits");
        let Some(stepped) = kernel.threads.get(&tid) else {
            return;
        };

        let mut begun = Begun::new(stepped, kind, presentation);
        begun.outer = outer;
        let bias = self.variant.image.bias();
        begun.targets = targets.iter().map(|address| address + bias).collect();

        begun.into_return = call.and_then(|call| {
            Some(
                crate::sim::semantics::original_instruction(&self.variant.image, call)
                    .ok()?
                    .next_ip(),
            )
        });
        kernel.tracking = Some(Tracking {
            tid,
            depth: if kind == crate::StepKind::Advance {
                usize::MAX
            } else {
                begun.shadow.depth()
            },
            positions: Vec::new(),
        });

        self.stepping = Some(begun);
    }

    fn judge_step(&mut self, reason: Option<&crate::StopReason>) -> Result<(), Failure> {
        let tracking = self.machine.kernel.borrow_mut().tracking.take();
        let begun = self.stepping.take();
        let (Some(begun), Some(tracking)) = (begun, tracking) else {
            return Ok(());
        };

        let stop = self.controller().and_then(SimController::public_stop);

        if begun.kind == crate::StepKind::Advance {
            let stopped = stop.is_some_and(|stop| self.still_at(crate::StopId::new(stop)));
            let judged = semantics::advance(
                &self.machine.kernel.borrow(),
                &begun,
                &tracking.positions,
                reason,
                stopped,
                self.variant,
            )
            .map_err(|message| Failure::debugger("stepping", message))?;
            if judged.is_some() {
                self.machine.marks.borrow_mut().hit(Mark::AdvanceJudged);
            }

            return Ok(());
        }

        let ended = matches!(reason, Some(crate::StopReason::Step { kind }) if *kind == begun.kind);
        if !ended || !stop.is_some_and(|stop| self.still_at(crate::StopId::new(stop))) {
            return Ok(());
        }

        let judged = semantics::step(
            &self.machine.kernel.borrow(),
            &begun,
            &tracking.positions,
            self.variant,
        )
        .map_err(|message| Failure::debugger("stepping", message))?;

        let mut marks = self.machine.marks.borrow_mut();
        if judged.is_some() {
            marks.hit(Mark::StepJudged);
        }

        if judged == Some(Judged::Exactly) {
            marks.hit(Mark::SourceStepExact);
        }

        Ok(())
    }

    fn flush(&mut self) {
        #[cfg(debug_assertions)]
        for line in self.capture.take() {
            self.trace.line(format!("    | {line}"));
        }

        for (id, session) in self.sessions.iter().enumerate() {
            for note in session.notes.borrow_mut().drain(..) {
                if id == FIRST {
                    self.trace.line(format!("    client: {note}"));
                } else {
                    self.trace.line(format!("    session {id}: {note}"));
                }
            }
        }
    }

    fn check(&mut self) -> Result<(), Failure> {
        if let Some(gap) = &self.machine.kernel.borrow().gap {
            return Err(Failure::model_gap(gap.0.clone()));
        }

        self.trace_served()?;
        self.check_alone()?;
        self.auditor.check(
            &self.machine.kernel.borrow(),
            &mut self.machine.marks.borrow_mut(),
        )?;

        if let Some(unseen) = self.machine.kernel.borrow().watching.unseen.first() {
            return Err(Failure::debugger(
                "watch accounting",
                format!(
                    "thread {} accessed {:#x}, which watchpoint {} watches, with no slot armed for it",
                    unseen.tid, unseen.address, unseen.watch
                ),
            ));
        }

        if let Some(&(tid, (address, now, was))) = self.machine.unclean.borrow().first() {
            return Err(Failure::debugger(
                "clean release",
                format!(
                    "thread {tid} was released with {now:#04x} at {address:#x}, where the \
                     program has {was:#04x}"
                ),
            ));
        }

        if let Some(tid) = self.machine.leaked.borrow().first() {
            return Err(Failure::debugger(
                "transparency",
                format!("thread {tid} received SIGCONT, which only a debugger sends"),
            ));
        }

        if let Some(lost) = self.machine.kernel.borrow().watching.lost.first() {
            return Err(Failure::debugger(
                "watch accounting",
                format!(
                    "thread {} ran on from an access to watchpoint {} that no stop reported",
                    lost.tid, lost.watch
                ),
            ));
        }

        let truth = self.controller().map(SimController::truth);
        let debugged = truth
            .as_ref()
            .filter(|truth| truth.established)
            .and_then(|truth| truth.inferior);
        let mut kernel = self.machine.kernel.borrow_mut();
        if kernel.debugged != debugged {
            kernel.watching.restart();
            kernel.debugged = debugged;
        }

        drop(kernel);
        if let Some(truth) = &truth {
            self.account_watches(truth)?;
        }

        self.check_sessions(truth)
    }

    fn judge_variables(
        &self,
        variables: &crate::VariableSnapshot,
        backtrace: &crate::Backtrace,
        evaluations: &[crate::sim::client::Evaluated],
    ) -> Result<(), Failure> {
        self.judge_backtrace(backtrace)?;
        let source = self
            .program
            .source
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or_default();
        let entry = semantics::entry_values(
            &self.machine.kernel.borrow(),
            variables,
            backtrace,
            self.variant,
        );
        let mut reached = self.judged("entry values", entry)?.unwrap_or_default();
        let evaluated = semantics::evaluations(
            &self.machine.kernel.borrow(),
            variables,
            backtrace,
            self.variant,
            source,
            &self.program.markers,
            evaluations,
        );
        reached.extend(self.judged("expressions", evaluated)?.unwrap_or_default());

        let mut marks = self.machine.marks.borrow_mut();
        for mark in reached {
            marks.hit(mark);
        }

        drop(marks);
        let inspected = semantics::variables(
            &self.machine.kernel.borrow(),
            variables,
            backtrace,
            self.variant,
            source,
            &self.program.markers,
        );
        let inspected = self.judged("variables", inspected)?.flatten();
        if inspected == Some(Inspected::Held) {
            self.machine.marks.borrow_mut().hit(Mark::MarkerHeld);
        }

        Ok(())
    }

    fn judged<T>(
        &self,
        check: &'static str,
        result: Result<T, String>,
    ) -> Result<Option<T>, Failure> {
        match result {
            Ok(judged) => Ok(Some(judged)),
            Err(_) if self.muted.contains(&check) => Ok(None),
            Err(message) => Err(Failure::debugger(check, message)),
        }
    }

    fn begin_alone(&mut self, stop: crate::StopId, thread: crate::ThreadId) {
        let Some(process) = self.controller().and_then(SimController::inferior) else {
            return;
        };

        let kernel = self.machine.kernel.borrow();
        let retired = kernel
            .threads
            .iter()
            .filter(|(_, other)| other.tgid == process)
            .map(|(&tid, other)| (tid, other.retired))
            .collect();
        drop(kernel);

        self.alone = Some(Alone {
            stop: stop.get(),
            process,
            thread: Tid::try_from(thread.get()).expect("a simulated thread id fits"),
            retired,
        });
    }

    fn check_alone(&mut self) -> Result<(), Failure> {
        let Some(alone) = &self.alone else {
            return Ok(());
        };

        if self
            .controller()
            .is_none_or(|controller| controller.inferior() != Some(alone.process))
        {
            self.alone = None;
            return Ok(());
        }

        let kernel = self.machine.kernel.borrow();

        for (&tid, thread) in &kernel.threads {
            let before = alone.retired.get(&tid).copied().unwrap_or(0);
            if tid != alone.thread && thread.tgid == alone.process && thread.retired != before {
                return Err(Failure::debugger(
                    "thread scope",
                    format!(
                        "thread {tid} ran {} instructions while the client continued thread {} \
                         alone",
                        thread.retired - before,
                        alone.thread
                    ),
                ));
            }
        }

        drop(kernel);
        let over = self.controller().is_none_or(|controller| {
            controller
                .public_stop()
                .is_some_and(|stop| stop != alone.stop)
        });
        if over {
            self.alone = None;
        }

        Ok(())
    }

    fn check_sessions(
        &mut self,
        truth: Option<crate::backend::sim_edge::Truth>,
    ) -> Result<(), Failure> {
        let mut first = truth;
        let mut reported = Vec::new();
        let kernel = self.machine.kernel.borrow();

        oracles::unseen_hits(&kernel)
            .map_err(|message| Failure::debugger("breakpoint accounting", message))?;
        catches::unseen(&kernel)
            .map_err(|message| Failure::debugger("catch accounting", message))?;

        for (id, session) in self.sessions.iter().enumerate() {
            let Some(controller) = &session.controller else {
                continue;
            };

            let truth = match first.take() {
                Some(truth) if id == FIRST => truth,
                _ => controller.truth(),
            };

            oracles::code_integrity(&kernel, &truth, &self.variant.image)
                .map_err(|message| Failure::debugger("code integrity", message))?;
            oracles::site_ownership(&kernel, &truth)
                .map_err(|message| Failure::debugger("site ownership", message))?;
            oracles::all_stop(&kernel, &truth)
                .map_err(|message| Failure::debugger("all-stop", message))?;
            oracles::held_stops(&kernel, &truth)
                .map_err(|message| Failure::debugger("held stops", message))?;
            oracles::requested_stops(&kernel, &truth)
                .map_err(|message| Failure::debugger("requested stops", message))?;
            oracles::calls(&kernel, &truth)
                .map_err(|message| Failure::debugger("calls", message))?;
            if id == FIRST {
                oracles::user_breakpoints(
                    &kernel,
                    &truth,
                    &self.shared.intent(self.variant.image.bias()),
                )
                .map_err(|message| Failure::debugger("breakpoint accounting", message))?;
                oracles::disabled_breakpoints(&kernel, &truth, &self.shared.disabled.borrow())
                    .map_err(|message| Failure::debugger("breakpoint accounting", message))?;
                let known = catches::known(
                    &mut self.catch_reasons,
                    &truth,
                    &self.shared.catches.borrow(),
                );
                let mut marks = self.machine.marks.borrow_mut();
                self.judged("catch stops", catches::judge(&kernel, &truth, &known))?
                    .unwrap_or_default()
                    .into_iter()
                    .for_each(|mark| marks.hit(mark));
                if truth.public_stop.is_some() {
                    reported.extend(truth.reasons.iter().filter_map(|(&tid, reason)| {
                        matches!(reason, crate::StopReason::Syscall(_)).then_some(tid)
                    }));
                }
            }
        }

        oracles::output_so_far(&kernel, self.run)
            .map_err(|message| Failure::debugger("transparency", message))?;
        drop(kernel);
        let mut kernel = self.machine.kernel.borrow_mut();
        for tid in reported {
            kernel.catching.unreported.remove(&tid);
        }

        Ok(())
    }

    fn trace_served(&self) -> Result<(), Failure> {
        let served = std::mem::take(&mut self.machine.kernel.borrow_mut().served);
        self.tracing
            .borrow_mut()
            .feed(served, &mut self.machine.marks.borrow_mut())
            .map_err(|message| Failure::debugger("tracing", message))
    }

    fn finish(&self) -> Result<Progress, Failure> {
        for (id, session) in self.sessions.iter().enumerate() {
            if session.client.is_some() {
                return Err(Failure::debugger(
                    "liveness",
                    format!(
                        "the client of session {id} waits but nothing can happen: {}",
                        self.dump()
                    ),
                ));
            }

            if session.controller.is_some() {
                return Err(Failure::debugger(
                    "liveness",
                    format!(
                        "the controller of session {id} kept running after it answered the \
                         shutdown"
                    ),
                ));
            }
        }

        self.trace_served()?;
        self.tracing
            .borrow()
            .finish()
            .map_err(|message| Failure::debugger("tracing", message))?;
        let kernel = self.machine.kernel.borrow();
        self.auditor.finish(&kernel)?;
        oracles::clean_exit(&kernel).map_err(|message| Failure::debugger("clean exit", message))?;
        oracles::transparency(&kernel, self.run)
            .map_err(|message| Failure::debugger("transparency", message))?;

        let mut marks = self.machine.marks.borrow_mut();
        let ran_on = kernel
            .ended
            .values()
            .any(|ended| ended.reaper == Parent::Launcher && !ended.killed_externally);
        if marks.count(Mark::Detached) > 0 && ran_on {
            marks.hit(Mark::FinishedAfterDetach);
        }

        Ok(Progress::Finished)
    }

    fn dump(&self) -> String {
        let kernel = self.machine.kernel.borrow();
        let mut text = String::new();

        for thread in kernel.threads.values() {
            let _ = write!(
                text,
                "\n  thread {} of {}: {:?}, report {:?}, pending {:?}, rip {:#x}",
                thread.tid,
                thread.tgid,
                thread.state,
                thread.report,
                thread.pending,
                thread.registers.rip
            );
        }

        for (id, session) in self.sessions.iter().enumerate() {
            let waiter = Rc::clone(&self.machine.waiters.borrow()[id]);
            let waiter = waiter.borrow();
            let _ = write!(
                text,
                "\n  session {id}, tracing as {}: waiter {}, holding {:?}; controller {}; \
                 client {}",
                session.tracer,
                if waiter.started() {
                    "started"
                } else {
                    "not started"
                },
                waiter.holding(),
                match &session.controller {
                    Some(controller) if controller.has_message() => {
                        "running, with messages queued"
                    }
                    Some(_) => "running, queue empty",
                    None => "exited",
                },
                if session.client.is_some() {
                    "waiting"
                } else {
                    "done"
                },
            );
        }

        text
    }
}

fn start_controller(
    machine: &Rc<Machine>,
    variant: &Variant,
    swarm: &Swarm,
    random: [u8; 16],
    tracer: i32,
) -> (
    SimController,
    DebuggerHandle,
    mpsc::UnboundedReceiver<DebuggerEvent>,
) {
    let waiter = Rc::new(RefCell::new(SimWaiter::new(tracer)));
    machine.waiters.borrow_mut().push(Rc::clone(&waiter));
    let debug_info = variant.debug_info();
    let (controller, channels) = SimController::new(SimParts {
        tracer,
        kernel: Rc::clone(&machine.kernel),
        waiter,
        preemption: Rc::clone(machine) as Rc<dyn Preemption>,
        launch: SimLaunch {
            image: Arc::clone(&variant.image),
            path: Arc::clone(&variant.path),
            random,
        },
        executable: SimExecutable {
            path: Arc::clone(&variant.path),
            data: Arc::clone(&variant.data),
            inode: variant.inode,
        },
        debug_info,
        queue_capacity: swarm.queue_capacity,
        event_capacity: swarm.event_capacity,
    });

    let handle = DebuggerHandle {
        launched: Arc::new(std::path::PathBuf::from(&*variant.path)),
        program: channels.program,
        core_dump: None,
        source_paths: Arc::default(),
        requests: channels.requests,
        events: channels.events,
    };

    (controller, handle, channels.audit)
}

fn tgid_of(process: ProcessId) -> Tgid {
    Tgid::new(Tid::try_from(process.get()).expect("a simulated process id fits"))
}

fn start_untraced(
    kernel: &mut Kernel,
    variant: &Variant,
    run: &Run,
    random: [u8; 16],
) -> ProcessId {
    let tgid = kernel.spawn_untraced(
        Arc::clone(&variant.image),
        &variant.path,
        &run.arguments,
        random,
    );

    ProcessId::new(u64::try_from(tgid).expect("process ids are positive"))
}

fn script(
    swarm: &Swarm,
    program: &Program,
    variant: &Variant,
    run: &Run,
    attach: Option<ProcessId>,
) -> Script {
    Script {
        arguments: run.arguments.clone(),
        stop_at_entry: swarm.stop_at_entry,
        requests: swarm.requests,
        launches: swarm.launches,
        early_breakpoints: swarm.early_breakpoints,
        watching: swarm.watching,
        functions: program.functions.clone(),
        defined: variant.functions.clone(),
        source: program.source.clone(),
        source_lines: program.source_lines,
        markers: program
            .markers
            .iter()
            .map(|marker| (marker.line, marker.clone()))
            .collect(),
        marker_rows: marker_rows(program, variant),
        globals: variant.globals.clone(),
        views: program.views.clone(),
        debug: swarm.debug,
        image: Arc::clone(&variant.image),
        attach,
        catching: swarm.catching,
        overlap: swarm.overlap,
        statements: statements(program, variant),
        call_sites: call_sites(program, variant),
        unoptimized_functions: (!variant.facts.optimized).then(|| {
            variant
                .facts
                .functions
                .iter()
                .map(|function| (function.name.clone(), function.start..function.end))
                .collect()
        }),
    }
}

fn statements(program: &Program, variant: &Variant) -> BTreeMap<u64, BTreeSet<u64>> {
    let facts = &variant.facts;
    let source = program
        .source
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default();
    let mut statements = BTreeMap::<u64, BTreeSet<u64>>::new();

    for &(address, line) in &facts.statements {
        if facts.file(line) == source {
            statements.entry(address).or_default().insert(line.line);
        }
    }

    statements
}

fn call_sites(program: &Program, variant: &Variant) -> BTreeMap<u64, BTreeSet<u64>> {
    let facts = &variant.facts;
    let source = program
        .source
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default();
    let mut call_sites = BTreeMap::<u64, BTreeSet<u64>>::new();

    for &(entry, line) in &facts.call_sites {
        if facts.file(line) == source {
            call_sites.entry(line.line).or_default().insert(entry);
        }
    }

    call_sites
}

fn marker_rows(program: &Program, variant: &Variant) -> BTreeMap<u64, u64> {
    let facts = &variant.facts;
    let source = program
        .source
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default();
    if facts.optimized {
        return BTreeMap::new();
    }

    program
        .markers
        .iter()
        .flat_map(|marker| {
            facts
                .line_starts(source, marker.line)
                .map(move |address| (address, marker.line))
        })
        .collect()
}

#[derive(Default)]
struct Arrival {
    address: u64,
    retired: u64,
    counted: BTreeSet<u64>,
}

fn trap_of(kernel: &Kernel, tid: Tid) -> Option<(Tid, u64, u64)> {
    let thread = kernel.threads.get(&tid)?;
    match thread.state {
        State::Stopped {
            kind: StopKind::Signal(libc::SIGTRAP),
            info,
        } if info.code == super::cpu::SI_KERNEL && thread.trapped_at.is_some() => thread
            .last_trap
            .map(|(address, retired)| (tid, address, retired)),
        _ => None,
    }
}

thread_local! {
    /// The source file of this thread's last panic.
    static PANIC_FILE: RefCell<Option<String>> = const { RefCell::new(None) };
}

fn catch_panic<T>(body: impl FnOnce() -> T) -> Result<T, Failure> {
    static HOOK: std::sync::Once = std::sync::Once::new();

    HOOK.call_once(|| {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            let file = info.location().map(|location| location.file().to_owned());
            PANIC_FILE.with_borrow_mut(|last| *last = file);
            previous(info);
        }));
    });
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(body)).map_err(|payload| {
        let message = payload
            .downcast_ref::<&str>()
            .map(|message| (*message).to_owned())
            .or_else(|| payload.downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "a panic without a message".to_owned());
        let file = PANIC_FILE.with_borrow_mut(Option::take).unwrap_or_default();
        let message = format!("panicked at {file}: {message}");
        if file.contains("src/sim/") || file.ends_with("sim_edge.rs") {
            Failure::simulator("panic", message)
        } else {
            Failure::debugger("panic", message)
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sim::report::FailureKind;

    #[test]
    fn a_failure_after_a_gap_is_the_gap() {
        let failure = Failure {
            step: 7,
            ..Failure::debugger("panic", "panicked at src/backend/linux/lifecycle.rs")
        };
        let gap = ModelGap("PTRACE_LISTEN".to_owned());
        let blamed = blame_gap(failure.clone(), Some(&gap));
        assert_eq!(blamed.kind, FailureKind::ModelGap);
        assert_eq!(blamed.message, "PTRACE_LISTEN");
        assert_eq!(blamed.step, 7);
        assert_eq!(blame_gap(failure, None).kind, FailureKind::Debugger);
    }
}
