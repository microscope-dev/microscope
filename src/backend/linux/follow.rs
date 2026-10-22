//! Following the processes a program starts.
//!
//! A group's processes are all traced by one tracer thread. A fork child of
//! a member is handed from its parent's controller to a controller of its
//! own on the same thread, and so never runs untraced. A process the group
//! does not debug, such as a shell between npm and node, is watched: the
//! thread resumes every stop it makes at once and decides again at each
//! exec, where a process that now runs a program the group debugs gets a
//! controller whose first stop is that exec.

use std::collections::{BTreeMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::task::Context;

use nix::libc;
use nix::unistd::Pid;
use tokio::sync::{broadcast, mpsc, oneshot, watch};

use super::native::{LinuxPtrace, LinuxTraceOps};
use super::signals::{Signal, WaitEvent};
use super::tracer::{HostContext, Hosted, Hosting};
use super::{Controller, Executable, Start, process_id};
use crate::backend::{ControllerChannels, ControllerJoin, ControllerMessage};
use crate::debug_info::{DebugFileSearch, DebugInfo};
use crate::{
    DebuggerEvent, ExecutionId, FollowMode, FollowPolicy, ModuleImage, ProcessId, Result, StopId,
};

/// A fork child its parent's controller hands to the tracer thread: still
/// traced by it, stopped in its first stop, and clean of the traps it
/// inherited.
pub(super) struct Transfer {
    pub(super) child: Pid,
    pub(super) parent: Pid,

    /// Whether it is a vfork child, which shares its parent's memory until
    /// it executes a program.
    pub(super) vfork: bool,

    /// The program it runs, which is its parent's, already read.
    pub(super) program: Option<(Executable, DebugInfo)>,

    /// Whether it descends from a launched process, which the group ends
    /// rather than leaving to run.
    pub(super) launched: bool,
}

/// A process that changes hands on the tracer thread.
enum Handover {
    Fork(Transfer),

    /// A watched process that executed a program the group debugs,
    /// stopped at its exec event, which its new controller is given.
    Exec {
        pid: Pid,
        parent: Option<Pid>,
        launched: bool,
        status: WaitEvent,
    },

    /// A member whose process executed a program the group watches,
    /// stopped at its exec event, which its controller let go of.
    Demote {
        pid: Pid,
        parent: Option<Pid>,
        launched: bool,
        program: PathBuf,
    },
}

/// What a group's controllers share: its policy, and the processes they
/// hand to the tracer thread, which takes each once the message that
/// found it has been served.
pub(super) struct Following {
    policy: Policy,
    handovers: Mutex<VecDeque<Handover>>,
}

impl Following {
    pub(super) fn new(policy: Policy) -> Self {
        Self {
            policy,
            handovers: Mutex::default(),
        }
    }

    pub(super) fn transfer(&self, transfer: Transfer) {
        self.push(Handover::Fork(transfer));
    }

    /// Hands over a member's process, stopped at the exec of `program`,
    /// which the group watches.
    pub(super) fn demote(&self, pid: Pid, parent: Option<Pid>, launched: bool, program: PathBuf) {
        self.push(Handover::Demote {
            pid,
            parent,
            launched,
            program,
        });
    }

    /// Whether a member that executed `program` stays one.
    pub(super) fn debugs(&self, program: &Path) -> bool {
        let inode = std::fs::metadata(program)
            .ok()
            .map(|metadata| std::os::unix::fs::MetadataExt::ino(&metadata));
        self.policy.decide(program, inode) == Decision::Debug
    }

    fn push(&self, handover: Handover) {
        self.handovers
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push_back(handover);
    }

    /// The fork children handed over so far, as (child, parent, vfork).
    #[cfg(test)]
    pub(super) fn take_forks(&self) -> Vec<(Pid, Pid, bool)> {
        std::iter::from_fn(|| self.take())
            .filter_map(|handover| match handover {
                Handover::Fork(transfer) => Some((transfer.child, transfer.parent, transfer.vfork)),
                Handover::Exec { .. } | Handover::Demote { .. } => None,
            })
            .collect()
    }

    fn take(&self) -> Option<Handover> {
        self.handovers
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .pop_front()
    }
}

/// A controller's place in its group.
pub(super) struct Membership {
    pub(super) following: Arc<Following>,

    /// Whether the controller's process is the group's root, which follows
    /// every program it executes, rather than one it started.
    pub(super) root: bool,

    /// The member whose process started this one's, if any.
    pub(super) parent: Option<Pid>,
}

impl<P: LinuxTraceOps> Controller<P> {
    /// Makes the session a member of a group, whose fork children go to
    /// `following`.
    pub(super) fn join_group(&mut self, membership: Membership) {
        self.group = Some(membership);
    }

    /// The program a fork child of the inferior runs, which is the main
    /// program the inferior runs now, already read.
    pub(super) fn forked_program(&self, child: Pid) -> Option<(Executable, DebugInfo)> {
        let main = self.modules.get(&self.main.id)?;
        Some((
            Executable {
                display_path: Arc::clone(&self.main.executable),
                launch_path: Arc::clone(&self.main.executable),
                identity: self.main.identity,
                process_start_time: self.ptrace.process_start_time(child),
                debug_files: self.debug_files.clone(),
                c_library: self.main.c_library,
            },
            DebugInfo {
                image: Arc::clone(&self.main.image),
                unwind: Arc::clone(&self.main.unwind),
                variables: Arc::clone(&main.variables),
            },
        ))
    }

    /// The group's queue, when the session hands its fork children to one.
    pub(super) fn following(&self) -> Option<&Arc<Following>> {
        self.group.as_ref().map(|group| &group.following)
    }

    /// Whether a member, stopped at the exec of `program` with no other
    /// thread left, leaves the group's debugging for its watching: the
    /// group's root never does, nor a process the policy debugs.
    pub(super) fn demotes_at_exec(&self, program: &Path) -> bool {
        self.group
            .as_ref()
            .is_some_and(|group| !group.root && !group.following.debugs(program))
            && self
                .inferior
                .as_ref()
                .is_some_and(|inferior| inferior.retired_threads.is_empty())
    }
}

/// What a group decides about a process whose program it now knows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Decision {
    /// Debug it as a member of its own.
    Debug,

    /// Keep it traced without debugging it, until it executes another
    /// program.
    Watch,
}

