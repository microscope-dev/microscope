mod addons;
mod arming;
pub(super) mod assignments;
#[cfg(test)]
mod connections;
mod functions;
mod interrupts;
mod maps;
mod pauses;
mod protocol;
mod quiescence;
#[cfg(test)]
mod scripted;
mod steps;
pub(super) mod teardown;

use std::collections::{BTreeMap, BTreeSet};
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use nix::unistd::Pid;

use super::breakpoints::remove_breakpoint_owner_from;
use super::cdp::{Connection, Incoming, RequestId, Session};
use super::native::LinuxTraceOps;
use super::node_launch::Ownership;
use super::trap::TrapEvent;
use super::{BreakpointOwner, Controller};
use crate::runtime_model::ManagedArm;
use crate::runtime_model::inspector_protocol;
use crate::{Error, Result, RuntimeId, VirtualAddress};

pub(super) use protocol::{is_script_frame, loopback_listeners};

static GENERATIONS: AtomicU64 = AtomicU64::new(1);

#[derive(Debug)]
pub struct InspectorMessage {
    generation: u64,
    runtime: RuntimeId,
    incoming: Incoming,
}

#[derive(Debug, Default)]
pub(super) struct Inspectors {
    pub(super) cleanup: Option<teardown::Cleanup>,
    startup: BTreeMap<RuntimeId, quiescence::Startup>,
    startup_planted: BTreeSet<VirtualAddress>,

    gate_planted: BTreeSet<VirtualAddress>,
    entries: BTreeMap<VirtualAddress, RuntimeId>,

    records: BTreeMap<RuntimeId, Inspector>,

    pub(super) interrupt: Option<std::time::Instant>,

    pub(super) locals: BTreeMap<VirtualAddress, crate::runtime_model::ReportedLocals>,

    maps: BTreeMap<Arc<str>, maps::ScriptMap>,
}

impl Inspectors {
    pub(super) fn has(&self, runtime: RuntimeId) -> bool {
        self.records.contains_key(&runtime)
    }

    pub(in crate::backend::linux) fn end_interrupt(&mut self) -> bool {
        for isolate in self.isolates_mut() {
            if isolate.pause == PauseRequest::Deferred {
                isolate.pause = PauseRequest::None;
            }
            if let Some(paused) = &mut isolate.paused {
                paused.interrupt_answered();
            }
        }
        self.interrupt.take().is_some()
    }

    fn isolates_mut(&mut self) -> impl Iterator<Item = &mut Isolate> {
        self.records
            .values_mut()
            .filter_map(|inspector| inspector.status.connected_mut())
            .flat_map(|(_, isolates)| isolates.values_mut())
    }

    pub(super) fn forget(&mut self, address: VirtualAddress) {
        self.entries.remove(&address);
        self.startup_planted.remove(&address);
        self.gate_planted.remove(&address);
    }

    pub(super) fn map_frames(&self, frames: &mut Arc<[crate::StackFrame]>) {
        let mapped = |frame: &crate::StackFrame| {
            frame.recorded.as_ref().is_some_and(|recorded| {
                recorded.generated.is_none()
                    && recorded
                        .place
                        .as_ref()
                        .is_some_and(|place| self.maps.contains_key(&place.path))
            })
        };
        if !frames.iter().any(mapped) {
            return;
        }
        let mut shown = frames.to_vec();
        for frame in shown.iter_mut().filter(|frame| mapped(frame)) {
            let Some(recorded) = frame.recorded.as_mut() else {
                continue;
            };
            let Some(generated) = recorded.place.as_ref() else {
                continue;
            };
            let Some(map) = self.maps.get(&generated.path) else {
                continue;
            };
            if let Err(reason) = map.map() {
                recorded.problem.get_or_insert(reason);
                continue;
            }
            let generated = recorded.place.take();
            let Some(generated) = generated else { continue };
            match map.original(&generated) {
                Ok(original) => {
                    if [".ts", ".mts", ".cts", ".tsx"]
                        .iter()
                        .any(|end| original.path.ends_with(end))
                    {
                        recorded.language = crate::SourceLanguage::TypeScript;
                    }
                    recorded.place = Some(original);
                }
                Err(reason) => {
                    recorded.problem.get_or_insert(reason);
                }
            }
            recorded.first_line = None;
            recorded.generated = Some(generated);
        }
        *frames = shown.into();
    }

