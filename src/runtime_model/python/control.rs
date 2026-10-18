//! PEP 768 bootstrap and the acknowledged native command transport.
use std::sync::Arc;

use super::{PythonRuntime, agent, exceptions, frames, hook, injection, objects, threads, values};
use crate::image::python::{Field, PythonLayout};
use crate::runtime_model::{
    AgentFailure, AgentRequestTicket, AgentRetirement, AgentRetirementProof, Arming,
    PreparedAgentRequest, RuntimeAgent, RuntimeStop, RuntimeWrite, TargetProcessId,
};
use crate::{ImageAddress, ThreadId, VirtualAddress};

impl PythonRuntime {
    fn prepare_agent(
        &self,
        stop: &dyn RuntimeStop,
        script: &str,
        preferred: Option<ThreadId>,
        only: Option<ThreadId>,
    ) -> Result<injection::Plan, AgentFailure> {
        let reading = self.reading()?;
        let mut evidence = Vec::new();
        let identities = threads::ThreadIds::read(stop.threads(), |catalog| {
            evidence = stop.target_thread_ids(catalog);
            evidence.clone()
        })?;
        let bias = stop.load_bias();
        let mut candidates = evidence
            .into_iter()
            .filter_map(|(thread, native)| {
                if identities.lookup(native.get()) != Ok(thread)
                    || only.is_some_and(|only| only != thread)
                {
                    return None;
                }
                let instruction = stop.instruction(thread)?;
                let at_entry = self
                    .entries
                    .iter()
                    .any(|entry| bias.load(*entry) == Some(instruction));
                let inside = self.evaluation.iter().any(|range| {
                    bias.load(range.start)
                        .zip(bias.load(range.end))
                        .is_some_and(|(start, end)| start <= instruction && instruction < end)
                });
                (at_entry || inside).then_some(injection::Candidate { thread, native })
            })
            .collect::<Vec<_>>();
        candidates.sort_unstable_by_key(|candidate| {
            (Some(candidate.thread) != preferred, candidate.thread)
        });
        candidates.dedup();
        let mut budget =
            crate::inspection::InspectionBudget::new(crate::InspectionLimits::default());
        let mut classification = None;
        let result = reading.values(stop, &mut budget, |values, code| {
            let plan = injection::prepare(&values.reader, values.runtime, script, &candidates)
                .map_err(|failure| {
                    let reason: Arc<str> = failure.to_string().into();
                    classification = Some(failure);
                    reason
                })?;
            let mut walk = frames::Walk {
                reader: values.reader,
                code,
                code_type: values.types.code,
            };
            let (frames, gap) = walk.thread(&plan.state);
            if let Some(gap) = gap {
                return Err(gap);
            }
            for frame in frames {
                let globals = VirtualAddress::new(
                    values.reader.field_word(frame.frame, Field::FrameGlobals)?,
                );
                let values::Object::Dict(dict) = values.object(globals)? else {
                    return Err("the injection frame's globals are not a dictionary".into());
                };
                let name = values
                    .lookup(
                        dict,
                        &crate::runtime_model::InterpretedKey::Text(b"__name__".to_vec()),
                    )
                    .map_err(|failure| match failure {
                        crate::runtime_model::InterpretedFailure::Refused(_, reason) => reason,
                        crate::runtime_model::InterpretedFailure::Unavailable(reason) => {
                            reason.to_string().into()
                        }
                    })?;
                if let Some(name) = name
                    && &*values.reader.string(name)? == "_microscope_agent"
                {
                    classification = Some(AgentFailure::NoThread);
                    return Err("a previous agent call is still executing".into());
                }
            }
            Ok(plan)
        });
        result.map_err(|reason| classification.unwrap_or(AgentFailure::Unavailable(reason)))
    }
}

