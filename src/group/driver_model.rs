use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex, PoisonError};

use proptest::prelude::*;
use tokio::sync::{broadcast, mpsc, oneshot, watch};

use super::{
    Command, Driver, GroupEvent, GroupExecutionId, Internal, Member, MemberNumber, MemberState,
    Shared,
};
use crate::backend::{ControllerJoin, EventSender, GroupNotice, Joined, Started};
use crate::{
    ControllerMessage, Debugger, DebuggerEvent, DebuggerHandle, Error, ExceptionDisposition,
    ExecutionId, ExitStatus, InferiorState, ProcessId, Request, ResumeScope, StateSnapshot, StopId,
    StopReason, StopScope, ThreadId,
};

const FIRST_PROCESS: u64 = 1 << 23;

const MAX_MEMBERS: u32 = 4;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Own,

    Pause,

    Join,
}

impl Kind {
    fn reason(self) -> StopReason {
        match self {
            Self::Own => StopReason::Breakpoint {
                address: Some(crate::VirtualAddress::new(0x1234)),
                hits: Arc::from([crate::BreakpointHit {
                    breakpoint: crate::BreakpointId::new(1),
                    hit_count: 1,
                }]),
            },
            Self::Pause => StopReason::Pause,
            Self::Join => StopReason::Entry,
        }
    }

