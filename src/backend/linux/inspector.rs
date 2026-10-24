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
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use nix::unistd::Pid;

use super::breakpoints::remove_breakpoint_owner_from;
use super::cdp::{Connection, Incoming, RequestId, Session};
use super::native::LinuxTraceOps;
use super::{BreakpointOwner, Controller};
use crate::runtime_model::inspector_protocol;
use crate::{Error, Result, RuntimeId, VirtualAddress};

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

    /// Whether the next listing is left whole, for microscope's own
    /// connection's upgrade.
    keep: bool,
    connection: Option<Link>,

    /// Why the inspector cannot be used, once that is known.
    problem: Option<Arc<str>>,
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
        let address = match self.ptrace.loopback_listeners(inferior.tgid) {
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

    /// Serves a message from an inspector connection.
    pub(super) fn inspector_message(&mut self, message: InspectorMessage) {
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
            return;
        };
        let Some(link) = inspector
            .connection
            .as_mut()
            .filter(|link| link.generation == generation)
        else {
            record!("a message from an ended inspector connection: {incoming:?}");
            return;
        };

        match incoming {
            Incoming::Connected => {
                link.connected = true;
                // A node the preload holds waits for this, and runs on.
                let released = link.connection.send(
                    &Session::main(),
                    inspector_protocol::RELEASE,
                    serde_json::json!({}),
                );
                record!("the inspector is connected; released: {released:?}");
                link.release = released.ok();
            }
            Incoming::Reply { id, .. } if link.release == Some(id) => {
                record!("the node is released, and nothing needs the inspector");
                link.connection.close();
                inspector.connection = None;
            }
            Incoming::Closed { reason } => {
                record!("the inspector connection closed: {reason}");
                inspector.connection = None;
                inspector.problem =
                    Some(format!("the inspector connection closed: {reason}").into());
            }
            Incoming::Reply { .. } | Incoming::Event { .. } | Incoming::Malformed { .. } => {
                record!("inspector: {incoming:?}");
            }
        }
    }
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
