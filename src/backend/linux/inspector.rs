//! Node's inspector, which microscope drives over the Chrome `DevTools`
//! Protocol: finding and hiding its address (F29), connecting, and
//! releasing a node the launch preload holds (F31).
//!
//! The address is read where Node lists its targets,
//! `InspectorIoDelegate::GetTargetIds`, which it calls as the server starts
//! and at every WebSocket upgrade. At the call's return microscope reads the
//! target's UUID from the returned `std::vector<std::string>` and empties
//! the vector, so Node neither prints nor serves the address. Only the call
//! made for microscope's own connection keeps it.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use nix::unistd::Pid;

use super::breakpoints::remove_breakpoint_owner_from;
use super::cdp::{Connection, Incoming, RequestId, Session};
use super::native::{InspectionOps, LinuxTraceOps};
use super::{BreakpointOwner, Controller};
use crate::runtime_model::inspector_protocol;
use crate::runtime_model::{Arming, ManagedArm, ReportedLocal, ReportedValue};
use crate::{Error, ManagedPlace, Result, RuntimeId, StopReason, VirtualAddress};

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

    /// The stack locals V8 named for each interpreted frame of a thread it
    /// holds paused, by frame, valid until that thread runs (G5).
    pub(super) locals: BTreeMap<VirtualAddress, crate::runtime_model::ReportedLocals>,
}

impl Inspectors {
    /// Forgets a site whose memory is gone.
    pub(super) fn forget(&mut self, address: VirtualAddress) {
        self.entries.remove(&address);
        self.returns.remove(&address);
        self.calls.retain(|call| call.returns_to != address);
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
    connection: Option<Link>,

    /// Why the inspector cannot be used, once that is known.
    problem: Option<Arc<str>>,

    /// The managed locations the runtime is armed with.
    desired: Vec<ManagedArm>,

    /// Each managed location set over the current connection, by number.
    sites: BTreeMap<u64, Site>,

    /// The pause V8 reported and holds its thread in, until resumed.
    paused: Option<Paused>,
}

impl Inspector {
    /// Whether anything needs the connection: a client that is connected
    /// when the program ends makes Node wait for it.
    const fn wanted(&self) -> bool {
        !self.desired.is_empty()
    }

    /// Whether the managed location `number` is set, or why it waits.
    fn readiness(&self, number: u64) -> std::result::Result<(), Arc<str>> {
        if let Some(problem) = &self.problem {
            return Err(Arc::clone(problem));
        }
        match self.sites.get(&number) {
            Some(Site {
                state: SiteState::Resolved,
                ..
            }) => Ok(()),
            Some(Site {
                state: SiteState::Refused(reason),
                ..
            }) => Err(Arc::clone(reason)),
            Some(Site {
                state: SiteState::Unresolved,
                ..
            }) => Err("no script by that name has loaded yet".into()),
            Some(Site {
                state: SiteState::Requested(_),
                ..
            }) => Err("waiting for Node's inspector to set the breakpoint".into()),
            None if self.target.is_none() => Err("Node's inspector has not started yet".into()),
            None => Err("Node's inspector is not connected".into()),
        }
    }
}

/// A managed location as the inspector holds it.
#[derive(Debug)]
struct Site {
    /// V8's id for it, once V8 set it.
    id: Option<Arc<str>>,
    state: SiteState,
}

#[derive(Debug)]
enum SiteState {
    Requested(RequestId),

    /// Set, with no loaded script at its place yet.
    Unresolved,
    Resolved,
    Refused(Arc<str>),
}

/// A pause V8 reported.
#[derive(Debug)]
struct Paused {
    session: Session,

    /// The ids of V8's breakpoints the pause hit.
    hits: Vec<Arc<str>>,

    /// The thread V8 holds, once found at an all-stop.
    thread: Option<Pid>,

    /// Whether the pause has been judged, at an all-stop.
    judged: bool,

    /// The paused thread's frames, innermost first, as V8 listed them.
    frames: Vec<PausedFrame>,

