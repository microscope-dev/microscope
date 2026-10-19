//! The harness for Debug Adapter Protocol scenarios.
//!
//! A [`Dap`] drives the real `microscope dap` binary over its stdin and stdout,
//! as editors do. It records every message in a transcript that it prints
//! on failure, bounds every wait with a deadline, and checks every message
//! the adapter sends against the protocol's JSON schema and against the
//! ordering rules strict clients depend on. Finishing a scenario
//! disconnects, checks that the adapter exited cleanly, and that the
//! program it launched is gone.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::io::{BufRead as _, BufReader, Read as _, Write as _};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::support::flight_recordings;

// A response may wait for the program to load, as `setBreakpoints` sent
// beside `launch` does, which takes seconds for the largest fixtures under
// `just stress`, so responses are bounded as events are.
const REQUEST_TIMEOUT: Duration = Duration::from_mins(1);
const EVENT_TIMEOUT: Duration = Duration::from_mins(1);

/// A position in the stream of received messages; waits look only after it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Mark(usize);

impl Mark {
    /// The start of the session.
    pub const START: Self = Self(0);
}

/// A request that was sent.
#[derive(Debug, Clone, Copy)]
pub struct Sent {
    pub seq: u64,

    /// The position before the request was sent.
    pub mark: Mark,
}

/// The order and arguments in which a client starts a session, taken from
/// each client's source.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Profile {
    /// Sends `launch` without waiting, configures in parallel after
    /// `initialized`, then `configurationDone` and `threads` at once.
    VsCode,

    /// Sends `launch` from the `initialize` callback and configures one
    /// request at a time.
    Neovim,

    /// Waits for `initialize`, then launches and configures sequentially.
    Helix,

    /// Configures first and sends `launch` only after `configurationDone`,
    /// as dape does with `defer-launch-attach`.
    DeferredLaunch,
}

impl Profile {
    pub const ALL: [Self; 4] = [
        Self::VsCode,
        Self::Neovim,
        Self::Helix,
        Self::DeferredLaunch,
    ];

    fn initialize_arguments(self) -> Value {
        let id = match self {
            Self::VsCode => "vscode",
            Self::Neovim => "neovim",
            Self::Helix => "hx",
            Self::DeferredLaunch => "dape",
        };

        json!({
            "clientID": id,
            "clientName": id,
            "adapterID": "microscope",
            "pathFormat": "path",
            "linesStartAt1": true,
            "columnsStartAt1": true,
            "supportsVariableType": true,
            "supportsVariablePaging": self == Self::VsCode,
            "supportsRunInTerminalRequest": self != Self::Helix,
            "supportsStartDebuggingRequest": matches!(self, Self::VsCode | Self::Neovim),
            "supportsMemoryReferences": self == Self::VsCode,
            "supportsProgressReporting": self == Self::VsCode,
            "supportsInvalidatedEvent": self == Self::VsCode,
            "supportsMemoryEvent": self == Self::VsCode,
            "supportsANSIStyling": false,
            "locale": "en",
        })
    }
}

/// Breakpoints a client sets while configuring a session.
#[derive(Debug, Clone, Default)]
pub struct Configuration {
    /// Source files and the breakpoint lines in each.
    pub sources: Vec<(PathBuf, Vec<u64>)>,
    pub functions: Vec<String>,

    /// The `setExceptionBreakpoints` arguments; `None` enables the
    /// advertised default filters.
    pub exceptions: Option<Value>,
}

/// The responses to a session's start.
#[derive(Debug)]
pub struct Started {
    pub capabilities: Value,

    /// Each source's breakpoints, in configuration order.
    pub source_breakpoints: Vec<Vec<Value>>,
    pub function_breakpoints: Vec<Value>,

    /// The position before the first request, to wait for events after.
    pub mark: Mark,
}

/// A `stopped` event.
#[derive(Debug, Clone)]
pub struct Stopped {
    pub thread: i64,
    pub reason: String,
    pub body: Value,
}

struct Received {
    message: Value,
    consumed: bool,
}

/// A session with the adapter.
pub struct Dap {
    name: String,
    child: Option<Child>,

    /// Where requests are written: the adapter's stdin or a socket.
    stdin: Option<Box<dyn std::io::Write + Send>>,
    inbox: Receiver<Result<Value, String>>,
    transcript: Arc<Mutex<Vec<String>>>,
    started: Instant,
    seq: u64,
    received: Vec<Received>,

    /// Requests awaiting their response, by `seq`.
    outstanding: BTreeMap<u64, String>,
    checks: Checks,

    /// The process the adapter reported, and whether it was attached.
    process: Option<(u32, bool)>,
    finished: bool,

    /// Whether a failure already printed the transcript.
    reported: std::cell::Cell<bool>,

    /// The programs run for `runInTerminal`, which the harness reaps.
    terminals: Vec<Child>,

    /// What those programs write, a line at a time.
    terminal_output: (mpsc::Sender<String>, Receiver<String>),

    /// The error `runInTerminal` is answered with instead of running.
    refused_terminal: Option<String>,

    /// Whether this is another adapter, compared with but not checked.
    reference: bool,

    /// `runInTerminal` requests held unanswered, when holding them.
    held_terminals: Option<Vec<Value>>,

    /// Where the adapter streams its flight recording.
    recording: Option<PathBuf>,

    /// How `startDebugging` is answered.
    children: Children,
}

/// How a session answers `startDebugging`.
enum Children {
    /// It is not expected: the scenario fails.
    Unexpected,

    /// With an error, as a client that cannot start the session does.
    Refused(String),

    /// With success, starting no session, as nvim-dap answers for a
    /// session it started that never attaches.
    Abandoned,

    /// By starting each child session with a program that does not exist,
    /// so that its attach fails, and answering with that failure, as VS
    /// Code does. The failures are kept, in order.
    Unattachable(Vec<String>),

