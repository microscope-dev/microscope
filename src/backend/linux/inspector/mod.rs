//! Node's inspector, which microscope drives over the Chrome `DevTools`
//! Protocol: finding and hiding its address (F29), connecting, and
//! releasing a node the launch preload holds (F31). One connection serves
//! a process: its main isolate, and each worker's through `NodeWorker`
//! sessions inside it, which wait to run until they are armed.
//!
//! The address is read where Node lists its targets,
//! `InspectorIoDelegate::GetTargetIds`, which it calls as the server starts
//! and at every WebSocket upgrade. At the call's return microscope reads the
//! target's UUID from the returned `std::vector<std::string>` and empties
//! the vector, so Node neither prints nor serves the address. Only the call
//! made for microscope's own connection keeps it.

mod arming;
mod functions;
mod interrupts;
mod maps;
mod pauses;
mod protocol;
#[cfg(test)]
mod scripted;
mod steps;

use std::collections::{BTreeMap, BTreeSet};
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use nix::unistd::Pid;

use super::breakpoints::remove_breakpoint_owner_from;
use super::cdp::{Connection, Incoming, RequestId, Session};
use super::native::LinuxTraceOps;
use super::{BreakpointOwner, Controller};
use crate::runtime_model::ManagedArm;
use crate::runtime_model::inspector_protocol;
use crate::{Error, Result, RuntimeId, VirtualAddress};

pub(super) use protocol::{catches_signal, is_script_frame, loopback_listeners};

/// The longest target id read: Node's are 36-character UUIDs.
const MAX_TARGET_ID: u64 = 64;

/// The size of a libstdc++ `std::string`: its data pointer, its length, and
/// a 16-byte buffer that holds short strings.
const STRING_SIZE: u64 = 32;

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
    /// The planted entries of `GetTargetIds`, by the runtime each lists.
    entries: BTreeMap<VirtualAddress, RuntimeId>,

    /// Calls of `GetTargetIds` under way, each awaiting its return.
    calls: Vec<Call>,

    /// The return sites planted for `calls`.
    returns: BTreeSet<VirtualAddress>,

    records: BTreeMap<RuntimeId, Inspector>,

    /// When a client's pause stops every thread natively, unless V8
    /// pauses running JavaScript first.
    pub(super) interrupt: Option<std::time::Instant>,

    /// The stack locals V8 named for each interpreted frame of a thread it
    /// holds paused, by frame, valid until that thread runs (G5).
    pub(super) locals: BTreeMap<VirtualAddress, crate::runtime_model::ReportedLocals>,

    /// How many `SIGUSR1`s microscope sent to open an attached node's
    /// inspector that have not yet arrived; each passes quietly.
    pub(super) opening: u32,

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
        self.returns.remove(&address);
        self.calls.retain(|call| call.returns_to != address);
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
            let Some(generated) = recorded.place.take() else {
                continue;
            };
            let Some(map) = self.maps.get(&generated.path) else {
                continue;
            };
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

    /// Every site planted: the entries and the calls' returns.
    pub(super) fn planted(&self) -> BTreeSet<VirtualAddress> {
        self.entries.keys().chain(&self.returns).copied().collect()
    }
}

#[derive(Debug, Clone, Copy)]
struct Call {
    thread: Pid,
    runtime: RuntimeId,
    returns_to: VirtualAddress,

    /// The stack pointer once the call returns, which tells its return
    /// from a deeper call's return to the same place.
    stack: u64,
}

/// One runtime's inspector.
#[derive(Debug, Default)]
struct Inspector {
    /// The target's id, once its server listed it.
    target: Option<Arc<str>>,

    /// Where its server listens, once found.
    address: Option<SocketAddr>,