fn probes<'a>(arming: &Arming<'a>) -> Result<Vec<agent::Probe<'a>>, Arc<str>> {
    arming
        .breakpoints
        .iter()
        .map(|arm| {
            let number = arm
                .number
                .checked_add(1)
                .ok_or_else(|| Arc::<str>::from("Python breakpoint identifiers are exhausted"))?;
            match &arm.place {
                crate::ManagedPlace::Line { path, line } => {
                    Ok(("LINE", 1, number, path.as_ref(), *line, ""))
                }
                crate::ManagedPlace::Function { name } => {
                    Ok(("PY_START", 1, number, "", 0, name.as_ref()))
                }
            }
        })
        .collect()
}

/// Where a step begins, as the agent finds its frame: the native thread as
/// CPython numbers it, how many callers the frame has, and its code.
#[derive(Debug, Clone, PartialEq, Eq)]
struct StepOrigin {
    native: u64,
    depth: u32,
    code: VirtualAddress,
}

impl super::Reading {
    /// Where the step from the interpreted frame at `frame` on `thread`
    /// begins.
    fn step_origin(
        &self,
        stop: &dyn RuntimeStop,
        thread: ThreadId,
        frame: VirtualAddress,
    ) -> Result<StepOrigin, Arc<str>> {
        let code_type = stop
            .load_bias()
            .load(self.code_type)
            .ok_or_else(|| Arc::<str>::from("CPython's code type lies outside the process"))?;
        self.read_values(stop, None, |values, table| {
            let (states, _) = threads::thread_states(&values.reader, values.runtime)?;
            let identities = threads::ThreadIds::new(stop)?;
            let state = states
                .iter()
                .filter(|state| state.frame.is_some())
                .find(|state| identities.resolve(state.native) == Ok(thread))
                .ok_or("the stepping thread runs no Python")?;
            let mut walk = frames::Walk {
                reader: values.reader,
                code: table,
                code_type,
            };
            // The agent counts the frame's callers, which must all be read.
            let (frames, end) = walk.thread(state);
            if let Some(end) = end {
                return Err(
                    format!("the stepping thread's frames cannot all be read: {end}").into(),
                );
            }
            let index = frames
                .iter()
                .position(|candidate| candidate.frame == frame)
                .ok_or("the stepping frame is not on its thread's stack")?;
            let depth = u32::try_from(frames.len() - 1 - index)
                .map_err(|_| "the stepping frame has too many callers")?;
            let record = values.reader.record(frame, Field::FrameSize)?;
            let (code, _) = walk.code_of(&record)?;
            Ok(StepOrigin {
                native: state.native,
                depth,
                code,
            })
        })
    }

    /// The step the agent is armed with, if one is active.
    fn step(
        &self,
        stop: &dyn RuntimeStop,
        arming: &Arming<'_>,
    ) -> Result<Option<agent::Step>, Arc<str>> {
        let Some(active) = arming.step else {
            return Ok(None);
        };
        let kind = match active.kind {
            crate::StepKind::IntoSource => agent::INTO,
            crate::StepKind::OverSource => agent::OVER,
            crate::StepKind::Out => agent::OUT,
            _ => return Err("Python code steps by lines: into, over, or out".into()),
        };
        // The native ABI reserves zero; the controller's numbers begin at it.
        let number = active
            .number
            .checked_add(1)
            .ok_or_else(|| Arc::<str>::from("Python step identifiers are exhausted"))?;
        let origin = self.step_origin(stop, active.thread, active.frame)?;
        Ok(Some((
            number,
            kind,
            origin.native,
            origin.depth,
            origin.code.get(),
        )))
    }
}

