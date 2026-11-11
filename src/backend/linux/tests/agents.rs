//! How a runtime's agent stands, through every event that changes it: not
//! installed, waiting for an interpreter thread, awaiting the
//! acknowledgement of a source request or of a command, read or unread,
//! timed out, ready with sites or idle, and refused by the agent or before
//! any write reached the program.
//!
//! Every phase is reached, and every event made, through the controller's
//! messages, wait statuses and hook hits, as in the run-phase table, so the
//! table holds however the controller keeps an agent's installation. A
//! scripted agent and its files log what the controller asks of them.

use std::collections::BTreeSet;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use super::lifecycle::{Reader, reader, send};
use super::run_phase::{OTHER, Run};
use super::*;
use crate::runtime_model::{
    AgentFailure, AgentRequestTicket, AgentRetirement, AgentRetirementProof, Arming, HookEvent,
    HookPlace, PreparedAgentRequest, RuntimeAgent, RuntimeStop, RuntimeWrite, TargetProcessId,
};

pub(super) const LEADER: Pid = Pid::from_raw(5000);

/// Where the runtime's image begins to run interpreted code, which the
/// controller traps while the agent waits for an initialized thread.
pub(super) const ENTRY: u64 = 0x60;

/// Where the agent's library loads, and where in it its report hook and
/// command slot are.
const LIBRARY_BIAS: u64 = 0x7300_0000;
const REPORT: u64 = LIBRARY_BIAS + 0x100;
const CONTROL: u64 = LIBRARY_BIAS + 0x200;

/// The interpreter's slot for a thread's pending source request.
const SLOT: u64 = 0x9000;

pub(super) type Log = Arc<Mutex<Vec<String>>>;

fn log(log: &Log, entry: String) {
    log.lock().expect("the log").push(entry);
}

/// An agent that logs what it is asked, and whose interpreter thread and
/// refusals each phase sets.
#[derive(Debug)]
pub(super) struct Agent {
    pub(super) log: Log,
    entries: [ImageAddress; 1],

    /// The thread a source request goes to, or none while the interpreter
    /// is not initialized.
    pub(super) thread: Mutex<Option<crate::ThreadId>>,

    /// Why a request is refused before any write.
    pub(super) refusal: Mutex<Option<Arc<str>>>,

    /// Whether a source request goes to the launch's bootstrap.
    bootstrap: Mutex<bool>,

    /// Whether a source request sits in the slot, untaken.
    pub(super) pending: Arc<AtomicBool>,

    /// The newest generation published.
    pub(super) generation: Mutex<u64>,

    /// The generation of the script last written, published only once
    /// it is injected.
    scripted: Mutex<u64>,
}

fn sites(arming: &Arming<'_>) -> String {
    arming
        .breakpoints
        .iter()
        .map(|arm| arm.number.to_string())
        .collect::<Vec<_>>()
        .join(" ")
}

impl RuntimeAgent for Agent {
    fn library(&self) -> Vec<u8> {
        b"hook".to_vec()
    }

    fn control_symbol(&self) -> &'static str {
        "microscope_control"
    }

    fn entries(&self) -> &[ImageAddress] {
        &self.entries
    }

    fn eligible_thread(
        &self,
        _stop: &dyn RuntimeStop,
        _preferred: Option<crate::ThreadId>,
        _arming: &Arming<'_>,
    ) -> std::result::Result<crate::ThreadId, AgentFailure> {
        (*self.thread.lock().expect("the script")).ok_or(AgentFailure::NoThread)
    }

    fn script(
        &self,
        _stop: &dyn RuntimeStop,
        _library: &str,
        generation: u64,
        _process: TargetProcessId,
        _arming: &Arming<'_>,
    ) -> std::result::Result<String, Arc<str>> {
        *self.scripted.lock().expect("the script") = generation;
        Ok("source".to_owned())
    }

    fn inject(
        &self,
        _stop: &dyn RuntimeStop,
        script: &str,
        thread: crate::ThreadId,
        arming: &Arming<'_>,
    ) -> std::result::Result<PreparedAgentRequest, AgentFailure> {
        let refusal = self.refusal.lock().expect("the script").clone();
        if let Some(reason) = refusal {
            log(&self.log, "inject refused".to_owned());
            return Err(AgentFailure::Refused(reason));
        }

        log(
            &self.log,
            format!("inject {script} on {thread} [{}]", sites(arming)),
        );
        *self.generation.lock().expect("the script") = *self.scripted.lock().expect("the script");
        if *self.bootstrap.lock().expect("the script") {
            return Ok(PreparedAgentRequest {
                writes: Vec::new(),
                ticket: Arc::new(crate::runtime_model::BootstrapTicket),
                bootstrapped: true,
            });
        }

        self.pending.store(true, Ordering::SeqCst);
        Ok(PreparedAgentRequest {
            writes: vec![RuntimeWrite {
                address: VirtualAddress::new(SLOT),
                expected: vec![0; 8],
                bytes: 1_u64.to_le_bytes().to_vec(),
            }],
            ticket: Arc::new(Ticket {
                log: Arc::clone(&self.log),
                pending: Arc::clone(&self.pending),
            }),
            bootstrapped: false,
        })
    }

    fn command(
        &self,
        _stop: &dyn RuntimeStop,
        control: VirtualAddress,
        previous: u64,
        generation: u64,
        arming: &Arming<'_>,
    ) -> std::result::Result<Vec<RuntimeWrite>, Arc<str>> {
        let refusal = self.refusal.lock().expect("the script").clone();
        if let Some(reason) = refusal {
            log(&self.log, "command refused".to_owned());
            return Err(reason);
        }

        log(
            &self.log,
            format!("command {previous}->{generation} [{}]", sites(arming)),
        );
        *self.generation.lock().expect("the script") = generation;
        Ok(vec![RuntimeWrite {
            address: control,
            expected: vec![0; 8],
            bytes: generation.to_le_bytes().to_vec(),
        }])
    }

    fn process_scope_required(&self) -> bool {
        false
    }
}

