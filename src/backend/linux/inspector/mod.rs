//! Node's inspector, which microscope drives over the Chrome `DevTools`
//! Protocol: finding and hiding its address (F29), connecting, and
//! releasing a node the launch preload holds (F31). One connection serves
//! a process: its main isolate, and each worker's through `NodeWorker`
//! sessions inside it, which wait to run until they are armed.
//!
//! The address is read where Node lists its targets,
//! `InspectorIoDelegate::GetTargetIds`, which it calls as the server starts
//! and at every WebSocket upgrade. At entry microscope reads the delegate's
//! immutable UUID and returns an empty vector for proven print/list callers,
//! before any string is allocated. Native upgrade validation runs unchanged.

mod arming;
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
use super::{BreakpointOwner, Controller};
use crate::runtime_model::ManagedArm;
use crate::runtime_model::inspector_protocol;
use crate::{Error, Result, RuntimeId, VirtualAddress};

pub(super) use protocol::{is_script_frame, loopback_listeners};

/// Numbers every connection, across processes, so that a message from a
/// connection that has ended is never taken for a newer one's.
static GENERATIONS: AtomicU64 = AtomicU64::new(1);

/// A message from the inspector connection of `generation`.
#[derive(Debug)]
pub struct InspectorMessage {
    generation: u64,
    runtime: RuntimeId,
    incoming: Incoming,
}

/// The inspectors of one process's runtimes and the sites that find them.
#[derive(Debug, Default)]
pub(super) struct Inspectors {
    pub(super) cleanup: Option<teardown::Cleanup>,
    startup: BTreeMap<RuntimeId, quiescence::Startup>,
    startup_planted: BTreeSet<VirtualAddress>,
    /// The planted entries of `GetTargetIds`, by the runtime each lists.
    entries: BTreeMap<VirtualAddress, RuntimeId>,

    records: BTreeMap<RuntimeId, Inspector>,

    /// When a client's pause stops every thread natively, unless V8
    /// pauses running JavaScript first.
    pub(super) interrupt: Option<std::time::Instant>,

    /// The stack locals V8 named for each interpreted frame of a thread it
    /// holds paused, by frame, valid until that thread runs (G5).
    pub(super) locals: BTreeMap<VirtualAddress, crate::runtime_model::ReportedLocals>,

    /// The source map of each script that names one, by the script's
    /// path, from any isolate.
    maps: BTreeMap<Arc<str>, maps::ScriptMap>,
}

impl Inspectors {
    /// Ends the interrupt under way, withdrawing the pauses not yet sent.
    pub(in crate::backend::linux) fn end_interrupt(&mut self) -> bool {
        for isolate in self.isolates_mut() {
            if isolate.pause == PauseRequest::Deferred {
                isolate.pause = PauseRequest::None;
            }
        }
        self.interrupt.take().is_some()
    }

    fn isolates_mut(&mut self) -> impl Iterator<Item = &mut Isolate> {
        self.records
            .values_mut()
            .flat_map(|inspector| inspector.isolates.values_mut())
    }

    /// Forgets a site whose memory is gone.
    pub(super) fn forget(&mut self, address: VirtualAddress) {
        self.entries.remove(&address);
        self.startup_planted.remove(&address);
    }

    /// Shows each script frame of `frames` at the original place its
    /// script's source map leads to, keeping the generated one. A frame in
    /// generated code the map leads nowhere loses its place, with why.
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
            // The function's first line is the generated code's.
            recorded.first_line = None;
            recorded.generated = Some(generated);
        }
        *frames = shown.into();
    }

    /// Every target, cleanup, and startup checkpoint planted.
    pub(super) fn planted(&self) -> BTreeSet<VirtualAddress> {
        self.entries
            .keys()
            .copied()
            .chain(self.cleanup.iter().flat_map(teardown::Cleanup::sites))
            .chain(self.startup_planted.iter().copied())
            .collect()
    }
}

/// One runtime's inspector.
#[derive(Debug, Default)]
struct Inspector {
    ownership: Ownership,
    owned_agent: Option<VirtualAddress>,
    /// The target's id, once its server listed it.
    target: Option<Arc<str>>,

    /// Where its server listens, once found.
    address: Option<SocketAddr>,

    /// The loopback sockets the process listened on before microscope
    /// opened its inspector, none of which is the inspector's.
    listening: BTreeSet<SocketAddr>,

    /// Whether microscope looked at how the inspector opens: by the
    /// `SIGUSR1` it sends a node it attached to, or by the launch preload.
    looked: bool,

