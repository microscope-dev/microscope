use std::sync::{Arc, mpsc as std_mpsc};
use std::task::{Context, Wake, Waker};
use std::thread::{self, JoinHandle, Thread};
use std::time::Instant;

use super::follow::{Group, GroupSetup};
use super::wait_hub::{HubEdge, MemberKey, WaitHub};
use super::{ControllerMessage, SessionLease};
use crate::Result;

pub(super) trait Hosted {
    fn next_message(&self) -> Option<ControllerMessage>;

    fn handle_message(&mut self, message: ControllerMessage) -> bool;

    fn poll_queue(&self, context: &mut Context<'_>) -> bool;

    fn deadline(&self) -> Option<Instant>;

    fn release_after_panic(&mut self);
}

pub(super) struct HostContext<'a> {
    hub: &'a WaitHub,
}

impl HostContext<'_> {
    pub(super) fn membership(&self, sender: mpsc::Sender<ControllerMessage>) -> HubMembership {
        let key = self.hub.register(sender);
        HubMembership {
            edge: self.hub.edge(key),
            key,
        }
    }
}

use tokio::sync::{mpsc, oneshot};

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

pub(super) struct Tracer {
    commands: Option<std_mpsc::Sender<Command>>,
    thread: Thread,
}

pub struct Hosting {
    ended: oneshot::Receiver<thread::Result<()>>,

    thread: Option<JoinHandle<()>>,
}

impl Hosting {
    pub(super) const fn member(ended: oneshot::Receiver<thread::Result<()>>) -> Self {
        Self {
            ended,
            thread: None,
        }
    }

    pub fn join(self) -> std::result::Result<(), ()> {
        let ended = self.ended.blocking_recv().map_err(|_| ())?;
        if let Some(thread) = self.thread {
            thread.join().map_err(|_| ())?;
        }

        ended.map_err(|_| ())
    }
}

impl Tracer {
    pub(super) fn start(group: Option<GroupSetup>) -> Result<(Self, JoinHandle<()>)> {
        let lease = SessionLease::acquire()?;
        let (commands, received) = std_mpsc::channel();
        let joined = thread::Builder::new()
            .name(super::CONTROLLER_THREAD_NAME.into())
            .spawn(move || serve(&lease, &received, group))?;

        Ok((
            Self {
                commands: Some(commands),
                thread: joined.thread().clone(),
            },
            joined,
        ))
    }

    pub(super) fn host(&self, build: Build) -> Result<oneshot::Receiver<thread::Result<()>>> {
        let (ended, receiver) = oneshot::channel();
        self.commands
            .as_ref()
            .ok_or(crate::Error::BackendThreadPanicked)?
            .send(Command::Host { build, ended })
            .map_err(|_| crate::Error::BackendThreadPanicked)?;
        self.thread.unpark();
        Ok(receiver)
    }

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
    fn drop(&mut self) {
        drop(self.commands.take());
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
        if hub.failed() {
            if let Some(group) = &mut group {
                let (joined, _) = group.serve(&HostContext { hub: &hub });
                members.extend(joined.into_iter().map(|(hosted, ended)| Member {
                    hosted,
                    ended: Some(ended),
                }));
            }

            for member in &mut members {
                release(member, Box::new("the waiter panicked"));
            }

            break;
        }

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

    drop(members);
    drop(group);
    drop(hub);
}

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
            release(member, panic);
            false
        }
    }
}

fn release(member: &mut Member, panic: Box<dyn std::any::Any + Send>) {
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        member.hosted.release_after_panic();
    }));
    if let Some(ended) = member.ended.take() {
        let _ = ended.send(Err(panic));
    }
}

const LATENCY_BUDGET: std::time::Duration = std::time::Duration::from_millis(50);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_idle_tracer_thread_ends_once_its_handle_is_dropped() {
        for _ in 0..500 {
            let (tracer, thread) = Tracer::start(None).expect("start a tracer thread");
            thread::sleep(std::time::Duration::from_micros(200));
            drop(tracer);
            let (done, ended) = std_mpsc::channel();
            thread::spawn(move || {
                let _ = done.send(thread.join().is_ok());
            });
            assert_eq!(
                ended.recv_timeout(std::time::Duration::from_secs(5)),
                Ok(true),
                "the tracer thread did not end"
            );
        }
    }
}
