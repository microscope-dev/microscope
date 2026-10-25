//! Steps V8 runs for microscope: `stepInto`, `stepOver`, and `stepOut`,
//! owned by the isolate they began in, which go on through Node's own
//! scripts, skipped files (F19), and generated code a source map leads to
//! no source. A step from a native stop of the main thread first pauses
//! V8 at its next statement.

use std::collections::BTreeMap;
use std::sync::Arc;

use nix::unistd::Pid;

use super::super::Controller;
use super::super::cdp::{Connection, Session};
use super::super::debug_thread_id;
use super::super::native::{InspectionOps, LinuxTraceOps};
use super::interrupts::ask_pause;
use super::pauses::PauseKey;
use super::{Isolate, send};
use crate::runtime_model::inspector_protocol;
use crate::{RuntimeId, StopReason};

/// A step V8 runs for microscope.
#[derive(Debug)]
pub(super) struct Stepping {
    pub(super) number: u64,
    pub(super) kind: crate::StepKind,

    /// The isolate that runs it, where it began: a pause in another does
    /// not end it.
    pub(super) session: Session,

    /// How many frames the paused thread had where the step began; `None`
    /// for a step begun where V8 did not hold the thread, which ends at
    /// V8's next pause outside skipped scripts.
    pub(super) depth: Option<usize>,

    /// The command the step continues with as its thread resumes: the
    /// step's own, then `stepOut` from where a breakpoint whose condition
    /// failed paused it deeper than it ends.
    pub(super) command: &'static str,

    /// Whether V8 was sent the command, so that its next pause ends it.
    pub(super) sent: bool,

    /// How many pauses in skipped scripts the step has gone on from.
    pub(super) skipped: u32,
}

impl<P: LinuxTraceOps> Controller<P> {
    /// Whether V8 goes on from the pause that `params` describes in the
    /// isolate `session` reaches, which no breakpoint stopped, with the
    /// program left running: a step that pauses in a skipped script goes
    /// on into the program's next statement, and an exception caught or a
    /// `debugger` statement there is ignored.
    pub(super) fn step_goes_on(
        &mut self,
        runtime: RuntimeId,
        session: &Session,
        params: &serde_json::Value,
        statement: bool,
    ) -> bool {
        let (skips, enter) = (self.step_skips.clone(), self.step_into_runtime);
        let Some(inspectors) = self
            .inferior
            .as_mut()
            .map(|inferior| &mut inferior.inspectors)
        else {
            return false;
        };
        let (Some(inspector), maps) = (inspectors.records.get_mut(&runtime), &inspectors.maps)
        else {
            return false;
        };
        let location = &params["callFrames"][0]["location"];
        let top = location["scriptId"]
            .as_str()
            .and_then(|id| inspector.isolates.get(session)?.scripts.get(id).cloned());
        let Some(link) = inspector.connection.as_ref() else {
            return false;
        };
        let original = top.as_ref().and_then(|url| {
            let map = maps.get(&*inspector_protocol::script_path(url))?;
            map.map().ok()?;
            let generated = crate::RecordedPlace {
                path: Arc::clone(url),
                line: u32::try_from(location["lineNumber"].as_u64()? + 1).ok()?,
                column: u32::try_from(location["columnNumber"].as_u64()? + 1).ok()?,
            };
            Some(map.original(&generated))
        });
        // Generated code with no source is no statement of the program.
        let unmapped = matches!(original, Some(Err(_)));
        let skip = match (&top, original) {
            (None, _) => false,
            (Some(_), Some(Err(_))) => true,
            (Some(_), Some(Ok(original))) => skipped(&original.path, &skips, enter),
            (Some(url), None) => skipped(url, &skips, enter),
        };
        if !skip {
            return false;
        }
        let step = inspector
            .step
            .as_mut()
            .filter(|step| step.sent && step.session == *session);
        let command = match (params["reason"].as_str(), step) {
            // A step from a native stop pauses for no reason V8 names, and
            // one into a script just loaded also at its sentinel.
            (Some("step" | "other" | "ambiguous"), Some(step)) if step.skipped < MAX_SKIPPED => {
                step.skipped += 1;
                // Out of generated code by the step's own kind, and out of
                // Node's into the program's next statement.
                if unmapped && step.depth.is_some() {
                    step.command
                } else {
                    inspector_protocol::STEP_INTO
                }
            }
            // An exception caught in code steps pass through is that
            // code's own business, as Node's module loader's are.
            (Some("exception" | "promiseRejection"), step)
                if params["data"]["uncaught"] != true =>
            {
                step.map_or(inspector_protocol::RESUME, |step| step.command)
            }
            // So is a `debugger` statement there.
            (Some("other"), None) if statement => inspector_protocol::RESUME,
            _ => return false,
        };
        send(&link.connection, session, command, serde_json::json!({})).is_ok()
    }