    pub(super) fn planted(&self) -> BTreeSet<VirtualAddress> {
        self.entries
            .keys()
            .copied()
            .chain(self.cleanup.iter().flat_map(teardown::Cleanup::sites))
            .chain(self.startup_planted.iter().copied())
            .chain(self.gate_planted.iter().copied())
            .collect()
    }
}

#[derive(Debug, Default)]
struct Inspector {
    ownership: Ownership,
    owned_agent: Option<VirtualAddress>,

    listening: BTreeSet<SocketAddr>,

    status: Status,

    desired: Vec<ManagedArm>,

    step: Option<steps::Stepping>,

    assigning: Option<assignments::Assigning>,

    through: Option<addons::Through>,

    gate: Option<steps::Gate>,

    pausing: &'static str,
}

type Isolates = BTreeMap<Session, Isolate>;

#[derive(Debug, Default)]
struct Isolate {
    sites: BTreeMap<u64, Site>,

    withdrawn: BTreeSet<RequestId>,

    scripts: BTreeMap<Arc<str>, Arc<str>>,

    sentinel: Option<Sentinel>,

    deferred: Option<serde_json::Value>,

    search: Option<functions::Search>,

    searched: BTreeSet<Arc<str>>,

    texts: BTreeMap<Arc<str>, Arc<functions::Text>>,

    profiling: bool,

    paused: Option<pauses::Paused>,

    told: Option<&'static str>,

    resuming: bool,
    pause: PauseRequest,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
enum PauseRequest {
    #[default]
    None,

    Deferred,

    Sent,
}

#[derive(Debug, Default)]
struct Site {
    placements: Vec<Placement>,
}

#[derive(Debug)]
struct Placement {
    id: Option<Arc<str>>,
    state: SiteState,

    mapped: Option<(Arc<str>, crate::debug_info::js_source_map::Position)>,

    actual: Option<(u64, u64)>,

    function: Option<(Arc<str>, u32)>,
}

#[derive(Debug)]
struct Sentinel {
    pattern: String,
    placement: Placement,
}

#[derive(Debug)]
enum SiteState {
    Requested(RequestId),

    Unresolved,
    Resolved,
    Refused(Arc<str>),

    Withdrawn(RequestId),
}

#[derive(Debug, Default)]
enum Status {
    #[default]
    Unknown,

    Awaited {
        unstarted: Option<&'static str>,
    },

    Listed(Server),

    Connecting(Server, Link),
    Connected(Server, Link, Isolates),

    Failed {
        reason: Arc<str>,
        target: Option<Arc<str>>,
    },
}

#[derive(Debug, Clone)]
struct Server {
    target: Arc<str>,
    address: SocketAddr,
}

struct Link {
    generation: u64,
    connection: Connection,
}

impl std::fmt::Debug for Link {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Link")
            .field("generation", &self.generation)
            .finish_non_exhaustive()
    }
}

impl Status {
    const fn link(&self) -> Option<&Link> {
        match self {
            Self::Connecting(_, link) | Self::Connected(_, link, _) => Some(link),
            Self::Unknown | Self::Awaited { .. } | Self::Listed(_) | Self::Failed { .. } => None,
        }
    }

    const fn connected(&self) -> Option<&Link> {
        match self {
            Self::Connected(_, link, _) => Some(link),
            Self::Unknown
            | Self::Awaited { .. }
            | Self::Listed(_)
            | Self::Connecting(..)
            | Self::Failed { .. } => None,
        }
    }

    const fn target(&self) -> Option<&Arc<str>> {
        match self {
            Self::Listed(server) | Self::Connecting(server, _) | Self::Connected(server, ..) => {
                Some(&server.target)
            }
            Self::Failed { target, .. } => target.as_ref(),
            Self::Unknown | Self::Awaited { .. } => None,
        }
    }

    fn isolates(&self) -> &Isolates {
        static NONE: Isolates = BTreeMap::new();
        match self {
            Self::Connected(_, _, isolates) => isolates,
            Self::Unknown
            | Self::Awaited { .. }
            | Self::Listed(_)
            | Self::Connecting(..)
            | Self::Failed { .. } => &NONE,
        }
    }

