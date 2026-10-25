//! One waiter for every process a tracer thread traces.
//!
//! `waitpid(-1)` returns any traced child's status, so two waiters in one
//! process would take each other's. The hub is the only one: it polls for
//! every controller its tracer thread serves and routes each status to the
//! controller that owns the thread it reports on. A thread is owned by the
//! controller whose process is its thread group, or else, for a new process
//! whose fork event has not been seen yet, by its parent's controller, which
//! settles fork children. Each routed thread is remembered until its exit
//! status, the last it reports, so a status read after `/proc` forgot the
//! thread still reaches its owner.

use std::collections::{BTreeMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::{self, JoinHandle, Thread};
use std::time::{Duration, Instant};

use nix::errno::Errno;
use nix::libc;
use nix::unistd::Pid;
use tokio::sync::mpsc;

use super::signals::WaitEvent;
use super::{ControllerMessage, WAITER_THREAD_NAME};
use crate::Result;

/// The shortest interval between polls while statuses keep arriving.
const MIN_POLL: Duration = Duration::from_micros(20);

/// The longest poll interval while every process runs without reporting.
const MAX_POLL: Duration = Duration::from_millis(5);

/// Identifies one controller the hub routes statuses to.
pub(super) type MemberKey = u64;

/// The waiter shared by a tracer thread's controllers. Dropping it stops
/// its thread.
pub(super) struct WaitHub {
    shared: Arc<Shared>,
    thread: Option<JoinHandle<()>>,
}

struct Shared {
    routes: Mutex<Routes>,
    stop: AtomicBool,

    /// Counts the waiter's loops, so a controller that stops waiting can
    /// tell that a status taken before it stopped has been delivered.
    epoch: AtomicU64,
    waiter: Mutex<Option<Thread>>,
}

#[derive(Default)]
struct Routes {
    members: BTreeMap<MemberKey, Member>,

    /// The member each thread already routed belongs to.
    owners: BTreeMap<Pid, MemberKey>,
    direct: BTreeMap<Pid, std::sync::mpsc::Sender<WaitEvent>>,
    next_key: MemberKey,
}

struct Member {
    sender: mpsc::Sender<ControllerMessage>,

    /// The thread group of the process the member traces, once it has one.
    process: Option<Pid>,

    /// Whether the member waits for statuses, as it does while its process
    /// lives.
    active: bool,

    /// Statuses a full queue could not take yet, in order.
    backlog: VecDeque<WaitEvent>,
}

impl WaitHub {
    pub(super) fn new() -> Result<Self> {
        let shared = Arc::new(Shared {
            routes: Mutex::default(),
            stop: AtomicBool::new(false),
            epoch: AtomicU64::new(0),
            waiter: Mutex::new(None),
        });
        let thread_shared = Arc::clone(&shared);
        let thread = thread::Builder::new()
            .name(WAITER_THREAD_NAME.into())
            .spawn(move || thread_shared.run())?;
        *shared.waiter.lock().unwrap_or_else(PoisonError::into_inner) =
            Some(thread.thread().clone());

        Ok(Self {
            shared,
            thread: Some(thread),
        })
    }

    /// Registers a controller whose statuses go to `sender`, before it has
    /// a process to wait for.
    pub(super) fn register(&self, sender: mpsc::Sender<ControllerMessage>) -> MemberKey {
        let mut routes = self.shared.lock();
        let key = routes.next_key;
        routes.next_key += 1;
        routes.members.insert(
            key,
            Member {
                sender,
                process: None,
                active: false,
                backlog: VecDeque::new(),
            },
        );
        key
    }

    /// The handle a controller's edge uses to wait for its process.
    pub(super) fn edge(&self, key: MemberKey) -> HubEdge {
        HubEdge {
            shared: Arc::clone(&self.shared),
            key,
        }
    }
}

impl Drop for WaitHub {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Release);
        self.shared.wake();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// One controller's view of the hub.
#[derive(Clone)]
pub(super) struct HubEdge {
    shared: Arc<Shared>,
    key: MemberKey,
}

impl HubEdge {
    pub(super) fn intercept(&self, pid: Pid) -> Option<DirectWait> {
        let (sender, receiver) = std::sync::mpsc::channel();
        let mut routes = self.shared.lock();
        if routes.direct.contains_key(&pid) {
            return None;
        }
        routes.direct.insert(pid, sender);
        drop(routes);
        Some(DirectWait {
            edge: self.clone(),
            pid,
            receiver,
        })
    }

    /// Starts waiting for the process `process`, whose statuses go to the
    /// member's queue until [`Self::stop_and_join`].
    pub(super) fn start(&self, process: Pid) {
        let mut routes = self.shared.lock();
        let member = routes.members.get_mut(&self.key);
        debug_assert!(member.is_some(), "a member waits only while registered");
        if let Some(member) = member {
            member.process = Some(process);
            member.active = true;
        }
        routes.owners.insert(process, self.key);
        drop(routes);
        self.shared.wake();
    }

    /// Forgets the member, once its controller has ended.
    pub(super) fn leave(&self, key: MemberKey) {
        let mut routes = self.shared.lock();
        routes.members.remove(&key);
        routes.owners.retain(|_, owner| *owner != key);
    }

    /// Makes the waiter poll promptly for a status a request just caused.
    pub(super) fn wake(&self) {
        self.shared.wake();
    }

    /// Stops waiting for the member's process, calling `serve` until every
    /// status the waiter took before is in the member's queue. While other
    /// members wait, the waiter goes on, and may still route a late status
    /// of this member's threads to its queue.
    pub(super) fn stop_and_join(&self, mut serve: impl FnMut()) {
        if let Some(member) = self.shared.lock().members.get_mut(&self.key) {
            member.active = false;
        }
        let started = self.shared.epoch.load(Ordering::Acquire);

        loop {
            serve();
            self.shared.wake();
            let delivered = self
                .shared
                .lock()
                .members
                .get(&self.key)
                .is_none_or(|member| member.backlog.is_empty());
            if delivered && self.shared.epoch.load(Ordering::Acquire) >= started + 2 {
                return;
            }

            thread::sleep(Duration::from_millis(1));
        }
    }
}

pub(super) struct DirectWait {
    edge: HubEdge,
    pid: Pid,
    pub(super) receiver: std::sync::mpsc::Receiver<WaitEvent>,
}

impl DirectWait {
    pub(super) fn forward(&self, status: WaitEvent) {
        let mut routes = self.edge.shared.lock();
        if let Some(member) = routes.members.get_mut(&self.edge.key) {
            member.backlog.push_back(status);
        }
    }
}

impl Drop for DirectWait {
    fn drop(&mut self) {
        let mut routes = self.edge.shared.lock();
        routes.direct.remove(&self.pid);
        while let Ok(status) = self.receiver.try_recv() {
            routes.route(status);
        }
        drop(routes);
    }
}

impl Shared {
    fn lock(&self) -> std::sync::MutexGuard<'_, Routes> {
        self.routes.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn wake(&self) {
        if let Some(waiter) = self
            .waiter
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .as_ref()
        {
            waiter.unpark();
        }
    }

    fn run(&self) {
        let mut interval = MIN_POLL;

        while !self.stop.load(Ordering::Acquire) {
            let waiting = {
                let mut routes = self.lock();
                routes.flush_backlogs();
                routes.members.values().any(|member| member.active)
            };

            if !waiting {
                thread::park_timeout(MAX_POLL);
                self.epoch.fetch_add(1, Ordering::AcqRel);
                continue;
            }

            match super::native::wait_for(Pid::from_raw(-1), libc::__WALL | libc::WNOHANG) {
                Ok(Some(status)) => {
                    interval = MIN_POLL;
                    self.lock().route(status);
                }
                Ok(None) | Err(Errno::ECHILD) => {
                    let parked = Instant::now();
                    thread::park_timeout(interval);

                    // Returning early means a controller woke the waiter.
                    interval = if parked.elapsed() < interval {
                        MIN_POLL
                    } else {
                        interval.saturating_mul(2).min(MAX_POLL)
                    };
                }
                Err(_) => thread::park_timeout(MAX_POLL),
            }

            self.epoch.fetch_add(1, Ordering::AcqRel);
        }

        record!("exited");
    }
}

impl Routes {
    /// Sends `status` to the member that owns its thread.
    fn route(&mut self, status: WaitEvent) {
        let pid = status.pid();
        if let Some(sender) = self.direct.get(&pid)
            && sender.send(status).is_ok()
        {
            return;
        }
        let Some(key) = self.owner_of(pid) else {
            record!("no member owns {pid}, whose status {status:?} is dropped");
            return;
        };

        if matches!(status, WaitEvent::Exited(..) | WaitEvent::Signaled(..)) {
            self.owners.remove(&pid);
        } else {
            self.owners.insert(pid, key);
        }

        let Some(member) = self.members.get_mut(&key) else {
            return;
        };

        if !member.backlog.is_empty() {
            member.backlog.push_back(status);
            return;
        }

        if let Some(status) = deliver(&member.sender, status) {
            member.backlog.push_back(status);
        }
    }

    /// Sends what full queues could not take before, as they have room.
    fn flush_backlogs(&mut self) {
        for member in self.members.values_mut() {
            while let Some(status) = member.backlog.pop_front() {
                if let Some(status) = deliver(&member.sender, status) {
                    member.backlog.push_front(status);
                    break;
                }
            }
        }
    }

    /// The member a thread belongs to: the one it was routed to before,
    /// or the one whose process is its thread group, or its parent's, or
    /// the only member there is.
    fn owner_of(&self, pid: Pid) -> Option<MemberKey> {
        if let Some(key) = self.owners.get(&pid) {
            return Some(*key);
        }

        let of_process = |process: Pid| {
            self.owners.get(&process).copied().or_else(|| {
                self.members
                    .iter()
                    .find(|(_, member)| member.process == Some(process))
                    .map(|(key, _)| *key)
            })
        };

        let status = super::proc_status::read_path(format!("/proc/{pid}/status")).ok();
        let field = |read: fn(&[u8]) -> Option<u32>| {
            status
                .as_deref()
                .and_then(read)
                .and_then(|value| i32::try_from(value).ok())
                .map(Pid::from_raw)
        };
        field(super::proc_status::tgid)
            .and_then(of_process)
            .or_else(|| field(super::proc_status::parent_pid).and_then(of_process))
            .or_else(|| {
                let mut members = self.members.keys();
                match (members.next(), members.next()) {
                    (Some(only), None) => Some(*only),
                    _ => None,
                }
            })
    }
}

/// Sends `status` to a member's queue, and returns it when the queue is
/// full. A member whose queue closed has ended, and needs none.
fn deliver(sender: &mpsc::Sender<ControllerMessage>, status: WaitEvent) -> Option<WaitEvent> {
    match sender.try_send(ControllerMessage::Wait(status)) {
        Err(mpsc::error::TrySendError::Full(ControllerMessage::Wait(status))) => Some(status),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_direct_wait_returns_unconsumed_statuses_in_order_and_releases_its_route() {
        let (sender, mut receiver) = mpsc::channel(8);
        let pid = Pid::from_raw(123);
        let mut routes = Routes::default();
        routes.members.insert(
            0,
            Member {
                sender,
                process: Some(pid),
                active: false,
                backlog: VecDeque::new(),
            },
        );
        routes.owners.insert(pid, 0);
        let edge = HubEdge {
            key: 0,
            shared: Arc::new(Shared {
                routes: Mutex::new(routes),
                stop: AtomicBool::new(false),
                epoch: AtomicU64::new(0),
                waiter: Mutex::new(None),
            }),
        };
        let wait = edge.intercept(pid).unwrap();
        assert!(edge.intercept(pid).is_none());
        let first = WaitEvent::Stopped(pid, super::super::Signal::SIGUSR1);
        let second = WaitEvent::Stopped(pid, super::super::Signal::SIGTRAP);
        edge.shared.lock().route(first);
        assert_eq!(
            wait.receiver.recv_timeout(Duration::from_secs(1)).unwrap(),
            first
        );
        wait.forward(first);
        edge.shared.lock().route(second);
        drop(wait);
        edge.shared.lock().flush_backlogs();
        for expected in [first, second] {
            assert!(
                matches!(receiver.try_recv().unwrap(), ControllerMessage::Wait(actual) if actual == expected)
            );
        }
        assert!(edge.shared.lock().direct.is_empty());
        assert!(edge.intercept(pid).is_some());
    }
}
