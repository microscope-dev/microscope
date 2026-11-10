//! How a Node runtime's inspector connection stands, through every event
//! that changes it: never looked for, its endpoint unreadable, its
//! listener missing or ambiguous, connecting, connected, refused, and
//! closed by Node.
//!
//! Every phase is reached, and every event made, through the controller's
//! entry points and the messages a real loopback connection delivers, so
//! the table holds however the controller keeps the connection. What a
//! client sees of it is each breakpoint's pending reason.

use std::net::{SocketAddr, TcpListener};
use std::os::fd::OwnedFd;
use std::sync::Arc;
use std::time::{Duration, Instant};

use super::super::cdp::Incoming;
use super::super::tests::{
    FlatUnwindInfo, ScriptedRuntime, WatchHarness, scripted_runtime, watch_harness,
};
use super::InspectorMessage;
use super::scripted::FakeInspector;
use crate::backend::ControllerMessage;
use crate::protocol::BreakpointSpec;
use crate::{ImageAddress, RuntimeId};
use rustix::net::{AddressFamily, SocketType};

/// Where the scripted runtime's `GetTargetIds` is.
const HOOK: u64 = 0x50;

/// How long the connection stays silent before a row takes it as settled.
const QUIET: Duration = Duration::from_millis(50);

const TARGET: &str = "0f2b5c1e-uuid";

/// An attached node whose runtime the controller drives through its
/// inspector, with a breakpoint at line 3 of its script.
struct Node {
    harness: WatchHarness,
    model: Arc<ScriptedRuntime>,
    runtime: RuntimeId,

    /// The fake inspectors that listen, the first the one Node lists.
    inspectors: Vec<FakeInspector>,

    /// A socket that takes connections and never answers them.
    silent: Option<TcpListener>,

    /// Sockets bound and not listening, which refuse connections and keep
    /// their ports from other sockets.
    refusing: Vec<OwnedFd>,

    /// Whether the listed inspector was told to close.
    closing: bool,
}

/// `text` with every loopback port written as `PORT`.
fn scrub(text: &str) -> String {
    let mut scrubbed = String::new();
    let mut rest = text;
    while let Some(at) = rest.find("127.0.0.1:") {
        let (before, after) = rest.split_at(at + "127.0.0.1:".len());
        scrubbed.push_str(before);
        scrubbed.push_str("PORT");
        rest = after.trim_start_matches(|c: char| c.is_ascii_digit());
    }
    scrubbed.push_str(rest);
    scrubbed
}

impl Node {
    fn new() -> Self {
        let mut harness = watch_harness(2);
        harness.controller.main.unwind = Arc::new(FlatUnwindInfo);
        harness
            .controller
            .session
            .inferior_mut()
            .expect("inferior")
            .origin = super::super::InferiorOrigin::Attached;
        let model = scripted_runtime(
            &harness,
            ScriptedRuntime::inspected(ImageAddress::new(HOOK)),
        );
        let inferior = harness.controller.session.inferior().expect("inferior");
        let runtime = harness.controller.runtimes(inferior)[0].id;
        harness
            .controller
            .sync_inspector_hooks()
            .expect("the target's entry is planted");
        let mut node = Self {
            harness,
            model,
            runtime,
            inspectors: Vec::new(),
            silent: None,
            refusing: Vec::new(),
            closing: false,
        };
        let said = node.add(3);
        assert_eq!(said, "ok", "the setup's breakpoint");
        node.harness.trace().take_actions();
        node.harness.published();
        node
    }

    fn add(&mut self, line: u64) -> String {
        let spec = BreakpointSpec::Source {
            path: "app.scripted".into(),
            line: crate::LineNumber::new(line).expect("a line"),
        };
        let said = self
            .harness
            .edit(|reply| super::super::Edit::AddBreakpoint {
                spec,
                options: Box::default(),
                reply,
            })
            .try_recv();
        match said {
            Ok(Ok(_)) => "ok".to_owned(),
            Ok(Err(error)) => format!("{error:?}"),
            Err(_) => "pending".to_owned(),
        }
    }