    /// By starting each child session as VS Code does, configured so, and
    /// answering once it has attached.
    Followed {
        configuration: Configuration,
        started: Vec<Dap>,
    },
}

/// Ordering rules checked on every message.
#[derive(Default)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "each tracks one protocol state"
)]
struct Checks {
    next_seq: u64,
    initialized_response: bool,
    initialized_event: bool,
    stopped: bool,
    threads: BTreeSet<i64>,

    /// The modules announced and not removed, by id.
    modules: BTreeSet<String>,

    /// Chaos clients race requests with stops on purpose.
    relaxed: bool,
}

impl Dap {
    /// Starts `microscope dap` with no session yet.
    pub fn start(name: impl Into<String>) -> Self {
        Self::start_in(name, &[])
    }

    /// Starts `microscope dap` with extra environment variables.
    pub fn start_in(name: impl Into<String>, environment: &[(&str, &str)]) -> Self {
        let mut command = Command::new(env!("CARGO_BIN_EXE_microscope"));
        command.arg("dap").envs(environment.iter().copied());
        let recording = flight_recordings::watch_adapter(&mut command);
        let mut dap = Self::spawn(name, &mut command);
        dap.recording = recording;
        dap
    }

    /// Starts another adapter to compare with, such as gdb's, whose
    /// messages are not held to this adapter's checks.
    pub fn reference(name: impl Into<String>, command: &mut Command) -> Self {
        let mut dap = Self::spawn(name, command);
        dap.reference = true;
        dap
    }

    fn spawn(name: impl Into<String>, command: &mut Command) -> Self {
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap_or_else(|error| panic!("start {command:?}: {error}"));

        let stdout = child.stdout.take().expect("adapter stdout");
        let stderr = child.stderr.take().expect("adapter stderr");
        let stdin = child.stdin.take().expect("adapter stdin");

        let dap = Self::over(name, Box::new(stdin), stdout, Some(child));
        let errors = Arc::clone(&dap.transcript);
        let started = dap.started;

        std::thread::spawn(move || {
            for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                errors.lock().expect("transcript").push(format!(
                    "[{:8.3}] !! adapter stderr: {line}",
                    started.elapsed().as_secs_f64()
                ));
            }
        });