    const fn connected_mut(&mut self) -> Option<(&Link, &mut Isolates)> {
        match self {
            Self::Connected(_, link, isolates) => Some((link, isolates)),
            Self::Unknown
            | Self::Awaited { .. }
            | Self::Listed(_)
            | Self::Connecting(..)
            | Self::Failed { .. } => None,
        }
    }

    fn connected_isolate(&mut self, session: &Session) -> Option<(&Link, &mut Isolate)> {
        let (link, isolates) = self.connected_mut()?;
        Some((link, isolates.get_mut(session)?))
    }

    fn disconnect(&mut self) -> Option<Link> {
        match std::mem::take(self) {
            Self::Connecting(server, link) | Self::Connected(server, link, _) => {
                *self = Self::Listed(server);
                Some(link)
            }
            other => {
                *self = other;
                None
            }
        }
    }

    fn fail(&mut self, reason: Arc<str>) {
        let target = self.target().cloned();
        *self = Self::Failed { reason, target };
    }
}

fn send(
    connection: &Connection,
    session: &Session,
    method: &str,
    params: serde_json::Value,
) -> std::result::Result<RequestId, super::cdp::CdpError> {
    let sent = connection.send(session, method, params);
    record!("{method} to {session:?}: {sent:?}");
    sent
}

impl<P: LinuxTraceOps> Controller<P> {
    pub(super) fn sync_inspector_hooks(&mut self) -> Result<()> {
        self.sync_step_gates(true)?;
        let Some(inferior) = self
            .session
            .inferior()
            .filter(|inferior| !inferior.observer)
        else {
            return Ok(());
        };
        let entries = self
            .runtimes(inferior)
            .into_iter()
            .filter_map(|bound| {
                let entry = bound.module.virtual_address(bound.model.inspector_hook()?);
                Some((entry.ok()?, bound.id))
            })
            .collect::<BTreeMap<_, _>>();

        let inferior = self.session.inferior_mut().ok_or(Error::NotRunning)?;
        let planted = inferior.inspectors.planted();
        let needed = entries
            .keys()
            .copied()
            .chain(
                inferior
                    .inspectors
                    .cleanup
                    .iter()
                    .flat_map(teardown::Cleanup::sites),
            )
            .chain(
                inferior
                    .inspectors
                    .startup
                    .values()
                    .flat_map(quiescence::Startup::sites),
            )
            .chain(inferior.inspectors.gate_planted.iter().copied())
            .collect::<BTreeSet<_>>();

        for &address in planted.difference(&needed) {
            remove_breakpoint_owner_from(
                &self.ptrace,
                inferior,
                address,
                BreakpointOwner::InspectorHook,
            )?;
            inferior.inspectors.forget(address);
        }

        let pid = inferior.memory_thread();
        for &address in needed.difference(&planted) {
            self.ptrace.install_breakpoint(
                pid,
                &mut inferior.breakpoints,
                address,
                BreakpointOwner::InspectorHook,
            )?;
            if inferior
                .inspectors
                .startup
                .values()
                .any(|startup| startup.sites().contains(&address))
            {
                inferior.inspectors.startup_planted.insert(address);
            }
        }

        inferior.inspectors.entries = entries;
        self.open_inspectors();
        self.capture_inspector_ownership();
        Ok(())
    }