    /// Makes the node's inspector already open at `address`, as Node lists
    /// it, or its endpoint unreadable.
    fn endpoint(&self, endpoint: Result<SocketAddr, &str>) {
        *self.model.inspector_endpoint.lock().expect("the script") = Some(
            endpoint
                .map(|address| (address, Arc::from(TARGET)))
                .map_err(Arc::from),
        );
    }

    /// A fake inspector listens, slower to answer than `QUIET`, so a row
    /// cannot settle by silence while a connection is still being made.
    fn listen(&mut self) -> SocketAddr {
        let inspector = FakeInspector::lagging(QUIET * 2);
        let address = inspector.address;
        self.inspectors.push(inspector);
        address
    }

    /// A loopback address that refuses connections while the node lives.
    fn refusing(&mut self) -> SocketAddr {
        let socket =
            rustix::net::socket(AddressFamily::INET, SocketType::STREAM, None).expect("socket");
        rustix::net::bind(&socket, &SocketAddr::from(([127, 0, 0, 1], 0))).expect("bind");
        let address = rustix::net::getsockname(&socket)
            .expect("the bound address")
            .try_into()
            .expect("an IPv4 address");
        self.refusing.push(socket);
        address
    }

    /// A socket listens that never finishes a handshake.
    fn silent(&mut self) -> SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let address = listener.local_addr().expect("local address");
        self.silent = Some(listener);
        address
    }

    /// The controller looks for the node's inspector.
    fn open(&mut self) -> String {
        self.harness.controller.open_inspectors();
        "-".to_owned()
    }

    /// The node's main thread lists its inspector target, as a WebSocket
    /// upgrade makes it do, the process resumed if it was stopped.
    fn listing(&mut self) -> String {
        let stopped = self
            .harness
            .controller
            .session
            .inferior()
            .is_some_and(|inferior| inferior.run.stop().is_some());
        if stopped && let Err(error) = self.harness.resume() {
            return format!("resume: {error:?}");
        }
        let main = self.harness.threads[0];
        match self.harness.hit_at(main, HOOK) {
            Ok(()) => "-".to_owned(),
            Err(error) => format!("{error:?}"),
        }
    }

    /// A message from a connection that ended long ago.
    fn stale(&mut self) -> String {
        let message = ControllerMessage::Inspector(InspectorMessage {
            generation: 0,
            runtime: self.runtime,
            incoming: Incoming::Closed {
                reason: "an old close".into(),
                answered: false,
            },
        });
        assert!(self.harness.controller.handle_message(message));
        "-".to_owned()
    }

    /// Closes the listed inspector, as Node does when its inspector ends.
    fn close(&mut self) {
        self.closing = true;
        self.inspectors[0].close();
    }

    /// Whether the connection has nothing on the way: no handshake but the
    /// silent socket's, no request the inspector has yet to read, and no
    /// close the controller has yet to hear.
    fn settled(&self) -> bool {
        let inferior = self
            .harness
            .controller
            .session
            .inferior()
            .expect("inferior");
        let Some(inspector) = inferior.inspectors.records.get(&self.runtime) else {
            return true;
        };
        match &inspector.status {
            super::Status::Connecting(server, _) => {
                let silent = self
                    .silent
                    .as_ref()
                    .and_then(|silent| silent.local_addr().ok());
                silent == Some(server.address)
            }
            super::Status::Connected(server, link) => {
                let closing = self.closing && server.address == self.inspectors[0].address;
                !closing
                    && self
                        .inspectors
                        .iter()
                        .filter(|inspector| inspector.address == server.address)
                        .all(|inspector| inspector.last_read() == link.connection.last_id())
            }
            _ => true,
        }
    }

    /// Hands the controller each message the connection delivers until it
    /// settles and falls quiet, and returns what they were.
    fn quiet(&mut self) -> Vec<String> {
        let mut delivered = Vec::new();
        let started = Instant::now();
        let mut last = Instant::now();
        while last.elapsed() < QUIET || !self.settled() {
            assert!(
                started.elapsed() < Duration::from_secs(5),
                "the connection never settled: {delivered:?}"
            );
            let next = self.harness.controller.messages.borrow_mut().try_recv();
            match next {
                Ok(ControllerMessage::Inspector(message)) => {
                    delivered.push(match &message.incoming {
                        Incoming::Connected => "connected".to_owned(),
                        Incoming::Closed { reason, .. } => format!("closed: {reason}"),
                        Incoming::Reply { .. } => "reply".to_owned(),
                        Incoming::Event { method, .. } => format!("event {method}"),
                        Incoming::Malformed { reason, .. } => format!("malformed: {reason}"),
                    });
                    assert!(
                        self.harness
                            .controller
                            .handle_message(ControllerMessage::Inspector(message)),
                        "the controller ends"
                    );
                    last = Instant::now();
                }
                Ok(_) => panic!("a controller message that is not the inspector's"),
                Err(tokio::sync::mpsc::error::TryRecvError::Empty) => {
                    std::thread::sleep(Duration::from_millis(1));
                }
                Err(error) => panic!("the controller's queue failed: {error}"),
            }
        }
        delivered
    }

    /// Each breakpoint's pending reason, then everything done since last
    /// asked: messages delivered, requests the inspector got, events
    /// published, and native actions other than memory traffic.
    fn describe(&mut self) -> String {
        let delivered = self.quiet();
        let snapshot = self.harness.controller.snapshot();
        let breakpoints = snapshot
            .breakpoints
            .iter()
            .map(|breakpoint| {
                let state = breakpoint
                    .pending_locations
                    .first()
                    .map_or_else(|| "ready".into(), |pending| Arc::clone(&pending.reason));
                format!("b{} {state}", breakpoint.id)
            })
            .collect::<Vec<_>>()
            .join("; ");
        let requests = self
            .inspectors
            .iter()
            .flat_map(FakeInspector::methods)
            .collect::<Vec<_>>();
        let published = self
            .harness
            .published()
            .iter()
            .map(|event| {
                let event = format!("{event:?}");
                event
                    .split([' ', '{', '('])
                    .next()
                    .unwrap_or_default()
                    .to_owned()
            })
            .collect::<Vec<_>>();
        let actions = self
            .harness
            .trace()
            .take_actions()
            .into_iter()
            .filter(|action| !action.starts_with("read") && !action.starts_with("write"))
            .collect::<Vec<_>>();
        scrub(&format!(
            "({breakpoints}) messages [{}] requests [{}] published [{}] native [{}]",
            delivered.join(", "),
            requests.join(", "),
            published.join(", "),
            actions.join(", "),
        ))
    }
}