impl RuntimeAgent for PythonRuntime {
    fn library(&self) -> Vec<u8> {
        hook::library()
    }
    fn control_symbol(&self) -> &'static str {
        hook::CONTROL_SYMBOL
    }
    fn entries(&self) -> &[ImageAddress] {
        &self.entries
    }
    fn eligible_thread(
        &self,
        stop: &dyn RuntimeStop,
        preferred: Option<ThreadId>,
    ) -> Result<ThreadId, AgentFailure> {
        self.prepare_agent(stop, "/", preferred, None)
            .map(|plan| plan.thread)
    }
    fn script(
        &self,
        stop: &dyn RuntimeStop,
        library: &str,
        generation: u64,
        process: TargetProcessId,
        arming: &Arming<'_>,
    ) -> Result<String, Arc<str>> {
        let stepping = self.reading()?.step(stop, arming)?;
        let exceptions = exceptions::bits(arming.exceptions);
        let source = agent::script(library, generation, &probes(arming)?, stepping, exceptions)
            .map_err(|failure| Arc::<str>::from(failure.to_string()))?;
        let mut script = format!(
            "import os as __microscope_os\nif __microscope_os.getpid() == {}:\n",
            process.get()
        );
        for line in source.lines() {
            script.push_str("    ");
            script.push_str(line);
            script.push('\n');
        }
        Ok(script)
    }

    fn inject(
        &self,
        stop: &dyn RuntimeStop,
        script: &str,
        thread: ThreadId,
    ) -> Result<PreparedAgentRequest, AgentFailure> {
        let plan = self.prepare_agent(stop, script, Some(thread), Some(thread))?;
        let reading = self.reading()?;
        let runtime = stop
            .load_bias()
            .load(reading.runtime.start)
            .ok_or_else(|| AgentFailure::from("the runtime address overflows"))?;
        Ok(PreparedAgentRequest {
            ticket: ticket(&reading.layout, runtime, &plan),
            writes: plan.writes.into(),
        })
    }
    fn command(
        &self,
        stop: &dyn RuntimeStop,
        control: VirtualAddress,
        previous: u64,
        generation: u64,
        arming: &Arming<'_>,
    ) -> Result<Vec<RuntimeWrite>, Arc<str>> {
        let stepping = self.reading()?.step(stop, arming)?;
        let exceptions = exceptions::bits(arming.exceptions);
        command(
            stop,
            control,
            previous,
            generation,
            &probes(arming)?,
            stepping,
            exceptions,
        )
    }
    fn process_scope_required(&self) -> bool {
        self.free_threaded
    }
    fn wanted(&self, arming: &Arming<'_>) -> bool {
        !arming.breakpoints.is_empty()
            || arming.step.is_some()
            || exceptions::bits(arming.exceptions) != 0
    }
}