    /// Whether a step V8 was told to run runs in the isolate `session`
    /// reaches.
    pub(super) fn steps_in(&self, runtime: RuntimeId, session: &Session) -> bool {
        self.inferior
            .as_ref()
            .and_then(|inferior| inferior.inspectors.records.get(&runtime)?.step.as_ref())
            .is_some_and(|step| step.sent && step.session == *session)
    }

    /// Why a pause of `pid` with `depth` frames, in the isolate `key`
    /// names, that no breakpoint stopped ends the step V8 runs there, if it
    /// does. One that does not end it continues it out of the call it
    /// paused in.
    pub(super) fn inspector_step_end(
        &mut self,
        key: &PauseKey,
        pid: Pid,
        depth: usize,
    ) -> Option<StopReason> {
        let (runtime, session) = key;
        let step = self
            .inferior
            .as_mut()?
            .inspectors
            .records
            .get_mut(runtime)?
            .step
            .as_mut()
            .filter(|step| step.sent && step.session == *session)?;
        if !ends_step(step, depth) {
            step.command = inspector_protocol::STEP_OUT;
            step.sent = false;
            return None;
        }
        let number = step.number;
        let incomplete = (step.skipped >= MAX_SKIPPED).then(|| {
            Arc::from(format!(
                "the step went through {MAX_SKIPPED} statements of Node's own scripts and stopped there"
            ))
        });
        self.runtime_step_end(pid, *runtime, number, incomplete)
    }
}

impl<P: InspectionOps> Controller<P> {
    /// The runtime whose inspector runs a step of `kind` from frame
    /// `frame` of `pid`, the main isolate's thread, which V8 does not hold:
    /// into or over from native code with no lines, from which a native
    /// step would run to whatever native code outside has them. It runs to
    /// the program's next JavaScript statement instead. A step out keeps
    /// its native meaning.
    pub(in crate::backend::linux) fn javascript_step_from_native(
        &self,
        stop_id: crate::protocol::StopId,
        pid: Pid,
        frame: crate::StackFrameId,
        kind: crate::StepKind,
    ) -> Option<RuntimeId> {
        let inferior = self.inferior.as_ref()?;
        if !matches!(
            kind,
            crate::StepKind::IntoSource | crate::StepKind::OverSource
        ) || pid != inferior.tgid
            || self.inspector_holds(debug_thread_id(pid))
        {
            return None;
        }
        let runtime = inferior
            .inspectors
            .records
            .iter()
            .find(|(_, inspector)| inspector.isolates.contains_key(&Session::main()))
            .map(|(&runtime, _)| runtime)?;
        let trace = self
            .backtrace(stop_id, &super::super::frames::StackRoot::of_thread(pid))
            .ok()?;
        trace
            .frames
            .iter()
            .find(|candidate| candidate.id == frame)
            .filter(|frame| frame.source.is_none() && frame.recorded.is_none())
            .map(|_| runtime)
    }
}

