use std::collections::BTreeMap;

use super::objects::{Problem, Reader};
use super::threads::ThreadState;
use crate::VirtualAddress;
use crate::image::python::Field;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Gil {
    Held,

    Waiting,

    Released,
}

impl Gil {
    pub(super) const fn label(self) -> Option<&'static str> {
        match self {
            Self::Held => Some("held"),
            Self::Waiting => Some("waiting"),
            Self::Released => None,
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct Lock {
    locked: bool,
    holder: u64,
}

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
        locked: locked == 1,
        holder,
    }))
}

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

fn int(reader: &Reader<'_>, address: VirtualAddress) -> Result<i32, Problem> {
    let bytes = reader.bytes(address, 4)?;
    let bytes: [u8; 4] = bytes
        .as_slice()
        .try_into()
        .map_err(|_| -> Problem { "an int read short".into() })?;
    Ok(i32::from_le_bytes(bytes))
}