    pub(super) fn open_inspectors(&mut self) {
        let Some(inferior) = self.session.inferior_mut() else {
            return;
        };
        let pid = inferior.tgid;
        if inferior.origin != super::InferiorOrigin::Attached {
            let inspectors = &mut inferior.inspectors;
            for &runtime in inspectors.entries.values() {
                let inspector = inspectors.records.entry(runtime).or_default();
                if matches!(inspector.status, Status::Unknown) {
                    inspector.status = Status::Awaited {
                        unstarted: super::node_launch::unstarted(pid.as_raw()),
                    };
                }
            }
            return;
        }
        let Some(inferior) = self.session.inferior() else {
            return;
        };
        if inferior
            .threads
            .values()
            .any(|thread| thread.state != super::NativeThreadState::Stopped)
        {
            return;
        }
        let runtimes = self.runtimes(inferior);
        for bound in runtimes {
            let runtime = bound.id;
            if bound.model.inspector_hook().is_none()
                || self.session.inferior().is_some_and(|inferior| {
                    inferior
                        .inspectors
                        .records
                        .get(&runtime)
                        .is_some_and(|inspector| !matches!(inspector.status, Status::Unknown))
                })
            {
                continue;
            }
            let Some(inferior) = self.session.inferior() else {
                return;
            };
            let existing = self.with_runtime_stop(inferior, &bound, pid, |stop| {
                bound.model.inspector_endpoint(stop)
            });
            let existing = match existing {
                Ok(existing) => existing,
                Err(reason) => {
                    record!("the inspector of runtime {runtime} cannot be used: {reason}");
                    if let Some(inferior) = self.session.inferior_mut() {
                        let inspector = inferior.inspectors.records.entry(runtime).or_default();
                        inspector.status.fail(reason);
                    }
                    continue;
                }
            };
            if let Some((address, target)) = existing {
                self.open_connection(runtime, Server { target, address });
                continue;
            }
            if self
                .session
                .inferior()
                .is_some_and(|inferior| inferior.run.execution().is_some())
            {
                self.activate_inspector(&bound, pid);
            }
        }
    }

    fn activate_inspector(&mut self, bound: &super::runtimes::BoundRuntime, pid: Pid) {
        let listening = self.ptrace.loopback_listeners(pid);
        let mut interrupted = None;
        let opened = bound
            .model
            .inspector_signal_handler()
            .and_then(|address| bound.module.virtual_address(address).ok())
            .ok_or_else(|| {
                Arc::<str>::from("Node's native SIGUSR1 handler cannot be proven from this image")
            })
            .and_then(|native| {
                if self
                    .session
                    .inferior()
                    .and_then(|inferior| inferior.threads.get(&pid))
                    .is_none_or(|thread| {
                        thread.pending_signal.is_some() || thread.held_signal.is_some()
                    })
                {
                    return Err(
                        "cannot query Node's signal disposition over a pending delivery".into(),
                    );
                }
                self.prepare_inspector_startup(bound)
                    .map_err(|error| Arc::from(error.to_string()))?;
                self.ptrace
                    .open_inspector_signal(pid, native.get(), &mut interrupted)
            });
        let Some(inferior) = self.session.inferior_mut() else {
            return;
        };
        if let Some(pending) = interrupted
            && let Some(thread) = inferior.threads.get_mut(&pid)
        {
            thread.pending_signal = Some(pending);
        }
        let inspector = inferior.inspectors.records.entry(bound.id).or_default();
        inspector.status = Status::Awaited { unstarted: None };
        match opened {
            Ok(true) => {
                inspector.ownership = Ownership::Created;
                inspector.listening = listening.unwrap_or_default().into_iter().collect();
                record!("SIGUSR1 opens the attached node's inspector");
            }
            Ok(false) => inspector.status.fail(
                "the node's SIGUSR1 disposition is not Node's native inspector handler; \
                 microscope will not deliver SIGUSR1 to application code; start it with \
                 --inspect=127.0.0.1:0"
                    .into(),
            ),
            Err(reason) => inspector.status.fail(reason),
        }
    }

