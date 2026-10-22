//! One OS thread that owns ptrace for a group of controllers.
//!
//! ptrace ties a tracee to the thread that traced it, and the kernel traces
//! a traced process's fork children to the same thread. So every process a
//! group debugs or watches is traced by one thread, and each process's
//! controller runs on it. The thread serves each controller's queue in
//! turn, through the same `handle_message` a controller of its own thread
//! runs, and parks while none has a message: a request, a wait status the
//! hub routed, or an agent's deadline wakes it.
//!
//! The thread lives until every controller has ended and every handle to
//! the tracer is dropped, so a launched process, which dies when the thread
//! that launched it does, never outlives the group early.

use std::sync::{Arc, mpsc as std_mpsc};
use std::task::{Context, Wake, Waker};
use std::thread::{self, JoinHandle, Thread};
use std::time::Instant;

use super::follow::{Group, GroupSetup};
use super::wait_hub::{HubEdge, MemberKey, WaitHub};
use super::{ControllerMessage, SessionLease};
use crate::Result;

/// A controller a tracer thread serves.
pub(super) trait Hosted {
    /// The next message queued, without waiting.
    fn next_message(&self) -> Option<ControllerMessage>;

    /// Serves one message, and returns whether the controller keeps running.
    fn handle_message(&mut self, message: ControllerMessage) -> bool;

    /// Takes a message from the controller's queue if one is there, and
    /// otherwise arranges for `context`'s waker to be woken when one is.
    fn poll_queue(&self, context: &mut Context<'_>) -> bool;

    /// When the controller must be served even if no message arrives.
    fn deadline(&self) -> Option<Instant>;

    /// Lets the controller's process go after a panic ended it.
    fn release_after_panic(&mut self);
}

/// What a controller's edge needs from the tracer thread it runs on.
pub(super) struct HostContext<'a> {
    hub: &'a WaitHub,
}

impl HostContext<'_> {
    /// Joins the wait hub, whose statuses for the member's process go to
    /// `sender`.
    pub(super) fn membership(&self, sender: mpsc::Sender<ControllerMessage>) -> HubMembership {
        let key = self.hub.register(sender);
        HubMembership {
            edge: self.hub.edge(key),
            key,
        }
    }
}

use tokio::sync::{mpsc, oneshot};

/// A controller's place in the wait hub, left when it is dropped.
pub(super) struct HubMembership {
    edge: HubEdge,
    key: MemberKey,
}

impl HubMembership {
    pub(super) const fn edge(&self) -> &HubEdge {
        &self.edge
    }
}

impl Drop for HubMembership {
    fn drop(&mut self) {
        self.edge.leave(self.key);
    }
}

