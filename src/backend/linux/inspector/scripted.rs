//! The controller's inspector logic against a fake inspector whose messages
//! each test orders exactly beside ptrace events (plans/node.html N3).
//!
//! The fake serves one WebSocket on loopback. Each message it sends waits in
//! the controller's queue until the test hands it on, so the test decides
//! where it falls among the native stops it reports through `FakeTrace`.

use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::{Arc, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use nix::libc;
use serde_json::{Value, json};
use tungstenite::{Message, WebSocket};

use super::super::cdp::{Incoming, Session};
use super::super::tests::{
    FlatUnwindInfo, ScriptedRuntime, WatchHarness, scripted_runtime, watch_harness,
};
use super::InspectorMessage;
use crate::backend::ControllerMessage;
use crate::protocol::{BreakpointSpec, DebuggerEvent, StopReason};
use crate::runtime_model::inspector_protocol as cdp;
use crate::{Breakpoint, ImageAddress, RuntimeId};

/// How long any wait of a test lasts before it fails.
const DEADLINE: Duration = Duration::from_secs(5);

/// The address of the scripted runtime's `GetTargetIds`, never hit here.
const HOOK: u64 = 0x50;

/// A request the controller sent, with its worker tunnels taken off.
#[derive(Debug, Clone)]
struct Request {
    /// The worker sessions it went through, outermost first.
    session: Vec<String>,
    id: u64,
    method: String,
    params: Value,
}

impl Request {
    /// The method, after the worker session it went to, if any.
    fn named(&self) -> String {
        match self.session.as_slice() {
            [] => self.method.clone(),
            path => format!("{} {}", path.join("/"), self.method),
        }
    }
}

enum Command {
    Send(Value),
    Close,
}

/// A fake Node inspector, scripted by the test that owns it.
struct FakeInspector {
    address: SocketAddr,
    requests: mpsc::Receiver<Request>,
    commands: mpsc::Sender<Command>,
    gone: mpsc::Receiver<()>,
}

impl FakeInspector {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let address = listener.local_addr().expect("local address");
        let (request_sender, requests) = mpsc::channel();
        let (commands, command_receiver) = mpsc::channel();
        let (gone_sender, gone) = mpsc::channel();
        thread::spawn(move || {
            if let Some(socket) = accept(&listener) {
                serve(socket, &request_sender, &command_receiver);
            }
            let _ = gone_sender.send(());
        });
        Self {
            address,
            requests,
            commands,
            gone,
        }
    }

    /// The next request the controller sent.
    fn request(&self) -> Request {
        self.requests
            .recv_timeout(DEADLINE)
            .expect("a request from the controller")
    }

    /// Sends `message` as the isolate at `session` does.
    fn send(&self, session: &[String], mut message: Value) {
        for worker in session.iter().rev() {
            message = json!({
                "method": "NodeWorker.receivedMessageFromWorker",
                "params": {"sessionId": worker, "message": message.to_string()},
            });
        }
        self.commands
            .send(Command::Send(message))
            .expect("the fake inspector runs");
    }

    fn reply(&self, request: &Request, result: Value) {
        let mut message = json!({"id": request.id});
        message["result"] = result;
        self.send(&request.session, message);
    }

    fn event(&self, session: &[&str], method: &str, params: Value) {
        let session = session.iter().map(|&id| id.to_owned()).collect::<Vec<_>>();
        let mut message = json!({"method": method});
        message["params"] = params;
        self.send(&session, message);
    }

    /// Closes the WebSocket, as a node that ends its inspector does.
    fn close(&self) {
        self.commands
            .send(Command::Close)
            .expect("the fake inspector runs");
    }

    /// Waits for the controller's side of the connection to go away.
    fn wait_gone(&self) {
        self.gone
            .recv_timeout(DEADLINE)
            .expect("the controller closes the connection");
    }
}

/// The one connection the fake serves, once the controller makes it.
fn accept(listener: &TcpListener) -> Option<WebSocket<TcpStream>> {
    listener.set_nonblocking(true).ok()?;
    let started = Instant::now();
    let stream = loop {
        match listener.accept() {
            Ok((stream, _)) => break stream,
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                if started.elapsed() > DEADLINE * 4 {
                    return None;
                }
                thread::sleep(Duration::from_millis(1));
            }
            Err(_) => return None,
        }
    };
    stream.set_nonblocking(false).ok()?;
    let socket = tungstenite::accept(stream).ok()?;
    socket
        .get_ref()
        .set_read_timeout(Some(Duration::from_millis(2)))
        .ok()?;
    Some(socket)
}

/// Passes what the controller sends to the test, and sends what the test
/// scripts, until either side closes.
fn serve(
    mut socket: WebSocket<TcpStream>,
    requests: &mpsc::Sender<Request>,
    commands: &mpsc::Receiver<Command>,
) {
    loop {
        loop {
            match commands.try_recv() {
                Ok(Command::Send(message)) => {
                    if socket.send(Message::text(message.to_string())).is_err() {
                        return;
                    }
                }
                Ok(Command::Close) => {
                    let _ = socket.close(None);
                    let _ = socket.flush();
                }
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => return,
            }
        }
        match socket.read() {
            Ok(Message::Text(text)) => {
                let message = serde_json::from_str(text.as_str()).expect("a JSON request");
                let _ = requests.send(untunnel(message));
            }
            Ok(_) => {}
            Err(tungstenite::Error::Io(error))
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) => {}
            Err(_) => return,
        }
    }
}