    pub(super) fn note_inspector_hook(&mut self, pid: Pid, address: VirtualAddress) -> Result<()> {
        if self.note_step_gate(pid, address)? {
            return Ok(());
        }
        if self.note_inspector_startup_hook(pid, address)? {
            return Ok(());
        }
        if self.note_inspector_cleanup_hook(pid, address)? {
            return Ok(());
        }
        let inferior = self.session.inferior().ok_or(Error::NotRunning)?;
        let Some(&runtime) = inferior.inspectors.entries.get(&address) else {
            return Ok(());
        };
        let bound = self
            .runtimes(inferior)
            .into_iter()
            .find(|bound| bound.id == runtime)
            .ok_or(Error::NotRunning)?;
        let mut registers = self.ptrace.registers(pid)?;
        let pending = registers.pending_return(|address| self.ptrace.read_word(pid, address))?;
        let caller = pending.returns_to;
        let target = self
            .with_runtime_stop(inferior, &bound, pid, |stop| {
                bound.model.inspector_target(
                    stop,
                    caller,
                    VirtualAddress::new(registers.arguments()[1]),
                )
            })
            .map_err(Error::RuntimeAgent)?;
        let Some(target) = target else {
            return Ok(());
        };
        for offset in [0, 8, 16] {
            let address = registers.arguments()[0]
                .checked_add(offset)
                .ok_or_else(|| {
                    Error::RuntimeAgent("the inspector result address overflows".into())
                })?;
            self.ptrace.write_word(pid, address, 0)?;
        }
        registers.return_now(pending, registers.arguments()[0]);
        self.ptrace.set_registers(pid, registers)?;
        let inferior = self.session.inferior_mut().ok_or(Error::NotRunning)?;
        inferior.trap_event(pid, TrapEvent::Moved(caller))?;
        let inspector = inferior.inspectors.records.entry(runtime).or_default();
        if inspector.status.target().is_none() {
            match self.inspector_address(runtime) {
                Ok(address) => self.open_connection(runtime, Server { target, address }),
                Err(reason) => {
                    record!("the inspector of runtime {runtime} cannot be used: {reason}");
                    let inferior = self.session.inferior_mut().ok_or(Error::NotRunning)?;
                    inferior
                        .inspectors
                        .records
                        .entry(runtime)
                        .or_default()
                        .status = Status::Failed {
                        reason,
                        target: Some(target),
                    };
                }
            }
            let inferior = self.session.inferior_mut().ok_or(Error::NotRunning)?;
            inferior.managed.mark_stale(runtime);
            if self.sync_runtime_arming() {
                self.publish_breakpoints_changed();
            }
        }
        self.queue_module_refresh()
    }

    fn inspector_address(&self, runtime: RuntimeId) -> std::result::Result<SocketAddr, Arc<str>> {
        let inferior = self.session.inferior().ok_or("the process is gone")?;
        let listening = inferior
            .inspectors
            .records
            .get(&runtime)
            .map(|inspector| &inspector.listening);
        let listeners = self
            .ptrace
            .loopback_listeners(inferior.tgid)?
            .into_iter()
            .filter(|listener| listening.is_none_or(|listening| !listening.contains(listener)))
            .collect::<Vec<_>>();
        match listeners[..] {
            [listener] => Ok(listener),
            [] => Err("the inspector's server has no listening socket".into()),
            _ => Err(
                "the process listens on more than one loopback socket, so microscope \
                      cannot tell which is the inspector's"
                    .into(),
            ),
        }
    }

    fn open_connection(&mut self, runtime: RuntimeId, server: Server) {
        let Some(inferior) = self.session.inferior_mut() else {
            return;
        };
        let inspector = inferior.inspectors.records.entry(runtime).or_default();
        let generation = GENERATIONS.fetch_add(1, Ordering::Relaxed);
        let sender = self.message_sender.clone();
        let Server { target, address } = &server;
        let opened = Connection::open(*address, &format!("/{target}"), move |incoming| {
            sender
                .blocking_send(crate::backend::ControllerMessage::Inspector(
                    InspectorMessage {
                        generation,
                        runtime,
                        incoming,
                    },
                ))
                .is_ok()
        });
        record!(
            "connecting to the inspector at {address}/{target}: {:?}",
            opened.as_ref().map(|_| ())
        );
        inspector.status = match opened {
            Ok(connection) => Status::Connecting(
                server,
                Link {
                    generation,
                    connection,
                },
            ),
            Err(error) => Status::Failed {
                reason: format!("cannot connect to the inspector: {error}").into(),
                target: Some(server.target),
            },
        };
    }