/// Asks the main isolate to pause for the step from a native stop that
/// `step` is, if it is one not yet begun, as its thread resumes. The gate
/// learns the request it waits to see handed on.
pub(super) fn pause_for_step(
    connection: &Connection,
    isolate: &mut Isolate,
    step: Option<&mut Stepping>,
    gate: Option<&mut Gate>,
) {
    let Some(step) = step.filter(|step| !step.sent && step.depth.is_none()) else {
        return;
    };
    step.sent = true;
    if isolate.resuming {
        // The pause is sent once V8 has resumed; the gate learns it then.
        ask_pause(connection, &Session::main(), isolate);
        return;
    }
    let sent = send(
        connection,
        &Session::main(),
        inspector_protocol::PAUSE,
        serde_json::json!({}),
    );
    if sent.is_ok() {
        isolate.pause = super::PauseRequest::Sent;
    }
    if let Some(gate) = gate {
        gate.sent(sent.ok());
    }
}

/// The main thread of a step from a native stop, held stopped until V8 is
/// sure to take the step's `Debugger.pause` before it runs on.
///
/// The main thread takes a message only at its next interrupt check, once
/// the inspector's I/O thread has handed the message on, and a pause from
/// there lands at the next check after that. Run at once, the main thread
/// may run statements past where the step should end before the I/O
/// thread does so, as it did under load. So it runs only once the I/O
/// thread has returned from the call that takes the pause's request.
#[derive(Debug, Clone)]
pub(super) struct Gate {
    /// The step the gate holds the main thread for.
    pub(super) number: u64,

    /// Where the I/O thread takes a message.
    entry: crate::VirtualAddress,
    phase: GatePhase,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum GatePhase {
    /// The pause is not yet sent: V8 has yet to resume.
    Unsent,
    Sent(Vec<u8>),

    /// Thread `pid` took the request at stack `stack`, and hands it on as
    /// it returns to `returns_to`.
    Receiving {
        pid: Pid,
        stack: u64,
        returns_to: crate::VirtualAddress,
    },
    Passed,
}

impl Gate {
    pub(super) const fn new(number: u64, entry: crate::VirtualAddress) -> Self {
        Self {
            number,
            entry,
            phase: GatePhase::Unsent,
        }
    }

    /// Notes the request `id` the step's pause was sent as, or that none
    /// was sent, which leaves nothing to wait for.
    pub(super) fn sent(&mut self, id: Option<super::super::cdp::RequestId>) {
        if self.phase != GatePhase::Unsent {
            return;
        }
        // The text `Connection::send` writes: its keys in serde_json's order.
        self.phase = id.map_or(GatePhase::Passed, |id| {
            let message = serde_json::json!({
                "id": id.number(),
                "method": inspector_protocol::PAUSE,
                "params": {},
            });
            GatePhase::Sent(message.to_string().into_bytes())
        });
    }

    /// Whether the gate still holds the main thread.
    pub(super) fn holds(&self) -> bool {
        self.phase != GatePhase::Passed
    }

    /// The traps the gate needs.
    pub(super) fn sites(&self) -> Vec<crate::VirtualAddress> {
        match self.phase {
            GatePhase::Unsent | GatePhase::Sent(_) => vec![self.entry],
            GatePhase::Receiving { returns_to, .. } => vec![self.entry, returns_to],
            GatePhase::Passed => Vec::new(),
        }
    }
}

impl<P: LinuxTraceOps> Controller<P> {
    /// Whether the gate of a step from a native stop holds thread `pid`.
    pub(in crate::backend::linux) fn step_gate_holds(&self, pid: Pid) -> bool {
        self.inferior.as_ref().is_some_and(|inferior| {
            pid == inferior.tgid
                && inferior.inspectors.records.values().any(|inspector| {
                    inspector.connection.is_some()
                        && inspector.gate.as_ref().is_some_and(Gate::holds)
                })
        })
    }