    /// The scope listings asked for and not yet answered, by the frame and
    /// scope each lists. The pause is judged once all are answered: the
    /// paused thread answers only until every thread is stopped.
    listing: BTreeMap<RequestId, (usize, usize)>,
}

/// A frame of a pause, as V8 listed it.
#[derive(Debug)]
struct PausedFrame {
    /// Its place: zero-based line and column.
    line: u64,
    column: u64,

    /// Its local scope and then its block scopes, outermost first, which
    /// is the order V8 declares their variables in; each with its
    /// variables once listed.
    scopes: Vec<Option<std::result::Result<Vec<ReportedLocal>, Arc<str>>>>,
}

impl PausedFrame {
    /// The frame's variables in declaration order, or why they are not
    /// known.
    fn locals(&self) -> crate::runtime_model::ReportedLocals {
        let mut locals = Vec::new();
        for scope in &self.scopes {
            match scope {
                Some(Ok(scope)) => locals.extend(scope.iter().cloned()),
                Some(Err(reason)) => return Err(Arc::clone(reason)),
                None => return Err("V8 did not list the frame's variables before the stop".into()),
            }
        }
        Ok(locals.into())
    }
}

/// A connection and the generation its messages carry.
struct Link {
    generation: u64,
    connection: Connection,
    connected: bool,

    /// The release, after whose reply a connection nothing needs closes:
    /// at exit, Node waits for every connected client to leave.
    release: Option<RequestId>,
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
        Ok(())
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
                    self.connect_inspector(call.runtime);
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
            Ok(listeners) if listeners.len() == 1 => listeners[0],
            Ok(listeners) if listeners.is_empty() => {
                self.inspector_problem(runtime, "the inspector's server has no listening socket");
                return;
            }
            Ok(_) => {
                self.inspector_problem(
                    runtime,
                    "the process listens on more than one loopback socket, so microscope cannot \
                     tell which is the inspector's",
                );
                return;
            }
            Err(reason) => {
                self.inspector_problem(runtime, &reason);
                return;
            }
        };

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
                    release: None,
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

    /// Arms the runtime `runtime`, whose inspector the debugger drives,
    /// with `arming`. Its locations are set over the connection when there
    /// is one, and each is ready once V8 places it.
    pub(super) fn arm_inspector(
        &mut self,
        runtime: RuntimeId,
        arming: &Arming<'_>,
    ) -> std::result::Result<(), Arc<str>> {
        if arming.step.is_some() || arming.assignment.is_some() {
            return Err("microscope cannot yet step or assign in JavaScript".into());
        }
        let inferior = self
            .inferior
            .as_mut()
            .ok_or_else(|| Arc::<str>::from("the process is gone"))?;
        let inspector = inferior.inspectors.records.entry(runtime).or_default();
        inspector.desired = arming.breakpoints.to_vec();
        self.sync_inspector(runtime);
        Ok(())
    }