    fn reported(self, scope: StopScope) -> bool {
        match self {
            Self::Own => true,
            Self::Pause => scope == StopScope::Process,
            Self::Join => false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Truth {
    Running {
        execution: ExecutionId,
        pausing: bool,
    },
    Stopped {
        stop: StopId,
        kind: Kind,
    },
    Exited,
    Detached,
    Lost,
}

struct Fake {
    process: ProcessId,

    handle: Option<DebuggerHandle>,
    events: EventSender,
    truth: Truth,
    revision: u64,
    next_id: u64,

    seen: BTreeSet<StopId>,

    named: BTreeSet<StopId>,
    reported: BTreeSet<StopId>,

    left: Vec<StopId>,

    unannounced: BTreeMap<u64, StopId>,

    late_join_event: bool,
    released: bool,
    lose: Option<oneshot::Sender<()>>,
}

impl Fake {
    fn next_id(&mut self) -> u64 {
        self.next_id += 1;
        self.next_id
    }

    fn publish(&mut self, event: impl FnOnce(u64) -> DebuggerEvent) -> u64 {
        self.revision += 1;
        let _ = self.events.send(event(self.revision));
        self.revision
    }

    fn snapshot(&self) -> StateSnapshot {
        let process_id = self.process;
        let (inferior, stop_id) = match self.truth {
            Truth::Running { execution, .. } => (
                InferiorState::Running {
                    process_id,
                    execution_id: Some(execution),
                },
                None,
            ),
            Truth::Stopped { stop, kind } => (
                InferiorState::Stopped {
                    process_id,
                    stop_id: stop,
                    thread_id: ThreadId::new(process_id.get()),
                    reason: kind.reason(),
                },
                Some(stop),
            ),
            Truth::Exited => (
                InferiorState::Exited {
                    process_id,
                    status: ExitStatus::Code(0),
                },
                None,
            ),
            Truth::Detached | Truth::Lost => (InferiorState::Detached { process_id }, None),
        };
        StateSnapshot {
            revision: self.revision,
            inferior,
            stop_id,
            selected: None,
            selected_frame: None,
            threads: Arc::from([]),
            presentation: None,
            breakpoints: Arc::from([]),
            watchpoints: Arc::from([]),
            catchpoints: Arc::from([]),
        }
    }

    fn stop(&mut self, own: bool) {
        let Truth::Running { execution, pausing } = self.truth else {
            return;
        };
        let kind = if pausing && !own {
            Kind::Pause
        } else {
            Kind::Own
        };
        let stop = StopId::new(self.next_id());
        self.truth = Truth::Stopped { stop, kind };
        let process_id = self.process;
        self.publish(|revision| DebuggerEvent::InferiorStopped {
            revision,
            process_id,
            execution_id: Some(execution),
            stop_id: stop,
            thread_id: ThreadId::new(process_id.get()),
            reason: kind.reason(),
        });
    }

    fn exit(&mut self) {
        let Truth::Running { execution, .. } = self.truth else {
            return;
        };
        self.truth = Truth::Exited;
        let process_id = self.process;
        self.publish(|revision| DebuggerEvent::InferiorExited {
            revision,
            process_id,
            execution_id: Some(execution),
            status: ExitStatus::Code(0),
        });
    }

    fn leave(&mut self, stop: StopId, client: bool) -> ExecutionId {
        let execution = ExecutionId::new(self.next_id());
        self.left.push(stop);
        self.truth = Truth::Running {
            execution,
            pausing: false,
        };
        let process_id = self.process;
        let revision = self.publish(|revision| DebuggerEvent::InferiorContinued {
            revision,
            process_id,
            execution_id: execution,
            resumed: ResumeScope::Process(process_id),
        });
        if client {
            self.unannounced.insert(revision, stop);
        }
        execution
    }

    fn state(&self) -> MemberState {
        match self.truth {
            Truth::Running { .. } => MemberState::Running,
            Truth::Stopped { kind, .. } => MemberState::Stopped(kind.reason()),
            Truth::Exited => MemberState::Exited(ExitStatus::Code(0)),
            Truth::Detached => MemberState::Detached,
            Truth::Lost => MemberState::Lost,
        }
    }
}

struct World {
    scope: StopScope,
    members: BTreeMap<MemberNumber, Fake>,
    group: broadcast::Receiver<GroupEvent>,

    client: bool,

    internal: bool,

    resumes: usize,
    trace: Vec<String>,
    failures: Vec<String>,
}

impl World {
    fn observe(&mut self) {
        loop {
            match self.group.try_recv() {
                Ok(event) => {
                    self.trace.push(format!("{event:?}"));
                    if let GroupEvent::GroupStopped { member, stop, .. } = event {
                        self.reported(member, stop);
                    }
                }
                Err(broadcast::error::TryRecvError::Empty) => return,
                Err(error) => {
                    self.failures.push(format!("the group's events: {error:?}"));
                    return;
                }
            }
        }
    }

    fn reported(&mut self, member: MemberNumber, stop: StopId) {
        let internal = self.internal;
        let Some(fake) = self.members.get_mut(&member) else {
            self.failures.push(format!(
                "the group reported member {member}, which never was"
            ));
            return;
        };
        if !fake.reported.insert(stop) {
            self.failures.push(format!(
                "rule 3: member {member}'s stop {stop} was reported twice"
            ));
        }

        let at = match fake.truth {
            Truth::Stopped { stop: now, .. } => now == stop,
            Truth::Exited | Truth::Lost | Truth::Detached => true,
            Truth::Running { .. } => false,
        };
        let unannounced = internal && fake.unannounced.values().any(|left| *left == stop);
        if !at && !unannounced {
            self.failures.push(format!(
                "rule 1: the group reported member {member}'s stop {stop}, while it is {:?}",
                fake.truth
            ));
        }
    }

    fn answer(&mut self, number: MemberNumber, request: Request) -> bool {
        self.observe();
        let (scope, by_client, resumes) = (self.scope, self.client, self.resumes);
        let mut failures = Vec::new();
        let fake = self
            .members
            .get_mut(&number)
            .expect("each session is a fake's");

        if fake.late_join_event {
            fake.late_join_event = false;
            if let Truth::Stopped { stop, kind } = fake.truth {
                let process_id = fake.process;
                fake.publish(|revision| DebuggerEvent::InferiorStopped {
                    revision,
                    process_id,
                    execution_id: None,
                    stop_id: stop,
                    thread_id: ThreadId::new(process_id.get()),
                    reason: kind.reason(),
                });
            }
        }

        let goes_on = match request {
            Request::Snapshot { reply } => {
                let _ = reply.send(Ok(fake.snapshot()));
                true
            }
            Request::Continue { stop_id, reply, .. } => {
                let continued = match fake.truth {
                    Truth::Stopped { stop, kind } if stop == stop_id => {
                        if !by_client {
                            let shown = fake.named.contains(&stop);
                            if !fake.seen.contains(&stop) && !shown {
                                failures.push(format!(
                                    "rule 2: the driver resumed member {number} at stop {stop}, \
                                     which nobody was shown"
                                ));
                            }
                            if kind.reported(scope)
                                && !fake.reported.contains(&stop)
                                && !shown
                                && resumes == 0
                            {
                                failures.push(format!(
                                    "rule 3: the driver resumed member {number}'s stop {stop} \
                                     before reporting it"
                                ));
                            }
                        }
                        Ok(fake.leave(stop, by_client))
                    }
                    Truth::Stopped { .. } => Err(Error::StaleStop),
                    Truth::Running { .. } => Err(Error::NotStopped),
                    Truth::Exited | Truth::Detached | Truth::Lost => Err(Error::NotRunning),
                };
                let _ = reply.send(continued);
                true
            }
            Request::Pause { reply, .. } => {
                let paused = match &mut fake.truth {
                    Truth::Running { execution, pausing } => {
                        *pausing = true;
                        Ok(*execution)
                    }
                    Truth::Stopped { .. } => Err(Error::AlreadyStopped),
                    Truth::Exited | Truth::Detached | Truth::Lost => Err(Error::NotRunning),
                };
                let _ = reply.send(paused);
                true
            }
            Request::ReleaseOnShutdown { reply } => {
                fake.released = true;
                let _ = reply.send(Ok(()));
                true
            }
            Request::Shutdown { reply } => {
                if fake.released {
                    fake.truth = Truth::Detached;
                }
                let _ = reply.send(Ok(()));
                false
            }
            _ => {
                failures.push(format!("member {number} got a request no fake serves"));
                true
            }
        };
        self.failures.extend(failures);
        goes_on
    }
}

fn lock(world: &Mutex<World>) -> std::sync::MutexGuard<'_, World> {
    world.lock().unwrap_or_else(PoisonError::into_inner)
}

async fn serve(
    world: Arc<Mutex<World>>,
    number: MemberNumber,
    mut requests: mpsc::Receiver<ControllerMessage>,
    mut lose: oneshot::Receiver<()>,
) {
    loop {
        let message = tokio::select! {
            biased;
            _ = &mut lose => return,
            message = requests.recv() => message,
        };
        match message {
            Some(ControllerMessage::Request(request)) => {
                if !lock(&world).answer(number, request) {
                    return;
                }
            }
            Some(_) => {}
            None => return,
        }
    }
}

fn image() -> watch::Receiver<Arc<crate::ModuleImage>> {
    let image = crate::ModuleImage::new(
        PathBuf::from("/test"),
        crate::TargetDescription {
            architecture: crate::Architecture::X86_64,
            byte_order: crate::ByteOrder::Little,
            pointer_width: crate::PointerWidth::Bits64,
        },
        crate::AddressRange {
            start: crate::ImageAddress::new(0),
            end: crate::ImageAddress::new(0x100),
        },
        crate::model::ModuleMetadata::default(),
    );
    watch::channel(Arc::new(image)).1
}

#[derive(Debug, Clone, Copy)]
enum Named {
    GroupStop,

    Current(usize),

    Previous(usize),
}

#[derive(Debug, Clone)]
enum Op {
    Deliver(usize),

    Lag(usize, usize),

    Stop {
        member: usize,
        own: bool,
    },
    Exit(usize),

    /// A client steps a stopped member through its own session.
    Step(usize),

    Lose(usize),

    Join {
        late: bool,
    },
    Continue,
    ContinueFrom(Named),
    Resume(Option<Named>),
    Pause,
    Detach(usize),
}

fn op() -> impl Strategy<Value = Op> {
    let member = 0..MAX_MEMBERS as usize;
    let named = prop_oneof![
        Just(Named::GroupStop),
        member.clone().prop_map(Named::Current),
        member.clone().prop_map(Named::Previous),
    ];
    prop_oneof![
        12 => (0..8usize).prop_map(Op::Deliver),
        1 => (member.clone(), 0..8usize).prop_map(|(member, count)| Op::Lag(member, count)),
        5 => (member.clone(), any::<bool>()).prop_map(|(member, own)| Op::Stop { member, own }),
        1 => member.clone().prop_map(Op::Exit),
        2 => member.clone().prop_map(Op::Step),
        1 => member.clone().prop_map(Op::Lose),
        2 => any::<bool>().prop_map(|late| Op::Join { late }),
        2 => Just(Op::Continue),
        2 => named.clone().prop_map(Op::ContinueFrom),
        1 => proptest::option::of(named).prop_map(Op::Resume),
        2 => Just(Op::Pause),
        1 => member.prop_map(Op::Detach),
    ]
}

enum Answer {
    Execution(oneshot::Receiver<crate::Result<GroupExecutionId>>),
    Done(oneshot::Receiver<crate::Result<()>>),
    Paused(oneshot::Receiver<crate::Result<Option<GroupExecutionId>>>),
}

struct Asked {
    op: Op,

    stale: bool,

    named: Option<(MemberNumber, StopId)>,
    events: usize,
    resume: bool,

    resumed: bool,
    answer: Answer,
}

type Source = (MemberNumber, bool);

struct Harness {
    world: Arc<Mutex<World>>,
    driver: Driver,
    shared: Arc<Shared>,
    pool: BTreeMap<Source, VecDeque<Internal>>,
    asked: Vec<Asked>,
}

impl Harness {
    fn new(scope: StopScope) -> Self {
        let (events, group) = broadcast::channel(4096);
        let world = Arc::new(Mutex::new(World {
            scope,
            members: BTreeMap::new(),
            group,
            client: false,
            internal: false,
            resumes: 0,
            trace: Vec::new(),
            failures: Vec::new(),
        }));

        let mut channels = None;
        let root = Debugger::start(Arc::new(PathBuf::from("/test")), |made| {
            channels = Some((made.receiver, made.events));
            let controller = ControllerJoin::Thread(std::thread::spawn(|| {}));
            Ok((image(), None, controller))
        })
        .expect("start the root's fake session");
        let (requests, root_events) = channels.expect("the root's channels");
        let handle = root.handle();
        let (lose, losing) = oneshot::channel();
        let process = ProcessId::new(FIRST_PROCESS + 1);
        let mut fake = Fake {
            process,
            handle: Some(handle.clone()),
            events: root_events,
            truth: Truth::Running {
                execution: ExecutionId::new(1),
                pausing: false,
            },
            revision: 0,
            next_id: 1,
            seen: BTreeSet::new(),
            named: BTreeSet::new(),
            reported: BTreeSet::new(),
            left: Vec::new(),
            unannounced: BTreeMap::new(),
            late_join_event: false,
            released: false,
            lose: Some(lose),
        };

        let shared = Arc::new(Shared {
            members: Mutex::new(vec![Member {
                number: 1,
                process: None,
                parent: None,
                executed: None,
                command: String::new(),
                state: MemberState::NotStarted,
            }]),
            handles: Mutex::new(BTreeMap::from([(1, handle)])),
            copies: Mutex::default(),
            unfollowed: AtomicBool::default(),
            unmirrored: AtomicBool::default(),
            last_stop: Mutex::default(),
        });
        let driver = Driver::new(Arc::clone(&shared), events, root, scope);

        fake.publish(|revision| DebuggerEvent::InferiorLaunched {
            revision,
            process_id: process,
            execution_id: ExecutionId::new(1),
        });
        lock(&world).members.insert(1, fake);
        tokio::spawn(serve(Arc::clone(&world), 1, requests, losing));

        Self {
            world,
            driver,
            shared,
            pool: BTreeMap::new(),
            asked: Vec::new(),
        }
    }

    async fn settle(&mut self) {
        for _ in 0..64 {
            tokio::task::yield_now().await;
        }
        lock(&self.world).observe();

        while let Ok(message) = self.driver.internal.1.try_recv() {
            let source = match &message {
                Internal::PauseFailed(number) => (*number, true),
                Internal::Event(number, _) | Internal::Lagged(number) | Internal::Ended(number) => {
                    (*number, false)
                }
            };
            self.pool.entry(source).or_default().push_back(message);
        }
        self.pool.retain(|_, messages| !messages.is_empty());
        self.check_answers();
    }

    fn number(&self, index: usize) -> MemberNumber {
        let numbers = lock(&self.world)
            .members
            .keys()
            .copied()
            .collect::<Vec<_>>();
        numbers[index % numbers.len()]
    }

    async fn deliver(&mut self, pick: usize) -> bool {
        let sources = self.pool.keys().copied().collect::<Vec<_>>();
        if sources.is_empty() {
            return false;
        }
        let source = sources[pick % sources.len()];
        let Some(message) = self.pool.get_mut(&source).and_then(VecDeque::pop_front) else {
            return false;
        };

        {
            let mut world = lock(&self.world);
            world.observe();
            world.internal = true;
            match &message {
                Internal::Event(number, event) => {
                    let fake = world.members.get_mut(number).expect("a member's event");
                    if let DebuggerEvent::InferiorStopped { stop_id, .. } = event {
                        fake.seen.insert(*stop_id);
                    }
                    fake.unannounced
                        .retain(|revision, _| *revision > event.revision());
                }

                Internal::Lagged(number) => {
                    let fake = world.members.get_mut(number).expect("a member's lag");
                    match fake.truth {
                        Truth::Stopped { stop, .. } => {
                            fake.seen.insert(stop);
                            fake.unannounced.clear();
                        }
                        Truth::Running { .. } => fake.unannounced.clear(),
                        Truth::Exited | Truth::Detached | Truth::Lost => {}
                    }
                }
                Internal::PauseFailed(_) | Internal::Ended(_) => {}
            }
            drop(world);
        }

        self.driver.internal(message).await;
        self.settle().await;
        lock(&self.world).internal = false;
        true
    }

    fn lag(&mut self, index: usize, count: usize) {
        let number = self.number(index);
        let Some(messages) = self.pool.get_mut(&(number, false)) else {
            return;
        };
        let events = messages
            .iter()
            .take_while(|message| matches!(message, Internal::Event(..)))
            .count();
        if events < 2 {
            return;
        }
        for _ in 0..=count % (events - 1) {
            messages.pop_front();
        }
        messages.push_front(Internal::Lagged(number));
    }

    async fn join(&mut self, late: bool) {
        let (number, root) = {
            let world = lock(&self.world);
            let count = u32::try_from(world.members.len()).expect("few members");
            if count >= MAX_MEMBERS {
                return;
            }
            (count + 1, world.members[&1].process)
        };
        let process = ProcessId::new(FIRST_PROCESS + u64::from(number));
        let (requests, received) = mpsc::channel(32);
        let (events, _) = broadcast::channel(1024);
        let (started, starting) = oneshot::channel();
        let (lose, losing) = oneshot::channel();
        let joined_at = StopId::new(1);
        let _ = started.send(Ok(joined_at));

        let joined = Joined {
            process,
            parent: Some(root),
            executed: None,
            requests,
            events: events.clone(),
            program: image(),
            launched: Arc::new(PathBuf::from("/test")),
            controller: ControllerJoin::Thread(std::thread::spawn(|| {})),
            started: Started::Fork(starting),
        };
        lock(&self.world).members.insert(
            number,
            Fake {
                process,
                handle: None,
                events: events.into(),
                truth: Truth::Stopped {
                    stop: joined_at,
                    kind: Kind::Join,
                },
                revision: 0,
                next_id: 1,
                seen: BTreeSet::from([joined_at]),
                named: BTreeSet::new(),
                reported: BTreeSet::new(),
                left: Vec::new(),
                unannounced: BTreeMap::new(),
                late_join_event: late,
                released: false,
                lose: Some(lose),
            },
        );
        tokio::spawn(serve(Arc::clone(&self.world), number, received, losing));

        self.driver.notice(GroupNotice::Joined(joined)).await;
        let handle = self
            .shared
            .handles
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&number)
            .cloned();
        lock(&self.world)
            .members
            .get_mut(&number)
            .expect("the member joined")
            .handle = handle;
        self.settle().await;
    }

    fn name(&self, named: Named) -> Option<(MemberNumber, StopId, bool)> {
        let (number, stop) = match named {
            Named::GroupStop => {
                let last = self
                    .shared
                    .last_stop
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .clone()?;
                (last.member, last.stop)
            }
            Named::Current(index) => {
                let number = self.number(index);
                match lock(&self.world).members[&number].truth {
                    Truth::Stopped { stop, .. } => (number, stop),
                    _ => return None,
                }
            }
            Named::Previous(index) => {
                let number = self.number(index);
                (number, *lock(&self.world).members[&number].left.last()?)
            }
        };

        let mut world = lock(&self.world);
        let fake = world.members.get_mut(&number)?;
        fake.named.insert(stop);
        let current = matches!(fake.truth, Truth::Stopped { stop: now, .. } if now == stop);
        drop(world);
        Some((number, stop, !current))
    }

    async fn ask(&mut self, op: &Op) {
        let mut stale = false;
        let mut resume = false;
        let mut named_stop = None;
        let (command, answer) = match op {
            Op::Continue => {
                let (reply, answer) = oneshot::channel();
                (Command::ContinueAll(None, reply), Answer::Execution(answer))
            }
            Op::ContinueFrom(named) => {
                let Some((number, stop, left)) = self.name(*named) else {
                    return;
                };
                stale = left;
                named_stop = Some((number, stop));
                let (reply, answer) = oneshot::channel();
                (
                    Command::ContinueAll(Some((number, stop)), reply),
                    Answer::Execution(answer),
                )
            }
            Op::Resume(named) => {
                let observed = match named {
                    Some(named) => {
                        let Some((number, stop, left)) = self.name(*named) else {
                            return;
                        };
                        stale = left;
                        named_stop = Some((number, stop));
                        Some((number, stop))
                    }
                    None => None,
                };
                resume = true;
                lock(&self.world).resumes += 1;
                let (reply, answer) = oneshot::channel();
                (
                    Command::ResumeAll(observed, reply),
                    Answer::Execution(answer),
                )
            }
            Op::Pause => {
                let (reply, answer) = oneshot::channel();
                (Command::PauseAll(reply), Answer::Paused(answer))
            }
            Op::Detach(index) => {
                let (reply, answer) = oneshot::channel();
                (
                    Command::Detach(self.number(*index), reply),
                    Answer::Done(answer),
                )
            }
            _ => unreachable!("{op:?} is not a command"),
        };

        let left = |world: &World| -> usize {
            world.members.values().map(|member| member.left.len()).sum()
        };
        let (events, before) = {
            let mut world = lock(&self.world);
            world.observe();
            (world.trace.len(), left(&world))
        };
        self.driver.serve(command).await;
        let resumed = left(&lock(&self.world)) > before;
        self.asked.push(Asked {
            op: op.clone(),
            stale,
            named: named_stop,
            events,
            resume,
            resumed,
            answer,
        });
        self.settle().await;
    }

    fn check_answers(&mut self) {
        let mut world = lock(&self.world);
        self.asked.retain_mut(|asked| {
            let answer = match &mut asked.answer {
                Answer::Execution(answer) => answer.try_recv().map(|result| result.map(drop)),
                Answer::Done(answer) => answer.try_recv(),
                Answer::Paused(answer) => answer.try_recv().map(|result| result.map(drop)),
            };
            let result = match answer {
                Ok(result) => result,
                Err(oneshot::error::TryRecvError::Empty) => return true,
                Err(oneshot::error::TryRecvError::Closed) => {
                    world
                        .failures
                        .push(format!("rule 4: {:?} was dropped unanswered", asked.op));
                    return false;
                }
            };
            if asked.resume {
                world.resumes -= 1;
            }
            if asked.resumed
                && !asked.stale
                && matches!(asked.answer, Answer::Execution(_))
                && let Err(error) = &result
            {
                world.failures.push(format!(
                    "rule 2: {:?} resumed a member, but failed with {error:?}",
                    asked.op
                ));
            }
            if asked.stale && result.is_ok() {
                world.failures.push(format!(
                    "rule 2: {:?}, from a stop already left, succeeded",
                    asked.op
                ));
            }

            if let (Some((member, stop)), Ok(())) = (asked.named, &result) {
                let resumed = world.members[&member].left.contains(&stop);
                let reported = world.trace[asked.events..]
                    .iter()
                    .any(|event| event.starts_with("GroupStopped"));
                if !resumed && !reported && !asked.stale {
                    world.failures.push(format!(
                        "rule 2: {:?} succeeded without resuming member {member}'s stop {stop}",
                        asked.op
                    ));
                }
            }
            false
        });
    }

    async fn apply(&mut self, op: &Op) {
        match op {
            Op::Deliver(pick) => {
                self.deliver(*pick).await;
            }
            Op::Lag(index, count) => self.lag(*index, *count),
            Op::Stop { member, own } => {
                let number = self.number(*member);
                let mut world = lock(&self.world);
                world.observe();
                world.members.get_mut(&number).expect("a member").stop(*own);
            }
            Op::Exit(index) => {
                let number = self.number(*index);
                let mut world = lock(&self.world);
                world.observe();
                world.members.get_mut(&number).expect("a member").exit();
            }
            Op::Step(index) => {
                let number = self.number(*index);
                let step = {
                    let mut world = lock(&self.world);
                    world.observe();
                    let fake = &world.members[&number];
                    let step = match fake.truth {
                        Truth::Stopped { stop, .. } => fake
                            .handle
                            .clone()
                            .map(|handle| (handle, fake.process, stop)),
                        _ => None,
                    };
                    world.client = step.is_some();
                    step
                };
                if let Some((handle, process, stop)) = step {
                    let _ = handle
                        .continue_execution(
                            stop,
                            ResumeScope::Process(process),
                            ExceptionDisposition::Pass,
                        )
                        .await;
                    lock(&self.world).client = false;
                }
            }
            Op::Lose(index) => {
                let number = self.number(*index);
                let mut world = lock(&self.world);
                world.observe();
                let fake = world.members.get_mut(&number).expect("a member");
                if matches!(fake.truth, Truth::Running { .. } | Truth::Stopped { .. }) {
                    fake.truth = Truth::Lost;
                    if let Some(lose) = fake.lose.take() {
                        let _ = lose.send(());
                    }
                }
                drop(world);
            }
            Op::Join { late } => self.join(*late).await,
            Op::Continue | Op::ContinueFrom(_) | Op::Resume(_) | Op::Pause | Op::Detach(_) => {
                self.ask(op).await;
            }
        }
        self.settle().await;
        if self.pool.is_empty() {
            self.check_states();
        }
    }

    fn check_states(&self) {
        let members = self
            .shared
            .members
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        let mut world = lock(&self.world);
        let mut failures = Vec::new();
        for member in members {
            let Some(fake) = world.members.get(&member.number) else {
                continue;
            };
            if member.state != fake.state() {
                failures.push(format!(
                    "rule 5: the group shows member {} as {:?}, while it is {:?}",
                    member.number, member.state, fake.truth
                ));
            }
        }
        world.failures.extend(failures);
    }

    async fn drain(&mut self) {
        for _ in 0..64 {
            let mut moved = false;
            while self.deliver(0).await {
                moved = true;
            }
            let pausing = lock(&self.world)
                .members
                .iter()
                .filter(|(_, fake)| matches!(fake.truth, Truth::Running { pausing: true, .. }))
                .map(|(number, _)| *number)
                .collect::<Vec<_>>();
            for number in pausing {
                lock(&self.world)
                    .members
                    .get_mut(&number)
                    .expect("a member")
                    .stop(false);
                moved = true;
            }
            self.settle().await;
            if !moved && self.pool.is_empty() {
                return;
            }
        }
    }

    async fn finish(&mut self) {
        self.drain().await;
        self.check_states();
        {
            let mut world = lock(&self.world);
            for asked in &self.asked {
                world
                    .failures
                    .push(format!("rule 4: {:?} was never answered", asked.op));
            }
            if self.driver.stops.is_stopping() {
                world
                    .failures
                    .push("rule 4: the group is left stopping".to_owned());
            }
        }

        for _ in 0..16 {
            let reported = lock(&self.world).trace.len();
            self.ask(&Op::Continue).await;
            self.drain().await;
            let trace = &lock(&self.world).trace;
            if !trace[reported..]
                .iter()
                .any(|event| event.starts_with("GroupStopped"))
            {
                break;
            }
        }

        let _ = self.driver.shutdown().await;
        lock(&self.world).observe();
    }
}

fn run(scope: StopScope, ops: &[Op]) -> (Vec<String>, Vec<String>) {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("a runtime");
    runtime.block_on(async {
        let mut harness = Harness::new(scope);
        harness.settle().await;
        for op in ops {
            harness.apply(op).await;
            if !lock(&harness.world).failures.is_empty() {
                break;
            }
        }
        if lock(&harness.world).failures.is_empty() {
            harness.finish().await;
        }
        let mut world = lock(&harness.world);
        (
            std::mem::take(&mut world.trace),
            std::mem::take(&mut world.failures),
        )
    })
}

fn scope() -> impl Strategy<Value = StopScope> {
    prop_oneof![Just(StopScope::Group), Just(StopScope::Process)]
}

proptest! {
    #[test]
    fn the_group_driver_keeps_its_rules_in_every_order(
        scope in scope(),
        ops in proptest::collection::vec(op(), 1..48),
    ) {
        let (trace, failures) = run(scope, &ops);
        prop_assert!(failures.is_empty(), "{failures:#?}\ngroup events: {trace:#?}");

        let (again, _) = run(scope, &ops);
        prop_assert_eq!(trace, again, "rule 6: the same inputs gave other group events");
    }
}

#[test]
fn a_pause_that_ended_with_no_stop_names_no_later_stop() {
    let ops = [
        Op::Deliver(0),
        Op::Lose(0),
        Op::Pause,
        Op::Deliver(0),
        Op::Deliver(0),
        Op::Join { late: false },
        Op::Stop {
            member: 1,
            own: true,
        },
        Op::Deliver(0),
        Op::Deliver(0),
    ];
    let (trace, failures) = run(StopScope::Group, &ops);
    assert!(failures.is_empty(), "{failures:#?}");

    let stopped = trace
        .iter()
        .find(|event| event.starts_with("GroupStopped"))
        .expect("the new member's stop");
    assert!(stopped.contains("member: 2"), "{trace:#?}");
    assert!(stopped.contains("execution: None"), "{trace:#?}");
}

#[test]
fn a_continue_from_a_shown_stop_resumes_it_past_a_dropped_waiting_stop() {
    let ops = [
        Op::Join { late: false },
        Op::Deliver(0),
        Op::Deliver(0),
        Op::Stop {
            member: 0,
            own: false,
        },
        Op::Step(0),
        Op::Stop {
            member: 1,
            own: false,
        },
        Op::Deliver(1),
        Op::Stop {
            member: 0,
            own: false,
        },
        Op::Deliver(0),
        Op::ContinueFrom(Named::Current(0)),
    ];
    let (trace, failures) = run(StopScope::Group, &ops);
    assert!(failures.is_empty(), "{failures:#?}\n{trace:#?}");
}

#[test]
fn a_continue_from_a_shown_stop_resumes_a_member_the_group_thinks_runs() {
    let ops = [
        Op::Join { late: false },
        Op::Deliver(0),
        Op::Stop {
            member: 1,
            own: false,
        },
        Op::Deliver(0),
        Op::Stop {
            member: 0,
            own: false,
        },
        Op::Deliver(1),
        Op::Step(0),
        Op::Deliver(0),
        Op::Deliver(0),
        Op::Deliver(0),
        Op::Stop {
            member: 0,
            own: false,
        },
        Op::ContinueFrom(Named::Current(0)),
    ];
    let (trace, failures) = run(StopScope::Group, &ops);
    assert!(failures.is_empty(), "{failures:#?}\n{trace:#?}");
}

#[test]
fn a_failed_continue_leaves_the_execution_under_way() {
    let ops = [
        Op::Deliver(0),
        Op::Stop {
            member: 0,
            own: true,
        },
        Op::Deliver(0),
        Op::Continue,
        Op::ContinueFrom(Named::Previous(0)),
        Op::Stop {
            member: 0,
            own: true,
        },
        Op::Deliver(0),
        Op::Deliver(0),
    ];
    let (trace, failures) = run(StopScope::Group, &ops);
    assert!(failures.is_empty(), "{failures:#?}");

    let stops = trace
        .iter()
        .filter(|event| event.starts_with("GroupStopped"))
        .collect::<Vec<_>>();
    assert!(
        stops
            .get(1)
            .is_some_and(|stop| stop.contains("execution: Some(GroupExecutionId(1))")),
        "{trace:#?}"
    );
}

#[test]
fn a_continue_that_resumed_a_member_succeeds_past_a_lost_one() {
    let ops = [
        Op::Stop {
            member: 0,
            own: false,
        },
        Op::Deliver(0),
        Op::Deliver(0),
        Op::Join { late: false },
        Op::Lose(1),
        Op::Continue,
    ];
    let (trace, failures) = run(StopScope::Group, &ops);
    assert!(failures.is_empty(), "{failures:#?}\n{trace:#?}");
}

#[test]
fn a_continue_from_a_shown_stop_succeeds_past_a_lost_member() {
    let ops = [
        Op::Join { late: false },
        Op::Deliver(0),
        Op::Deliver(0),
        Op::Stop {
            member: 0,
            own: false,
        },
        Op::Lose(0),
        Op::Stop {
            member: 1,
            own: false,
        },
        Op::Deliver(6),
        Op::Deliver(1),
        Op::Continue,
        Op::ContinueFrom(Named::GroupStop),
    ];
    let (trace, failures) = run(StopScope::Group, &ops);
    assert!(failures.is_empty(), "{failures:#?}\n{trace:#?}");
}