type Build = Box<dyn FnOnce(&HostContext<'_>) -> Box<dyn Hosted> + Send>;

enum Command {
    Host {
        build: Build,
        ended: oneshot::Sender<thread::Result<()>>,
    },
}

/// A handle to a tracer thread, through which controllers join it.
pub(super) struct Tracer {
    commands: std_mpsc::Sender<Command>,
    thread: Thread,
}

/// What a controller's owner waits for once it asked it to end.
pub struct Hosting {
    ended: oneshot::Receiver<thread::Result<()>>,

    /// The tracer thread, for an owner that is the only one, so that the
    /// tracing lease is free once the controller has ended.
    thread: Option<JoinHandle<()>>,
}

impl Hosting {
    /// What the owner of a group's member waits for: the tracer thread is
    /// the group's, and outlives the member.
    pub(super) const fn member(ended: oneshot::Receiver<thread::Result<()>>) -> Self {
        Self {
            ended,
            thread: None,
        }
    }

    /// Waits for the controller to end, and for the tracer thread too
    /// when this hosting owns it. Fails when the controller panicked.
    pub fn join(self) -> std::result::Result<(), ()> {
        let ended = self.ended.blocking_recv().map_err(|_| ())?;
        if let Some(thread) = self.thread {
            thread.join().map_err(|_| ())?;
        }

        ended.map_err(|_| ())
    }
}

impl Tracer {
    /// Starts a tracer thread, which holds the process's tracing lease, and
    /// returns its handle and its join handle.
    pub(super) fn start(group: Option<GroupSetup>) -> Result<(Self, JoinHandle<()>)> {
        let lease = SessionLease::acquire()?;
        let (commands, received) = std_mpsc::channel();
        let joined = thread::Builder::new()
            .name(super::CONTROLLER_THREAD_NAME.into())
            .spawn(move || serve(&lease, &received, group))?;

        Ok((
            Self {
                commands,
                thread: joined.thread().clone(),
            },
            joined,
        ))
    }

    /// Starts a controller that `build` makes on the tracer thread.
    pub(super) fn host(&self, build: Build) -> Result<oneshot::Receiver<thread::Result<()>>> {
        let (ended, receiver) = oneshot::channel();
        self.commands
            .send(Command::Host { build, ended })
            .map_err(|_| crate::Error::BackendThreadPanicked)?;
        self.thread.unpark();
        Ok(receiver)
    }

    /// Starts a tracer thread serving one controller, which ends when it
    /// does.
    pub(super) fn alone(build: Build) -> Result<Hosting> {
        let (tracer, thread) = Self::start(None)?;
        let ended = tracer.host(build)?;
        Ok(Hosting {
            ended,
            thread: Some(thread),
        })
    }
}

impl Drop for Tracer {
    /// Wakes the thread, which ends once it has no controller left and no
    /// handle to it.
    fn drop(&mut self) {
        self.thread.unpark();
    }
}

struct Unparker(Thread);

impl Wake for Unparker {
    fn wake(self: Arc<Self>) {
        self.0.unpark();
    }

    fn wake_by_ref(self: &Arc<Self>) {
        self.0.unpark();
    }
}

struct Member {
    hosted: Box<dyn Hosted>,
    ended: Option<oneshot::Sender<thread::Result<()>>>,
}

/// The tracer thread's loop.
fn serve(_lease: &SessionLease, commands: &std_mpsc::Receiver<Command>, group: Option<GroupSetup>) {
    let hub = match WaitHub::new() {
        Ok(hub) => hub,
        #[cfg_attr(
            not(debug_assertions),
            expect(unused_variables, reason = "only recorded")
        )]
        Err(error) => {
            record!("the waiter did not start: {error}");
            return;
        }
    };
    let waker = Waker::from(Arc::new(Unparker(thread::current())));
    let mut members: Vec<Member> = Vec::new();
    let mut closed = false;
    let mut group = group.map(|setup| Group::new(setup, &HostContext { hub: &hub }));

    loop {
        loop {
            match commands.try_recv() {
                Ok(Command::Host { build, ended }) => {
                    let hosted = build(&HostContext { hub: &hub });
                    members.push(Member {
                        hosted,
                        ended: Some(ended),
                    });
                }
                Err(std_mpsc::TryRecvError::Empty) => break,
                Err(std_mpsc::TryRecvError::Disconnected) => {
                    closed = true;
                    break;
                }
            }
        }

        // A group's watched processes end only once the group has no
        // owner left, and every member has ended.
        let watching =
            closed && members.is_empty() && group.as_mut().is_some_and(|group| !group.close());
        if closed && members.is_empty() && !watching {
            break;
        }

        let mut served = false;
        members.retain_mut(|member| {
            let Some(message) = member.hosted.next_message() else {
                return true;
            };

            served = true;
            serve_one(member, message)
        });

        if let Some(group) = &mut group {
            let (joined, served_group) = group.serve(&HostContext { hub: &hub });
            served |= served_group;
            members.extend(joined.into_iter().map(|(hosted, ended)| Member {
                hosted,
                ended: Some(ended),
            }));
        }

        if served {
            continue;
        }

        let mut context = Context::from_waker(&waker);
        let mut ready = false;
        for member in &members {
            ready |= member.hosted.poll_queue(&mut context);
        }
        if let Some(group) = &mut group {
            ready |= group.poll(&mut context);
        }

        if ready {
            continue;
        }

        let deadline = members
            .iter()
            .filter_map(|member| member.hosted.deadline())
            .min();
        match deadline {
            Some(deadline) => {
                thread::park_timeout(deadline.saturating_duration_since(Instant::now()));
            }
            None if closed => thread::park_timeout(std::time::Duration::from_millis(50)),
            None => thread::park(),
        }
    }

    // Controllers end before the hub that routes their statuses.
    drop(members);
    drop(group);
    drop(hub);
}

/// Serves one message to `member`, and returns whether it keeps running.
fn serve_one(member: &mut Member, message: ControllerMessage) -> bool {
    let started = Instant::now();
    let served = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        member.hosted.handle_message(message)
    }));
    let took = started.elapsed();
    if took > LATENCY_BUDGET {
        crate::count!("tracer_slow_messages", 1);
        record!("a message took {took:?} on the tracer thread");
    }

    match served {
        Ok(true) => true,
        Ok(false) => {
            if let Some(ended) = member.ended.take() {
                let _ = ended.send(Ok(()));
            }
            false
        }
        Err(panic) => {
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                member.hosted.release_after_panic();
            }));
            if let Some(ended) = member.ended.take() {
                let _ = ended.send(Err(panic));
            }
            false
        }
    }
}

/// The longest one message should hold the tracer thread, which every
/// other controller on it waits through.
const LATENCY_BUDGET: std::time::Duration = std::time::Duration::from_millis(50);