        dap
    }

    /// Connects to an adapter serving TCP; its process is the caller's.
    pub fn connect(name: impl Into<String>, address: SocketAddr) -> Self {
        let stream = TcpStream::connect(address).expect("connect to the adapter");
        let output = stream.try_clone().expect("clone the socket");
        Self::over(name, Box::new(stream), output, None)
    }

    fn over(
        name: impl Into<String>,
        input: Box<dyn std::io::Write + Send>,
        output: impl std::io::Read + Send + 'static,
        child: Option<Child>,
    ) -> Self {
        let transcript = Arc::new(Mutex::new(Vec::new()));
        let started = Instant::now();
        let (send, inbox) = mpsc::channel();

        std::thread::spawn(move || read_frames(output, &send));

        Self {
            name: name.into(),
            stdin: Some(input),
            child,
            inbox,
            transcript,
            started,
            seq: 0,
            received: Vec::new(),
            outstanding: BTreeMap::new(),
            checks: Checks {
                next_seq: 1,
                ..Checks::default()
            },
            process: None,
            finished: false,
            reported: std::cell::Cell::new(false),
            terminals: Vec::new(),
            terminal_output: mpsc::channel(),
            refused_terminal: None,
            reference: false,
            held_terminals: None,
            recording: None,
            children: Children::Unexpected,
        }
    }

    /// Answers `startDebugging` as VS Code does: starts another adapter,
    /// initializes it, attaches it with the requested configuration, sets
    /// `configuration`'s breakpoints, and answers once it has attached.
    pub fn follow_children(&mut self, configuration: Configuration) {
        self.children = Children::Followed {
            configuration,
            started: Vec::new(),
        };
    }

    /// Answers `startDebugging` with an error.
    pub fn refuse_children(&mut self, message: &str) {
        self.children = Children::Refused(message.to_owned());
    }

    /// Answers `startDebugging` with success, but starts no session.
    pub fn abandon_children(&mut self) {
        self.children = Children::Abandoned;
    }

    /// Starts child sessions whose attach fails, and answers
    /// `startDebugging` with the failure.
    pub fn fail_children(&mut self) {
        self.children = Children::Unattachable(Vec::new());
    }

    /// Waits for the next child session to fail, and returns its failure.
    pub fn child_failure(&mut self) -> String {
        let deadline = Instant::now() + EVENT_TIMEOUT;
        loop {
            if let Children::Unattachable(failures) = &mut self.children
                && !failures.is_empty()
            {
                return failures.remove(0);
            }

            self.receive(deadline, "a startDebugging request");
        }
    }

    /// Waits for the next child session started, and returns it with the
    /// mark its events follow.
    pub fn child(&mut self) -> (Self, Mark) {
        let deadline = Instant::now() + EVENT_TIMEOUT;
        loop {
            if let Children::Followed { started, .. } = &mut self.children
                && !started.is_empty()
            {
                let child = started.remove(0);
                return (child, Mark::START);
            }

            self.receive(deadline, "a startDebugging request");
        }
    }

    /// Starts a child session for a `startDebugging` request, and answers
    /// it.
    fn start_debugging(&mut self, request: &Value) {
        let arguments = &request["arguments"];
        let result = match &mut self.children {
            Children::Unexpected => None,
            Children::Refused(message) => Some(Err(message.clone())),
            Children::Abandoned => Some(Ok(())),
            Children::Unattachable(failures) => {
                let name = arguments["configuration"]["name"]
                    .as_str()
                    .expect("child name");
                let mut child = Self::start(format!("{} › {name}", self.name));
                let mut start = arguments["configuration"].clone();
                start["program"] = "/nonexistent/program".into();
                child.initialize(Profile::VsCode);
                let attach = child.send("attach", start);
                let failure = child.failure(attach);
                child.finish();
                failures.push(failure.clone());
                Some(Err(failure))
            }

            Children::Followed {
                configuration,
                started,
            } => {
                let request = arguments["request"].as_str().expect("request");
                let child_configuration = &arguments["configuration"];
                let name = child_configuration["name"].as_str().expect("child name");
                let mut child = Self::start(format!("{} › {name}", self.name));
                let mut start = child_configuration.clone();
                start["type"] = "microscope".into();
                start["request"] = request.into();
                child.begin(Profile::VsCode, (request, start), configuration);
                started.push(child);
                Some(Ok(()))
            }
        };

        let Some(result) = result else {
            self.fail("an unexpected startDebugging request");
        };

        self.seq += 1;
        let mut response = json!({
            "seq": self.seq,
            "type": "response",
            "request_seq": request["seq"],
            "command": "startDebugging",
            "success": result.is_ok(),
        });

        if let Err(message) = result {
            response["message"] = message.into();
        }

        self.write(&response.to_string());
    }

    /// Answers `runInTerminal` with an error, as a client whose terminal
    /// failed does.
    pub fn refuse_terminals(&mut self, message: &str) {
        self.refused_terminal = Some(message.to_owned());
    }

    /// Leaves `runInTerminal` requests unanswered until released, keeping
    /// the adapter waiting inside the request that sent them.
    pub fn hold_terminals(&mut self) {
        self.held_terminals = Some(Vec::new());
    }

    /// Waits for a held `runInTerminal` request, then answers every held
    /// one and stops holding them.
    pub fn release_terminals(&mut self) {
        let deadline = Instant::now() + EVENT_TIMEOUT;

        while self.held_terminals.as_ref().is_some_and(Vec::is_empty) {
            self.receive(deadline, "a runInTerminal request");
        }

        for request in self.held_terminals.take().unwrap_or_default() {
            self.run_in_terminal(&request);
        }
    }

    /// Waits for the next line a program run in a terminal writes.
    pub fn terminal_line(&self) -> String {
        self.terminal_output
            .1
            .recv_timeout(EVENT_TIMEOUT)
            .unwrap_or_else(|_| self.fail("no output arrived from the terminal"))
    }

    /// Runs a `runInTerminal` request's command as a terminal would, and
    /// answers it.
    fn run_in_terminal(&mut self, request: &Value) {
        let arguments = &request["arguments"];

        let result = self
            .refused_terminal
            .clone()
            .map_or_else(|| Ok(self.spawn_terminal(arguments)), Err);

        self.seq += 1;
        let mut response = json!({
            "seq": self.seq,
            "type": "response",
            "request_seq": request["seq"],
            "command": "runInTerminal",
            "success": result.is_ok(),
        });

        match result {
            Ok(body) => response["body"] = body,
            Err(message) => response["message"] = message.into(),
        }

        self.write(&response.to_string());
    }

    /// Spawns a terminal's command, returning the `runInTerminal` body.
    fn spawn_terminal(&mut self, arguments: &Value) -> Value {
        let args = arguments["args"]
            .as_array()
            .expect("args")
            .iter()
            .map(|argument| argument.as_str().expect("string argument"))
            .collect::<Vec<_>>();

        let mut command = Command::new(args[0]);
        command
            .args(&args[1..])
            .current_dir(arguments["cwd"].as_str().expect("cwd"))
            .stdin(Stdio::null());

        // One pipe for both streams, as a terminal has, keeps their order.
        let (output, input) = std::io::pipe().expect("create the terminal's pipe");

        command
            .stdout(input.try_clone().expect("share the terminal's pipe"))
            .stderr(input);

        for (name, value) in arguments["env"].as_object().into_iter().flatten() {
            match value.as_str() {
                Some(value) => command.env(name, value),
                None => command.env_remove(name),
            };
        }

        let child = command.spawn().expect("run the terminal's command");

        // The command holds copies of the pipe's input; without them the
        // pipe ends when the program's copies close.
        drop(command);

        let lines = self.terminal_output.0.clone();

        std::thread::spawn(move || {
            for line in BufReader::new(output).lines().map_while(Result::ok) {
                let _ = lines.send(line);
            }
        });

        let process = child.id();
        self.terminals.push(child);

        json!({"processId": process})
    }

    /// Kills and reaps the programs run in terminals.
    fn reap_terminals(&mut self) {
        for mut child in self.terminals.drain(..) {
            let _ = child.kill();
            let _ = child.wait();
        }
    }

    /// Accepts requests racing stops and stops nobody looks at, as a
    /// chaos client causes them.
    pub const fn relax_ordering_checks(&mut self) {
        self.checks.relaxed = true;
    }

    /// Sends text as a request with `seq`, which may be malformed, and
    /// expects one response to it.
    pub fn send_raw(&mut self, seq: u64, command: &str, text: &str) -> Sent {
        let mark = self.mark();
        self.outstanding.insert(seq, command.to_owned());
        self.write(text);
        Sent { seq, mark }
    }

    fn record(&self, direction: &str, text: &str) {
        let mut text = text.to_owned();
        if text.len() > 2000 {
            let mut end = 2000;
            while !text.is_char_boundary(end) {
                end -= 1;
            }

            text.truncate(end);
            text.push('…');
        }

        self.transcript.lock().expect("transcript").push(format!(
            "[{:8.3}] {direction} {text}",
            self.started.elapsed().as_secs_f64()
        ));
    }

    /// The position after every message received so far.
    pub fn mark(&mut self) -> Mark {
        self.drain();
        Mark(self.received.len())
    }

    /// Sends a request without waiting for its response.
    pub fn send(&mut self, command: &str, arguments: Value) -> Sent {
        let mark = self.mark();

        self.seq += 1;

        let mut message = json!({"seq": self.seq, "type": "request", "command": command});

        if !arguments.is_null() {
            message["arguments"] = arguments;
        }

        self.outstanding.insert(self.seq, command.to_owned());
        self.write(&message.to_string());

        Sent {
            seq: self.seq,
            mark,
        }
    }

    /// Writes raw text as one message, for malformed input.
    pub fn write(&mut self, text: &str) {
        self.record("->", text);
        self.write_bytes(format!("Content-Length: {}\r\n\r\n{text}", text.len()).as_bytes());
    }

    /// Writes raw bytes to the adapter's stdin.
    pub fn write_bytes(&mut self, bytes: &[u8]) {
        let stdin = self.stdin.as_mut().expect("adapter stdin is open");
        stdin.write_all(bytes).expect("write to adapter");
        stdin.flush().expect("flush adapter stdin");
    }

    /// Closes the adapter's stdin, as a client that vanishes does.
    pub fn close_stdin(&mut self) {
        self.stdin = None;
    }

    /// Waits for a request's response.
    pub fn response(&mut self, sent: Sent) -> Value {
        let deadline = Instant::now() + REQUEST_TIMEOUT;
        loop {
            if let Some(index) = self.received.iter().position(|received| {
                received.message["type"] == "response"
                    && received.message["request_seq"] == sent.seq
            }) {
                self.received[index].consumed = true;
                return self.received[index].message.clone();
            }

            self.receive(deadline, &format!("the response to request {}", sent.seq));
        }
    }

    /// Sends a request and returns its body, which must be a success.
    pub fn request(&mut self, command: &str, arguments: Value) -> Value {
        let sent = self.send(command, arguments);
        self.success(sent)
    }

    /// Waits for a response that must be a success and returns its body.
    pub fn success(&mut self, sent: Sent) -> Value {
        let response = self.response(sent);
        if response["success"] != true {
            self.fail(&format!("request {} failed: {response}", sent.seq));
        }

        response["body"].clone()
    }

    /// Sends a request that must fail and returns its error message.
    pub fn request_error(&mut self, command: &str, arguments: Value) -> String {
        let sent = self.send(command, arguments);
        self.failure(sent)
    }

    /// Waits for a response that must fail and returns its message.
    pub fn failure(&mut self, sent: Sent) -> String {
        let response = self.response(sent);

        if response["success"] != false {
            self.fail(&format!(
                "request {} unexpectedly succeeded: {response}",
                sent.seq
            ));
        }

        response["body"]["error"]["format"]
            .as_str()
            .unwrap_or_default()
            .replace("{{", "{")
            .replace("}}", "}")
    }

    /// Waits for an event after `mark` matching `predicate`, and returns its
    /// body.
    pub fn event(&mut self, mark: Mark, event: &str, predicate: impl Fn(&Value) -> bool) -> Value {
        let deadline = Instant::now() + EVENT_TIMEOUT;
        let mut searched = mark.0;
        loop {
            while searched < self.received.len() {
                let received = &mut self.received[searched];
                searched += 1;
                if !received.consumed
                    && received.message["type"] == "event"
                    && received.message["event"] == event
                    && predicate(&received.message["body"])
                {
                    received.consumed = true;
                    return received.message["body"].clone();
                }
            }

            self.receive(deadline, &format!("a {event} event"));
        }
    }

    /// Returns every event of a kind received after `mark` so far, without
    /// waiting.
    pub fn events(&mut self, mark: Mark, event: &str) -> Vec<Value> {
        self.drain();
        let mut bodies = Vec::new();
        for received in &mut self.received[mark.0..] {
            if received.message["type"] == "event" && received.message["event"] == event {
                received.consumed = true;
                bodies.push(received.message["body"].clone());
            }
        }

        bodies
    }

    /// Waits for the first unconsumed event after `mark` of any of `kinds`,
    /// and returns its kind and body.
    pub fn next_event(&mut self, mark: Mark, kinds: &[&str]) -> (String, Value) {
        let deadline = Instant::now() + EVENT_TIMEOUT;
        let mut searched = mark.0;
        loop {
            while searched < self.received.len() {
                let received = &mut self.received[searched];
                searched += 1;
                if let Some(kind) = received.message["event"]
                    .as_str()
                    .filter(|kind| kinds.contains(kind))
                    && !received.consumed
                    && received.message["type"] == "event"
                {
                    received.consumed = true;
                    return (kind.to_owned(), received.message["body"].clone());
                }
            }

            self.receive(deadline, &format!("one of the events {kinds:?}"));
        }
    }

    /// Waits for the next `stopped` event after `mark`.
    pub fn stopped(&mut self, mark: Mark) -> Stopped {
        let body = self.event(mark, "stopped", |_| true);

        Stopped {
            thread: body["threadId"].as_i64().expect("threadId"),
            reason: body["reason"].as_str().expect("reason").to_owned(),
            body,
        }
    }

    /// Every message received after `mark`, in order.
    pub fn messages_since(&mut self, mark: Mark) -> Vec<Value> {
        self.drain();

        self.received[mark.0..]
            .iter()
            .map(|received| received.message.clone())
            .collect()
    }

    /// Concatenates the text of `output` events of one category after
    /// `mark`, waiting until it contains `expected`.
    pub fn output_containing(&mut self, mark: Mark, category: &str, expected: &str) -> String {
        let deadline = Instant::now() + EVENT_TIMEOUT;
        loop {
            let text = self.output_text(mark, category);
            if text.contains(expected) {
                return text;
            }

            self.receive(
                deadline,
                &format!("{category} output containing {expected:?}"),
            );
        }
    }

    /// The text of every `output` event of one category after `mark`.
    pub fn output_text(&mut self, mark: Mark, category: &str) -> String {
        self.drain();

        self.received[mark.0..]
            .iter()
            .filter(|received| {
                received.message["event"] == "output"
                    && received.message["body"]["category"] == category
            })
            .filter_map(|received| received.message["body"]["output"].as_str())
            .collect()
    }

    fn drain(&mut self) {
        while let Ok(message) = self.inbox.try_recv() {
            self.accept(message);
        }
    }

    fn receive(&mut self, deadline: Instant, waiting_for: &str) {
        let remaining = deadline.saturating_duration_since(Instant::now());

        match self.inbox.recv_timeout(remaining) {
            Ok(message) => self.accept(message),
            Err(RecvTimeoutError::Timeout) => {
                self.fail(&format!("timed out waiting for {waiting_for}"))
            }
            Err(RecvTimeoutError::Disconnected) => {
                self.fail(&format!(
                    "the adapter closed its output while waiting for {waiting_for}"
                ));
            }
        }
    }

    fn accept(&mut self, message: Result<Value, String>) {
        let message = match message {
            Ok(message) => message,
            Err(violation) => self.fail(&format!("the adapter broke framing: {violation}")),
        };

        self.record("<-", &message.to_string());

        if self.reference {
            self.received.push(Received {
                message,
                consumed: false,
            });
            return;
        }

        if let Err(problem) = self.check(&message) {
            self.fail(&format!(
                "protocol violation: {problem}\nmessage: {message}"
            ));
        }

        if message["type"] == "request" && message["command"] == "startDebugging" {
            self.start_debugging(&message);
        } else if message["type"] == "request" {
            match &mut self.held_terminals {
                Some(held) => held.push(message.clone()),
                None => self.run_in_terminal(&message),
            }
        }

        // An attached program that exits is gone, not left running.
        if message["event"] == "exited" && self.process.is_some_and(|(_, attached)| attached) {
            self.process = None;
        }

        if message["event"] == "process" {
            let attached = message["body"]["startMethod"] == "attach";
            self.process = message["body"]["systemProcessId"]
                .as_u64()
                .map(|pid| (u32::try_from(pid).expect("pid fits u32"), attached));
        }

        self.received.push(Received {
            message,
            consumed: false,
        });
    }

    /// Checks one message against the schema and the ordering rules.
    fn check(&mut self, message: &Value) -> Result<(), String> {
        validate_schema(message)?;
        check_ids(message)?;

        let checks = &mut self.checks;
        if message["seq"] != checks.next_seq {
            return Err(format!("expected seq {}", checks.next_seq));
        }

        checks.next_seq += 1;
        if message["type"] != "request" && !message["body"].is_object() {
            return Err("every response and event needs a body object".to_owned());
        }

        match message["type"].as_str() {
            Some("request")
                if matches!(
                    message["command"].as_str(),
                    Some("runInTerminal" | "startDebugging")
                ) => {}
            Some("response") => {
                let request_seq = message["request_seq"]
                    .as_u64()
                    .ok_or("request_seq is not a number")?;

                let command = self
                    .outstanding
                    .remove(&request_seq)
                    .ok_or_else(|| format!("no outstanding request {request_seq}"))?;

                if message["command"] != command.as_str() {
                    return Err(format!("the response to {command} names another command"));
                }

                if command == "initialize" && message["success"] == true {
                    checks.initialized_response = true;
                }

                if message["success"] == true
                    && matches!(command.as_str(), "continue" | "next" | "stepIn" | "stepOut")
                {
                    checks.stopped = false;
                }
            }
            Some("event") => {
                if !checks.initialized_response {
                    return Err("an event preceded the initialize response".to_owned());
                }

                self.check_event(message)?;
            }
            other => return Err(format!("unexpected message type {other:?}")),
        }

        Ok(())
    }

    /// Checks the ordering rules of one event.
    fn check_event(&mut self, message: &Value) -> Result<(), String> {
        let checks = &mut self.checks;
        let body = &message["body"];
        match message["event"].as_str() {
            Some("initialized") => {
                if checks.initialized_event {
                    return Err("a second initialized event".to_owned());
                }

                checks.initialized_event = true;
            }
            Some("stopped") => {
                if !checks.relaxed {
                    // A thread moved without running stops again, after the
                    // response to the request that moved it.
                    if checks.stopped && body["reason"] != "goto" {
                        return Err("a second stopped event without a resume between".to_owned());
                    }

                    if let Some(command) = self.outstanding.values().find(|command| {
                        matches!(
                            command.as_str(),
                            "continue" | "next" | "stepIn" | "stepOut" | "goto"
                        )
                    }) {
                        return Err(format!("a stop preceded the response to {command}"));
                    }
                }

                checks.stopped = true;
                let thread = body["threadId"]
                    .as_i64()
                    .ok_or("stopped without threadId")?;

                if !checks.threads.contains(&thread) {
                    return Err(format!(
                        "stopped names thread {thread} before its thread event"
                    ));
                }

                if body["allThreadsStopped"] != true {
                    return Err("an all-stop debugger stops every thread".to_owned());
                }
            }
            Some("continued") => checks.stopped = false,
            Some("thread") => {
                let thread = body["threadId"].as_i64().ok_or("thread without threadId")?;

                match body["reason"].as_str() {
                    Some("started") => {
                        if !checks.threads.insert(thread) {
                            return Err(format!("thread {thread} started twice"));
                        }
                    }
                    Some("exited") => {
                        if !checks.threads.remove(&thread) {
                            return Err(format!("thread {thread} exited without starting"));
                        }
                    }
                    other => return Err(format!("unexpected thread reason {other:?}")),
                }
            }
            Some("exited") => {
                if body["exitCode"].as_i64().is_none_or(|code| code < 0) {
                    return Err("exit codes must be non-negative".to_owned());
                }

                checks.stopped = false;
            }
            Some("terminated") => {
                checks.stopped = false;
                checks.threads.clear();
            }
            Some("module") => {
                let id = body["module"]["id"]
                    .as_str()
                    .ok_or("module without a string id")?
                    .to_owned();

                match body["reason"].as_str() {
                    Some("new") => {
                        if !checks.modules.insert(id.clone()) {
                            return Err(format!("module {id} is new twice"));
                        }
                    }
                    Some("changed") => {
                        if !checks.modules.contains(&id) {
                            return Err(format!("module {id} changed before it was new"));
                        }
                    }
                    Some("removed") => {
                        if !checks.modules.remove(&id) {
                            return Err(format!("module {id} was removed without being new"));
                        }
                    }
                    other => return Err(format!("unexpected module reason {other:?}")),
                }
            }
            _ => {}
        }

        Ok(())
    }

    fn fail(&self, problem: &str) -> ! {
        self.reported.set(true);
        panic!("{}: {problem}\n{}", self.name, self.transcript())
    }

    fn transcript(&self) -> String {
        self.transcript.lock().expect("transcript").join("\n")
    }

    /// Initializes the session as `profile` does and returns the
    /// capabilities.
    pub fn initialize(&mut self, profile: Profile) -> Value {
        let mark = self.mark();
        let capabilities = self.request("initialize", profile.initialize_arguments());
        self.event(mark, "initialized", |_| true);

        capabilities
    }

    /// Starts a session as `profile` does: initializes, configures
    /// breakpoints, and launches or attaches with `start` (`"launch"` or
    /// `"attach"` and its arguments).
    pub fn begin(
        &mut self,
        profile: Profile,
        start: (&str, Value),
        configuration: &Configuration,
    ) -> Started {
        let mark = self.mark();
        let initialize = self.send("initialize", profile.initialize_arguments());
        let (command, arguments) = start;
        let capabilities = self.success(initialize);

        // All but a deferring client send the start request once
        // initialize answers.
        let mut launch =
            (profile != Profile::DeferredLaunch).then(|| self.send(command, arguments.clone()));

        self.event(mark, "initialized", |_| true);

        let exceptions = configuration.exceptions.clone().unwrap_or_else(|| {
            let filters = capabilities["exceptionBreakpointFilters"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|filter| filter["default"] == true)
                .filter_map(|filter| filter["filter"].as_str().map(str::to_owned))
                .collect::<Vec<_>>();
            json!({"filters": filters})
        });

        let mut sent = Vec::new();

        for (path, lines) in &configuration.sources {
            sent.push(self.send(
                "setBreakpoints",
                json!({
                    "source": {"name": path.file_name().map(|name| name.to_string_lossy()), "path": path},
                    "breakpoints": lines.iter().map(|line| json!({"line": line})).collect::<Vec<_>>(),
                    "sourceModified": false,
                }),
            ));

            if profile != Profile::VsCode {
                let last = *sent.last().expect("just sent");
                self.success(last);
            }
        }

        let functions = (!configuration.functions.is_empty()).then(|| {
            self.send(
                "setFunctionBreakpoints",
                json!({"breakpoints": configuration.functions.iter().map(|name| json!({"name": name})).collect::<Vec<_>>()}),
            )
        });

        let exceptions = self.send("setExceptionBreakpoints", exceptions);
        let done = self.send("configurationDone", Value::Null);
        let threads = (profile == Profile::VsCode).then(|| self.send("threads", Value::Null));

        if profile == Profile::DeferredLaunch {
            launch = Some(self.send(command, arguments));
        }

        let source_breakpoints = sent
            .into_iter()
            .map(|sent| breakpoints(&self.success(sent)))
            .collect();

        let function_breakpoints = functions
            .map(|sent| breakpoints(&self.success(sent)))
            .unwrap_or_default();

        self.success(exceptions);
        self.success(done);

        if let Some(threads) = threads {
            let body = self.success(threads);
            assert!(
                !body["threads"].as_array().is_none_or(Vec::is_empty),
                "threads is never empty"
            );
        }

        let launch = launch.expect("the profile sent the start request");
        self.success(launch);

        Started {
            capabilities,
            source_breakpoints,
            function_breakpoints,
            mark,
        }
    }

    /// Launches `program` as `profile` would, with `configuration` set
    /// before it runs and extra launch arguments merged in.
    pub fn launch(
        &mut self,
        profile: Profile,
        program: &Path,
        extra: Value,
        configuration: &Configuration,
    ) -> Started {
        let mut arguments = json!({
            "type": "microscope",
            "request": "launch",
            "name": "Launch",
            "program": program,
            "__sessionId": "d3e5c1a0-0000-4000-8000-000000000000",
        });

        if let (Some(arguments), Value::Object(extra)) = (arguments.as_object_mut(), extra) {
            arguments.extend(extra);
        }

        self.begin(profile, ("launch", arguments), configuration)
    }

    /// Inspects a stop as `profile` does, returning the stopped thread's
    /// frames. Each client asks for different things on every stop, and
    /// the adapter must answer each consistently.
    pub fn inspect_as(&mut self, profile: Profile, stopped: &Stopped) -> Vec<Value> {
        match profile {
            Profile::VsCode => {
                let threads = self.request("threads", Value::Null);

                assert!(
                    threads["threads"]
                        .as_array()
                        .expect("threads")
                        .iter()
                        .any(|thread| thread["id"] == stopped.thread)
                );

                let first = self.request(
                    "stackTrace",
                    json!({"threadId": stopped.thread, "startFrame": 0, "levels": 1}),
                );
                let rest = self.request(
                    "stackTrace",
                    json!({"threadId": stopped.thread, "startFrame": 1, "levels": 19}),
                );

                let mut frames = first["stackFrames"].as_array().expect("frames").clone();
                frames.extend(
                    rest["stackFrames"]
                        .as_array()
                        .expect("frames")
                        .iter()
                        .cloned(),
                );

                assert_eq!(first["totalFrames"], rest["totalFrames"]);

                let scopes = self.request("scopes", json!({"frameId": frames[0]["id"]}));
                if let Some(scope) = scopes["scopes"]
                    .as_array()
                    .expect("scopes")
                    .iter()
                    .find(|scope| scope["expensive"] == false && scope["variablesReference"] != 0)
                {
                    self.request(
                        "variables",
                        json!({"variablesReference": scope["variablesReference"]}),
                    );
                }

                frames
            }
            Profile::Neovim | Profile::DeferredLaunch => {
                let trace = self.request("stackTrace", json!({"threadId": stopped.thread}));
                let frames = trace["stackFrames"].as_array().expect("frames").clone();

                let scopes = self.request("scopes", json!({"frameId": frames[0]["id"]}));
                for scope in scopes["scopes"].as_array().expect("scopes") {
                    if scope["expensive"] == false && scope["variablesReference"] != 0 {
                        self.request(
                            "variables",
                            json!({"variablesReference": scope["variablesReference"]}),
                        );
                    }
                }

                frames
            }
            Profile::Helix => {
                let threads = self.request("threads", Value::Null);
                let mut stopped_frames = Vec::new();
                for thread in threads["threads"].as_array().expect("threads") {
                    let trace = self.request("stackTrace", json!({"threadId": thread["id"]}));
                    if thread["id"] == stopped.thread {
                        stopped_frames.clone_from(trace["stackFrames"].as_array().expect("frames"));
                    }
                }

                stopped_frames
            }
        }
    }

    /// The threads the client was told exist.
    pub fn known_threads(&self) -> BTreeSet<i64> {
        self.checks.threads.clone()
    }

    /// The ids of the modules the client was told are loaded.
    pub fn known_modules(&self) -> BTreeSet<String> {
        self.checks.modules.clone()
    }

    /// The process the adapter reported in its `process` event.
    pub fn process_id(&self) -> Option<u32> {
        self.process.map(|(pid, _)| pid)
    }

    /// Disconnects and checks that the adapter exits cleanly, that every
    /// stop was looked at, and that a launched program is gone and an
    /// attached one is left running untraced.
    pub fn finish(mut self) {
        self.finish_with(json!({}));
    }

    /// Disconnects with explicit arguments, then checks as [`Self::finish`].
    pub fn finish_with(&mut self, arguments: Value) {
        self.drain();
        if self.stdin.is_some() {
            let sent = self.send("disconnect", arguments);
            self.success(sent);
            self.close_stdin();
        }

        if self.child.is_some() {
            self.wait_for_exit();
        } else {
            self.wait_for_end();
        }

        // Nothing may follow the disconnect response.
        self.drain();

        let unexpected_stops = self
            .received
            .iter()
            .filter(|received| !received.consumed && received.message["event"] == "stopped")
            .map(|received| received.message.to_string())
            .collect::<Vec<_>>();

        if !unexpected_stops.is_empty() && !self.checks.relaxed {
            self.fail(&format!("stops nobody waited for: {unexpected_stops:?}"));
        }

        if !self.outstanding.is_empty() {
            self.fail(&format!("requests never answered: {:?}", self.outstanding));
        }

        if let Some((pid, attached)) = self.process {
            let status = std::fs::read_to_string(format!("/proc/{pid}/status"));
            if attached {
                let status = status.unwrap_or_else(|_| self.fail("the attached process is gone"));
                if !status
                    .lines()
                    .any(|line| line.split_whitespace().eq(["TracerPid:", "0"]))
                {
                    self.fail("the attached process is still traced");
                }
            } else if status.is_ok_and(|status| !status.contains("State:\tZ")) {
                self.fail(&format!("the launched process {pid} outlived the session"));
            }
        }

        self.reap_terminals();
        self.finished = true;
    }

    /// Waits for the adapter to exit, which it must do with status 0.
    pub fn wait_for_exit(&mut self) {
        let mut child = self.child.take().expect("adapter process");
        let pid = child.id();
        let (send, exited) = mpsc::channel();

        std::thread::spawn(move || {
            let _ = send.send(child.wait());
        });

        match exited.recv_timeout(REQUEST_TIMEOUT) {
            Ok(Ok(status)) if status.success() => {}
            Ok(Ok(status)) => self.fail(&format!("the adapter exited with {status}")),
            Ok(Err(error)) => self.fail(&format!("cannot wait for the adapter: {error}")),
            Err(_) => {
                let _ = nix::sys::signal::kill(
                    nix::unistd::Pid::from_raw(i32::try_from(pid).expect("pid fits i32")),
                    nix::sys::signal::Signal::SIGKILL,
                );
                self.fail("the adapter did not exit");
            }
        }

        self.wait_for_end();
    }

    /// Waits for the adapter to close its output, as it does when its
    /// session ends.
    fn wait_for_end(&mut self) {
        let deadline = Instant::now() + REQUEST_TIMEOUT;
        loop {
            match self
                .inbox
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            {
                Ok(message) => self.accept(message),
                Err(RecvTimeoutError::Disconnected) => break,
                Err(RecvTimeoutError::Timeout) => self.fail("the adapter's output did not end"),
            }
        }
    }

    /// Sends a signal to the adapter process.
    pub fn signal(&self, signal: nix::sys::signal::Signal) {
        let pid = self.child.as_ref().expect("adapter process").id();

        nix::sys::signal::kill(
            nix::unistd::Pid::from_raw(i32::try_from(pid).expect("pid fits i32")),
            signal,
        )
        .expect("signal the adapter");
    }

    /// Marks the session finished without disconnecting, for scenarios
    /// that end it some other way and check the outcome themselves.
    pub fn abandon(mut self) {
        self.finished = true;

        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

impl Drop for Dap {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }

        self.reap_terminals();

        if let Some(recording) = &self.recording {
            flight_recordings::adapter_finished(recording);
        }

        if std::thread::panicking() {
            if !self.reported.get() {
                eprintln!("{} transcript:\n{}", self.name, self.transcript());
            }
        } else if !self.finished {
            panic!("{}: the scenario never finished its session", self.name);
        }
    }
}

