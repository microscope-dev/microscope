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
use crate::Catchpoint;
use crate::backend::{ControllerChannels, ControllerJoin, ControllerMessage};
use crate::debug_info::{DebugFileSearch, DebugInfo};
use crate::{
    DebuggerEvent, ExecutionId, FollowMode, FollowPolicy, ModuleImage, ProcessId, Result, StopId,
};

pub(super) struct Transfer {
    pub(super) child: Pid,
    pub(super) parent: Pid,

    pub(super) vfork: bool,

    pub(super) program: Option<(Executable, DebugInfo)>,

    pub(super) launched: bool,
}

enum Handover {
    Fork(Transfer),

    Exec {
        pid: Pid,
        parent: Option<Pid>,
        launched: bool,
        status: WaitEvent,
    },

    Demote {
        pid: Pid,
        parent: Option<Pid>,
        launched: bool,
        program: PathBuf,
    },
}

pub(super) struct Following {
    policy: Policy,
    handovers: Mutex<VecDeque<Handover>>,
    settings: Mutex<Settings>,
}

#[derive(Clone, Default)]
struct Settings {
    revision: u64,

    signals: BTreeMap<Signal, crate::SignalPolicy>,
    exception_stops: Option<crate::ExceptionStops>,
    exec_stops: Option<bool>,
    step_into_runtime: Option<bool>,
    step_skips: Option<crate::StepSkips>,
    views: Option<Arc<crate::view::ViewSet>>,
    views_enabled: Option<bool>,

    catchpoints: BTreeMap<crate::CatchpointId, crate::Catchpoint>,
    next_catchpoint: u64,
}

pub(super) enum Setting {
    Signal(Signal, crate::SignalPolicy),
    ExceptionStops(crate::ExceptionStops),
    ExecStops(bool),
    StepIntoRuntime(bool),
    StepSkips(crate::StepSkips),
    Views(Arc<crate::view::ViewSet>),
    ViewsEnabled(bool),
    AddCatchpoint(crate::Catchpoint),
    RemoveCatchpoint(crate::CatchpointId),
}

impl Following {
    pub(super) fn new(policy: Policy) -> Self {
        Self {
            policy,
            handovers: Mutex::default(),
            settings: Mutex::new(Settings {
                next_catchpoint: 1,
                ..Settings::default()
            }),
        }
    }

    pub(super) fn next_catchpoint(&self) -> crate::CatchpointId {
        let mut settings = self.settings();
        let id = crate::CatchpointId::new(settings.next_catchpoint);
        settings.next_catchpoint += 1;
        id
    }

    fn settings(&self) -> std::sync::MutexGuard<'_, Settings> {
        self.settings.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub(super) fn transfer(&self, transfer: Transfer) {
        self.push(Handover::Fork(transfer));
    }

    pub(super) fn demote(&self, pid: Pid, parent: Option<Pid>, launched: bool, program: PathBuf) {
        self.push(Handover::Demote {
            pid,
            parent,
            launched,
            program,
        });
    }

    pub(super) fn follows(&self) -> bool {
        self.policy.policy.mode != crate::FollowMode::None
    }

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

pub(super) struct Membership {
    pub(super) following: Arc<Following>,

    pub(super) root: bool,

    pub(super) parent: Option<Pid>,

    pub(super) settings_taken: u64,
}

impl<P: super::native::InspectionOps> Controller<P> {
    pub(super) fn share(&mut self, setting: Setting) {
        let Some(membership) = self.group.as_mut() else {
            return;
        };

        let mut settings = membership.following.settings();
        let changed = match setting {
            Setting::Signal(signal, policy) => {
                settings.signals.insert(signal, policy) != Some(policy)
            }
            Setting::ExceptionStops(stops) => {
                settings.exception_stops.replace(stops) != Some(stops)
            }
            Setting::ExecStops(stops) => settings.exec_stops.replace(stops) != Some(stops),
            Setting::StepIntoRuntime(enter) => {
                settings.step_into_runtime.replace(enter) != Some(enter)
            }
            Setting::StepSkips(skips) => settings.step_skips.replace(skips.clone()) != Some(skips),
            Setting::Views(views) => !settings
                .views
                .replace(Arc::clone(&views))
                .is_some_and(|old| Arc::ptr_eq(&old, &views)),
            Setting::ViewsEnabled(enabled) => {
                settings.views_enabled.replace(enabled) != Some(enabled)
            }
            Setting::AddCatchpoint(catchpoint) => {
                let id = catchpoint.id;
                settings
                    .catchpoints
                    .insert(
                        id,
                        Catchpoint {
                            hit_count: 0,
                            ..catchpoint
                        },
                    )
                    .is_none()
            }
            Setting::RemoveCatchpoint(id) => settings.catchpoints.remove(&id).is_some(),
        };
        if !changed {
            return;
        }

        let complete = membership.settings_taken == settings.revision;
        settings.revision += 1;
        if complete {
            membership.settings_taken = settings.revision;
        }
    }
}

impl<P: LinuxTraceOps> Controller<P> {
    pub(super) fn join_group(&mut self, membership: Membership) {
        self.group = Some(membership);
    }