/// A group's follow policy, with what it needs to judge a program.
pub(super) struct Policy {
    pub(super) policy: FollowPolicy,

    /// The inode of the program the group was started for, which `auto`
    /// debugs wherever it runs again.
    pub(super) launched: Option<u64>,
}

impl Policy {
    /// Whether the group debugs a process that runs `program`, whose inode
    /// is `inode`.
    pub(super) fn decide(&self, program: &Path, inode: Option<u64>) -> Decision {
        let named = program
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| {
                self.policy
                    .programs
                    .iter()
                    .any(|pattern| matches_glob(pattern, name))
            });
        let debug = match self.policy.mode {
            FollowMode::All => true,
            FollowMode::None => false,
            FollowMode::Auto => {
                named
                    || inode.is_some_and(|inode| self.launched == Some(inode))
                    || program_runtime(program).is_some()
            }
        };

        if debug {
            Decision::Debug
        } else {
            Decision::Watch
        }
    }
}

/// The language runtime a program runs that microscope models, by the
/// marks it leaves in the file: V8's `v8dbg_` symbols or a `libnode`
/// dependency for Node, CPython's `_PyRuntime` or a `libpython3`
/// dependency, or Go's build information. A runtime in a library, as
/// Debian's Node and nixpkgs' CPython keep theirs, is found by the
/// executable's dependency on it, which its dynamic string table names,
/// so no library needs to load first. Only headers, the dynamic string
/// table, and symbol names are read.
pub(super) fn program_runtime(program: &Path) -> Option<&'static str> {
    use object::{Object as _, ObjectSection as _, ObjectSymbol as _};

    let file = std::fs::File::open(program).ok()?;
    let cache = object::read::ReadCache::new(file);
    let object = object::File::parse(&cache).ok()?;

    if object.section_by_name(".go.buildinfo").is_some() {
        return Some("go");
    }

    if let Some(strings) = object
        .section_by_name(".dynstr")
        .and_then(|section| section.data().ok())
    {
        let needs = |name: &[u8]| {
            strings
                .split(|byte| *byte == 0)
                .any(|entry| entry.starts_with(name))
        };
        if needs(b"libnode.so") {
            return Some("node");
        }
        if needs(b"libpython3.") {
            return Some("python");
        }
    }

    for symbol in object.dynamic_symbols().chain(object.symbols()) {
        match symbol.name_bytes() {
            Ok(name) if name.starts_with(b"v8dbg_") => return Some("node"),
            Ok(b"_PyRuntime") => return Some("python"),
            _ => {}
        }
    }

    None
}