/// The breakpoints of a `setBreakpoints`-like response body.
pub fn breakpoints(body: &Value) -> Vec<Value> {
    body["breakpoints"].as_array().expect("breakpoints").clone()
}

/// Reads framed messages from the adapter's stdout, reporting anything
/// that is not a well-formed frame.
fn read_frames(stdout: impl std::io::Read, send: &mpsc::Sender<Result<Value, String>>) {
    let mut reader = BufReader::new(stdout);
    loop {
        let mut length = None;
        loop {
            let mut line = String::new();
            match reader.read_line(&mut line) {
                Ok(0) if length.is_none() => return,
                Ok(0) => {
                    let _ = send.send(Err("the stream ended inside a header".to_owned()));
                    return;
                }
                Ok(_) => {}
                Err(error) => {
                    let _ = send.send(Err(format!("unreadable header: {error}")));
                    return;
                }
            }

            if line == "\r\n" {
                break;
            }

            let Some(value) = line
                .strip_suffix("\r\n")
                .and_then(|line| line.strip_prefix("Content-Length: "))
            else {
                let _ = send.send(Err(format!("unexpected header line {line:?}")));
                return;
            };

            length = value.parse::<usize>().ok();
        }

        let Some(length) = length else {
            let _ = send.send(Err("a frame without Content-Length".to_owned()));
            return;
        };

        let mut body = vec![0; length];
        if let Err(error) = reader.read_exact(&mut body) {
            let _ = send.send(Err(format!("a truncated body: {error}")));
            return;
        }

        let message = serde_json::from_slice::<Value>(&body)
            .map_err(|error| format!("invalid JSON: {error}"));

        if send.send(message).is_err() {
            return;
        }
    }
}