    /// Brings what the inspector of `runtime` holds to what it is armed
    /// with: connects when something needs it, and sets and removes its
    /// breakpoints.
    #[cfg_attr(
        not(debug_assertions),
        expect(unused_variables, reason = "only recorded")
    )]
    fn sync_inspector(&mut self, runtime: RuntimeId) {
        let Some(inspector) = self
            .inferior
            .as_mut()
            .and_then(|inferior| inferior.inspectors.records.get_mut(&runtime))
        else {
            return;
        };

        let Some(link) = inspector.connection.as_ref().filter(|link| link.connected) else {
            if inspector.connection.is_none()
                && inspector.wanted()
                && inspector.target.is_some()
                && inspector.problem.is_none()
            {
                self.connect_inspector(runtime);
            }
            return;
        };

        let desired = inspector
            .desired
            .iter()
            .map(|arm| (arm.number, arm))
            .collect::<BTreeMap<_, _>>();
        let gone = inspector
            .sites
            .keys()
            .filter(|number| !desired.contains_key(number))
            .copied()
            .collect::<Vec<_>>();
        for number in gone {
            if let Some(Site { id: Some(id), .. }) = inspector.sites.remove(&number) {
                let removed = link.connection.send(
                    &Session::main(),
                    inspector_protocol::REMOVE_BREAKPOINT,
                    serde_json::json!({ "breakpointId": id }),
                );
                record!("remove V8 breakpoint {id} of location {number}: {removed:?}");
            }
        }

        for (number, arm) in desired {
            if inspector.sites.contains_key(&number) {
                continue;
            }
            let state = match &arm.place {
                ManagedPlace::Line { path, line } => {
                    let sent = link.connection.send(
                        &Session::main(),
                        inspector_protocol::SET_BREAKPOINT,
                        serde_json::json!({
                            "urlRegex": url_pattern(path),
                            "lineNumber": line.saturating_sub(1),
                        }),
                    );
                    record!("set location {number} at {path}:{line}: {sent:?}");
                    match sent {
                        Ok(request) => SiteState::Requested(request),
                        Err(error) => SiteState::Refused(error.to_string().into()),
                    }
                }
                ManagedPlace::Function { .. } => SiteState::Refused(
                    "microscope cannot yet break at a JavaScript function".into(),
                ),
            };
            inspector.sites.insert(number, Site { id: None, state });
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
        let wanted = inspector.wanted();
        let Some(link) = inspector
            .connection
            .as_mut()
            .filter(|link| link.generation == generation)
        else {
            record!("a message from an ended inspector connection: {incoming:?}");
            return Ok(());
        };

        match incoming {
            Incoming::Connected => {
                link.connected = true;
                if wanted {
                    // Node then reports the program's end, rather than
                    // waiting for the debugger to leave unasked.
                    for (method, params) in [
                        (
                            inspector_protocol::NOTIFY_END,
                            serde_json::json!({ "enabled": true }),
                        ),
                        (inspector_protocol::ENABLE_DEBUGGER, serde_json::json!({})),
                    ] {
                        let sent = link.connection.send(&Session::main(), method, params);
                        record!("{method}: {sent:?}");
                    }
                }
                self.sync_inspector(runtime);
                let Some(link) = self
                    .inferior
                    .as_mut()
                    .and_then(|inferior| inferior.inspectors.records.get_mut(&runtime))
                    .and_then(|inspector| inspector.connection.as_mut())
                else {
                    return Ok(());
                };
                // A node the preload holds waits for this, and runs on.
                let released = link.connection.send(
                    &Session::main(),
                    inspector_protocol::RELEASE,
                    serde_json::json!({}),
                );
                record!("the inspector is connected; released: {released:?}");
                if !wanted {
                    link.release = released.ok();
                }
                self.publish_breakpoints_changed();
            }
            Incoming::Reply { id, .. } if link.release == Some(id) => {
                record!("the node is released, and nothing needs the inspector");
                link.connection.close();
                inspector.connection = None;
            }
            Incoming::Reply { id, result, .. }
                if inspector
                    .paused
                    .as_ref()
                    .is_some_and(|paused| paused.listing.contains_key(&id)) =>
            {
                return self.scope_listed(runtime, id, result);
            }
            Incoming::Reply { id, result, .. } => self.breakpoint_set(runtime, id, result),
            Incoming::Event {
                session,
                method,
                params,
            } => return self.inspector_event(runtime, session, &method, &params),
            Incoming::Closed { reason } => {
                record!("the inspector connection closed: {reason}");
                inspector.connection = None;
                inspector.sites.clear();
                inspector.paused = None;
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

    /// Keeps the variables V8 listed for a scope of the pause in
    /// `runtime`, and has the pause judged once every scope is listed.
    fn scope_listed(
        &mut self,
        runtime: RuntimeId,
        id: RequestId,
        result: std::result::Result<serde_json::Value, super::cdp::CdpError>,
    ) -> Result<()> {
        let Some(paused) = self
            .inferior
            .as_mut()
            .and_then(|inferior| inferior.inspectors.records.get_mut(&runtime))
            .and_then(|inspector| inspector.paused.as_mut())
        else {
            return Ok(());
        };
        let Some((frame, scope)) = paused.listing.remove(&id) else {
            return Ok(());
        };
        let listed = result
            .map_err(|error| Arc::from(error.to_string()))
            .and_then(|properties| scope_locals(&properties));
        if let Some(slot) = paused
            .frames
            .get_mut(frame)
            .and_then(|frame| frame.scopes.get_mut(scope))
        {
            *slot = Some(listed);
        }
        if paused.listing.is_empty() {
            self.judge_inspector_pause(runtime)?;
        }
        Ok(())
    }

    /// Notes what V8 answered to setting a managed location in `runtime`.
    #[cfg_attr(
        not(debug_assertions),
        expect(unused_variables, reason = "only recorded")
    )]
    fn breakpoint_set(
        &mut self,
        runtime: RuntimeId,
        id: RequestId,
        result: std::result::Result<serde_json::Value, super::cdp::CdpError>,
    ) {
        let Some(inspector) = self
            .inferior
            .as_mut()
            .and_then(|inferior| inferior.inspectors.records.get_mut(&runtime))
        else {
            return;
        };
        let Some((number, site)) = inspector
            .sites
            .iter_mut()
            .find(|(_, site)| matches!(site.state, SiteState::Requested(request) if request == id))
        else {
            record!("inspector reply {id:?}: {result:?}");
            return;
        };
        match result {
            Ok(set) => {
                site.id = set["breakpointId"].as_str().map(Arc::from);
                let placed = set["locations"]
                    .as_array()
                    .is_some_and(|locations| !locations.is_empty());
                site.state = if placed {
                    SiteState::Resolved
                } else {
                    SiteState::Unresolved
                };
            }
            Err(error) => site.state = SiteState::Refused(error.to_string().into()),
        }
        record!("location {number} is {:?}", site.state);
        self.publish_breakpoints_changed();
    }

    fn inspector_event(
        &mut self,
        runtime: RuntimeId,
        session: Session,
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

        match method {
            inspector_protocol::RESOLVED => {
                let id = params["breakpointId"].as_str().unwrap_or_default();
                if let Some(site) = inspector
                    .sites
                    .values_mut()
                    .find(|site| site.id.as_deref() == Some(id))
                {
                    site.state = SiteState::Resolved;
                    self.publish_breakpoints_changed();
                }
            }
            inspector_protocol::PAUSED if session.path().is_empty() => {
                return self.inspector_paused(runtime, session, params);
            }
            inspector_protocol::END => {
                record!("the program ends; the debugger leaves its inspector");
                if let Some(link) = inspector.connection.take() {
                    link.connection.close();
                }
                inspector.sites.clear();
                inspector.paused = None;
            }
            _ => record!("inspector event {method} of {session:?}"),
        }
        Ok(())
    }

    /// Asks V8 for the variables of each frame of the pause it reported in
    /// `runtime`, while the paused thread can still answer (G5), and has
    /// the pause judged once they are listed.
    fn inspector_paused(
        &mut self,
        runtime: RuntimeId,
        session: Session,
        params: &serde_json::Value,
    ) -> Result<()> {
        let Some(inspector) = self
            .inferior
            .as_mut()
            .and_then(|inferior| inferior.inspectors.records.get_mut(&runtime))
        else {
            return Ok(());
        };
        let hits = params["hitBreakpoints"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|hit| hit.as_str().map(Arc::from))
            .collect();
        record!("V8 paused: {} {hits:?}", params["reason"]);
        let Some(link) = inspector.connection.as_ref() else {
            return Ok(());
        };
        let mut frames = Vec::new();
        let mut listing = BTreeMap::new();
        for (index, frame) in params["callFrames"]
            .as_array()
            .into_iter()
            .flatten()
            .enumerate()
        {
            let chain = frame["scopeChain"]
                .as_array()
                .map_or(&[][..], Vec::as_slice);
            // The local scope, then the blocks inside it from the
            // outermost; V8 lists the chain innermost first.
            let local = chain
                .iter()
                .position(|scope| scope["type"] == "local")
                .unwrap_or(chain.len());
            let listed = chain[local..].iter().take(1).chain(
                chain[..local]
                    .iter()
                    .rev()
                    .filter(|scope| scope["type"] == "block"),
            );
            let mut scopes = Vec::new();
            for scope in listed {
                let sent = scope["object"]["objectId"]
                    .as_str()
                    .ok_or_else(|| Arc::<str>::from("V8 gave the scope no object"))
                    .and_then(|object| {
                        link.connection
                            .send(
                                &session,
                                inspector_protocol::GET_PROPERTIES,
                                serde_json::json!({
                                    "objectId": object,
                                    "ownProperties": true,
                                    "accessorPropertiesOnly": false,
                                    "generatePreview": false,
                                }),
                            )
                            .map_err(|error| Arc::from(error.to_string()))
                    });
                match sent {
                    Ok(request) => {
                        listing.insert(request, (index, scopes.len()));
                        scopes.push(None);
                    }
                    Err(reason) => scopes.push(Some(Err(reason))),
                }
            }
            frames.push(PausedFrame {
                line: frame["location"]["lineNumber"].as_u64().unwrap_or(u64::MAX),
                column: frame["location"]["columnNumber"]
                    .as_u64()
                    .unwrap_or(u64::MAX),
                scopes,
            });
        }
        let listed = listing.is_empty();
        inspector.paused = Some(Paused {
            session,
            hits,
            thread: None,
            judged: false,
            frames,
            listing,
        });
        if listed {
            self.judge_inspector_pause(runtime)?;
        }
        Ok(())
    }

    /// Has the pause V8 reported in `runtime` judged, once its frames'
    /// variables are listed: at once while the program runs, or as it
    /// resumes when it is stopped.
    fn judge_inspector_pause(&mut self, runtime: RuntimeId) -> Result<()> {
        let inferior = self.inferior.as_ref().ok_or(Error::NotRunning)?;
        if inferior.public_stop.is_none() && inferior.active.is_some() {
            self.queue_internal_edit(super::Edit::InspectorPaused { runtime })?;
            self.finish_barrier_if_ready()?;
        }
        Ok(())
    }

    /// Binds the variables V8 listed for the frames of the pause in
    /// `runtime` to the interpreted frames of `pid`'s stack, pairing them
    /// innermost first while their places agree.
    fn bind_paused_frames(&mut self, runtime: RuntimeId, pid: Pid) -> Result<()> {
        let inferior = self.inferior.as_ref().ok_or(Error::NotRunning)?;
        let Some(paused) = inferior
            .inspectors
            .records
            .get(&runtime)
            .and_then(|inspector| inspector.paused.as_ref())
        else {
            return Ok(());
        };
        let presentation = self.presentation_for_thread(pid, None)?;
        let root = super::frames::StackRoot::of_thread(pid);
        let stack = self.physical_stack(inferior, &root, crate::unwind::DEFAULT_MAX_FRAMES)?;
        let modules = self.unwind_modules(inferior);
        let expanded = self.expand_backtrace(
            inferior,
            &root,
            &stack,
            crate::unwind::DEFAULT_MAX_FRAMES,
            &modules,
            Some(&presentation),
        )?;
        let scripts = expanded.trace.frames.iter().filter(|frame| {
            frame.recorded.as_ref().is_some_and(|recorded| {
                matches!(
                    recorded.language,
                    crate::SourceLanguage::JavaScript | crate::SourceLanguage::TypeScript
                )
            })
        });
        let mut bound = BTreeMap::new();
        for (frame, listed) in scripts.zip(&paused.frames) {
            let place = frame
                .recorded
                .as_ref()
                .and_then(|recorded| recorded.place.as_ref());
            if !place.is_some_and(|place| {
                u64::from(place.line) == listed.line + 1
                    && u64::from(place.column) == listed.column + 1
            }) {
                record!("V8's frames part from the stack's at {frame:?}");
                break;
            }
            let resolved =
                self.resolved_in(inferior, &root, &stack, &modules, &expanded, frame.id)?;
            if let super::frames::FrameScope::Interpreted { frame, .. } = resolved.scope {
                bound.insert(frame, listed.locals());
            }
        }
        record!("V8 named the locals of {} frames", bound.len());
        if let Some(inferior) = self.inferior.as_mut() {
            inferior.inspectors.locals = bound;
        }
        Ok(())
    }

    /// Judges the pause V8 reported in `runtime`, once every thread is
    /// stopped: it stops for the breakpoints it hit whose conditions hold,
    /// or else the program resumes as though it never paused.
    #[cfg_attr(
        not(debug_assertions),
        expect(unused_variables, reason = "only recorded")
    )]
    pub(super) fn apply_inspector_pause(&mut self, runtime: RuntimeId) {
        let Some(inferior) = self.inferior.as_mut() else {
            return;
        };
        // Node runs its main isolate on the process's main thread.
        let pid = inferior.tgid;
        let Some(inspector) = inferior.inspectors.records.get_mut(&runtime) else {
            return;
        };
        let Some(paused) = inspector.paused.as_mut().filter(|paused| !paused.judged) else {
            return;
        };
        paused.judged = true;
        paused.thread = Some(pid);
        if let Err(error) = self.bind_paused_frames(runtime, pid) {
            record!("the pause's frames are not bound: {error}");
        }
        let Some(inferior) = self.inferior.as_mut() else {
            return;
        };
        let Some(inspector) = inferior.inspectors.records.get_mut(&runtime) else {
            return;
        };
        let Some(paused) = inspector.paused.as_ref() else {
            return;
        };
        let numbers = paused
            .hits
            .iter()
            .filter_map(|hit| {
                inspector
                    .sites
                    .iter()
                    .find(|(_, site)| site.id.as_ref() == Some(hit))
                    .map(|(&number, _)| number)
            })
            .collect::<Vec<_>>();
        let owners = numbers
            .iter()
            .filter_map(|&number| inferior.managed.site(runtime, number))
            .flat_map(|(_, owners)| owners.iter().copied())
            .collect::<BTreeSet<_>>();
        if owners.is_empty() {
            record!("V8's pause hit none of microscope's breakpoints");
            return;
        }

        let hits = self.judge_hits(pid, &owners, None);
        if hits.is_empty() {
            return;
        }
        let reason = StopReason::Breakpoint {
            address: None,
            hits,
        };
        let Some(inferior) = self.inferior.as_mut() else {
            return;
        };
        if let Some(thread) = inferior.threads.get_mut(&pid) {
            thread.reason = Some(reason.clone());
        }
        if let Some(barrier) = inferior.barrier.as_mut()
            && barrier.reason.as_ref().is_none_or(|current| {
                super::classify::visible_stop_priority(&reason)
                    > super::classify::visible_stop_priority(current)
            })
        {
            barrier.triggering_thread = pid;
            barrier.reason = Some(reason);
        }
    }

    /// Lets go the threads V8 holds paused among `resumed`, which have just
    /// been resumed; a pause not yet judged is judged at an internal stop.
    #[cfg_attr(
        not(debug_assertions),
        expect(unused_variables, reason = "only recorded")
    )]
    pub(super) fn resume_inspectors(&mut self, resumed: &BTreeSet<Pid>) -> Result<()> {
        let Some(inferior) = self.inferior.as_mut() else {
            return Ok(());
        };
        let mut unjudged = Vec::new();
        for (&runtime, inspector) in &mut inferior.inspectors.records {
            let Some(paused) = inspector.paused.as_ref() else {
                continue;
            };
            if !paused.judged {
                // A pause still listing its variables is judged once
                // they are listed.
                if paused.listing.is_empty() {
                    unjudged.push(runtime);
                }
                continue;
            }
            if !paused
                .thread
                .is_some_and(|thread| resumed.contains(&thread))
            {
                continue;
            }
            if let Some(link) = inspector.connection.as_ref() {
                let resumed = link.connection.send(
                    &paused.session,
                    inspector_protocol::RESUME,
                    serde_json::json!({}),
                );
                record!("V8 resumes: {resumed:?}");
            }
            inspector.paused = None;
        }
        // Names hold only while their thread stays where V8 paused it.
        if !inferior.inspectors.records.values().any(|inspector| {
            inspector
                .paused
                .as_ref()
                .is_some_and(|paused| paused.judged)
        }) {
            inferior.inspectors.locals.clear();
        }

        for runtime in unjudged {
            self.queue_internal_edit(super::Edit::InspectorPaused { runtime })?;
        }
        Ok(())
    }
}