/// Whether `name` matches `pattern`, where `*` matches any run of
/// characters and `?` any one.
pub(super) fn matches_glob(pattern: &str, name: &str) -> bool {
    let pattern = pattern.chars().collect::<Vec<_>>();
    let name = name.chars().collect::<Vec<_>>();
    let (mut p, mut n) = (0, 0);
    let mut star: Option<(usize, usize)> = None;

    while let Some(&current) = name.get(n) {
        match pattern.get(p) {
            Some('*') => {
                star = Some((p, n));
                p += 1;
            }
            Some(&c) if c == '?' || c == current => {
                p += 1;
                n += 1;
            }
            _ => match star {
                Some((star_p, star_n)) => {
                    p = star_p + 1;
                    n = star_n + 1;
                    star = Some((star_p, star_n + 1));
                }
                None => return false,
            },
        }
    }

    pattern
        .get(p..)
        .is_some_and(|rest| rest.iter().all(|c| *c == '*'))
}

/// The program a process runs now, as `/proc` names it, and its inode.
fn running_program(pid: Pid) -> Option<(PathBuf, u64)> {
    use std::os::unix::fs::MetadataExt as _;

    let link = format!("/proc/{pid}/exe");
    let path = std::fs::read_link(&link).ok()?;
    let inode = std::fs::metadata(&link).ok()?.ino();
    Some((path, inode))
}

/// How large a member's queues are, as the library sizes a debugger's.
#[derive(Debug, Clone, Copy)]
pub struct Capacity {
    pub requests: usize,
    pub events: usize,
}

/// What a group's owner learns from its tracer thread.
pub enum GroupNotice {
    /// A process became a member, debugged by a controller of its own,
    /// which has yet to publish its first stop.
    Joined(Joined),

    /// A process is watched: it runs `program`, which the group does not
    /// debug, and stays traced in case it executes one it does.
    Watched {
        process: ProcessId,
        parent: Option<ProcessId>,
        program: PathBuf,
    },

    /// A process executed a program the group would debug but cannot
    /// load, and runs on watched.
    Unfollowed {
        process: ProcessId,
        program: PathBuf,
        reason: String,
    },
}

/// A new member's controller and the channels to it.
pub struct Joined {
    pub process: ProcessId,

    /// The process that started it, a member or a watched process.
    pub parent: Option<ProcessId>,

    /// The program it joined by executing, or none for a fork child,
    /// which runs its parent's.
    pub executed: Option<PathBuf>,
    pub requests: mpsc::Sender<ControllerMessage>,
    pub events: broadcast::Sender<DebuggerEvent>,
    pub program: watch::Receiver<Arc<ModuleImage>>,
    pub launched: Arc<PathBuf>,
    pub controller: ControllerJoin,
    pub started: Started,
}

/// The first stop a new member publishes, before which it runs nothing.
pub enum Started {
    Fork(oneshot::Receiver<Result<StopId>>),
    Exec(oneshot::Receiver<Result<ExecutionId>>),
}

