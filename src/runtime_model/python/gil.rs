//! GIL ownership and waits. From 3.15, thread states report both; earlier versions
//! identify only the holder, from a locked GIL's last-holder field.
//! A disabled GIL in a free-threaded build has neither holders nor waiters.
//!
//! Live reads may race threads taking or dropping the GIL.

use std::collections::BTreeMap;

use super::objects::{Problem, Reader};
use super::threads::ThreadState;
use crate::VirtualAddress;
use crate::image::python::Field;

/// What a thread does with its interpreter's GIL.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Gil {
    Held,

    /// It asks for the GIL another thread holds, which only 3.15 and later
    /// say.
    Waiting,

    /// It neither holds nor, where the runtime says, asks for it.
    Released,
}

impl Gil {
    /// The label a sample of the thread carries, if any.
    pub(super) const fn label(self) -> Option<&'static str> {
        match self {
            Self::Held => Some("held"),
            Self::Waiting => Some("waiting"),
            Self::Released => None,
        }
    }
}

/// An interpreter's GIL, as read once for its threads.
#[derive(Debug, Clone, Copy)]
struct Lock {
    locked: bool,
    holder: u64,
}

/// What each of `states` does with its interpreter's GIL, in their order:
/// `None` for a thread whose interpreter has no GIL on, or an error that
/// says why it cannot be read.
pub(super) fn gil_states(
    reader: &Reader<'_>,
    states: &[ThreadState],
) -> Vec<Result<Option<Gil>, Problem>> {
    let mut locks: BTreeMap<VirtualAddress, Result<Option<Lock>, Problem>> = BTreeMap::new();
    states
        .iter()
        .map(|state| {
            let lock = locks
                .entry(state.interpreter_state)
                .or_insert_with(|| lock(reader, state.interpreter_state))
                .clone()?;
            let Some(lock) = lock else {
                return Ok(None);
            };

            thread_gil(reader, state, lock).map(Some)
        })
        .collect()
}

/// The GIL the interpreter at `interpreter` takes, or `None` where it is
/// off or not yet made.
fn lock(reader: &Reader<'_>, interpreter: VirtualAddress) -> Result<Option<Lock>, Problem> {
    let layout = reader.layout;
    let gil = reader.field_word(interpreter, Field::InterpreterCevalGil)?;
    if gil == 0 {
        return Ok(None);
    }

    let gil = VirtualAddress::new(gil);
    if let Some(enabled) = layout.field(Field::GilEnabled)
        && int(reader, gil.wrapping_add(enabled))? == 0
    {
        return Ok(None);
    }

    // The GIL's structure holds `locked` and `last_holder` where the
    // interpreter's own copy of it does, less where that copy begins.
    let within = |field: Field| -> Result<u64, Problem> {
        reader
            .offset(field)?
            .checked_sub(reader.offset(Field::InterpreterGil)?)
            .ok_or_else(|| format!("CPython's layout puts {} before its GIL", field.name()).into())
    };

    let locked = int(
        reader,
        gil.wrapping_add(within(Field::InterpreterGilLocked)?),
    )?;
    let holder = reader.word(gil.wrapping_add(within(Field::InterpreterGilHolder)?))?;

    Ok(Some(Lock {
        // An uninitialized GIL is locked -1.
        locked: locked == 1,
        holder,
    }))
}

/// What the thread state `state` does with its interpreter's GIL, which
/// `lock` is.
fn thread_gil(reader: &Reader<'_>, state: &ThreadState, lock: Lock) -> Result<Gil, Problem> {
    let layout = reader.layout;
    if let (Some(holds), Some(requested)) = (
        layout.field(Field::ThreadHoldsGil),
        layout.field(Field::ThreadGilRequested),
    ) {
        return Ok(if int(reader, state.address.wrapping_add(holds))? != 0 {
            Gil::Held
        } else if int(reader, state.address.wrapping_add(requested))? != 0 {
            Gil::Waiting
        } else {
            Gil::Released
        });
    }

    Ok(if lock.locked && lock.holder == state.address.get() {
        Gil::Held
    } else {
        Gil::Released
    })
}

/// The C `int` at `address`.
fn int(reader: &Reader<'_>, address: VirtualAddress) -> Result<i32, Problem> {
    let bytes = reader.bytes(address, 4)?;
    let bytes: [u8; 4] = bytes
        .as_slice()
        .try_into()
        .map_err(|_| -> Problem { "an int read short".into() })?;
    Ok(i32::from_le_bytes(bytes))
}