    /// Notes thread `pid` at a trap of a step's gate, at `address`: the
    /// I/O thread taking the step's pause, or returning once it handed it
    /// on, which lets the main thread run. Returns whether the trap is the
    /// gate's.
    pub(super) fn note_step_gate(
        &mut self,
        pid: Pid,
        address: crate::VirtualAddress,
    ) -> crate::Result<bool> {
        let Some(inferior) = self.inferior.as_ref() else {
            return Ok(false);
        };
        let Some((runtime, gate)) =
            inferior
                .inspectors
                .records
                .iter()
                .find_map(|(id, inspector)| {
                    let gate = inspector.gate.as_ref()?;
                    gate.sites().contains(&address).then(|| (*id, gate.clone()))
                })
        else {
            return Ok(false);
        };
        let registers = self.ptrace.registers(pid)?;
        let phase = match &gate.phase {
            GatePhase::Sent(message) if address == gate.entry => {
                let bound = self
                    .runtimes(inferior)
                    .into_iter()
                    .find(|bound| bound.id == runtime)
                    .ok_or(crate::Error::NotRunning)?;
                let matches = self
                    .with_runtime_stop(inferior, &bound, pid, |stop| {
                        bound.model.inspector_received_matches(
                            stop,
                            crate::VirtualAddress::new(registers.rdx),
                            message,
                        )
                    })
                    .map_err(crate::Error::RuntimeAgent)?;
                if !matches {
                    return Ok(true);
                }
                GatePhase::Receiving {
                    pid,
                    stack: registers.rsp,
                    returns_to: crate::VirtualAddress::new(
                        self.ptrace.read_word(pid, registers.rsp)?,
                    ),
                }
            }
            GatePhase::Receiving {
                pid: owner,
                stack,
                returns_to,
            } if pid == *owner
                && address == *returns_to
                && registers.rsp == stack.wrapping_add(8) =>
            {
                record!("the inspector's I/O thread handed the step's pause on");
                GatePhase::Passed
            }
            _ => return Ok(true),
        };
        if let Some(gate) = self
            .inferior
            .as_mut()
            .and_then(|inferior| inferior.inspectors.records.get_mut(&runtime))
            .and_then(|inspector| inspector.gate.as_mut())
        {
            gate.phase = phase;
        }
        // The trap this thread stopped at is repaired as it goes on, so
        // none is removed here: the all-stop a refresh makes removes those
        // a gate that passed leaves. The main thread runs as the execution
        // goes on from there.
        self.sync_step_gates(false)?;
        if !self.step_gate_holds(self.inferior.as_ref().map_or(pid, |inferior| inferior.tgid)) {
            self.queue_module_refresh()?;
        }
        Ok(true)
    }

    /// Plants the traps the steps' gates need and, with `remove`, which
    /// only an all-stop with no repair under way may ask, removes those no
    /// gate needs any longer. A thread stopped at a trap removed runs on
    /// from it, its instruction restored.
    pub(super) fn sync_step_gates(&mut self, remove: bool) -> crate::Result<()> {
        let Some(inferior) = self.inferior.as_mut() else {
            return Ok(());
        };
        let needed = inferior
            .inspectors
            .records
            .values()
            .filter(|inspector| inspector.connection.is_some())
            .filter_map(|inspector| inspector.gate.as_ref())
            .flat_map(Gate::sites)
            .collect::<std::collections::BTreeSet<_>>();
        let planted = inferior.inspectors.gate_planted.clone();
        let unneeded = if remove && inferior.repairs.is_empty() {
            planted.difference(&needed).copied().collect()
        } else {
            Vec::new()
        };
        for address in unneeded {
            inferior.inspectors.gate_planted.remove(&address);
            if inferior.inspectors.planted().contains(&address) {
                continue;
            }
            super::super::breakpoints::remove_breakpoint_owner_from(
                &self.ptrace,
                inferior,
                address,
                super::super::BreakpointOwner::InspectorHook,
            )?;
            for thread in inferior.threads.values_mut() {
                if thread.stopped_at_breakpoint == Some(address) {
                    thread.stopped_at_breakpoint = None;
                }
            }
        }
        let pid = inferior.memory_thread();
        for &address in needed.difference(&planted) {
            if !inferior.inspectors.planted().contains(&address) {
                self.ptrace.install_breakpoint(
                    pid,
                    &mut inferior.breakpoints,
                    address,
                    super::super::BreakpointOwner::InspectorHook,
                )?;
            }
            inferior.inspectors.gate_planted.insert(address);
        }
        Ok(())
    }