/// Builds the phase `name`.
fn phase(name: &str) -> Node {
    let mut node = Node::new();
    match name {
        "unopened" => {}
        "launched" => {
            node.harness
                .controller
                .session
                .inferior_mut()
                .expect("inferior")
                .origin = super::super::InferiorOrigin::Launched;
            node.open();
        }
        "endpoint unreadable" => {
            node.endpoint(Err("scripted unreadable endpoint"));
            node.open();
        }
        "connecting" => {
            let address = node.silent();
            node.endpoint(Ok(address));
            node.open();
        }
        "connected" | "closed by node" => {
            let address = node.listen();
            node.endpoint(Ok(address));
            node.open();
            if name == "closed by node" {
                node.quiet();
                node.close();
            }
        }
        "refused" => {
            let address = node.refusing();
            // Rows and other tests run beside this one, and none may take
            // the port while the row expects it refused.
            assert!(
                TcpListener::bind(address).is_err(),
                "another socket can take the refused port"
            );
            node.endpoint(Ok(address));
            node.open();
        }
        "listed, no listener" | "listed, two listeners" => {
            let listeners = if name == "listed, no listener" {
                Vec::new()
            } else {
                vec![node.refusing(), node.refusing()]
            };
            node.harness.trace().loopback.replace(Some(listeners));
            node.listing();
        }
        _ => unreachable!("no phase {name}"),
    }
    node.describe();
    node
}