    #[cfg_attr(
        not(debug_assertions),
        expect(unused_variables, reason = "only recorded")
    )]
    pub(super) fn inspector_message(&mut self, message: InspectorMessage) -> Result<()> {
        let starting = self.inspector_cleanup_starting();
        if self.inspector_cleanup_running() && !starting {
            return self.inspector_cleanup_message(message);
        }
        let InspectorMessage {
            generation,
            runtime,
            incoming,
        } = message;
        let Some(inspector) = self
            .session
            .inferior_mut()
            .and_then(|inferior| inferior.inspectors.records.get_mut(&runtime))
        else {
            return Ok(());
        };
        if inspector
            .status
            .link()
            .is_none_or(|link| link.generation != generation)
        {
            record!("a message from an ended inspector connection: {incoming:?}");
            return Ok(());
        }

        match incoming {
            Incoming::Connected => self.inspector_connected(runtime),
            Incoming::Reply { id, result, .. } => {
                if starting {
                    self.queue_internal_edit(super::Edit::InspectorCleanup)?;
                    return self.finish_barrier_if_ready();
                }
                if inspector
                    .assigning
                    .as_ref()
                    .is_some_and(|assignment| assignment.awaits(id))
                {
                    return self.assignment_replied(runtime, id, result);
                }
                if inspector.status.isolates().values().any(|isolate| {
                    isolate
                        .paused
                        .as_ref()
                        .is_some_and(|paused| paused.listing.contains_key(&id))
                }) {
                    return self.scope_listed(runtime, id, result);
                }
                if self.searches(runtime, id) {
                    return self.search_answered(runtime, id, result);
                }
                return self.breakpoint_set(runtime, id, result);
            }
            Incoming::Event {
                session,
                method,
                params,
            } => {
                if starting && method.as_ref() == inspector_protocol::PAUSED {
                    if let Some(link) = inspector.status.link() {
                        let _ = send(
                            &link.connection,
                            &session,
                            inspector_protocol::RESUME,
                            serde_json::json!({}),
                        );
                    }
                    return Ok(());
                }
                return self.inspector_event(runtime, &session, &method, &params);
            }
            Incoming::Closed { reason, .. } => {
                record!("the inspector connection closed: {reason}");
                inspector
                    .status
                    .fail(format!("the inspector connection closed: {reason}").into());
                self.publish_breakpoints_changed();
                self.release_step_gate()?;
            }
            Incoming::Malformed { session, reason } => {
                record!("inspector {session:?} sent a malformed message: {reason}");
            }
        }
        Ok(())
    }

    fn inspector_connected(&mut self, runtime: RuntimeId) {
        let Some(inspector) = self
            .session
            .inferior_mut()
            .and_then(|inferior| inferior.inspectors.records.get_mut(&runtime))
        else {
            return;
        };
        inspector.status = match std::mem::take(&mut inspector.status) {
            Status::Connecting(server, link) => Status::Connected(
                server,
                link,
                Isolates::from([(Session::main(), Isolate::default())]),
            ),
            other => other,
        };
        let Some(link) = inspector.status.connected() else {
            return;
        };
        let main = Session::main();
        let _ = send(
            &link.connection,
            &main,
            inspector_protocol::NOTIFY_END,
            serde_json::json!({ "enabled": true }),
        );
        ready_isolate(&link.connection, &main);
        self.sync_inspector(runtime);
        if let Some(link) = self
            .session
            .inferior()
            .and_then(|inferior| inferior.inspectors.records.get(&runtime))
            .and_then(|inspector| inspector.status.link())
        {
            let _ = send(
                &link.connection,
                &main,
                "Runtime.enable",
                serde_json::json!({}),
            );
            let _ = send(
                &link.connection,
                &main,
                inspector_protocol::ENABLE_RUNTIME,
                serde_json::json!({}),
            );
        }
        self.publish_breakpoints_changed();
    }

    fn release_isolate(&self, runtime: RuntimeId, session: &Session) {
        if let Some(link) = self
            .session
            .inferior()
            .and_then(|inferior| inferior.inspectors.records.get(&runtime))
            .and_then(|inspector| inspector.status.link())
        {
            let _ = send(
                &link.connection,
                session,
                inspector_protocol::RELEASE,
                serde_json::json!({}),
            );
        }
    }

    fn inspector_preload_acknowledged(
        &mut self,
        runtime: RuntimeId,
        params: &serde_json::Value,
    ) -> Result<()> {
        let Some(inferior) = self.session.inferior_mut() else {
            return Ok(());
        };
        if inferior.origin == super::InferiorOrigin::Attached {
            return Ok(());
        }
        let Some(inspector) = inferior.inspectors.records.get_mut(&runtime) else {
            return Ok(());
        };
        if inspector.ownership != Ownership::Pending {
            return Ok(());
        }
        let Some(target) = inspector.status.target() else {
            return Ok(());
        };
        let ownership = super::node_launch::acknowledge(inferior.tgid, target, params);
        if ownership == Ownership::Pending {
            return Ok(());
        }
        record!("the launch preload confirms inspector ownership {ownership:?}");
        inspector.ownership = ownership;
        self.queue_module_refresh()
    }

    fn inspector_event(
        &mut self,
        runtime: RuntimeId,
        session: &Session,
        method: &str,
        params: &serde_json::Value,
    ) -> Result<()> {
        if method == "Runtime.consoleAPICalled" && *session == Session::main() {
            return self.inspector_preload_acknowledged(runtime, params);
        }
        let Some(inspector) = self
            .session
            .inferior_mut()
            .and_then(|inferior| inferior.inspectors.records.get_mut(&runtime))
        else {
            return Ok(());
        };
        if method == inspector_protocol::WORKER_ATTACHED {
            if let Some(id) = params["sessionId"].as_str() {
                self.attach_worker(runtime, &session.worker(id));
            }
            return Ok(());
        }
        if method == inspector_protocol::WORKER_DETACHED {
            if let Some(id) = params["sessionId"].as_str() {
                let worker = session.worker(id);
                record!("worker {worker:?} is gone");
                if let Some((_, isolates)) = inspector.status.connected_mut() {
                    isolates.retain(|session, _| !session.path().starts_with(worker.path()));
                }
                self.publish_breakpoints_changed();
            }
            return Ok(());
        }
        let Some((link, isolate)) = inspector.status.connected_isolate(session) else {
            record!("inspector event {method} of an unknown isolate {session:?}");
            return Ok(());
        };

        match method {
            inspector_protocol::SCRIPT_PARSED => {
                if let (Some(id), Some(url)) = (params["scriptId"].as_str(), params["url"].as_str())
                {
                    isolate.scripts.insert(id.into(), url.into());
                    if let Some(map) = params["sourceMapURL"]
                        .as_str()
                        .filter(|map| !map.is_empty())
                    {
                        self.script_mapped(runtime, session, id, url, map);
                    }
                }
            }
            inspector_protocol::RESOLVED => {
                let id = params["breakpointId"].as_str().unwrap_or_default();
                if let Some(placement) = isolate
                    .sites
                    .values_mut()
                    .flat_map(|site| site.placements.iter_mut())
                    .find(|placement| placement.id.as_deref() == Some(id))
                {
                    placement.state = SiteState::Resolved;
                    self.publish_breakpoints_changed();
                }
            }
            inspector_protocol::RESUMED => {
                isolate.resuming = false;
                if isolate.pause == PauseRequest::Deferred {
                    let sent = send(
                        &link.connection,
                        session,
                        inspector_protocol::PAUSE,
                        serde_json::json!({}),
                    );
                    isolate.pause = match sent {
                        Ok(_) => PauseRequest::Sent,
                        Err(_) => PauseRequest::None,
                    };
                    if *session == Session::main()
                        && let Some(gate) = inspector.gate.as_mut()
                    {
                        gate.sent(sent.ok());
                        return self.release_step_gate();
                    }
                }
            }
            inspector_protocol::PAUSED => {
                return self.inspector_paused(runtime, session, params);
            }
            inspector_protocol::WAITING => self.release_isolate(runtime, session),
            inspector_protocol::END if session.path().is_empty() => {
                record!("the program ends; the debugger leaves its inspector");
                if let Some(link) = inspector.status.disconnect() {
                    link.connection.close();
                }
            }
            _ => record!("inspector event {method} of {session:?}"),
        }
        Ok(())
    }

    fn attach_worker(&mut self, runtime: RuntimeId, session: &Session) {
        record!("worker {session:?} attached");
        let Some(inspector) = self
            .session
            .inferior_mut()
            .and_then(|inferior| inferior.inspectors.records.get_mut(&runtime))
        else {
            return;
        };
        let Some((link, isolates)) = inspector.status.connected_mut() else {
            return;
        };
        ready_isolate(&link.connection, session);
        isolates.insert(session.clone(), Isolate::default());
        self.sync_inspector(runtime);
        self.release_isolate(runtime, session);
    }
}

fn ready_isolate(connection: &Connection, session: &Session) {
    let _ = send(
        connection,
        session,
        inspector_protocol::ENABLE_DEBUGGER,
        serde_json::json!({}),
    );
    let _ = send(
        connection,
        session,
        inspector_protocol::ENABLE_WORKERS,
        serde_json::json!({ "waitForDebuggerOnStart": true }),
    );
}
