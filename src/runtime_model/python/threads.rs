//! The thread states of every interpreter in the process: CPython's
//! runtime state lists the interpreters, and each interpreter lists its
//! threads' states, which name the native thread each runs on and its
//! innermost frame.

use std::collections::{BTreeMap, HashSet};

use super::objects::{Problem, Reader};
use crate::image::python::Field;
use crate::runtime_model::{RuntimeStop, TargetThreadId};
use crate::{ThreadId, VirtualAddress};

/// The most interpreters read, and the most thread states of one.
const MAX_INTERPRETERS: usize = 256;
const MAX_THREAD_STATES: usize = 1 << 14;

/// Bound the host catalog independently of the interpreter's thread list.
const MAX_MAPPED_THREADS: usize = 1 << 14;

/// Proven target-namespace identities, with ambiguity retained rather than
/// assigning an interpreter frame to whichever host thread was seen last.
#[derive(Debug)]
pub(super) struct ThreadIds(BTreeMap<TargetThreadId, Option<ThreadId>>);

impl ThreadIds {
    pub(super) fn new(stop: &dyn RuntimeStop) -> Result<Self, Problem> {
        let mut threads = stop.threads();
        if threads.len() > MAX_MAPPED_THREADS {
            return Err(format!(
                "mapping Python's native ids exceeds {MAX_MAPPED_THREADS} controlled threads"
            )
            .into());
        }
        threads.sort_unstable();
        threads.dedup();
        let mut ids = BTreeMap::new();
        for (thread, target) in stop.target_thread_ids(&threads) {
            if threads.binary_search(&thread).is_ok() {
                ids.entry(target)
                    .and_modify(|known| {
                        if *known != Some(thread) {
                            *known = None;
                        }
                    })
                    .or_insert(Some(thread));
            }
        }
        Ok(Self(ids))
    }

    pub(super) fn resolve(&self, native: u64) -> Result<ThreadId, Problem> {
        let target = TargetThreadId::new(native).ok_or("Python's native thread id is zero")?;
        match self.0.get(&target) {
            Some(Some(thread)) => Ok(*thread),
            Some(None) => Err(format!("Python native thread {native} has an ambiguous debugger-thread mapping").into()),
            None => Err(format!("Python native thread {native} has no proven matching debugger thread in its namespace").into()),
        }
    }
}

/// A thread's state in one interpreter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct ThreadState {
    pub(super) address: VirtualAddress,

    /// The interpreter's state, and its number.
    pub(super) interpreter_state: VirtualAddress,
    pub(super) interpreter: i64,

    /// The native thread it runs on, as the process's own namespace
    /// numbers it.
    pub(super) native: u64,

    /// Its innermost frame, and from 3.15 the frame its chain ends at.
    pub(super) frame: Option<VirtualAddress>,
    pub(super) base: Option<VirtualAddress>,
}

/// Every thread state of every interpreter of the runtime whose state is
/// at `runtime`, and why some may be missing.
pub(super) fn thread_states(
    reader: &Reader<'_>,
    runtime: VirtualAddress,
) -> Result<(Vec<ThreadState>, Vec<Problem>), Problem> {
    let mut states = Vec::new();
    let mut gaps = Vec::new();
    let mut interpreters = HashSet::new();
    let mut next = Some(reader.field_word(runtime, Field::RuntimeInterpretersHead)?)
        .filter(|address| *address != 0)
        .map(VirtualAddress::new);
    while let Some(interpreter) = next {
        if !interpreters.insert(interpreter) {
            gaps.push("CPython's list of interpreters loops".into());
            break;
        }

        if interpreters.len() > MAX_INTERPRETERS {
            gaps.push(format!("CPython runs more than {MAX_INTERPRETERS} interpreters").into());
            break;
        }

        let id = reader
            .field_word(interpreter, Field::InterpreterId)?
            .cast_signed();
        if let Err(problem) = interpreter_threads(reader, interpreter, id, &mut states) {
            gaps.push(problem);
        }

        next = Some(reader.field_word(interpreter, Field::InterpreterNext)?)
            .filter(|address| *address != 0)
            .map(VirtualAddress::new);
    }

    Ok((states, gaps))
}

/// Adds the thread states of the interpreter at `interpreter`, numbered
/// `id`, to `states`, or says why some cannot be read.
fn interpreter_threads(
    reader: &Reader<'_>,
    interpreter: VirtualAddress,
    id: i64,
    states: &mut Vec<ThreadState>,
) -> Result<(), Problem> {
    let mut seen = HashSet::new();
    let mut next = Some(reader.field_word(interpreter, Field::InterpreterThreadsHead)?)
        .filter(|address| *address != 0)
        .map(VirtualAddress::new);
    while let Some(address) = next {
        if !seen.insert(address) {
            return Err(format!("interpreter {id}'s list of threads loops").into());
        }

        if seen.len() > MAX_THREAD_STATES {
            return Err(
                format!("interpreter {id} has more than {MAX_THREAD_STATES} threads").into(),
            );
        }

        let state = reader.record(address, Field::ThreadSize)?;
        let frame = match reader.layout.field(Field::CframeCurrentFrame) {
            // 3.12 keeps the innermost frame in the thread's `_PyCFrame`.
            Some(offset) => match state.pointer(Field::ThreadCurrentFrame)? {
                Some(cframe) => Some(reader.word(cframe.wrapping_add(offset))?),
                None => None,
            }
            .filter(|frame| *frame != 0)
            .map(VirtualAddress::new),
            None => state.pointer(Field::ThreadCurrentFrame)?,
        };

        let base = if reader.layout.has(Field::ThreadBaseFrame) {
            state.pointer(Field::ThreadBaseFrame)?
        } else {
            None
        };

        states.push(ThreadState {
            address,
            interpreter_state: interpreter,
            interpreter: id,
            native: state.word(Field::ThreadNativeId)?,
            frame,
            base,
        });
        next = state.pointer(Field::ThreadNext)?;
    }

    Ok(())
}