    /// Whether the next listing is left whole, for microscope's own
    /// connection's upgrade.
    keep: bool,

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
    /// Plants the entry of each loaded runtime's `GetTargetIds`, and the
    /// returns of its calls under way, and removes the rest.
    pub(super) fn sync_inspector_hooks(&mut self) -> Result<()> {
        let Some(inferior) = self.inferior.as_ref() else {
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
        let live = |pid: &Pid| inferior.threads.contains_key(pid);
        let calls = (inferior.inspectors.calls.iter())
            .filter(|call| live(&call.thread) && entries.values().any(|id| *id == call.runtime))
            .copied()
            .collect::<Vec<_>>();
        let returns = calls
            .iter()
            .map(|call| call.returns_to)
            .collect::<BTreeSet<_>>();
        let planted = inferior.inspectors.planted();
        let needed = entries
            .keys()
            .chain(&returns)
            .copied()
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
            if !entries.contains_key(&address) {
                inferior.inspectors.returns.insert(address);
            }
        }

        inferior.inspectors.entries = entries;
        inferior.inspectors.calls = calls;
        inferior.inspectors.returns = returns;
        self.open_inspectors();
        Ok(())
    }

    /// Opens the inspector of each runtime of an attached node that has
    /// not listed its target, with the `SIGUSR1` Node handles by starting
    /// it (F12). A launched node opens its own, through the preload, and
    /// what may keep it from doing so is noted.
    fn open_inspectors(&mut self) {
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
        let runtimes = inferior
            .inspectors
            .entries
            .values()
            .copied()
            .collect::<Vec<_>>();
        for runtime in runtimes {
            let caught = self.ptrace.catches_signal(pid, super::Signal::SIGUSR1);
            let listening = self.ptrace.loopback_listeners(pid);
            let Some(inferior) = self.inferior.as_mut() else {
                return;
            };
            let inspector = inferior.inspectors.records.entry(runtime).or_default();
            if inspector.looked || inspector.target.is_some() {
                continue;
            }
            inspector.looked = true;
            match caught {
                Ok(true) => {}
                Ok(false) => {
                    inspector.problem = Some(
                        "the node does not handle SIGUSR1, as with --disable-sigusr1, so \
                         microscope cannot open its inspector; start it with \
                         --inspect=127.0.0.1:0"
                            .into(),
                    );
                    continue;
                }
                Err(reason) => {
                    inspector.problem = Some(reason);
                    continue;
                }
            }
            inspector.listening = listening.unwrap_or_default().into_iter().collect();
            match self.ptrace.kill(pid, super::Signal::SIGUSR1) {
                Ok(()) => {
                    record!("SIGUSR1 opens the attached node's inspector");
                    inferior.inspectors.opening += 1;
                }
                Err(error) => {
                    inspector.problem = Some(
                        format!("cannot signal the node to open its inspector: {error}").into(),
                    );
                }
            }
        }
    }