impl Started {
    /// Waits until the member has published its first stop.
    pub async fn wait(self) -> Result<()> {
        let cancelled = |_| crate::Error::RequestCancelled;
        match self {
            Self::Fork(started) => started.await.map_err(cancelled)?.map(drop),
            Self::Exec(started) => started.await.map_err(cancelled)?.map(drop),
        }
    }
}

/// What a tracer thread needs to serve a group, made where the group is.
pub(super) struct GroupSetup {
    pub(super) following: Arc<Following>,
    pub(super) notices: mpsc::UnboundedSender<GroupNotice>,
    pub(super) capacity: Capacity,
    pub(super) debug_files: DebugFileSearch,
}

/// A new controller for the tracer thread to serve, and where it reports
/// that it ended.
pub(super) type NewMember = (Box<dyn Hosted>, oneshot::Sender<std::thread::Result<()>>);

/// A group as its tracer thread serves it.
pub(super) struct Group {
    setup: GroupSetup,
    watcher: Watcher,
}

impl Group {
    pub(super) fn new(setup: GroupSetup, host: &HostContext<'_>) -> Self {
        let (sender, messages) = mpsc::channel(WATCHER_CAPACITY);
        let membership = host.membership(sender);
        Self {
            watcher: Watcher {
                ptrace: LinuxPtrace::hosted(membership),
                messages,
                pending: VecDeque::new(),
                processes: BTreeMap::new(),
                threads: BTreeMap::new(),
                fresh: Vec::new(),
                following: Arc::clone(&setup.following),
                notices: setup.notices.clone(),
                closing: false,
            },
            setup,
        }
    }

    /// Takes the processes controllers handed over and the statuses of
    /// watched ones, returning the members made for them, and whether
    /// anything was served.
    pub(super) fn serve(&mut self, host: &HostContext<'_>) -> (Vec<NewMember>, bool) {
        let mut served = self.watcher.serve();
        let mut members = Vec::new();

        while let Some(handover) = self.setup.following.take() {
            served = true;
            if let Some(member) = self.hand_over(handover, host) {
                members.push(member);
            }
        }

        (members, served)
    }

    /// Whether a watched process has a status waiting, arranging for
    /// `context` to be woken when one arrives otherwise.
    pub(super) fn poll(&mut self, context: &mut Context<'_>) -> bool {
        self.watcher.poll(context)
    }

    /// Ends watching: launched processes are killed, attached ones
    /// released. Returns whether every watched process is gone.
    pub(super) fn close(&mut self) -> bool {
        self.watcher.close();
        self.watcher.processes.is_empty()
    }

    fn hand_over(&mut self, handover: Handover, host: &HostContext<'_>) -> Option<NewMember> {
        match handover {
            Handover::Fork(transfer) if transfer.vfork || self.watcher.closing => {
                self.watcher.watch(
                    transfer.child,
                    Some(transfer.parent),
                    transfer.launched,
                    None,
                );
                None
            }
            Handover::Fork(Transfer {
                child,
                parent,
                program,
                launched,
                ..
            }) => {
                if let Some((executable, debug_info)) = program {
                    return Some(
                        self.adopt_fork(host, child, parent, launched, executable, debug_info),
                    );
                }

                self.watcher.watch(child, Some(parent), launched, None);
                None
            }
            Handover::Exec {
                pid,
                parent,
                launched,
                status,
            } => self.adopt_exec(host, pid, parent, launched, status),
            Handover::Demote {
                pid,
                parent,
                launched,
                program,
            } => {
                self.watcher.watch(pid, parent, launched, Some(program));
                None
            }
        }
    }

    fn adopt_fork(
        &self,
        host: &HostContext<'_>,
        pid: Pid,
        parent: Pid,
        launched: bool,
        executable: Executable,
        debug_info: DebugInfo,
    ) -> NewMember {
        let (reply, started) = oneshot::channel();
        self.member(
            host,
            executable,
            debug_info,
            (pid, Some(parent), None),
            |controller| {
                controller.start(Start::Adopt {
                    pid,
                    launched,
                    reply,
                });
            },
            Started::Fork(started),
        )
    }