/// A source request's slot: one still pending is withdrawn, and one a
/// thread took is unconfirmed until its agent acknowledges it.
#[derive(Debug)]
struct Ticket {
    log: Log,
    pending: Arc<AtomicBool>,
}

impl AgentRequestTicket for Ticket {
    fn retire(
        &self,
        _stop: &dyn RuntimeStop,
        proof: AgentRetirementProof,
    ) -> std::result::Result<AgentRetirement, Arc<str>> {
        let pending = self.pending.swap(false, Ordering::SeqCst);
        let outcome = if pending {
            "withdrawn"
        } else if proof == AgentRetirementProof::Cancel {
            self.pending.store(false, Ordering::SeqCst);
            log(&self.log, format!("retire {proof:?}: unconfirmed"));
            return Ok(AgentRetirement::Unconfirmed);
        } else {
            "retired"
        };

        log(&self.log, format!("retire {proof:?}: {outcome}"));
        Ok(AgentRetirement::Retired(Vec::new()))
    }
}

/// A run of two threads in a process whose runtime has an agent, whose
/// library is loaded.
pub(super) struct Session {
    pub(super) run: Run,
    pub(super) agent: Arc<Agent>,
    runtime: Arc<ScriptedRuntime>,
}

fn exported(id: u32, name: &str, address: u64) -> crate::SymbolInfo {
    crate::SymbolInfo {
        id: crate::SymbolId::new(id),
        name: name.into(),
        address: ImageAddress::new(address - LIBRARY_BIAS),
        kind: crate::SymbolKind::Function,
        binding: crate::SymbolBinding::Global,
        exported: true,
        extent: None,
        storage: None,
        role: crate::CodeRole::Ordinary,
    }
}

impl Session {
    pub(super) fn new() -> Self {
        let mut run = Run::new(watch_harness(2));
        run.shown_writes = BTreeSet::from([SLOT, CONTROL]);
        let agent = Arc::new(Agent {
            log: Arc::clone(&run.harness.trace().agent_log),
            entries: [ImageAddress::new(ENTRY)],
            thread: Mutex::new(Some(debug_thread_id(OTHER))),
            refusal: Mutex::new(None),
            bootstrap: Mutex::new(false),
            pending: Arc::new(AtomicBool::new(false)),
            generation: Mutex::new(0),
            scripted: Mutex::new(0),
        });
        let runtime = scripted_runtime(
            &run.harness,
            ScriptedRuntime {
                agent: Some(Arc::clone(&agent) as _),
                internal: vec![HookPlace::Companion("microscope_report")],
                ..ScriptedRuntime::default()
            },
        );

        let library = PathBuf::from(AGENT_LIBRARY);
        run.harness
            .trace()
            .libraries
            .borrow_mut()
            .insert(library.clone(), (crate::LoadBias::new(LIBRARY_BIAS), true));
        run.harness.trace().library_symbols.borrow_mut().insert(
            library,
            vec![
                exported(0, "microscope_report", REPORT),
                exported(1, "microscope_control", CONTROL),
            ],
        );
        run.harness
            .controller
            .refresh_libraries()
            .expect("the agent's library loads");

        let mut session = Self {
            run,
            agent,
            runtime,
        };
        session.forget();
        session
    }

    /// Forgets what was logged, called and published so far.
    pub(super) fn forget(&mut self) {
        self.run.take_calls();
        self.run.calls.clear();
        self.run.harness.published();
        self.agent.log.lock().expect("the log").clear();
    }

    pub(super) fn add(&mut self, line: u64) -> Reader {
        let (reply, receiver) = tokio::sync::oneshot::channel();
        send(
            &mut self.run.harness,
            Request::AddBreakpoint {
                spec: BreakpointSpec::Source {
                    path: "app.scripted".into(),
                    line: crate::LineNumber::new(line).expect("a line"),
                },
                options: Box::default(),
                reply,
            },
        );
        self.run.settle(true);
        reader(receiver)
    }

    pub(super) fn remove(&mut self, id: u64) -> Reader {
        let (reply, receiver) = tokio::sync::oneshot::channel();
        send(
            &mut self.run.harness,
            Request::RemoveBreakpoint {
                id: BreakpointId::new(id),
                reply,
            },
        );
        self.run.settle(true);
        reader(receiver)
    }

    pub(super) fn resume(&mut self) -> Reader {
        let mut resumed = self.run.resume();
        self.run.settle(true);
        let said = resumed();
        Box::new(move || said.clone())
    }

    pub(super) fn pause(&mut self) -> Reader {
        let mut paused = self.run.pause();
        self.run.settle(true);
        let said = paused();
        Box::new(move || said.clone())
    }

    /// Continues only the leader, which the source request's thread is
    /// not.
    pub(super) fn resume_leader(&mut self) -> Reader {
        let (reply, receiver) = tokio::sync::oneshot::channel();
        let process_id = self.run.process();
        let stop_id = self.run.stop_id();
        send(
            &mut self.run.harness,
            Request::Continue {
                process_id,
                stop_id,
                scope: ResumeScope::Thread(debug_thread_id(LEADER)),
                exception: ExceptionDisposition::Pass,
                reply,
            },
        );
        self.run.settle(true);
        reader(receiver)
    }

    /// The source request's thread hits the hook at `address`, which
    /// reports `event`. A stopped program is continued first.
    pub(super) fn report(&mut self, address: u64, event: HookEvent) -> String {
        let resumed = if self.run.is_stopped() {
            format!("continue {}, ", self.resume()())
        } else {
            String::new()
        };

        self.runtime.reports(Ok(event));
        self.run.harness.trace().set_pc(OTHER, address + 1);
        self.run.signal_info(OTHER, libc::SI_KERNEL);
        self.run
            .program_stop(WaitEvent::Stopped(OTHER, Signal::SIGTRAP));
        self.run.settle(true);
        format!("{resumed}-")
    }

    /// The agent runs the newest request, taking a source request from
    /// its slot, and reports how its configuration went.
    pub(super) fn agent_reports(&mut self, failure: Option<&str>, stale: bool) -> String {
        self.agent.pending.store(false, Ordering::SeqCst);
        let newest = *self.agent.generation.lock().expect("the script");
        let generation = if stale {
            newest.saturating_sub(1)
        } else {
            newest
        };
        let event = failure.map_or(HookEvent::Armed { generation }, |description| {
            HookEvent::AgentFailed {
                generation,
                description: description.into(),
            }
        });
        self.report(REPORT, event)
    }