    /// Notes a thread that reached `GetTargetIds`, or the return of a call
    /// of it, if `address` is one: at the entry, every thread stops to plant
    /// the return; at the return, the target is read and hidden.
    pub(super) fn note_inspector_hook(&mut self, pid: Pid, address: VirtualAddress) -> Result<()> {
        let inferior = self.inferior.as_ref().ok_or(Error::NotRunning)?;
        let inspectors = &inferior.inspectors;
        if !inspectors.entries.contains_key(&address) && !inspectors.returns.contains(&address) {
            return Ok(());
        }

        let registers = self.ptrace.registers(pid)?;
        if let Some(&runtime) = inspectors.entries.get(&address) {
            let returns_to = VirtualAddress::new(self.ptrace.read_word(pid, registers.rsp)?);
            record!("{pid} lists the inspector's targets, returning to {returns_to}");
            let inferior = self.inferior.as_mut().ok_or(Error::NotRunning)?;
            inferior.inspectors.calls.push(Call {
                thread: pid,
                runtime,
                returns_to,
                stack: registers.rsp.wrapping_add(8),
            });
            return self.queue_module_refresh();
        }

        let Some(index) = inspectors.calls.iter().position(|call| {
            call.thread == pid && call.returns_to == address && call.stack == registers.rsp
        }) else {
            return Ok(());
        };
        let inferior = self.inferior.as_mut().ok_or(Error::NotRunning)?;
        let call = inferior.inspectors.calls.remove(index);

        // The vector is returned through the hidden pointer, which rax holds.
        let listed = self.list_targets(pid, registers.rax);
        let inferior = self.inferior.as_mut().ok_or(Error::NotRunning)?;
        let inspector = inferior.inspectors.records.entry(call.runtime).or_default();
        match listed {
            Ok(Some((begin, target))) => {
                if inspector.keep {
                    inspector.keep = false;
                    record!("the inspector lists its target {target} for microscope");
                } else {
                    self.ptrace.write_word(pid, registers.rax + 8, begin)?;
                    record!("the inspector's target {target} is hidden");
                }
                let inferior = self.inferior.as_mut().ok_or(Error::NotRunning)?;
                let inspector = inferior.inspectors.records.entry(call.runtime).or_default();
                if inspector.target.is_none() {
                    inspector.target = Some(target);
                    // What the inspector is to do is decided by arming, as
                    // for any runtime: which exceptions pause, as well as
                    // the breakpoints. A node the preload holds waits for
                    // the connection either way.
                    inferior.managed.mark_stale(call.runtime);
                    if self.sync_runtime_arming() {
                        self.publish_breakpoints_changed();
                    }
                    let connected = self.inferior.as_ref().is_some_and(|inferior| {
                        inferior
                            .inspectors
                            .records
                            .get(&call.runtime)
                            .is_some_and(|inspector| inspector.connection.is_some())
                    });
                    if !connected {
                        self.connect_inspector(call.runtime);
                    }
                }
            }
            Ok(None) => {}
            Err(reason) => {
                record!("the inspector's targets cannot be read: {reason}");
                inspector.problem = Some(reason);
            }
        }

        self.queue_module_refresh()
    }

    /// The first target's id in the `std::vector<std::string>` at `vector`,
    /// with the vector's begin, or `None` for an empty vector.
    fn list_targets(
        &self,
        pid: Pid,
        vector: u64,
    ) -> std::result::Result<Option<(u64, Arc<str>)>, Arc<str>> {
        let word = |address: u64| {
            self.ptrace
                .read_word(pid, address)
                .map_err(|error| Arc::<str>::from(error.to_string()))
        };
        let begin = word(vector)?;
        let end = word(vector + 8)?;
        if begin == end {
            return Ok(None);
        }
        if end < begin || (end - begin) % STRING_SIZE != 0 {
            return Err("the inspector's list of targets is not a vector of strings".into());
        }

        let data = word(begin)?;
        let length = word(begin + 8)?;
        if length == 0 || length > MAX_TARGET_ID {
            return Err(format!("the inspector's target id is {length} bytes long").into());
        }
        let mut bytes = Vec::new();
        let mut offset = 0;
        while offset < length {
            bytes.extend_from_slice(&word(data + offset)?.to_le_bytes());
            offset += 8;
        }
        bytes.truncate(usize::try_from(length).map_err(|_| "a target id too long to read")?);
        if !bytes
            .iter()
            .all(|byte| byte.is_ascii_alphanumeric() || *byte == b'-')
        {
            return Err("the inspector's target id is not a UUID".into());
        }
        let target = String::from_utf8(bytes).map_err(|_| "the target id is not text")?;
        Ok(Some((begin, target.into())))
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
        // The connection's upgrade lists the targets again, and must find
        // its own.
        inspector.keep = true;
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
                inspector.keep = false;
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
            } => return self.inspector_event(runtime, &session, &method, &params),
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

    fn inspector_event(
        &mut self,
        runtime: RuntimeId,
        session: &Session,
        method: &str,
        params: &serde_json::Value,
    ) -> Result<()> {
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