    fn adopt_exec(
        &mut self,
        host: &HostContext<'_>,
        pid: Pid,
        parent: Option<Pid>,
        launched: bool,
        status: WaitEvent,
    ) -> Option<NewMember> {
        let program =
            crate::backend::process_executable_source(process_id(pid)).and_then(|mut source| {
                source.debug_files = self.setup.debug_files.clone();
                let debug_info = crate::debug_info::load_program(
                    &source.display_path,
                    &source.data,
                    &source.debug_files,
                )?;
                Ok((source.described(), debug_info))
            });

        let (executable, debug_info) = match program {
            Ok(program) => program,
            Err(error) => {
                let program = running_program(pid)
                    .map(|(path, _)| path)
                    .unwrap_or_default();
                let _ = self.setup.notices.send(GroupNotice::Unfollowed {
                    process: process_id(pid),
                    program,
                    reason: error.to_string(),
                });
                self.watcher.watch(pid, parent, launched, None);
                return None;
            }
        };

        let executed = Some(executable.display_path.to_path_buf());
        let (reply, started) = oneshot::channel();
        Some(self.member(
            host,
            executable,
            debug_info,
            (pid, parent, executed),
            |controller| {
                controller.start(Start::AdoptExec {
                    pid,
                    launched,
                    reply,
                });
                // The exec stop the watcher took is the launch's initial
                // stop, which the new controller serves next.
                let _ = controller
                    .message_sender
                    .try_send(ControllerMessage::Wait(status));
            },
            Started::Exec(started),
        ))
    }

    /// Makes a member's controller for `pid`, which `start` begins.
    fn member(
        &self,
        host: &HostContext<'_>,
        executable: Executable,
        debug_info: DebugInfo,
        (pid, parent, executed): (Pid, Option<Pid>, Option<PathBuf>),
        start: impl FnOnce(&mut Controller<super::HostedEdge>),
        started: Started,
    ) -> NewMember {
        let (requests, receiver) = mpsc::channel(self.setup.capacity.requests);
        let (events, _) = broadcast::channel(self.setup.capacity.events);
        let program = watch::Sender::new(Arc::clone(&debug_info.image));
        let current = program.subscribe();
        let launched = Arc::clone(&executable.launch_path);
        let mut controller = super::hosted_controller(
            host,
            executable,
            debug_info,
            ControllerChannels {
                sender: requests.clone(),
                receiver,
                events: events.clone().into(),
            },
            program,
        );
        controller.join_group(Membership {
            following: Arc::clone(&self.setup.following),
            root: false,
            parent,
        });
        start(&mut controller);

        let (ended, ended_receiver) = oneshot::channel();
        let _ = self.setup.notices.send(GroupNotice::Joined(Joined {
            process: process_id(pid),
            parent: parent.map(process_id),
            executed,
            requests,
            events,
            program: current,
            launched,
            controller: ControllerJoin::Hosted(Hosting::member(ended_receiver)),
            started,
        }));

        (Box::new(controller), ended)
    }
}

/// How many statuses the watcher's queue holds before the hub keeps the
/// rest in order for it.
const WATCHER_CAPACITY: usize = 256;

/// A process the group watches.
struct Watched {
    parent: Option<Pid>,
    launched: bool,
}

/// The processes a group traces without debugging. Each stop is resumed
/// at once with its signal, so a watched process runs as it would
/// untraced; only its forks, threads, execs, and exits are noted.
struct Watcher {
    ptrace: LinuxPtrace,
    messages: mpsc::Receiver<ControllerMessage>,
    pending: VecDeque<ControllerMessage>,

    /// Watched processes by thread group.
    processes: BTreeMap<Pid, Watched>,

    /// The thread group of each watched thread.
    threads: BTreeMap<Pid, Pid>,

    /// New threads whose first stop, for the `SIGSTOP` that stopped them
    /// for the tracer, is yet to be resumed without it.
    fresh: Vec<Pid>,
    following: Arc<Following>,
    notices: mpsc::UnboundedSender<GroupNotice>,