/// The request inside every `NodeWorker.sendMessageToWorker` around it.
fn untunnel(mut message: Value) -> Request {
    let mut session = Vec::new();
    while message["method"] == "NodeWorker.sendMessageToWorker" {
        let params = &message["params"];
        session.push(params["sessionId"].as_str().expect("a worker").to_owned());
        message = serde_json::from_str(params["message"].as_str().expect("a tunneled message"))
            .expect("a JSON tunneled message");
    }
    Request {
        session,
        id: message["id"].as_u64().expect("a request id"),
        method: message["method"].as_str().expect("a method").to_owned(),
        params: message["params"].clone(),
    }
}

/// A controller whose one runtime's inspector the fake serves.
struct Scripted {
    harness: WatchHarness,
    inspector: FakeInspector,
    runtime: RuntimeId,
    _model: Arc<ScriptedRuntime>,
}

impl Scripted {
    /// A stopped process of `threads` threads, the first the main isolate's,
    /// whose inspector has just connected.
    fn connected(threads: i32) -> Self {
        let mut harness = watch_harness(threads);
        harness.controller.main.unwind = Arc::new(FlatUnwindInfo);
        let model = scripted_runtime(
            &harness,
            ScriptedRuntime::inspected(ImageAddress::new(HOOK)),
        );
        let inferior = harness.controller.inferior.as_ref().expect("inferior");
        let runtime = harness.controller.runtimes(inferior)[0].id;
        let mut scripted = Self {
            harness,
            inspector: FakeInspector::start(),
            runtime,
            _model: model,
        };
        scripted.open();
        assert_eq!(scripted.pump(), Incoming::Connected);
        scripted
    }

    /// Connects to the fake as the controller does once it found the target.
    fn open(&mut self) {
        let address = self.inspector.address;
        let record = self.record();
        record.target = Some("0f2b5c1e-uuid".into());
        record.address = Some(address);
        record.problem = None;
        self.harness.controller.connect_inspector(self.runtime);
    }

    fn record(&mut self) -> &mut super::Inspector {
        let runtime = self.runtime;
        self.harness
            .controller
            .inferior
            .as_mut()
            .expect("inferior")
            .inspectors
            .records
            .entry(runtime)
            .or_default()
    }

    /// The next message the connection delivered to the controller.
    fn message(&self) -> InspectorMessage {
        let started = Instant::now();
        loop {
            match self.harness.controller.messages.borrow_mut().try_recv() {
                Ok(ControllerMessage::Inspector(message)) => return message,
                Ok(_) => panic!("a controller message that is not the inspector's"),
                Err(tokio::sync::mpsc::error::TryRecvError::Empty) => {
                    assert!(started.elapsed() < DEADLINE, "no inspector message came");
                    thread::sleep(Duration::from_millis(1));
                }
                Err(error) => panic!("the controller's queue failed: {error}"),
            }
        }
    }

    /// Hands the controller the next message, and returns it.
    fn pump(&mut self) -> Incoming {
        let message = self.message();
        let incoming = message.incoming.clone();
        assert!(
            self.harness
                .controller
                .handle_message(ControllerMessage::Inspector(message)),
            "the controller ends"
        );
        incoming
    }

    /// Hands the controller the next message, which is the event `method`.
    fn pump_event(&mut self, method: &str) {
        let incoming = self.pump();
        assert!(
            matches!(&incoming, Incoming::Event { method: sent, .. } if &**sent == method),
            "expected {method}, got {incoming:?}"
        );
    }

    /// Hands the controller the next message, which is a reply.
    fn pump_reply(&mut self) {
        let incoming = self.pump();
        assert!(
            matches!(incoming, Incoming::Reply { .. }),
            "expected a reply, got {incoming:?}"
        );
    }

    /// Every request the controller has sent since the last call: a marker
    /// sent after them arrives once they all have.
    fn sent(&mut self) -> Vec<Request> {
        let link = self.record().connection.as_ref().expect("a connection");
        link.connection
            .send(&Session::main(), "Test.marker", json!({}))
            .expect("the marker is sent");
        std::iter::from_fn(|| Some(self.inspector.request()))
            .take_while(|request| request.method != "Test.marker")
            .collect()
    }

    fn sent_names(&mut self) -> Vec<String> {
        self.sent().iter().map(Request::named).collect()
    }

    /// Adds a breakpoint on `line` of `path`, the runtime's.
    fn add_line(&mut self, path: &str, line: u64, options: crate::BreakpointOptions) -> Breakpoint {
        let spec = BreakpointSpec::Source {
            path: path.into(),
            line: crate::LineNumber::new(line).expect("a line"),
        };
        self.harness
            .edit(|reply| super::super::Edit::AddBreakpoint {
                spec,
                options: Box::new(options),
                reply,
            })
            .try_recv()
            .expect("a reply")
            .expect("added")
    }

    /// Adds a breakpoint on `line` of `path` and answers its arming, the
    /// sentinel's first if it is set, V8 placing it as `id`.
    fn arm_line(&mut self, path: &str, line: u64, id: &str) -> Breakpoint {
        self.arm_line_with(path, line, id, crate::BreakpointOptions::default())
    }