    /// The alarm rings once the acknowledgement budget would have run out
    /// had the program run all along.
    pub(super) fn alarm(&mut self) -> String {
        let timed_out = self
            .run
            .harness
            .controller
            .agent_timeout(Instant::now() + Duration::from_secs(6));
        self.run.settle(true);
        match timed_out {
            Ok(()) => "-".to_owned(),
            Err(error) => format!("{error:?}"),
        }
    }

    /// Ends the session, detaching from the process.
    pub(super) fn detach(&mut self) -> Reader {
        if let Some(inferior) = self.run.harness.controller.session.inferior_mut() {
            inferior.origin = InferiorOrigin::Attached;
        }
        let (reply, receiver) = tokio::sync::oneshot::channel();
        send(&mut self.run.harness, Request::Shutdown { reply });
        self.run.settle(true);
        reader(receiver)
    }

    /// The process exits, its threads one after the other.
    pub(super) fn exit(&mut self) {
        for pid in [OTHER, LEADER] {
            self.run
                .harness
                .controller
                .handle_wait(WaitEvent::Exited(pid, 0));
            self.run.take_calls();
        }
    }

    /// Where the session stands, and what it published, logged and
    /// called since last asked.
    fn describe(&mut self) -> String {
        let run = self.run.describe();
        let snapshot = self.run.snapshot();
        let breakpoints = snapshot
            .breakpoints
            .iter()
            .map(|breakpoint| {
                let state = breakpoint
                    .pending_locations
                    .first()
                    .map_or("ready", |pending| short(&pending.reason));
                format!("b{} {state}", breakpoint.id)
            })
            .collect::<Vec<_>>()
            .join(", ");
        let logged = std::mem::take(&mut *self.agent.log.lock().expect("the log"));
        let alarm = if self.run.harness.controller.agent_deadline().is_some() {
            " alarm"
        } else {
            ""
        };

        // How a report hook's hit is stepped over is the run-phase table's;
        // this one names it.
        let run = run
            .replace(
                &format!("set_registers {OTHER} rip={REPORT:#x}, request_stop {LEADER}"),
                "report hit",
            )
            .replace(
                &format!("remove_site {REPORT:#x}, step {OTHER}, reinstall_site {REPORT:#x}"),
                "step over report",
            );

        format!("{run} ({breakpoints}) <{}>{alarm}", logged.join(", "))
    }
}

/// A pending location's reason, named by what it says.
fn short(reason: &str) -> &str {
    [
        ("acknowledge its configuration", "unacknowledged"),
        ("initialized interpreter thread", "no thread"),
        ("acknowledgement timed out", "timed out"),
        ("bootstrap did not take", "bootstrap timed out"),
        ("scripted failure", "failed"),
        ("scripted refusal", "refused"),
    ]
    .into_iter()
    .find_map(|(says, name)| reason.contains(says).then_some(name))
    .unwrap_or(reason)
}

/// Builds the phase `name`: a session with one breakpoint at line 1,
/// stopped at a pause unless the name says it runs.
fn phase(name: &str) -> Session {
    let mut session = Session::new();
    let built = |said: String, step: &str| {
        assert!(
            said == "ok" || said.ends_with('-'),
            "{name}: {step} said {said}"
        );
    };

    match name {
        "uninstalled" => {}
        "waiting" | "waiting, running" => {
            *session.agent.thread.lock().expect("the script") = None;
            built(session.add(1)(), "add");
            if name == "waiting, running" {
                built(session.resume()(), "continue");
            }
        }
        "source unread" => built(session.add(1)(), "add"),
        "source running" | "source timed out" => {
            built(session.add(1)(), "add");
            built(session.resume()(), "continue");
            if name == "source timed out" {
                built(session.alarm(), "alarm");
            }
        }
        "ready" | "ready, running" | "idle" | "command unread" | "command running"
        | "command timed out" => {
            built(session.add(1)(), "add");
            built(session.resume()(), "continue");
            built(session.agent_reports(None, false), "ack");
            if name == "idle" {
                built(session.remove(1)(), "remove");
                built(session.agent_reports(None, false), "ack");
            }
            if name != "ready, running" {
                built(session.pause()(), "pause");
            }
            if name.starts_with("command") {
                built(session.add(2)(), "add");
            }
            if name == "command running" || name == "command timed out" {
                built(session.resume()(), "continue");
            }
            if name == "command timed out" {
                built(session.alarm(), "alarm");
            }
        }
        "refused by the agent" => {
            built(session.add(1)(), "add");
            built(
                session.agent_reports(Some("scripted failure"), false),
                "failure",
            );
        }
        "bootstrap unread" | "bootstrap timed out" => {
            *session.agent.bootstrap.lock().expect("the script") = true;
            session
                .run
                .harness
                .controller
                .session
                .inferior_mut()
                .expect("the process")
                .python_launch = Some(super::super::python_launch::PythonLaunch::create());
            built(session.add(1)(), "add");
            if name == "bootstrap timed out" {
                built(session.resume()(), "continue");
                built(session.alarm(), "alarm");
            }
        }
        "refused before any write" => {
            *session.agent.refusal.lock().expect("the script") = Some("scripted refusal".into());
            built(session.add(1)(), "add");
        }
        _ => unreachable!("no phase {name}"),
    }

    session.forget();
    session
}

const PHASES: [&str; 16] = [
    "uninstalled",
    "waiting",
    "waiting, running",
    "source unread",
    "source running",
    "source timed out",
    "ready",
    "ready, running",
    "idle",
    "command unread",
    "command running",
    "command timed out",
    "refused by the agent",
    "refused before any write",
    "bootstrap unread",
    "bootstrap timed out",
];

const EVENTS: [&str; 13] = [
    "add",
    "refused add",
    "remove",
    "continue",
    "continue leader",
    "pause",
    "ack",
    "stale ack",
    "failure",
    "entry",
    "alarm",
    "detach",
    "exit",
];