    /// Whether the group is ending, so that a process is ended or released
    /// at its next stop rather than resumed.
    closing: bool,
}

impl Watcher {
    /// Watches `pid`, a process stopped in a stop another owner took, and
    /// resumes it. `program` names what it executed, when it left a member
    /// for that.
    fn watch(&mut self, pid: Pid, parent: Option<Pid>, launched: bool, program: Option<PathBuf>) {
        self.processes.insert(pid, Watched { parent, launched });
        self.threads.insert(pid, pid);
        // Statuses of the process come here from now on.
        self.ptrace.route_here(pid);

        if let Some(program) = program {
            let _ = self.notices.send(GroupNotice::Watched {
                process: process_id(pid),
                parent: parent.map(process_id),
                program,
            });
        }

        if self.closing {
            self.end(pid);
        } else {
            self.resume(pid, None);
        }
    }

    fn serve(&mut self) -> bool {
        let mut served = false;
        while let Some(message) = self
            .pending
            .pop_front()
            .or_else(|| self.messages.try_recv().ok())
        {
            served = true;
            match message {
                ControllerMessage::Wait(status) => self.handle(status),
                ControllerMessage::Request(_) | ControllerMessage::AgentTimeout => {}
            }
        }

        served
    }

    fn poll(&mut self, context: &mut Context<'_>) -> bool {
        match self.messages.poll_recv(context) {
            std::task::Poll::Ready(Some(message)) => {
                self.pending.push_back(message);
                true
            }
            std::task::Poll::Ready(None) | std::task::Poll::Pending => false,
        }
    }

    fn close(&mut self) {
        if self.closing {
            return;
        }

        self.closing = true;
        let processes = self.processes.keys().copied().collect::<Vec<_>>();
        for pid in processes {
            self.end(pid);
        }
    }

    /// Kills a launched process, or stops an attached one's threads so
    /// that each is released at its stop.
    fn end(&self, process: Pid) {
        if self
            .processes
            .get(&process)
            .is_some_and(|watched| watched.launched)
        {
            let _ = self.ptrace.kill(process, Signal::SIGKILL);
            return;
        }

        for (&thread, &group) in &self.threads {
            if group == process {
                let _ = self.ptrace.interrupt(thread);
            }
        }
    }

    fn handle(&mut self, status: WaitEvent) {
        let pid = status.pid();
        let process = self.process_of(pid);

        match status {
            WaitEvent::Exited(..) | WaitEvent::Signaled(..) => self.forget(pid, process),
            WaitEvent::Continued(_) => {}
            WaitEvent::PtraceEvent(_, _, libc::PTRACE_EVENT_EXEC) => self.executed(pid),
            WaitEvent::PtraceEvent(
                _,
                _,
                event @ (libc::PTRACE_EVENT_FORK
                | libc::PTRACE_EVENT_VFORK
                | libc::PTRACE_EVENT_CLONE),
            ) => {
                if let Ok(child) = self.ptrace.event_message(pid)
                    && let Ok(child) = i32::try_from(child)
                {
                    let child = Pid::from_raw(child);
                    if event == libc::PTRACE_EVENT_CLONE {
                        self.note_new(child, process, None);
                    } else {
                        self.note_new(child, child, Some(process));
                    }
                }
                self.resume(pid, None);
            }
            WaitEvent::PtraceEvent(..) | WaitEvent::PtraceSyscall(_) => self.resume(pid, None),
            WaitEvent::Stopped(_, signal) => {
                let first = signal == Signal::SIGSTOP && self.first_stop(pid);
                self.resume(pid, (!first).then_some(signal));
            }
        }
    }

    /// The thread group of a watched thread, learning it for a thread whose
    /// creation event has not been seen yet.
    fn process_of(&mut self, pid: Pid) -> Pid {
        if let Some(process) = self.threads.get(&pid) {
            return *process;
        }

        let status = super::proc_status::read_path(format!("/proc/{pid}/status")).ok();
        let field = |read: fn(&[u8]) -> Option<u32>| {
            status
                .as_deref()
                .and_then(read)
                .and_then(|value| i32::try_from(value).ok())
                .map(Pid::from_raw)
        };
        let process = field(super::proc_status::tgid).unwrap_or(pid);
        let parent = field(super::proc_status::parent_pid);
        self.note_new(pid, process, parent.filter(|_| process == pid));
        process
    }

