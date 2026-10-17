//! PEP 768 requests, computed without changing memory or running code.

use std::cell::Cell;
use std::sync::Arc;

use super::objects::{Problem, Reader};
use super::threads;
use crate::image::python::{Constant, Field};
use crate::runtime_model::{RuntimeWrite, TargetThreadId};
use crate::{ThreadId, VirtualAddress};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Candidate {
    pub(super) thread: ThreadId,
    pub(super) native: TargetThreadId,
}

struct ReadAccounting<'a> {
    payer: &'a dyn super::objects::Charge,
    limited: Cell<bool>,
}

impl super::objects::Charge for ReadAccounting<'_> {
    fn charge(&self, bytes: usize) -> Result<(), Problem> {
        self.payer
            .charge(bytes)
            .inspect_err(|_| self.limited.set(true))
    }
}

/// Outcomes of this attempt, not permanent runtime capabilities. The
/// lifecycle owner decides whether and when to retry.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub(super) enum AgentFailure {
    #[error("the Python interpreter has not finished initialization")]
    Initializing,
    #[error("no thread is running Python")]
    NoThread,
    #[error("the Python thread already has a pending remote-debugging request")]
    PendingRequest,
    #[error("the Python remote-debugging script slot at {address} has not been cleared")]
    UnclearedSlot { address: VirtualAddress },
    #[error("{0}")]
    ReadLimit(Arc<str>),
    #[error("{0}")]
    Unavailable(Arc<str>),
    #[error("{0}")]
    Refused(Arc<str>),
}

impl From<Arc<str>> for AgentFailure {
    fn from(reason: Arc<str>) -> Self {
        Self::Unavailable(reason)
    }
}

impl From<&'static str> for AgentFailure {
    fn from(reason: &'static str) -> Self {
        Self::Refused(reason.into())
    }
}

/// The caller first establishes that the target build supports PEP 768;
/// the configuration flag below is policy, not proof of compiled support.
/// Candidates are live threads the caller proved are executing Python,
/// ordered by preference. This computes a request, never makes a write.
/// The caller owns outstanding generations and must await acknowledgement
/// before preparing another request. Its eligibility proof must exclude a
/// thread consuming a PEP 768 request, including the path-copy window.
/// Acknowledgement must precede clearing the old script slot for reuse.
pub(super) fn plan(
    reader: &Reader<'_>,
    runtime: VirtualAddress,
    script: &str,
    candidates: &[Candidate],
) -> Result<Vec<RuntimeWrite>, AgentFailure> {
    let mut fallback = crate::inspection::InspectionBudget::new(crate::InspectionLimits::default());
    let fallback = super::values::Budget::new(&mut fallback);
    let accounting = ReadAccounting {
        payer: reader.charge.unwrap_or(&fallback),
        limited: Cell::new(false),
    };
    let reader = Reader {
        charge: Some(&accounting),
        ..*reader
    };
    match compute(&reader, runtime, script, candidates) {
        Err(AgentFailure::Unavailable(reason)) if accounting.limited.get() => {
            Err(AgentFailure::ReadLimit(reason))
        }
        result => result,
    }
}

fn compute(
    reader: &Reader<'_>,
    runtime: VirtualAddress,
    script: &str,
    candidates: &[Candidate],
) -> Result<Vec<RuntimeWrite>, AgentFailure> {
    let field = |field| {
        reader.layout.field(field).ok_or_else(|| {
            AgentFailure::Refused(
                format!(
                    "CPython's layout has no {}; PEP 768 needs 3.14 or later",
                    field.name()
                )
                .into(),
            )
        })
    };
    let capacity = field(Field::DebugScriptSize)?;
    if !(2..=4096).contains(&capacity)
        || script.is_empty()
        || !script.starts_with('/')
        || script.as_bytes().contains(&0)
        || script.len() as u64 >= capacity
    {
        return Err("the Python agent's script path does not fit its PEP 768 buffer".into());
    }

    let ranges = control_ranges(reader)?;
    let bit = u64::from(reader.layout.constant(Constant::DebugStopBit));
    if !bit.is_power_of_two() {
        return Err("CPython's layout has no valid remote-debugging eval breaker bit".into());
    }
    let finalizing = runtime
        .checked_add(field(Field::RuntimeFinalizing)?)
        .ok_or("CPython's finalization address overflows")?;
    let enabled_offset = field(Field::DebugEnabled)?;
    let initialized = runtime
        .checked_add(field(Field::RuntimeInitialized)?)
        .ok_or("CPython's initialization address overflows")?;
    if reader.word(finalizing)? != 0 {
        return Err("the Python runtime is finalizing".into());
    }
    let initialized = reader.bytes(initialized, 4)?;
    if initialized == [0; 4] {
        return Err(AgentFailure::Initializing);
    }
    if initialized != 1_i32.to_le_bytes() {
        return Err("CPython has an invalid initialization flag".into());
    }
    let breaker = ranges[2].start;
    let state = select_thread(reader, runtime, candidates)?;

    let add = |base: VirtualAddress, offset: u64| {
        base.get()
            .checked_add(offset)
            .map(VirtualAddress::new)
            .ok_or_else(|| AgentFailure::from("the Python injection address overflows"))
    };
    let enabled = add(state.interpreter_state, enabled_offset)?;
    let enabled = reader.bytes(enabled, 4)?;
    if enabled == [0; 4] {
        return Err("remote debugging is disabled in the Python interpreter".into());
    }
    if enabled != 1_i32.to_le_bytes() {
        return Err("CPython has an invalid remote-debugging setting".into());
    }

    let change = |offset, bytes: Vec<u8>| -> Result<RuntimeWrite, AgentFailure> {
        let address = add(state.address, offset)?;
        Ok(RuntimeWrite {
            address,
            expected: reader.bytes(address, bytes.len() as u64)?,
            bytes,
        })
    };
    let pending = ranges[0].start;
    let pending_address = add(state.address, pending)?;
    let occupied = reader.bytes(pending_address, 4)?;
    if occupied == 1_i32.to_le_bytes() {
        return Err(AgentFailure::PendingRequest);
    }
    if occupied != [0; 4] {
        return Err("CPython has an invalid pending remote-debugging flag".into());
    }
    let slot = add(state.address, ranges[1].start)?;
    if reader.bytes(slot, 1)? != [0] {
        return Err(AgentFailure::UnclearedSlot { address: slot });
    }
    let breaker_value = reader.word(add(state.address, breaker)?)?;
    let mut path = script.as_bytes().to_vec();
    path.push(0);
    // Publish the request last. The controller applies these in order at
    // an all-stop, checking every expected byte before it changes any.
    Ok(vec![
        change(ranges[1].start, path)?,
        change(pending, 1_i32.to_le_bytes().to_vec())?,
        change(breaker, (breaker_value | bit).to_le_bytes().to_vec())?,
    ])
}