    fn arm_line_with(
        &mut self,
        path: &str,
        line: u64,
        id: &str,
        options: crate::BreakpointOptions,
    ) -> Breakpoint {
        let breakpoint = self.add_line(path, line, options);
        for request in self.sent() {
            if request.method != cdp::SET_BREAKPOINT {
                continue;
            }
            let placed = if request.params["lineNumber"] == 0 {
                json!({"breakpointId": format!("sentinel-{}", request.id), "locations": []})
            } else {
                let location = json!({"scriptId": "7", "lineNumber": line - 1, "columnNumber": 4});
                json!({"breakpointId": id, "locations": [location]})
            };
            self.inspector.reply(&request, placed);
            self.pump_reply();
        }
        breakpoint
    }

    /// The reason the first breakpoint's one location waits, if it does.
    fn pending(&self) -> Option<Arc<str>> {
        let snapshot = self.harness.controller.snapshot();
        snapshot.breakpoints[0]
            .pending_locations
            .first()
            .map(|pending| Arc::clone(&pending.reason))
    }

    /// Takes the native actions recorded since the last look.
    fn actions(&self) -> Vec<String> {
        self.harness.trace().take_actions()
    }

    fn main(&self) -> nix::unistd::Pid {
        self.harness.threads[0]
    }

    /// Resumes the published stop for a continue.
    fn resume(&mut self) {
        self.harness.resume().expect("continue");
    }

    /// The stops published since the last look.
    fn stops(&mut self) -> Vec<(crate::ThreadId, StopReason)> {
        self.harness
            .published()
            .into_iter()
            .filter_map(|event| match event {
                DebuggerEvent::InferiorStopped {
                    thread_id, reason, ..
                } => Some((thread_id, reason)),
                _ => None,
            })
            .collect()
    }
}

/// V8's report of a pause at `hits` in a stack of `depth` frames, which
/// lists no scopes.
fn paused(reason: &str, hits: &[&str], depth: u64) -> Value {
    let frames = (0..depth)
        .map(|index| {
            json!({
                "callFrameId": index.to_string(),
                "location": {"scriptId": "7", "lineNumber": 2 + index, "columnNumber": 4},
                "scopeChain": [],
            })
        })
        .collect::<Vec<_>>();
    json!({"reason": reason, "hitBreakpoints": hits, "callFrames": frames})
}

/// A connection readies the main isolate, arms it as the runtime is armed,
/// and lets the node run only once Node reports that it waits: a release
/// sent before Node's `waitForDebugger` begins is lost.
#[test]
fn connecting_readies_the_main_isolate_and_releases_it_once_node_waits() {
    let mut scripted = Scripted::connected(1);
    assert_eq!(
        scripted.sent_names(),
        [
            cdp::NOTIFY_END,
            cdp::ENABLE_DEBUGGER,
            cdp::ENABLE_WORKERS,
            "Runtime.enable",
            cdp::ENABLE_RUNTIME
        ]
    );

    let breakpoint = scripted.add_line("app.scripted", 3, crate::BreakpointOptions::default());
    let arming = scripted.sent();
    assert_eq!(
        arming.iter().map(Request::named).collect::<Vec<_>>(),
        [
            cdp::PAUSE_ON_EXCEPTIONS,
            cdp::SET_BREAKPOINT,
            cdp::SET_BREAKPOINT
        ],
        "{arming:?}"
    );
    assert_eq!(
        scripted.pending().as_deref(),
        Some("waiting for Node's inspector to set the breakpoint")
    );
    let set = arming
        .iter()
        .find(|request| request.params["lineNumber"] == 2)
        .expect("the breakpoint's request");
    let location = json!({"scriptId": "7", "lineNumber": 2, "columnNumber": 4});
    scripted
        .inspector
        .reply(set, json!({"breakpointId": "app", "locations": [location]}));
    scripted.pump_reply();
    assert_eq!(scripted.pending(), None, "{breakpoint:?}");

    scripted.inspector.event(&[], cdp::WAITING, json!({}));
    scripted.pump_event(cdp::WAITING);
    assert_eq!(scripted.sent_names(), [cdp::RELEASE]);
}

#[test]
fn removing_a_breakpoint_before_v8_answers_removes_its_late_placements() {
    let mut scripted = Scripted::connected(1);
    scripted.sent();
    let breakpoint = scripted.add_line("app.scripted", 3, crate::BreakpointOptions::default());
    let requests = scripted.sent();
    let placements = requests
        .iter()
        .filter(|request| request.method == cdp::SET_BREAKPOINT)
        .collect::<Vec<_>>();
    assert_eq!(
        placements.len(),
        2,
        "the sentinel and the user's breakpoint"
    );
    scripted
        .harness
        .edit(|reply| super::super::Edit::RemoveBreakpoint {
            id: breakpoint.id,
            reply,
        })
        .try_recv()
        .expect("a reply")
        .expect("removed");
    scripted.sent();
    for request in placements {
        let id = format!("late-{}", request.id);
        scripted
            .inspector
            .reply(request, json!({"breakpointId": id, "locations": []}));
        scripted.pump_reply();
        let removed = scripted.sent();
        assert!(
            removed.iter().any(|request| {
                request.method == cdp::REMOVE_BREAKPOINT && request.params["breakpointId"] == id
            }),
            "the withdrawn placement {id} remains in V8: {removed:?}"
        );
    }
}

/// The managed breakpoint `breakpoint`'s hit, counted `count` times.
fn managed_hit(breakpoint: &Breakpoint, count: u64) -> StopReason {
    StopReason::Breakpoint {
        address: None,
        hits: Arc::from([crate::BreakpointHit {
            breakpoint: breakpoint.id,
            hit_count: count,
        }]),
    }
}

