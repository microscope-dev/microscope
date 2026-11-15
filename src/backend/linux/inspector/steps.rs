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

#[derive(Debug)]
pub(super) struct Stepping {
    pub(super) number: u64,
    pub(super) kind: crate::StepKind,

    pub(super) session: Session,

    pub(super) depth: Option<usize>,

    pub(super) command: &'static str,

    pub(super) sent: bool,

    pub(super) skipped: u32,
}

impl<P: LinuxTraceOps> Controller<P> {
    pub(super) fn step_goes_on(
        &mut self,
        runtime: RuntimeId,
        session: &Session,
        params: &serde_json::Value,
        statement: bool,
    ) -> bool {
        let (skips, enter) = (self.step_skips.clone(), self.step_into_runtime);
        let Some(inspectors) = self
            .session
            .inferior_mut()
            .map(|inferior| &mut inferior.inspectors)
        else {
            return false;
        };
        let (Some(inspector), maps) = (inspectors.records.get_mut(&runtime), &inspectors.maps)
        else {
            return false;
        };
        let location = params
            .pointer("/callFrames/0/location")
            .unwrap_or(&serde_json::Value::Null);
        let top = location["scriptId"].as_str().and_then(|id| {
            inspector
                .status
                .isolates()
                .get(session)?
                .scripts
                .get(id)
                .cloned()
        });
        let Some(link) = inspector.status.link() else {
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
            (Some("step" | "other" | "ambiguous"), Some(step)) if step.skipped < MAX_SKIPPED => {
                step.skipped += 1;
                if unmapped && step.depth.is_some() {
                    step.command
                } else {
                    inspector_protocol::STEP_INTO
                }
            }
            (Some("exception" | "promiseRejection"), step)
                if params.pointer("/data/uncaught") != Some(&serde_json::Value::Bool(true)) =>
            {
                step.map_or(inspector_protocol::RESUME, |step| step.command)
            }
            (Some("other"), None) if statement => inspector_protocol::RESUME,
            _ => return false,
        };
        send(&link.connection, session, command, serde_json::json!({})).is_ok()
    }

    pub(super) fn steps_in(&self, runtime: RuntimeId, session: &Session) -> bool {
        self.session
            .inferior()
            .and_then(|inferior| inferior.inspectors.records.get(&runtime)?.step.as_ref())
            .is_some_and(|step| step.sent && step.session == *session)
    }

    pub(super) fn inspector_step_end(
        &mut self,
        key: &PauseKey,
        pid: Pid,
        depth: usize,
    ) -> Option<StopReason> {
        let (runtime, session) = key;
        let step = self
            .session
            .inferior_mut()?
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
    pub(in crate::backend::linux) fn javascript_step_from_native(
        &self,
        stop_id: crate::protocol::StopId,
        pid: Pid,
        frame: crate::StackFrameId,
        kind: crate::StepKind,
    ) -> Option<RuntimeId> {
        let inferior = self.session.inferior()?;
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
            .find(|(_, inspector)| inspector.status.isolates().contains_key(&Session::main()))
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

#[derive(Debug, Clone)]
pub(super) struct Gate {
    pub(super) number: u64,

    entry: crate::VirtualAddress,
    phase: GatePhase,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum GatePhase {
    Unsent,
    Sent(Vec<u8>),

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

    pub(super) fn sent(&mut self, id: Option<super::super::cdp::RequestId>) {
        if self.phase != GatePhase::Unsent {
            return;
        }
        self.phase = id.map_or(GatePhase::Passed, |id| {
            let message = serde_json::json!({
                "id": id.number(),
                "method": inspector_protocol::PAUSE,
                "params": {},
            });
            GatePhase::Sent(message.to_string().into_bytes())
        });
    }

    pub(super) const fn holds(&self, resuming: bool) -> bool {
        match self.phase {
            GatePhase::Unsent => !resuming,
            GatePhase::Sent(_) | GatePhase::Receiving { .. } => true,
            GatePhase::Passed => false,
        }
    }

    pub(super) fn sites(&self) -> Vec<crate::VirtualAddress> {
        match self.phase {
            GatePhase::Unsent | GatePhase::Sent(_) => vec![self.entry],
            GatePhase::Receiving { returns_to, .. } => vec![self.entry, returns_to],
            GatePhase::Passed => Vec::new(),
        }
    }
}

impl<P: LinuxTraceOps> Controller<P> {
    pub(in crate::backend::linux) fn step_gate_holds(&self, pid: Pid) -> bool {
        self.session.inferior().is_some_and(|inferior| {
            pid == inferior.tgid
                && inferior.inspectors.records.values().any(|inspector| {
                    let resuming = inspector
                        .status
                        .isolates()
                        .get(&Session::main())
                        .is_some_and(|isolate| isolate.resuming);
                    inspector.status.link().is_some()
                        && inspector
                            .gate
                            .as_ref()
                            .is_some_and(|gate| gate.holds(resuming))
                })
        })
    }

    pub(super) fn note_step_gate(
        &mut self,
        pid: Pid,
        address: crate::VirtualAddress,
    ) -> crate::Result<bool> {
        let Some(inferior) = self.session.inferior() else {
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
            .session
            .inferior_mut()
            .and_then(|inferior| inferior.inspectors.records.get_mut(&runtime))
            .and_then(|inspector| inspector.gate.as_mut())
        {
            gate.phase = phase;
        }
        self.sync_step_gates(false)?;
        if !self.step_gate_holds(
            self.session
                .inferior()
                .map_or(pid, |inferior| inferior.tgid),
        ) {
            self.queue_module_refresh()?;
        }
        Ok(true)
    }

    pub(super) fn sync_step_gates(&mut self, remove: bool) -> crate::Result<()> {
        let Some(inferior) = self.session.inferior_mut() else {
            return Ok(());
        };
        let needed = inferior
            .inspectors
            .records
            .values()
            .filter(|inspector| inspector.status.link().is_some())
            .filter_map(|inspector| inspector.gate.as_ref())
            .flat_map(Gate::sites)
            .collect::<std::collections::BTreeSet<_>>();
        let planted = inferior.inspectors.gate_planted.clone();
        let unneeded = if remove && !inferior.repair_under_way() {
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

    pub(super) fn release_step_gate(&mut self) -> crate::Result<()> {
        let Some(inferior) = self.session.inferior() else {
            return Ok(());
        };
        let main = inferior.tgid;
        let runs = inferior.run.barrier().is_none()
            && !inferior.repair_under_way()
            && inferior
                .run
                .execution()
                .is_some_and(|active| active.resume_threads.contains(&main));
        if runs && !self.step_gate_holds(main) {
            self.continue_thread(main)?;
        }
        Ok(())
    }
}

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

const MAX_SKIPPED: u32 = 1000;

fn skipped(url: &str, skips: &crate::StepSkips, enter_runtime: bool) -> bool {
    (url.starts_with("node:") && !enter_runtime)
        || skips.matches(&inspector_protocol::script_path(url))
}

pub(super) const fn ends_step(step: &Stepping, depth: usize) -> bool {
    match (step.kind, step.depth) {
        (crate::StepKind::OverSource, Some(began)) => depth <= began,
        (crate::StepKind::Out, Some(began)) => depth < began,
        _ => true,
    }
}