    /// Why the inspector of a launched node may never start, if its
    /// command line and environment tell.
    unstarted: Option<&'static str>,
    connection: Option<Link>,

    /// Why the inspector cannot be used, once that is known.
    problem: Option<Arc<str>>,

    /// The managed locations the runtime is armed with, which every
    /// isolate is given.
    desired: Vec<ManagedArm>,

    /// The step the runtime is armed with, which V8 runs.
    step: Option<steps::Stepping>,

    /// Which exceptions V8 is to pause at, as `setPauseOnExceptions`
    /// names them.
    pausing: &'static str,

    /// The main isolate and each worker's, by the session that reaches
    /// it, while connected.
    isolates: BTreeMap<Session, Isolate>,
}

/// What one isolate holds and does, as its session reported it.
#[derive(Debug, Default)]
struct Isolate {
    /// Each managed location set in it, by number.
    sites: BTreeMap<u64, Site>,

    /// Placement replies still owed for breakpoints already removed.
    withdrawn: BTreeSet<RequestId>,

    /// The URL of each script V8 parsed, by its id: V8's pauses name
    /// scripts only by id (F19).
    scripts: BTreeMap<Arc<str>, Arc<str>>,

    /// The breakpoint at the start of every script, while there are line
    /// breakpoints, whose pause lets a script's mapped breakpoints be set
    /// before it runs.
    sentinel: Option<Sentinel>,

    /// A sentinel's pause, as V8 reported it, judged once V8 answers the
    /// mapped breakpoints set in its script: V8 sets them while paused,
    /// and does not count one at the pause's own place hit.
    deferred: Option<serde_json::Value>,

    /// The search of its functions under way, for function breakpoints.
    search: Option<functions::Search>,

    /// The scripts whose start a search ran at.
    searched: BTreeSet<Arc<str>>,

    /// The text of each script a function breakpoint is placed in.
    texts: BTreeMap<Arc<str>, Arc<functions::Text>>,

    /// Whether its profiler, which lists its functions, is enabled.
    profiling: bool,

    /// The pause V8 reported and holds its thread in, until resumed.
    paused: Option<pauses::Paused>,

    /// Which exceptions it was last told to pause at.
    told: Option<&'static str>,

    /// Whether V8 was told to resume and has not yet left its pause.
    resuming: bool,
    pause: PauseRequest,
}

/// Where the client's request that V8 pause is.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
enum PauseRequest {
    #[default]
    None,

    /// It waits for V8 to leave its pause, as V8 drops one asked for
    /// before.
    Deferred,

    /// V8 was asked and has not paused since. It reports that pause as it
    /// does a `debugger` statement.
    Sent,
}

/// A managed location as an isolate holds it: set by its path, and at
/// each place in a script that a source map leads its line to.
#[derive(Debug, Default)]
struct Site {
    placements: Vec<Placement>,
}

/// One of V8's breakpoints.
#[derive(Debug)]
struct Placement {
    /// V8's id for it, once V8 set it.
    id: Option<Arc<str>>,
    state: SiteState,

    /// The script and zero-based place a source map led it to; `None` for
    /// one set by the location's path.
    mapped: Option<(Arc<str>, crate::debug_info::js_source_map::Position)>,

    /// The zero-based line and column V8 placed it at, once it answered:
    /// the breakable place at or after the one asked for.
    actual: Option<(u64, u64)>,

    /// The script and offset of the function a function breakpoint was
    /// placed in, as V8's coverage starts it.
    function: Option<(Arc<str>, u32)>,
}

/// An isolate's sentinel breakpoint and the URL pattern it was set with.
#[derive(Debug)]
struct Sentinel {
    pattern: String,
    placement: Placement,
}

#[derive(Debug)]
enum SiteState {
    Requested(RequestId),

    /// Set, with no loaded script at its place yet.
    Unresolved,
    Resolved,
    Refused(Arc<str>),

    /// Requested by path, then found to fall in a script whose source map
    /// moves its lines: removed once V8 names it.
    Withdrawn(RequestId),
}

/// A connection and the generation its messages carry.
struct Link {
    generation: u64,
    connection: Connection,
    connected: bool,
}

impl std::fmt::Debug for Link {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Link")
            .field("generation", &self.generation)
            .field("connected", &self.connected)
            .finish_non_exhaustive()
    }
}