    /// Notes a new thread of `process`, which is a new process of its own
    /// when it is `pid`, started by `parent`. Its first stop is yet to be
    /// resumed without its `SIGSTOP`.
    fn note_new(&mut self, pid: Pid, process: Pid, parent: Option<Pid>) {
        if self.threads.contains_key(&pid) {
            return;
        }

        if process == pid {
            let launched = parent
                .and_then(|parent| self.processes.get(&parent))
                .is_none_or(|watched| watched.launched);
            self.processes.insert(pid, Watched { parent, launched });
        }
        self.threads.insert(pid, process);
        self.fresh.push(pid);
    }

    /// Whether this is a new thread's first stop, whose `SIGSTOP` the
    /// kernel sent to stop it for its tracer and the program never sees.
    fn first_stop(&mut self, pid: Pid) -> bool {
        let Some(index) = self.fresh.iter().position(|fresh| *fresh == pid) else {
            return false;
        };

        self.fresh.swap_remove(index);
        true
    }

    fn forget(&mut self, pid: Pid, process: Pid) {
        self.threads.remove(&pid);
        if !self.threads.values().any(|group| *group == process) {
            self.processes.remove(&process);
        }
    }

    /// Decides a process that executed a program: it becomes a member, or
    /// runs on watched.
    fn executed(&mut self, pid: Pid) {
        // The exec ended every other thread of the process, and the one
        // that executed it took the leader's id.
        let process = self.process_of(pid);
        self.threads.retain(|_, group| *group != process);
        self.threads.insert(pid, pid);
        let watched = self.processes.remove(&process).unwrap_or(Watched {
            parent: None,
            launched: true,
        });

        let program = running_program(pid);
        let decision = program.as_ref().map_or(Decision::Watch, |(path, inode)| {
            self.following.policy.decide(path, Some(*inode))
        });

        if decision == Decision::Debug && !self.closing {
            self.threads.remove(&pid);
            self.following.push(Handover::Exec {
                pid,
                parent: watched.parent,
                launched: watched.launched,
                status: WaitEvent::PtraceEvent(pid, Signal::SIGTRAP, libc::PTRACE_EVENT_EXEC),
            });
            return;
        }

        let parent = watched.parent;
        self.processes.insert(pid, watched);
        if let Some((program, _)) = program {
            let _ = self.notices.send(GroupNotice::Watched {
                process: process_id(pid),
                parent: parent.map(process_id),
                program,
            });
        }
        self.resume(pid, None);
    }

    /// Resumes a stopped watched thread, or while the group ends, ends or
    /// releases it.
    fn resume(&self, pid: Pid, signal: Option<Signal>) {
        let process = self.threads.get(&pid).copied().unwrap_or(pid);
        let attached = self
            .processes
            .get(&process)
            .is_some_and(|watched| !watched.launched);

        if self.closing && attached {
            let _ = self.ptrace.detach(pid, signal);
            return;
        }

        let _ = self.ptrace.continue_execution(pid, signal);
    }
}

#[cfg(test)]
mod tests {
    use super::matches_glob;

    #[test]
    fn globs_match_names_as_a_shell_does() {
        for (pattern, name, matches) in [
            ("node", "node", true),
            ("node", "nodejs", false),
            ("python*", "python3.12", true),
            ("python*", "ipython", false),
            ("*", "", true),
            ("py?hon", "python", true),
            ("py?hon", "pyhon", false),
            ("*.test", "a.test", true),
            ("a*b*c", "aXbYbZc", true),
            ("a*b*c", "aXbYbZ", false),
        ] {
            assert_eq!(matches_glob(pattern, name), matches, "{pattern} {name}");
        }
    }
}