/// The three control spans, checked before any process write.
fn control_ranges(reader: &Reader<'_>) -> Result<[std::ops::Range<u64>; 3], AgentFailure> {
    let field = |field| {
        reader
            .layout
            .field(field)
            .ok_or_else(|| AgentFailure::Refused(format!("CPython has no {}", field.name()).into()))
    };
    // `Reader` is also used with synthesized layouts in tests. Validate
    // the walk's requirements here rather than relying on its provider.
    for required in [
        Field::RuntimeInterpretersHead,
        Field::InterpreterNext,
        Field::InterpreterThreadsHead,
        Field::InterpreterId,
        Field::ThreadNext,
        Field::ThreadCurrentFrame,
        Field::ThreadNativeId,
    ] {
        field(required)?;
    }
    let capacity = field(Field::DebugScriptSize)?;
    let support = field(Field::DebugSupport)?;
    let pending = support.checked_add(field(Field::DebugPending)?);
    let path = support.checked_add(field(Field::DebugScript)?);
    let breaker = field(Field::DebugEvalBreaker)?;
    let size = field(Field::ThreadSize)?;
    if size > crate::image::python::MAX_STRUCTURE {
        return Err("CPython's thread state exceeds the structure read limit".into());
    }
    let [Some(pending), Some(path), Some(breaker)] = [
        pending.and_then(|start| Some(start..start.checked_add(4)?)),
        path.and_then(|start| Some(start..start.checked_add(capacity)?)),
        breaker.checked_add(8).map(|end| breaker..end),
    ] else {
        return Err("CPython's PEP 768 control addresses overflow".into());
    };
    let ranges = [pending, path, breaker];
    if ranges.iter().any(|range| range.end > size) {
        return Err("CPython's PEP 768 fields exceed its thread state".into());
    }
    if ranges.iter().enumerate().any(|(at, range)| {
        ranges[..at]
            .iter()
            .any(|other| range.start < other.end && other.start < range.end)
    }) {
        return Err("CPython's PEP 768 control fields overlap".into());
    }

    Ok(ranges)
}

/// Select only among the caller's proven candidates, and refuse ambiguous
/// interpreter ownership instead of installing in an arbitrary interpreter.
fn select_thread(
    reader: &Reader<'_>,
    runtime: VirtualAddress,
    candidates: &[Candidate],
) -> Result<threads::ThreadState, AgentFailure> {
    let first = reader.field_word(runtime, Field::RuntimeInterpretersHead)?;
    if first == 0 {
        return Err(AgentFailure::NoThread);
    }
    let next = reader.field_word(VirtualAddress::new(first), Field::InterpreterNext)?;
    if next == first {
        return Err("CPython's list of interpreters loops".into());
    }
    if next != 0 {
        return Err(
            "Python execution control with multiple interpreters is not implemented".into(),
        );
    }
    let (states, gaps) = threads::thread_states(reader, runtime)?;
    if let Some(gap) = gaps.first() {
        return Err(Arc::clone(gap).into());
    }
    if states
        .iter()
        .any(|state| state.interpreter_state.get() != first)
    {
        return Err("the Python interpreter list changed during request preparation".into());
    }
    let live = reader.stop.threads();
    let mut ambiguous = false;
    for candidate in candidates
        .iter()
        .filter(|candidate| live.contains(&candidate.thread))
    {
        let mut matching = states
            .iter()
            .filter(|state| state.native == candidate.native.get());
        let Some(state) = matching.next() else {
            continue;
        };
        if matching.next().is_some() {
            ambiguous = true;
            continue;
        }
        return Ok(*state);
    }
    if ambiguous {
        Err("each matching injection candidate has more than one CPython thread state".into())
    } else {
        Err(AgentFailure::NoThread)
    }
}