impl<P: InspectionOps> Controller<P> {
    /// Whether the managed location `number` of `runtime` is set, or why
    /// it waits, when the runtime's inspector arms it.
    pub(super) fn inspector_readiness(
        &self,
        runtime: RuntimeId,
        number: u64,
    ) -> Option<std::result::Result<(), Arc<str>>> {
        let inspector = self.inferior.as_ref()?.inspectors.records.get(&runtime)?;
        Some(inspector.readiness(number))
    }

    /// Whether V8 holds thread `thread` paused, where its innermost
    /// JavaScript frame is the stop's.
    pub(super) fn inspector_holds(&self, thread: crate::ThreadId) -> bool {
        self.inferior.as_ref().is_some_and(|inferior| {
            inferior.inspectors.records.values().any(|inspector| {
                inspector
                    .paused
                    .as_ref()
                    .and_then(|paused| paused.thread)
                    .is_some_and(|pid| super::debug_thread_id(pid) == thread)
            })
        })
    }
}

/// The variables a `Runtime.getProperties` reply lists for a scope, in
/// its order.
fn scope_locals(
    properties: &serde_json::Value,
) -> std::result::Result<Vec<ReportedLocal>, Arc<str>> {
    let listed = properties["result"]
        .as_array()
        .ok_or_else(|| Arc::<str>::from("V8 listed no properties for the scope"))?;
    listed
        .iter()
        .map(|property| {
            let name = property["name"]
                .as_str()
                .ok_or_else(|| Arc::<str>::from("V8 listed a variable with no name"))?;
            // V8 makes a value it cannot give an accessor, which it lists
            // with no value.
            let value = if property.get("value").is_none() {
                ReportedValue::Unavailable
            } else {
                reported_value(&property["value"])
            };
            Ok(ReportedLocal {
                name: name.into(),
                value,
            })
        })
        .collect()
}