/// One event in one phase: what it was answered, and where the session
/// settles.
fn outcome(name: &str, event: &str) -> Option<String> {
    let mut session = phase(name);
    let stopped = session.run.is_stopped();
    let entry_trapped = session
        .run
        .harness
        .inferior()
        .breakpoints
        .contains_key(&VirtualAddress::new(ENTRY));

    let said = match (name, event) {
        // With no agent these are plain run control, which the run-phase
        // table holds, or a breakpoint that is not there.
        ("uninstalled", "remove" | "continue" | "alarm") | ("idle", "remove") => return None,
        (_, "add") => session.add(3)(),
        (_, "refused add") => {
            *session.agent.refusal.lock().expect("the script") = Some("scripted refusal".into());
            session.add(3)()
        }
        (_, "remove") => session.remove(1)(),
        (_, "continue") if stopped => session.resume()(),
        (_, "continue leader") if stopped => session.resume_leader()(),
        (_, "pause") if !stopped => session.pause()(),
        (_, "ack") => session.agent_reports(None, false),
        (_, "stale ack") => session.agent_reports(None, true),
        (_, "failure") => session.agent_reports(Some("scripted failure"), false),
        (_, "entry") if entry_trapped => {
            *session.agent.thread.lock().expect("the script") = Some(debug_thread_id(OTHER));
            session.report(ENTRY, HookEvent::AgentEntry)
        }
        (_, "alarm") => session.alarm(),
        (_, "detach") => session.detach()(),
        (_, "exit") => {
            session.exit();
            "-".to_owned()
        }
        _ => return None,
    };

    let row = format!("{name} / {event}: {said} -> {}", session.describe());
    Some(
        [
            "runtime agent acknowledgement timed out; configuration remains unconfirmed",
            "the Python launch bootstrap did not take the agent's request; it runs only where \
             the interpreter imports sitecustomize from PYTHONPATH, which -I, -E, and -S prevent",
        ]
        .into_iter()
        .fold(row, |row, reason| row.replace(reason, short(reason))),
    )
}

/// Events that vary another, each with the one it varies. A variant's row
/// is written only in phases where it settles differently, so a phase
/// whose variant starts to differ, or stops differing, fails the table.
const VARIANTS: [(&str, &str); 3] = [
    ("refused add", "add"),
    ("stale ack", "ack"),
    ("failure", "ack"),
];

/// Each request, report and alarm, in each phase of an agent's
/// installation, is answered and ends as it always has: which stop is
/// published, which breakpoints are pending and why, what the agent and
/// its files are asked, which traps and writes the controller makes, and
/// whether an acknowledgement's alarm is set.
#[test]
fn each_agent_event_settles_the_same_in_each_phase() {
    let mut actual = Vec::new();
    for name in PHASES {
        let rows: Vec<(&str, String)> = EVENTS
            .iter()
            .filter_map(|event| Some((*event, outcome(name, event)?)))
            .collect();
        // What follows the event's name: how it was answered and settled.
        let settled = |event: &str| {
            rows.iter()
                .find(|(written, _)| *written == event)
                .and_then(|(_, row)| row.split_once(": ").map(|(_, rest)| rest.to_owned()))
        };
        for (event, row) in &rows {
            let same = VARIANTS
                .iter()
                .find(|(variant, _)| variant == event)
                .is_some_and(|(_, base)| settled(event) == settled(base));
            if !same {
                actual.push(row.clone());
            }
        }
    }

    let rows = actual.join("\n");
    assert_eq!(
        EXPECTED,
        actual.as_slice(),
        "the agent table changed; its rows are now:\n{rows}"
    );
}