/// Validates a message against the protocol's schema definition for it.
fn validate_schema(message: &Value) -> Result<(), String> {
    static VALIDATORS: OnceLock<Mutex<HashMap<String, Arc<jsonschema::Validator>>>> =
        OnceLock::new();
    static SCHEMA: OnceLock<Value> = OnceLock::new();

    let schema = SCHEMA.get_or_init(|| {
        serde_json::from_str(include_str!("../dap/schema/debugAdapterProtocol.json"))
            .expect("schema")
    });

    let custom = message["command"]
        .as_str()
        .is_some_and(|command| command.starts_with("microscope/"));

    let definition = match message["type"].as_str() {
        Some("response") if message["success"] == false => "ErrorResponse".to_owned(),
        // The adapter's own requests, which the protocol allows any adapter
        // to add, have only the protocol's general shape.
        Some("response") if custom => "Response".to_owned(),
        Some("request") if custom => "Request".to_owned(),
        Some("response") => format!(
            "{}Response",
            capitalized(message["command"].as_str().unwrap_or_default())
        ),
        Some("event") => format!(
            "{}Event",
            capitalized(message["event"].as_str().unwrap_or_default())
        ),
        Some("request") => format!(
            "{}Request",
            capitalized(message["command"].as_str().unwrap_or_default())
        ),
        _ => return Err("not a request, response, or event".to_owned()),
    };

    if schema["definitions"].get(&definition).is_none() {
        return Err(format!("the protocol defines no {definition}"));
    }

    let validator = {
        let mut validators = VALIDATORS
            .get_or_init(Mutex::default)
            .lock()
            .expect("validators");
        Arc::clone(validators.entry(definition.clone()).or_insert_with(|| {
            let document = json!({
                "$ref": format!("#/definitions/{definition}"),
                "definitions": schema["definitions"],
            });
            Arc::new(jsonschema::validator_for(&document).expect("valid schema"))
        }))
    };

    let errors = validator
        .iter_errors(message)
        .map(|error| format!("{} at {}", error, error.instance_path()))
        .collect::<Vec<_>>();

    if errors.is_empty() {
        Ok(())
    } else {
        Err(format!("{definition} schema: {}", errors.join("; ")))
    }
}