/// What a CDP `RemoteObject` says a value is.
fn reported_value(object: &serde_json::Value) -> ReportedValue {
    let text = |key: &str| object[key].as_str().map(Arc::<str>::from);
    match object["type"].as_str() {
        Some("undefined") => ReportedValue::Undefined,
        Some("boolean") => object["value"]
            .as_bool()
            .map_or(ReportedValue::Unknown, ReportedValue::Boolean),
        Some("number") => match (
            object["value"].as_f64(),
            object["unserializableValue"].as_str(),
        ) {
            (Some(value), _) => ReportedValue::Number(value),
            (None, Some("NaN")) => ReportedValue::Number(f64::NAN),
            (None, Some("Infinity")) => ReportedValue::Number(f64::INFINITY),
            (None, Some("-Infinity")) => ReportedValue::Number(f64::NEG_INFINITY),
            (None, Some("-0")) => ReportedValue::Number(-0.0),
            _ => ReportedValue::Unknown,
        },
        Some("string") => text("value").map_or(ReportedValue::Unknown, ReportedValue::String),
        Some("bigint") => object["unserializableValue"]
            .as_str()
            .and_then(|digits| digits.strip_suffix('n'))
            .map_or(ReportedValue::Unknown, |digits| {
                ReportedValue::BigInt(digits.into())
            }),
        Some("symbol") => ReportedValue::Symbol,
        Some("object") if object["subtype"] == "null" => ReportedValue::Null,
        Some("object" | "function") => {
            text("className").map_or(ReportedValue::Unknown, ReportedValue::Object)
        }
        _ => ReportedValue::Unknown,
    }
}