/// The native breakpoint at [`TRAP`] that the second thread hits.
const TRAP: u64 = 0x40;

/// A breakpoint V8 pauses at, and a native one that another thread hits
/// while the same all-stop forms, in the order `v8_first` names: one stop
/// is published, the native hit's, as the first of equal priority. V8's
/// stays held through that stop's resume and is published at its own.
fn pause_beside_a_native_trap(v8_first: bool) {
    let mut scripted = Scripted::connected(2);
    scripted.sent();
    let managed = scripted.arm_line("app.scripted", 3, "app");
    let native = scripted.harness.add_breakpoint(TRAP);
    let (main, other) = (scripted.main(), scripted.harness.threads[1]);
    scripted.harness.start_continue();
    scripted.actions();

    let pause = |scripted: &mut Scripted| {
        scripted
            .inspector
            .event(&[], cdp::PAUSED, paused("other", &["app"], 1));
        scripted.pump_event(cdp::PAUSED);
    };
    if v8_first {
        pause(&mut scripted);
        let actions = scripted.actions();
        assert!(
            actions.contains(&format!("request_stop {other}")),
            "{actions:?}"
        );
        scripted
            .harness
            .hit_at(other, TRAP)
            .expect("the native trap");
    } else {
        scripted
            .harness
            .hit_at(other, TRAP)
            .expect("the native trap");
        let actions = scripted.actions();
        assert!(
            actions.contains(&format!("request_stop {main}")),
            "{actions:?}"
        );
        pause(&mut scripted);
    }
    scripted.harness.settle_requested_stops();
    let native_hit = StopReason::Breakpoint {
        address: Some(crate::VirtualAddress::new(TRAP)),
        hits: Arc::from([crate::BreakpointHit {
            breakpoint: native.id,
            hit_count: 1,
        }]),
    };
    assert_eq!(
        scripted.stops(),
        [(super::super::debug_thread_id(other), native_hit)]
    );
    assert_eq!(scripted.sent_names(), Vec::<String>::new());

    // The native hit's thread steps over its trap, then every thread runs.
    scripted.resume();
    scripted
        .harness
        .finish_step(other)
        .expect("the repair step");
    let actions = scripted.actions();
    assert!(
        actions.contains(&format!("request_stop {main}")),
        "{actions:?}"
    );
    assert_eq!(
        scripted.sent_names(),
        Vec::<String>::new(),
        "the unpublished pause stays held"
    );
    scripted.harness.settle_requested_stops();
    assert_eq!(
        scripted.stops(),
        [(
            super::super::debug_thread_id(main),
            managed_hit(&managed, 1)
        )],
        "the held pause stops on its own, its hit counted once"
    );

    scripted.resume();
    assert_eq!(scripted.sent_names(), [cdp::RESUME]);
    assert_eq!(scripted.stops(), []);
}

#[test]
fn a_v8_pause_then_a_native_trap_in_one_all_stop_publish_one_each() {
    pause_beside_a_native_trap(true);
}

#[test]
fn a_native_trap_then_a_v8_pause_in_one_all_stop_publish_one_each() {
    pause_beside_a_native_trap(false);
}

/// A client's pause asked while V8's pause at a breakpoint is still being
/// listed is answered by that breakpoint's stop: its deadline ends with
/// the stop rather than stopping every thread after the next continue.
#[test]
fn an_interrupt_answered_by_a_breakpoint_stop_ends_with_it() {
    let mut scripted = Scripted::connected(2);
    scripted.sent();
    let managed = scripted.arm_line("app.scripted", 3, "app");
    scripted.harness.start_continue();
    scripted.actions();

    let mut pause = paused("other", &["app"], 1);
    pause["callFrames"][0]["scopeChain"] =
        json!([{"type": "local", "object": {"objectId": "scope"}}]);
    scripted.inspector.event(&[], cdp::PAUSED, pause);
    scripted.pump_event(cdp::PAUSED);
    let listing = scripted.sent();
    let process = super::super::process_id(scripted.main());
    scripted
        .harness
        .controller
        .begin_pause(process)
        .expect("the pause begins");
    assert_eq!(scripted.sent_names(), [cdp::PAUSE]);

    scripted.inspector.reply(&listing[0], json!({"result": []}));
    scripted.pump_reply();
    scripted.harness.settle_requested_stops();
    assert_eq!(
        scripted.stops(),
        [(
            super::super::debug_thread_id(scripted.main()),
            managed_hit(&managed, 1)
        )]
    );

    scripted.resume();
    scripted.actions();
    scripted
        .harness
        .controller
        .interrupt_expired(Instant::now() + Duration::from_secs(1))
        .expect("the deadline is looked at");
    assert_eq!(
        scripted.actions(),
        Vec::<String>::new(),
        "the interrupt ended with the stop that answered it"
    );
}