/// The table's rows, as [`outcome`] writes them.
const EXPECTED: &[&str] = &[
    "uninstalled / add: ok -> stopped Pause by 5000 [breakpoints] {write_word 5000 0x9000 0x1} (b1 unacknowledged) <create files in 5001, file 1, inject /tmp/agent/agent-1.py on 5001 [0]>",
    "uninstalled / refused add: ok -> stopped Pause by 5000 [breakpoints] {} (b1 refused) <create files in 5001, file 1, inject refused, drop file 1>",
    "uninstalled / continue leader: ok -> running e2 [continued e2] {continue 5000 None} () <>",
    "uninstalled / ack: continue ok, - -> running e2 [continued e2] {continue 5000 None, continue 5001 None, report hit, step over report, continue 5000 None, continue 5001 None} () <>",
    "uninstalled / detach: ok -> not running [] {remove_site 0x73000100, detach 5000 None, detach 5001 None} () <>",
    "uninstalled / exit: - -> not running [5001 exited] {} () <>",
    "waiting / add: ok -> stopped Pause by 5000 [breakpoints] {} (b1 no thread, b2 no thread) <>",
    "waiting / remove: ok -> stopped Pause by 5000 [breakpoints] {remove_site 0x60} () <>",
    "waiting / continue: ok -> running e2 [continued e2] {continue 5000 None, continue 5001 None} (b1 no thread) <>",
    "waiting / continue leader: ok -> running e2 [continued e2] {continue 5000 None} (b1 no thread) <>",
    "waiting / ack: continue ok, - -> running e2 [continued e2] {continue 5000 None, continue 5001 None, report hit, step over report, continue 5000 None, continue 5001 None} (b1 no thread) <>",
    "waiting / entry: continue ok, - -> running e2 [continued e2, breakpoints] {continue 5000 None, continue 5001 None, set_registers 5001 rip=0x60, request_stop 5000, write_word 5000 0x9000 0x1, remove_site 0x60, continue 5000 None, continue 5001 None} (b1 unacknowledged) <create files in 5001, file 1, inject /tmp/agent/agent-1.py on 5001 [0]> alarm",
    "waiting / alarm: - -> stopped Pause by 5000 [] {} (b1 no thread) <>",
    "waiting / detach: ok -> not running [] {remove_site 0x60, remove_site 0x73000100, detach 5000 None, detach 5001 None} (b1 ready) <>",
    "waiting / exit: - -> not running [5001 exited] {} (b1 ready) <>",
    "waiting, running / add: ok -> running e2 [breakpoints] {request_stop 5000, request_stop 5001, continue 5000 None, continue 5001 None} (b1 no thread, b2 no thread) <>",
    "waiting, running / remove: ok -> running e2 [breakpoints] {request_stop 5000, request_stop 5001, remove_site 0x60, continue 5000 None, continue 5001 None} () <>",
    "waiting, running / pause: ok -> stopped Pause by 5000 [stopped e2 Pause by 5000] {request_stop 5000, request_stop 5001} (b1 no thread) <>",
    "waiting, running / ack: - -> running e2 [] {report hit, step over report, continue 5000 None, continue 5001 None} (b1 no thread) <>",
    "waiting, running / entry: - -> running e2 [breakpoints] {set_registers 5001 rip=0x60, request_stop 5000, write_word 5000 0x9000 0x1, remove_site 0x60, continue 5000 None, continue 5001 None} (b1 unacknowledged) <create files in 5001, file 1, inject /tmp/agent/agent-1.py on 5001 [0]> alarm",
    "waiting, running / alarm: - -> running e2 [] {} (b1 no thread) <>",
    "waiting, running / detach: ok -> not running [] {interrupt 5000, interrupt 5001, remove_site 0x60, remove_site 0x73000100, detach 5000 None, detach 5001 None} (b1 ready) <>",
    "waiting, running / exit: - -> not running [5001 exited] {} (b1 ready) <>",
    "source unread / add: ok -> stopped Pause by 5000 [breakpoints] {} (b1 unacknowledged, b2 unacknowledged) <>",
    "source unread / remove: ok -> stopped Pause by 5000 [breakpoints] {} () <>",
    "source unread / continue: ok -> running e2 [continued e2] {continue 5000 None, continue 5001 None} (b1 unacknowledged) <> alarm",
    "source unread / continue leader: RuntimeResumeUnsupported(\"the outstanding source request belongs to a different interpreter thread\") -> stopped Pause by 5000 [] {} (b1 unacknowledged) <>",
    "source unread / ack: continue ok, - -> running e2 [continued e2, breakpoints] {continue 5000 None, continue 5001 None, report hit, step over report, continue 5000 None, continue 5001 None} (b1 ready) <retire Acknowledged: retired, drop file 1>",
    "source unread / stale ack: continue ok, - -> running e2 [continued e2] {continue 5000 None, continue 5001 None, report hit, step over report, continue 5000 None, continue 5001 None} (b1 unacknowledged) <> alarm",
    "source unread / failure: continue ok, - -> stopped RuntimeEvent { description: \"scripted failure\" } by 5001 [continued e2, breakpoints, stopped e2 RuntimeEvent { description: \"scripted failure\" } by 5001] {continue 5000 None, continue 5001 None, report hit} (b1 failed) <retire Acknowledged: retired, drop file 1>",
    "source unread / alarm: - -> stopped Pause by 5000 [] {} (b1 unacknowledged) <>",
    "source unread / detach: ok -> not running [] {remove_site 0x73000100, detach 5000 None, detach 5001 None} (b1 ready) <retire Cancel: withdrawn, drop file 1, close files>",
    "source unread / exit: - -> not running [5001 exited] {} (b1 ready) <remove files>",
    "source running / add: ok -> running e2 [breakpoints] {request_stop 5000, request_stop 5001, continue 5000 None, continue 5001 None} (b1 unacknowledged, b2 unacknowledged) <> alarm",
    "source running / remove: ok -> running e2 [breakpoints] {request_stop 5000, request_stop 5001, continue 5000 None, continue 5001 None} () <> alarm",
    "source running / pause: ok -> stopped Pause by 5000 [stopped e2 Pause by 5000] {request_stop 5000, request_stop 5001} (b1 unacknowledged) <>",
    "source running / ack: - -> running e2 [breakpoints] {report hit, step over report, continue 5000 None, continue 5001 None} (b1 ready) <retire Acknowledged: retired, drop file 1>",
    "source running / stale ack: - -> running e2 [] {report hit, step over report, continue 5000 None, continue 5001 None} (b1 unacknowledged) <> alarm",
    "source running / failure: - -> stopped RuntimeEvent { description: \"scripted failure\" } by 5001 [breakpoints, stopped e2 RuntimeEvent { description: \"scripted failure\" } by 5001] {report hit} (b1 failed) <retire Acknowledged: retired, drop file 1>",
    "source running / alarm: - -> stopped RuntimeEvent { description: \"timed out\" } by 5000 [breakpoints, stopped e2 RuntimeEvent { description: \"timed out\" } by 5000] {request_stop 5000, request_stop 5001} (b1 timed out) <>",
    "source running / detach: ok -> not running [] {interrupt 5000, interrupt 5001, remove_site 0x73000100, detach 5000 None, detach 5001 None} (b1 ready) <retire Cancel: withdrawn, drop file 1, close files>",
    "source running / exit: - -> not running [5001 exited] {} (b1 ready) <remove files>",
    "source timed out / add: ok -> stopped RuntimeEvent { description: \"timed out\" } by 5000 [breakpoints] {} (b1 timed out, b2 timed out) <>",
    "source timed out / remove: ok -> stopped RuntimeEvent { description: \"timed out\" } by 5000 [breakpoints] {} () <>",
    "source timed out / continue: ok -> running e3 [continued e3] {continue 5000 None, continue 5001 None} (b1 timed out) <>",
    "source timed out / continue leader: RuntimeResumeUnsupported(\"the outstanding source request belongs to a different interpreter thread\") -> stopped RuntimeEvent { description: \"timed out\" } by 5000 [] {} (b1 timed out) <>",
    "source timed out / ack: continue ok, - -> running e3 [continued e3, breakpoints] {continue 5000 None, continue 5001 None, report hit, step over report, continue 5000 None, continue 5001 None} (b1 ready) <retire Acknowledged: retired, drop file 1>",
    "source timed out / stale ack: continue ok, - -> running e3 [continued e3] {continue 5000 None, continue 5001 None, report hit, step over report, continue 5000 None, continue 5001 None} (b1 timed out) <>",
    "source timed out / failure: continue ok, - -> stopped RuntimeEvent { description: \"scripted failure\" } by 5001 [continued e3, breakpoints, stopped e3 RuntimeEvent { description: \"scripted failure\" } by 5001] {continue 5000 None, continue 5001 None, report hit} (b1 failed) <retire Acknowledged: retired, drop file 1>",
    "source timed out / alarm: - -> stopped RuntimeEvent { description: \"timed out\" } by 5000 [] {} (b1 timed out) <>",
    "source timed out / detach: ok -> not running [] {remove_site 0x73000100, detach 5000 None, detach 5001 None} (b1 ready) <retire Cancel: withdrawn, drop file 1, close files>",
    "source timed out / exit: - -> not running [5001 exited] {} (b1 ready) <remove files>",
    "ready / add: ok -> stopped Pause by 5000 [breakpoints] {write_word 5000 0x73000200 0x2} (b1 ready, b2 unacknowledged) <command 0->2 [0 1]>",
    "ready / refused add: ok -> stopped Pause by 5000 [breakpoints] {} (b1 ready, b2 refused) <command refused>",
    "ready / remove: ok -> stopped Pause by 5000 [breakpoints] {write_word 5000 0x73000200 0x2} () <command 0->2 []>",
    "ready / continue: ok -> running e3 [continued e3] {continue 5000 None, continue 5001 None} (b1 ready) <>",
    "ready / continue leader: ok -> running e3 [continued e3] {continue 5000 None} (b1 ready) <>",
    "ready / ack: continue ok, - -> running e3 [continued e3] {continue 5000 None, continue 5001 None, report hit, step over report, continue 5000 None, continue 5001 None} (b1 ready) <>",
    "ready / failure: continue ok, - -> stopped RuntimeEvent { description: \"scripted failure\" } by 5001 [continued e3, breakpoints, stopped e3 RuntimeEvent { description: \"scripted failure\" } by 5001] {continue 5000 None, continue 5001 None, report hit} (b1 failed) <>",
    "ready / alarm: - -> stopped Pause by 5000 [] {} (b1 ready) <>",
    "ready / detach: ok -> not running [] {write_word 5000 0x73000200 0x2, remove_site 0x73000100, detach 5000 None, detach 5001 None} (b1 ready) <command 0->2 [], close files>",
    "ready / exit: - -> not running [5001 exited] {} (b1 ready) <remove files>",
    "ready, running / add: ok -> running e2 [breakpoints] {request_stop 5000, request_stop 5001, write_word 5000 0x73000200 0x2, continue 5000 None, continue 5001 None} (b1 ready, b2 unacknowledged) <command 0->2 [0 1]> alarm",
    "ready, running / refused add: ok -> running e2 [breakpoints] {request_stop 5000, request_stop 5001, continue 5000 None, continue 5001 None} (b1 ready, b2 refused) <command refused>",
    "ready, running / remove: ok -> running e2 [breakpoints] {request_stop 5000, request_stop 5001, write_word 5000 0x73000200 0x2, continue 5000 None, continue 5001 None} () <command 0->2 []> alarm",
    "ready, running / pause: ok -> stopped Pause by 5000 [stopped e2 Pause by 5000] {request_stop 5000, request_stop 5001} (b1 ready) <>",
    "ready, running / ack: - -> running e2 [] {report hit, step over report, continue 5000 None, continue 5001 None} (b1 ready) <>",
    "ready, running / failure: - -> stopped RuntimeEvent { description: \"scripted failure\" } by 5001 [breakpoints, stopped e2 RuntimeEvent { description: \"scripted failure\" } by 5001] {report hit} (b1 failed) <>",
    "ready, running / alarm: - -> running e2 [] {} (b1 ready) <>",
    "ready, running / detach: ok -> not running [] {interrupt 5000, interrupt 5001, write_word 5000 0x73000200 0x2, remove_site 0x73000100, detach 5000 None, detach 5001 None} (b1 ready) <command 0->2 [], close files>",
    "ready, running / exit: - -> not running [5001 exited] {} (b1 ready) <remove files>",
    "idle / add: ok -> stopped Pause by 5000 [breakpoints] {write_word 5000 0x9000 0x1} (b2 unacknowledged) <anchor 5001, file 3, inject /tmp/agent/agent-3.py on 5001 [1]>",
    "idle / refused add: ok -> stopped Pause by 5000 [breakpoints] {} (b2 refused) <anchor 5001, file 3, inject refused, drop file 3>",
    "idle / continue: ok -> running e3 [continued e3] {continue 5000 None, continue 5001 None} () <>",
    "idle / continue leader: ok -> running e3 [continued e3] {continue 5000 None} () <>",
    "idle / ack: continue ok, - -> running e3 [continued e3] {continue 5000 None, continue 5001 None, report hit, step over report, continue 5000 None, continue 5001 None} () <>",
    "idle / failure: continue ok, - -> stopped RuntimeEvent { description: \"scripted failure\" } by 5001 [continued e3, breakpoints, stopped e3 RuntimeEvent { description: \"scripted failure\" } by 5001] {continue 5000 None, continue 5001 None, report hit} () <>",
    "idle / alarm: - -> stopped Pause by 5000 [] {} () <>",
    "idle / detach: ok -> not running [] {write_word 5000 0x73000200 0x3, remove_site 0x73000100, detach 5000 None, detach 5001 None} () <command 2->3 [], close files>",
    "idle / exit: - -> not running [5001 exited] {} () <remove files>",
    "command unread / add: ok -> stopped Pause by 5000 [breakpoints] {write_word 5000 0x73000200 0x3} (b1 ready, b2 unacknowledged, b3 unacknowledged) <command 2->3 [0 1 2]>",
    "command unread / refused add: ok -> stopped Pause by 5000 [breakpoints] {} (b1 ready, b2 refused, b3 refused) <command refused>",
    "command unread / remove: ok -> stopped Pause by 5000 [breakpoints] {write_word 5000 0x73000200 0x3} (b2 unacknowledged) <command 2->3 [1]>",
    "command unread / continue: ok -> running e3 [continued e3] {continue 5000 None, continue 5001 None} (b1 ready, b2 unacknowledged) <> alarm",
    "command unread / continue leader: ok -> running e3 [continued e3] {continue 5000 None} (b1 ready, b2 unacknowledged) <> alarm",
    "command unread / ack: continue ok, - -> running e3 [continued e3, breakpoints] {continue 5000 None, continue 5001 None, report hit, step over report, continue 5000 None, continue 5001 None} (b1 ready, b2 ready) <>",
    "command unread / stale ack: continue ok, - -> running e3 [continued e3] {continue 5000 None, continue 5001 None, report hit, step over report, continue 5000 None, continue 5001 None} (b1 ready, b2 unacknowledged) <> alarm",
    "command unread / failure: continue ok, - -> stopped RuntimeEvent { description: \"scripted failure\" } by 5001 [continued e3, breakpoints, stopped e3 RuntimeEvent { description: \"scripted failure\" } by 5001] {continue 5000 None, continue 5001 None, report hit} (b1 failed, b2 failed) <>",
    "command unread / alarm: - -> stopped Pause by 5000 [] {} (b1 ready, b2 unacknowledged) <>",
    "command unread / detach: ok -> not running [] {write_word 5000 0x73000200 0x3, remove_site 0x73000100, detach 5000 None, detach 5001 None} (b1 ready, b2 ready) <command 2->3 [], close files>",
    "command unread / exit: - -> not running [5001 exited] {} (b1 ready, b2 ready) <remove files>",
    "command running / add: ok -> running e3 [breakpoints] {request_stop 5000, request_stop 5001, continue 5000 None, continue 5001 None} (b1 ready, b2 unacknowledged, b3 unacknowledged) <> alarm",
    "command running / remove: ok -> running e3 [breakpoints] {request_stop 5000, request_stop 5001, continue 5000 None, continue 5001 None} (b2 unacknowledged) <> alarm",
    "command running / pause: ok -> stopped Pause by 5000 [stopped e3 Pause by 5000] {request_stop 5000, request_stop 5001} (b1 ready, b2 unacknowledged) <>",
    "command running / ack: - -> running e3 [breakpoints] {report hit, step over report, continue 5000 None, continue 5001 None} (b1 ready, b2 ready) <>",
    "command running / stale ack: - -> running e3 [] {report hit, step over report, continue 5000 None, continue 5001 None} (b1 ready, b2 unacknowledged) <> alarm",
    "command running / failure: - -> stopped RuntimeEvent { description: \"scripted failure\" } by 5001 [breakpoints, stopped e3 RuntimeEvent { description: \"scripted failure\" } by 5001] {report hit} (b1 failed, b2 failed) <>",
    "command running / alarm: - -> stopped RuntimeEvent { description: \"timed out\" } by 5000 [breakpoints, stopped e3 RuntimeEvent { description: \"timed out\" } by 5000] {request_stop 5000, request_stop 5001} (b1 timed out, b2 timed out) <>",
    "command running / detach: ok -> not running [] {interrupt 5000, interrupt 5001, write_word 5000 0x73000200 0x3, remove_site 0x73000100, detach 5000 None, detach 5001 None} (b1 ready, b2 ready) <command 2->3 [], close files>",
    "command running / exit: - -> not running [5001 exited] {} (b1 ready, b2 ready) <remove files>",
    "command timed out / add: ok -> stopped RuntimeEvent { description: \"timed out\" } by 5000 [breakpoints] {} (b1 timed out, b2 timed out, b3 timed out) <>",
    "command timed out / remove: ok -> stopped RuntimeEvent { description: \"timed out\" } by 5000 [breakpoints] {} (b2 timed out) <>",
    "command timed out / continue: ok -> running e4 [continued e4] {continue 5000 None, continue 5001 None} (b1 timed out, b2 timed out) <>",
    "command timed out / continue leader: ok -> running e4 [continued e4] {continue 5000 None} (b1 timed out, b2 timed out) <>",
    "command timed out / ack: continue ok, - -> running e4 [continued e4, breakpoints] {continue 5000 None, continue 5001 None, report hit, step over report, continue 5000 None, continue 5001 None} (b1 ready, b2 ready) <>",
    "command timed out / stale ack: continue ok, - -> running e4 [continued e4] {continue 5000 None, continue 5001 None, report hit, step over report, continue 5000 None, continue 5001 None} (b1 timed out, b2 timed out) <>",
    "command timed out / failure: continue ok, - -> stopped RuntimeEvent { description: \"scripted failure\" } by 5001 [continued e4, breakpoints, stopped e4 RuntimeEvent { description: \"scripted failure\" } by 5001] {continue 5000 None, continue 5001 None, report hit} (b1 failed, b2 failed) <>",
    "command timed out / alarm: - -> stopped RuntimeEvent { description: \"timed out\" } by 5000 [] {} (b1 timed out, b2 timed out) <>",
    "command timed out / detach: ok -> not running [] {write_word 5000 0x73000200 0x3, remove_site 0x73000100, detach 5000 None, detach 5001 None} (b1 ready, b2 ready) <command 2->3 [], close files>",
    "command timed out / exit: - -> not running [5001 exited] {} (b1 ready, b2 ready) <remove files>",
    "refused by the agent / add: ok -> stopped RuntimeEvent { description: \"scripted failure\" } by 5001 [breakpoints] {write_word 5000 0x9000 0x1} (b1 unacknowledged, b2 unacknowledged) <anchor 5001, file 2, inject /tmp/agent/agent-2.py on 5001 [0 1]>",
    "refused by the agent / refused add: ok -> stopped RuntimeEvent { description: \"scripted failure\" } by 5001 [breakpoints] {} (b1 refused, b2 refused) <anchor 5001, file 2, inject refused, drop file 2>",
    "refused by the agent / remove: ok -> stopped RuntimeEvent { description: \"scripted failure\" } by 5001 [breakpoints] {write_word 5000 0x9000 0x1} () <anchor 5001, file 2, inject /tmp/agent/agent-2.py on 5001 []>",
    "refused by the agent / continue: ok -> running e3 [continued e3] {step over report, continue 5000 None, continue 5001 None} (b1 failed) <>",
    "refused by the agent / continue leader: ok -> running e3 [continued e3] {continue 5000 None} (b1 failed) <>",
    "refused by the agent / ack: continue ok, - -> running e3 [continued e3] {step over report, continue 5000 None, continue 5001 None, report hit, step over report, continue 5000 None, continue 5001 None} (b1 failed) <>",
    "refused by the agent / failure: continue ok, - -> stopped RuntimeEvent { description: \"scripted failure\" } by 5001 [continued e3, stopped e3 RuntimeEvent { description: \"scripted failure\" } by 5001] {step over report, continue 5000 None, continue 5001 None, report hit} (b1 failed) <>",
    "refused by the agent / alarm: - -> stopped RuntimeEvent { description: \"scripted failure\" } by 5001 [] {} (b1 failed) <>",
    "refused by the agent / detach: ok -> not running [] {write_word 5000 0x73000200 0x2, remove_site 0x73000100, detach 5000 None, detach 5001 None} (b1 ready) <command 0->2 [], close files>",
    "refused by the agent / exit: - -> not running [5001 exited] {} (b1 ready) <remove files>",
    "refused before any write / add: ok -> stopped Pause by 5000 [breakpoints] {} (b1 refused, b2 refused) <anchor 5001, file 1, inject refused, drop file 1>",
    "refused before any write / remove: ok -> stopped Pause by 5000 [breakpoints] {} () <anchor 5001, file 1, inject refused, drop file 1>",
    "refused before any write / continue: ok -> running e2 [continued e2] {continue 5000 None, continue 5001 None} (b1 refused) <>",
    "refused before any write / continue leader: ok -> running e2 [continued e2] {continue 5000 None} (b1 refused) <>",
    "refused before any write / ack: continue ok, - -> running e2 [continued e2] {continue 5000 None, continue 5001 None, report hit, step over report, continue 5000 None, continue 5001 None} (b1 refused) <>",
    "refused before any write / alarm: - -> stopped Pause by 5000 [] {} (b1 refused) <>",
    "refused before any write / detach: ok -> not running [] {remove_site 0x73000100, detach 5000 None, detach 5001 None} (b1 ready) <close files>",
    "refused before any write / exit: - -> not running [5001 exited] {} (b1 ready) <remove files>",
    "bootstrap unread / add: ok -> stopped Pause by 5000 [breakpoints] {} (b1 unacknowledged, b2 unacknowledged) <>",
    "bootstrap unread / remove: ok -> stopped Pause by 5000 [breakpoints] {} () <>",
    "bootstrap unread / continue: ok -> running e2 [continued e2] {continue 5000 None, continue 5001 None} (b1 unacknowledged) <> alarm",
    "bootstrap unread / continue leader: RuntimeResumeUnsupported(\"the outstanding source request belongs to a different interpreter thread\") -> stopped Pause by 5000 [] {} (b1 unacknowledged) <>",
    "bootstrap unread / ack: continue ok, - -> running e2 [continued e2, breakpoints] {continue 5000 None, continue 5001 None, report hit, step over report, continue 5000 None, continue 5001 None} (b1 ready) <drop file 1>",
    "bootstrap unread / stale ack: continue ok, - -> running e2 [continued e2] {continue 5000 None, continue 5001 None, report hit, step over report, continue 5000 None, continue 5001 None} (b1 unacknowledged) <> alarm",
    "bootstrap unread / failure: continue ok, - -> stopped RuntimeEvent { description: \"scripted failure\" } by 5001 [continued e2, breakpoints, stopped e2 RuntimeEvent { description: \"scripted failure\" } by 5001] {continue 5000 None, continue 5001 None, report hit} (b1 failed) <drop file 1>",
    "bootstrap unread / alarm: - -> stopped Pause by 5000 [] {} (b1 unacknowledged) <>",
    "bootstrap unread / detach: ok -> not running [] {remove_site 0x73000100, detach 5000 None, detach 5001 None} (b1 ready) <drop file 1, close files>",
    "bootstrap unread / exit: - -> not running [5001 exited] {} (b1 ready) <remove files>",
    "bootstrap timed out / add: ok -> stopped RuntimeEvent { description: \"bootstrap timed out\" } by 5000 [breakpoints] {} (b1 bootstrap timed out, b2 bootstrap timed out) <>",
    "bootstrap timed out / remove: ok -> stopped RuntimeEvent { description: \"bootstrap timed out\" } by 5000 [breakpoints] {} () <>",
    "bootstrap timed out / continue: ok -> running e3 [continued e3] {continue 5000 None, continue 5001 None} (b1 bootstrap timed out) <>",
    "bootstrap timed out / continue leader: RuntimeResumeUnsupported(\"the outstanding source request belongs to a different interpreter thread\") -> stopped RuntimeEvent { description: \"bootstrap timed out\" } by 5000 [] {} (b1 bootstrap timed out) <>",
    "bootstrap timed out / ack: continue ok, - -> running e3 [continued e3, breakpoints] {continue 5000 None, continue 5001 None, report hit, step over report, continue 5000 None, continue 5001 None} (b1 ready) <drop file 1>",
    "bootstrap timed out / stale ack: continue ok, - -> running e3 [continued e3] {continue 5000 None, continue 5001 None, report hit, step over report, continue 5000 None, continue 5001 None} (b1 bootstrap timed out) <>",
    "bootstrap timed out / failure: continue ok, - -> stopped RuntimeEvent { description: \"scripted failure\" } by 5001 [continued e3, breakpoints, stopped e3 RuntimeEvent { description: \"scripted failure\" } by 5001] {continue 5000 None, continue 5001 None, report hit} (b1 failed) <drop file 1>",
    "bootstrap timed out / alarm: - -> stopped RuntimeEvent { description: \"bootstrap timed out\" } by 5000 [] {} (b1 bootstrap timed out) <>",
    "bootstrap timed out / detach: ok -> not running [] {remove_site 0x73000100, detach 5000 None, detach 5001 None} (b1 ready) <drop file 1, close files>",
    "bootstrap timed out / exit: - -> not running [5001 exited] {} (b1 ready) <remove files>",
];