const PHASES: [&str; 9] = [
    "unopened",
    "launched",
    "endpoint unreadable",
    "listed, no listener",
    "listed, two listeners",
    "connecting",
    "connected",
    "refused",
    "closed by node",
];

const EVENTS: [&str; 6] = [
    "add",
    "open",
    "listing, no sockets",
    "listing, one listener",
    "node closes",
    "stale close",
];

/// One event in one phase: what it was answered, and where the connection
/// settles.
fn outcome(name: &str, event: &str) -> Option<String> {
    if event == "node closes" && name != "connected" {
        return None;
    }
    // Whether a launched node's inspector may never start is read from the
    // process with its pid, which the fake's need not be.
    if name == "launched" && !event.starts_with("listing") {
        return None;
    }
    let mut node = phase(name);
    let said = match event {
        "add" => node.add(4),
        "open" => node.open(),
        "listing, no sockets" => node.listing(),
        "listing, one listener" => {
            let address = node.listen();
            node.harness.trace().loopback.replace(Some(vec![address]));
            node.listing()
        }
        "node closes" => {
            node.close();
            "-".to_owned()
        }
        "stale close" => node.stale(),
        _ => unreachable!("no event {event}"),
    };
    Some(format!("{name} / {event}: {said} -> {}", node.describe()))
}

/// Each lookup, listing and connection message, in each phase of a
/// node's inspector connection, ends as it always has: what each
/// breakpoint waits for, and what the controller did on the way.
#[test]
fn each_inspector_event_settles_the_same_in_each_phase() {
    let actual: Vec<String> = std::thread::scope(|scope| {
        #[expect(
            clippy::needless_collect,
            reason = "every row starts before any is joined"
        )]
        let rows: Vec<_> = PHASES
            .iter()
            .flat_map(|name| EVENTS.iter().map(move |event| (*name, *event)))
            .map(|(name, event)| scope.spawn(move || outcome(name, event)))
            .collect();
        rows.into_iter()
            .filter_map(|row| row.join().expect("a row"))
            .collect()
    });
    let rows = actual.join("\n");
    assert_eq!(
        EXPECTED,
        actual.as_slice(),
        "the inspector table changed; its rows are now:\n{rows}"
    );
}