/// A pause at a breakpoint id no location of microscope's has is no
/// `debugger` statement: it stops nothing, and V8 is told to resume.
#[test]
fn a_pause_at_an_unknown_breakpoint_resumes_unseen() {
    let mut scripted = Scripted::connected(2);
    scripted.sent();
    scripted.arm_line("app.scripted", 3, "app");
    scripted.harness.start_continue();
    scripted.actions();

    scripted
        .inspector
        .event(&[], cdp::PAUSED, paused("other", &["not-microscopes"], 1));
    scripted.pump_event(cdp::PAUSED);
    let actions = scripted.actions();
    assert!(
        actions
            .iter()
            .any(|action| action.starts_with("request_stop")),
        "the pause is judged at an internal stop: {actions:?}"
    );
    scripted.harness.settle_requested_stops();
    assert_eq!(scripted.stops(), []);
    assert_eq!(scripted.sent_names(), [cdp::RESUME]);
    let actions = scripted.actions();
    for pid in scripted.harness.threads.clone() {
        assert!(
            actions.contains(&format!("continue {pid} None")),
            "{actions:?}"
        );
    }
}

/// Begins a client's pause while every thread runs, which asks V8 to pause
/// the main isolate, then lets its deadline pass with no pause from V8:
/// every thread stops natively for the pause.
fn interrupt_unanswered(scripted: &mut Scripted) {
    scripted.harness.start_continue();
    scripted.actions();
    let before = Instant::now();
    let process = super::super::process_id(scripted.main());
    scripted
        .harness
        .controller
        .begin_pause(process)
        .expect("the pause begins");
    assert_eq!(scripted.sent_names(), [cdp::PAUSE]);
    assert_eq!(scripted.actions(), Vec::<String>::new());

    scripted
        .harness
        .controller
        .interrupt_expired(before)
        .expect("too early to expire");
    assert_eq!(scripted.actions(), Vec::<String>::new(), "V8 has time yet");
    scripted
        .harness
        .controller
        .interrupt_expired(Instant::now() + Duration::from_secs(1))
        .expect("the interrupt expires");
    let actions = scripted.actions();
    for pid in scripted.harness.threads.clone() {
        assert!(
            actions.contains(&format!("request_stop {pid}")),
            "{actions:?}"
        );
    }
    scripted.harness.settle_requested_stops();
    assert_eq!(
        scripted.stops(),
        [(
            super::super::debug_thread_id(scripted.main()),
            StopReason::Pause
        )]
    );
    // The stop's module refresh plants the inspector's hook.
    scripted.actions();
}

/// An interrupt V8 never answers, as in an idle event loop, stops every
/// thread natively. The pause V8 takes after the resume answers nothing:
/// it is judged, absorbed, and resumed.
#[test]
fn an_unanswered_interrupt_stops_natively_and_absorbs_its_late_pause() {
    let mut scripted = Scripted::connected(2);
    scripted.sent();
    interrupt_unanswered(&mut scripted);

    scripted.resume();
    assert_eq!(scripted.sent_names(), Vec::<String>::new());
    scripted.actions();
    scripted
        .inspector
        .event(&[], cdp::PAUSED, paused("other", &[], 1));
    scripted.pump_event(cdp::PAUSED);
    scripted.harness.settle_requested_stops();
    assert_eq!(scripted.stops(), [], "the late pause is absorbed");
    assert_eq!(scripted.sent_names(), [cdp::RESUME]);
}

/// The interrupt's pause V8 reports while the native stop it gave up on
/// is published is held there, and judged once that stop resumes.
#[test]
fn an_interrupt_pause_reported_at_the_native_stop_is_judged_at_the_resume() {
    let mut scripted = Scripted::connected(2);
    scripted.sent();
    interrupt_unanswered(&mut scripted);

    scripted
        .inspector
        .event(&[], cdp::PAUSED, paused("other", &[], 1));
    scripted.pump_event(cdp::PAUSED);
    assert_eq!(scripted.actions(), Vec::<String>::new());
    assert_eq!(scripted.sent_names(), Vec::<String>::new());

    scripted.resume();
    let actions = scripted.actions();
    assert!(
        actions
            .iter()
            .any(|action| action.starts_with("request_stop")),
        "the held pause is judged at an internal stop: {actions:?}"
    );
    assert_eq!(scripted.sent_names(), Vec::<String>::new());
    scripted.harness.settle_requested_stops();
    assert_eq!(scripted.stops(), []);
    assert_eq!(scripted.sent_names(), [cdp::RESUME]);
}

/// A pause whose scope listing V8 answers only after another thread's stop
/// was published, and that stop resumed, is judged then: its breakpoint
/// stops at a stop of its own.
#[test]
fn a_pause_listed_after_another_stop_resumed_stops_at_its_own() {
    let mut scripted = Scripted::connected(2);
    scripted.sent();
    let managed = scripted.arm_line("app.scripted", 3, "app");
    scripted.harness.add_breakpoint(TRAP);
    let (main, other) = (scripted.main(), scripted.harness.threads[1]);
    scripted.harness.start_continue();

    let mut pause = paused("other", &["app"], 1);
    pause["callFrames"][0]["scopeChain"] =
        json!([{"type": "local", "object": {"objectId": "scope"}}]);
    scripted.inspector.event(&[], cdp::PAUSED, pause);
    scripted.pump_event(cdp::PAUSED);
    let listing = scripted.sent();
    assert_eq!(
        listing.iter().map(Request::named).collect::<Vec<_>>(),
        [cdp::GET_PROPERTIES]
    );
    scripted.actions();

    scripted
        .harness
        .hit_at(other, TRAP)
        .expect("the native trap");
    scripted.harness.settle_requested_stops();
    let stops = scripted.stops();
    assert_eq!(stops.len(), 1, "{stops:?}");
    assert_eq!(stops[0].0, super::super::debug_thread_id(other));

    scripted.resume();
    scripted
        .harness
        .finish_step(other)
        .expect("the repair step");
    assert_eq!(
        scripted.sent_names(),
        Vec::<String>::new(),
        "a pause still listing is not let go"
    );
    scripted.actions();

    let local = json!({"name": "late", "value": {"type": "number", "value": 7}});
    scripted
        .inspector
        .reply(&listing[0], json!({"result": [local]}));
    scripted.pump_reply();
    let actions = scripted.actions();
    assert!(
        actions.contains(&format!("request_stop {main}")),
        "{actions:?}"
    );
    scripted.harness.settle_requested_stops();
    assert_eq!(
        scripted.stops(),
        [(
            super::super::debug_thread_id(main),
            managed_hit(&managed, 1)
        )]
    );
    assert!(
        scripted.record().isolates[&Session::main()]
            .paused
            .is_some(),
        "V8 holds the pause published"
    );
}

