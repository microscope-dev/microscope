//! PEP 768 bootstrap and the acknowledged native command transport.
use std::sync::Arc;

use super::{PythonRuntime, agent, frames, hook, injection, objects, threads, values};
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
                let at_entry = self.entry.and_then(|entry| bias.load(entry)) == Some(instruction);
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
    if arming.step.is_some() {
        return Err("Python stepping is not implemented".into());
    }
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

impl RuntimeAgent for PythonRuntime {
    fn library(&self) -> Vec<u8> {
        hook::library()
    }
    fn control_symbol(&self) -> &'static str {
        hook::CONTROL_SYMBOL
    }
    fn entry(&self) -> Option<ImageAddress> {
        self.entry
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
        library: &str,
        generation: u64,
        process: TargetProcessId,
        arming: &Arming<'_>,
    ) -> Result<String, Arc<str>> {
        let source = agent::script(library, generation, &probes(arming)?)
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
        command(stop, control, previous, generation, &probes(arming)?)
    }
    fn process_scope_required(&self) -> bool {
        self.free_threaded
    }
}

pub(super) fn command(
    stop: &dyn RuntimeStop,
    control: VirtualAddress,
    previous: u64,
    generation: u64,
    probes: &[agent::Probe<'_>],
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
    let payload = serde_json::to_vec(&(generation, probes))
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