    pub(super) fn take_group_settings(&mut self) {
        let Some(membership) = self.group.as_mut() else {
            return;
        };
        let settings = {
            let settings = membership.following.settings();
            if settings.revision == membership.settings_taken {
                return;
            }
            settings.clone()
        };
        membership.settings_taken = settings.revision;

        for (&signal, &policy) in &settings.signals {
            self.signals.set(signal, policy);
        }
        if let Some(stops) = settings.exec_stops {
            self.exec_stops = stops;
        }
        if let Some(enter) = settings.step_into_runtime {
            self.step_into_runtime = enter;
        }
        if let Some(skips) = settings.step_skips {
            self.step_skips = skips;
        }
        if let Some(views) = settings.views
            && !Arc::ptr_eq(&views, &self.views.set)
        {
            self.views.replace(views);
        }
        if let Some(enabled) = settings.views_enabled {
            self.views.enabled = enabled;
        }
        if let Some(stops) = settings.exception_stops
            && stops != self.exception_stops
        {
            self.edit(super::Edit::SetExceptionStops {
                stops,
                reply: oneshot::channel().0,
            });
        }

        let removed = self
            .catchpoints
            .keys()
            .filter(|id| !settings.catchpoints.contains_key(id))
            .copied()
            .collect::<Vec<_>>();
        for id in removed {
            self.edit(super::Edit::RemoveCatchpoint {
                id,
                reply: oneshot::channel().0,
            });
        }
        for (id, catchpoint) in settings.catchpoints {
            if !self.catchpoints.contains_key(&id) {
                self.edit(super::Edit::AddCatchpoint {
                    id: Some(id),
                    calls: catchpoint.calls,
                    phase: catchpoint.phase,
                    condition: catchpoint.condition,
                    reply: oneshot::channel().0,
                });
            }
        }
    }

    pub(super) fn forked_program(&self, child: Pid) -> Option<(Executable, DebugInfo)> {
        let main = self.modules.get(&self.main.id)?;
        Some((
            Executable {
                display_path: Arc::clone(&self.main.executable),
                launch_path: Arc::clone(&self.main.executable),
                identity: self.main.identity,
                process_start_time: self.ptrace.process_start_time(child),
                debug_files: self.debug_files.clone(),
                kept: None,
                c_library: self.main.c_library,
            },
            DebugInfo {
                image: Arc::clone(&self.main.image),
                unwind: Arc::clone(&self.main.unwind),
                variables: Arc::clone(&main.variables),
            },
        ))
    }

    pub(super) fn following(&self) -> Option<&Arc<Following>> {
        self.group.as_ref().map(|group| &group.following)
    }

    pub(super) fn demotes_at_exec(&self, program: &Path) -> bool {
        self.group
            .as_ref()
            .is_some_and(|group| !group.root && !group.following.debugs(program))
            && self.session.inferior().is_some_and(|inferior| {
                !(inferior.tracked.values()).any(|record| *record == super::Tracked::Retired)
            })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Decision {
    Debug,

    Watch,
}

pub(super) struct Policy {
    pub(super) policy: FollowPolicy,

    pub(super) launched: Option<u64>,
}

impl Policy {
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

pub fn program_runtime(program: &Path) -> Option<&'static str> {
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

fn running_program(pid: Pid) -> Option<(PathBuf, u64)> {
    use std::os::unix::fs::MetadataExt as _;

    let link = format!("/proc/{pid}/exe");
    let path = std::fs::read_link(&link).ok()?;
    let inode = std::fs::metadata(&link).ok()?.ino();
    Some((path, inode))
}

#[derive(Debug, Clone, Copy)]
pub struct Capacity {
    pub requests: usize,
    pub events: usize,
}

pub enum GroupNotice {
    Joined(Joined),

    Watched {
        process: ProcessId,
        parent: Option<ProcessId>,
        program: PathBuf,
    },

    Unfollowed {
        process: ProcessId,
        program: PathBuf,
        reason: String,
    },
}

pub struct Joined {
    pub process: ProcessId,

    pub parent: Option<ProcessId>,

    pub executed: Option<PathBuf>,
    pub requests: mpsc::Sender<ControllerMessage>,
    pub events: broadcast::Sender<DebuggerEvent>,
    pub program: watch::Receiver<Arc<ModuleImage>>,
    pub launched: Arc<PathBuf>,
    pub controller: ControllerJoin,
    pub started: Started,
}

pub enum Started {
    Fork(oneshot::Receiver<Result<StopId>>),
    Exec(oneshot::Receiver<Result<ExecutionId>>),
}

impl Started {
    pub async fn wait(self) -> Result<()> {
        let cancelled = |_| crate::Error::RequestCancelled;
        match self {
            Self::Fork(started) => started.await.map_err(cancelled)?.map(drop),
            Self::Exec(started) => started.await.map_err(cancelled)?.map(drop),
        }
    }
}

pub(super) struct GroupSetup {
    pub(super) following: Arc<Following>,
    pub(super) notices: mpsc::UnboundedSender<GroupNotice>,
    pub(super) capacity: Capacity,
    pub(super) debug_files: DebugFileSearch,
}

pub(super) type NewMember = (Box<dyn Hosted>, oneshot::Sender<std::thread::Result<()>>);

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

    pub(super) fn poll(&mut self, context: &mut Context<'_>) -> bool {
        self.watcher.poll(context)
    }

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
                let _ = controller
                    .message_sender
                    .try_send(ControllerMessage::Wait(status));
            },
            Started::Exec(started),
        ))
    }

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
            settings_taken: 0,
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