/// A connection that closes with a breakpoint's request outstanding leaves
/// the breakpoint pending, saying why. A message the ended connection sent
/// is ignored once a new one is made.
#[test]
fn a_disconnect_mid_request_leaves_the_breakpoint_pending() {
    let mut scripted = Scripted::connected(1);
    scripted.sent();
    scripted.add_line("app.scripted", 3, crate::BreakpointOptions::default());
    let outstanding = scripted.sent();
    assert!(
        outstanding
            .iter()
            .any(|request| request.method == cdp::SET_BREAKPOINT
                && request.params["lineNumber"] == 2),
        "{outstanding:?}"
    );
    let ended = scripted
        .record()
        .connection
        .as_ref()
        .expect("a connection")
        .generation;

    scripted.inspector.close();
    let mut replies = 0;
    loop {
        match scripted.pump() {
            Incoming::Reply { result, .. } => {
                assert!(
                    matches!(result, Err(super::super::cdp::CdpError::Closed(_))),
                    "{result:?}"
                );
                replies += 1;
            }
            Incoming::Closed { .. } => break,
            other => panic!("{other:?} as the connection closes"),
        }
    }
    assert!(replies >= outstanding.len(), "every request is answered");
    assert_eq!(
        scripted.pending().as_deref(),
        Some("the inspector connection closed: the inspector closed the connection")
    );
    scripted.inspector.wait_gone();

    scripted.inspector = FakeInspector::start();
    scripted.open();
    assert_eq!(scripted.pump(), Incoming::Connected);
    scripted.sent();
    scripted.harness.start_continue();
    scripted.actions();
    let stale = |incoming| {
        ControllerMessage::Inspector(InspectorMessage {
            generation: ended,
            runtime: scripted.runtime,
            incoming,
        })
    };
    let pause = stale(Incoming::Event {
        session: Session::main(),
        method: cdp::PAUSED.into(),
        params: paused("other", &[], 1),
    });
    let closed = stale(Incoming::Closed {
        reason: "an old close".into(),
    });
    for message in [pause, closed] {
        assert!(scripted.harness.controller.handle_message(message));
    }
    assert_eq!(scripted.actions(), Vec::<String>::new());
    let record = scripted.record();
    assert!(record.connection.is_some(), "the new connection stays");
    assert!(record.isolates[&Session::main()].paused.is_none());
}

/// A worker that starts while the process is stopped is readied, armed,
/// and released, and its pause at its first statement is let go, with no
/// stop of its own.
#[test]
fn a_worker_attached_while_stopped_is_armed_and_released_unseen() {
    let mut scripted = Scripted::connected(2);
    scripted.sent();
    scripted.arm_line("app.scripted", 3, "app");
    let stop = scripted.harness.stop_id();
    scripted.harness.published();
    scripted.actions();

    attach_worker(&mut scripted, "w1");
    assert_eq!(scripted.harness.stop_id(), stop);
    assert_eq!(scripted.stops(), []);
    assert_eq!(scripted.actions(), Vec::<String>::new());
}

/// Attaches the worker at session `id`, answering its arming, and lets its
/// pause at its first statement go.
fn attach_worker(scripted: &mut Scripted, id: &str) {
    let params = json!({"sessionId": id, "waitingForDebugger": true, "workerInfo": {}});
    scripted.inspector.event(&[], cdp::WORKER_ATTACHED, params);
    scripted.pump_event(cdp::WORKER_ATTACHED);
    let readied = scripted.sent();
    let named = |method: &str| format!("{id} {method}");
    assert_eq!(
        readied.iter().map(Request::named).collect::<Vec<_>>(),
        [
            named(cdp::ENABLE_DEBUGGER),
            named(cdp::ENABLE_WORKERS),
            named(cdp::PAUSE_ON_EXCEPTIONS),
            named(cdp::SET_BREAKPOINT),
            named(cdp::SET_BREAKPOINT),
            named(cdp::RELEASE),
        ]
    );
    for request in readied
        .iter()
        .filter(|request| request.method == cdp::SET_BREAKPOINT)
    {
        let placed = if request.params["lineNumber"] == 0 {
            json!({"breakpointId": format!("{id}-sentinel"), "locations": []})
        } else {
            let location = json!({"scriptId": "7", "lineNumber": 2, "columnNumber": 4});
            json!({"breakpointId": format!("{id}-app"), "locations": [location]})
        };
        scripted.inspector.reply(request, placed);
        scripted.pump_reply();
    }

    let start = paused(cdp::BREAK_ON_START, &[], 1);
    scripted.inspector.event(&[id], cdp::PAUSED, start);
    scripted.pump_event(cdp::PAUSED);
    assert_eq!(scripted.sent_names(), [named(cdp::RESUME)]);
}