pub(super) fn command(
    stop: &dyn RuntimeStop,
    control: VirtualAddress,
    previous: u64,
    generation: u64,
    probes: &[agent::Probe<'_>],
    stepping: Option<agent::Step>,
    exceptions: u64,
) -> Result<Vec<RuntimeWrite>, Arc<str>> {
    if generation <= previous || !control.get().is_multiple_of(8) {
        return Err("the Python command sequence or alignment is invalid".into());
    }
    let mut header = [0; hook::CONTROL_HEADER];
    if !stop.read(control, &mut header) {
        return Err("the Python command header cannot be read".into());
    }
    if &header[..8] != hook::CONTROL_MAGIC
        || header[8..16] != previous.to_le_bytes()
        || header[20..24] != hook::CONTROL_CAPACITY.to_le_bytes()
        || header[24..] != [0; 8]
    {
        return Err("the Python command header or acknowledged sequence changed".into());
    }
    let payload = serde_json::to_vec(&(generation, probes, stepping, exceptions))
        .map_err(|failure| Arc::<str>::from(failure.to_string()))?;
    let length = u32::try_from(payload.len())
        .ok()
        .filter(|length| *length <= hook::CONTROL_CAPACITY)
        .ok_or_else(|| {
            Arc::<str>::from("the Python configuration exceeds its bounded command slot")
        })?;
    let mut writes = Vec::new();
    for (offset, bytes) in [
        (hook::CONTROL_HEADER as u64, payload),
        (16, length.to_le_bytes().to_vec()),
        (8, generation.to_le_bytes().to_vec()),
    ] {
        let address = control
            .checked_add(offset)
            .ok_or_else(|| Arc::<str>::from("the Python command address overflows"))?;
        let mut expected = vec![0; bytes.len()];
        if !stop.read(address, &mut expected) {
            return Err("the Python command slot cannot be read".into());
        }
        writes.push(RuntimeWrite {
            address,
            expected,
            bytes,
        });
    }
    Ok(writes)
}

pub(super) fn ticket(
    layout: &PythonLayout,
    runtime: VirtualAddress,
    plan: &injection::Plan,
) -> Arc<dyn AgentRequestTicket> {
    Arc::new(Ticket {
        layout: layout.clone(),
        runtime,
        state: plan.state,
        path: plan.writes[0].clone(),
        pending: plan.writes[1].clone(),
    })
}

#[derive(Debug)]
struct Ticket {
    layout: PythonLayout,
    runtime: VirtualAddress,
    state: threads::ThreadState,
    path: RuntimeWrite,
    pending: RuntimeWrite,
}

impl AgentRequestTicket for Ticket {
    fn retire(
        &self,
        stop: &dyn RuntimeStop,
        proof: AgentRetirementProof,
    ) -> Result<AgentRetirement, Arc<str>> {
        let mut budget =
            crate::inspection::InspectionBudget::new(crate::InspectionLimits::default());
        let charge = values::Budget::new(&mut budget);
        let reader = objects::Reader {
            stop,
            layout: &self.layout,
            charge: Some(&charge),
            blocks: None,
        };
        let (states, gaps) = threads::thread_states(&reader, self.runtime)?;
        if let Some(gap) = gaps.into_iter().next() {
            return Err(gap);
        }
        let mut same = states
            .iter()
            .filter(|state| state.address == self.state.address);
        let Some(state) = same.next() else {
            return Ok(AgentRetirement::Retired(Vec::new()));
        };
        if same.next().is_some()
            || state.interpreter_state != self.state.interpreter_state
            || state.native != self.state.native
        {
            return Err("the Python request's thread-state identity changed".into());
        }
        let pending = reader.bytes(self.pending.address, 4)?;
        if pending != [0; 4] && pending != 1_i32.to_le_bytes() {
            return Err("the Python pending request flag is invalid".into());
        }
        let path = reader.bytes(self.path.address, self.path.bytes.len() as u64)?;
        let ours = path == self.path.bytes;
        let partial_ours = proof == AgentRetirementProof::Unresumed
            && path
                .iter()
                .zip(self.path.expected.iter().zip(&self.path.bytes))
                .all(|(held, (before, after))| held == before || held == after);
        if proof == AgentRetirementProof::Unresumed && !partial_ours {
            return Err("the unresumed Python script slot has an unowned change".into());
        }
        if proof != AgentRetirementProof::Unresumed && !ours && path.first() != Some(&0) {
            return Err("another request replaced the Python script slot".into());
        }
        if pending == [0; 4] && proof == AgentRetirementProof::Cancel {
            return Ok(AgentRetirement::Unconfirmed);
        }
        let mut writes = Vec::new();
        if pending != [0; 4] {
            if !ours && !partial_ours {
                return Err("the pending Python request is not owned".into());
            }
            writes.push(RuntimeWrite {
                address: self.pending.address,
                expected: pending,
                bytes: vec![0; 4],
            });
        }
        if let Some(&first) = path.first()
            && first != 0
        {
            writes.push(RuntimeWrite {
                address: self.path.address,
                expected: vec![first],
                bytes: vec![0],
            });
        }
        // Keep the shared eval-breaker bit. Other runtime stop requests can
        // use it too; the handler checks the now-cleared pending flag.
        Ok(AgentRetirement::Retired(writes))
    }
}