const WATCHER_CAPACITY: usize = 256;

struct Watched {
    parent: Option<Pid>,
    launched: bool,
}

struct Watcher {
    ptrace: LinuxPtrace,
    messages: mpsc::Receiver<ControllerMessage>,
    pending: VecDeque<ControllerMessage>,

    processes: BTreeMap<Pid, Watched>,

    threads: BTreeMap<Pid, Pid>,

    fresh: Vec<Pid>,
    following: Arc<Following>,
    notices: mpsc::UnboundedSender<GroupNotice>,

    closing: bool,
}

impl Watcher {
    fn watch(&mut self, pid: Pid, parent: Option<Pid>, launched: bool, program: Option<PathBuf>) {
        self.processes.insert(pid, Watched { parent, launched });
        self.threads.insert(pid, pid);
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
                ControllerMessage::Request(_)
                | ControllerMessage::AgentTimeout
                | ControllerMessage::Inspector(_) => {}
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

    fn executed(&mut self, pid: Pid) {
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
    use std::path::{Path, PathBuf};

    use super::{Decision, Policy, matches_glob, program_runtime};
    use crate::{FollowMode, FollowPolicy};

    fn fixture(name: &str) -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("build/test-programs")
            .join(name)
    }

    fn inode(path: &Path) -> Option<u64> {
        std::fs::metadata(path)
            .ok()
            .map(|metadata| std::os::unix::fs::MetadataExt::ino(&metadata))
    }

    #[test]
    fn a_runtime_is_recognized_by_the_marks_it_leaves_in_its_program() {
        assert_eq!(program_runtime(&fixture("callers-go")), Some("go"));
        assert_eq!(program_runtime(&fixture("process-tree")), None);
        assert_eq!(program_runtime(Path::new("/nonexistent")), None);
    }

    #[test]
    fn the_policy_debugs_the_launched_program_runtimes_and_named_programs() {
        let launched = fixture("process-tree");
        let other = fixture("exec-target");
        let go = fixture("callers-go");
        let policy = |mode, programs: &[&str]| Policy {
            policy: FollowPolicy {
                mode,
                programs: programs.iter().map(|name| (*name).to_owned()).collect(),
                ..FollowPolicy::default()
            },
            launched: inode(&launched),
        };
        let decide = |policy: &Policy, path: &Path| policy.decide(path, inode(path));

        let auto = policy(FollowMode::Auto, &[]);
        assert_eq!(decide(&auto, &launched), Decision::Debug);
        assert_eq!(decide(&auto, &go), Decision::Debug);
        assert_eq!(decide(&auto, &other), Decision::Watch);

        let named = policy(FollowMode::Auto, &["exec-*"]);
        assert_eq!(decide(&named, &other), Decision::Debug);

        assert_eq!(
            decide(&policy(FollowMode::All, &[]), &other),
            Decision::Debug
        );
        let none = policy(FollowMode::None, &["exec-*"]);
        assert_eq!(decide(&none, &launched), Decision::Watch);
        assert_eq!(decide(&none, &other), Decision::Watch);
    }

    fn glob_reference(pattern: &[char], name: &[char]) -> bool {
        match pattern.split_first() {
            None => name.is_empty(),
            Some(('*', rest)) => (0..=name.len()).any(|taken| glob_reference(rest, &name[taken..])),
            Some((&first, rest)) => name.split_first().is_some_and(|(&character, name)| {
                (first == '?' || first == character) && glob_reference(rest, name)
            }),
        }
    }

    #[test]
    fn globs_match_as_the_reference_does_on_every_short_input() {
        fn words(alphabet: &[char], longest: usize) -> Vec<Vec<char>> {
            let mut words = vec![Vec::new()];
            let mut last = vec![Vec::new()];
            for _ in 0..longest {
                last = last
                    .iter()
                    .flat_map(|word: &Vec<char>| {
                        alphabet.iter().map(move |&character| {
                            let mut longer = word.clone();
                            longer.push(character);
                            longer
                        })
                    })
                    .collect();
                words.extend(last.iter().cloned());
            }
            words
        }

        let names = words(&['a', 'b'], 6);
        for pattern in words(&['a', 'b', '*', '?'], 5) {
            let text = pattern.iter().collect::<String>();
            for name in &names {
                assert_eq!(
                    matches_glob(&text, &name.iter().collect::<String>()),
                    glob_reference(&pattern, name),
                    "{text:?} against {name:?}"
                );
            }
        }
    }
}