    /// Lets the main thread run once no gate holds it, unless a stop is
    /// being made or the execution has ended.
    pub(super) fn release_step_gate(&mut self) -> crate::Result<()> {
        let Some(inferior) = self.inferior.as_ref() else {
            return Ok(());
        };
        let main = inferior.tgid;
        let runs = inferior.barrier.is_none()
            && inferior.repairs.is_empty()
            && inferior
                .active
                .as_ref()
                .is_some_and(|active| active.resume_threads.contains(&main));
        if runs && !self.step_gate_holds(main) {
            self.continue_thread(main)?;
        }
        Ok(())
    }
}

/// The step V8 runs for `step`, which begins where an isolate of
/// `isolates` holds the step's thread paused. From anywhere else on
/// `main`, the thread of the main isolate, it pauses at the next statement
/// V8 runs (F19).
pub(super) fn stepping(
    isolates: &BTreeMap<Session, Isolate>,
    step: &crate::runtime_model::RuntimeStep,
    main: crate::ThreadId,
) -> std::result::Result<Stepping, Arc<str>> {
    let command = match step.kind {
        crate::StepKind::IntoSource => inspector_protocol::STEP_INTO,
        crate::StepKind::OverSource => inspector_protocol::STEP_OVER,
        crate::StepKind::Out => inspector_protocol::STEP_OUT,
        kind => return Err(format!("V8 cannot step JavaScript by {kind:?}").into()),
    };
    let paused = isolates.iter().find_map(|(session, isolate)| {
        let depth = isolate.paused.as_ref()?.depth_holding(step.thread)?;
        Some((session.clone(), depth))
    });
    let (session, depth, command) = match paused {
        Some((session, depth)) => (session, Some(depth), command),
        None if step.thread == main => (Session::main(), None, inspector_protocol::PAUSE),
        None => {
            return Err(
                "JavaScript steps from a native stop run on the main thread; on a worker, they begin where V8 paused it".into(),
            );
        }
    };
    Ok(Stepping {
        number: step.number,
        kind: step.kind,
        session,
        depth,
        command,
        sent: false,
        skipped: 0,
    })
}

/// The most pauses in skipped scripts a step goes on from before it ends
/// where it is. From a timer's landing in Node's code, the program's next
/// statement took 59 (F19).
const MAX_SKIPPED: u32 = 1000;

/// Whether a step goes on from a pause in the script at `url`: Node's own
/// scripts, unless steps enter a runtime's code, and the files `skips`
/// names. V8's blackboxing cannot skip Node's: it decides by a script's
/// context, which the builtins in Node's snapshot do not record.
fn skipped(url: &str, skips: &crate::StepSkips, enter_runtime: bool) -> bool {
    (url.starts_with("node:") && !enter_runtime)
        || skips.matches(&inspector_protocol::script_path(url))
}

/// Whether a pause with `depth` frames ends `step`, which no breakpoint
/// stopped: V8 ends a step at its next pause, unless a breakpoint whose
/// condition failed paused it in a call the step runs through.
pub(super) const fn ends_step(step: &Stepping, depth: usize) -> bool {
    match (step.kind, step.depth) {
        (crate::StepKind::OverSource, Some(began)) => depth <= began,
        (crate::StepKind::Out, Some(began)) => depth < began,
        _ => true,
    }
}