/// The table's rows, as [`outcome`] writes them.
const EXPECTED: &[&str] = &[
    "unopened / add: ok -> (b1 Node's inspector has not started yet; b2 Node's inspector has not started yet) messages [] requests [] published [BreakpointsChanged] native []",
    "unopened / open: - -> (b1 Node's inspector has not started yet) messages [] requests [] published [] native []",
    "unopened / listing, no sockets: - -> (b1 this target has no sockets) messages [] requests [] published [] native [continue 5000 None, continue 5001 None, set_registers 5000 rip=0x50, set_registers 5000 rip=0x0, interrupt 5001]",
    "unopened / listing, one listener: - -> (b1 waiting for Node's inspector to set the breakpoint) messages [connected] requests [NodeRuntime.notifyWhenWaitingForDisconnect, Debugger.enable, NodeWorker.enable, Debugger.setPauseOnExceptions, Debugger.setBreakpointByUrl, Debugger.setBreakpointByUrl, Runtime.enable, NodeRuntime.enable] published [BreakpointsChanged] native [continue 5000 None, continue 5001 None, set_registers 5000 rip=0x50, set_registers 5000 rip=0x0, interrupt 5001]",
    "unopened / stale close: - -> (b1 Node's inspector has not started yet) messages [] requests [] published [] native []",
    "launched / listing, no sockets: - -> (b1 this target has no sockets) messages [] requests [] published [] native [continue 5000 None, continue 5001 None, set_registers 5000 rip=0x50, set_registers 5000 rip=0x0, request_stop 5001]",
    "launched / listing, one listener: - -> (b1 waiting for Node's inspector to set the breakpoint) messages [connected] requests [NodeRuntime.notifyWhenWaitingForDisconnect, Debugger.enable, NodeWorker.enable, Debugger.setPauseOnExceptions, Debugger.setBreakpointByUrl, Debugger.setBreakpointByUrl, Runtime.enable, NodeRuntime.enable] published [BreakpointsChanged] native [continue 5000 None, continue 5001 None, set_registers 5000 rip=0x50, set_registers 5000 rip=0x0, request_stop 5001]",
    "endpoint unreadable / add: ok -> (b1 scripted unreadable endpoint; b2 scripted unreadable endpoint) messages [] requests [] published [BreakpointsChanged] native []",
    "endpoint unreadable / open: - -> (b1 scripted unreadable endpoint) messages [] requests [] published [] native []",
    "endpoint unreadable / listing, no sockets: - -> (b1 this target has no sockets) messages [] requests [] published [] native [continue 5000 None, continue 5001 None, set_registers 5000 rip=0x50, set_registers 5000 rip=0x0, interrupt 5001]",
    "endpoint unreadable / listing, one listener: - -> (b1 waiting for Node's inspector to set the breakpoint) messages [connected] requests [NodeRuntime.notifyWhenWaitingForDisconnect, Debugger.enable, NodeWorker.enable, Debugger.setPauseOnExceptions, Debugger.setBreakpointByUrl, Debugger.setBreakpointByUrl, Runtime.enable, NodeRuntime.enable] published [BreakpointsChanged] native [continue 5000 None, continue 5001 None, set_registers 5000 rip=0x50, set_registers 5000 rip=0x0, interrupt 5001]",
    "endpoint unreadable / stale close: - -> (b1 scripted unreadable endpoint) messages [] requests [] published [] native []",
    "listed, no listener / add: pending -> (b1 the inspector's server has no listening socket) messages [] requests [] published [] native []",
    "listed, no listener / open: - -> (b1 the inspector's server has no listening socket) messages [] requests [] published [] native []",
    "listed, no listener / listing, no sockets: - -> (b1 the inspector's server has no listening socket) messages [] requests [] published [] native [set_registers 5000 rip=0x50, set_registers 5000 rip=0x0]",
    "listed, no listener / listing, one listener: - -> (b1 the inspector's server has no listening socket) messages [] requests [] published [] native [set_registers 5000 rip=0x50, set_registers 5000 rip=0x0]",
    "listed, no listener / stale close: - -> (b1 the inspector's server has no listening socket) messages [] requests [] published [] native []",
    "listed, two listeners / add: pending -> (b1 the process listens on more than one loopback socket, so microscope cannot tell which is the inspector's) messages [] requests [] published [] native []",
    "listed, two listeners / open: - -> (b1 the process listens on more than one loopback socket, so microscope cannot tell which is the inspector's) messages [] requests [] published [] native []",
    "listed, two listeners / listing, no sockets: - -> (b1 the process listens on more than one loopback socket, so microscope cannot tell which is the inspector's) messages [] requests [] published [] native [set_registers 5000 rip=0x50, set_registers 5000 rip=0x0]",
    "listed, two listeners / listing, one listener: - -> (b1 the process listens on more than one loopback socket, so microscope cannot tell which is the inspector's) messages [] requests [] published [] native [set_registers 5000 rip=0x50, set_registers 5000 rip=0x0]",
    "listed, two listeners / stale close: - -> (b1 the process listens on more than one loopback socket, so microscope cannot tell which is the inspector's) messages [] requests [] published [] native []",
    "connecting / add: ok -> (b1 Node's inspector is not connected; b2 Node's inspector is not connected) messages [] requests [] published [BreakpointsChanged] native []",
    "connecting / open: - -> (b1 Node's inspector is not connected) messages [] requests [] published [] native []",
    "connecting / listing, no sockets: - -> (b1 Node's inspector is not connected) messages [] requests [] published [] native [continue 5000 None, continue 5001 None, set_registers 5000 rip=0x50, set_registers 5000 rip=0x0, interrupt 5001]",
    "connecting / listing, one listener: - -> (b1 Node's inspector is not connected) messages [] requests [] published [] native [continue 5000 None, continue 5001 None, set_registers 5000 rip=0x50, set_registers 5000 rip=0x0, interrupt 5001]",
    "connecting / stale close: - -> (b1 Node's inspector is not connected) messages [] requests [] published [] native []",
    "connected / add: ok -> (b1 waiting for Node's inspector to set the breakpoint; b2 waiting for Node's inspector to set the breakpoint) messages [] requests [Debugger.setBreakpointByUrl] published [BreakpointsChanged] native []",
    "connected / open: - -> (b1 waiting for Node's inspector to set the breakpoint) messages [] requests [] published [] native []",
    "connected / listing, no sockets: - -> (b1 waiting for Node's inspector to set the breakpoint) messages [] requests [] published [] native [continue 5000 None, continue 5001 None, set_registers 5000 rip=0x50, set_registers 5000 rip=0x0, interrupt 5001]",
    "connected / listing, one listener: - -> (b1 waiting for Node's inspector to set the breakpoint) messages [] requests [] published [] native [continue 5000 None, continue 5001 None, set_registers 5000 rip=0x50, set_registers 5000 rip=0x0, interrupt 5001]",
    "connected / node closes: - -> (b1 the inspector connection closed: the inspector closed the connection) messages [reply, reply, reply, reply, reply, reply, reply, reply, closed: the inspector closed the connection] requests [] published [BreakpointsChanged, BreakpointsChanged] native []",
    "connected / stale close: - -> (b1 waiting for Node's inspector to set the breakpoint) messages [] requests [] published [] native []",
    "refused / add: ok -> (b1 the inspector connection closed: cannot connect to 127.0.0.1:PORT: Connection refused (os error 111); b2 the inspector connection closed: cannot connect to 127.0.0.1:PORT: Connection refused (os error 111)) messages [] requests [] published [BreakpointsChanged] native []",
    "refused / open: - -> (b1 the inspector connection closed: cannot connect to 127.0.0.1:PORT: Connection refused (os error 111)) messages [] requests [] published [] native []",
    "refused / listing, no sockets: - -> (b1 the inspector connection closed: cannot connect to 127.0.0.1:PORT: Connection refused (os error 111)) messages [] requests [] published [] native [continue 5000 None, continue 5001 None, set_registers 5000 rip=0x50, set_registers 5000 rip=0x0, interrupt 5001]",
    "refused / listing, one listener: - -> (b1 the inspector connection closed: cannot connect to 127.0.0.1:PORT: Connection refused (os error 111)) messages [] requests [] published [] native [continue 5000 None, continue 5001 None, set_registers 5000 rip=0x50, set_registers 5000 rip=0x0, interrupt 5001]",
    "refused / stale close: - -> (b1 the inspector connection closed: cannot connect to 127.0.0.1:PORT: Connection refused (os error 111)) messages [] requests [] published [] native []",
    "closed by node / add: ok -> (b1 the inspector connection closed: the inspector closed the connection; b2 the inspector connection closed: the inspector closed the connection) messages [] requests [] published [BreakpointsChanged] native []",
    "closed by node / open: - -> (b1 the inspector connection closed: the inspector closed the connection) messages [] requests [] published [] native []",
    "closed by node / listing, no sockets: - -> (b1 the inspector connection closed: the inspector closed the connection) messages [] requests [] published [] native [continue 5000 None, continue 5001 None, set_registers 5000 rip=0x50, set_registers 5000 rip=0x0, interrupt 5001]",
    "closed by node / listing, one listener: - -> (b1 the inspector connection closed: the inspector closed the connection) messages [] requests [] published [] native [continue 5000 None, continue 5001 None, set_registers 5000 rip=0x50, set_registers 5000 rip=0x0, interrupt 5001]",
    "closed by node / stale close: - -> (b1 the inspector connection closed: the inspector closed the connection) messages [] requests [] published [] native []",
];