/// The process exiting while V8 has not answered its arming ends the
/// connection: every request outstanding is answered, and the replies,
/// for a process that is gone, change nothing.
#[test]
fn a_process_exiting_while_armed_closes_the_connection() {
    let mut scripted = Scripted::connected(2);
    let mut requests = scripted.sent().len() + 1;
    scripted.add_line("app.scripted", 3, crate::BreakpointOptions::default());
    requests += scripted.sent().len() + 1;
    scripted.harness.start_continue();

    for pid in scripted.harness.threads.clone().into_iter().rev() {
        scripted
            .harness
            .controller
            .process_wait(super::super::WaitEvent::Exited(pid, 0))
            .expect("exit");
    }
    assert!(scripted.harness.controller.inferior.is_none());
    scripted.inspector.wait_gone();

    let mut replies = 0;
    loop {
        match scripted.pump() {
            Incoming::Reply { result, .. } => {
                assert!(
                    matches!(result, Err(super::super::cdp::CdpError::Closed(_))),
                    "{result:?}"
                );
                replies += 1;
            }
            Incoming::Closed { reason } => {
                assert_eq!(&*reason, "closed by microscope");
                break;
            }
            other => panic!("{other:?} as the connection closes"),
        }
    }
    assert_eq!(replies, requests, "every request, markers too, is answered");
}

/// The main isolate and a worker pause at once. The main isolate's pause
/// runs on the main thread and stops; no thread is found running the
/// worker's, whose script frames the fake cannot unwind, so its failure
/// is reported separately without attributing its breakpoint to a thread.
#[test]
fn an_unidentified_worker_pause_does_not_replace_a_concurrent_breakpoint() {
    let mut scripted = Scripted::connected(2);
    scripted.sent();
    let managed = scripted.arm_line("app.scripted", 3, "app");
    attach_worker(&mut scripted, "w1");
    scripted.harness.start_continue();
    scripted.actions();

    scripted
        .inspector
        .event(&[], cdp::PAUSED, paused("other", &["app"], 1));
    scripted
        .inspector
        .event(&["w1"], cdp::PAUSED, paused("other", &["w1-app"], 1));
    scripted.pump_event(cdp::PAUSED);
    scripted.pump_event(cdp::PAUSED);
    scripted.harness.settle_requested_stops();
    assert_eq!(
        scripted.stops(),
        [(
            super::super::debug_thread_id(scripted.main()),
            managed_hit(&managed, 1)
        )]
    );
    assert_eq!(scripted.sent_names(), Vec::<String>::new());

    scripted.resume();
    assert_eq!(scripted.sent_names(), [cdp::RESUME]);
    scripted.harness.settle_requested_stops();
    let stops = scripted.stops();
    assert!(
        matches!(stops.as_slice(), [(_, StopReason::RuntimeEvent { description })] if description.contains("no native thread")),
        "{stops:?}"
    );
    scripted.resume();
    assert_eq!(scripted.sent_names(), [format!("w1 {}", cdp::RESUME)]);
    assert_eq!(scripted.stops(), []);
}

/// A step over from a V8 pause runs as V8's `stepOver`. A pause deeper in,
/// at a breakpoint whose hit condition fails, does not end it: it goes on
/// out of that call. The next pause no deeper than the step began ends it.
#[test]
fn a_v8_step_over_ends_only_at_a_pause_no_deeper_than_it_began() {
    let mut scripted = Scripted::connected(1);
    scripted.sent();
    let managed = scripted.arm_line("app.scripted", 3, "app");
    let options = crate::BreakpointOptions {
        hit_condition: Some("==5".parse().expect("a hit condition")),
        ..crate::BreakpointOptions::default()
    };
    scripted.arm_line_with("deep.scripted", 9, "deep", options);
    let main = scripted.main();
    scripted.harness.start_continue();
    scripted
        .inspector
        .event(&[], cdp::PAUSED, paused("other", &["app"], 2));
    scripted.pump_event(cdp::PAUSED);
    scripted.harness.settle_requested_stops();
    assert_eq!(
        scripted.stops(),
        [(
            super::super::debug_thread_id(main),
            managed_hit(&managed, 1)
        )]
    );

    let process = super::super::process_id(main);
    let stop = scripted.harness.stop_id();
    scripted
        .harness
        .controller
        .begin_runtime_step(
            process,
            stop,
            main,
            (scripted.runtime, Some(crate::VirtualAddress::new(0x9000))),
            crate::StepKind::OverSource,
            crate::protocol::ResumeScope::Process(process),
            crate::protocol::ExceptionDisposition::Pass,
        )
        .expect("the step begins");
    assert_eq!(scripted.sent_names(), [cdp::STEP_OVER]);

    scripted
        .inspector
        .event(&[], cdp::PAUSED, paused("other", &["deep"], 3));
    scripted.pump_event(cdp::PAUSED);
    scripted.harness.settle_requested_stops();
    assert_eq!(
        scripted.stops(),
        [],
        "the deeper pause does not end the step"
    );
    assert_eq!(scripted.sent_names(), [cdp::STEP_OUT]);

    scripted
        .inspector
        .event(&[], cdp::PAUSED, paused("other", &[], 2));
    scripted.pump_event(cdp::PAUSED);
    scripted.harness.settle_requested_stops();
    assert_eq!(
        scripted.stops(),
        [(
            super::super::debug_thread_id(main),
            StopReason::Step {
                kind: crate::StepKind::OverSource
            }
        )]
    );
}

