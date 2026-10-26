//! Enumerates interpreters from CPython's runtime state, then each interpreter's threads.
//! Thread states identify their native thread and innermost frame.

use std::collections::HashSet;

use super::objects::{Problem, Reader};
use crate::VirtualAddress;
use crate::image::python::Field;

/// The most interpreters read, and the most thread states of one.
const MAX_INTERPRETERS: usize = 256;
const MAX_THREAD_STATES: usize = 1 << 14;

pub(super) use crate::runtime_model::thread_ids::ThreadIds;

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

        // An interpreter that cannot be read ends the list, but not the
        // threads of those before it.
        let id = match reader.field_word(interpreter, Field::InterpreterId) {
            Ok(id) => id.cast_signed(),
            Err(problem) => {
                gaps.push(problem);
                break;
            }
        };

        if let Err(problem) = interpreter_threads(reader, interpreter, id, &mut states) {
            gaps.push(problem);
        }

        next = match reader.field_word(interpreter, Field::InterpreterNext) {
            Ok(next) => Some(next)
                .filter(|address| *address != 0)
                .map(VirtualAddress::new),
            Err(problem) => {
                gaps.push(problem);
                break;
            }
        };
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