/// A JavaScript regular expression matching the URLs of the scripts at
/// `path`: V8 names a `CommonJS` script by its path and an ES module by its
/// `file:` URL, percent-encoded. A relative path matches the scripts whose
/// paths end with its whole components.
fn url_pattern(path: &str) -> String {
    let mut pattern = String::from(if path.starts_with('/') {
        "^(?:file://)?"
    } else {
        "^(?:file://)?(?:.*/)?"
    });
    for character in path.chars() {
        if character.is_ascii_alphanumeric() || "-_~/".contains(character) {
            pattern.push(character);
            continue;
        }
        let literal = if "\\^$.|?*+()[]{}".contains(character) {
            format!("\\{character}")
        } else {
            character.to_string()
        };
        if character == '.' {
            pattern.push_str(&literal);
            continue;
        }
        let mut bytes = [0; 4];
        let encoded = character
            .encode_utf8(&mut bytes)
            .bytes()
            .map(|byte| {
                let hex = format!("{byte:02X}");
                hex.chars()
                    .map(|digit| {
                        if digit.is_ascii_digit() {
                            digit.to_string()
                        } else {
                            format!("[{digit}{}]", digit.to_ascii_lowercase())
                        }
                    })
                    .fold(String::from("%"), |encoded, digit| encoded + &digit)
            })
            .collect::<String>();
        let _ = write!(pattern, "(?:{literal}|{encoded})");
    }
    pattern.push('$');
    pattern
}