/// A thread exits while an attached session's shutdown releases a borrowed
/// inspector, so its exit status is the last the cleanup's all-stop
/// awaits. That status completes the all-stop, and the detach follows.
#[test]
fn a_thread_exiting_in_the_cleanup_stop_of_a_shutdown_lets_it_detach() {
    use super::super::{Signal, WaitEvent};

    let mut scripted = Scripted::connected(2);
    scripted.sent();
    scripted.record().ownership = super::super::node_launch::Ownership::Borrowed;
    let [main, worker] = scripted.harness.threads[..] else {
        unreachable!("two threads");
    };
    scripted
        .harness
        .controller
        .inferior
        .as_mut()
        .expect("inferior")
        .origin = super::super::InferiorOrigin::Attached;

    let (reply, mut result) = tokio::sync::oneshot::channel();
    scripted.harness.controller.begin_shutdown(Some(reply));
    let release = scripted
        .sent()
        .into_iter()
        .find(|request| request.method == "Runtime.getIsolateId")
        .expect("the cleanup asks the borrowed inspector to acknowledge");

    // The worker begins exiting while the cleanup runs every thread.
    scripted
        .harness
        .controller
        .handle_wait(WaitEvent::PtraceEvent(
            worker,
            Signal::SIGTRAP,
            libc::PTRACE_EVENT_EXIT,
        ));
    scripted.inspector.reply(&release, json!({"id": "isolate"}));
    scripted.pump_reply();
    scripted
        .harness
        .controller
        .handle_wait(WaitEvent::PtraceEvent(
            main,
            Signal::SIGTRAP,
            libc::PTRACE_EVENT_STOP,
        ));
    assert!(
        result.try_recv().is_err(),
        "the detach awaits the exiting worker"
    );

    scripted
        .harness
        .controller
        .handle_wait(WaitEvent::Exited(worker, 0));
    assert!(
        matches!(result.try_recv(), Ok(Ok(()))),
        "the worker's exit ends the cleanup's stop and the detach: {:?}",
        scripted.actions()
    );
}

/// A step from a native stop asks V8 to pause, which the main thread takes
/// only once the inspector's I/O thread has handed the request on. Until
/// the I/O thread returns from taking it, the main thread stays stopped,
/// since running it sooner lets it run on past the next statement.
#[test]
fn a_step_from_a_native_stop_holds_the_main_thread_until_its_pause_is_handed_on() {
    let mut scripted = Scripted::connected(2);
    scripted.sent();
    let [main, io] = scripted.harness.threads[..] else {
        unreachable!("two threads");
    };
    let entry = {
        let controller = &scripted.harness.controller;
        let inferior = controller.inferior.as_ref().expect("inferior");
        controller.runtimes(inferior)[0]
            .module
            .virtual_address(crate::ImageAddress::new(0x60))
            .expect("the entry")
            .get()
    };
    scripted.actions();

    let process = super::super::process_id(main);
    let stop = scripted.harness.stop_id();
    scripted
        .harness
        .controller
        .begin_runtime_step(
            process,
            stop,
            main,
            (scripted.runtime, None),
            crate::StepKind::OverSource,
            crate::protocol::ResumeScope::Process(process),
            crate::protocol::ExceptionDisposition::Pass,
        )
        .expect("the step begins");
    assert!(scripted.sent_names().contains(&cdp::PAUSE.to_owned()));
    let actions = scripted.actions();
    assert!(
        actions.contains(&format!("continue {io} None")),
        "{actions:?}"
    );
    assert!(
        !actions.contains(&format!("continue {main} None")),
        "the main thread waits for the pause to be handed on: {actions:?}"
    );

    // The I/O thread takes the request, to return to 0x8000.
    let trace = scripted.harness.trace();
    trace.set_pc(io, entry + 1);
    trace.stack_pointers.borrow_mut().insert(io, 0x9000);
    scripted.harness.store(0x9000, 0x8000);
    scripted
        .harness
        .trap(io, libc::SI_KERNEL, 0)
        .expect("the entry trap");
    let mut actions = scripted.actions();
    if actions.contains(&format!("step {io}")) {
        scripted.harness.finish_step(io).expect("the repair step");
        actions.extend(scripted.actions());
    }
    assert!(
        !actions.contains(&format!("continue {main} None")),
        "the main thread waits while the request is handed on: {actions:?}"
    );

    // It returns, the request handed on.
    let trace = scripted.harness.trace();
    trace.set_pc(io, 0x8001);
    trace.stack_pointers.borrow_mut().insert(io, 0x9008);
    scripted
        .harness
        .trap(io, libc::SI_KERNEL, 0)
        .expect("the return trap");
    scripted.harness.settle_requested_stops();
    for _ in 0..4 {
        let actions = scripted.actions();
        if actions.contains(&format!("continue {main} None")) {
            return;
        }
        if let Some(step) = actions.iter().find(|action| action.starts_with("step ")) {
            let pid = nix::unistd::Pid::from_raw(step[5..].parse().expect("a pid"));
            scripted.harness.finish_step(pid).expect("the repair step");
            continue;
        }
        panic!("the main thread never runs: {actions:?}");
    }
    panic!("the main thread never runs");
}