/// Sends `method` to the isolate `session` reaches, and records what came
/// of it.
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
    /// Plants each loaded runtime's target entry and cleanup checkpoints.
    pub(super) fn sync_inspector_hooks(&mut self) -> Result<()> {
        let Some(inferior) = self.inferior.as_ref().filter(|inferior| !inferior.observer) else {
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

        let inferior = self.inferior.as_mut().ok_or(Error::NotRunning)?;
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
            .collect::<BTreeSet<_>>();

        // The record changes with each site, so that a failed edit leaves
        // it naming exactly the sites planted.
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

    /// Opens the inspector of each runtime of an attached node that has
    /// not listed its target, with the `SIGUSR1` Node handles by starting
    /// it (F12). A launched node opens its own, through the preload, and
    /// what may keep it from doing so is noted.
    pub(super) fn open_inspectors(&mut self) {
        let Some(inferior) = self.inferior.as_mut() else {
            return;
        };
        let pid = inferior.tgid;
        if inferior.origin != super::InferiorOrigin::Attached {
            let inspectors = &mut inferior.inspectors;
            for &runtime in inspectors.entries.values() {
                let inspector = inspectors.records.entry(runtime).or_default();
                if !inspector.looked {
                    inspector.looked = true;
                    inspector.unstarted = super::node_launch::unstarted(pid.as_raw());
                }
            }
            return;
        }
        let Some(inferior) = self.inferior.as_ref() else {
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
                || self.inferior.as_ref().is_some_and(|inferior| {
                    inferior
                        .inspectors
                        .records
                        .get(&runtime)
                        .is_some_and(|inspector| inspector.looked || inspector.target.is_some())
                })
            {
                continue;
            }
            let Some(inferior) = self.inferior.as_ref() else {
                return;
            };
            let existing = self.with_runtime_stop(inferior, &bound, pid, |stop| {
                bound.model.inspector_endpoint(stop)
            });
            let existing = match existing {
                Ok(existing) => existing,
                Err(reason) => {
                    self.inspector_problem(runtime, &reason);
                    if let Some(inferior) = self.inferior.as_mut() {
                        inferior
                            .inspectors
                            .records
                            .entry(runtime)
                            .or_default()
                            .looked = true;
                    }
                    continue;
                }
            };
            if let Some((address, target)) = existing {
                let Some(inferior) = self.inferior.as_mut() else {
                    return;
                };
                let inspector = inferior.inspectors.records.entry(runtime).or_default();
                inspector.looked = true;
                inspector.address = Some(address);
                inspector.target = Some(target);
                self.open_connection(runtime, address);
                continue;
            }
            // Do not enqueue Node's asynchronous startup before the first
            // resume: an immediate detach must leave no pending server open.
            if self
                .inferior
                .as_ref()
                .is_some_and(|inferior| inferior.active.is_some())
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
                    .inferior
                    .as_ref()
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
        let Some(inferior) = self.inferior.as_mut() else {
            return;
        };
        if let Some(pending) = interrupted
            && let Some(thread) = inferior.threads.get_mut(&pid)
        {
            thread.pending_signal = Some(pending);
        }
        let inspector = inferior.inspectors.records.entry(bound.id).or_default();
        inspector.looked = true;
        match opened {
            Ok(true) => {
                inspector.ownership = Ownership::Created;
                inspector.listening = listening.unwrap_or_default().into_iter().collect();
                record!("SIGUSR1 opens the attached node's inspector");
            }
            Ok(false) => {
                inspector.problem = Some(
                    "the node's SIGUSR1 disposition is not Node's native inspector handler; \
                     microscope will not deliver SIGUSR1 to application code; start it with \
                     --inspect=127.0.0.1:0"
                        .into(),
                );
            }
            Err(reason) => inspector.problem = Some(reason),
        }
    }

    /// Suppresses proven print/list calls before they allocate a UUID copy.
    pub(super) fn note_inspector_hook(&mut self, pid: Pid, address: VirtualAddress) -> Result<()> {
        if self.note_inspector_startup_hook(pid, address)? {
            return Ok(());
        }
        if self.note_inspector_cleanup_hook(pid, address)? {
            return Ok(());
        }
        let inferior = self.inferior.as_ref().ok_or(Error::NotRunning)?;
        let Some(&runtime) = inferior.inspectors.entries.get(&address) else {
            return Ok(());
        };
        let bound = self
            .runtimes(inferior)
            .into_iter()
            .find(|bound| bound.id == runtime)
            .ok_or(Error::NotRunning)?;
        let mut registers = self.ptrace.registers(pid)?;
        let caller = VirtualAddress::new(self.ptrace.read_word(pid, registers.rsp)?);
        let target = self
            .with_runtime_stop(inferior, &bound, pid, |stop| {
                bound
                    .model
                    .inspector_target(stop, caller, VirtualAddress::new(registers.rsi))
            })
            .map_err(Error::RuntimeAgent)?;
        let Some(target) = target else {
            return Ok(());
        };
        // SysV's nontrivial vector result is uninitialized storage in rdi.
        // No element has been constructed: an all-null empty vector owns nothing.
        for offset in [0, 8, 16] {
            let address = registers.rdi.checked_add(offset).ok_or_else(|| {
                Error::RuntimeAgent("the inspector result address overflows".into())
            })?;
            self.ptrace.write_word(pid, address, 0)?;
        }
        registers.rax = registers.rdi;
        registers.rip = caller.get();
        registers.rsp = registers
            .rsp
            .checked_add(8)
            .ok_or_else(|| Error::RuntimeAgent("the inspector return stack overflows".into()))?;
        self.ptrace.set_registers(pid, registers)?;
        let inferior = self.inferior.as_mut().ok_or(Error::NotRunning)?;
        inferior.thread_mut(pid)?.stopped_at_breakpoint = None;
        let inspector = inferior.inspectors.records.entry(runtime).or_default();
        if inspector.target.is_none() {
            inspector.target = Some(target);
            inferior.managed.mark_stale(runtime);
            if self.sync_runtime_arming() {
                self.publish_breakpoints_changed();
            }
            self.connect_inspector(runtime);
        }
        self.queue_module_refresh()
    }

    /// Connects to the inspector of `runtime`, whose target is known, at
    /// the process's one loopback listener.
    fn connect_inspector(&mut self, runtime: RuntimeId) {
        let Some(inferior) = self.inferior.as_ref() else {
            return;
        };
        let known = inferior
            .inspectors
            .records
            .get(&runtime)
            .and_then(|inspector| inspector.address);
        let address = match known.map_or_else(
            || self.ptrace.loopback_listeners(inferior.tgid),
            |address| Ok(vec![address]),
        ) {
            Ok(listeners) => {
                let listening = inferior
                    .inspectors
                    .records
                    .get(&runtime)
                    .map(|inspector| &inspector.listening);
                let new = listeners
                    .into_iter()
                    .filter(|listener| {
                        listening.is_none_or(|listening| !listening.contains(listener))
                    })
                    .collect::<Vec<_>>();
                self.inspector_listener(runtime, &new)
            }
            Err(reason) => {
                self.inspector_problem(runtime, &reason);
                return;
            }
        };
        let Some(address) = address else {
            return;
        };
        self.open_connection(runtime, address);
    }

    /// The inspector's socket among `listeners`, the loopback sockets the
    /// process opened since microscope looked, or `None` with the problem
    /// noted.
    fn inspector_listener(
        &mut self,
        runtime: RuntimeId,
        listeners: &[SocketAddr],
    ) -> Option<SocketAddr> {
        match listeners {
            [listener] => Some(*listener),
            [] => {
                self.inspector_problem(runtime, "the inspector's server has no listening socket");
                None
            }
            _ => {
                self.inspector_problem(
                    runtime,
                    "the process listens on more than one loopback socket, so microscope cannot \
                     tell which is the inspector's",
                );
                None
            }
        }
    }

    /// Connects to the inspector of `runtime` listening at `address`.
    fn open_connection(&mut self, runtime: RuntimeId, address: SocketAddr) {
        let Some(inspector) = self
            .inferior
            .as_mut()
            .and_then(|inferior| inferior.inspectors.records.get_mut(&runtime))
        else {
            return;
        };
        let Some(target) = inspector.target.clone() else {
            return;
        };
        inspector.address = Some(address);
        let generation = GENERATIONS.fetch_add(1, Ordering::Relaxed);
        let sender = self.message_sender.clone();
        let opened = Connection::open(address, &format!("/{target}"), move |incoming| {
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
        match opened {
            Ok(connection) => {
                inspector.connection = Some(Link {
                    generation,
                    connection,
                    connected: false,
                });
            }
            Err(error) => {
                inspector.problem =
                    Some(format!("cannot connect to the inspector: {error}").into());
            }
        }
    }

    fn inspector_problem(&mut self, runtime: RuntimeId, reason: &str) {
        record!("the inspector of runtime {runtime} cannot be used: {reason}");
        if let Some(inferior) = self.inferior.as_mut() {
            inferior
                .inspectors
                .records
                .entry(runtime)
                .or_default()
                .problem = Some(reason.into());
        }
    }

    /// Serves a message from an inspector connection.
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
            .inferior
            .as_mut()
            .and_then(|inferior| inferior.inspectors.records.get_mut(&runtime))
        else {
            return Ok(());
        };
        if inspector
            .connection
            .as_ref()
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
                if inspector.isolates.values().any(|isolate| {
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
                    if let Some(link) = inspector.connection.as_ref() {
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
            Incoming::Closed { reason } => {
                record!("the inspector connection closed: {reason}");
                inspector.connection = None;
                inspector.isolates.clear();
                inspector.problem =
                    Some(format!("the inspector connection closed: {reason}").into());
                self.publish_breakpoints_changed();
            }
            Incoming::Malformed { session, reason } => {
                record!("inspector {session:?} sent a malformed message: {reason}");
            }
        }
        Ok(())
    }

    /// Readies the main isolate of a connection just made, and lets a node
    /// the preload holds run on.
    fn inspector_connected(&mut self, runtime: RuntimeId) {
        let Some(inspector) = self
            .inferior
            .as_mut()
            .and_then(|inferior| inferior.inspectors.records.get_mut(&runtime))
        else {
            return;
        };
        let Some(link) = inspector.connection.as_mut() else {
            return;
        };
        link.connected = true;
        // The connection stays while the process is debugged, so an
        // interrupt can pause its JavaScript (F20: it costs nothing while
        // it runs). Node then reports the program's end, rather than
        // waiting for the debugger to leave unasked.
        let main = Session::main();
        let _ = send(
            &link.connection,
            &main,
            inspector_protocol::NOTIFY_END,
            serde_json::json!({ "enabled": true }),
        );
        ready_isolate(&link.connection, &main);
        inspector.isolates.insert(main.clone(), Isolate::default());
        self.sync_inspector(runtime);
        // A release Node takes before the preload's waitForDebugger begins
        // is lost, so the release answers Node's report that it waits.
        if let Some(link) = self
            .inferior
            .as_ref()
            .and_then(|inferior| inferior.inspectors.records.get(&runtime))
            .and_then(|inspector| inspector.connection.as_ref())
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

    /// Lets the isolate `session` reaches run on, once it is armed: a node
    /// the preload holds, or a worker waiting for its debugger.
    fn release_isolate(&self, runtime: RuntimeId, session: &Session) {
        if let Some(link) = self
            .inferior
            .as_ref()
            .and_then(|inferior| inferior.inspectors.records.get(&runtime))
            .and_then(|inspector| inspector.connection.as_ref())
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
        let Some(inferior) = self.inferior.as_mut() else {
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
        let Some(target) = inspector.target.as_ref() else {
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
            .inferior
            .as_mut()
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
                inspector
                    .isolates
                    .retain(|session, _| !session.path().starts_with(worker.path()));
                self.publish_breakpoints_changed();
            }
            return Ok(());
        }
        let Some(isolate) = inspector.isolates.get_mut(session) else {
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
                if isolate.pause == PauseRequest::Deferred
                    && let Some(link) = inspector.connection.as_ref()
                {
                    isolate.pause = match send(
                        &link.connection,
                        session,
                        inspector_protocol::PAUSE,
                        serde_json::json!({}),
                    ) {
                        Ok(_) => PauseRequest::Sent,
                        Err(_) => PauseRequest::None,
                    };
                }
            }
            inspector_protocol::PAUSED => {
                return self.inspector_paused(runtime, session, params);
            }
            inspector_protocol::WAITING => self.release_isolate(runtime, session),
            inspector_protocol::END if session.path().is_empty() => {
                record!("the program ends; the debugger leaves its inspector");
                if let Some(link) = inspector.connection.take() {
                    link.connection.close();
                }
                inspector.isolates.clear();
            }
            _ => record!("inspector event {method} of {session:?}"),
        }
        Ok(())
    }

    /// Readies a worker's isolate, which waits for its debugger: it is
    /// given the process's breakpoints and exceptions, then runs on.
    fn attach_worker(&mut self, runtime: RuntimeId, session: &Session) {
        record!("worker {session:?} attached");
        let Some(inspector) = self
            .inferior
            .as_mut()
            .and_then(|inferior| inferior.inspectors.records.get_mut(&runtime))
        else {
            return;
        };
        let Some(link) = inspector.connection.as_ref() else {
            return;
        };
        ready_isolate(&link.connection, session);
        inspector
            .isolates
            .insert(session.clone(), Isolate::default());
        self.sync_inspector(runtime);
        self.release_isolate(runtime, session);
    }
}

/// Has the isolate `session` reaches report its scripts and pauses, and
/// its workers as each starts, holding each until it is armed.
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