/// The loopback TCP sockets process `pid` listens on: its sockets, by the
/// inodes its descriptors name, that `/proc/PID/net/tcp` lists listening on
/// 127.0.0.1.
pub(super) fn loopback_listeners(pid: Pid) -> std::result::Result<Vec<SocketAddr>, Arc<str>> {
    let problem = |error: std::io::Error| -> Arc<str> {
        format!("cannot read the process's sockets: {error}").into()
    };
    let mut inodes = BTreeSet::new();
    for entry in std::fs::read_dir(format!("/proc/{pid}/fd")).map_err(problem)? {
        let Ok(target) = std::fs::read_link(entry.map_err(problem)?.path()) else {
            continue;
        };
        if let Some(inode) = target
            .to_str()
            .and_then(|target| target.strip_prefix("socket:["))
            .and_then(|target| target.strip_suffix(']'))
            .and_then(|inode| inode.parse::<u64>().ok())
        {
            inodes.insert(inode);
        }
    }

    let table = std::fs::read_to_string(format!("/proc/{pid}/net/tcp")).map_err(problem)?;
    Ok(listeners(&table, &inodes))
}

/// The sockets of `table`, as `/proc/net/tcp` writes it, listening on
/// 127.0.0.1 whose inodes are in `inodes`.
fn listeners(table: &str, inodes: &BTreeSet<u64>) -> Vec<SocketAddr> {
    const LISTEN: &str = "0A";

    table
        .lines()
        .skip(1)
        .filter_map(|line| {
            let fields = line.split_whitespace().collect::<Vec<_>>();
            let (local, state, inode) = (fields.get(1)?, fields.get(3)?, fields.get(9)?);
            if *state != LISTEN || !inodes.contains(&inode.parse().ok()?) {
                return None;
            }
            let (address, port) = local.split_once(':')?;
            // The kernel writes the address as a native-endian word.
            let address = Ipv4Addr::from(u32::from_str_radix(address, 16).ok()?.swap_bytes());
            let port = u16::from_str_radix(port, 16).ok()?;
            address
                .is_loopback()
                .then(|| SocketAddr::V4(SocketAddrV4::new(address, port)))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn listeners_are_the_processs_loopback_sockets_that_listen() {
        let table = "  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode
   0: 0100007F:A1B2 00000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 41 1 0 100 0 0 10 0
   1: 00000000:1F90 00000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 42 1 0 100 0 0 10 0
   2: 0100007F:1F91 0100007F:D431 01 00000000:00000000 00:00000000 00000000  1000        0 43 1 0 100 0 0 10 0
   3: 0100007F:1F92 00000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 44 1 0 100 0 0 10 0
";
        assert_eq!(
            listeners(table, &BTreeSet::from([41, 42, 43])),
            vec!["127.0.0.1:41394".parse::<SocketAddr>().expect("an address")],
            "a socket of another process, one on every address, and a connection are not listeners"
        );
    }
}