fn capitalized(name: &str) -> String {
    let mut characters = name.chars();
    characters
        .next()
        .map(|first| first.to_ascii_uppercase().to_string() + characters.as_str())
        .unwrap_or_default()
}

/// Checks that every id is a positive 32-bit integer and every reference a
/// non-negative one, as clients store them.
fn check_ids(value: &Value) -> Result<(), String> {
    match value {
        Value::Object(object) => {
            for (key, value) in object {
                let positive = matches!(key.as_str(), "id" | "threadId" | "frameId");
                let reference = matches!(key.as_str(), "variablesReference" | "sourceReference");

                if (positive || reference)
                    && let Some(number) = value.as_i64()
                    && !(i64::from(!reference)..=i64::from(i32::MAX)).contains(&number)
                {
                    return Err(format!("{key} {number} is not a valid 32-bit id"));
                }

                if key == "hitBreakpointIds" {
                    for id in value.as_array().into_iter().flatten() {
                        if id
                            .as_i64()
                            .is_none_or(|id| !(1..=i64::from(i32::MAX)).contains(&id))
                        {
                            return Err(format!("hit breakpoint id {id} is not a valid 32-bit id"));
                        }
                    }
                }

                check_ids(value)?;
            }
        }
        Value::Array(values) => {
            for value in values {
                check_ids(value)?;
            }
        }
        _ => {}
    }

    Ok(())
}

/// The path of a built fixture.
pub fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("build/test-programs")
        .join(name)
}

/// The path of a fixture source file.
pub fn source(path: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(path)
}

/// The line of the first source line containing `marker`.
pub fn line_of(path: &Path, marker: &str) -> u64 {
    let text = std::fs::read_to_string(path)
        .unwrap_or_else(|error| panic!("read {}: {error}", path.display()));

    text.lines()
        .position(|line| line.contains(marker))
        .map_or_else(
            || panic!("{} has no line containing {marker:?}", path.display()),
            |index| index as u64 + 1,
        )
}
